//! A restore's captured→live window map is a statement about **one** tmux
//! server, and it must stop being believed the moment that server is gone.
//!
//! tmux hands out `@0`, `@1`, … from zero to every server it starts, so a
//! restart within one boot — a crash, a `kill-server`, a user starting their
//! session manager again — reissues the very ids a previous restore wrote
//! down. Nothing in the mapping said which server it belonged to, so every
//! mapping ever recorded for a snapshot stayed eligible forever, and the only
//! thing between a carried session and an unrelated window was a window name
//! and a pane count. The authors had already named this hole themselves:
//! "same-boot window identity relies on a name+pane-count check that a
//! within-boot tmux restart could fool".
//!
//! Every tmux server started here is private, addressed by a `-L` socket name
//! carrying this process's id.

mod common;

use osm::{boot, capture, db, restore, tmux::Tmux};

struct Server(Tmux);

impl Server {
    fn start(label: &str) -> Self {
        Server(Tmux::with_socket(&format!(
            "osm-serverid-{}-{}",
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

fn session_names(conn: &rusqlite::Connection, snap: i64) -> Vec<String> {
    let mut names: Vec<String> = conn
        .prepare("SELECT name FROM session_rows WHERE snapshot_id=?1")
        .unwrap()
        .query_map([snap], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    names.sort();
    names
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

/// How many *distinct windows* named `name` a snapshot holds. One means the
/// carried session and the live one are recorded as sharing a window; two
/// means each has its own.
fn window_rows_named(conn: &rusqlite::Connection, snap: i64, name: &str) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM window_rows WHERE snapshot_id=?1 AND name=?2",
        rusqlite::params![snap, name],
        |r| r.get(0),
    )
    .unwrap()
}

/// How many sessions of a snapshot are linked into the window the live server
/// calls `tmux_id`.
fn links_to(conn: &rusqlite::Connection, snap: i64, tmux_id: &str) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM session_window_links l
         JOIN window_rows w ON w.row_id = l.window_row_id
         WHERE w.snapshot_id = ?1 AND w.tmux_window_id = ?2",
        rusqlite::params![snap, tmux_id],
        |r| r.get(0),
    )
    .unwrap()
}

/// The live window id a restore recorded for the captured window called
/// `window`.
fn mapped_live_window(conn: &rusqlite::Connection, snap: i64, window: &str) -> String {
    conn.query_row(
        "SELECT m.live_window_id
         FROM restore_window_map m
         JOIN restore_attempts a ON a.id = m.attempt_id
         JOIN window_rows w ON w.tmux_window_id = m.captured_window_id
                           AND w.snapshot_id = a.snapshot_id
         WHERE a.snapshot_id = ?1 AND w.name = ?2",
        rusqlite::params![snap, window],
        |r| r.get(0),
    )
    .unwrap()
}

/// Makes one captured window's restore fail: far more panes than its captured
/// 80x24 layout can hold, so `split-window` fails once it is full. Aimed at a
/// named window so breaking `beta` does not break the session it shares a
/// window with.
fn break_window_restore(conn: &rusqlite::Connection, snap: i64, session: &str, window: &str) {
    let window_row: i64 = conn
        .query_row(
            "SELECT w.row_id FROM window_rows w
             JOIN session_window_links l ON l.window_row_id = w.row_id
             JOIN session_rows s ON s.row_id = l.session_row_id
             WHERE s.snapshot_id = ?1 AND s.name = ?2 AND w.name = ?3",
            rusqlite::params![snap, session, window],
            |r| r.get(0),
        )
        .unwrap();
    for idx in 1..9 {
        conn.execute(
            "INSERT INTO pane_rows (window_row_id, tmux_pane_id, idx, cwd, restore_policy)
             VALUES (?1, ?2, ?3, '/tmp', 'shell')",
            rusqlite::params![window_row, format!("%80{idx}"), idx],
        )
        .unwrap();
    }
}

/// `alpha` and `beta` sharing one window called `shared`, captured from a
/// previous boot, with `beta`'s own window rigged to fail its restore.
fn captured_pair(src: &Server) -> (tempfile::TempDir, rusqlite::Connection, i64) {
    let t = src.t();
    for (name, own) in [("alpha", "own"), ("beta", "own2")] {
        t.run(&["new-session", "-d", "-s", name, "-n", own, "-c", "/tmp"])
            .unwrap();
    }
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
    t.run(&["link-window", "-d", "-s", "alpha:shared", "-t", "beta:"])
        .unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let mut conn = db::open(&tmp.path().join("state.db")).unwrap();
    let snap = capture::snapshot(&mut conn, t, "test").unwrap();
    conn.execute("UPDATE snapshots SET boot_id='boot-a' WHERE id=?1", [snap])
        .unwrap();
    break_window_restore(&conn, snap, "beta", "own2");
    (tmp, conn, snap)
}

/// A tmux restart within one boot, on the same socket: the ids start again
/// from `@0`, so the previous server's `@N` is reissued to something with
/// nothing to do with it.
///
/// Builds `gamma` on the fresh server with enough windows that the one called
/// `shared` — a single pane, exactly like the captured one — lands on `id`.
fn rebuild_server_reaching(t: &Tmux, id: &str) {
    let n: u32 = id
        .trim_start_matches('@')
        .parse()
        .expect("a tmux window id");
    let name = |i: u32| {
        if i == n {
            "shared".to_string()
        } else {
            format!("filler{i}")
        }
    };
    // `kill-server` returns before the socket is gone, and a client that
    // reaches the dying server is told "server exited unexpectedly". Removing
    // the socket this test owns — never a glob, never a directory — makes the
    // next command start a genuinely new server, and the retry absorbs the
    // instant in which the old one is still unlinking it.
    let _ = t.run(&["kill-server"]);
    for path in common::socket_paths(t) {
        let _ = std::fs::remove_file(path);
    }
    let start = [
        "new-session",
        "-d",
        "-s",
        "gamma",
        "-n",
        &name(0),
        "-c",
        "/tmp",
    ];
    let mut started = t.run(&start);
    for _ in 0..50 {
        if started.is_ok() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
        started = t.run(&start);
    }
    started.expect("a fresh server on the same socket");
    for i in 1..=n {
        t.run(&[
            "new-window",
            "-d",
            "-t",
            "gamma",
            "-n",
            &name(i),
            "-c",
            "/tmp",
        ])
        .unwrap();
    }
    let built: Vec<String> = t
        .list_windows()
        .unwrap()
        .into_iter()
        .filter(|w| w.id == id && w.name == "shared")
        .map(|w| w.id)
        .collect();
    assert_eq!(
        built.len(),
        1,
        "the fixture must reissue {id} to a window called 'shared'"
    );
}

/// The restart case. Boot A captured `alpha` and `beta` sharing `shared`; a
/// restore delivered `alpha` and recorded which live window `shared` became;
/// tmux then restarted and handed that same id to an unrelated window with a
/// matching name and pane count. Carrying `beta` forward must not link it into
/// that window — the mapping describes a server that no longer exists.
#[test]
fn a_window_map_is_not_believed_after_the_server_that_made_it_restarted() {
    let src = Server::start("restart-src");
    let (_tmp, mut conn, snap_a) = captured_pair(&src);
    drop(src);

    let dst = Server::start("restart-dst");
    let report = restore::run_restore(&mut conn, dst.t(), false).unwrap();
    assert_eq!(
        report.outcome.created,
        vec!["alpha".to_string()],
        "{report:?}"
    );
    assert_eq!(
        owed(&conn, snap_a),
        vec!["beta".to_string()],
        "only the session that was not delivered is still owed"
    );

    let live_id = mapped_live_window(&conn, snap_a, "shared");
    rebuild_server_reaching(dst.t(), &live_id);

    let snap_b = capture::snapshot(&mut conn, dst.t(), "hook").unwrap();
    assert_eq!(
        session_names(&conn, snap_b),
        vec!["beta".to_string(), "gamma".to_string()],
        "the capture records what is live and carries what is owed"
    );
    assert_eq!(
        links_to(&conn, snap_b, &live_id),
        1,
        "{live_id} belongs to gamma alone; a mapping from a dead server must \
         not link a carried session into it"
    );
    assert_eq!(
        window_rows_named(&conn, snap_b, "shared"),
        2,
        "the carried beta must bring its own window, not adopt an unrelated \
         one that merely reuses the id"
    );
}

/// The same server, two attempts: the newer one recorded no mapping for the
/// shared window, so nothing on this machine says which live window it is now.
/// An older attempt's answer must not fill that gap — it describes windows
/// this run may since have rebuilt or replaced.
#[test]
fn an_older_attempts_mapping_does_not_fill_the_newest_attempts_gap() {
    let src = Server::start("gap-src");
    let (_tmp, mut conn, snap_a) = captured_pair(&src);
    drop(src);

    let dst = Server::start("gap-dst");
    let report = restore::run_restore(&mut conn, dst.t(), false).unwrap();
    assert_eq!(
        report.outcome.created,
        vec!["alpha".to_string()],
        "{report:?}"
    );
    let _ = dst.t().run(&["kill-session", "-t", "beta"]);

    // A second attempt against the very same server that got nowhere: it has
    // an identity, and no mappings at all.
    let server = dst
        .t()
        .server_incarnation()
        .expect("the destination server must have an identity");
    conn.execute(
        "INSERT INTO restore_attempts (snapshot_id, started_at, state, destination_server)
         VALUES (?1, ?2, 'failed', ?3)",
        rusqlite::params![snap_a, boot::now_epoch(), server],
    )
    .unwrap();

    let snap_b = capture::snapshot(&mut conn, dst.t(), "hook").unwrap();
    assert_eq!(
        session_names(&conn, snap_b),
        vec!["alpha".to_string(), "beta".to_string()]
    );
    assert_eq!(
        window_rows_named(&conn, snap_b, "shared"),
        2,
        "with no mapping from the attempt that ran last, the carried window is \
         copied; an older attempt must not answer for it"
    );
}

/// What a server identity built out of the server's own *public* facts looks
/// like: boot id, pid, the second it started, and the socket it listens on.
///
/// This is not a helper the engine uses — it is the shape the engine must
/// **not** have. Every field in it is readable by anyone, reproducible by
/// anyone, and — pid and second both — reusable by the next server on the
/// same socket.
fn public_facts_token(t: &Tmux) -> String {
    let facts = t
        .run(&[
            "display-message",
            "-p",
            "#{pid}:#{start_time}:#{socket_path}",
        ])
        .expect("the server must report its own public facts");
    format!("{}:{}", boot::current_boot_id().unwrap(), facts.trim())
}

/// A server's identity must not be a function of facts another server can
/// reproduce.
///
/// tmux stores its start time as a `timeval`, but the format layer emits only
/// `tv_sec` — the subsecond half never reaches osm. A pid is reused within a
/// boot (force it with a small `pid_max` and a little process churn), and two
/// servers started in the same second on the same `-L` socket then produce a
/// byte-identical `{boot}:{pid}:{start_time}:{socket}`. The dead server's
/// window map is accepted against the live server, and its reissued `@N`s
/// point at whatever the new server happens to have put there.
#[test]
fn the_server_identity_is_not_the_tuple_any_server_can_reproduce() {
    let s = Server::start("token");
    s.t()
        .run(&["new-session", "-d", "-s", "x", "-c", "/tmp"])
        .unwrap();

    let identity = s.t().server_incarnation().expect("a running server");
    assert_ne!(
        identity,
        public_facts_token(s.t()),
        "the identity is exactly the pid/start-second/socket tuple, which the \
         next server on this socket can reproduce"
    );

    assert_eq!(
        identity,
        s.t().server_incarnation().unwrap(),
        "the identity must be stable: reading it twice must not mint a new one"
    );
}

/// The same server, restarted: the identity must change even though every
/// public fact about it *could* be the same.
#[test]
fn a_restarted_server_gets_a_new_identity() {
    let s = Server::start("newid");
    s.t()
        .run(&["new-session", "-d", "-s", "x", "-c", "/tmp"])
        .unwrap();
    let first = s.t().server_incarnation().unwrap();

    common::shutdown(s.t());
    let mut started = s.t().run(&["new-session", "-d", "-s", "x", "-c", "/tmp"]);
    for _ in 0..50 {
        if started.is_ok() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
        started = s.t().run(&["new-session", "-d", "-s", "x", "-c", "/tmp"]);
    }
    started.expect("a fresh server on the same socket");

    assert_ne!(
        first,
        s.t().server_incarnation().unwrap(),
        "a new server on the same socket must not inherit the old one's identity"
    );
}

/// The reuse case, with the reuse forced rather than waited for.
///
/// A same-second restart onto a reused pid leaves the *dead* server's attempt
/// carrying a `destination_server` byte-identical to the one the live server
/// reports for itself. Nothing else about that situation is reproducible in a
/// test — pid reuse needs a tiny `pid_max` and root — but the artifact it
/// leaves behind is exactly this row, so the row is what the test writes.
///
/// If the engine's identity is the public tuple, the mapping recorded by the
/// server that has since died is accepted against the live one, and the
/// carried `beta` is linked into `gamma`'s unrelated window.
#[test]
fn a_dead_servers_map_is_refused_even_when_its_public_facts_come_round_again() {
    let src = Server::start("reuse-src");
    let (_tmp, mut conn, snap_a) = captured_pair(&src);
    drop(src);

    let dst = Server::start("reuse-dst");
    let report = restore::run_restore(&mut conn, dst.t(), false).unwrap();
    assert_eq!(
        report.outcome.created,
        vec!["alpha".to_string()],
        "{report:?}"
    );
    let live_id = mapped_live_window(&conn, snap_a, "shared");
    rebuild_server_reaching(dst.t(), &live_id);

    // The forged collision: the attempt made by the server that is now dead
    // is stamped with the identity the *live* server's public facts produce.
    // That is byte for byte what a same-second restart onto a reused pid
    // would have left in this row.
    let collided = public_facts_token(dst.t());
    conn.execute(
        "UPDATE restore_attempts SET destination_server = ?2 WHERE snapshot_id = ?1",
        rusqlite::params![snap_a, collided],
    )
    .unwrap();

    let snap_b = capture::snapshot(&mut conn, dst.t(), "hook").unwrap();
    assert_eq!(
        links_to(&conn, snap_b, &live_id),
        1,
        "{live_id} belongs to gamma alone; a dead server's map must not link a \
         carried session into it just because the two servers' public facts match"
    );
    assert_eq!(
        window_rows_named(&conn, snap_b, "shared"),
        2,
        "the carried beta must bring its own window"
    );
}

/// The tmux server option osm keeps its identity in — named here, and not
/// imported, precisely because this is the thing a user's configuration can
/// reach.
const SERVER_ID_OPTION: &str = "@osm-server-id";

/// A structurally perfect id that osm did not mint: 32 hex characters, the
/// exact shape the check accepts.
const CONFIGURED_ID: &str = "0123456789abcdef0123456789abcdef";

/// Start a server on `t`'s socket with `@osm-server-id` already set, as a
/// `.tmux.conf` line or a restored dump of server options would leave it, and
/// return the identity osm then reports for it.
fn start_with_configured_id(t: &Tmux) -> String {
    let args = ["new-session", "-d", "-s", "x", "-c", "/tmp"];
    let mut started = t.run(&args);
    for _ in 0..50 {
        if started.is_ok() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
        started = t.run(&args);
    }
    started.expect("a server on the socket this test owns");
    t.run(&["set-option", "-s", SERVER_ID_OPTION, CONFIGURED_ID])
        .expect("the user's configuration must actually take");
    let identity = t.server_incarnation().expect("a running server");
    assert!(
        identity.contains(CONFIGURED_ID),
        "the fixture only means anything if osm really reads the configured value: {identity}"
    );
    identity
}

/// A pre-existing `@osm-server-id` is not evidence that osm minted it.
///
/// One `set-option -s @osm-server-id 0123…` in a `.tmux.conf` — or a dump of
/// server options restored wholesale — is applied to **every** server the user
/// ever starts. If the option's value is the identity, every one of those
/// servers is the same incarnation as far as osm can tell, which is exactly
/// the state a within-boot restart is supposed to be distinguishable from.
#[test]
fn two_servers_given_the_same_configured_id_are_still_told_apart() {
    let s = Server::start("configured");
    let first = start_with_configured_id(s.t());
    common::shutdown(s.t());
    let second = start_with_configured_id(s.t());
    assert_ne!(
        first, second,
        "a configured {SERVER_ID_OPTION} is a value anyone can copy; the identity must \
         be bound to something about this server process that cannot be"
    );
}

/// The same forgery, end to end, against the thing it would actually cost.
///
/// Boot A captured `alpha` and `beta` sharing `shared`; a restore delivered
/// `alpha` and recorded which live window `shared` became; tmux then restarted
/// and handed that same id to an unrelated window with a matching name and
/// pane count. Both servers carry the user's configured `@osm-server-id`, so
/// with the option alone as the identity the stale mapping is still believed:
/// the carried `beta` is linked into `gamma`'s window, the two sessions are
/// recorded as sharing it, and the only snapshot that still knew otherwise is
/// discharged and free to be pruned.
#[test]
fn a_configured_server_id_does_not_carry_a_window_map_across_a_restart() {
    let src = Server::start("cfg-src");
    let (_tmp, mut conn, snap_a) = captured_pair(&src);
    drop(src);

    let dst = Server::start("cfg-dst");
    // The user's configuration, in place before osm ever looks at this server.
    dst.t()
        .run(&["new-session", "-d", "-s", "keeper", "-c", "/tmp"])
        .unwrap();
    dst.t()
        .run(&["set-option", "-s", SERVER_ID_OPTION, CONFIGURED_ID])
        .unwrap();

    let report = restore::run_restore(&mut conn, dst.t(), false).unwrap();
    assert_eq!(
        report.outcome.created,
        vec!["alpha".to_string()],
        "{report:?}"
    );

    let live_id = mapped_live_window(&conn, snap_a, "shared");
    rebuild_server_reaching(dst.t(), &live_id);
    // ...and in place on the server that replaced it, exactly as the same
    // configuration file would put it there.
    dst.t()
        .run(&["set-option", "-s", SERVER_ID_OPTION, CONFIGURED_ID])
        .unwrap();

    let snap_b = capture::snapshot(&mut conn, dst.t(), "hook").unwrap();
    assert_eq!(
        links_to(&conn, snap_b, &live_id),
        1,
        "{live_id} belongs to gamma alone; a map from a server that merely shares a \
         configured id must not link a carried session into it"
    );
    assert_eq!(
        window_rows_named(&conn, snap_b, "shared"),
        2,
        "the carried beta must bring its own window"
    );
}
