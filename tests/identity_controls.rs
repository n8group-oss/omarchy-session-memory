//! The four things a server-identity check must **not** break.
//!
//! Twice now a fix in this area has cost more than the hole it closed: a
//! cross-session check that pinned a debt forever, and an identity check that
//! risked failing every restore. Tightening "which server did this work" is
//! exactly the kind of change whose damage shows up as a refusal in the
//! ordinary cases, so the ordinary cases are asserted here, deliberately in
//! one file, in terms of the public API only — so that this file compiles and
//! passes against the engine both before and after the identity work.
//!
//! Every tmux server here is private, addressed by a `-L` socket name carrying
//! this process's id.

mod common;

use osm::{capture, db, restore, tmux::Tmux};

struct Server(Tmux);

impl Server {
    fn start(label: &str) -> Self {
        Server(Tmux::with_socket(&format!(
            "osm-controls-{}-{}",
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
}

impl Drop for Server {
    fn drop(&mut self) {
        common::shutdown(&self.0);
    }
}

/// A previous boot's snapshot of `alpha` and `beta`, in a database of its own.
fn captured(src: &Server) -> (tempfile::TempDir, rusqlite::Connection, i64) {
    src.session("alpha");
    src.t()
        .run(&["new-session", "-d", "-s", "beta", "-c", "/tmp"])
        .unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let mut conn = db::open(&tmp.path().join("state.db")).unwrap();
    let snap = capture::snapshot(&mut conn, src.t(), "test").unwrap();
    conn.execute("UPDATE snapshots SET boot_id='boot-a' WHERE id=?1", [snap])
        .unwrap();
    (tmp, conn, snap)
}

fn live_names(t: &Tmux) -> Vec<String> {
    let mut names: Vec<String> = t
        .list_sessions()
        .map(|v| v.into_iter().map(|s| s.name).collect())
        .unwrap_or_default();
    names.sort();
    names
}

/// **Control 1.** The whole point of the engine still works: a snapshot from a
/// previous boot, restored into a healthy server, comes back and is recorded.
#[test]
fn a_normal_restore_into_a_healthy_server_still_succeeds_end_to_end() {
    let src = Server::start("normal-src");
    let (_tmp, mut conn, snap) = captured(&src);
    drop(src);

    let dst = Server::start("normal-dst");
    let report = restore::run_restore(&mut conn, dst.t(), false).unwrap();
    assert_eq!(report.state, "succeeded", "{report:?}");
    assert_eq!(
        report.outcome.created,
        vec!["alpha".to_string(), "beta".to_string()],
        "{report:?}"
    );
    assert_eq!(
        live_names(dst.t()),
        vec!["alpha".to_string(), "beta".to_string()]
    );

    let state: String = conn
        .query_row("SELECT state FROM snapshots WHERE id=?1", [snap], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(state, "restored", "a verified success retires the source");

    // This boot's replacement exists, and the incarnation it is recorded
    // against is the one the attempt says it built on. Those two agreeing is
    // what makes the published snapshot a statement about the restore's own
    // work rather than about whatever server happened to answer last.
    let (reason, published): (String, Option<String>) = conn
        .query_row(
            "SELECT reason, server FROM snapshots ORDER BY id DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(reason, "post_restore");
    let stamped: Option<String> = conn
        .query_row(
            "SELECT destination_server FROM restore_attempts WHERE snapshot_id=?1",
            [snap],
            |r| r.get(0),
        )
        .unwrap();
    assert!(published.is_some(), "the published snapshot names a server");
    assert_eq!(
        published, stamped,
        "the snapshot published by a restore belongs to the server the restore built on"
    );
}

/// **Control 2.** A machine that has never run tmux: nothing to restore is a
/// state, not a failure, and nothing may be invented to fill it.
#[test]
fn a_first_ever_run_with_no_tmux_server_at_all_is_not_a_failure() {
    let s = Server::start("empty");
    let tmp = tempfile::tempdir().unwrap();
    let mut conn = db::open(&tmp.path().join("state.db")).unwrap();

    let report = restore::run_restore(&mut conn, s.t(), false)
        .expect("with nothing to restore, a restore is a no-op and not an error");
    assert_eq!(report.state, "nothing_to_restore", "{report:?}");
    assert_eq!(report.reason, "no_previous_boot_snapshot", "{report:?}");
    assert!(report.snapshot_id.is_none(), "{report:?}");

    // And a capture with no server on the socket refuses rather than writing
    // an empty snapshot that a later restore would select and "restore".
    let err =
        capture::snapshot(&mut conn, s.t(), "test").expect_err("there is no server to capture");
    let text = format!("{err:#}");
    assert!(
        text.contains("list sessions") && text.contains("error connecting"),
        "the error must be tmux's own — there is no server to talk to — and not \
         something invented about identity: {text}"
    );
    let snapshots: i64 = conn
        .query_row("SELECT COUNT(*) FROM snapshots", [], |r| r.get(0))
        .unwrap();
    assert_eq!(snapshots, 0);

    // Starting tmux for the first time then makes both work.
    s.session("alpha");
    capture::snapshot(&mut conn, s.t(), "test").expect("a server that exists is capturable");
}

/// **Control 3.** A user whose tmux configuration sets `@`-options of its own —
/// which is ordinary; plugins and themes do it constantly — is untouched.
///
/// The options set here are at every scope a user can reach: server (`-s`),
/// global session (`-g`) and window (`-w -g`), including one whose name is a
/// near miss for osm's own.
#[test]
fn unrelated_at_options_in_the_users_config_change_nothing() {
    let src = Server::start("opts-src");
    let (_tmp, mut conn, _snap) = captured(&src);
    drop(src);

    let dst = Server::start("opts-dst");
    dst.session("keeper");
    for args in [
        ["set-option", "-s", "@my-plugin-state", "on"],
        ["set-option", "-g", "@theme-flavour", "mocha"],
        ["set-option", "-g", "@osm-server-idle", "no"],
        ["set-option", "-wg", "@catppuccin_window_style", "rounded"],
    ] {
        dst.t()
            .run(&args)
            .unwrap_or_else(|e| panic!("the fixture option {args:?} must take: {e:#}"));
    }

    let report = restore::run_restore(&mut conn, dst.t(), false).unwrap();
    assert_eq!(
        report.state, "succeeded",
        "an unrelated @-option is not osm's business: {report:?}"
    );
    assert_eq!(
        live_names(dst.t()),
        vec![
            "alpha".to_string(),
            "beta".to_string(),
            "keeper".to_string()
        ]
    );

    capture::snapshot(&mut conn, dst.t(), "hook")
        .expect("a capture is not affected by the user's other options either");
}

/// **Control 4.** A client attaching to, and detaching from, the destination
/// server does not make the restore or the capture around it fail.
///
/// A real client needs a real terminal, so one is borrowed from a second,
/// private tmux server: a pane on the helper server runs `tmux attach` against
/// the server under test, which gives it a genuine pty-backed client. Killing
/// the helper detaches it, exactly as closing a terminal window would.
#[test]
fn a_client_attaching_and_detaching_does_not_break_a_restore() {
    let src = Server::start("client-src");
    let (_tmp, mut conn, _snap) = captured(&src);
    drop(src);

    let dst = Server::start("client-dst");
    dst.session("keeper");

    let helper = Server::start("client-helper");
    let attach = format!(
        "env -u TMUX tmux -L {} attach -t keeper",
        dst.t().socket().expect("the destination is on a -L socket")
    );
    helper
        .t()
        .run(&["new-session", "-d", "-s", "holder", "-c", "/tmp", &attach])
        .unwrap();

    let mut clients = String::new();
    for _ in 0..100 {
        clients = dst
            .t()
            .run(&["list-clients", "-F", "#{client_tty}"])
            .unwrap_or_default();
        if !clients.trim().is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(
        !clients.trim().is_empty(),
        "the fixture must actually attach a client to the destination server"
    );

    let report = restore::run_restore(&mut conn, dst.t(), false).unwrap();
    assert_eq!(
        report.state, "succeeded",
        "a client sitting on the destination is not a change of server: {report:?}"
    );

    // Now detach it, the way closing the terminal does, and capture again.
    drop(helper);
    let mut left = String::new();
    for _ in 0..100 {
        left = dst
            .t()
            .run(&["list-clients", "-F", "#{client_tty}"])
            .unwrap_or_default();
        if left.trim().is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(
        left.trim().is_empty(),
        "the fixture must actually detach the client before the capture below can \
         exercise the detached case: {left:?}"
    );

    capture::snapshot(&mut conn, dst.t(), "hook")
        .expect("a detach is not a change of server either");
}
