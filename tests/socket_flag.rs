//! Regression test for `osm --socket <NAME>`.
//!
//! This flag is the fix for a real data-loss incident: with no way to target
//! a non-default tmux server, an earlier test run reached for an env var
//! tmux 3.7c on this machine turns out to ignore, then ran `tmux
//! kill-server` against what turned out to be the *default* server,
//! destroying a live 7-session, 24-pane development environment. This test
//! proves the flag actually redirects every tmux call the `osm` binary
//! makes: it starts a server on a private socket (via `-L`, the mechanism
//! that does work on every tmux version) holding one uniquely-named
//! session, runs `osm --socket <name> snapshot`, and asserts the captured
//! snapshot contains that session. It deliberately makes no assertion
//! about the default server's contents — the point is to never touch it,
//! not to inspect it.

mod common;

use osm::tmux::Tmux;
use std::process::Command;

struct Server(Tmux);

impl Drop for Server {
    fn drop(&mut self) {
        common::shutdown(&self.0);
    }
}

#[test]
fn socket_flag_captures_from_the_named_server_not_the_default_one() {
    let socket = format!("osm-socketflag-{}", std::process::id());
    let session_name = format!("osm-socketflag-session-{}", std::process::id());
    let t = Tmux::with_socket(&socket);
    let _server = Server(t.clone());

    t.run(&["new-session", "-d", "-s", &session_name, "-c", "/tmp"])
        .expect("start private tmux server");

    let tmp = tempfile::tempdir().unwrap();
    common::write_headless_config(&tmp.path().join("config"));
    let out = Command::new(env!("CARGO_BIN_EXE_osm"))
        .env("XDG_STATE_HOME", tmp.path().join("state"))
        .env("XDG_CONFIG_HOME", tmp.path().join("config"))
        .args(["--socket", &socket, "snapshot"])
        .output()
        .expect("run osm --socket snapshot");
    assert!(
        out.status.success(),
        "osm --socket {socket} snapshot failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let db = tmp.path().join("state/osm/state.db");
    let conn = osm::db::open(&db).unwrap();
    let names: Vec<String> = {
        let mut stmt = conn.prepare("SELECT name FROM session_rows").unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    };

    assert_eq!(
        names,
        vec![session_name.clone()],
        "the snapshot captured by `osm --socket {socket} snapshot` must contain \
         exactly the session that lives on that private server ({session_name:?}); \
         if --socket were not honoured, osm would have targeted the default \
         server instead and this uniquely-named session would be absent"
    );
}
