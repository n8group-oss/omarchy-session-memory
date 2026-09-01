//! A snapshot marked `complete` is the one thing `osm restore` will rebuild
//! from, so it must describe a tmux server state that actually existed.
//!
//! `collect` stitches one topology out of three separate tmux invocations
//! (`list-sessions`, `list-windows`, `list-panes`). A session created or
//! destroyed between them yields a graph no live server can produce, and the
//! writer used to `continue` past every such row and then mark the snapshot
//! `complete` anyway: a session removed after the first query became a row
//! with no windows, which restores as a fabricated empty session, and a
//! session created after it was omitted entirely.

mod common;

use osm::capture::{self, Topology};
use osm::db;
use osm::tmux::{PaneRec, SessionRec, Tmux, WindowRec};

struct Server(Tmux);

impl Server {
    fn start(label: &str) -> Self {
        let t = Tmux::with_socket(&format!("osm-cons-{}-{}", label, std::process::id()));
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
        Server(t)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        common::shutdown(&self.0);
    }
}

fn session(id: &str, name: &str) -> SessionRec {
    SessionRec {
        id: id.to_string(),
        name: name.to_string(),
    }
}

fn window(session_id: &str, id: &str, idx: u32, name: &str) -> WindowRec {
    WindowRec {
        session_id: session_id.to_string(),
        id: id.to_string(),
        idx,
        name: name.to_string(),
        layout: "abcd,80x24,0,0,0".to_string(),
        active: true,
        zoomed: false,
        auto_named: false,
    }
}

fn pane(window_id: &str, id: &str) -> PaneRec {
    PaneRec {
        window_id: window_id.to_string(),
        id: id.to_string(),
        idx: 0,
        active: true,
        dead: false,
        pid: 1,
        cwd: "/tmp".to_string(),
        title: "t".to_string(),
        cmd: "bash".to_string(),
    }
}

/// A consistent baseline, so every test below differs from a *writable*
/// topology in exactly the one respect it is about.
fn consistent() -> Topology {
    Topology {
        placements: None,
        sessions: vec![session("$0", "alpha")],
        windows: vec![window("$0", "@0", 0, "main")],
        panes: vec![pane("@0", "%0")],
        // Hand-built, but it still has to claim an incarnation: an unprefixed
        // `$0`/`@0`/`%0` names something only on the server that issued it, so
        // a topology attributed to nothing is refused outright (see
        // `a_topology_attributed_to_no_server_is_refused`). The value is what
        // `Tmux::server_incarnation` produces — boot, minted id, server pid,
        // start ticks, socket — and this fixture is the only thing that ever
        // makes one up.
        server: Some(FAKE_SERVER.to_string()),
        server_at_end: Some(FAKE_SERVER.to_string()),
    }
}

/// A stand-in for one server incarnation, in the shape
/// `Tmux::server_incarnation` returns.
const FAKE_SERVER: &str =
    "boot-a:0123456789abcdef0123456789abcdef:4242:9999999:/tmp/tmux-1000/osm-test";

fn write(topo: &Topology) -> (tempfile::TempDir, rusqlite::Connection, anyhow::Result<i64>) {
    let tmp = tempfile::tempdir().unwrap();
    let mut conn = db::open(&tmp.path().join("state.db")).unwrap();
    let result = capture::write_topology(&mut conn, topo, "test", None);
    (tmp, conn, result)
}

fn complete_snapshots(conn: &rusqlite::Connection) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM snapshots WHERE state='complete'",
        [],
        |r| r.get(0),
    )
    .unwrap()
}

#[test]
fn the_baseline_topology_is_accepted() {
    let topo = consistent();
    assert_eq!(topo.inconsistency(), None);
    let (_tmp, conn, result) = write(&topo);
    result.expect("a consistent topology must be recorded");
    assert_eq!(complete_snapshots(&conn), 1);
}

#[test]
fn a_session_that_vanished_mid_capture_is_not_written_as_an_empty_session() {
    // `list-sessions` saw two sessions; `list-windows` ran after "beta" was
    // closed, so beta has no windows. Written as-is, beta becomes a session
    // row with zero windows and restores as a fabricated empty session.
    let mut topo = consistent();
    topo.sessions.push(session("$1", "beta"));

    assert!(
        topo.inconsistency().is_some(),
        "a session with no windows cannot come from a live server"
    );
    let (_tmp, conn, result) = write(&topo);
    let err = result.expect_err("a session with no windows must not be recorded");
    assert!(
        format!("{err:#}").contains("beta"),
        "the error must name the offending session: {err:#}"
    );
    assert_eq!(
        complete_snapshots(&conn),
        0,
        "an inconsistent graph must never be published as `complete`"
    );
}

#[test]
fn a_session_created_mid_capture_does_not_leave_an_orphan_window() {
    // `list-windows` saw a window of a session `list-sessions` ran too early
    // to see. The window used to be dropped silently.
    let mut topo = consistent();
    topo.windows.push(window("$9", "@9", 0, "late"));
    topo.panes.push(pane("@9", "%9"));

    assert!(topo.inconsistency().is_some());
    let (_tmp, conn, result) = write(&topo);
    result.expect_err("a window with no session must not be recorded");
    assert_eq!(complete_snapshots(&conn), 0);
}

#[test]
fn a_window_whose_panes_were_never_seen_is_not_written() {
    let mut topo = consistent();
    topo.windows.push(window("$0", "@1", 1, "logs"));

    assert!(topo.inconsistency().is_some());
    let (_tmp, conn, result) = write(&topo);
    result.expect_err("a window with no panes must not be recorded");
    assert_eq!(complete_snapshots(&conn), 0);
}

#[test]
fn a_pane_whose_window_was_never_seen_is_not_written() {
    let mut topo = consistent();
    topo.panes.push(pane("@7", "%7"));

    assert!(topo.inconsistency().is_some());
    let (_tmp, conn, result) = write(&topo);
    result.expect_err("a pane with no window must not be recorded");
    assert_eq!(complete_snapshots(&conn), 0);
}

/// The consistency rules must not reject the ordinary case: a real server,
/// including a linked window (which tmux reports once per session it is
/// linked into).
#[test]
fn a_real_server_including_a_linked_window_reads_as_consistent() {
    let s = Server::start("live");
    let t = &s.0;
    t.run(&["new-window", "-t", "alpha", "-n", "logs", "-c", "/tmp"])
        .unwrap();
    t.run(&["split-window", "-t", "alpha:logs", "-c", "/tmp"])
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
    t.run(&["link-window", "-d", "-s", "alpha:logs", "-t", "beta:"])
        .unwrap();

    let topo = capture::collect(t).expect("a live server must read as consistent");
    assert_eq!(topo.inconsistency(), None);

    let tmp = tempfile::tempdir().unwrap();
    let mut conn = db::open(&tmp.path().join("state.db")).unwrap();
    capture::write_topology(&mut conn, &topo, "test", None).unwrap();
    assert_eq!(complete_snapshots(&conn), 1);
}

/// A topology that names no server incarnation must not be recorded.
///
/// Every row a snapshot holds is written down under an unprefixed tmux id, and
/// an unprefixed id names something only on the server that issued it: tmux
/// hands `$0`, `@0` and `%0` out again to every server it starts. With nothing
/// attributing the rows to one incarnation, those ids are satisfied by
/// whichever server is asked next — which is how a carried session could be
/// linked into an unrelated window that merely reuses `@0`.
///
/// This shape used to be produced by an ordinary capture whenever the server
/// declined to identify itself, because the error was flattened to `None` and
/// `None` meant "no server".
#[test]
fn a_topology_attributed_to_no_server_is_refused() {
    let mut topo = consistent();
    topo.server = None;
    topo.server_at_end = None;

    let (_tmp, conn, result) = write(&topo);
    let err = result.expect_err("rows that belong to no server incarnation must not be recorded");
    assert!(
        format!("{err:#}").contains("no identified tmux server"),
        "the refusal must say what is missing: {err:#}"
    );
    assert_eq!(complete_snapshots(&conn), 0);
}

/// The empty case is the one legitimate exception: there is no server to name
/// precisely because there is nothing on it.
#[test]
fn an_empty_topology_needs_no_server_to_be_attributed_to() {
    let topo = Topology {
        placements: None,
        sessions: vec![],
        windows: vec![],
        panes: vec![],
        server: None,
        server_at_end: None,
    };
    assert_eq!(topo.inconsistency(), None);
    let (_tmp, conn, result) = write(&topo);
    result.expect("nothing to record is not a failure to record it");
    assert_eq!(complete_snapshots(&conn), 1);
}

/// The three `list-*` calls a topology is stitched from can straddle a server
/// restart, and the halves then describe different servers under the same ids.
///
/// The identity is therefore read at both ends and both are kept: a
/// disagreement is an inconsistency like any other, so `collect` re-reads
/// rather than committing a graph that passes every structural check and
/// describes nothing that ever existed.
#[test]
fn a_server_replaced_between_the_list_commands_is_an_inconsistency() {
    let mut topo = consistent();
    topo.server_at_end = Some(
        "boot-a:fedcba9876543210fedcba9876543210:4243:9999999:/tmp/tmux-1000/osm-test".to_string(),
    );

    let why = topo
        .inconsistency()
        .expect("two servers cannot describe one state");
    assert!(
        why.contains("changed identity"),
        "the reason must say which property failed: {why}"
    );
    let (_tmp, conn, result) = write(&topo);
    result.expect_err("a graph stitched from two servers must not be recorded");
    assert_eq!(complete_snapshots(&conn), 0);
}
