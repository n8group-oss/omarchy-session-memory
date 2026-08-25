//! The `osm restore` JSON contract, asserted against the real binary on every
//! exit path.
//!
//! `src/ipc.rs` exists so Plan 4's QML widget can parse this output. Before
//! this suite the command emitted four different shapes — a consumer doing
//! `.created[]` broke outright on the lock path, and three unrelated causes
//! all reported `"state":"skipped"`.
//!
//! Every test drives the installed binary with `XDG_STATE_HOME` and
//! `XDG_CONFIG_HOME` redirected into a temporary directory, and with
//! `--socket <unique-name>` (derived from the test label and this process's
//! id, so parallel test binaries cannot collide) so nothing here can ever
//! reach the developer's real tmux server. Any private tmux server this
//! suite starts directly goes through [`osm::tmux::Tmux::with_socket`],
//! which passes that same name to tmux via `-L` — the isolation mechanism
//! that actually works on every tmux version, unlike some environment
//! variables. See the CI guard "No test may target the default tmux
//! server".

mod common;

use osm::tmux::Tmux;
use serde_json::Value;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

/// Keys a consumer may read unconditionally, on any exit path.
const CONTRACT_KEYS: &[&str] = &[
    "state",
    "reason",
    "snapshot_id",
    "attempt_id",
    "created",
    "adopted",
    "skipped",
    "failed",
    "conflicts",
    "degraded",
    "skipped_layouts",
    "retryable",
];

struct Env {
    dir: tempfile::TempDir,
    socket: String,
}

impl Env {
    /// `label` plus this process's id must make the tmux socket name unique
    /// across every test in every test binary running concurrently.
    fn new(label: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        Env {
            dir,
            socket: format!("osm-restorejson-{}-{}", label, std::process::id()),
        }
    }

    fn state_dir(&self) -> PathBuf {
        self.dir.path().join("state/osm")
    }

    fn db(&self) -> PathBuf {
        self.state_dir().join("state.db")
    }

    fn lock(&self) -> PathBuf {
        self.state_dir().join("restore.lock")
    }

    fn write_config(&self, body: &str) {
        let dir = self.dir.path().join("config/osm");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.toml"), body).unwrap();
    }

    fn osm(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_osm"));
        cmd.env("XDG_STATE_HOME", self.dir.path().join("state"))
            .env("XDG_CONFIG_HOME", self.dir.path().join("config"))
            .arg("--socket")
            .arg(&self.socket);
        cmd
    }

    fn tmux(&self) -> Tmux {
        Tmux::with_socket(&self.socket)
    }

    /// Runs `osm restore`, checks the JSON contract *and* that the process
    /// exit status matches the reported state.
    fn restore(&self, args: &[&str]) -> Value {
        let out = self.osm().arg("restore").args(args).output().unwrap();
        let v: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
            panic!(
                "restore output is not JSON ({e}): {}",
                String::from_utf8_lossy(&out.stdout)
            )
        });
        assert_contract(&v);
        assert_exit_status(&out.status, &v);
        v
    }
}

/// `osm restore` used to exit 0 on every path, so `osm-restore.service`
/// reported a clean success whether or not the user's sessions came back.
/// The status now follows the reported state, and the JSON is still written
/// in full either way.
fn assert_exit_status(status: &std::process::ExitStatus, v: &Value) {
    let state = v["state"].as_str().unwrap();
    let want = osm::ipc::exit_code_for_state(state);
    assert_eq!(
        status.code(),
        Some(want),
        "state {state:?} must exit with status {want}, got {status:?}"
    );
}

fn assert_contract(v: &Value) {
    let obj = v
        .as_object()
        .unwrap_or_else(|| panic!("restore output must be a JSON object, got {v}"));
    for key in CONTRACT_KEYS {
        assert!(
            obj.contains_key(*key),
            "restore output is missing the contract key `{key}`: {v}"
        );
    }
    for key in [
        "created",
        "adopted",
        "skipped",
        "failed",
        "conflicts",
        "degraded",
        "skipped_layouts",
    ] {
        assert!(
            obj[key].is_array(),
            "`{key}` must always be an array so a consumer can index it: {v}"
        );
    }
    for key in ["state", "reason"] {
        let token = obj[key]
            .as_str()
            .unwrap_or_else(|| panic!("`{key}` must be a string: {v}"));
        assert!(
            !token.is_empty()
                && token
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
            "`{key}` must be a stable snake_case token, not prose; got {token:?}"
        );
    }
}

/// Inserts a `complete` snapshot attributed to an earlier boot, so a restore
/// has something to select without any tmux involvement.
fn seed_previous_boot_snapshot(env: &Env) -> i64 {
    let conn = osm::db::open(&env.db()).unwrap();
    conn.execute(
        "INSERT INTO snapshots (taken_at, boot_id, reason, state)
         VALUES (1, 'boot-previous', 'test', 'complete')",
        [],
    )
    .unwrap();
    conn.last_insert_rowid()
}

#[test]
fn nothing_to_restore_emits_the_full_contract() {
    let env = Env::new("nothing");
    let v = env.restore(&[]);
    assert_eq!(v["state"], "nothing_to_restore");
    assert_eq!(v["reason"], "no_previous_boot_snapshot");
    assert!(v["snapshot_id"].is_null());
    assert!(v["attempt_id"].is_null());
}

#[test]
fn dry_run_emits_the_full_contract_and_its_own_state() {
    let env = Env::new("dryrun");
    let snap = seed_previous_boot_snapshot(&env);

    let v = env.restore(&["--dry-run"]);
    assert_eq!(
        v["state"], "dry_run",
        "a dry run is not the same event as having nothing to restore"
    );
    assert_eq!(v["reason"], "dry_run");
    assert_eq!(v["snapshot_id"], snap);
}

#[test]
fn a_lock_held_past_the_deadline_emits_the_full_contract() {
    let env = Env::new("lockheld");
    env.write_config("[restore]\nreadiness_timeout_secs = 1\n");
    std::fs::create_dir_all(env.state_dir()).unwrap();
    let _held = osm::lock::SingleInstance::acquire(&env.lock())
        .unwrap()
        .expect("test holds the lock");

    let v = env.restore(&[]);
    assert_eq!(v["state"], "blocked");
    assert_eq!(
        v["reason"], "restore_lock_unavailable",
        "the holder may be a capture, so the reason must not claim a restore is running"
    );
}

#[test]
fn a_capture_holding_the_lock_delays_the_boot_restore_but_does_not_cancel_it() {
    let env = Env::new("delay");
    // Ten seconds of budget: the lock is released well inside it.
    env.write_config("[restore]\nreadiness_timeout_secs = 10\n");
    std::fs::create_dir_all(env.state_dir()).unwrap();
    let held = osm::lock::SingleInstance::acquire(&env.lock())
        .unwrap()
        .expect("stand in for a capture that grabbed the shared lock");

    let started = Instant::now();
    let child = env
        .osm()
        .arg("restore")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(700));
    drop(held);

    let out = child.wait_with_output().unwrap();
    let elapsed = started.elapsed();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_contract(&v);
    assert_exit_status(&out.status, &v);

    assert_eq!(
        v["state"], "nothing_to_restore",
        "the boot restore must wait for a short capture, not abandon the boot"
    );
    assert!(
        elapsed >= Duration::from_millis(600),
        "the restore returned after {elapsed:?}; it gave up instead of waiting"
    );
}

struct Server(Tmux);

impl Drop for Server {
    fn drop(&mut self) {
        common::shutdown(&self.0);
    }
}

#[test]
fn a_successful_restore_emits_the_full_contract() {
    let env = Env::new("success");
    let t = env.tmux();
    let _server = Server(t.clone());

    t.run(&["new-session", "-d", "-s", "alpha", "-c", "/tmp"])
        .expect("start tmux");

    let snapshot = env.osm().arg("snapshot").output().unwrap();
    assert!(
        snapshot.status.success(),
        "osm snapshot failed: {}",
        String::from_utf8_lossy(&snapshot.stderr)
    );

    {
        let conn = osm::db::open(&env.db()).unwrap();
        conn.execute("UPDATE snapshots SET boot_id='boot-previous'", [])
            .unwrap();
    }
    t.run(&["kill-server"]).expect("kill tmux");

    let v = env.restore(&[]);
    assert_eq!(v["state"], "succeeded");
    assert_eq!(v["reason"], "ok");
    assert!(v["snapshot_id"].is_i64());
    assert!(v["attempt_id"].is_i64());
    assert_eq!(v["created"], serde_json::json!(["alpha"]));
}

/// A restore that only partly delivered must say so in its exit status, not
/// only in its JSON: systemd reads neither stdout nor the database.
#[test]
fn a_partial_restore_emits_the_full_contract_and_exits_non_zero() {
    let env = Env::new("partial");
    let t = env.tmux();
    let _server = Server(t.clone());

    t.run(&["new-session", "-d", "-s", "alpha", "-c", "/tmp"])
        .expect("start tmux");
    t.run(&["new-session", "-d", "-s", "beta", "-c", "/tmp"])
        .expect("second session");

    let snapshot = env.osm().arg("snapshot").output().unwrap();
    assert!(
        snapshot.status.success(),
        "osm snapshot failed: {}",
        String::from_utf8_lossy(&snapshot.stderr)
    );

    {
        let conn = osm::db::open(&env.db()).unwrap();
        conn.execute("UPDATE snapshots SET boot_id='boot-previous'", [])
            .unwrap();
        // beta's captured window is given more panes than its captured 80x24
        // layout can hold, so its restore fails after tmux has already created
        // the session: one session up, one not.
        //
        // Not by corrupting the layout string: on tmux 3.3a a malformed layout
        // aborts the whole tmux server rather than failing one command, which
        // destroys the session the assertions below are about.
        let window_row: i64 = conn
            .query_row(
                "SELECT l.window_row_id FROM session_window_links l
                 JOIN session_rows s ON s.row_id = l.session_row_id
                 WHERE s.name = 'beta'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        for idx in 1..9 {
            conn.execute(
                "INSERT INTO pane_rows (window_row_id, tmux_pane_id, idx, cwd, restore_policy)
                 VALUES (?1, ?2, ?3, '/tmp', 'shell')",
                rusqlite::params![window_row, format!("%90{idx}"), idx],
            )
            .unwrap();
        }
    }
    t.run(&["kill-server"]).expect("kill tmux");

    // `Env::restore` asserts the exit status matches the state.
    let v = env.restore(&[]);
    assert_eq!(v["state"], "partial");
    assert_eq!(v["reason"], "partial_restore");
    assert_eq!(v["created"], serde_json::json!(["alpha"]));
    assert_eq!(
        v["retryable"], true,
        "a partial restore must leave the snapshot for the next run"
    );
    assert!(!v["failed"].as_array().unwrap().is_empty());
}
