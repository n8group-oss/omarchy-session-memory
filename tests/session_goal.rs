//! What a recorded session is *about*, as `osm status --json` reports it.
//!
//! # Why this file exists
//!
//! `status.sessions` is what the menu draws, and every row of it said the same
//! kind of thing: `6w · 18p · 4a · DP-3`. True, and no help at all in telling
//! eight sessions apart — the maintainer's own complaint after living with the
//! plugin. A session's *goal* is the one thing that distinguishes it, and the
//! menu must be able to draw it without running a second command.
//!
//! # What a session's goal is, and why that one
//!
//! A tmux session holds several panes and therefore several conversations.
//! The goal reported here is **the title of the most recently active
//! conversation in the session that has one**, attributed to the conversation
//! it came from — never a summary stitched out of several, and never a
//! paraphrase. After a reboot the thing you were doing last is the thing you
//! want back, and a title that is shown against the wrong conversation is
//! worse than none.
//!
//! Every conversation the session holds is reported beside it, so "go deep"
//! means reading the list rather than guessing from one line.

mod common;

use osm::tmux::Tmux;
use serde_json::Value;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime};

struct Server(Tmux);
impl Drop for Server {
    fn drop(&mut self) {
        common::shutdown(&self.0);
    }
}

/// A private tmux server with one session, `dev`.
fn server(label: &str) -> Server {
    let t = Tmux::with_socket(&format!("osm-goal-{}-{}", label, std::process::id()));
    t.run(&["new-session", "-d", "-s", "dev", "-c", "/tmp"])
        .unwrap();
    Server(t)
}

/// Claude only, and no compositor: this machine has neither the developer's
/// other agents nor a Hyprland to ask.
fn config_home(dir: &Path) -> std::path::PathBuf {
    let config_home = dir.join("config");
    let path = config_home.join("osm/config.toml");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        &path,
        "[agents]\nenabled = [\"claude\"]\n[restore]\nplace_windows = false\n",
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
        .env("XDG_CONFIG_HOME", config_home(dir))
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

/// Write a transcript for `id` under the fixture Claude home, with `body` as
/// its contents and `age_secs` seconds of age, and return the home.
///
/// The age is set rather than waited for: `last_active` is the transcript's
/// mtime, and which conversation is the newest is the whole of the rule under
/// test.
fn transcript(dir: &Path, id: &str, body: &str, age_secs: u64) -> std::path::PathBuf {
    let home = dir.join("claude");
    let path = home.join("projects/-tmp").join(format!("{id}.jsonl"));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, body).unwrap();
    let when = SystemTime::now() - Duration::from_secs(age_secs);
    let file = std::fs::File::options().write(true).open(&path).unwrap();
    file.set_times(
        std::fs::FileTimes::new()
            .set_accessed(when)
            .set_modified(when),
    )
    .unwrap();
    home
}

/// A transcript whose agent named it `title`.
fn titled(id: &str, title: &str) -> String {
    format!(
        "{{\"cwd\":\"/tmp\",\"sessionId\":\"{id}\"}}\n\
         {{\"type\":\"ai-title\",\"aiTitle\":\"{title}\",\"sessionId\":\"{id}\"}}\n"
    )
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

fn status(tmux: &Tmux, dir: &Path, home: &Path) -> Value {
    let out = osm(tmux, dir, home, &["status", "--json"]);
    serde_json::from_slice(&out.stdout).expect("status is one JSON object")
}

const NEWER: &str = "0cfebf91-81c0-43d5-af63-c9fe7e844d01";
const OLDER: &str = "0cfebf91-81c0-43d5-af63-c9fe7e844d02";
const UNTITLED: &str = "0cfebf91-81c0-43d5-af63-c9fe7e844d03";

/// The row says what the session is for, and says which conversation said so.
#[test]
fn a_session_reports_the_goal_of_its_most_recent_conversation() {
    let tmp = tempfile::tempdir().unwrap();
    transcript(
        tmp.path(),
        OLDER,
        &titled(OLDER, "Rewrite the restore path"),
        3600,
    );
    let home = transcript(
        tmp.path(),
        NEWER,
        &titled(NEWER, "Tmux memory management plugin"),
        10,
    );
    let bin = common::stub_agent(tmp.path());
    let s = server("goal");
    s.0.run(&["split-window", "-t", "=dev:", "-c", "/tmp"])
        .unwrap();
    let panes: Vec<String> =
        s.0.list_panes()
            .unwrap()
            .into_iter()
            .map(|p| p.id)
            .collect();
    run_agent(&s, &bin, &home, &panes[0], OLDER);
    run_agent(&s, &bin, &home, &panes[1], NEWER);

    osm(&s.0, tmp.path(), &home, &["snapshot", "--reason", "test"]);
    let v = status(&s.0, tmp.path(), &home);
    let dev = v["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "dev")
        .unwrap_or_else(|| panic!("dev is listed: {v}"))
        .clone();

    assert_eq!(
        dev["goal"]["title"], "Tmux memory management plugin",
        "the newest conversation in the session is the one that says what the \
         session is for; the older one is what it was for an hour ago: {dev}"
    );
    assert_eq!(
        dev["goal"]["native_id"], NEWER,
        "a goal names the conversation it came from, so it can never be read \
         against the wrong one: {dev}"
    );
    assert_eq!(dev["goal"]["kind"], "claude", "{dev}");
    assert_eq!(dev["goal"]["source"], "agent", "{dev}");

    let conversations = dev["conversations"]
        .as_array()
        .unwrap_or_else(|| panic!("a session lists its conversations: {dev}"));
    assert_eq!(
        conversations.len(),
        2,
        "both conversations in the session are listed, so \"go deep\" is \
         reading a list rather than guessing: {dev}"
    );
    assert_eq!(
        conversations[0]["native_id"], NEWER,
        "newest first, the same order the goal is chosen in: {dev}"
    );
    assert_eq!(conversations[1]["native_id"], OLDER, "{dev}");
    assert_eq!(
        conversations[1]["title"], "Rewrite the restore path",
        "every conversation carries its own title, not just the one the goal \
         came from: {dev}"
    );
    assert!(
        conversations[0]["pane_idx"].is_i64() && conversations[0]["window_idx"].is_i64(),
        "a conversation says which pane it was in, or \"go deep\" cannot point \
         at anything: {dev}"
    );
}

/// A session whose conversations have no titles is *untitled*, not blank and
/// not invented.
#[test]
fn a_session_with_nothing_to_say_reports_no_goal_rather_than_a_guess() {
    let tmp = tempfile::tempdir().unwrap();
    let home = transcript(
        tmp.path(),
        UNTITLED,
        &format!("{{\"cwd\":\"/tmp\",\"sessionId\":\"{UNTITLED}\"}}\n"),
        10,
    );
    let bin = common::stub_agent(tmp.path());
    let s = server("untitled");
    let pane = s.0.list_panes().unwrap()[0].id.clone();
    run_agent(&s, &bin, &home, &pane, UNTITLED);

    osm(&s.0, tmp.path(), &home, &["snapshot", "--reason", "test"]);
    let v = status(&s.0, tmp.path(), &home);
    let dev = v["sessions"][0].clone();

    assert!(
        dev["goal"].is_null(),
        "no conversation here has a title, and the session must say so rather \
         than borrow one: {dev}"
    );
    let conversations = dev["conversations"].as_array().unwrap();
    assert_eq!(conversations.len(), 1, "{dev}");
    assert!(
        conversations[0]["title"].is_null(),
        "an untitled conversation is listed with a null title, never with a \
         stand-in: {dev}"
    );
}

/// A session with no conversations at all keeps the fields, empty.
///
/// A widget indexes them unconditionally; a missing key and an empty list are
/// different bugs, and only one of them is visible.
#[test]
fn a_session_with_no_conversations_still_has_the_fields() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("claude");
    std::fs::create_dir_all(home.join("projects")).unwrap();
    let s = server("bare");

    osm(&s.0, tmp.path(), &home, &["snapshot", "--reason", "test"]);
    let v = status(&s.0, tmp.path(), &home);
    let dev = v["sessions"][0].clone();

    assert_eq!(dev["name"], "dev", "{v}");
    assert!(dev["goal"].is_null(), "{dev}");
    assert_eq!(
        dev["conversations"].as_array().map(|c| c.len()),
        Some(0),
        "the field is an empty array, not absent: {dev}"
    );
}
