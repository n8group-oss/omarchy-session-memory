//! Turning `privacy.prompt_titles` off has to take the prompt back.
//!
//! # Why this file exists
//!
//! `privacy.prompt_titles` is the one switch osm offers over the one thing it
//! reads out of a conversation's contents: when an agent wrote no title of its
//! own, one truncated line of the user's first message stands in for one. The
//! switch governed the *derivation* and nothing else. Once a prompt-derived
//! title had been captured, setting the key to `false` neither removed it nor
//! hid it — the capture upsert keeps the title already on record when
//! extraction returns nothing (`COALESCE(excluded.title, agent_sessions.title)`)
//! and `osm status --json` reads `agent_sessions.title` unconditionally. The
//! user's sentence stayed in SQLite and stayed on the panel, and the only way
//! to get it out was to delete the database.
//!
//! A privacy control that cannot revoke is not a privacy control. `false` is
//! the strict setting, and strict has to mean *strict now*: no prompt-derived
//! title is shown from the moment the key is read, and none survives the next
//! capture.
//!
//! # What this actually verifies
//!
//! End to end, through the shipped binary and a real (private) tmux server: a
//! prompt-derived title is captured with the key on, the key is turned off,
//! and then both places it could still be are checked — what `osm status
//! --json` says, and what is in the database file afterwards. Both, because
//! they are different failures: hiding it on the panel while it sits in
//! SQLite is not revocation, and clearing it in SQLite while the panel still
//! draws the copy it read a moment ago is not either.

mod common;

use osm::tmux::Tmux;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

struct Server(Tmux);
impl Drop for Server {
    fn drop(&mut self) {
        common::shutdown(&self.0);
    }
}

/// A private tmux server with one session, `dev`.
fn server(label: &str) -> Server {
    let t = Tmux::with_socket(&format!("osm-privacy-{}-{}", label, std::process::id()));
    t.run(&["new-session", "-d", "-s", "dev", "-c", "/tmp"])
        .unwrap();
    Server(t)
}

/// Claude only, no compositor, and `privacy.prompt_titles` set explicitly.
///
/// Written afresh on each call, because switching the key is the subject: the
/// test turns it off between two runs of the same binary, exactly as a user
/// editing the file would.
fn write_config(dir: &Path, prompt_titles: bool) -> PathBuf {
    let config_home = dir.join("config");
    let path = config_home.join("osm/config.toml");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        &path,
        format!(
            "[agents]\nenabled = [\"claude\"]\n\
             [restore]\nplace_windows = false\n\
             [privacy]\nprompt_titles = {prompt_titles}\n"
        ),
    )
    .unwrap();
    config_home
}

fn osm(tmux: &Tmux, dir: &Path, home: &Path, args: &[&str]) -> std::process::Output {
    let out = Command::new(env!("CARGO_BIN_EXE_osm"))
        .arg("--socket")
        .arg(tmux.socket().expect("a named socket"))
        .args(args)
        .env("OSM_CLAUDE_HOME", home)
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("XDG_STATE_HOME", dir.join("state"))
        .output()
        .expect("run osm");
    assert!(
        out.status.success(),
        "osm {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

fn status(tmux: &Tmux, dir: &Path, home: &Path) -> Value {
    let out = osm(tmux, dir, home, &["status", "--json"]);
    serde_json::from_slice(&out.stdout).expect("status is one JSON object")
}

fn dev_session(v: &Value) -> Value {
    v["sessions"]
        .as_array()
        .unwrap_or_else(|| panic!("status lists sessions: {v}"))
        .iter()
        .find(|s| s["name"] == "dev")
        .unwrap_or_else(|| panic!("dev is listed: {v}"))
        .clone()
}

/// Every `(title, title_source)` the database holds for a conversation.
///
/// Read out of the file itself rather than out of a status response: the point
/// of the second half of this test is what is *stored*, and a report is not
/// storage.
fn stored_titles(dir: &Path) -> Vec<(Option<String>, Option<String>)> {
    let db = dir.join("state/osm/state.db");
    let conn = rusqlite::Connection::open_with_flags(
        &db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .unwrap_or_else(|e| panic!("open {}: {e}", db.display()));
    let mut stmt = conn
        .prepare("SELECT title, title_source FROM agent_sessions ORDER BY native_id")
        .unwrap();
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, Option<String>>(0)?,
                r.get::<_, Option<String>>(1)?,
            ))
        })
        .unwrap();
    rows.map(|r| r.unwrap()).collect()
}

/// Write a transcript for `id` under the fixture Claude home, and return the
/// home.
fn transcript(dir: &Path, id: &str, body: &str) -> PathBuf {
    let home = dir.join("claude");
    let path = home.join("projects/-tmp").join(format!("{id}.jsonl"));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, body).unwrap();
    home
}

/// Start the stub agent on `id` in `pane` and wait until the pane is running
/// it, so that a capture binds the pane to that conversation.
fn run_agent(s: &Server, bin: &Path, home: &Path, pane: &str, id: &str) {
    s.0.run(&[
        "send-keys",
        "-t",
        pane,
        &format!(
            "OSM_CLAUDE_HOME={} {}/claude --resume {id}",
            home.display(),
            bin.display()
        ),
        "C-m",
    ])
    .unwrap();
    assert_eq!(
        common::wait_for_pane_cmd(&s.0, pane, "claude", Duration::from_secs(10)),
        "claude",
        "the stub agent never started in {pane}"
    );
}

const ID: &str = "0cfebf91-81c0-43d5-af63-c9fe7e844d10";

/// The sentence a user would not want kept. It is what a first prompt is:
/// their own words, in full, until osm truncates them.
const PROMPT: &str = "Rotate the production database credentials before Friday";

/// A conversation the agent never named, so the only title osm can give it is
/// the user's opening line.
fn untitled_transcript(id: &str) -> String {
    format!(
        "{{\"cwd\":\"/tmp\",\"sessionId\":\"{id}\"}}\n\
         {{\"type\":\"user\",\"sessionId\":\"{id}\",\"message\":{{\"role\":\"user\",\
           \"content\":\"{PROMPT}\"}}}}\n"
    )
}

/// The whole of the finding, in one run: on, captured, off, and then both
/// places the prompt could still be.
#[test]
fn switching_prompt_titles_off_revokes_a_prompt_already_captured() {
    let tmp = tempfile::tempdir().unwrap();
    let home = transcript(tmp.path(), ID, &untitled_transcript(ID));
    let bin = common::stub_agent(tmp.path());
    write_config(tmp.path(), true);

    let s = server("revoke");
    let pane = s.0.list_panes().unwrap()[0].id.clone();
    run_agent(&s, &bin, &home, &pane, ID);
    osm(&s.0, tmp.path(), &home, &["snapshot", "--reason", "test"]);

    // Precondition, not decoration: without it a later assertion could pass
    // because nothing was ever captured.
    let before = dev_session(&status(&s.0, tmp.path(), &home));
    assert_eq!(
        before["goal"]["title"], PROMPT,
        "with prompt_titles on, the user's opening line is the session's goal — \
         this is the state the switch has to be able to take back: {before}"
    );
    assert_eq!(before["goal"]["source"], "first_prompt", "{before}");
    assert_eq!(
        stored_titles(tmp.path()),
        vec![(Some(PROMPT.to_string()), Some("first_prompt".to_string()))],
        "the prompt-derived title is on record in the database"
    );

    // The user changes their mind. Nothing else about the machine changes.
    write_config(tmp.path(), false);

    // Half one: the panel. `osm status --json` is what the widget polls every
    // five seconds, and it must stop saying the prompt straight away — before
    // any capture has had a chance to run, because on a quiet machine the next
    // one may be minutes away.
    let after = dev_session(&status(&s.0, tmp.path(), &home));
    assert!(
        after["goal"].is_null(),
        "prompt_titles was switched off and status still reports the prompt as \
         this session's goal; the setting revoked nothing: {after}"
    );
    let conversations = after["conversations"].as_array().unwrap();
    assert_eq!(conversations.len(), 1, "{after}");
    assert!(
        conversations[0]["title"].is_null() && conversations[0]["title_source"].is_null(),
        "the conversation row still carries the prompt-derived title after the \
         setting was switched off: {after}"
    );

    // Half two: the store. A title that is merely hidden is still the user's
    // sentence sitting in a file they were told the setting governed.
    osm(&s.0, tmp.path(), &home, &["snapshot", "--reason", "after"]);
    assert_eq!(
        stored_titles(tmp.path()),
        vec![(None, None)],
        "the prompt-derived title is still stored in agent_sessions after a \
         capture taken with prompt_titles off; hiding it is not revoking it"
    );
}

/// The switch revokes prompts, and only prompts.
///
/// A title the agent wrote about its own conversation is not transcript
/// content and was never what `prompt_titles` governed. Clearing it too would
/// be the mirror-image defect: a user who asked osm not to read their messages
/// losing the labels Claude itself wrote, and the strict mode becoming one
/// nobody can live with.
#[test]
fn revocation_leaves_the_agents_own_titles_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let home = transcript(
        tmp.path(),
        ID,
        &format!(
            "{{\"cwd\":\"/tmp\",\"sessionId\":\"{ID}\"}}\n\
             {{\"type\":\"ai-title\",\"aiTitle\":\"Named by the agent\",\
               \"sessionId\":\"{ID}\"}}\n\
             {{\"type\":\"user\",\"sessionId\":\"{ID}\",\"message\":{{\"role\":\"user\",\
               \"content\":\"{PROMPT}\"}}}}\n"
        ),
    );
    let bin = common::stub_agent(tmp.path());
    write_config(tmp.path(), true);

    let s = server("keep");
    let pane = s.0.list_panes().unwrap()[0].id.clone();
    run_agent(&s, &bin, &home, &pane, ID);
    osm(&s.0, tmp.path(), &home, &["snapshot", "--reason", "test"]);

    write_config(tmp.path(), false);
    osm(&s.0, tmp.path(), &home, &["snapshot", "--reason", "after"]);

    let after = dev_session(&status(&s.0, tmp.path(), &home));
    assert_eq!(
        after["goal"]["title"], "Named by the agent",
        "the agent's own name for the conversation is not a prompt and is not \
         revoked with them: {after}"
    );
    assert_eq!(after["goal"]["source"], "agent", "{after}");
    assert_eq!(
        stored_titles(tmp.path()),
        vec![(
            Some("Named by the agent".to_string()),
            Some("agent".to_string())
        )],
        "the agent's title stays on record"
    );
}
