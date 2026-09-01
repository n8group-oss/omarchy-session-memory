mod common;

use osm::capture::{self, CaptureOutcome};
use osm::lock::SingleInstance;
use osm::{db, tmux::Tmux};

struct Server(Tmux);

impl Server {
    fn start(label: &str) -> Self {
        let t = Tmux::with_socket(&format!("osm-debounce-{}-{}", label, std::process::id()));
        t.run(&["new-session", "-d", "-s", "alpha", "-c", "/tmp"])
            .unwrap();
        Server(t)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        common::shutdown(&self.0);
    }
}

fn snapshot_count(conn: &rusqlite::Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM snapshots", [], |r| r.get(0))
        .unwrap()
}

#[test]
fn debounced_capture_immediately_after_another_is_skipped_and_writes_no_row() {
    let s = Server::start("skip");
    let tmp = tempfile::tempdir().unwrap();
    let mut conn = db::open(&tmp.path().join("state.db")).unwrap();
    let lock_path = tmp.path().join("restore.lock");
    let last_capture_path = tmp.path().join("last-capture");

    let first = capture::snapshot_maybe_debounced(
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
    assert_eq!(first, CaptureOutcome::Captured(1));
    assert_eq!(snapshot_count(&conn), 1);

    let second = capture::snapshot_maybe_debounced(
        &mut conn,
        &s.0,
        None,
        "hook",
        &lock_path,
        &last_capture_path,
        5,
        1_002, // 2s later, inside the 5s window
        true,
    )
    .unwrap();
    assert_eq!(second, CaptureOutcome::Debounced);
    assert_eq!(
        snapshot_count(&conn),
        1,
        "a debounced skip must not write a new snapshot row"
    );
}

#[test]
fn non_debounced_capture_is_never_skipped() {
    let s = Server::start("nondebounced");
    let tmp = tempfile::tempdir().unwrap();
    let mut conn = db::open(&tmp.path().join("state.db")).unwrap();
    let lock_path = tmp.path().join("restore.lock");
    let last_capture_path = tmp.path().join("last-capture");

    capture::snapshot_maybe_debounced(
        &mut conn,
        &s.0,
        None,
        "manual",
        &lock_path,
        &last_capture_path,
        5,
        1_000,
        false,
    )
    .unwrap();

    let second = capture::snapshot_maybe_debounced(
        &mut conn,
        &s.0,
        None,
        "manual",
        &lock_path,
        &last_capture_path,
        5,
        1_001, // 1s later, well inside the 5s window
        false, // but debounced was not requested
    )
    .unwrap();

    assert_eq!(second, CaptureOutcome::Captured(2));
    assert_eq!(snapshot_count(&conn), 2);
}

#[test]
fn debounced_capture_fires_again_once_the_window_has_elapsed() {
    let s = Server::start("elapsed");
    let tmp = tempfile::tempdir().unwrap();
    let mut conn = db::open(&tmp.path().join("state.db")).unwrap();
    let lock_path = tmp.path().join("restore.lock");
    let last_capture_path = tmp.path().join("last-capture");

    // Backdate the timestamp file directly instead of sleeping, so the test
    // stays deterministic.
    std::fs::write(&last_capture_path, "1000").unwrap();

    let outcome = capture::snapshot_maybe_debounced(
        &mut conn,
        &s.0,
        None,
        "hook",
        &lock_path,
        &last_capture_path,
        5,
        1_006, // 6s after the backdated timestamp, past the 5s window
        true,
    )
    .unwrap();

    assert_eq!(outcome, CaptureOutcome::Captured(1));
    assert_eq!(snapshot_count(&conn), 1);
}

#[test]
fn missing_or_corrupt_timestamp_file_captures_rather_than_skips_or_errors() {
    let s = Server::start("corrupt");
    let tmp = tempfile::tempdir().unwrap();
    let lock_path = tmp.path().join("restore.lock");

    // Missing file.
    {
        let mut conn = db::open(&tmp.path().join("missing.db")).unwrap();
        let last_capture_path = tmp.path().join("absent-last-capture");
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
        assert_eq!(outcome, CaptureOutcome::Captured(1));
    }

    // Corrupt contents.
    {
        let mut conn = db::open(&tmp.path().join("corrupt.db")).unwrap();
        let last_capture_path = tmp.path().join("corrupt-last-capture");
        std::fs::write(&last_capture_path, "not-a-timestamp").unwrap();
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
        assert_eq!(outcome, CaptureOutcome::Captured(1));
    }
}

#[test]
fn lock_held_takes_precedence_over_an_open_debounce_window() {
    let s = Server::start("precedence");
    let tmp = tempfile::tempdir().unwrap();
    let mut conn = db::open(&tmp.path().join("state.db")).unwrap();
    let lock_path = tmp.path().join("restore.lock");
    let last_capture_path = tmp.path().join("last-capture");

    // Open debounce window: a capture "just happened".
    std::fs::write(&last_capture_path, "1000").unwrap();

    let _restore_guard = SingleInstance::acquire(&lock_path).unwrap().unwrap();

    let outcome = capture::snapshot_maybe_debounced(
        &mut conn,
        &s.0,
        None,
        "hook",
        &lock_path,
        &last_capture_path,
        5,
        1_001, // 1s later: still well inside the 5s debounce window too
        true,
    )
    .unwrap();

    assert_eq!(
        outcome,
        CaptureOutcome::RestoreInProgress,
        "a held restore lock must be reported even when the debounce window is also open"
    );
}
