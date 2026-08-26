mod common;

use osm::{capture, db, lock::SingleInstance, restore, tmux::Tmux};

struct Server(Tmux);

impl Server {
    fn start(label: &str) -> Self {
        Server(Tmux::with_socket(&format!(
            "osm-att-{}-{}",
            label,
            std::process::id()
        )))
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        common::shutdown(&self.0);
    }
}

#[test]
fn second_lock_holder_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("restore.lock");
    let first = SingleInstance::acquire(&path).unwrap();
    assert!(first.is_some());
    let second = SingleInstance::acquire(&path).unwrap();
    assert!(second.is_none(), "second acquire must be refused");
    drop(first);
    let third = SingleInstance::acquire(&path).unwrap();
    assert!(third.is_some(), "lock is released on drop");
}

#[test]
fn run_restore_marks_snapshot_restored_and_records_objects() {
    let src = Server::start("src");
    src.0
        .run(&[
            "new-session",
            "-n",
            "code",
            "-d",
            "-s",
            "alpha",
            "-c",
            "/tmp",
        ])
        .unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let mut conn = db::open(&tmp.path().join("state.db")).unwrap();
    let snap = capture::snapshot(&mut conn, &src.0, "test").unwrap();

    // Force the snapshot to look like it came from a previous boot.
    conn.execute(
        "UPDATE snapshots SET boot_id='boot-previous' WHERE id=?1",
        [snap],
    )
    .unwrap();

    let dst = Server::start("dst");
    let report = restore::run_restore(&mut conn, &dst.0, false).unwrap();

    assert_eq!(report.snapshot_id, Some(snap));
    assert_eq!(report.state, "succeeded");
    assert_eq!(report.outcome.created, vec!["alpha".to_string()]);

    let snap_state: String = conn
        .query_row("SELECT state FROM snapshots WHERE id=?1", [snap], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(snap_state, "restored");

    let attempt_state: String = conn
        .query_row(
            "SELECT state FROM restore_attempts WHERE snapshot_id=?1",
            [snap],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(attempt_state, "succeeded");

    let objects: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM restore_objects WHERE kind='session' AND state='done'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(objects, 1);
}

#[test]
fn run_restore_without_a_previous_boot_snapshot_is_a_noop() {
    let src = Server::start("noop");
    src.0
        .run(&[
            "new-session",
            "-n",
            "code",
            "-d",
            "-s",
            "alpha",
            "-c",
            "/tmp",
        ])
        .unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let mut conn = db::open(&tmp.path().join("state.db")).unwrap();
    capture::snapshot(&mut conn, &src.0, "test").unwrap(); // current boot

    let report = restore::run_restore(&mut conn, &src.0, false).unwrap();
    assert_eq!(report.snapshot_id, None);
    assert_eq!(report.state, "nothing_to_restore");
    assert_eq!(report.reason, "no_previous_boot_snapshot");
}

/// Leaves the database exactly as a restore killed mid-run leaves it: the
/// snapshot marked `restore_in_progress` and its attempt still `running`.
///
/// This constructs the *crash*, which is the precondition under test — it is
/// not the state-patching this file used to do. The predecessor test reset
/// `state='complete'` between runs, a step production performs nowhere, and
/// so made retry look supported when in reality the snapshot was wedged.
fn wedge_as_if_killed_mid_restore(conn: &rusqlite::Connection, snapshot_id: i64) {
    conn.execute(
        "UPDATE snapshots SET state='restore_in_progress' WHERE id=?1",
        [snapshot_id],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO restore_attempts (snapshot_id, started_at, state)
         VALUES (?1, 1, 'running')",
        [snapshot_id],
    )
    .unwrap();
}

#[test]
fn restore_reclaims_a_snapshot_wedged_by_a_killed_restore() {
    let src = Server::start("wedged-src");
    src.0
        .run(&[
            "new-session",
            "-n",
            "code",
            "-d",
            "-s",
            "alpha",
            "-c",
            "/tmp",
        ])
        .unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let mut conn = db::open(&tmp.path().join("state.db")).unwrap();
    let snap = capture::snapshot(&mut conn, &src.0, "test").unwrap();
    conn.execute(
        "UPDATE snapshots SET boot_id='boot-previous' WHERE id=?1",
        [snap],
    )
    .unwrap();

    wedge_as_if_killed_mid_restore(&conn, snap);

    // No hand-patching back to 'complete': the engine itself must notice that
    // no restore is actually running and make the snapshot reachable again.
    let dst = Server::start("wedged-dst");
    let report = restore::run_restore(&mut conn, &dst.0, false).unwrap();

    assert_eq!(
        report.snapshot_id,
        Some(snap),
        "a wedged snapshot must not be reported as nothing to restore"
    );
    assert_eq!(report.state, "succeeded");
    assert_eq!(report.outcome.created, vec!["alpha".to_string()]);

    let snap_state: String = conn
        .query_row("SELECT state FROM snapshots WHERE id=?1", [snap], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(snap_state, "restored");

    let orphan_still_running: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM restore_attempts WHERE state='running'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        orphan_still_running, 0,
        "the dead attempt must be marked failed, not left running forever"
    );
}

#[test]
fn a_reclaimed_restore_adopts_live_sessions_instead_of_duplicating() {
    let src = Server::start("readopt-src");
    src.0
        .run(&[
            "new-session",
            "-n",
            "code",
            "-d",
            "-s",
            "alpha",
            "-c",
            "/tmp",
        ])
        .unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let mut conn = db::open(&tmp.path().join("state.db")).unwrap();
    let snap = capture::snapshot(&mut conn, &src.0, "test").unwrap();
    conn.execute(
        "UPDATE snapshots SET boot_id='boot-previous' WHERE id=?1",
        [snap],
    )
    .unwrap();

    let dst = Server::start("readopt-dst");
    let first = restore::run_restore(&mut conn, &dst.0, false).unwrap();
    assert_eq!(first.outcome.created, vec!["alpha".to_string()]);

    // The restore is killed after tmux already created the session but
    // before the terminal write lands — the sessions are live, the snapshot
    // reads `restore_in_progress`.
    wedge_as_if_killed_mid_restore(&conn, snap);

    let second = restore::run_restore(&mut conn, &dst.0, false).unwrap();
    assert_eq!(second.outcome.adopted, vec!["alpha".to_string()]);
    assert!(second.outcome.created.is_empty());
    assert_eq!(
        dst.0.list_sessions().unwrap().len(),
        1,
        "reclaiming must not duplicate sessions the dead restore already made"
    );
}

#[test]
fn error_after_attempt_row_exists_fails_the_attempt_and_keeps_the_snapshot() {
    let src = Server::start("finda-src");
    src.0
        .run(&[
            "new-session",
            "-n",
            "code",
            "-d",
            "-s",
            "alpha",
            "-c",
            "/tmp",
        ])
        .unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let mut conn = db::open(&tmp.path().join("state.db")).unwrap();
    let snap = capture::snapshot(&mut conn, &src.0, "test").unwrap();
    conn.execute(
        "UPDATE snapshots SET boot_id='boot-previous' WHERE id=?1",
        [snap],
    )
    .unwrap();

    // Inject a failure that can only happen AFTER the restore_attempts row
    // already exists as 'running': drop restore_objects so the per-session
    // bookkeeping insert inside run_restore's post-attempt-insert body
    // errors out with a genuine DB error, while restore_attempts and
    // snapshots remain writable and must still receive a terminal state.
    conn.execute("DROP TABLE restore_objects", []).unwrap();

    let dst = Server::start("finda-dst");
    let report = restore::run_restore(&mut conn, &dst.0, false).unwrap();
    assert_eq!(report.state, "failed");

    let snap_state: String = conn
        .query_row("SELECT state FROM snapshots WHERE id=?1", [snap], |r| {
            r.get(0)
        })
        .unwrap();
    // This assertion used to demand `failed`, which encoded the bug: a
    // `failed` snapshot is invisible to `select_restore_source`, so one
    // transient error permanently retired the only copy of the user's
    // pre-reboot state. The snapshot must come back to `complete` — not be
    // left at `restore_in_progress` either, which is equally unreachable.
    assert_eq!(
        snap_state, "complete",
        "a failed attempt must hand the snapshot back to the restorable pool"
    );

    let attempt_state: String = conn
        .query_row(
            "SELECT state FROM restore_attempts WHERE snapshot_id=?1",
            [snap],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        attempt_state, "failed",
        "attempt must not be left at running"
    );

    let finished_at: Option<i64> = conn
        .query_row(
            "SELECT finished_at FROM restore_attempts WHERE snapshot_id=?1",
            [snap],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        finished_at.is_some(),
        "a terminal attempt must record finished_at"
    );
}

/// Makes one captured session's restore fail partway through, **after** tmux
/// has already created the session: the window is given far more panes than
/// its captured 80x24 layout can hold, so `split-window` fails with "no space
/// for a new pane" once the window is full.
///
/// Deliberately *not* "corrupt the captured layout string", which is what
/// this used to do. On tmux 3.3a a malformed layout handed to `select-layout`
/// does not fail the command — it takes the whole server down ("server exited
/// unexpectedly"), destroying every session on it, including the one the test
/// had just verified was restored. tmux 3.7c rejects the same string cleanly
/// ("invalid layout: ..."), which is why that injection looked fine on the
/// developer's machine and wrecked the destination server in CI.
///
/// The overfill failure is clean on both (3.3a and 3.7c), leaves the server
/// running, and is a restore failure users can actually hit: a window
/// captured on a large terminal cannot always be rebuilt on a small one.
fn break_session_restore(conn: &rusqlite::Connection, snap: i64, session: &str) {
    let window_row: i64 = conn
        .query_row(
            "SELECT l.window_row_id FROM session_window_links l
             JOIN session_rows s ON s.row_id = l.session_row_id
             WHERE s.snapshot_id = ?1 AND s.name = ?2",
            rusqlite::params![snap, session],
            |r| r.get(0),
        )
        .unwrap();
    // Four panes already exhaust an 80x24 window on both tmux versions; nine
    // leaves no doubt.
    for idx in 1..9 {
        conn.execute(
            "INSERT INTO pane_rows (window_row_id, tmux_pane_id, idx, cwd, restore_policy)
             VALUES (?1, ?2, ?3, '/tmp', 'shell')",
            rusqlite::params![window_row, format!("%90{idx}"), idx],
        )
        .unwrap();
    }
}

#[test]
fn partial_state_is_reached_when_one_session_fails_and_another_succeeds() {
    let src = Server::start("partial-src");
    src.0
        .run(&[
            "new-session",
            "-n",
            "code",
            "-d",
            "-s",
            "alpha",
            "-c",
            "/tmp",
        ])
        .unwrap();
    src.0
        .run(&[
            "new-session",
            "-n",
            "code",
            "-d",
            "-s",
            "beta",
            "-c",
            "/tmp",
        ])
        .unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let mut conn = db::open(&tmp.path().join("state.db")).unwrap();
    let snap = capture::snapshot(&mut conn, &src.0, "test").unwrap();
    conn.execute(
        "UPDATE snapshots SET boot_id='boot-previous' WHERE id=?1",
        [snap],
    )
    .unwrap();

    // Make beta's restore fail partway through — after tmux has already
    // created the "beta" session (a real, live side effect) but while filling
    // in its one window. alpha's captured data is untouched and restores
    // normally.
    break_session_restore(&conn, snap, "beta");

    let dst = Server::start("partial-dst");
    let report = restore::run_restore(&mut conn, &dst.0, false).unwrap();

    assert_eq!(report.state, "partial");
    assert_eq!(report.outcome.created, vec!["alpha".to_string()]);
    assert_eq!(report.outcome.failed.len(), 1);
    assert_eq!(report.outcome.failed[0].0, "beta");

    let attempt_state: String = conn
        .query_row(
            "SELECT state FROM restore_attempts WHERE snapshot_id=?1",
            [snap],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(attempt_state, "partial");

    let done: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM restore_objects WHERE ref='alpha' AND state='done'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(done, 1);

    let failed_row: (String, Option<String>) = conn
        .query_row(
            "SELECT state, detail FROM restore_objects WHERE ref='beta'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(failed_row.0, "failed");
    assert!(
        failed_row.1.is_some(),
        "a failed restore_objects row must carry error detail"
    );

    let pending: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM restore_objects WHERE state='pending'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(pending, 0, "no restore_objects row may be left pending");
}
