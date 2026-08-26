//! A tmux server that is replaced **while a restore is running** did not build
//! what the earlier part of that restore built.
//!
//! `restore_tree` records each session as delivered the moment its own tmux
//! calls return, and the only identity check ran once at the end: if the
//! server had no identity before the work and one after it — the ordinary boot
//! case — the final identity was accepted outright, and if it changed, the
//! sessions already recorded as delivered were left in `created` regardless.
//! So a server that died between two sessions cost the user the ones it had
//! already built, while the run discharged their debt, published the surviving
//! topology and retired the source that was the only remaining record of them.
//!
//! The existing restart test restarts tmux *after* `run_restore` returns,
//! which is why this survived five reviews.
//!
//! Every tmux server here is private, addressed by a `-L` socket name carrying
//! this process's id.

mod common;

use osm::{capture, db, restore, tmux::Tmux};

struct Server(Tmux);

impl Server {
    fn start(label: &str) -> Self {
        Server(Tmux::with_socket(&format!(
            "osm-midrestart-{}-{}",
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

fn live_names(t: &Tmux) -> Vec<String> {
    let mut names: Vec<String> = t
        .list_sessions()
        .map(|v| v.into_iter().map(|s| s.name).collect())
        .unwrap_or_default();
    names.sort();
    names
}

/// Three sessions captured from a previous boot, named so that tmux — and
/// therefore the snapshot, and therefore the restore — visits them in the
/// order `alpha`, `mid`, `zbeta`.
fn captured_three(src: &Server) -> (tempfile::TempDir, rusqlite::Connection, i64) {
    for name in ["alpha", "mid", "zbeta"] {
        src.t()
            .run(&["new-session", "-d", "-s", name, "-n", name, "-c", "/tmp"])
            .unwrap();
    }
    let tmp = tempfile::tempdir().unwrap();
    let mut conn = db::open(&tmp.path().join("state.db")).unwrap();
    let snap = capture::snapshot(&mut conn, src.t(), "test").unwrap();
    conn.execute("UPDATE snapshots SET boot_id='boot-a' WHERE id=?1", [snap])
        .unwrap();
    (tmp, conn, snap)
}

/// The server that built `alpha` is killed the instant `mid` is created, so
/// `alpha` is gone and `zbeta` is rebuilt on a server that never held it.
///
/// Nothing this restore did survived on one server, so nothing may be reported
/// as delivered and nothing may be discharged — least of all `alpha`, whose
/// only remaining record is the source snapshot.
#[test]
fn a_server_replaced_between_two_sessions_delivers_nothing() {
    let src = Server::start("src");
    let (_tmp, mut conn, snap) = captured_three(&src);
    drop(src);

    let dst = Server::start("dst");
    // The trap needs a server to be armed on, and a server needs a session.
    // `keeper` is not in the snapshot and plays no other part.
    dst.t()
        .run(&[
            "new-session",
            "-n",
            "code",
            "-d",
            "-s",
            "keeper",
            "-c",
            "/tmp",
        ])
        .unwrap();
    dst.t()
        .run(&[
            "set-hook",
            "-g",
            "after-new-session",
            "if -F \"#{==:#{session_name},mid}\" kill-server",
        ])
        .unwrap();

    let report = restore::run_restore(&mut conn, dst.t(), false).unwrap();

    // The fixture has to have actually destroyed the first server, or this
    // test proves nothing.
    let live = live_names(dst.t());
    assert!(
        !live.contains(&"alpha".to_string()),
        "the fixture must destroy the server that built alpha; live: {live:?}"
    );
    assert!(
        !live.contains(&"keeper".to_string()),
        "the fixture must destroy the *first* server; live: {live:?}"
    );

    assert_ne!(
        report.state, "succeeded",
        "a restore whose server was replaced under it verified nothing: {report:?}"
    );
    assert!(
        owed(&conn, snap).contains(&"alpha".to_string()),
        "alpha died with the server that built it, so its debt must survive; \
         owed={:?} report={report:?}",
        owed(&conn, snap)
    );
    assert!(
        report.outcome.created.is_empty() && report.outcome.adopted.is_empty(),
        "nothing was delivered under one continuously verified server: {report:?}"
    );

    let state: String = conn
        .query_row("SELECT state FROM snapshots WHERE id=?1", [snap], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        state, "complete",
        "the source must stay selectable so a later restore can try again"
    );

    let stamped: Option<String> = conn
        .query_row(
            "SELECT destination_server FROM restore_attempts WHERE snapshot_id=?1",
            [snap],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        stamped.is_none(),
        "no single server built this attempt, so its window ids belong to none \
         of them: {stamped:?}"
    );
}
