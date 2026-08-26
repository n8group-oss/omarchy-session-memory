//! The inventory `osm agents` prints: every conversation on disk, split into
//! the ones a live pane is holding open right now and the ones that are only
//! resumable.
//!
//! **One stand-in agent per test binary.** `detect::bind` inspects the open
//! file descriptors of the probe's process tree, and every test in this file
//! shares one process. Two tests each running a stand-in agent would each see
//! the other's transcript, which `bind` correctly reads as an ambiguous pane
//! and refuses to bind — so exactly one test below starts one, and the rest
//! work from fixtures nothing has open.

use osm::agent::detect::PaneProbe;
use osm::agent::{AgentAdapter, AgentKind};
use std::path::Path;

fn fixture(root: &Path, id: &str) -> std::path::PathBuf {
    let path = root.join("projects/-tmp").join(format!("{id}.jsonl"));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "{\"cwd\":\"/tmp\"}\n").unwrap();
    path
}

fn claude(root: &Path) -> Vec<Box<dyn AgentAdapter>> {
    vec![Box::new(osm::agent::claude::Claude::with_home(root)) as Box<dyn AgentAdapter>]
}

/// Ids unique to this file. `bind` reads the *process's* open file
/// descriptors, and `is_active_elsewhere` (used elsewhere in the suite)
/// scans every process on the machine, so a conversation id shared with
/// another test binary makes the two interfere whenever they overlap.
const LIVE_ID: &str = "0cfebf91-81c0-43d5-af63-c9fe7e844a01";
const IDLE_ID: &str = "0cfebf91-81c0-43d5-af63-c9fe7e844a02";

/// A child running under the name `claude` and holding `transcript` open,
/// killed when it goes out of scope.
///
/// Detached stdio: a child left holding the test harness's stdout keeps
/// `cargo test` waiting on the pipe, so a failing assertion would hang the
/// suite instead of reporting.
struct Holder(std::process::Child);

impl Holder {
    fn spawn(transcript: &Path) -> Self {
        use std::os::unix::process::CommandExt;
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.arg0("claude")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            // The loop stops dash exec'ing into `sleep`, which would keep the
            // descriptor but rename the process.
            .arg("-c")
            .arg(format!(
                "exec 3<{:?}; while :; do sleep 60; done",
                transcript
            ));
        let child = cmd.spawn().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while osm::agent::detect::open_transcripts(child.id()).is_empty() {
            assert!(
                std::time::Instant::now() < deadline,
                "the stand-in agent never opened the transcript"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        Self(child)
    }
}

impl Drop for Holder {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn a_conversation_a_live_pane_holds_open_is_live_and_not_also_resumable() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let held = fixture(root, LIVE_ID);
    fixture(root, IDLE_ID);

    // A child process called `claude` holding the transcript open — the
    // *agent's own lineage*, which is the only place a transcript descriptor
    // counts as evidence (see `tests/agent_detect_lineage.rs`). Opening the
    // file in this process would prove nothing about any pane: the binding
    // has to come from the agent, not from whatever happens to be running.
    let _agent = Holder::spawn(&held);

    let probes = vec![
        PaneProbe {
            pane_id: "%7".to_string(),
            pane_pid: std::process::id(),
            cwd: "/tmp".to_string(),
            foreground_cmd: "claude".to_string(),
        },
        // Same process tree, same open transcript, but an idle shell in the
        // foreground: below the threshold, so it must not be reported as
        // holding the conversation.
        PaneProbe {
            pane_id: "%8".to_string(),
            pane_pid: std::process::id(),
            cwd: "/tmp".to_string(),
            foreground_cmd: "bash".to_string(),
        },
    ];

    let inv = osm::agent::inventory(&probes, &claude(root)).unwrap();

    assert_eq!(
        inv.live.len(),
        1,
        "one pane holds one conversation: {:?}",
        inv.live
    );
    assert_eq!(inv.live[0].pane_id, "%7");
    assert_eq!(inv.live[0].native_id, LIVE_ID);
    assert_eq!(inv.live[0].kind, AgentKind::Claude);
    assert!(
        inv.live[0].session.as_ref().is_some_and(|s| s.alive),
        "a live entry's session is marked alive, not left at discover()'s false"
    );

    let resumable: Vec<&str> = inv.resumable.iter().map(|s| s.native_id.as_str()).collect();
    assert_eq!(
        resumable,
        vec![IDLE_ID],
        "a conversation already live must not also be offered as resumable"
    );
}

#[test]
fn with_no_panes_at_all_every_conversation_is_resumable() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fixture(root, LIVE_ID);
    fixture(root, IDLE_ID);

    let inv = osm::agent::inventory(&[], &claude(root)).unwrap();

    assert!(inv.live.is_empty(), "{:?}", inv.live);
    let mut ids: Vec<&str> = inv.resumable.iter().map(|s| s.native_id.as_str()).collect();
    ids.sort_unstable();
    let mut expected = vec![LIVE_ID, IDLE_ID];
    expected.sort_unstable();
    assert_eq!(ids, expected);
}

#[test]
fn no_adapters_means_no_inventory_rather_than_an_error() {
    let inv = osm::agent::inventory(&[], &[]).unwrap();
    assert!(inv.live.is_empty());
    assert!(inv.resumable.is_empty());
}

#[test]
fn resumable_is_newest_first_so_the_menu_can_take_the_head() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let older = fixture(root, LIVE_ID);
    let newer = fixture(root, IDLE_ID);
    // Distinct mtimes, oldest first in creation order, so a stable sort that
    // never reorders would fail this.
    filetime_set(&older, 1_000_000);
    filetime_set(&newer, 2_000_000);

    let inv = osm::agent::inventory(&[], &claude(root)).unwrap();
    let ids: Vec<&str> = inv.resumable.iter().map(|s| s.native_id.as_str()).collect();
    assert_eq!(ids, vec![IDLE_ID, LIVE_ID], "newest last_active first");
}

/// Set a file's mtime, so `last_active` can be made deliberately distinct.
/// `std::fs::File::set_times` does this from the standard library; a
/// filetime crate would be a new dependency, which this plan forbids.
fn filetime_set(path: &Path, epoch_secs: u64) {
    let f = std::fs::File::options().write(true).open(path).unwrap();
    let t = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(epoch_secs);
    f.set_times(std::fs::FileTimes::new().set_modified(t))
        .unwrap();
}
