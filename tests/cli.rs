//! Command-surface checks that need no tmux state of their own.
//!
//! These once ran `osm` bare: no `--socket`, no `XDG_STATE_HOME`. Compiled
//! with the `default-server` feature that resolves to the developer's own
//! tmux server and their real `~/.local/state/osm`, so running the suite
//! read — and could have written — live state. `Env` is the only way this
//! project invokes the binary: private state directory, private socket.

mod common;

#[test]
fn status_json_reports_protocol_version() {
    let env = common::Env::new("cli-proto");
    let out = env.osm(&["status", "--json"]);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("stdout is valid JSON");
    assert_eq!(v["protocol_version"], 1);
    assert!(v["engine_version"].is_string());
}
