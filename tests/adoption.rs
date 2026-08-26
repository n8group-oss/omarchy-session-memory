//! Adoption must prove the captured state is actually back.
//!
//! `restore_tree` used to adopt any live session whose *name* matched and
//! count it as a success. If something else at startup created `dev` with one
//! empty window while the snapshot held six windows, restore reported
//! `adopted: ["dev"]`, retired the snapshot, and the other five windows were
//! gone for good — with no signal that anything had happened.
//!
//! Adoption still never destroys live work. It just no longer claims a
//! restore that did not happen.

mod common;

use osm::{capture, db, model, restore, tmux::Tmux};

struct Server(Tmux);

impl Server {
    fn start(label: &str) -> Self {
        Server(Tmux::with_socket(&format!(
            "osm-adopt-{}-{}",
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

/// A three-window `dev` session, captured into a fresh database.
fn capture_dev(src: &Server) -> (tempfile::TempDir, rusqlite::Connection, model::SnapshotTree) {
    let t = src.t();
    t.run(&[
        "new-session",
        "-d",
        "-s",
        "dev",
        "-n",
        "code",
        "-c",
        "/tmp",
        "-x",
        "200",
        "-y",
        "50",
    ])
    .unwrap();
    t.run(&["split-window", "-t", "dev:code", "-c", "/tmp"])
        .unwrap();
    t.run(&["new-window", "-d", "-t", "dev", "-n", "logs", "-c", "/tmp"])
        .unwrap();
    t.run(&["new-window", "-d", "-t", "dev", "-n", "notes", "-c", "/tmp"])
        .unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let mut conn = db::open(&tmp.path().join("state.db")).unwrap();
    let id = capture::snapshot(&mut conn, t, "test").unwrap();
    let tree = model::load(&conn, id).unwrap();
    (tmp, conn, tree)
}

fn window_names(t: &Tmux, session: &str) -> Vec<String> {
    let sid = t
        .list_sessions()
        .unwrap()
        .into_iter()
        .find(|s| s.name == session)
        .unwrap()
        .id;
    let mut names: Vec<String> = t
        .list_windows()
        .unwrap()
        .into_iter()
        .filter(|w| w.session_id == sid)
        .map(|w| w.name)
        .collect();
    names.sort();
    names
}

#[test]
fn a_name_match_with_a_different_window_count_is_a_conflict_not_an_adoption() {
    let src = Server::start("count-src");
    let (_tmp, _conn, tree) = capture_dev(&src);

    // Something else got there first: one window called "shell", not three.
    let dst = Server::start("count-dst");
    dst.t()
        .run(&[
            "new-session",
            "-d",
            "-s",
            "dev",
            "-n",
            "shell",
            "-c",
            "/tmp",
        ])
        .unwrap();

    let out = restore::restore_tree(dst.t(), &tree).unwrap();

    assert!(
        out.adopted.is_empty(),
        "a session that does not match the snapshot must not be adopted: {:?}",
        out.adopted
    );
    assert_eq!(
        out.conflicted.len(),
        1,
        "the mismatch must be reported as a conflict: {out:?}"
    );
    assert_eq!(out.conflicted[0].0, "dev");
    assert!(
        out.conflicted[0].1.contains("window"),
        "the conflict must say what differs, got {:?}",
        out.conflicted[0].1
    );

    assert_eq!(
        window_names(dst.t(), "dev"),
        vec!["shell".to_string()],
        "a conflict must never clobber the live session"
    );
}

#[test]
fn a_name_match_with_a_different_pane_count_is_a_conflict() {
    let src = Server::start("panes-src");
    let (_tmp, _conn, tree) = capture_dev(&src);

    // Right window names and indices, wrong contents: dev:code has one pane
    // live and two in the snapshot.
    let dst = Server::start("panes-dst");
    let t = dst.t();
    t.run(&[
        "new-session",
        "-d",
        "-s",
        "dev",
        "-n",
        "code",
        "-c",
        "/tmp",
        "-x",
        "200",
        "-y",
        "50",
    ])
    .unwrap();
    t.run(&["new-window", "-d", "-t", "dev", "-n", "logs", "-c", "/tmp"])
        .unwrap();
    t.run(&["new-window", "-d", "-t", "dev", "-n", "notes", "-c", "/tmp"])
        .unwrap();

    let out = restore::restore_tree(t, &tree).unwrap();
    assert!(out.adopted.is_empty(), "adopted: {:?}", out.adopted);
    assert_eq!(out.conflicted.len(), 1, "{out:?}");
    assert!(
        out.conflicted[0].1.contains("pane"),
        "the conflict must name the pane-count difference, got {:?}",
        out.conflicted[0].1
    );
}

#[test]
fn a_session_restore_actually_produced_is_adopted_on_a_rerun() {
    let src = Server::start("rerun-src");
    let (_tmp, _conn, tree) = capture_dev(&src);

    let dst = Server::start("rerun-dst");
    let first = restore::restore_tree(dst.t(), &tree).unwrap();
    assert_eq!(first.created, vec!["dev".to_string()]);
    assert!(first.conflicted.is_empty(), "{first:?}");
    let after_first = window_names(dst.t(), "dev");

    let second = restore::restore_tree(dst.t(), &tree).unwrap();
    assert_eq!(
        second.adopted,
        vec!["dev".to_string()],
        "a session this very restore built must verify as a match: {second:?}"
    );
    assert!(second.created.is_empty());
    assert!(second.conflicted.is_empty(), "{second:?}");
    assert_eq!(window_names(dst.t(), "dev"), after_first);
}

#[test]
fn a_conflicting_session_does_not_stop_the_other_sessions_from_restoring() {
    let src = Server::start("mix-src");
    let (_tmp, _conn, tree) = {
        let t = src.t();
        t.run(&[
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
        t.run(&[
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
        t.run(&[
            "new-window",
            "-d",
            "-t",
            "beta",
            "-n",
            "extra",
            "-c",
            "/tmp",
        ])
        .unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let mut conn = db::open(&tmp.path().join("state.db")).unwrap();
        let id = capture::snapshot(&mut conn, t, "test").unwrap();
        let tree = model::load(&conn, id).unwrap();
        (tmp, conn, tree)
    };

    let dst = Server::start("mix-dst");
    dst.t()
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

    let out = restore::restore_tree(dst.t(), &tree).unwrap();
    assert_eq!(out.created, vec!["alpha".to_string()], "{out:?}");
    assert_eq!(out.conflicted.len(), 1, "{out:?}");
    assert_eq!(out.conflicted[0].0, "beta");
}

/// The case that makes structure-only adoption actively harmful.
///
/// A restore whose captured directory was unavailable creates the panes in
/// `$HOME` and reports itself degraded. The *shape* of that session — window
/// count, window names, indices, pane layout — is a perfect match for the
/// snapshot, so a structure-only comparison adopts it on the next run and
/// declares success. That is precisely the run that could have repaired it:
/// the volume is mounted now. Instead the snapshot is retired and the real
/// directories are gone from the record for good.
#[test]
fn a_session_restored_into_the_wrong_directories_is_not_a_match() {
    // A volume that exists at capture time and is gone at restore time — an
    // unmounted disk, an encrypted home that is not open yet, a network mount
    // that is slow at graphical-session start.
    //
    // The two panes get *different* directories on purpose. Giving them the
    // same one made this test pass against a comparison that sorted each
    // side's directories and compared the two sorted lists, which cannot tell
    // a window from itself with two panes swapped — see
    // `a_session_with_its_pane_directories_swapped_is_not_a_match`.
    let volume = tempfile::tempdir().unwrap();
    let first_dir = volume.path().join("code").to_str().unwrap().to_string();
    let second_dir = volume.path().join("docs").to_str().unwrap().to_string();
    std::fs::create_dir(&first_dir).unwrap();
    std::fs::create_dir(&second_dir).unwrap();
    let captured_dir = first_dir.clone();

    let src = Server::start("cwd-src");
    let t = src.t();
    t.run(&[
        "new-session",
        "-d",
        "-s",
        "dev",
        "-n",
        "code",
        "-c",
        &first_dir,
        "-x",
        "200",
        "-y",
        "50",
    ])
    .unwrap();
    t.run(&["split-window", "-t", "dev:code", "-c", &second_dir])
        .unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let mut conn = db::open(&tmp.path().join("state.db")).unwrap();
    let id = capture::snapshot(&mut conn, t, "test").unwrap();
    let tree = model::load(&conn, id).unwrap();
    drop(src);

    // The volume goes away before the restore.
    volume.close().unwrap();

    let dst = Server::start("cwd-dst");
    let first = restore::restore_tree(dst.t(), &tree).unwrap();
    assert_eq!(first.created, vec!["dev".to_string()], "{first:?}");
    assert_eq!(
        first.degraded.len(),
        2,
        "both panes must be reported as started in the wrong place: {first:?}"
    );

    // The retry. The session is structurally identical to the snapshot and
    // still has none of its directories.
    let second = restore::restore_tree(dst.t(), &tree).unwrap();
    assert!(
        second.adopted.is_empty(),
        "a session whose panes are in the wrong directories is not the \
         captured session and must not be adopted as one: {second:?}"
    );
    assert_eq!(
        second.conflicted.len(),
        1,
        "the retry must report the session as still needing repair: {second:?}"
    );
    assert_eq!(second.conflicted[0].0, "dev");
    assert!(
        second.conflicted[0].1.contains("director"),
        "the report must name the directory difference, got {:?}",
        second.conflicted[0].1
    );
    assert!(
        second.conflicted[0].1.contains(&captured_dir),
        "the report must name the directory that is still missing, got {:?}",
        second.conflicted[0].1
    );

    // Non-destructive, as adoption and conflict always are.
    assert_eq!(window_names(dst.t(), "dev"), vec!["code".to_string()]);
}

/// The focus is part of the captured state, and each of the three tests below
/// changes exactly one field of a session this very restore produced — the
/// session that `a_session_restore_actually_produced_is_adopted_on_a_rerun`
/// proves is otherwise a perfect match. Without the field in the comparison,
/// the altered session is adopted and the snapshot permanently retired.
fn restore_then(label: &str, alter: impl Fn(&Tmux)) -> (Server, restore::RestoreOutcome) {
    let src = Server::start(&format!("focus-src-{label}"));
    let (_tmp, _conn, tree) = capture_dev(&src);
    drop(src);

    let dst = Server::start(&format!("focus-dst-{label}"));
    let first = restore::restore_tree(dst.t(), &tree).unwrap();
    assert_eq!(first.created, vec!["dev".to_string()], "{first:?}");
    assert!(first.conflicted.is_empty(), "{first:?}");

    alter(dst.t());

    let second = restore::restore_tree(dst.t(), &tree).unwrap();
    (dst, second)
}

#[test]
fn a_session_focused_on_a_different_window_is_not_a_match() {
    let (_dst, out) = restore_then("window", |t| {
        t.run(&["select-window", "-t", "dev:logs"]).unwrap();
    });
    assert!(
        out.adopted.is_empty(),
        "a session sitting on the wrong window must not be adopted: {out:?}"
    );
    assert_eq!(out.conflicted.len(), 1, "{out:?}");
    assert!(
        out.conflicted[0].1.contains("current window"),
        "the conflict must name the focus difference, got {:?}",
        out.conflicted[0].1
    );
}

#[test]
fn a_session_with_a_different_active_pane_is_not_a_match() {
    let (_dst, out) = restore_then("pane", |t| {
        // dev:code has two panes; the restore left the captured one focused.
        t.run(&["select-pane", "-t", "dev:code", "-U"]).unwrap();
        t.run(&["select-pane", "-t", "dev:code", "-L"]).unwrap();
    });
    assert!(
        out.adopted.is_empty(),
        "a session with the cursor in the wrong pane must not be adopted: {out:?}"
    );
    assert_eq!(out.conflicted.len(), 1, "{out:?}");
    assert!(
        out.conflicted[0].1.contains("active"),
        "the conflict must name the active-pane difference, got {:?}",
        out.conflicted[0].1
    );
}

#[test]
fn a_window_that_is_no_longer_zoomed_is_not_a_match() {
    // Captured zoomed, restored zoomed, then unzoomed by hand.
    let src = Server::start("zoom-src");
    let (_tmp, _conn, tree) = capture_dev(&src);
    drop(src);

    let dst = Server::start("zoom-dst");
    restore::restore_tree(dst.t(), &tree).unwrap();
    dst.t()
        .run(&["resize-pane", "-t", "dev:code", "-Z"])
        .unwrap();

    let out = restore::restore_tree(dst.t(), &tree).unwrap();
    assert!(
        out.adopted.is_empty(),
        "a zoomed window is not the captured unzoomed one: {out:?}"
    );
    assert_eq!(out.conflicted.len(), 1, "{out:?}");
    assert!(
        out.conflicted[0].1.contains("zoom"),
        "the conflict must name the zoom difference, got {:?}",
        out.conflicted[0].1
    );
}

/// The multiset bug, on its own.
///
/// The comparison used to sort each side's pane directories and compare the
/// two sorted lists. A two-pane window with pane 1 in `code` and pane 2 in
/// `docs` therefore *matched* a live session holding those two directories the
/// other way round: restore reported `succeeded`, adopted a session that is
/// not the captured one, and retired the snapshot — which was the only record
/// of which pane had been where.
///
/// The session here is built by the restore itself and then altered with
/// `respawn-pane -c`, so nothing else about it can differ: same window, same
/// name, same index, same layout, same geometry, same active pane. The only
/// difference is which cell holds which directory.
#[test]
fn a_session_with_its_pane_directories_swapped_is_not_a_match() {
    let root = tempfile::tempdir().unwrap();
    let code = root.path().join("code").to_str().unwrap().to_string();
    let docs = root.path().join("docs").to_str().unwrap().to_string();
    std::fs::create_dir(&code).unwrap();
    std::fs::create_dir(&docs).unwrap();

    let src = Server::start("swap-src");
    let (_tmp, _conn, tree) = {
        let t = src.t();
        t.run(&[
            "new-session",
            "-d",
            "-s",
            "dev",
            "-n",
            "code",
            "-c",
            &code,
            "-x",
            "200",
            "-y",
            "50",
        ])
        .unwrap();
        t.run(&["split-window", "-t", "dev:code", "-c", &docs])
            .unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let mut conn = db::open(&tmp.path().join("state.db")).unwrap();
        let id = capture::snapshot(&mut conn, t, "test").unwrap();
        let tree = model::load(&conn, id).unwrap();
        (tmp, conn, tree)
    };
    drop(src);

    let dst = Server::start("swap-dst");
    let first = restore::restore_tree(dst.t(), &tree).unwrap();
    assert_eq!(first.created, vec!["dev".to_string()], "{first:?}");
    assert!(first.degraded.is_empty(), "{first:?}");

    // Both panes are respawned in the *other* pane's directory. `respawn-pane`
    // keeps the pane's id, its cell and the window's layout, so this changes
    // exactly one thing about the session.
    let panes: Vec<String> = dst
        .t()
        .run(&["list-panes", "-t", "dev:code", "-F", "#{pane_id}"])
        .unwrap()
        .lines()
        .map(|l| l.trim().to_string())
        .collect();
    assert_eq!(panes.len(), 2, "the restore must have built two panes");
    dst.t()
        .run(&["respawn-pane", "-k", "-c", &docs, "-t", &panes[0]])
        .unwrap();
    dst.t()
        .run(&["respawn-pane", "-k", "-c", &code, "-t", &panes[1]])
        .unwrap();

    let second = restore::restore_tree(dst.t(), &tree).unwrap();
    assert!(
        second.adopted.is_empty(),
        "a session holding the captured directories in the wrong panes is not \
         the captured session: {second:?}"
    );
    assert_eq!(second.conflicted.len(), 1, "{second:?}");
    assert_eq!(second.conflicted[0].0, "dev");
    assert!(
        second.conflicted[0].1.contains("director"),
        "the conflict must name the directory difference, got {:?}",
        second.conflicted[0].1
    );

    // Non-destructive, as adoption and conflict always are.
    assert_eq!(window_names(dst.t(), "dev"), vec!["code".to_string()]);
}

/// A snapshot whose layout string names one pane twice.
///
/// It is still a well-formed tmux layout — `parse_with_pane_ids` accepts it and
/// its leaf count matches the window's pane count — so nothing rejects it
/// before the comparison. Building the captured session's shape then unwrapped
/// a pane slot that the duplicate had already emptied and **panicked**, which
/// aborts the whole restore: every session still ahead of this one in the tree
/// is left unbuilt, and the same crash sits in the capture path that discharges
/// carry debt.
#[test]
fn a_captured_layout_naming_one_pane_twice_is_a_conflict_not_a_crash() {
    let src = Server::start("duplayout-src");
    let (_tmp, conn, tree) = capture_dev(&src);
    drop(src);

    // `dev:code` has two panes; both cells of this layout name pane 0.
    let corrupt = "abcd,200x50,0,0{100x50,0,0,0,99x50,101,0,0}";
    assert!(
        osm::layout::parse_with_pane_ids(corrupt).is_ok(),
        "the corruption must be a layout tmux itself would accept, or the \
         restore rejects it long before the comparison"
    );
    let changed = conn
        .execute(
            "UPDATE window_rows SET layout = ?1 WHERE name = 'code'",
            [corrupt],
        )
        .unwrap();
    assert_eq!(changed, 1, "exactly one window's layout is corrupted");
    let tree = model::load(&conn, tree.snapshot_id).unwrap();

    // A live `dev`, so the restore takes the adoption path.
    let dst = Server::start("duplayout-dst");
    dst.t()
        .run(&[
            "new-session",
            "-d",
            "-s",
            "dev",
            "-n",
            "shell",
            "-c",
            "/tmp",
        ])
        .unwrap();

    let out = restore::restore_tree(dst.t(), &tree).unwrap();

    assert!(out.adopted.is_empty(), "{out:?}");
    assert_eq!(
        out.conflicted.len(),
        1,
        "the live session must be reported as a conflict: {out:?}"
    );
    assert_eq!(out.conflicted[0].0, "dev");
    assert_eq!(
        window_names(dst.t(), "dev"),
        vec!["shell".to_string()],
        "a conflict must never clobber the live session"
    );
}

/// A live window the **user** named is not a window nobody named, however
/// perfectly the rest of it lines up.
///
/// The snapshot's window here is auto-named — tmux chose `bash` for it — and
/// the live `dev` has the same one window at the same index, the same single
/// pane, the same directory and the same geometry. The only difference is that
/// the user has called their window `notes`. Adopting across that difference
/// takes over a session the user is working in *and* retires the snapshot that
/// still holds the real `dev`, which is the one trade this comparison exists
/// not to make.
#[test]
fn a_live_window_the_user_named_is_not_an_auto_named_captured_one() {
    let src = Server::start("username-src");
    src.t()
        .run(&[
            "new-session",
            "-d",
            "-s",
            "dev",
            "-c",
            "/tmp",
            "-x",
            "200",
            "-y",
            "50",
        ])
        .unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let mut conn = db::open(&tmp.path().join("state.db")).unwrap();
    let id = capture::snapshot(&mut conn, src.t(), "test").unwrap();
    let tree = model::load(&conn, id).unwrap();
    drop(src);

    // Same shape, same place, same directory — a name the user chose.
    let dst = Server::start("username-dst");
    dst.t()
        .run(&[
            "new-session",
            "-d",
            "-s",
            "dev",
            "-n",
            "notes",
            "-c",
            "/tmp",
            "-x",
            "200",
            "-y",
            "50",
        ])
        .unwrap();

    let out = restore::restore_tree(dst.t(), &tree).unwrap();

    assert!(
        out.adopted.is_empty(),
        "a session whose window the user named must not be adopted as an \
         auto-named captured one: {:?}",
        out.adopted
    );
    assert_eq!(out.conflicted.len(), 1, "{out:?}");
    assert_eq!(out.conflicted[0].0, "dev");
    assert!(
        out.conflicted[0].1.contains("named"),
        "the conflict must say the names differ, got {:?}",
        out.conflicted[0].1
    );
    assert_eq!(
        window_names(dst.t(), "dev"),
        vec!["notes".to_string()],
        "a conflict must never clobber the live session"
    );
}
