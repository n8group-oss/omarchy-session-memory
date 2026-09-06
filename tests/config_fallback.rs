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
        // Fixture stores, always. The config under test does not load, so the
        // *default* adapter list is what runs — claude, codex and opencode —
        // and without these two the binary would walk the developer's own
        // 14 GB of real conversations.
        .env("OSM_CLAUDE_HOME", root.join("claude"))
        .env("OSM_CODEX_HOME", root.join("codex"))
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

// ---------------------------------------------------------------------------
// A config that fails to load must not reverse the user's explicit "no".
//
// `privacy.prompt_titles = false` is the one switch osm offers over the one
// thing it reads out of a conversation's contents. Put an unrelated typo in
// the same file — a misspelled key three sections away — and the whole file
// fails to load, and both the capture path (`config_for_capture`) and the
// agent subcommands (`agent_config`) fell back to `Config::default()`, whose
// `prompt_titles` is `true`. The user's explicit refusal was silently reversed
// by a mistake that had nothing to do with it, and their first prompt was read
// and written into SQLite.
//
// Which way a fallback fails is the whole question. Falling back to the
// default *adapter list* is right — a typo must not quietly stop tracking
// every pane. Falling back to the default *privacy* is not: nobody can be
// asked to consent by accident.
// ---------------------------------------------------------------------------

/// A Claude transcript the agent never named, whose only possible title is the
/// user's opening line. Returns the fixture home.
fn untitled_conversation(root: &std::path::Path, id: &str, prompt: &str) -> std::path::PathBuf {
    let home = root.join("claude");
    let path = home.join("projects/-tmp").join(format!("{id}.jsonl"));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        &path,
        format!(
            "{{\"cwd\":\"/tmp\",\"sessionId\":\"{id}\"}}\n\
             {{\"type\":\"user\",\"sessionId\":\"{id}\",\"message\":{{\"role\":\"user\",\
               \"content\":\"{prompt}\"}}}}\n"
        ),
    )
    .unwrap();
    home
}

#[test]
fn an_invalid_config_still_obeys_a_refusal_to_read_prompts() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("config/osm")).unwrap();

    const ID: &str = "0cfebf91-81c0-43d5-af63-c9fe7e844d20";
    const PROMPT: &str = "Rotate the production database credentials before Friday";
    let home = untitled_conversation(root, ID, PROMPT);

    // The user said no, and made a typo somewhere else entirely. `[capture]`
    // has nothing to do with `[privacy]`, and `deny_unknown_fields` rejects
    // the whole file over it.
    std::fs::write(
        root.join("config/osm/config.toml"),
        "[privacy]\nprompt_titles = false\n\
         [capture]\nfallback_intervall_secs = 120\n",
    )
    .unwrap();

    let socket = socket_name("privacyfallback");
    let t = Tmux::with_socket(&socket);
    let _server = Server(t.clone());
    t.run(&["new-session", "-d", "-s", "alpha", "-c", "/tmp"])
        .expect("start private tmux server");

    // The config really is the broken one, and osm really is saying so: without
    // this the rest could pass because the file happened to load.
    let out = env_cmd(root, &socket, &["status", "--json"]);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        v["ready"], false,
        "the config under test must be invalid: {v}"
    );

    // A pane holding that conversation open, so a capture binds it and would
    // derive a title for it.
    let bin = common::stub_agent(root);
    let pane = t.list_panes().unwrap()[0].id.clone();
    t.run(&[
        "send-keys",
        "-t",
        &pane,
        &format!(
            "OSM_CLAUDE_HOME={} {}/claude --resume {ID}",
            home.display(),
            bin.display()
        ),
        "C-m",
    ])
    .unwrap();
    assert_eq!(
        common::wait_for_pane_cmd(&t, &pane, "claude", std::time::Duration::from_secs(10)),
        "claude",
        "the stub agent never started"
    );

    // `osm agents` reads the stores directly, and is the shortest path from a
    // transcript to a printed title.
    let out = env_cmd(root, &socket, &["agents", "--json"]);
    let listed = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        !listed.contains(PROMPT),
        "`osm agents` read the user's first prompt out of a transcript after \
         they had switched prompt titles off; an unrelated typo in the config \
         reversed their answer: {listed}"
    );

    let out = env_cmd(root, &socket, &["snapshot"]);
    assert!(
        out.status.success(),
        "capture must still happen with a broken config: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // And nothing of it was written down. The database is the half that
    // outlives the mistake: a title read once and stored is there until the
    // file is deleted.
    let conn = osm::db::open(&root.join("state/osm/state.db")).unwrap();
    let stored: Vec<(Option<String>, Option<String>)> = conn
        .prepare("SELECT title, title_source FROM agent_sessions")
        .unwrap()
        .query_map([], |r| {
            Ok((
                r.get::<_, Option<String>>(0)?,
                r.get::<_, Option<String>>(1)?,
            ))
        })
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert!(
        !stored.is_empty(),
        "the capture bound no conversation at all, so this test proves nothing"
    );
    assert_eq!(
        stored,
        vec![(None, None)],
        "the prompt was read and stored despite prompt_titles = false, because \
         an unrelated typo made the whole config fall back to the defaults"
    );

    // Last, what the panel would draw.
    let out = env_cmd(root, &socket, &["status", "--json"]);
    let shown = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        !shown.contains(PROMPT),
        "`osm status --json` reported the prompt to the panel: {shown}"
    );
}
