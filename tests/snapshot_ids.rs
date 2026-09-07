//! Snapshot ids, and the one property the maintainer's outage turned on.
//!
//! `snapshots.id` was declared `INTEGER PRIMARY KEY` — a plain rowid. SQLite
//! allocates one of those as `max(id) + 1`, so deleting the highest row hands
//! its id straight back to the next insert. On its own that is harmless: the
//! snapshot that had it is gone. It stops being harmless the moment anything
//! *else* still claims that id, because the child tables are keyed by it and
//! `window_rows` is `UNIQUE (snapshot_id, tmux_window_id)`.
//!
//! That is what happened. 198 rows in the maintainer's database — 72
//! `window_rows`, 72 `session_rows`, 54 `terminal_windows` — outlived the
//! snapshots 3717…3725 they belonged to, the highest surviving snapshot was
//! 3716, and every capture from then on was handed 3717, met those rows and
//! died with
//!
//! ```text
//! UNIQUE constraint failed: window_rows.snapshot_id, window_rows.tmux_window_id
//! ```
//!
//! for 83 minutes and 41 systemd restarts. Nothing in the engine could have
//! recovered from it: the next capture would be handed 3717 again, for ever.

mod common;

use osm::{capture, db, tmux::Tmux};

struct Server(Tmux);

impl Server {
    fn start(label: &str) -> Self {
        let sock = format!("osm-ids-{}-{}", label, std::process::id());
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

/// Strand `newest`'s child rows: delete the snapshot row itself on a
/// connection that enforces nothing, which is what left the maintainer's
/// database in this shape.
///
/// `PRAGMA foreign_keys` is a **per-connection** setting, and the SQLite the
/// rest of the world links against defaults it off: the `sqlite3` shell on
/// this machine reports `0`, and so does anything built on a stock
/// `libsqlite3`. (osm's own bundled SQLite is compiled with
/// `SQLITE_DEFAULT_FOREIGN_KEYS=1`, which is why this has to be switched off
/// explicitly here — the point being that no connection *outside* this
/// process is obliged to do the same.) The trigger this build now carries is
/// dropped for the same reason: the subject of this test is a database that
/// has *somehow* reached the broken state, and it must not be able to reach it
/// only through the guards being tested elsewhere.
fn strand_children_of(path: &std::path::Path, newest: i64) {
    let raw = rusqlite::Connection::open(path).unwrap();
    raw.pragma_update(None, "foreign_keys", "OFF").unwrap();
    let fk: i64 = raw
        .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
        .unwrap();
    assert_eq!(fk, 0, "this connection must enforce no foreign keys");
    raw.execute_batch("DROP TRIGGER IF EXISTS snapshots_cascade_delete")
        .unwrap();
    raw.execute("DELETE FROM snapshots WHERE id = ?1", [newest])
        .unwrap();
}

/// Rows in the child tables that name a snapshot no longer on record.
fn orphan_rows(conn: &rusqlite::Connection) -> i64 {
    conn.query_row(
        "SELECT (SELECT COUNT(*) FROM window_rows
                  WHERE snapshot_id NOT IN (SELECT id FROM snapshots))
              + (SELECT COUNT(*) FROM session_rows
                  WHERE snapshot_id NOT IN (SELECT id FROM snapshots))
              + (SELECT COUNT(*) FROM terminal_windows
                  WHERE snapshot_id NOT IN (SELECT id FROM snapshots))",
        [],
        |r| r.get(0),
    )
    .unwrap()
}

/// **The decisive test.** A database whose highest snapshot is `N-1` while
/// child rows still claim `N`, and a capture that has to land anyway.
///
/// Both snapshots are written by the real capture path against a real tmux
/// server, so the stranded rows are the rows a capture actually writes —
/// eight windows' worth on the maintainer's machine, one window's worth here
/// — rather than a hand-built approximation of them.
///
/// The capture at the end runs on the *same* connection, deliberately: this
/// is about which id the insert is handed, and reopening the database would
/// let the repair in [`osm::db::open`] answer the question instead.
#[test]
fn a_capture_lands_when_child_rows_already_claim_the_next_id() {
    let s = Server::start("reuse");
    let t = &s.0;
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("state.db");

    let mut conn = db::open(&path).unwrap();
    let first = capture::snapshot(&mut conn, t, "test").unwrap();
    let second = capture::snapshot(&mut conn, t, "test").unwrap();
    assert!(second > first);
    drop(conn);

    strand_children_of(&path, second);

    let mut conn = db::open(&path).unwrap();
    let highest: i64 = conn
        .query_row("SELECT MAX(id) FROM snapshots", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        highest, first,
        "the fixture must leave {second} as the id nothing holds any more"
    );

    let third = capture::snapshot(&mut conn, t, "test")
        .expect("a capture must not be stopped by rows belonging to a snapshot that is gone");
    assert!(
        third > second,
        "capture was handed id {third}, which rows already claim; \
         snapshot ids must never be reused"
    );
}

/// The property underneath it, stated on its own: an id, once handed out, is
/// never handed out again — however many snapshots are deleted afterwards,
/// and whether or not anything was left behind.
#[test]
fn an_id_is_never_handed_out_twice() {
    let s = Server::start("monotonic");
    let t = &s.0;
    let tmp = tempfile::tempdir().unwrap();
    let mut conn = db::open(&tmp.path().join("state.db")).unwrap();

    let first = capture::snapshot(&mut conn, t, "test").unwrap();
    let second = capture::snapshot(&mut conn, t, "test").unwrap();
    // Everything on record, gone — the ordinary end of a retention pass, and
    // the state `osm uninstall` leaves short of removing the file.
    conn.execute("DELETE FROM snapshots", []).unwrap();
    assert_eq!(
        orphan_rows(&conn),
        0,
        "the delete must take the rows with it"
    );

    let third = capture::snapshot(&mut conn, t, "test").unwrap();
    assert!(
        third > second && second > first,
        "ids went {first}, {second}, {third}: an emptied table must not \
         restart the sequence"
    );
}
