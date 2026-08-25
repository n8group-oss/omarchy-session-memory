//! A snapshot that was only partly restored must not be silently superseded.
//!
//! Restore picks its source by recency. Boot A's snapshot holds `alpha` and
//! `beta`; boot B restores only `alpha`, hands the source back to `complete`,
//! and a hook then captures boot B's *incomplete* topology. On boot C the
//! newest complete snapshot is boot B's, so `beta` is never restored — and
//! retention deletes boot A's snapshot, with no error at any point. The user's
//! session is gone and nothing ever said so.
//!
//! The retry suites stopped short of exactly this: the capture between one
//! boot's restore and the next boot's selection.

mod common;

use osm::{boot, capture, db, restore, snapshots, tmux::Tmux};
use rusqlite::Connection;
use std::collections::HashSet;

struct Server(Tmux);

impl Server {
    fn start(label: &str) -> Self {
        Server(Tmux::with_socket(&format!(
            "osm-carry-{}-{}",
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

fn unresolved(conn: &rusqlite::Connection, snap: i64) -> bool {
    conn.query_row(
        "SELECT unresolved FROM snapshots WHERE id=?1",
        [snap],
        |r| r.get::<_, i64>(0),
    )
    .unwrap()
        == 1
}

/// The sessions a snapshot is still owed, by name.
fn owed(conn: &rusqlite::Connection, snap: i64) -> Vec<String> {
    let mut names: Vec<String> = conn
        .prepare("SELECT name FROM session_rows WHERE snapshot_id=?1 AND unresolved=1")
        .unwrap()
        .query_map([snap], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    names.sort();
    names
}

/// How many panes a snapshot holds for one session.
fn snapshot_panes(conn: &rusqlite::Connection, snap: i64, session: &str) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM pane_rows p
         JOIN session_window_links l ON l.window_row_id = p.window_row_id
         JOIN session_rows s ON s.row_id = l.session_row_id
         WHERE s.snapshot_id = ?1 AND s.name = ?2",
        rusqlite::params![snap, session],
        |r| r.get(0),
    )
    .unwrap()
}

fn snapshot_exists(conn: &rusqlite::Connection, snap: i64) -> bool {
    conn.query_row("SELECT COUNT(*) FROM snapshots WHERE id=?1", [snap], |r| {
        r.get::<_, i64>(0)
    })
    .unwrap()
        == 1
}

fn live_panes(t: &Tmux, session: &str) -> usize {
    t.run(&["list-panes", "-s", "-t", session, "-F", "#{pane_id}"])
        .map(|o| o.lines().count())
        .unwrap_or(0)
}

fn live_session_names(t: &Tmux) -> HashSet<String> {
    t.list_sessions()
        .unwrap_or_default()
        .into_iter()
        .map(|s| s.name)
        .collect()
}

/// Makes one captured session's restore fail after tmux has already created
/// the sessions before it: the window is given far more panes than its
/// captured 80x24 layout can hold, so `split-window` fails once it is full.
/// Same injection as `tests/restore_retry.rs`, and clean on 3.3a and 3.7c.
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
    for idx in 1..9 {
        conn.execute(
            "INSERT INTO pane_rows (window_row_id, tmux_pane_id, idx, cwd, restore_policy)
             VALUES (?1, ?2, ?3, '/tmp', 'shell')",
            rusqlite::params![window_row, format!("%90{idx}"), idx],
        )
        .unwrap();
    }
}

/// The same injection, aimed at one named window, so breaking a session that
/// shares a window with another does not break the sharer too.
fn break_window_restore(conn: &rusqlite::Connection, snap: i64, session: &str, window: &str) {
    let window_row: i64 = conn
        .query_row(
            "SELECT w.row_id FROM window_rows w
             JOIN session_window_links l ON l.window_row_id = w.row_id
             JOIN session_rows s ON s.row_id = l.session_row_id
             WHERE s.snapshot_id = ?1 AND s.name = ?2 AND w.name = ?3",
            rusqlite::params![snap, session, window],
            |r| r.get(0),
        )
        .unwrap();
    for idx in 1..9 {
        conn.execute(
            "INSERT INTO pane_rows (window_row_id, tmux_pane_id, idx, cwd, restore_policy)
             VALUES (?1, ?2, ?3, '/tmp', 'shell')",
            rusqlite::params![window_row, format!("%80{idx}"), idx],
        )
        .unwrap();
    }
}

/// Undo it: the condition that made the restore fail clears (a smaller
/// window, a mounted volume, whatever it was).
fn unbreak_session_restore(conn: &rusqlite::Connection, snap: i64, session: &str) {
    conn.execute(
        "DELETE FROM pane_rows WHERE idx >= 1 AND window_row_id IN (
           SELECT l.window_row_id FROM session_window_links l
           JOIN session_rows s ON s.row_id = l.session_row_id
           WHERE s.snapshot_id = ?1 AND s.name = ?2)",
        rusqlite::params![snap, session],
    )
    .unwrap();
}

/// Undo [`break_window_restore`] for one named window.
fn unbreak_window_restore(conn: &rusqlite::Connection, snap: i64, session: &str, window: &str) {
    conn.execute(
        "DELETE FROM pane_rows WHERE idx >= 1 AND window_row_id IN (
           SELECT w.row_id FROM window_rows w
           JOIN session_window_links l ON l.window_row_id = w.row_id
           JOIN session_rows s ON s.row_id = l.session_row_id
           WHERE s.snapshot_id = ?1 AND s.name = ?2 AND w.name = ?3)",
        rusqlite::params![snap, session, window],
    )
    .unwrap();
}

/// The full A → B → C sequence, capture included — including the state the
/// failed restore actually leaves behind.
///
/// The old version of this test killed the half-built `beta` before capturing,
/// which quietly removed the hard case: a restore that fails partway leaves a
/// *truncated* session under the captured name. Carry-forward used to treat
/// "a session by that name is live" as recovery, so that truncated session
/// discharged the debt, the newest snapshot held four panes where the source
/// held nine, and retention was then free to delete the source. Nothing
/// reported anything.
#[test]
fn a_session_the_restore_missed_survives_into_the_next_generation() {
    // --- Boot A: alpha and beta are captured.
    let src = Server::start("abc-src");
    for name in ["alpha", "beta"] {
        src.t()
            .run(&["new-session", "-d", "-s", name, "-c", "/tmp"])
            .unwrap();
    }
    let tmp = tempfile::tempdir().unwrap();
    let mut conn = db::open(&tmp.path().join("state.db")).unwrap();
    let snap_a = capture::snapshot(&mut conn, src.t(), "test").unwrap();
    conn.execute(
        "UPDATE snapshots SET boot_id='boot-a' WHERE id=?1",
        [snap_a],
    )
    .unwrap();
    drop(src);
    break_session_restore(&conn, snap_a, "beta");
    let captured_beta_panes = snapshot_panes(&conn, snap_a, "beta");
    assert_eq!(captured_beta_panes, 9, "the injection must widen beta");

    // --- Boot B: only alpha comes back.
    let dst = Server::start("abc-b");
    let report = restore::run_restore(&mut conn, dst.t(), false).unwrap();
    assert_eq!(report.state, "partial", "{report:?}");
    assert_eq!(report.outcome.created, vec!["alpha".to_string()]);
    assert_eq!(
        owed(&conn, snap_a),
        vec!["beta".to_string()],
        "a partial restore must owe exactly the session it did not deliver"
    );
    assert!(unresolved(&conn, snap_a));

    // The injection fails late enough that tmux keeps the session and the
    // panes it had already built, so `beta` really is live — and really is not
    // the captured `beta`.
    assert!(
        live_session_names(dst.t()).contains("beta"),
        "the fixture must leave a truncated beta behind"
    );
    assert!(
        (live_panes(dst.t(), "beta") as i64) < captured_beta_panes,
        "the live beta must be missing panes for this test to mean anything"
    );

    // --- Boot B, a hook fires while the truncated beta is still there.
    let snap_b = capture::snapshot(&mut conn, dst.t(), "hook").unwrap();
    assert_eq!(
        session_names(&conn, snap_b),
        vec!["alpha".to_string(), "beta".to_string()],
        "the capture records what is live"
    );
    assert!(
        snapshot_panes(&conn, snap_b, "beta") < captured_beta_panes,
        "…and what is live is the truncated beta"
    );
    assert_eq!(
        owed(&conn, snap_a),
        vec!["beta".to_string()],
        "a live session that is not the captured one must not discharge the debt"
    );
    assert!(
        !owed(&conn, snap_a).contains(&"alpha".to_string()),
        "the session the restore did deliver must not still be owed"
    );

    // Retention cannot take the only copy of the nine-pane beta.
    snapshots::prune(&conn, 1, &boot::current_boot_id().unwrap()).unwrap();
    assert!(
        snapshot_exists(&conn, snap_a),
        "the snapshot that still owes beta must survive retention"
    );

    // The user closes the broken session. Now — and only now — the debt can
    // travel forward.
    dst.t().run(&["kill-session", "-t", "beta"]).unwrap();
    let snap_c = capture::snapshot(&mut conn, dst.t(), "hook").unwrap();
    assert_eq!(
        session_names(&conn, snap_c),
        vec!["alpha".to_string(), "beta".to_string()],
        "the capture after an incomplete restore must carry the session the \
         restore did not put back"
    );
    assert_eq!(
        snapshot_panes(&conn, snap_c, "beta"),
        captured_beta_panes,
        "…all of it, not the truncated live copy"
    );
    assert!(
        !unresolved(&conn, snap_a),
        "boot A's snapshot is superseded now that its content travelled forward"
    );
    assert_eq!(
        owed(&conn, snap_c),
        vec!["beta".to_string()],
        "the new snapshot holds a session nothing has recovered"
    );

    // A second capture in the same boot keeps carrying it: the marker moves
    // with the content, so the chain does not break at generation two.
    let snap_d = capture::snapshot(&mut conn, dst.t(), "hook").unwrap();
    assert_eq!(
        session_names(&conn, snap_d),
        vec!["alpha".to_string(), "beta".to_string()],
        "the second capture must carry it too"
    );

    // Retention runs, keeping a single snapshot.
    snapshots::prune(&conn, 1, &boot::current_boot_id().unwrap()).unwrap();

    // --- Boot C. The machine rebooted, so boot B's snapshots are a previous
    // boot's, and whatever made beta fail has cleared.
    conn.execute(
        "UPDATE snapshots SET boot_id='boot-b' WHERE boot_id NOT IN ('boot-a')",
        [],
    )
    .unwrap();
    let selected = snapshots::select_restore_source(&conn, &boot::current_boot_id().unwrap())
        .unwrap()
        .expect("a restore source must still exist");
    unbreak_session_restore(&conn, selected, "beta");

    let next = Server::start("abc-c");
    let report = restore::run_restore(&mut conn, next.t(), false).unwrap();
    assert_eq!(report.state, "succeeded", "{report:?}");
    assert_eq!(
        live_session_names(next.t()),
        HashSet::from(["alpha".to_string(), "beta".to_string()]),
        "boot A's beta must come back on boot C: {report:?}"
    );
}

/// Retention must not be able to delete the one snapshot that still holds an
/// unrecovered session.
#[test]
fn retention_keeps_an_unresolved_snapshot() {
    let tmp = tempfile::tempdir().unwrap();
    let conn = db::open(&tmp.path().join("state.db")).unwrap();
    let boot_id = boot::current_boot_id().unwrap();
    for i in 1..=5 {
        conn.execute(
            "INSERT INTO snapshots (id, taken_at, boot_id, reason, state, unresolved)
             VALUES (?1, ?1, ?2, 'test', 'complete', ?3)",
            rusqlite::params![i, boot_id, i64::from(i == 1)],
        )
        .unwrap();
    }

    snapshots::prune(&conn, 1, &boot_id).unwrap();

    let kept: Vec<i64> = conn
        .prepare("SELECT id FROM snapshots ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(
        kept.contains(&1),
        "the unresolved snapshot must survive retention, kept: {kept:?}"
    );
    assert!(kept.contains(&5), "the newest must survive too: {kept:?}");
}

/// Carrying forward must keep a linked window linked: the two carried
/// sessions have to end up sharing one window row, not one each.
#[test]
fn a_carried_linked_window_stays_one_window() {
    let src = Server::start("link-src");
    let t = src.t();
    t.run(&[
        "new-session",
        "-d",
        "-s",
        "alpha",
        "-n",
        "own",
        "-c",
        "/tmp",
    ])
    .unwrap();
    t.run(&[
        "new-window",
        "-d",
        "-t",
        "alpha",
        "-n",
        "shared",
        "-c",
        "/tmp",
    ])
    .unwrap();
    t.run(&[
        "new-session",
        "-d",
        "-s",
        "beta",
        "-n",
        "own2",
        "-c",
        "/tmp",
    ])
    .unwrap();
    t.run(&["link-window", "-d", "-s", "alpha:shared", "-t", "beta:"])
        .unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let mut conn = db::open(&tmp.path().join("state.db")).unwrap();
    let snap = capture::snapshot(&mut conn, t, "test").unwrap();
    drop(src);
    // Nothing recovered either session; both must travel forward together.
    snapshots::set_unresolved(&conn, snap, true).unwrap();

    let dst = Server::start("link-dst");
    dst.t()
        .run(&["new-session", "-d", "-s", "gamma", "-c", "/tmp"])
        .unwrap();
    let carried = capture::snapshot(&mut conn, dst.t(), "hook").unwrap();

    assert_eq!(
        session_names(&conn, carried),
        vec!["alpha".to_string(), "beta".to_string(), "gamma".to_string()]
    );
    let shared_rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM window_rows WHERE snapshot_id=?1 AND name='shared'",
            [carried],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        shared_rows, 1,
        "a carried linked window must stay one window row"
    );
    let links: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM session_window_links l
             JOIN window_rows w ON w.row_id = l.window_row_id
             WHERE w.snapshot_id=?1 AND w.name='shared'",
            [carried],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(links, 2, "…linked into both carried sessions");
}

/// A live session owns its name: the snapshot's copy is not carried on top of
/// it, because a snapshot holding two sessions with one name could never be
/// restored.
#[test]
fn a_session_whose_name_is_live_is_not_carried() {
    let src = Server::start("name-src");
    src.t()
        .run(&["new-session", "-d", "-s", "alpha", "-c", "/tmp"])
        .unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let mut conn = db::open(&tmp.path().join("state.db")).unwrap();
    let snap = capture::snapshot(&mut conn, src.t(), "test").unwrap();
    drop(src);
    snapshots::set_unresolved(&conn, snap, true).unwrap();

    let dst = Server::start("name-dst");
    dst.t()
        .run(&["new-session", "-d", "-s", "alpha", "-c", "/tmp"])
        .unwrap();
    let carried = capture::snapshot(&mut conn, dst.t(), "hook").unwrap();

    assert_eq!(session_names(&conn, carried), vec!["alpha".to_string()]);
    assert!(
        !unresolved(&conn, carried),
        "nothing was carried, so the new snapshot has nothing outstanding"
    );

    // The other half, and the half the test used to leave out: the debt is
    // *discharged*, not merely not carried. Without these the capture could
    // resolve nothing at all — leaving the old snapshot owed forever, exempt
    // from retention forever, and permanently ahead of every newer snapshot in
    // the eyes of anything that reads the debt — and the test would still pass.
    assert!(
        owed(&conn, snap).is_empty(),
        "the source's session is verifiably back, so its debt must be gone: {:?}",
        owed(&conn, snap)
    );
    assert!(
        !unresolved(&conn, snap),
        "…and the snapshot-level cache of that debt must be cleared with it"
    );
}

/// The other way boot B ends up holding only `alpha`: the restore process was
/// killed between the two sessions. It never reached any reporting code, so
/// the marker has to be set when the restore *starts*, not when it finishes.
#[test]
fn a_capture_after_an_interrupted_restore_carries_what_it_never_reached() {
    let src = Server::start("killed-src");
    for name in ["alpha", "beta"] {
        src.t()
            .run(&["new-session", "-d", "-s", name, "-c", "/tmp"])
            .unwrap();
    }
    let tmp = tempfile::tempdir().unwrap();
    let mut conn = db::open(&tmp.path().join("state.db")).unwrap();
    let snap = capture::snapshot(&mut conn, src.t(), "test").unwrap();
    conn.execute("UPDATE snapshots SET boot_id='boot-a' WHERE id=?1", [snap])
        .unwrap();
    let tree = osm::model::load(&conn, snap).unwrap();
    drop(src);

    // What a restore killed after `alpha` leaves behind: alpha live, the
    // snapshot wedged in `restore_in_progress`, the attempt still `running`.
    let dst = Server::start("killed-dst");
    let alpha_only = osm::model::SnapshotTree {
        snapshot_id: tree.snapshot_id,
        sessions: tree
            .sessions
            .iter()
            .filter(|s| s.name == "alpha")
            .cloned()
            .collect(),
    };
    restore::restore_tree(dst.t(), &alpha_only).unwrap();
    snapshots::set_state(&conn, snap, "restore_in_progress").unwrap();
    snapshots::set_unresolved(&conn, snap, true).unwrap();
    conn.execute(
        "INSERT INTO restore_attempts (snapshot_id, started_at, state)
         VALUES (?1, ?2, 'running')",
        rusqlite::params![snap, boot::now_epoch()],
    )
    .unwrap();

    let carried = capture::snapshot(&mut conn, dst.t(), "hook").unwrap();
    assert_eq!(
        session_names(&conn, carried),
        vec!["alpha".to_string(), "beta".to_string()],
        "the capture must carry the session the killed restore never reached"
    );
}

/// A window linked into two sessions, where a restore delivered exactly one of
/// them.
///
/// `alpha` and `beta` share window `shared`. The restore rebuilds `alpha` — and
/// with it the shared window, under a new id — and fails on `beta`. The capture
/// that follows sees `alpha` live and has to carry `beta`, and the only fact on
/// the machine that says the live `@N` *is* the captured shared window is the
/// restore's own record of having built it. Without that, `beta` got a second,
/// independent copy of the window and the link relation was gone with nothing
/// reported.
///
/// The existing linked-window test carries both sharers together, which never
/// exercises this; the check here runs a *subsequent restore* and asks the
/// server whether the two sessions really are holding one window.
#[test]
fn a_linked_window_survives_a_restore_that_delivered_only_one_sharer() {
    let src = Server::start("mixed-src");
    let t = src.t();
    t.run(&[
        "new-session",
        "-d",
        "-s",
        "alpha",
        "-n",
        "own",
        "-c",
        "/tmp",
    ])
    .unwrap();
    t.run(&[
        "new-window",
        "-d",
        "-t",
        "alpha",
        "-n",
        "shared",
        "-c",
        "/tmp",
    ])
    .unwrap();
    t.run(&[
        "new-session",
        "-d",
        "-s",
        "beta",
        "-n",
        "own2",
        "-c",
        "/tmp",
    ])
    .unwrap();
    t.run(&["link-window", "-d", "-s", "alpha:shared", "-t", "beta:"])
        .unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let mut conn = db::open(&tmp.path().join("state.db")).unwrap();
    let snap_a = capture::snapshot(&mut conn, t, "test").unwrap();
    conn.execute(
        "UPDATE snapshots SET boot_id='boot-a' WHERE id=?1",
        [snap_a],
    )
    .unwrap();
    drop(src);

    // Only `beta`'s *own* window is broken. Breaking the shared one would break
    // alpha's restore too, and then there would be no live sharer at all.
    break_window_restore(&conn, snap_a, "beta", "own2");

    // --- Boot B: alpha comes back, beta does not.
    let dst = Server::start("mixed-b");
    let report = restore::run_restore(&mut conn, dst.t(), false).unwrap();
    assert_eq!(
        report.outcome.created,
        vec!["alpha".to_string()],
        "{report:?}"
    );
    assert_eq!(
        owed(&conn, snap_a),
        vec!["beta".to_string()],
        "only the session that was not delivered is still owed"
    );
    let _ = dst.t().run(&["kill-session", "-t", "beta"]);

    // --- The capture that has to reconcile one live sharer with one carried one.
    let snap_b = capture::snapshot(&mut conn, dst.t(), "hook").unwrap();
    assert_eq!(
        session_names(&conn, snap_b),
        vec!["alpha".to_string(), "beta".to_string()]
    );
    let shared_rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM window_rows WHERE snapshot_id=?1 AND name='shared'",
            [snap_b],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        shared_rows, 1,
        "the carried session must link into the live window, not copy it"
    );
    let links: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM session_window_links l
             JOIN window_rows w ON w.row_id = l.window_row_id
             WHERE w.snapshot_id=?1 AND w.name='shared'",
            [snap_b],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(links, 2, "…linked into both sessions");

    // --- Boot C: the snapshot has to rebuild it as one window on the server.
    conn.execute(
        "UPDATE snapshots SET boot_id='boot-b' WHERE boot_id NOT IN ('boot-a')",
        [],
    )
    .unwrap();
    let selected = snapshots::select_restore_source(&conn, &boot::current_boot_id().unwrap())
        .unwrap()
        .expect("a restore source must still exist");
    assert_eq!(selected, snap_b);
    unbreak_window_restore(&conn, snap_b, "beta", "own2");

    let next = Server::start("mixed-c");
    let report = restore::run_restore(&mut conn, next.t(), false).unwrap();
    assert_eq!(report.state, "succeeded", "{report:?}");

    let windows = next.t().list_windows().unwrap();
    let sessions = next.t().list_sessions().unwrap();
    let shared_in = |session: &str| -> Vec<String> {
        let id = &sessions
            .iter()
            .find(|s| s.name == session)
            .expect("session is live")
            .id;
        windows
            .iter()
            .filter(|w| &w.session_id == id && w.name == "shared")
            .map(|w| w.id.clone())
            .collect()
    };
    let in_alpha = shared_in("alpha");
    let in_beta = shared_in("beta");
    assert_eq!(in_alpha.len(), 1, "alpha must hold the shared window");
    assert_eq!(in_beta.len(), 1, "beta must hold the shared window");
    assert_eq!(
        in_alpha, in_beta,
        "the two sessions must hold *one* window, not a copy each"
    );
}

/// Carry debt is created by a restore that did not deliver a session, never by
/// a capture — so closing a session that *was* delivered leaves it closed.
///
/// The old whole-snapshot marker made every session of an unresolved snapshot
/// carry-forward's business. `alpha` came back, the user closed it, and the
/// next capture put it straight back into the newest snapshot, from where the
/// next boot would restore it: the engine undoing a deliberate action.
#[test]
fn closing_a_session_the_restore_delivered_does_not_bring_it_back() {
    let src = Server::start("tomb-src");
    for name in ["alpha", "beta"] {
        src.t()
            .run(&["new-session", "-d", "-s", name, "-c", "/tmp"])
            .unwrap();
    }
    let tmp = tempfile::tempdir().unwrap();
    let mut conn = db::open(&tmp.path().join("state.db")).unwrap();
    let snap_a = capture::snapshot(&mut conn, src.t(), "test").unwrap();
    conn.execute(
        "UPDATE snapshots SET boot_id='boot-a' WHERE id=?1",
        [snap_a],
    )
    .unwrap();
    drop(src);
    break_session_restore(&conn, snap_a, "beta");

    let dst = Server::start("tomb-b");
    let report = restore::run_restore(&mut conn, dst.t(), false).unwrap();
    assert_eq!(report.state, "partial", "{report:?}");
    let _ = dst.t().run(&["kill-session", "-t", "beta"]);
    // Something else the user is working in, so closing `alpha` below closes a
    // session rather than the whole tmux server.
    dst.t()
        .run(&["new-session", "-d", "-s", "keeper", "-c", "/tmp"])
        .unwrap();
    let snap_b = capture::snapshot(&mut conn, dst.t(), "hook").unwrap();
    assert_eq!(
        session_names(&conn, snap_b),
        vec![
            "alpha".to_string(),
            "beta".to_string(),
            "keeper".to_string()
        ]
    );
    assert_eq!(
        owed(&conn, snap_b),
        vec!["beta".to_string()],
        "only the carried session is owed; the live one is simply live"
    );

    // The user closes the session the restore *did* deliver.
    dst.t().run(&["kill-session", "-t", "alpha"]).unwrap();
    let snap_c = capture::snapshot(&mut conn, dst.t(), "hook").unwrap();
    assert_eq!(
        session_names(&conn, snap_c),
        vec!["beta".to_string(), "keeper".to_string()],
        "a session that was delivered and then closed must stay closed"
    );
}

/// The same debt, under repeated renames: the snapshot must not grow a name
/// per generation.
///
/// Every rename made the old name "missing" from the live server, and the old
/// marker said the whole snapshot was outstanding, so each capture carried one
/// more corpse forward. Ten renames, ten dead sessions, and every one of them
/// would have been restored on the next boot.
#[test]
fn renaming_a_live_session_does_not_accumulate_dead_names() {
    let src = Server::start("rename-src");
    for name in ["alpha", "beta"] {
        src.t()
            .run(&["new-session", "-d", "-s", name, "-c", "/tmp"])
            .unwrap();
    }
    let tmp = tempfile::tempdir().unwrap();
    let mut conn = db::open(&tmp.path().join("state.db")).unwrap();
    let snap = capture::snapshot(&mut conn, src.t(), "test").unwrap();
    conn.execute("UPDATE snapshots SET boot_id='boot-a' WHERE id=?1", [snap])
        .unwrap();
    drop(src);
    break_session_restore(&conn, snap, "beta");

    let dst = Server::start("rename-b");
    restore::run_restore(&mut conn, dst.t(), false).unwrap();
    let _ = dst.t().run(&["kill-session", "-t", "beta"]);

    let mut current = "alpha".to_string();
    let mut last = capture::snapshot(&mut conn, dst.t(), "hook").unwrap();
    for generation in 1..=4 {
        let next = format!("alpha-{generation}");
        dst.t()
            .run(&["rename-session", "-t", &current, &next])
            .unwrap();
        current = next;
        last = capture::snapshot(&mut conn, dst.t(), "hook").unwrap();
        assert_eq!(
            session_names(&conn, last),
            vec![current.clone(), "beta".to_string()],
            "generation {generation} must hold the live session under its \
             current name and the one session that is genuinely owed — \
             nothing else"
        );
    }
    assert_eq!(
        owed(&conn, last),
        vec!["beta".to_string()],
        "the debt must not grow with every rename"
    );
}

/// Delete exactly the pane rows [`break_window_restore`] added, leaving the
/// window's real panes alone whatever `pane-base-index` the running tmux uses.
fn remove_injected_panes(conn: &rusqlite::Connection, snap: i64, session: &str, window: &str) {
    let window_row: i64 = conn
        .query_row(
            "SELECT w.row_id FROM window_rows w
             JOIN session_window_links l ON l.window_row_id = w.row_id
             JOIN session_rows s ON s.row_id = l.session_row_id
             WHERE s.snapshot_id = ?1 AND s.name = ?2 AND w.name = ?3",
            rusqlite::params![snap, session, window],
            |r| r.get(0),
        )
        .unwrap();
    for idx in 1..9 {
        conn.execute(
            "DELETE FROM pane_rows WHERE window_row_id = ?1 AND tmux_pane_id = ?2",
            rusqlite::params![window_row, format!("%80{idx}")],
        )
        .unwrap();
    }
}

/// `alpha` and `beta` sharing one window called `shared`, captured as boot A,
/// then boot B's restore — which delivers `alpha` (and with it the shared
/// window, under a new id) and fails on `beta`, leaving `beta` owed.
///
/// The truncated `beta` the failure leaves behind is closed here, so what each
/// caller builds in its place is the only `beta` on the server.
fn a_restore_that_delivered_only_alpha(
    label: &str,
) -> (tempfile::TempDir, Connection, i64, Server) {
    let src = Server::start(&format!("{label}-src"));
    let t = src.t();
    for (name, own) in [("alpha", "own"), ("beta", "own2")] {
        t.run(&["new-session", "-d", "-s", name, "-n", own, "-c", "/tmp"])
            .unwrap();
    }
    t.run(&[
        "new-window",
        "-d",
        "-t",
        "alpha",
        "-n",
        "shared",
        "-c",
        "/tmp",
    ])
    .unwrap();
    t.run(&["link-window", "-d", "-s", "alpha:shared", "-t", "beta:"])
        .unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let mut conn = db::open(&tmp.path().join("state.db")).unwrap();
    let snap_a = capture::snapshot(&mut conn, t, "test").unwrap();
    conn.execute(
        "UPDATE snapshots SET boot_id='boot-a' WHERE id=?1",
        [snap_a],
    )
    .unwrap();
    drop(src);
    // Only `beta`'s own window; breaking the shared one would break alpha's
    // restore too, and then there would be no live sharer at all.
    break_window_restore(&conn, snap_a, "beta", "own2");

    let dst = Server::start(&format!("{label}-dst"));
    let report = restore::run_restore(&mut conn, dst.t(), false).unwrap();
    assert_eq!(
        report.outcome.created,
        vec!["alpha".to_string()],
        "{report:?}"
    );
    assert_eq!(
        owed(&conn, snap_a),
        vec!["beta".to_string()],
        "only the session that was not delivered is still owed"
    );
    let _ = dst.t().run(&["kill-session", "-t", "beta"]);
    // The injection has done its job. Removing exactly the rows it added —
    // by id, not by index, since a pane's index depends on the running
    // tmux's `pane-base-index` — leaves the captured `beta` as it really was:
    // one pane per window. A live `beta` built the same way can then match
    // it, so the tests below turn on the *link* rather than on nine panes
    // that were never there.
    remove_injected_panes(&conn, snap_a, "beta", "own2");
    (tmp, conn, snap_a, dst)
}

/// The control for the test below, and it is load-bearing: it builds `beta`
/// the *right* way — linking the very window `alpha` is holding — so the two
/// really are one window on the server. The debt must be discharged here, or
/// the check that refuses the lookalike is simply refusing everything.
#[test]
fn a_sharer_that_really_holds_the_shared_window_discharges_its_debt() {
    let (_tmp, mut conn, snap_a, dst) = a_restore_that_delivered_only_alpha("relink");

    dst.t()
        .run(&[
            "new-session",
            "-d",
            "-s",
            "beta",
            "-n",
            "own2",
            "-c",
            "/tmp",
        ])
        .unwrap();
    dst.t()
        .run(&["link-window", "-d", "-s", "alpha:shared", "-t", "beta:"])
        .unwrap();

    capture::snapshot(&mut conn, dst.t(), "hook").unwrap();

    assert!(
        owed(&conn, snap_a).is_empty(),
        "a beta that holds the shared window is the captured beta: {:?}",
        owed(&conn, snap_a)
    );
    assert!(
        !unresolved(&conn, snap_a),
        "…and the snapshot-level cache must say so too"
    );
}

/// The lookalike. Same indices, same window names, same layouts, same
/// directories, same focus — and a window of its own where the snapshot has a
/// window shared with `alpha`.
///
/// Compared session by session it is indistinguishable from the captured
/// `beta`, so its debt was discharged, the new snapshot recorded two
/// independent windows, and retention was then free to delete the only
/// snapshot that still knew the two were one. The link is a property *between*
/// sessions, and verifying each session on its own could never see it.
#[test]
fn a_lookalike_holding_its_own_window_does_not_discharge_a_shared_debt() {
    let (_tmp, mut conn, snap_a, dst) = a_restore_that_delivered_only_alpha("lookalike");

    // Built exactly like the control above, except that `shared` is a new
    // window instead of the one `alpha` holds.
    dst.t()
        .run(&[
            "new-session",
            "-d",
            "-s",
            "beta",
            "-n",
            "own2",
            "-c",
            "/tmp",
        ])
        .unwrap();
    dst.t()
        .run(&[
            "new-window",
            "-d",
            "-t",
            "beta",
            "-n",
            "shared",
            "-c",
            "/tmp",
        ])
        .unwrap();

    let snap_b = capture::snapshot(&mut conn, dst.t(), "hook").unwrap();
    assert_eq!(
        session_names(&conn, snap_b),
        vec!["alpha".to_string(), "beta".to_string()],
        "the capture records what is live"
    );

    assert_eq!(
        owed(&conn, snap_a),
        vec!["beta".to_string()],
        "a beta holding its own window is not the beta that shared one"
    );
    assert!(unresolved(&conn, snap_a));

    // Which is what keeps the only record of the link out of retention's
    // reach.
    snapshots::prune(&conn, 1, &boot::current_boot_id().unwrap()).unwrap();
    assert!(
        snapshot_exists(&conn, snap_a),
        "the snapshot that still proves alpha and beta shared a window must \
         survive retention"
    );
}

/// `alpha`, `beta` and `gamma` all sharing one window called `shared`,
/// captured as boot A; boot B's restore delivers `alpha` and `gamma` and fails
/// on `beta`.
///
/// A hook then captures the result — which is where the debt goes to live:
/// `beta` is carried into the new snapshot, linked to the very window `alpha`
/// came back holding, and boot A's snapshot is resolved and left behind. From
/// that moment nothing on the machine has a *restore attempt* for the snapshot
/// that owes `beta`, so the live sessions sharing that window are the only
/// evidence there is.
///
/// The user then closes `gamma`, which the restore did bring back. Returns the
/// snapshot that is owed `beta`.
fn a_carried_debt_with_a_closed_third_sharer(
    label: &str,
) -> (tempfile::TempDir, Connection, i64, Server) {
    let src = Server::start(&format!("{label}-src"));
    let t = src.t();
    for (name, own) in [("alpha", "own"), ("beta", "own2"), ("gamma", "own3")] {
        t.run(&["new-session", "-d", "-s", name, "-n", own, "-c", "/tmp"])
            .unwrap();
    }
    t.run(&[
        "new-window",
        "-d",
        "-t",
        "alpha",
        "-n",
        "shared",
        "-c",
        "/tmp",
    ])
    .unwrap();
    for other in ["beta:", "gamma:"] {
        t.run(&["link-window", "-d", "-s", "alpha:shared", "-t", other])
            .unwrap();
    }

    let tmp = tempfile::tempdir().unwrap();
    let mut conn = db::open(&tmp.path().join("state.db")).unwrap();
    let snap_a = capture::snapshot(&mut conn, t, "test").unwrap();
    conn.execute(
        "UPDATE snapshots SET boot_id='boot-a' WHERE id=?1",
        [snap_a],
    )
    .unwrap();
    drop(src);
    // Only `beta`'s own window: breaking the shared one would break every
    // sharer's restore.
    break_window_restore(&conn, snap_a, "beta", "own2");

    let dst = Server::start(&format!("{label}-dst"));
    let report = restore::run_restore(&mut conn, dst.t(), false).unwrap();
    assert_eq!(
        report.outcome.created,
        vec!["alpha".to_string(), "gamma".to_string()],
        "{report:?}"
    );
    assert_eq!(
        owed(&conn, snap_a),
        vec!["beta".to_string()],
        "only the session that was not delivered is still owed"
    );
    let _ = dst.t().run(&["kill-session", "-t", "beta"]);
    remove_injected_panes(&conn, snap_a, "beta", "own2");

    // The capture that moves the debt onto a snapshot of its own — with
    // `gamma` still live, so it is recorded as a sharer of that window.
    let snap_b = capture::snapshot(&mut conn, dst.t(), "hook").unwrap();
    assert_eq!(
        owed(&conn, snap_b),
        vec!["beta".to_string()],
        "the debt must have moved to the new snapshot with the content"
    );
    assert!(
        owed(&conn, snap_a).is_empty(),
        "…and left the old one, which is what makes the restore attempt's \
         window map irrelevant from here on"
    );

    // The user closes a session the restore delivered. `alpha` keeps the
    // shared window; `gamma` simply stops being evidence about anything.
    dst.t().run(&["kill-session", "-t", "gamma"]).unwrap();
    (tmp, conn, snap_b, dst)
}

/// One deliberately closed sharer must not pin an unrelated debt forever.
///
/// `beta` comes back the only way it can — linked to the exact window `alpha`
/// still holds — so on the server it *is* the captured `beta`. The check
/// demanded corroboration from every historic sharer of that window instead of
/// establishing the window's live identity once, so `gamma`, which the user
/// closed, supplied nothing, the debt could never be discharged again, and the
/// snapshot holding it was permanently exempt from retention.
#[test]
fn a_closed_third_sharer_does_not_pin_a_relinked_sessions_debt() {
    let (_tmp, mut conn, snap_b, dst) = a_carried_debt_with_a_closed_third_sharer("threeway");

    dst.t()
        .run(&[
            "new-session",
            "-d",
            "-s",
            "beta",
            "-n",
            "own2",
            "-c",
            "/tmp",
        ])
        .unwrap();
    dst.t()
        .run(&["link-window", "-d", "-s", "alpha:shared", "-t", "beta:"])
        .unwrap();

    capture::snapshot(&mut conn, dst.t(), "hook").unwrap();

    assert!(
        owed(&conn, snap_b).is_empty(),
        "a beta holding the very window alpha holds is the captured beta, \
         whatever became of gamma: {:?}",
        owed(&conn, snap_b)
    );
    assert!(
        !unresolved(&conn, snap_b),
        "…and the snapshot-level cache must say so too"
    );
}

/// The control, and it must hold both before and after the fix above: the same
/// three-sharer fixture, the same closed `gamma`, and a `beta` built with a
/// window of its own.
///
/// It must stay owed. Establishing the shared window's identity once — rather
/// than demanding evidence from every historic sharer — must not become
/// "discharge everything": the live `alpha` still says which window that is,
/// and this `beta` is not holding it.
#[test]
fn a_lookalike_is_still_refused_when_a_third_sharer_is_gone() {
    let (_tmp, mut conn, snap_b, dst) = a_carried_debt_with_a_closed_third_sharer("threeway-look");

    dst.t()
        .run(&[
            "new-session",
            "-d",
            "-s",
            "beta",
            "-n",
            "own2",
            "-c",
            "/tmp",
        ])
        .unwrap();
    dst.t()
        .run(&[
            "new-window",
            "-d",
            "-t",
            "beta",
            "-n",
            "shared",
            "-c",
            "/tmp",
        ])
        .unwrap();

    capture::snapshot(&mut conn, dst.t(), "hook").unwrap();

    assert_eq!(
        owed(&conn, snap_b),
        vec!["beta".to_string()],
        "a beta holding its own window is not the beta that shared one"
    );
    assert!(unresolved(&conn, snap_b));

    snapshots::prune(&conn, 1, &boot::current_boot_id().unwrap()).unwrap();
    assert!(
        snapshot_exists(&conn, snap_b),
        "the snapshot that still proves the three shared a window must survive \
         retention"
    );
}
