//! The scoring rules `bind` applies, exercised where no transcript is open at
//! all: the floor cases, which are the ones that decide whether a *guess* can
//! ever be recorded.
//!
//! No test in this file opens a transcript, and that is load-bearing —
//! `bind` inspects the open descriptors of the probe's process, which here is
//! the test binary itself, so one test holding a transcript open would change
//! what every other test in the binary sees. The lineage cases that need open
//! descriptors live in `tests/agent_detect_lineage.rs`, one process to
//! themselves.

use osm::agent::detect::{self, PaneProbe};
use osm::agent::{AgentAdapter, CONFIDENCE_THRESHOLD};

fn probe(cmd: &str) -> PaneProbe {
    PaneProbe {
        pane_id: "%1".to_string(),
        pane_pid: std::process::id(),
        cwd: "/home/u/app".to_string(),
        foreground_cmd: cmd.to_string(),
    }
}

/// Adapters aimed at a fixture home with nothing in it, so these tests never
/// read the developer's real `~/.claude` — and never depend on what is in it.
fn adapters(root: &std::path::Path) -> Vec<Box<dyn AgentAdapter>> {
    vec![
        Box::new(osm::agent::claude::Claude::with_home(&root.join("claude")))
            as Box<dyn AgentAdapter>,
        Box::new(osm::agent::codex::Codex::with_home(&root.join("codex"))) as Box<dyn AgentAdapter>,
    ]
}

#[test]
fn descendants_include_this_process_and_terminate() {
    let d = detect::descendants(std::process::id());
    assert!(
        d.contains(&std::process::id()),
        "a process is its own descendant root"
    );
    assert!(
        d.len() < 10_000,
        "the walk must terminate, not chase a cycle"
    );
}

#[test]
fn a_foreground_command_alone_does_not_bind() {
    // 0.4 on its own is below the threshold: a shell that happens to be
    // named `claude` is not evidence of a conversation.
    let tmp = tempfile::tempdir().unwrap();
    let adapters = adapters(tmp.path());
    let prepared = detect::prepare(&adapters).unwrap();
    let b = detect::bind(&probe("claude"), &prepared);
    assert!(b.is_none(), "bound on a name alone: {b:?}");
}

#[test]
fn an_idle_shell_binds_to_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let adapters = adapters(tmp.path());
    let prepared = detect::prepare(&adapters).unwrap();
    assert!(detect::bind(&probe("bash"), &prepared).is_none());
}

#[test]
fn no_enabled_adapters_binds_to_nothing() {
    assert!(detect::bind(&probe("claude"), &[]).is_none());
}

#[test]
fn the_threshold_rejects_every_single_signal() {
    // Guards the scoring table itself: if someone raises a weight so one
    // signal suffices, this fails.
    for single in [0.4_f32, 0.5, 0.1] {
        assert!(
            single < CONFIDENCE_THRESHOLD,
            "{single} alone would bind, so one signal is enough — that is a guess"
        );
    }
}

/// The assertion this test used to be missing entirely: it checked only that
/// whatever came back had a `.jsonl` extension, which an empty list satisfies
/// vacuously — and an empty list is *also* what a broken `open_transcripts`
/// returns, so the test could not fail either way.
#[test]
fn a_process_holding_no_transcript_yields_no_transcripts() {
    let found = detect::open_transcripts(std::process::id());
    assert!(
        found.is_empty(),
        "this test binary holds no transcript open, so nothing may be reported: {found:?}"
    );
}

/// And the same call does return something when there *is* something, so the
/// emptiness above is a fact about this process rather than about the
/// function.
#[test]
fn a_process_holding_a_transcript_yields_it() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp
        .path()
        .join("0cfebf91-81c0-43d5-af63-c9fe7e844c01.jsonl");
    std::fs::write(&path, "{}\n").unwrap();
    // A child, not this process: every other test in this binary depends on
    // this one holding nothing open.
    // Detached stdio, so a failing assertion below cannot leave a child
    // holding the harness's stdout open and wedge `cargo test`.
    let mut child = std::process::Command::new("/bin/sh")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .arg("-c")
        .arg(format!("exec 3<{:?}; sleep 30", path))
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let found = loop {
        let found = detect::open_transcripts(child.id());
        if !found.is_empty() || std::time::Instant::now() >= deadline {
            break found;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    let _ = child.kill();
    let _ = child.wait();
    assert_eq!(found, vec![path], "the transcript the child holds open");
}
