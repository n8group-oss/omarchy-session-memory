//! The delivery *mechanism*: a command reaches the pane and the pane runs it.
//!
//! Deliberately `deliver_command`, not `deliver`. This suite is about
//! `send`/`paste` plumbing and the incarnation guard, with commands
//! (`sleep`, `true`) that are not agents at all — `deliver` additionally
//! requires the pane to be *bound* to the exact conversation afterwards,
//! which is proved end to end in `tests/agent_e2e.rs` and refused in
//! `tests/agent_resume_wrong_conversation.rs`.

use osm::agent::resume::{self, Outcome};
use osm::tmux::Tmux;
use std::time::Duration;

struct Server(Tmux);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.run(&["kill-server"]);
        if let Some(s) = self.0.socket() {
            let _ = std::fs::remove_file(format!("/tmp/tmux-{}/{s}", real_uid()));
        }
    }
}

// Avoid a libc dependency: the real uid is readable straight out of
// /proc/self/status, matching the pattern used elsewhere in this project's
// test suite (e.g. tests/agent_resume_preconditions.rs).
fn real_uid() -> u32 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(1000)
}

fn server(label: &str) -> Server {
    let t = Tmux::with_socket(&format!("osm-del-{label}-{}", std::process::id()));
    t.run(&["new-session", "-d", "-s", "alpha", "-c", "/tmp"])
        .unwrap();
    Server(t)
}

/// The incarnation a delivery is bound to. Read from the server itself, which
/// is what every caller does: `deliver` refuses outright unless the server
/// answering is still this one.
fn incarnation(s: &Server) -> String {
    s.0.running_server_incarnation()
        .unwrap()
        .expect("the test server is running")
}

#[test]
fn a_command_that_starts_is_reported_resumed() {
    let s = server("ok");
    let pane = s.0.list_panes().unwrap()[0].id.clone();
    let argv = vec!["sleep".to_string(), "30".to_string()];
    let out = resume::deliver_command(
        &s.0,
        &pane,
        &argv,
        "sleep",
        Duration::from_secs(10),
        &incarnation(&s),
    );
    assert!(matches!(out, Outcome::Resumed), "{out:?}");
    assert_eq!(s.0.list_panes().unwrap()[0].cmd, "sleep");
}

#[test]
fn a_command_that_never_starts_is_failed_not_resumed() {
    // The whole point: a resume that silently did nothing must not be
    // recorded as success, or the snapshot retires with the work undone.
    let s = server("nostart");
    let pane = s.0.list_panes().unwrap()[0].id.clone();
    let argv = vec!["true".to_string()];
    let out = resume::deliver_command(
        &s.0,
        &pane,
        &argv,
        "definitely-not-running",
        Duration::from_secs(2),
        &incarnation(&s),
    );
    match out {
        Outcome::Failed(msg) => assert!(msg.contains("definitely-not-running"), "{msg}"),
        other => panic!("expected Failed, got {other:?}"),
    }
}

#[test]
fn delivery_into_a_pane_that_vanished_is_failed() {
    let s = server("gone");
    let out = resume::deliver_command(
        &s.0,
        "%999",
        &["sleep".to_string(), "1".to_string()],
        "sleep",
        Duration::from_secs(2),
        &incarnation(&s),
    );
    assert!(matches!(out, Outcome::Failed(_)), "{out:?}");
}
