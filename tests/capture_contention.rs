//! A capture that loses the race to *another capture* must not be dropped.
//!
//! The restore lock is shared between restores and captures. Any failure to
//! take it used to be reported as `RestoreInProgress` and the capture thrown
//! away — so when hook capture B lost the race to hook capture A, B's work
//! vanished and the CLI blamed a restore that was not running. A pane split
//! during A's capture then stayed unrecorded until the 120-second fallback
//! timer, and was lost outright if the machine rebooted first.

mod common;

use osm::capture::{self, CaptureOutcome};
use osm::lock::SingleInstance;
use osm::{db, tmux::Tmux};
use std::process::Command;
use std::time::{Duration, Instant};

struct Server(Tmux);

impl Drop for Server {
    fn drop(&mut self) {
        common::shutdown(&self.0);
    }
}

fn server(label: &str) -> Server {
    let t = Tmux::with_socket(&format!("osm-contend-{}-{}", label, std::process::id()));
    t.run(&["new-session", "-d", "-s", "alpha", "-c", "/tmp"])
        .unwrap();
    Server(t)
}

fn snapshot_count(conn: &rusqlite::Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM snapshots", [], |r| r.get(0))
        .unwrap()
}

#[test]
fn work_flagged_as_pending_is_captured_even_inside_the_debounce_window() {
    let s = server("pending");
    let tmp = tempfile::tempdir().unwrap();
    let mut conn = db::open(&tmp.path().join("state.db")).unwrap();
    let lock_path = tmp.path().join("restore.lock");
    let last_capture_path = tmp.path().join("last-capture");
    let dirty = tmp.path().join("capture-dirty");

    // A capture happened one second ago, well inside the 5s window…
    std::fs::write(&last_capture_path, "1000").unwrap();
    // …but an earlier capture was deferred and its state was never recorded.
    std::fs::write(&dirty, "1").unwrap();

    let outcome = capture::snapshot_maybe_debounced(
        &mut conn,
        &s.0,
        None,
        "hook",
        &lock_path,
        &last_capture_path,
        5,
        1_001,
        true,
    )
    .unwrap();

    assert_eq!(
        outcome,
        CaptureOutcome::Captured(1),
        "pending work must be captured; the debounce window may skip a \
         redundant capture, never an outstanding one"
    );
    assert_eq!(snapshot_count(&conn), 1);
    assert!(
        !dirty.exists(),
        "the pending flag must be cleared once the work it describes is captured"
    );
}

#[test]
fn losing_the_race_to_another_capture_defers_the_work_instead_of_dropping_it() {
    let s = server("defer");
    let tmp = tempfile::tempdir().unwrap();
    let mut conn = db::open(&tmp.path().join("state.db")).unwrap();
    let lock_path = tmp.path().join("restore.lock");
    let capture_lock = tmp.path().join("capture.lock");
    let last_capture_path = tmp.path().join("last-capture");
    let dirty = tmp.path().join("capture-dirty");

    // Another capture is mid-commit: it holds the capture lock. No restore is
    // running — the restore lock is free.
    let other_capture = SingleInstance::acquire(&capture_lock).unwrap().unwrap();

    let outcome = capture::snapshot_maybe_debounced(
        &mut conn,
        &s.0,
        None,
        "hook",
        &lock_path,
        &last_capture_path,
        5,
        1_000,
        true,
    )
    .unwrap();

    assert_ne!(
        outcome,
        CaptureOutcome::RestoreInProgress,
        "no restore is running; reporting one is a lie the operator acts on"
    );
    assert_eq!(outcome, CaptureOutcome::Deferred);
    assert_eq!(snapshot_count(&conn), 0);
    assert!(
        dirty.exists(),
        "deferred work must leave a pending flag, or it is simply lost"
    );

    // The holder finishes. The next capture must run despite the debounce
    // window being wide open, because the flag says work is outstanding.
    drop(other_capture);
    std::fs::write(&last_capture_path, "1000").unwrap();
    let outcome = capture::snapshot_maybe_debounced(
        &mut conn,
        &s.0,
        None,
        "hook",
        &lock_path,
        &last_capture_path,
        5,
        1_001,
        true,
    )
    .unwrap();
    assert_eq!(outcome, CaptureOutcome::Captured(1));
    assert_eq!(snapshot_count(&conn), 1);
}

/// Events that arrive *while* a capture is running are absorbed by that same
/// capture with a second pass, rather than waiting for the next hook or the
/// 120-second timer.
#[test]
fn a_capture_takes_another_pass_for_work_flagged_while_it_was_running() {
    let s = server("coalesce");
    let tmp = tempfile::tempdir().unwrap();
    let mut conn = db::open(&tmp.path().join("state.db")).unwrap();
    let lock_path = tmp.path().join("restore.lock");
    let capture_lock = tmp.path().join("capture.lock");
    let last_capture_path = tmp.path().join("last-capture");
    let dirty = tmp.path().join("capture-dirty");

    // Stands in for a second hook process that keeps losing the capture lock
    // race while the capture below runs: each time it finds the lock held, it
    // raises the pending flag exactly as a deferred capture would.
    let flagger = std::thread::spawn({
        let capture_lock = capture_lock.clone();
        let dirty = dirty.clone();
        move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut saw_held = false;
            while Instant::now() < deadline {
                match SingleInstance::acquire(&capture_lock).unwrap() {
                    Some(_free) if saw_held => break,
                    Some(_free) => {}
                    None => {
                        saw_held = true;
                        let _ = std::fs::write(&dirty, "1");
                    }
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            saw_held
        }
    });

    let outcome = capture::snapshot_maybe_debounced(
        &mut conn,
        &s.0,
        None,
        "hook",
        &lock_path,
        &last_capture_path,
        5,
        1_000,
        false,
    )
    .unwrap();

    let saw_held = flagger.join().unwrap();
    assert!(
        saw_held,
        "the capture must hold a capture lock while it runs, or captures \
         cannot tell each other apart from a restore"
    );
    assert!(matches!(outcome, CaptureOutcome::Captured(_)));
    assert_eq!(
        snapshot_count(&conn),
        2,
        "state flagged during the capture must be picked up by a second pass"
    );
}

/// The CLI's `reason` must name the contention it actually hit.
#[test]
fn the_cli_reports_capture_contention_as_such() {
    let socket = format!("osm-contend-cli-{}", std::process::id());
    let t = Tmux::with_socket(&socket);
    let _server = Server(t.clone());
    t.run(&["new-session", "-d", "-s", "alpha", "-c", "/tmp"])
        .unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let state_dir = tmp.path().join("state").join("osm");
    std::fs::create_dir_all(&state_dir).unwrap();
    let _other_capture = SingleInstance::acquire(&state_dir.join("capture.lock"))
        .unwrap()
        .unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_osm"))
        .env("XDG_STATE_HOME", tmp.path().join("state"))
        .env("XDG_CONFIG_HOME", tmp.path().join("config"))
        .args(["--socket", &socket, "snapshot"])
        .output()
        .expect("run osm snapshot");
    assert!(out.status.success());

    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("stdout is JSON");
    assert_eq!(v["snapshot_id"], serde_json::Value::Null);
    assert_eq!(
        v["reason"], "capture in progress",
        "a capture blocked by another capture must not be reported as a restore"
    );
    assert_eq!(
        v["retry_pending"], true,
        "the work is queued, and the consumer needs to know that"
    );
}
