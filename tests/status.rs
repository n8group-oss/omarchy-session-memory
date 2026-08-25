//! `osm status --json` has to be able to say "captures are broken".
//!
//! It used to report `ready: true` whenever the config merely *parsed*. The
//! tmux hooks send all output to `/dev/null` and the daemon discarded every
//! capture result, so a capture that failed on every event left the service
//! looking perfectly healthy while the newest snapshot aged out of retention.
//! That is precisely how the linked-window bug — which broke every capture on
//! a server with any linked window — went unnoticed.

mod common;

use osm::tmux::Tmux;
use serde_json::Value;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

struct Env {
    dir: tempfile::TempDir,
    socket: String,
}

impl Env {
    fn new(label: &str) -> Self {
        Env {
            dir: tempfile::tempdir().unwrap(),
            socket: format!("osm-status-{}-{}", label, std::process::id()),
        }
    }

    fn osm(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_osm"));
        cmd.env("XDG_STATE_HOME", self.dir.path().join("state"))
            .env("XDG_CONFIG_HOME", self.dir.path().join("config"))
            .arg("--socket")
            .arg(&self.socket);
        cmd
    }

    fn write_config(&self, body: &str) {
        let dir = self.dir.path().join("config/osm");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.toml"), body).unwrap();
    }

    fn state_dir(&self) -> PathBuf {
        self.dir.path().join("state/osm")
    }

    fn tmux(&self) -> Tmux {
        Tmux::with_socket(&self.socket)
    }

    fn status(&self) -> Value {
        let out = self.osm().args(["status", "--json"]).output().unwrap();
        assert!(
            out.status.success(),
            "osm status failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let text = String::from_utf8_lossy(&out.stdout);
        // One object, still: a widget parses this whole, and multiple
        // documents or a stray log line would break it.
        assert_eq!(
            text.lines().filter(|l| !l.trim().is_empty()).count(),
            1,
            "status must print exactly one JSON object, got: {text}"
        );
        let v: Value = serde_json::from_str(&text).expect("status output is valid JSON");
        assert!(v.is_object());
        assert_eq!(v["protocol_version"], 1, "protocol_version must stay 1");
        assert!(v["engine_version"].is_string());
        v
    }
}

struct Server(Tmux);

impl Drop for Server {
    fn drop(&mut self) {
        common::shutdown(&self.0);
    }
}

#[test]
fn status_reports_capture_freshness_after_a_successful_capture() {
    let env = Env::new("fresh");
    let t = env.tmux();
    let _server = Server(t.clone());
    t.run(&["new-session", "-d", "-s", "alpha", "-c", "/tmp"])
        .unwrap();

    let snapshot = env.osm().arg("snapshot").output().unwrap();
    assert!(snapshot.status.success());

    let v = env.status();
    assert_eq!(v["ready"], true);
    assert!(
        v["capture"]["last_success_at"].is_i64(),
        "a successful capture must be recorded: {v}"
    );
    assert_eq!(
        v["capture"]["stale"], false,
        "a capture that just happened is not stale: {v}"
    );
    assert_eq!(v["capture"]["consecutive_failures"], 0);
    assert!(v["capture"]["last_error"].is_null());
    assert!(v["capture"]["stale_after_secs"].is_u64());

    assert_eq!(v["database"]["reachable"], true);
    assert_eq!(v["database"]["snapshots"], 1);
    assert!(v["database"]["newest_snapshot_at"].is_i64());

    assert_eq!(
        v["tmux"]["reachable"], true,
        "the server this osm was pointed at is running: {v}"
    );
    assert_eq!(v["tmux"]["socket"], env.socket.as_str());
}

/// Nothing has ever been captured: the engine can run, but it has nothing to
/// restore from. `stale` has to say so.
#[test]
fn a_never_captured_engine_reports_stale_and_an_unreachable_tmux() {
    let env = Env::new("never");
    let v = env.status();

    assert!(v["capture"]["last_success_at"].is_null());
    assert!(v["capture"]["age_secs"].is_null());
    assert_eq!(
        v["capture"]["stale"], true,
        "an engine that has never captured has nothing to restore from: {v}"
    );
    assert_eq!(
        v["tmux"]["reachable"], false,
        "no server has been started on this socket: {v}"
    );
}

/// The case the old `status` could not express: captures are failing, right
/// now, repeatedly.
#[test]
fn status_surfaces_a_persistent_capture_failure() {
    let env = Env::new("broken");
    // No tmux server on this socket at all, so every capture fails.
    for _ in 0..2 {
        let out = env.osm().arg("snapshot").output().unwrap();
        assert!(
            !out.status.success(),
            "a capture against a dead server must fail loudly"
        );
    }

    let v = env.status();
    assert_eq!(
        v["capture"]["consecutive_failures"], 2,
        "repeated failures must be counted: {v}"
    );
    assert!(
        v["capture"]["last_error"]
            .as_str()
            .is_some_and(|e| e.contains("tmux")),
        "the failure must be reported with its cause: {v}"
    );
    assert!(v["capture"]["last_error_at"].is_i64());
    assert_eq!(v["capture"]["stale"], true);
    assert_eq!(v["tmux"]["reachable"], false);
}

/// A capture failure must not be reported forever once captures work again.
#[test]
fn a_successful_capture_clears_the_failure_streak() {
    let env = Env::new("recover");
    let out = env.osm().arg("snapshot").output().unwrap();
    assert!(!out.status.success());
    assert_eq!(env.status()["capture"]["consecutive_failures"], 1);

    let t = env.tmux();
    let _server = Server(t.clone());
    t.run(&["new-session", "-d", "-s", "alpha", "-c", "/tmp"])
        .unwrap();
    assert!(env.osm().arg("snapshot").output().unwrap().status.success());

    let v = env.status();
    assert_eq!(v["capture"]["consecutive_failures"], 0, "{v}");
    assert_eq!(v["capture"]["stale"], false, "{v}");
}

/// A broken config is still reported, and now says so in `ready` alongside
/// everything else rather than being the only thing `status` knew about.
#[test]
fn an_invalid_config_makes_the_engine_not_ready() {
    let env = Env::new("badcfg");
    env.write_config("[capture]\nkeep_snapshots = 0\n");
    let v = env.status();
    assert_eq!(v["ready"], false);
    assert!(v["message"]
        .as_str()
        .is_some_and(|m| m.contains("keep_snapshots")));
    // The rest of the report is still there: a widget must not lose capture
    // freshness because the config has a typo.
    assert!(v["capture"].is_object());
    assert!(v["database"].is_object());
    assert!(v["tmux"].is_object());
}

/// Repeated capture failures must reach systemd, which reads neither stdout
/// nor the database — only the exit status and the journal.
#[test]
fn the_daemon_exits_non_zero_after_repeated_capture_failures() {
    let env = Env::new("daemon");
    // One-second ticks so the failure streak builds quickly; there is no tmux
    // server on this socket, so every tick fails.
    env.write_config("[capture]\nfallback_interval_secs = 1\n");
    std::fs::create_dir_all(env.state_dir()).unwrap();

    // Killed on drop, so a daemon that never gives up cannot outlive the test
    // and keep capturing in the background.
    struct Child(std::process::Child);
    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut child = Child(
        env.osm()
            .arg("daemon")
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap(),
    );

    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(s) = child.0.try_wait().unwrap() {
            break s;
        }
        assert!(
            Instant::now() < deadline,
            "the daemon kept looping silently through failing captures"
        );
        std::thread::sleep(Duration::from_millis(100));
    };

    assert_eq!(
        status.code(),
        Some(1),
        "a daemon whose captures keep failing must not look healthy to systemd"
    );

    let v = env.status();
    assert!(
        v["capture"]["consecutive_failures"].as_u64().unwrap() >= 3,
        "{v}"
    );
}

/// A preserved database must be *visible*. `osm::db::open` runs on every
/// subcommand, so the person who upgraded and typed `osm status` is exactly
/// the one who needs to be told their old snapshots moved.
#[test]
fn status_reports_a_database_preserved_by_a_schema_change() {
    let env = Env::new("preserved");
    let db = env.state_dir().join("state.db");
    std::fs::create_dir_all(env.state_dir()).unwrap();
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE snapshots (
               id INTEGER PRIMARY KEY, taken_at INTEGER NOT NULL,
               boot_id TEXT NOT NULL, reason TEXT NOT NULL, state TEXT NOT NULL);
             INSERT INTO snapshots (id, taken_at, boot_id, reason, state)
               VALUES (1, 100, 'boot-a', 'manual', 'complete');
             INSERT INTO meta (key, value) VALUES ('schema_version', '1');",
        )
        .unwrap();
    }

    let v = env.status();
    let preserved = &v["database"]["preserved"];
    assert!(
        preserved.is_object(),
        "the preserved database must be reported: {v}"
    );
    assert_eq!(preserved["schema_version"], 1);
    let path = preserved["path"].as_str().expect("a path");
    assert!(
        std::path::Path::new(path).exists(),
        "the preserved database must still be on disk at {path}"
    );
    assert!(
        v["message"].as_str().unwrap_or_default().contains(path),
        "the message must name the preserved database: {v}"
    );
    // The engine still runs: a preserved database is a permanent fact about
    // the state directory, not a reason to report the service as broken
    // forever.
    assert_eq!(v["ready"], true, "{v}");
}
