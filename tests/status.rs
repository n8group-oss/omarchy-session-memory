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
    // No compositor here — this suite's subject is capture health, not the
    // desktop. See `common::write_headless_config`.
    env.write_config("[restore]\nplace_windows = false\n");
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
    env.write_config("[restore]\nplace_windows = false\n");
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

/// What osm will and will not do automatically, said where a user looks for
/// capabilities.
///
/// OpenCode's automatic capture path was unreachable — it owns no transcript,
/// so binding could score it at most 0.4 against a 0.75 threshold — and
/// nothing anywhere said so, in the output, in the README, or in a test. A
/// capability osm does not have now has to be declared, and this is the
/// declaration a consumer can read.
#[test]
fn status_names_the_agents_osm_will_not_capture_or_resume_by_itself() {
    let env = Env::new("agents");
    env.write_config("[agents]\nenabled = [\"claude\", \"codex\", \"opencode\"]\n");

    let v = env.status();
    let agents = &v["agents"];
    assert_eq!(
        agents["enabled"],
        serde_json::json!(["claude", "codex", "opencode"]),
        "{v}"
    );
    let unsupported = agents["unsupported"].as_array().unwrap();
    assert_eq!(
        unsupported.len(),
        1,
        "exactly the one kind osm cannot act on: {v}"
    );
    assert_eq!(unsupported[0]["kind"], "opencode");
    let reason = unsupported[0]["reason"].as_str().unwrap();
    assert!(
        !reason.is_empty() && reason.contains("osm"),
        "the reason is prose a user can act on: {reason}"
    );

    // And a configuration without it says there is nothing unsupported,
    // rather than listing a kind that is simply absent.
    env.write_config("[agents]\nenabled = [\"claude\"]\n");
    let v = env.status();
    assert_eq!(v["agents"]["enabled"], serde_json::json!(["claude"]), "{v}");
    assert!(
        v["agents"]["unsupported"].as_array().unwrap().is_empty(),
        "{v}"
    );
}

// ---------------------------------------------------------------------------
// The database block, as one answer.
//
// Every field under `database`, plus `snapshot` and `sessions`, is read out of
// the same database at the same instant. They used not to be: the count was an
// autocommit query taken just before the read transaction that produced
// everything else, so a capture committing in that gap put `"snapshots": 0`
// beside a `"snapshot"` object with an id in it.
//
// The interleaving itself is pinned by `ipc::tests::
// an_empty_archive_never_reports_a_snapshot_out_of_it`, which runs the other
// process's commit at the exact instant that matters. What these two check is
// the shape a consumer actually reads.
// ---------------------------------------------------------------------------

/// The count, the newest recorded time, and the snapshot agree with each
/// other and with the sessions listed.
#[test]
fn the_database_block_and_the_snapshot_describe_one_moment() {
    let env = Env::new("dbmoment");
    env.write_config("[restore]\nplace_windows = false\n");
    let server = Server(env.tmux());
    server
        .0
        .run(&["new-session", "-d", "-s", "dev", "-c", "/tmp"])
        .unwrap();

    for _ in 0..3 {
        let out = env
            .osm()
            .args(["snapshot", "--reason", "test"])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let v = env.status();
    let db = &v["database"];
    assert_eq!(db["reachable"], true, "{v}");
    let snapshots = db["snapshots"].as_i64().expect("a snapshot count");
    assert_eq!(snapshots, 3, "three captures, three snapshots: {v}");

    let snap = &v["snapshot"];
    assert!(snap.is_object(), "a snapshot was recorded: {v}");
    assert_eq!(
        db["newest_snapshot_at"], snap["taken_at"],
        "the newest recorded time belongs to the snapshot reported: {v}"
    );
    assert_eq!(
        snap["sessions"].as_u64().unwrap() as usize,
        v["sessions"].as_array().unwrap().len(),
        "the snapshot's session count and the session list disagree: {v}"
    );
    assert!(
        snapshots > 0,
        "an archive of {snapshots} snapshots holding snapshot #{}: {v}",
        snap["id"]
    );
}

/// A count that could not be read is `null`, not `0`.
///
/// Reading it used to be `unwrap_or((0, None))`, so a failed query reported an
/// empty archive — a widget shows "nothing recorded" over a database full of
/// snapshots, which is the same lie as every other one this plugin is built
/// not to tell. The database here opens (its `schema_version` is this build's)
/// and cannot be read, which is the only way to reach that branch.
#[test]
fn a_database_that_opens_and_cannot_be_read_reports_null_rather_than_zero() {
    let env = Env::new("dbunreadable");
    env.write_config("[restore]\nplace_windows = false\n");
    let server = Server(env.tmux());
    server
        .0
        .run(&["new-session", "-d", "-s", "dev", "-c", "/tmp"])
        .unwrap();
    let out = env
        .osm()
        .args(["snapshot", "--reason", "test"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The schema version stays this build's, so `open` migrates nothing and
    // preserves nothing — and its `CREATE TABLE IF NOT EXISTS` batch would put
    // a dropped table straight back. A renamed *column* is the damage it does
    // not repair, and it is what a summary query trips over.
    {
        let conn = rusqlite::Connection::open(env.state_dir().join("state.db")).unwrap();
        conn.execute(
            "ALTER TABLE terminal_windows RENAME COLUMN workspace_ref TO gone",
            [],
        )
        .unwrap();
    }

    let v = env.status();
    let db = &v["database"];
    assert!(
        db["snapshots"].is_null(),
        "a count that could not be read is unknown, not zero: {v}"
    );
    assert!(db["newest_snapshot_at"].is_null(), "{v}");
    assert!(v["snapshot"].is_null(), "{v}");
    assert_eq!(v["sessions"].as_array().unwrap().len(), 0, "{v}");
    assert!(
        v["message"]
            .as_str()
            .unwrap_or_default()
            .contains("database"),
        "the user has to be told the database could not be read: {v}"
    );
}

// ---------------------------------------------------------------------------
// What is *in* the preserved database.
//
// The notice used to end "its snapshots are intact but osm no longer reads
// them" without ever opening the file. On a machine whose preserved database
// held nothing — a backup taken from an already-empty state directory — the
// panel therefore told its owner, every five seconds, that snapshots he had
// never lost were sitting somewhere unreadable. Asserting a fact nobody
// checked is the same class of defect as rendering unknown as no.
//
// So the file is opened, read-only, and counted. Three outcomes, and all
// three read differently: it holds snapshots, it holds none, or it could not
// be read at all and what it holds is unknown. A fourth is the ordinary end
// of the story — the user took up the offer and deleted it — and must not be
// reported as a file osm cannot read.
// ---------------------------------------------------------------------------

/// A database from an older schema, holding `snapshots` snapshot rows.
/// Opening it makes this build preserve it beside itself.
///
/// WAL, like every database this engine writes: it is the journal mode that
/// decides whether merely *reading* the preserved file creates files beside
/// it, which one of the tests below is about.
fn write_legacy_db(path: &std::path::Path, snapshots: usize) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.pragma_update(None, "journal_mode", "WAL").unwrap();
    conn.execute_batch(
        "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
         CREATE TABLE snapshots (
           id INTEGER PRIMARY KEY, taken_at INTEGER NOT NULL,
           boot_id TEXT NOT NULL, reason TEXT NOT NULL, state TEXT NOT NULL);
         INSERT INTO meta (key, value) VALUES ('schema_version', '1');",
    )
    .unwrap();
    for i in 1..=snapshots {
        conn.execute(
            "INSERT INTO snapshots (id, taken_at, boot_id, reason, state)
             VALUES (?1, ?2, 'boot-a', 'manual', 'complete')",
            rusqlite::params![i as i64, 100 + i as i64],
        )
        .unwrap();
    }
}

/// The preserved file that does hold rows: counted, and reported as a count.
///
/// This test used to require the word "intact", which nothing had earned —
/// see `a_preserved_database_is_never_called_intact_on_the_strength_of_a_row_count`
/// below, and `preserved_notice` in `src/main.rs`. What is established here is
/// that three rows are in the `snapshots` table of a file osm opened, and that
/// is what the notice must say.
#[test]
fn a_preserved_database_reports_the_snapshots_it_actually_holds() {
    let env = Env::new("preservedfull");
    write_legacy_db(&env.state_dir().join("state.db"), 3);

    let v = env.status();
    let preserved = &v["database"]["preserved"];
    assert_eq!(
        preserved["snapshots"], 3,
        "the count in the preserved file must be reported: {v}"
    );
    assert_eq!(preserved["present"], true, "{v}");
    assert!(preserved["error"].is_null(), "{v}");
    let msg = v["message"].as_str().unwrap_or_default().to_string();
    assert!(
        msg.contains("contains 3 snapshot records"),
        "the notice must say how many snapshot records are in it: {msg}"
    );
    assert!(
        !msg.contains("intact"),
        "the file was counted, not checked; a row count does not establish \
         that anything in it is intact: {msg}"
    );

    // Counting it must not write into the user's state directory. A plain
    // read-only connection to a WAL database has SQLite build the
    // shared-memory index it reads the WAL through, which means creating
    // `state.db.v1.bak-shm` beside a backup osm's business with is over — on
    // a status the panel runs every five seconds.
    let path = std::path::PathBuf::from(preserved["path"].as_str().expect("a path"));
    let shm = path.with_file_name(format!(
        "{}-shm",
        path.file_name().unwrap().to_string_lossy()
    ));
    assert!(
        !shm.exists(),
        "reading the preserved database created {} beside it",
        shm.display()
    );
}

/// The preserved file that holds nothing: not "your snapshots are intact",
/// and an offer to delete it that osm does not act on itself.
#[test]
fn a_preserved_database_holding_nothing_is_never_called_intact() {
    let env = Env::new("preservedempty");
    write_legacy_db(&env.state_dir().join("state.db"), 0);

    let v = env.status();
    let preserved = &v["database"]["preserved"];
    assert_eq!(
        preserved["snapshots"], 0,
        "an empty preserved file holds zero snapshots, and that is a fact: {v}"
    );
    assert_eq!(preserved["present"], true, "{v}");
    let msg = v["message"].as_str().unwrap_or_default().to_string();
    assert!(
        msg.contains("holds no snapshots"),
        "the notice must say the file holds nothing: {msg}"
    );
    assert!(
        !msg.contains("intact"),
        "a file with nothing in it must not be reported as holding intact \
         snapshots: {msg}"
    );
    assert!(
        msg.contains("delete"),
        "a file holding nothing can simply be removed, and the notice must \
         say so: {msg}"
    );

    // The offer is the user's to take. osm names the file; it does not
    // remove it.
    let path = preserved["path"].as_str().expect("a path");
    assert!(
        std::path::Path::new(path).exists(),
        "osm deleted the user's preserved database at {path}"
    );
}

/// The preserved file nothing could read: neither "intact" nor "empty".
#[test]
fn a_preserved_database_that_cannot_be_read_is_counted_neither_way() {
    let env = Env::new("preservedjunk");
    write_legacy_db(&env.state_dir().join("state.db"), 2);

    let first = env.status();
    let path = first["database"]["preserved"]["path"]
        .as_str()
        .expect("a path")
        .to_string();
    // Whatever the file is now, it is not a database this can count.
    std::fs::write(&path, b"this is not a database").unwrap();

    let v = env.status();
    let preserved = &v["database"]["preserved"];
    assert!(
        preserved["snapshots"].is_null(),
        "a preserved file that could not be read holds an unknown number of \
         snapshots, not zero and not some: {v}"
    );
    assert_eq!(preserved["present"], true, "{v}");
    assert!(
        preserved["error"].as_str().is_some(),
        "the reason it could not be read must be reported: {v}"
    );
    let msg = v["message"].as_str().unwrap_or_default().to_string();
    assert!(
        msg.contains("unknown"),
        "the notice must say what is in it is unknown: {msg}"
    );
    assert!(
        !msg.contains("intact") && !msg.contains("holds no snapshots"),
        "an unreadable file must not be reported as either of the two files \
         that could be read: {msg}"
    );
}

/// And once the user takes the offer, the notice must not turn into an alarm
/// about a file that is gone because they removed it.
#[test]
fn a_preserved_database_the_user_removed_is_forgotten_entirely() {
    let env = Env::new("preservedgone");
    write_legacy_db(&env.state_dir().join("state.db"), 0);

    let first = env.status();
    let path = first["database"]["preserved"]["path"]
        .as_str()
        .expect("a path")
        .to_string();
    assert!(
        first["message"]
            .as_str()
            .unwrap_or_default()
            .contains("was preserved at"),
        "while the file is there, the notice names it: {first}"
    );

    // The user does what the zero-snapshot notice invites and deletes it.
    std::fs::remove_file(&path).unwrap();

    let v = env.status();
    assert!(
        v["database"]["preserved"].is_null(),
        "a preservation whose file the user removed is forgotten, not reported \
         with present:false forever: {v}"
    );
    assert!(
        v["message"].is_null()
            || !v["message"]
                .as_str()
                .unwrap_or_default()
                .contains("was preserved at"),
        "there is nothing left to say about a file that is not there, and the \
         panel polls this every five seconds: {v}"
    );

    // And it stays forgotten — the record is gone from the database, not
    // merely filtered out of one response.
    let again = env.status();
    assert!(again["database"]["preserved"].is_null(), "{again}");
}

/// A row in `snapshots` is not a snapshot, and counting rows is not an
/// integrity check.
///
/// The notice used to end "which are intact but osm no longer reads them" for
/// any file whose `COUNT(*) FROM snapshots` came back positive. That count is
/// one query against one table. It says nothing about whether the sessions,
/// windows and panes those snapshots are made of are still there, nothing
/// about whether the file is internally consistent, and nothing about whether
/// the newest row is a `building` one that describes a capture that never
/// finished. A backup with a readable `snapshots` table and nothing behind it
/// answers the query perfectly and holds nothing anyone could restore.
///
/// Telling someone their data is intact on the strength of a row count is the
/// same defect as rendering unknown as no, pointed at the thing they would
/// most want to be sure about. So the notice reports the count as a count.
///
/// The fixture is exactly that file: two rows in `snapshots`, one of them
/// still `building`, and the tables they refer to empty.
fn write_hollow_legacy_db(path: &std::path::Path) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.pragma_update(None, "journal_mode", "WAL").unwrap();
    conn.execute_batch(
        "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
         CREATE TABLE snapshots (
           id INTEGER PRIMARY KEY, taken_at INTEGER NOT NULL,
           boot_id TEXT NOT NULL, reason TEXT NOT NULL, state TEXT NOT NULL);
         CREATE TABLE session_rows (
           row_id INTEGER PRIMARY KEY, snapshot_id INTEGER NOT NULL,
           name TEXT NOT NULL);
         INSERT INTO meta (key, value) VALUES ('schema_version', '1');
         -- One finished snapshot with nothing behind it, and one that was
         -- still being written when the machine went down.
         INSERT INTO snapshots (id, taken_at, boot_id, reason, state)
           VALUES (1, 101, 'boot-a', 'manual', 'complete'),
                  (2, 102, 'boot-a', 'manual', 'building');",
    )
    .unwrap();
}

#[test]
fn a_preserved_database_is_never_called_intact_on_the_strength_of_a_row_count() {
    let env = Env::new("preservedhollow");
    write_hollow_legacy_db(&env.state_dir().join("state.db"));

    let v = env.status();
    let preserved = &v["database"]["preserved"];
    assert_eq!(
        preserved["snapshots"], 2,
        "the rows really are there to be counted: {v}"
    );
    let msg = v["message"].as_str().unwrap_or_default().to_string();
    assert!(
        msg.contains("contains 2 snapshot records"),
        "the notice must report what was actually established — a count of \
         rows in one table: {msg}"
    );
    assert!(
        !msg.contains("intact"),
        "the file has two rows in `snapshots`, one of them a `building` row, \
         and nothing at all behind either of them; nobody opened it far enough \
         to call anything in it intact: {msg}"
    );
}

/// The newest snapshot says what it knows about window placement, and a
/// snapshot that knows nothing is not reported as a failure.
///
/// # Why this is on the snapshot and not in `capture`
///
/// A capture whose placement could not be read now succeeds — it records the
/// tmux topology, which is the thing worth having — so `capture.last_error`
/// and `consecutive_failures` stay clean, correctly: nothing failed. But the
/// snapshot it produced does not know where the user's windows were, and a
/// panel that shows only "captures are fresh" would be telling the user their
/// state is fully recorded when part of it is not.
///
/// So the fact travels with the snapshot it is a fact about. `known`,
/// `unknown` and `disabled` are three different sentences, and `unknown` is
/// the only one of them that is a shortfall.
#[test]
fn the_newest_snapshot_reports_what_it_knows_about_placement() {
    let env = Env::new("placement");
    env.write_config("[restore]\nplace_windows = false\n");
    let t = env.tmux();
    let _server = Server(t.clone());
    t.run(&["new-session", "-d", "-s", "alpha", "-c", "/tmp"])
        .unwrap();

    assert!(env.osm().arg("snapshot").output().unwrap().status.success());

    let v = env.status();
    assert_eq!(
        v["snapshot"]["placement"], "disabled",
        "placement is switched off; the snapshot must say so rather than \
         claim it looked and found nothing: {v}"
    );

    // The same snapshot, as a capture that could not read the compositor
    // would have recorded it.
    let db = env.state_dir().join("state.db");
    {
        let conn = osm::db::open(&db).unwrap();
        conn.execute("UPDATE snapshots SET placement_state = 'unknown'", [])
            .unwrap();
    }

    let v = env.status();
    assert_eq!(
        v["snapshot"]["placement"], "unknown",
        "the newest snapshot does not know where the windows were, and \
         nothing in the report says so: {v}"
    );
    assert_eq!(
        v["capture"]["consecutive_failures"], 0,
        "an unknown placement is not a capture failure: {v}"
    );
    assert!(
        v["capture"]["last_error"].is_null(),
        "and it must not be dressed up as one: {v}"
    );
    assert_eq!(v["ready"], true, "{v}");

    {
        let conn = osm::db::open(&db).unwrap();
        conn.execute("UPDATE snapshots SET placement_state = 'known'", [])
            .unwrap();
    }
    assert_eq!(env.status()["snapshot"]["placement"], "known");
}

// ---------------------------------------------------------------------------
// The wedge, made visible.
//
// The maintainer's panel read "captures are fresh · 40 failing in a row" for
// 83 minutes while the real reason — a UNIQUE violation against rows that
// belonged to snapshots that were gone — was in the journal only, where the
// tmux hooks that hit it the most send their output to /dev/null. The
// database's own consistency was never part of the report at all, so nothing
// a user could look at said what was wrong or that anything had been done
// about it.
// ---------------------------------------------------------------------------

/// A database holding rows whose snapshot is gone is repaired by the open
/// `status` itself does, and the report says so.
///
/// Both fields matter and they answer different questions. `orphan_rows` is
/// the condition *now*: anything but zero means the next capture is one id
/// away from the wedge. `repaired` is the record that it happened, which is
/// the part a user can act on — something removed a snapshot without taking
/// its rows, and that is worth knowing even after osm has cleared up.
#[test]
fn a_database_that_had_to_be_repaired_says_so() {
    let env = Env::new("repaired");
    let path = env.state_dir().join("state.db");
    {
        let conn = osm::db::open(&path).unwrap();
        conn.execute_batch(
            "INSERT INTO snapshots (id, taken_at, boot_id, reason, state)
               VALUES (1, 100, 'boot-a', 'manual', 'complete'),
                      (2, 200, 'boot-a', 'manual', 'complete');
             INSERT INTO session_rows (row_id, snapshot_id, tmux_session_id, name)
               VALUES (10, 1, '$0', 'kept'), (11, 2, '$0', 'doomed');
             INSERT INTO window_rows (row_id, snapshot_id, tmux_window_id, name, layout)
               VALUES (20, 1, '@0', 'w', 'l'), (21, 2, '@0', 'w', 'l');",
        )
        .unwrap();
    }
    {
        let raw = rusqlite::Connection::open(&path).unwrap();
        raw.pragma_update(None, "foreign_keys", "OFF").unwrap();
        raw.execute_batch("DROP TRIGGER IF EXISTS snapshots_cascade_delete")
            .unwrap();
        raw.execute("DELETE FROM snapshots WHERE id = 2", [])
            .unwrap();
    }

    let v = env.status();
    let db = &v["database"];
    assert_eq!(
        db["orphan_rows"], 0,
        "the open behind this report must have cleared them: {v}"
    );
    assert_eq!(
        db["repaired"]["rows"], 2,
        "and must say how many it cleared: {v}"
    );
    assert!(
        db["repaired"]["at"].as_i64().unwrap_or(0) > 0,
        "and when: {v}"
    );
}

/// A healthy database reports no repair at all — `null`, not a repair of zero
/// rows.
///
/// A widget that shows "the database was repaired" every five seconds on a
/// machine where nothing ever went wrong is the same defect as one that shows
/// nothing when something did.
#[test]
fn a_healthy_database_reports_no_repair() {
    let env = Env::new("norepair");
    let v = env.status();
    let db = &v["database"];
    assert_eq!(db["orphan_rows"], 0, "{v}");
    assert!(
        db["repaired"].is_null(),
        "a database that has never needed a repair must not report one: {v}"
    );
}

/// An engine that calls itself not ready always says why.
///
/// The maintainer's panel once read "The engine reported ready:false and gave
/// no message." — an unhealthy verdict with no reason, which is worse than the
/// notice it replaced: there is nothing to act on and nothing to look up. The
/// two fields are produced from one list, so this cannot happen today; the
/// test exists so a later change that pushes an empty problem, or sets `ready`
/// from something other than that list, is caught here rather than on his bar.
#[test]
fn an_engine_that_is_not_ready_always_says_why() {
    let env = Env::new("readywhy");
    // A database path that cannot be opened: a directory where the file goes.
    std::fs::create_dir_all(env.state_dir().join("state.db")).unwrap();

    let v = env.status();
    assert_eq!(
        v["ready"], false,
        "an unopenable database is not ready: {v}"
    );
    let msg = v["message"].as_str().unwrap_or_default();
    assert!(
        !msg.trim().is_empty(),
        "ready:false must carry a reason the user can act on: {v}"
    );
}
