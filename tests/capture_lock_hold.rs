//! A capture must not hold the database's write lock while it reads the
//! agent conversation stores.
//!
//! # The failure this is about
//!
//! `tests/hooks.rs::firing_each_hooked_event_produces_a_snapshot` failed
//! about one run in four with
//!
//! ```text
//! called `Result::unwrap()` on an `Err` value: database is locked
//! Caused by: Error code 5: The database file is locked
//! ```
//!
//! from `osm::db::open`, even though `db.rs` sets `busy_timeout` to five
//! seconds. Instrumenting the failing site showed it was not the WAL switch
//! (which has its own retry loop and its own error context) and not a
//! handler-bypassing pragma: `BEGIN IMMEDIATE` had `busy_timeout=5000` in
//! force and failed **after 5.298 s**, so the busy handler ran its whole
//! budget and gave up. Something was holding the write lock for seconds at a
//! time.
//!
//! It was the capture. `write_topology` opened its transaction and *then*
//! called `agent::detect::prepare`, which walks every enabled agent's
//! conversation store on disk — 1.23 s to 1.35 s of a 1.35 s transaction on
//! the maintainer's machine, where everything else in it took between one and
//! thirty-five milliseconds. tmux fires hooks in parallel and a capture takes
//! a second pass for work flagged while it ran, so those holds arrive back to
//! back; SQLite's busy handler is not a queue, so a waiter that keeps losing
//! to a freshly-arrived writer exhausts its budget and returns SQLITE_BUSY.
//!
//! This is not a test-only problem. A new user's first minutes are exactly
//! this shape — a fresh database with hooks firing captures in parallel — and
//! a capture that fails there is a snapshot they do not have.
//!
//! # How this is checked without a timing threshold
//!
//! `Detection::of` is made to *block*, precisely, at a point this test
//! chooses. Its first statement reads the configuration — that is where the
//! enabled adapter list comes from — so the configuration file is a **FIFO**:
//! `read_to_string` opens it and blocks until something writes and closes.
//! Opening the other end for writing blocks until that reader arrives, so
//! when this test's `open` returns, the capture is inside `Detection::of` and
//! nowhere else.
//!
//! That is the whole of the off-disk phase, not merely its first line. The
//! store walk is later in the same function, and the transaction is opened
//! only once that function has *returned* — so a build in which the lock is
//! free here is one in which the lock is free for the walk too. It is also
//! exactly the historical bug: with `write_topology` opening its transaction
//! first and calling `prepare` inside it, this read was inside the
//! transaction as well, and the probes below would have been refused.
//!
//! It used to be the transcript itself that was the FIFO, which parked the
//! capture inside the store walk proper. That is no longer possible, and the
//! reason is a deliberate one: discovery now requires a store entry to be a
//! **regular file**, and every read of a conversation file refuses anything
//! that is not one, so that content from outside the configured store cannot
//! become a persisted title (`src/agent/mod.rs::open_conversation_file`,
//! `tests/agent_hardening.rs`). A FIFO in a conversation store used to park
//! a capture in `read` for as long as nobody wrote to it — which this test
//! relied on and which was, on a real machine, a capture that never finished.
//!
//! With the capture parked there, the write lock is asked for twice: once as
//! a bare `BEGIN IMMEDIATE` with **no** busy timeout, which is the invariant
//! itself, and once through `db::open`, which is the call that actually
//! failed. Neither may be refused. A real transcript is left in the store as
//! well, so the capture that runs is one that really does walk a store.

mod common;

use osm::capture::{self, Topology};
use osm::db;
use osm::tmux::{PaneRec, SessionRec, WindowRec};
use std::io::Write;
use std::path::Path;

/// A stand-in for one server incarnation, in the shape
/// `Tmux::server_incarnation` returns. Same fixture as
/// `tests/capture_consistency.rs`: a hand-built topology still has to name an
/// incarnation, or the writer refuses it.
const FAKE_SERVER: &str =
    "boot-a:0123456789abcdef0123456789abcdef:4242:9999999:/tmp/tmux-1000/osm-test";

fn topology() -> Topology {
    Topology {
        placements: osm::desktop::Placements::Off,
        sessions: vec![SessionRec {
            id: "$0".to_string(),
            name: "alpha".to_string(),
        }],
        windows: vec![WindowRec {
            session_id: "$0".to_string(),
            id: "@0".to_string(),
            idx: 0,
            name: "main".to_string(),
            layout: "abcd,80x24,0,0,0".to_string(),
            active: true,
            zoomed: false,
            auto_named: false,
        }],
        panes: vec![PaneRec {
            window_id: "@0".to_string(),
            id: "%0".to_string(),
            idx: 0,
            active: true,
            dead: false,
            pid: 1,
            cwd: "/tmp".to_string(),
            title: "t".to_string(),
            cmd: "bash".to_string(),
        }],
        server: Some(FAKE_SERVER.to_string()),
        server_at_end: Some(FAKE_SERVER.to_string()),
    }
}

/// A Claude home holding one ordinary conversation.
///
/// It is here so the capture under test is one that really walks a store and
/// finds something in it, rather than one whose scan has no work to do.
fn store_with_one_conversation(home: &Path) {
    let project = home.join("projects/-tmp-osm-lock");
    std::fs::create_dir_all(&project).unwrap();
    // A real conversation id: discovery requires the whole file stem to be
    // one, so a made-up name would simply be skipped.
    std::fs::write(
        project.join("11111111-2222-3333-4444-555555555555.jsonl"),
        "{\"cwd\":\"/tmp\"}\n",
    )
    .unwrap();
}

/// The configuration file, as a named pipe.
///
/// `Detection::of` reads the configuration before it does anything else —
/// that is where the enabled adapter list comes from — so this is a place the
/// capture stops at, inside the off-disk phase and before the transaction,
/// until this test lets it go.
fn config_that_blocks_until_written(config_home: &Path) -> std::path::PathBuf {
    let dir = config_home.join("osm");
    std::fs::create_dir_all(&dir).unwrap();
    let fifo = dir.join("config.toml");
    let out = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .output()
        .expect("mkfifo(1) is installed");
    assert!(
        out.status.success(),
        "mkfifo failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    fifo
}

/// Wait, bounded, for the capture to open the configuration.
///
/// Opening the write end of a FIFO blocks until a reader arrives, which *is*
/// the rendezvous this test wants — but a build in which the capture never
/// reads the configuration at all would then park here forever rather than
/// failing. `O_NONBLOCK` turns the same wait into a poll: on Linux the write
/// end of a FIFO with no reader fails with `ENXIO` immediately.
///
/// `None` means the capture never opened it, which the caller reports as this
/// test proving nothing — never as the invariant holding.
fn wait_for_the_capture_to_open(fifo: &Path) -> Option<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    // Linux's value. This project is Linux-only (it reads `/proc` to bind
    // panes to conversations), so there is no portability question to answer.
    const O_NONBLOCK: i32 = 0o4000;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(O_NONBLOCK)
            .open(fifo)
        {
            Ok(f) => return Some(f),
            Err(_) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(5))
            }
            Err(_) => return None,
        }
    }
}

/// Take the database's write lock with **no** patience at all.
///
/// `busy_timeout = 0` is the point: this asks whether the lock is free right
/// now, not whether it becomes free within five seconds. A capture that is
/// only reading files has no business holding it.
fn write_lock_is_free(path: &Path) -> Result<(), rusqlite::Error> {
    let conn = rusqlite::Connection::open(path)?;
    conn.pragma_update(None, "busy_timeout", 0)?;
    conn.execute_batch("BEGIN IMMEDIATE; ROLLBACK;")
}

#[test]
fn a_capture_does_not_hold_the_write_lock_while_it_reads_off_disk() {
    let tmp = tempfile::tempdir().unwrap();
    let claude_home = tmp.path().join("claude-home");
    store_with_one_conversation(&claude_home);
    let fifo = config_that_blocks_until_written(&tmp.path().join("config"));
    let db_path = tmp.path().join("state.db");

    // SAFETY: this is the only test in this binary, and it reads no other
    // process's environment.
    //
    // All three, so nothing here reaches the developer's own files: the
    // conversation store is this test's, and so is the config the adapter
    // list is read from.
    std::env::set_var("OSM_CLAUDE_HOME", &claude_home);
    std::env::set_var("XDG_CONFIG_HOME", tmp.path().join("config"));
    std::env::set_var("XDG_STATE_HOME", tmp.path().join("state"));

    let mut conn = db::open(&db_path).unwrap();
    let topo = topology();

    let (probes, snapshot) = std::thread::scope(|scope| {
        let capturing = scope.spawn(|| capture::write_topology(&mut conn, &topo, "test", None));

        // Returns once the capture has the other end open. At that moment the
        // capture is parked in `read` on this pipe, inside `Detection::of`:
        // it has done whatever comes before the off-disk phase and none of
        // what follows it.
        let writer = wait_for_the_capture_to_open(&fifo);
        let probes = writer.map(|mut writer| {
            let free_now = write_lock_is_free(&db_path);
            // The call that actually failed on the maintainer's machine, made
            // from where a parallel hook process would make it.
            let opened = db::open(&db_path).map(|_| ());
            // Let the capture finish, however the two probes went, so this
            // test cannot leave a thread parked on a pipe. Valid TOML, and
            // dropping the handle is what gives `read_to_string` its EOF.
            let _ = writeln!(writer, "[restore]\nplace_windows = false");
            (free_now, opened)
        });

        let snapshot = capturing.join().unwrap();
        (probes, snapshot)
    });

    std::env::remove_var("OSM_CLAUDE_HOME");
    std::env::remove_var("XDG_CONFIG_HOME");
    std::env::remove_var("XDG_STATE_HOME");

    let Some((free_now, opened)) = probes else {
        panic!(
            "the capture never opened the configuration, so it never entered \
             the off-disk phase and this test proves nothing about the lock: \
             `Detection::of` no longer reads the config before it walks the \
             stores, and this rendezvous has to be moved to wherever it does \
             its first read"
        );
    };
    assert!(
        free_now.is_ok(),
        "the capture held the database's write lock while it was doing its \
         off-disk work — reading the config and walking every enabled agent's \
         conversation store, which is filesystem work of unbounded length: \
         {:?}. Every other osm process — a parallel hook's \
         capture, the widget's status probe — is then queued behind a scan \
         that has nothing to do with the database, and the ones that run out \
         of patience fail with `database is locked`",
        free_now.unwrap_err()
    );
    assert!(
        opened.is_ok(),
        "osm::db::open failed while a capture was doing its off-disk work: \
         {:?}. This is the failure verbatim: a hook that cannot open the \
         database captures nothing, and a snapshot nobody took is a snapshot \
         the user does not have",
        opened.unwrap_err()
    );
    assert!(
        snapshot.is_ok(),
        "the capture itself failed: {:?}",
        snapshot.unwrap_err()
    );
}
