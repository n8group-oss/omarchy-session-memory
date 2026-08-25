mod common;

use osm::{capture, db, tmux::Tmux};

struct Server(Tmux);

impl Server {
    fn start(label: &str) -> Self {
        let sock = format!("osm-cap-{}-{}", label, std::process::id());
        let t = Tmux::with_socket(&sock);
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

#[test]
fn snapshot_persists_full_topology_as_complete() {
    let s = Server::start("full");
    let t = &s.0;
    t.run(&["new-window", "-t", "alpha", "-n", "logs", "-c", "/tmp"])
        .unwrap();
    t.run(&["split-window", "-t", "alpha:logs", "-c", "/tmp"])
        .unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let mut conn = db::open(&tmp.path().join("state.db")).unwrap();

    let id = capture::snapshot(&mut conn, t, "test").unwrap();

    let state: String = conn
        .query_row("SELECT state FROM snapshots WHERE id=?1", [id], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(state, "complete");

    let sessions: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM session_rows WHERE snapshot_id=?1",
            [id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(sessions, 1);

    let windows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM window_rows WHERE snapshot_id = ?1",
            [id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(windows, 2);

    let panes: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pane_rows p
             JOIN window_rows w ON w.row_id = p.window_row_id
             WHERE w.snapshot_id = ?1",
            [id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(panes, 3);
}

#[test]
fn snapshot_records_active_window_and_default_shell_policy() {
    let s = Server::start("active");
    let tmp = tempfile::tempdir().unwrap();
    let mut conn = db::open(&tmp.path().join("state.db")).unwrap();
    let id = capture::snapshot(&mut conn, &s.0, "test").unwrap();

    let active: Option<String> = conn
        .query_row(
            "SELECT active_window_id FROM session_rows WHERE snapshot_id=?1",
            [id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(active.is_some(), "active window must be recorded");

    let policy: String = conn
        .query_row("SELECT restore_policy FROM pane_rows LIMIT 1", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(policy, "shell");
}

#[test]
fn two_snapshots_do_not_collide_on_native_ids() {
    let s = Server::start("twice");
    let tmp = tempfile::tempdir().unwrap();
    let mut conn = db::open(&tmp.path().join("state.db")).unwrap();
    let a = capture::snapshot(&mut conn, &s.0, "first").unwrap();
    let b = capture::snapshot(&mut conn, &s.0, "second").unwrap();
    assert_ne!(a, b);
    let rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM session_rows", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 2, "same tmux session appears once per snapshot");
}

#[test]
fn boot_id_is_stable_and_non_empty() {
    let a = osm::boot::current_boot_id().unwrap();
    let b = osm::boot::current_boot_id().unwrap();
    assert!(!a.is_empty());
    assert_eq!(a, b);
}
