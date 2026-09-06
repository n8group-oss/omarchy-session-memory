//! What a bar widget needs out of `osm status --json`.
//!
//! The menu shows sessions grouped by workspace with an agent count each.
//! Deriving that in QML would mean the widget recomputing, from several
//! commands, what the engine already knows — with its own idea of what
//! counts. One command, one shape, one truth.

mod common;

use serde_json::Value;

fn status(env: &common::Env) -> Value {
    let out = env.osm(&["status", "--json"]);
    serde_json::from_slice(&out.stdout).expect("status is one JSON object")
}

#[test]
fn status_lists_each_session_with_its_counts() {
    let env = common::Env::new("status-sessions");
    env.tmux(&["new-session", "-d", "-s", "dev", "-c", "/tmp"]);
    env.tmux(&["split-window", "-t", "=dev:", "-c", "/tmp"]);
    env.osm(&["snapshot", "--reason", "test"]);

    let v = status(&env);
    let sessions = v["sessions"].as_array().expect("sessions is an array");
    let dev = sessions
        .iter()
        .find(|s| s["name"] == "dev")
        .expect("dev is listed");
    assert_eq!(dev["panes"], 2, "{dev}");
    assert_eq!(dev["windows"], 1, "{dev}");
    assert_eq!(dev["agents"], 0, "no agent is running in a bare shell");
}

/// This machine has no compositor (the harness says so), so nothing recorded
/// where the session's terminal window was. The fields are still present and
/// they are `null` — "unknown" must never render as a workspace named
/// something, and a widget that indexes them unconditionally must not break.
#[test]
fn a_session_with_no_recorded_placement_reports_null_not_a_guess() {
    let env = common::Env::new("status-noplace");
    env.tmux(&["new-session", "-d", "-s", "dev", "-c", "/tmp"]);
    env.osm(&["snapshot", "--reason", "test"]);

    let v = status(&env);
    let dev = v["sessions"][0].clone();
    assert_eq!(dev["name"], "dev", "the session is listed at all: {v}");
    assert!(dev["workspace"].is_null(), "{dev}");
    assert!(dev["monitor"].is_null(), "{dev}");
}

#[test]
fn status_reports_the_newest_snapshot_and_its_age() {
    let env = common::Env::new("status-snapshot");
    env.tmux(&["new-session", "-d", "-s", "dev", "-c", "/tmp"]);
    env.osm(&["snapshot", "--reason", "test"]);

    let v = status(&env);
    let snap = &v["snapshot"];
    assert!(snap["id"].is_i64(), "{snap}");
    assert_eq!(snap["state"], "complete");
    assert_eq!(snap["sessions"], 1, "{snap}");
    assert!(
        snap["age_secs"].as_i64().unwrap() >= 0,
        "age is never negative: {snap}"
    );
}

#[test]
fn status_with_no_snapshot_says_so_rather_than_inventing_one() {
    let env = common::Env::new("status-empty");
    let v = status(&env);
    assert!(v["snapshot"].is_null(), "{v}");
    assert_eq!(v["sessions"].as_array().unwrap().len(), 0);
}

#[test]
fn the_protocol_version_is_present_and_unchanged() {
    // The widget refuses to render against a major it was not built for.
    // Changing this is a breaking change for every installed plugin.
    let env = common::Env::new("status-proto");
    assert_eq!(status(&env)["protocol_version"], 1);
}
