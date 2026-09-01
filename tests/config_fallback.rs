//! A broken config must never cost the user snapshots.
//!
//! The capture path used to read `capture.keep_snapshots` with
//! `.unwrap_or(20)`, so any config error at all — including a typo in an
//! unrelated key, which `status --json` simultaneously reports as invalid —
//! silently reinstated the built-in retention. A user who had set
//! `keep_snapshots = 100` lost eighty snapshots to a number they never chose.
//!
//! Capture must still happen (skipping it would lose state too); only the
//! deletion is withheld.
//!
//! Every `osm` invocation here passes `--socket <unique-name>` (derived from
//! the test label and this process's id, so parallel test binaries cannot
//! collide), and the private tmux server used to exercise capture is
//! started through [`osm::tmux::Tmux::with_socket`], which passes that same
//! name to tmux via `-L` — the isolation mechanism that actually works on
//! every tmux version, unlike some environment variables. See the CI guard
//! "No test may target the default tmux server".

mod common;

use osm::tmux::Tmux;
use std::process::Command;

fn socket_name(label: &str) -> String {
    format!("osm-cfgfallback-{}-{}", label, std::process::id())
}

fn env_cmd(root: &std::path::Path, socket: &str, args: &[&str]) -> std::process::Output {
    // A stub compositor on the `PATH`, because the config under test is
    // *invalid*: nothing in it can turn placement off, so the capture runs
    // with the built-in default (placement on) and is held to reading a
    // placement. Which is the point — a broken config must not be taken as
    // consent to stop recording where the user's windows are. The stub
    // answers as an empty desktop and cannot dispatch.
    let bin = common::stub_hyprctl(root);
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    Command::new(env!("CARGO_BIN_EXE_osm"))
        .env("PATH", path)
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .arg("--socket")
        .arg(socket)
        .args(args)
        .output()
        .expect("run osm")
}

struct Server(Tmux);

impl Drop for Server {
    fn drop(&mut self) {
        common::shutdown(&self.0);
    }
}

#[test]
fn an_invalid_config_captures_but_prunes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("config/osm")).unwrap();
    let socket = socket_name("prune");
    let t = Tmux::with_socket(&socket);
    let _server = Server(t.clone());

    // A typo in a key that has nothing to do with retention.
    std::fs::write(
        root.join("config/osm/config.toml"),
        "[capture]\nkeep_snapshots = 100\nfallback_intervall_secs = 120\n",
    )
    .unwrap();

    t.run(&["new-session", "-d", "-s", "alpha", "-c", "/tmp"])
        .expect("start private tmux server");

    let db = root.join("state/osm/state.db");
    // Pre-load more snapshots than the built-in default retention would keep.
    {
        let conn = osm::db::open(&db).unwrap();
        for i in 1..=30 {
            conn.execute(
                "INSERT INTO snapshots (taken_at, boot_id, reason, state)
                 VALUES (?1, 'boot-previous', 'test', 'complete')",
                [i],
            )
            .unwrap();
        }
    }

    let out = env_cmd(root, &socket, &["snapshot"]);
    assert!(
        out.status.success(),
        "capture must still happen with a broken config: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let conn = osm::db::open(&db).unwrap();
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM snapshots", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        count, 31,
        "an invalid config must not fall back to the default retention and prune"
    );
}

#[test]
fn status_reports_the_underlying_toml_error_not_just_the_file_name() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("config/osm")).unwrap();
    std::fs::write(
        root.join("config/osm/config.toml"),
        "[restore\nauto = true\n",
    )
    .unwrap();

    let socket = socket_name("statuserr");
    let out = env_cmd(root, &socket, &["status", "--json"]);
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["ready"], false);

    let message = v["message"]
        .as_str()
        .expect("an unready status explains why");
    assert!(
        message.contains("config.toml"),
        "message should name the file: {message}"
    );
    assert!(
        message.contains("TOML parse error") || message.contains("expected"),
        "message must carry the real parse error, not only the outermost \
         context line; got {message:?}"
    );
}
