//! Regression tests for windows linked into more than one session.
//!
//! `tmux link-window` makes one window a member of several sessions. tmux
//! reports such a window once per session in `list-windows -a`, and its panes
//! once per link in `list-panes -a`. The original schema keyed windows by
//! session, so the same pane was inserted twice under one window row and the
//! capture died on
//! `UNIQUE constraint failed: pane_rows.window_row_id, pane_rows.tmux_pane_id`.
//! The transaction rolled back, so **every** capture failed for as long as the
//! link existed — silently, because the tmux hooks discard output.

mod common;

use osm::{capture, db, model, restore, tmux::Tmux};
use std::collections::HashSet;

struct Server(Tmux);

impl Drop for Server {
    fn drop(&mut self) {
        common::shutdown(&self.0);
    }
}

fn server(label: &str) -> Server {
    Server(Tmux::with_socket(&format!(
        "osm-link-{}-{}",
        label,
        std::process::id()
    )))
}

/// (session name, window id) for every window row tmux reports.
fn session_windows(t: &Tmux) -> Vec<(String, String)> {
    let sessions = t.list_sessions().unwrap();
    let mut out: Vec<(String, String)> = t
        .list_windows()
        .unwrap()
        .into_iter()
        .map(|w| {
            let name = sessions
                .iter()
                .find(|s| s.id == w.session_id)
                .expect("window belongs to a listed session")
                .name
                .clone();
            (name, w.id)
        })
        .collect();
    out.sort();
    out
}

fn window_id_named(t: &Tmux, name: &str) -> HashSet<String> {
    t.list_windows()
        .unwrap()
        .into_iter()
        .filter(|w| w.name == name)
        .map(|w| w.id)
        .collect()
}

#[test]
fn a_linked_window_is_captured_once_and_restored_as_one_shared_window() {
    let src = server("src");
    let t = &src.0;
    t.run(&["new-session", "-d", "-s", "alpha", "-c", "/tmp"])
        .unwrap();
    t.run(&["new-window", "-t", "alpha", "-n", "shared", "-c", "/tmp"])
        .unwrap();
    t.run(&["split-window", "-t", "alpha:shared", "-c", "/tmp"])
        .unwrap();
    t.run(&["new-session", "-d", "-s", "beta", "-c", "/tmp"])
        .unwrap();
    t.run(&["link-window", "-d", "-s", "alpha:shared", "-t", "beta:"])
        .unwrap();

    // Precondition: tmux really does report one window twice, and its panes
    // twice. Without this the test could pass against a tmux that does not.
    let src_windows = t.list_windows().unwrap();
    assert_eq!(
        src_windows.iter().filter(|w| w.name == "shared").count(),
        2,
        "tmux lists a linked window once per session"
    );
    assert_eq!(
        window_id_named(t, "shared").len(),
        1,
        "…but it is one window with one id"
    );

    let tmp = tempfile::tempdir().unwrap();
    let mut conn = db::open(&tmp.path().join("state.db")).unwrap();

    // Pre-fix this is where everything ended:
    //   UNIQUE constraint failed: pane_rows.window_row_id, pane_rows.tmux_pane_id
    let id = capture::snapshot(&mut conn, t, "test").expect("capture must survive a linked window");

    let windows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM window_rows WHERE snapshot_id = ?1",
            [id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        windows, 3,
        "one row per window: alpha's own, shared, beta's own"
    );

    let links: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM session_window_links l
             JOIN session_rows s ON s.row_id = l.session_row_id
             WHERE s.snapshot_id = ?1",
            [id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(links, 4, "one link row per (session, window) pair");

    let shared_panes: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pane_rows p
             JOIN window_rows w ON w.row_id = p.window_row_id
             WHERE w.snapshot_id = ?1 AND w.name = 'shared'",
            [id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        shared_panes, 2,
        "the shared window's panes are stored once, not once per link"
    );

    let tree = model::load(&conn, id).unwrap();

    let dst = server("dst");
    let out = restore::restore_tree(&dst.0, &tree).unwrap();
    assert!(out.failed.is_empty(), "restore failed: {:?}", out.failed);

    let restored = session_windows(&dst.0);
    assert_eq!(restored.len(), 4, "four (session, window) memberships");
    let distinct: HashSet<&String> = restored.iter().map(|(_, id)| id).collect();
    assert_eq!(
        distinct.len(),
        3,
        "three real windows: a linked window must not be rebuilt as two copies"
    );

    let shared_ids = window_id_named(&dst.0, "shared");
    assert_eq!(
        shared_ids.len(),
        1,
        "both sessions must point at the same restored window"
    );
    let shared_id = shared_ids.into_iter().next().unwrap();
    let owners: HashSet<&String> = restored
        .iter()
        .filter(|(_, id)| *id == shared_id)
        .map(|(session, _)| session)
        .collect();
    assert_eq!(
        owners.len(),
        2,
        "the restored window is linked into alpha and beta"
    );

    let panes: HashSet<String> = dst
        .0
        .list_panes()
        .unwrap()
        .into_iter()
        .filter(|p| p.window_id == shared_id)
        .map(|p| p.id)
        .collect();
    assert_eq!(panes.len(), 2, "the shared window keeps both of its panes");
}

/// The awkward case: a session whose *first* window is a link to a window an
/// earlier session already created. A tmux session cannot be created around
/// an existing window, so restore builds it with a throwaway window and
/// removes that once the link is in — and must not leave the stray window
/// behind.
#[test]
fn a_session_made_only_of_linked_windows_restores_without_a_stray_window() {
    let src = server("only-src");
    let t = &src.0;
    t.run(&["new-session", "-d", "-s", "alpha", "-c", "/tmp"])
        .unwrap();
    t.run(&["new-window", "-t", "alpha", "-n", "shared", "-c", "/tmp"])
        .unwrap();
    t.run(&["new-session", "-d", "-s", "beta", "-n", "own", "-c", "/tmp"])
        .unwrap();
    t.run(&["link-window", "-d", "-s", "alpha:shared", "-t", "beta:"])
        .unwrap();
    // beta is now nothing but the linked window.
    t.run(&["kill-window", "-t", "beta:own"]).unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let mut conn = db::open(&tmp.path().join("state.db")).unwrap();
    let id = capture::snapshot(&mut conn, t, "test").unwrap();
    let tree = model::load(&conn, id).unwrap();

    let dst = server("only-dst");
    let out = restore::restore_tree(&dst.0, &tree).unwrap();
    assert!(out.failed.is_empty(), "restore failed: {:?}", out.failed);

    let restored = session_windows(&dst.0);
    let beta_windows: Vec<&(String, String)> =
        restored.iter().filter(|(s, _)| s == "beta").collect();
    assert_eq!(
        beta_windows.len(),
        1,
        "beta holds exactly the linked window, with no placeholder left over: {restored:?}"
    );
    let shared_ids = window_id_named(&dst.0, "shared");
    assert_eq!(shared_ids.len(), 1);
    assert_eq!(
        beta_windows[0].1,
        *shared_ids.iter().next().unwrap(),
        "beta's only window is the very window alpha holds"
    );
}

/// A tree holding only the session called `name`, to stand in for a restore
/// that was interrupted after that session and before the rest.
fn only_session(tree: &model::SnapshotTree, name: &str) -> model::SnapshotTree {
    model::SnapshotTree {
        snapshot_id: tree.snapshot_id,
        sessions: tree
            .sessions
            .iter()
            .filter(|s| s.name == name)
            .cloned()
            .collect(),
    }
}

/// alpha and beta, sharing one window, captured from a private server.
fn linked_pair(label: &str) -> (Server, tempfile::TempDir, model::SnapshotTree) {
    let src = server(label);
    let t = &src.0;
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
    let id = capture::snapshot(&mut conn, t, "test").unwrap();
    let tree = model::load(&conn, id).unwrap();
    (src, tmp, tree)
}

/// The retry case. An interrupted restore left `alpha` behind; the next run
/// adopts it and must re-link the window `beta` shares with it instead of
/// building `beta` a second, independent copy.
///
/// Adoption used to return without recording the live window ids it had
/// matched, so the shared-window map stayed empty and `beta` was rebuilt from
/// scratch. Nothing was reported: `bad_count` stayed zero and the source was
/// retired as a success, with the link — and everything that later happens in
/// one of the two windows and not the other — gone for good.
#[test]
fn a_retry_relinks_the_window_the_adopted_session_already_holds() {
    let (src, _tmp, tree) = linked_pair("relink-src");
    drop(src);

    let dst = server("relink-dst");
    let first = restore::restore_tree(&dst.0, &only_session(&tree, "alpha")).unwrap();
    assert_eq!(first.created, vec!["alpha".to_string()], "{first:?}");

    let second = restore::restore_tree(&dst.0, &tree).unwrap();
    assert_eq!(second.adopted, vec!["alpha".to_string()], "{second:?}");
    assert_eq!(second.created, vec!["beta".to_string()], "{second:?}");
    assert!(
        second.conflicted.is_empty(),
        "a correctly re-linked restore is not a conflict: {second:?}"
    );

    let shared = window_id_named(&dst.0, "shared");
    assert_eq!(
        shared.len(),
        1,
        "the shared window must be one window, not one per session: {shared:?}"
    );
    let shared_id = shared.into_iter().next().unwrap();
    let owners: HashSet<String> = session_windows(&dst.0)
        .into_iter()
        .filter(|(_, id)| *id == shared_id)
        .map(|(session, _)| session)
        .collect();
    assert_eq!(
        owners,
        HashSet::from(["alpha".to_string(), "beta".to_string()]),
        "the restored window must be linked into both sessions"
    );
}

/// Two live sessions that each look like their captured selves but hold
/// *independent* windows where the snapshot has one shared window. Adopting
/// both retires the snapshot and makes the loss of the link permanent, so
/// this has to be reported.
#[test]
fn two_lookalike_sessions_holding_separate_windows_are_not_both_adopted() {
    let (src, _tmp, tree) = linked_pair("split-src");
    drop(src);

    // Each session restored on its own, so each builds its own copy of the
    // window the snapshot says they share.
    let dst = server("split-dst");
    restore::restore_tree(&dst.0, &only_session(&tree, "alpha")).unwrap();
    restore::restore_tree(&dst.0, &only_session(&tree, "beta")).unwrap();
    assert_eq!(
        window_id_named(&dst.0, "shared").len(),
        2,
        "precondition: the two sessions hold separate windows"
    );

    let out = restore::restore_tree(&dst.0, &tree).unwrap();
    assert!(
        !out.conflicted.is_empty(),
        "sessions holding separate copies of a shared window must not both be \
         adopted as a verified success: {out:?}"
    );
    assert!(
        out.adopted.len() < 2,
        "at most one of the two can be the captured shared window: {out:?}"
    );
    // Non-destructive, as always: nothing live was touched.
    assert_eq!(window_id_named(&dst.0, "shared").len(), 2);
}

/// A session that failed to restore must not also be reported as a link
/// conflict, and must never be able to abort the sessions that did restore.
/// The link check runs at the very end, over the whole tree, so a lookup
/// against a session tmux never created would otherwise take the entire
/// restore down with it.
#[test]
fn a_failed_session_does_not_break_the_shared_window_check() {
    let (src, _tmp, tree) = linked_pair("failcheck-src");
    drop(src);

    // beta's own window is given far more panes than its captured 80x24
    // layout can hold, so `split-window` fails once the window is full — the
    // same clean-on-every-tmux-version injection `tests/restore_retry.rs`
    // uses.
    let mut broken = tree.clone();
    let beta = broken
        .sessions
        .iter_mut()
        .find(|s| s.name == "beta")
        .unwrap();
    let window = beta.windows.iter_mut().find(|w| w.name == "own2").unwrap();
    for idx in 1..9 {
        window.panes.push(osm::model::PanePlan {
            tmux_pane_id: format!("%90{idx}"),
            idx,
            cwd: "/tmp".to_string(),
            restore_policy: "shell".to_string(),
        });
    }

    let dst = server("failcheck-dst");
    let out = restore::restore_tree(&dst.0, &broken).expect("a failed session must not abort");
    assert_eq!(out.created, vec!["alpha".to_string()], "{out:?}");
    assert_eq!(out.failed.len(), 1, "{out:?}");
    assert_eq!(out.failed[0].0, "beta");
    assert!(
        !out.conflicted.iter().any(|(s, _)| s == "beta"),
        "a failed session must not also be reported as a link conflict: {out:?}"
    );
}
