//! A tmux server that is *there* and cannot be identified is a hard failure,
//! and it is not the same thing as no tmux server at all.
//!
//! Both used to arrive as `server: None`, and `None` was then read everywhere
//! downstream as "there is no server". The consequences were not symmetric
//! with the cost:
//!
//! * a **capture** wrote its rows with no incarnation attached. tmux reissues
//!   `$0`, `@0` and `%0` to every server it starts, so an unattributed graph's
//!   ids are satisfied by whichever server is asked next — including a session
//!   list from one server sitting beside another's windows and panes, a shape
//!   every structural check accepts;
//! * a **restore** saw `(None, None)` at every identity check and called that
//!   "nothing changed". A server with, say, `@osm-server-id mine` in the
//!   user's configuration therefore made a mid-restore restart completely
//!   invisible: the run reported success, discharged the debt and retired the
//!   only snapshot that still held the sessions.
//!
//! Every tmux server here is private, addressed by a `-L` socket name carrying
//! this process's id.

mod common;

use osm::{capture, db, restore, tmux::Tmux};

/// What a user's configuration can put in osm's option. Not hex, so there is
/// no reading of it under which osm minted it.
const NOT_MINTED: &str = "mine";

struct Server(Tmux);

impl Server {
    fn start(label: &str) -> Self {
        Server(Tmux::with_socket(&format!(
            "osm-unusable-{}-{}",
            label,
            std::process::id()
        )))
    }
    fn t(&self) -> &Tmux {
        &self.0
    }
    fn session(&self, name: &str) {
        let args = ["new-session", "-d", "-s", name, "-c", "/tmp"];
        let mut started = self.0.run(&args);
        for _ in 0..50 {
            if started.is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
            started = self.0.run(&args);
        }
        started.expect("a server on the socket this test owns");
    }
    /// Put the user's unusable value in osm's option.
    fn misconfigure(&self) {
        self.0
            .run(&["set-option", "-s", "@osm-server-id", NOT_MINTED])
            .unwrap();
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        common::shutdown(&self.0);
    }
}

fn snapshot_count(conn: &rusqlite::Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM snapshots", [], |r| r.get(0))
        .unwrap()
}

/// A capture against a server whose identity is unusable must fail loudly,
/// not record rows that belong to nobody.
#[test]
fn a_capture_refuses_a_server_that_cannot_be_identified() {
    let s = Server::start("capture");
    s.session("alpha");
    s.misconfigure();

    let tmp = tempfile::tempdir().unwrap();
    let mut conn = db::open(&tmp.path().join("state.db")).unwrap();
    let err = capture::snapshot(&mut conn, s.t(), "test")
        .expect_err("a server that will not identify itself has no capturable state");
    let text = format!("{err:#}");
    assert!(
        text.contains("@osm-server-id") && text.contains(NOT_MINTED),
        "the error must say what is wrong and where: {text}"
    );
    assert_eq!(
        snapshot_count(&conn),
        0,
        "nothing may be written for a server nothing can be attributed to"
    );
}

/// The same server, but as a restore destination: the run must not report a
/// success, and the source must stay exactly as restorable as it was.
#[test]
fn a_restore_refuses_a_destination_that_cannot_be_identified() {
    let src = Server::start("restore-src");
    src.session("alpha");
    let tmp = tempfile::tempdir().unwrap();
    let mut conn = db::open(&tmp.path().join("state.db")).unwrap();
    let snap = capture::snapshot(&mut conn, src.t(), "test").unwrap();
    conn.execute("UPDATE snapshots SET boot_id='boot-a' WHERE id=?1", [snap])
        .unwrap();
    drop(src);

    let dst = Server::start("restore-dst");
    dst.session("keeper");
    dst.misconfigure();

    let report = restore::run_restore(&mut conn, dst.t(), false).unwrap();
    assert_ne!(
        report.state, "succeeded",
        "a restore that cannot name the server it is building on has verified nothing: \
         {report:?}"
    );
    assert!(
        report.retryable,
        "the source must stay selectable so a later run can try again: {report:?}"
    );

    let state: String = conn
        .query_row("SELECT state FROM snapshots WHERE id=?1", [snap], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        state, "complete",
        "the source is the only record of alpha and must not be retired"
    );
    let owed: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM session_rows WHERE snapshot_id=?1 AND unresolved=1",
            [snap],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        owed, 1,
        "alpha cannot be shown to be anywhere, so it is still owed"
    );
    assert_eq!(
        snapshot_count(&conn),
        1,
        "no post-restore snapshot may be published for an unidentifiable server"
    );
}

/// The distinction the two tests above rest on: no server at all is not an
/// error, it is a state.
#[test]
fn no_server_at_all_reports_no_identity_rather_than_a_failure() {
    let s = Server::start("absent");
    assert_eq!(
        s.t()
            .running_server_incarnation()
            .expect("a socket with no server on it is not an error"),
        None
    );

    s.session("alpha");
    let present = s
        .t()
        .running_server_incarnation()
        .expect("a healthy server identifies itself")
        .expect("a running server has an incarnation");

    s.misconfigure();
    s.t()
        .running_server_incarnation()
        .expect_err("a running server that will not identify itself is an error, not a None");

    assert!(
        present.contains(&format!("osm-unusable-absent-{}", std::process::id())),
        "the identity must name the socket this test owns, so it is this server's: \
         {present:?}"
    );
}
