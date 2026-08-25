//! A restore that did not fully succeed must not retire the snapshot.
//!
//! `run_restore` used to mark the snapshot `restored` after a *partial*
//! restore and `failed` after a total one. Neither state is selectable by
//! `select_restore_source`, so a directory that was not mounted yet, or a
//! momentarily unwritable database, permanently retired the only copy of the
//! user's pre-reboot layout — and `osm restore` exited 0 either way, so
//! `osm-restore.service` reported a clean success while the state was gone.

mod common;

use osm::{boot, capture, db, restore, snapshots, tmux::Tmux};

struct Server(Tmux);

impl Server {
    fn start(label: &str) -> Self {
        Server(Tmux::with_socket(&format!(
            "osm-retry-{}-{}",
            label,
            std::process::id()
        )))
    }
    fn t(&self) -> &Tmux {
        &self.0
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        common::shutdown(&self.0);
    }
}

fn snapshot_state(conn: &rusqlite::Connection, snap: i64) -> String {
    conn.query_row("SELECT state FROM snapshots WHERE id=?1", [snap], |r| {
        r.get(0)
    })
    .unwrap()
}

fn still_selectable(conn: &rusqlite::Connection, snap: i64) -> bool {
    let boot_id = boot::current_boot_id().unwrap();
    snapshots::select_restore_source(conn, &boot_id).unwrap() == Some(snap)
}

/// Captures `sessions` from a private server and back-dates the snapshot so a
/// restore will select it.
fn previous_boot_snapshot(
    src: &Server,
    sessions: &[&str],
) -> (tempfile::TempDir, rusqlite::Connection, i64) {
    for name in sessions {
        src.t()
            .run(&["new-session", "-d", "-s", name, "-c", "/tmp"])
            .unwrap();
    }
    let tmp = tempfile::tempdir().unwrap();
    let mut conn = db::open(&tmp.path().join("state.db")).unwrap();
    let snap = capture::snapshot(&mut conn, src.t(), "test").unwrap();
    conn.execute(
        "UPDATE snapshots SET boot_id='boot-previous' WHERE id=?1",
        [snap],
    )
    .unwrap();
    (tmp, conn, snap)
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
fn a_partial_restore_leaves_the_snapshot_restorable() {
    let src = Server::start("partial-src");
    let (_tmp, mut conn, snap) = previous_boot_snapshot(&src, &["alpha", "beta"]);
    break_session_restore(&conn, snap, "beta");

    let dst = Server::start("partial-dst");
    let report = restore::run_restore(&mut conn, dst.t(), false).unwrap();

    assert_eq!(report.state, "partial");
    assert!(report.retryable, "a partial restore must stay retryable");
    assert_eq!(
        snapshot_state(&conn, snap),
        "complete",
        "a partial restore must not retire the snapshot"
    );
    assert!(
        still_selectable(&conn, snap),
        "the next `osm restore` must be able to select the snapshot again"
    );

    // The attempt itself is still recorded as partial: the audit trail keeps
    // the truth even though the source stays live.
    let attempt_state: String = conn
        .query_row(
            "SELECT state FROM restore_attempts WHERE snapshot_id=?1",
            [snap],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(attempt_state, "partial");
}

#[test]
fn a_total_failure_leaves_the_snapshot_restorable() {
    let src = Server::start("total-src");
    let (_tmp, mut conn, snap) = previous_boot_snapshot(&src, &["alpha"]);
    break_session_restore(&conn, snap, "alpha");

    let dst = Server::start("total-dst");
    let report = restore::run_restore(&mut conn, dst.t(), false).unwrap();

    assert_eq!(report.state, "failed");
    assert!(report.retryable);
    assert_eq!(snapshot_state(&conn, snap), "complete");
    assert!(still_selectable(&conn, snap));
}

/// The whole point of keeping the snapshot: a transient problem costs one
/// boot, not the user's state. Here the failure is in the database rather
/// than in tmux, so the first attempt has no live side effects at all.
#[test]
fn a_transient_failure_costs_one_attempt_not_the_snapshot() {
    let src = Server::start("transient-src");
    let (tmp, mut conn, snap) = previous_boot_snapshot(&src, &["alpha"]);
    let db_path = tmp.path().join("state.db");

    conn.execute("DROP TABLE restore_objects", []).unwrap();

    let dst = Server::start("transient-dst");
    let first = restore::run_restore(&mut conn, dst.t(), false).unwrap();
    assert_eq!(first.state, "failed");
    assert!(first.retryable);
    // `list_sessions` errors when no server exists at all, which is itself
    // proof that nothing was created.
    assert!(
        dst.t().list_sessions().unwrap_or_default().is_empty(),
        "nothing should have been created by the failed attempt"
    );

    // The transient condition clears (the table comes back).
    drop(conn);
    let mut conn = db::open(&db_path).unwrap();
    assert!(
        still_selectable(&conn, snap),
        "the snapshot must have survived the failed attempt"
    );

    let second = restore::run_restore(&mut conn, dst.t(), false).unwrap();
    assert_eq!(
        second.state, "succeeded",
        "the retry must restore what the failed attempt could not: {second:?}"
    );
    assert_eq!(second.outcome.created, vec!["alpha".to_string()]);
    assert!(!second.retryable, "a verified success retires the snapshot");
    assert_eq!(snapshot_state(&conn, snap), "restored");
}

/// A retry must adopt the sessions the previous attempt already created
/// rather than building a second copy of each.
#[test]
fn a_retry_adopts_what_the_previous_attempt_already_created() {
    let src = Server::start("readopt-src");
    let (_tmp, mut conn, snap) = previous_boot_snapshot(&src, &["alpha", "beta"]);
    break_session_restore(&conn, snap, "beta");

    let dst = Server::start("readopt-dst");
    let first = restore::run_restore(&mut conn, dst.t(), false).unwrap();
    assert_eq!(first.state, "partial");
    assert_eq!(first.outcome.created, vec!["alpha".to_string()]);
    let live_after_first = dst.t().list_sessions().unwrap().len();

    let second = restore::run_restore(&mut conn, dst.t(), false).unwrap();
    assert_eq!(
        second.snapshot_id,
        Some(snap),
        "the retry must select the same snapshot"
    );
    assert_eq!(
        second.outcome.adopted,
        vec!["alpha".to_string()],
        "the session the first attempt built must be adopted, not rebuilt: {second:?}"
    );
    assert!(second.outcome.created.is_empty());
    assert_eq!(
        dst.t().list_sessions().unwrap().len(),
        live_after_first,
        "a retry must not duplicate sessions"
    );
}

/// A conflicting live session is another way a restore fails to deliver the
/// captured state, and it must keep the snapshot too — the conflict may well
/// be gone by the next run.
#[test]
fn a_conflict_leaves_the_snapshot_restorable() {
    let src = Server::start("conflict-src");
    let (_tmp, mut conn, snap) = previous_boot_snapshot(&src, &["alpha"]);
    src.t()
        .run(&[
            "new-window",
            "-d",
            "-t",
            "alpha",
            "-n",
            "extra",
            "-c",
            "/tmp",
        ])
        .unwrap();
    let snap2 = capture::snapshot(&mut conn, src.t(), "test").unwrap();
    conn.execute(
        "UPDATE snapshots SET boot_id='boot-previous' WHERE id=?1",
        [snap2],
    )
    .unwrap();
    assert_ne!(snap, snap2);

    // Someone else already owns the name, with a different shape.
    let dst = Server::start("conflict-dst");
    dst.t()
        .run(&["new-session", "-d", "-s", "alpha", "-c", "/tmp"])
        .unwrap();

    let report = restore::run_restore(&mut conn, dst.t(), false).unwrap();
    assert_eq!(report.state, "failed", "{report:?}");
    assert_eq!(report.outcome.conflicted.len(), 1, "{report:?}");
    assert!(report.retryable);
    assert!(still_selectable(&conn, snap2));
}

fn snapshot_ids_for_boot(conn: &rusqlite::Connection, boot_id: &str, state: &str) -> Vec<i64> {
    conn.prepare("SELECT id FROM snapshots WHERE boot_id=?1 AND state=?2 ORDER BY id")
        .unwrap()
        .query_map(rusqlite::params![boot_id, state], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn session_names(conn: &rusqlite::Connection, snap: i64) -> Vec<String> {
    let mut names: Vec<String> = conn
        .prepare("SELECT name FROM session_rows WHERE snapshot_id=?1")
        .unwrap()
        .query_map([snap], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    names.sort();
    names
}

/// The crash window a successful restore used to open.
///
/// Every hook fired *by* the restore loses the race for the exclusive restore
/// lock and returns without capturing, and the daemon is still sleeping out
/// its first interval — so between retiring the source and the next capture
/// there was no `complete` snapshot on the machine at all. A power loss there
/// left the next boot with nothing to restore.
#[test]
fn a_successful_restore_publishes_this_boot_before_retiring_the_source() {
    let src = Server::start("publish-src");
    let (_tmp, mut conn, snap) = previous_boot_snapshot(&src, &["alpha", "beta"]);
    drop(src);

    let dst = Server::start("publish-dst");
    let report = restore::run_restore(&mut conn, dst.t(), false).unwrap();
    assert_eq!(report.state, "succeeded", "{report:?}");
    assert_eq!(snapshot_state(&conn, snap), "restored");

    let boot_id = boot::current_boot_id().unwrap();
    let published = snapshot_ids_for_boot(&conn, &boot_id, "complete");
    assert_eq!(
        published.len(),
        1,
        "a verified success must leave this boot's topology on record: {published:?}"
    );
    assert_eq!(
        session_names(&conn, published[0]),
        vec!["alpha".to_string(), "beta".to_string()],
        "the published snapshot must hold what the restore put back"
    );
}

/// And it has to be a real, restorable snapshot — not a marker row. Aged into
/// a previous boot, it is exactly what the next `osm restore` would rebuild.
#[test]
fn the_published_snapshot_is_what_the_next_boot_would_restore() {
    let src = Server::start("published-src");
    let (_tmp, mut conn, _snap) = previous_boot_snapshot(&src, &["alpha"]);
    drop(src);

    let dst = Server::start("published-dst");
    restore::run_restore(&mut conn, dst.t(), false).unwrap();
    let boot_id = boot::current_boot_id().unwrap();
    let published = snapshot_ids_for_boot(&conn, &boot_id, "complete")[0];

    // The machine reboots: this boot's snapshot becomes a previous boot's.
    conn.execute(
        "UPDATE snapshots SET boot_id='boot-just-ended' WHERE id=?1",
        [published],
    )
    .unwrap();
    assert_eq!(
        snapshots::select_restore_source(&conn, &boot_id).unwrap(),
        Some(published),
        "the published snapshot must be the next boot's restore source"
    );

    let next = Server::start("published-next");
    let report = restore::run_restore(&mut conn, next.t(), false).unwrap();
    assert_eq!(report.state, "succeeded", "{report:?}");
    assert_eq!(report.outcome.created, vec!["alpha".to_string()]);
}

/// The other half of the promise: if this boot's snapshot cannot be written,
/// the source is *not* retired, **and the restore does not call itself a
/// success**.
///
/// This used to be stored and reported as `succeeded`. The JSON then said
/// `state: "succeeded"` beside `retryable: true` — a document contradicting
/// itself — and, because the exit status follows the state, `osm restore`
/// exited 0 and told systemd there was nothing to restart. Meanwhile the
/// user's sessions existed only on a running tmux server with no snapshot
/// behind them, which is exactly the state the whole engine exists to prevent.
#[test]
fn a_success_whose_snapshot_cannot_be_published_is_not_reported_as_success() {
    let tmp = tempfile::tempdir().unwrap();
    let mut conn = db::open(&tmp.path().join("state.db")).unwrap();
    // A snapshot with no sessions in it: the restore has nothing to do and
    // succeeds, and the destination server is never started, so capturing it
    // afterwards fails.
    conn.execute(
        "INSERT INTO snapshots (taken_at, boot_id, reason, state)
         VALUES (?1, 'boot-previous', 'test', 'complete')",
        [boot::now_epoch()],
    )
    .unwrap();
    let snap = conn.last_insert_rowid();

    let dst = Server::start("unpublished-dst");
    let report = restore::run_restore(&mut conn, dst.t(), false).unwrap();

    assert_eq!(
        report.state, "unsecured",
        "a restore whose durability step failed is not a success: {report:?}"
    );
    assert_eq!(report.reason, "post_restore_capture_failed", "{report:?}");
    assert!(
        report.retryable,
        "a restore with no replacement snapshot must stay retryable: {report:?}"
    );
    assert_ne!(
        osm::ipc::exit_code_for_state(&report.state),
        0,
        "systemd must see a failure it can restart, not a clean exit"
    );
    assert_eq!(
        snapshot_state(&conn, snap),
        "complete",
        "the source must not be retired when nothing replaced it"
    );
    assert!(still_selectable(&conn, snap));

    // And it is persisted, not merely reported: a later `osm status` or an
    // operator reading the database must see the same thing the exit status
    // said.
    let attempt: String = conn
        .query_row(
            "SELECT state FROM restore_attempts WHERE id = ?1",
            [report.attempt_id.expect("an attempt row")],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        attempt, "unsecured",
        "the attempt row must record the unsecured outcome"
    );
}
