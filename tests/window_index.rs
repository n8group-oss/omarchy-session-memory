//! Captured window indices are part of the state, not decoration.
//!
//! `WindowPlan.idx` was loaded from the database and then ignored: restore
//! created each window with whatever index tmux picked next, so a session
//! holding windows 1 and 9 came back as 1 and 2, and a destination server
//! with a different `base-index` shifted every window. Scripts and muscle
//! memory that say `session:9` break silently.

mod common;

use osm::{capture, db, model, restore, tmux::Tmux};

struct Server(Tmux);

impl Server {
    fn start(label: &str) -> Self {
        Server(Tmux::with_socket(&format!(
            "osm-widx-{}-{}",
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

fn indices(t: &Tmux, session: &str) -> Vec<(u32, String)> {
    let sessions = t.list_sessions().unwrap();
    let sid = sessions
        .iter()
        .find(|s| s.name == session)
        .unwrap_or_else(|| panic!("no session {session}"))
        .id
        .clone();
    let mut out: Vec<(u32, String)> = t
        .list_windows()
        .unwrap()
        .into_iter()
        .filter(|w| w.session_id == sid)
        .map(|w| (w.idx, w.name))
        .collect();
    out.sort();
    out
}

/// A source session holding windows at 1 and 9 — a gap, so "place them in
/// order from wherever tmux starts" cannot accidentally produce the right
/// answer. Built without assuming the source server's own `base-index`,
/// which differs between a bare CI container (0) and a developer's
/// configured tmux (1).
fn source_with_a_gap(t: &Tmux) {
    t.run(&[
        "new-session",
        "-d",
        "-s",
        "alpha",
        "-n",
        "first",
        "-c",
        "/tmp",
    ])
    .unwrap();
    let wid = t
        .run(&["list-windows", "-t", "alpha", "-F", "#{window_id}"])
        .unwrap()
        .trim()
        .to_string();
    let at = t
        .run(&["list-windows", "-t", "alpha", "-F", "#{window_index}"])
        .unwrap()
        .trim()
        .to_string();
    if at != "1" {
        t.run(&["move-window", "-d", "-s", &wid, "-t", "alpha:1"])
            .unwrap();
    }
    t.run(&[
        "new-window",
        "-d",
        "-t",
        "alpha:9",
        "-n",
        "ninth",
        "-c",
        "/tmp",
    ])
    .unwrap();
}

/// The destination server is given `base-index 5`, so "wherever tmux starts"
/// is a different number from the source's. A keeper session holds the
/// server open: a tmux server with no sessions exits immediately, taking the
/// option with it.
fn destination_with_base_index_5(label: &str) -> Server {
    let dst = Server::start(label);
    dst.t()
        .run(&[
            "new-session",
            "-n",
            "code",
            "-d",
            "-s",
            "osm-keeper",
            "-c",
            "/tmp",
        ])
        .unwrap();
    dst.t()
        .run(&["set-option", "-g", "base-index", "5"])
        .unwrap();
    dst
}

#[test]
fn windows_are_restored_at_their_captured_indices_including_a_gap() {
    let src = Server::start("gap-src");
    source_with_a_gap(src.t());
    assert_eq!(
        indices(src.t(), "alpha"),
        vec![(1, "first".to_string()), (9, "ninth".to_string())],
        "source setup must actually produce windows at 1 and 9"
    );

    let tmp = tempfile::tempdir().unwrap();
    let mut conn = db::open(&tmp.path().join("state.db")).unwrap();
    let id = capture::snapshot(&mut conn, src.t(), "test").unwrap();
    let tree = model::load(&conn, id).unwrap();

    let dst = destination_with_base_index_5("gap-dst");
    let out = restore::restore_tree(dst.t(), &tree).unwrap();
    assert!(out.failed.is_empty(), "restore failed: {:?}", out.failed);

    assert_eq!(
        indices(dst.t(), "alpha"),
        vec![(1, "first".to_string()), (9, "ninth".to_string())],
        "captured window indices must survive restore onto a server whose \
         base-index (5) differs from the source's"
    );
}

/// The first window is the one `new-session` creates, and it is the one a
/// naive implementation leaves wherever the destination's `base-index` put
/// it. Asserted on its own so a fix that only handles `new-window` cannot
/// pass.
#[test]
fn the_first_window_is_placed_too_not_left_at_the_destination_base_index() {
    let src = Server::start("first-src");
    src.t()
        .run(&[
            "new-session",
            "-d",
            "-s",
            "alpha",
            "-n",
            "only",
            "-c",
            "/tmp",
        ])
        .unwrap();
    let wid = src
        .t()
        .run(&["list-windows", "-t", "alpha", "-F", "#{window_id}"])
        .unwrap()
        .trim()
        .to_string();
    src.t()
        .run(&["move-window", "-d", "-s", &wid, "-t", "alpha:3"])
        .unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let mut conn = db::open(&tmp.path().join("state.db")).unwrap();
    let id = capture::snapshot(&mut conn, src.t(), "test").unwrap();
    let tree = model::load(&conn, id).unwrap();

    let dst = destination_with_base_index_5("first-dst");
    restore::restore_tree(dst.t(), &tree).unwrap();

    assert_eq!(
        indices(dst.t(), "alpha"),
        vec![(3, "only".to_string())],
        "the session's only window must land at its captured index 3, not at \
         the destination server's base-index 5"
    );
}
