//! Placement survives a snapshot, and a distrusted compositor writes nothing.

use osm::desktop::{self, Placement, Placements};
use osm::{capture, db, tmux::Tmux};

mod common;

fn place(session: &str, ws: &str, connector: &str) -> Placement {
    Placement {
        session: session.into(),
        address: format!("0x{session}"),
        class: "com.mitchellh.ghostty".into(),
        terminal_kind: "ghostty".into(),
        workspace_kind: "numbered".into(),
        workspace_ref: ws.into(),
        monitor_connector: connector.into(),
        monitor_desc: Some("Dell Inc. AW3423DWF 8CM42S3".into()),
        monitor_scale: Some(1.0),
        monitor_transform: Some(0),
        floating: false,
        rel: Some((0.1, 0.2, 0.5, 0.6)),
    }
}

fn server(label: &str) -> Tmux {
    let t = Tmux::with_socket(&format!("osm-per-{label}-{}", std::process::id()));
    t.run(&["new-session", "-d", "-s", "alpha", "-n", "w", "-c", "/tmp"])
        .unwrap();
    t
}

#[test]
fn placement_is_stored_with_the_snapshot_and_read_back_intact() {
    let t = server("roundtrip");
    let tmp = tempfile::tempdir().unwrap();
    let mut conn = db::open(&tmp.path().join("state.db")).unwrap();

    let mut topo = capture::collect(&t).unwrap();
    topo.placements = Placements::Known(vec![place("alpha", "7", "HDMI-A-2")]);
    let snap = capture::write_topology(&mut conn, &topo, "test", None).unwrap();

    let back = desktop::placements_of(&conn, snap).unwrap();
    assert_eq!(back.len(), 1, "{back:?}");
    assert_eq!(
        back[0].session, "alpha",
        "the placement links to its session"
    );
    assert_eq!(back[0].workspace_ref, "7");
    assert_eq!(back[0].monitor_connector, "HDMI-A-2");
    assert_eq!(
        back[0].monitor_desc.as_deref(),
        Some("Dell Inc. AW3423DWF 8CM42S3")
    );
    let rel = back[0].rel.expect("relative geometry survives");
    assert!(
        (rel.0 - 0.1).abs() < 1e-5 && (rel.3 - 0.6).abs() < 1e-5,
        "{rel:?}"
    );

    common::shutdown(&t);
}

#[test]
fn a_distrusted_compositor_writes_no_placement_rather_than_an_empty_one() {
    // The rule the maintainer's lost layout paid for: no placement is honest,
    // empty placement over a good layout is not.
    let t = server("none");
    let tmp = tempfile::tempdir().unwrap();
    let mut conn = db::open(&tmp.path().join("state.db")).unwrap();

    let mut topo = capture::collect(&t).unwrap();
    topo.placements = Placements::Unknown("the compositor stopped answering".into());
    let snap = capture::write_topology(&mut conn, &topo, "test", None).unwrap();

    assert!(desktop::placements_of(&conn, snap).unwrap().is_empty());
    let rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM terminal_windows", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 0, "unknown must write nothing at all");

    common::shutdown(&t);
}

/// The three placement answers are three different rows on disk.
///
/// Everything downstream — retention deciding what it may delete, restore
/// deciding whether it has a layout, the panel deciding what to tell the user
/// — reads this one column. If `Unknown` and `Known(vec![])` land on disk as
/// the same thing, all three of them are back to guessing from "there are no
/// `terminal_windows` rows", which is the ambiguity this whole change exists
/// to remove.
#[test]
fn the_three_placement_answers_are_stored_apart() {
    let t = server("tristate");
    let tmp = tempfile::tempdir().unwrap();
    let mut conn = db::open(&tmp.path().join("state.db")).unwrap();

    let mut topo = capture::collect(&t).unwrap();

    topo.placements = Placements::Known(vec![place("alpha", "7", "DP-1")]);
    let known = capture::write_topology(&mut conn, &topo, "test", None).unwrap();

    topo.placements = Placements::Known(vec![]);
    let empty = capture::write_topology(&mut conn, &topo, "test", None).unwrap();

    topo.placements = Placements::Unknown("hyprctl -j clients timed out".into());
    let unknown = capture::write_topology(&mut conn, &topo, "test", None).unwrap();

    topo.placements = Placements::Off;
    let off = capture::write_topology(&mut conn, &topo, "test", None).unwrap();

    assert_eq!(desktop::placement_state_of(&conn, known).unwrap(), "known");
    assert_eq!(
        desktop::placement_state_of(&conn, empty).unwrap(),
        "known",
        "the compositor answered and there were no terminal windows; that is \
         an answer, not a gap"
    );
    assert_eq!(
        desktop::placement_state_of(&conn, unknown).unwrap(),
        "unknown",
        "a capture that could not read placement must say so, and must not be \
         readable as a machine with no windows"
    );
    assert_eq!(
        desktop::placement_state_of(&conn, off).unwrap(),
        "disabled",
        "nobody asked the compositor; nothing is owed and nothing failed"
    );

    for id in [empty, unknown, off] {
        assert!(
            desktop::placements_of(&conn, id).unwrap().is_empty(),
            "snapshot {id} must hold no placement rows"
        );
    }
    assert_eq!(desktop::placements_of(&conn, known).unwrap().len(), 1);

    common::shutdown(&t);
}

#[test]
fn a_placement_for_a_session_not_in_the_snapshot_is_kept_but_unlinked() {
    // Its workspace is still worth knowing even if the session vanished
    // between the tmux read and the compositor read.
    let t = server("orphan");
    let tmp = tempfile::tempdir().unwrap();
    let mut conn = db::open(&tmp.path().join("state.db")).unwrap();

    let mut topo = capture::collect(&t).unwrap();
    topo.placements = Placements::Known(vec![place("ghost", "4", "DP-1")]);
    let snap = capture::write_topology(&mut conn, &topo, "test", None).unwrap();

    let linked: Option<i64> = conn
        .query_row(
            "SELECT session_row_id FROM terminal_windows WHERE snapshot_id = ?1",
            [snap],
            |r| r.get(0),
        )
        .unwrap();
    assert!(linked.is_none(), "an unknown session links to nothing");
    assert_eq!(
        desktop::placements_of(&conn, snap).unwrap().len(),
        1,
        "but the row is kept"
    );

    common::shutdown(&t);
}

#[test]
fn a_v9_database_gains_session_name_without_losing_its_placement() {
    // The column was added in 10. A database written by an earlier build
    // must upgrade rather than be discarded — Plan 1 established that
    // opening a database never destroys it.
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("old.db");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            // `unresolved` (added at 5) and `server` (added at 7) are part of
            // what a real v9 `snapshots` holds; a fixture without them is not
            // a v9 database, and the migration is entitled to expect them.
            "CREATE TABLE snapshots (id INTEGER PRIMARY KEY, taken_at INTEGER NOT NULL,
               boot_id TEXT NOT NULL, reason TEXT NOT NULL, state TEXT NOT NULL,
               unresolved INTEGER NOT NULL DEFAULT 0, server TEXT);
             CREATE TABLE session_rows (row_id INTEGER PRIMARY KEY, snapshot_id INTEGER,
               name TEXT NOT NULL);
             CREATE TABLE terminal_windows (row_id INTEGER PRIMARY KEY,
               snapshot_id INTEGER NOT NULL, hypr_address TEXT NOT NULL,
               window_class TEXT NOT NULL, terminal_kind TEXT NOT NULL,
               session_row_id INTEGER, workspace_kind TEXT NOT NULL,
               workspace_ref TEXT NOT NULL, monitor_connector TEXT NOT NULL,
               monitor_desc TEXT, monitor_scale REAL, monitor_transform INTEGER,
               floating INTEGER NOT NULL DEFAULT 0,
               rel_x REAL, rel_y REAL, rel_w REAL, rel_h REAL);
             CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO snapshots (id, taken_at, boot_id, reason, state)
               VALUES (1, 1, 'b', 'r', 'complete');
             INSERT INTO session_rows VALUES (11, 1, 'dev');
             INSERT INTO terminal_windows (row_id, snapshot_id, hypr_address, window_class,
               terminal_kind, session_row_id, workspace_kind, workspace_ref,
               monitor_connector, floating)
               VALUES (1, 1, '0xA', 'ghostty', 'ghostty', 11, 'numbered', '5', 'DP-1', 0);
             INSERT INTO meta VALUES ('schema_version', '9');",
        )
        .unwrap();
    }

    // Opening must migrate, not discard.
    let conn = osm::db::open(&path).unwrap();
    let ps = osm::desktop::placements_of(&conn, 1).unwrap();
    assert_eq!(ps.len(), 1, "the placement survived the upgrade: {ps:?}");
    assert_eq!(
        ps[0].session, "dev",
        "and its session name was backfilled from the row link"
    );
    assert_eq!(ps[0].workspace_ref, "5");
}
