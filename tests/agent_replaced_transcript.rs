//! A transcript replaced under a running agent must never read as "nobody is
//! holding this conversation".
//!
//! # The defect these tests exist for
//!
//! Agents rewrite their transcript by writing a new file and renaming it over
//! the old one. The agent that had the old file open goes on writing to it,
//! and `/proc/<pid>/fd/N` then points at `…/<id>.jsonl (deleted)` — whose
//! extension is `jsonl (deleted)`, not `jsonl`. The scan that looked for
//! transcript-shaped descriptors dropped it, discovery meanwhile indexed the
//! *replacement*, and the two never met: `is_active_elsewhere` answered
//! `Inactive` for a conversation that was very much alive, which is the one
//! answer that lets a second client be attached to it.
//!
//! Not knowing is now `Unknown`, and every caller already fails closed on
//! that. The two tests use different fixture homes and different conversation
//! ids so that neither can see the other's descriptors when the binary runs
//! its tests in parallel.

use osm::agent::{AgentAdapter, Liveness};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const HELD: &str = "0cfebf91-81c0-43d5-af63-c9fe7e844e11";
const UNHELD: &str = "0cfebf91-81c0-43d5-af63-c9fe7e844e12";

fn fixture(home: &Path, id: &str) -> std::path::PathBuf {
    let path = home.join("projects/-tmp").join(format!("{id}.jsonl"));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "{\"cwd\":\"/tmp\"}\n").unwrap();
    path
}

/// Write a new file beside `path` and rename it over the top, which is how an
/// agent (and every other well-behaved writer) replaces a file it cannot
/// afford to leave half-written. The inode anyone had open is unlinked by it.
fn replace_atomically(path: &Path) {
    let staging = path.with_extension("jsonl.new");
    std::fs::write(&staging, "{\"cwd\":\"/tmp\",\"replaced\":true}\n").unwrap();
    std::fs::rename(&staging, path).unwrap();
}

/// Kills its child on drop, including when an assertion panics — the child
/// would otherwise hold the harness's stdout open and hang `cargo test`.
struct Holder(Child);
impl Drop for Holder {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
impl Holder {
    fn pid(&self) -> u32 {
        self.0.id()
    }
}

fn holder(transcript: &Path) -> Holder {
    use std::os::unix::process::CommandExt;
    let mut cmd = Command::new("/bin/sh");
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // The loop keeps `sh` from exec'ing into `sleep` and taking the descriptor
    // out from under the name this test is about.
    cmd.arg0("claude").arg("-c").arg(format!(
        "exec 3<{:?}; while :; do sleep 60; done",
        transcript
    ));
    Holder(cmd.spawn().unwrap())
}

fn wait_until_holding(pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while osm::agent::detect::open_transcripts(pid).is_empty() {
        assert!(
            Instant::now() < deadline,
            "the child never opened its transcript"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The whole failure, in the order it happens.
#[test]
fn a_conversation_whose_file_was_replaced_under_its_agent_is_not_free_to_resume() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("claude");
    let transcript = fixture(&home, HELD);
    let adapter = osm::agent::claude::Claude::with_home(&home);

    let child = holder(&transcript);
    wait_until_holding(child.pid());
    assert_eq!(
        adapter.is_active_elsewhere(HELD).unwrap(),
        Liveness::Active,
        "a held transcript that still has its name is plainly active, or this \
         test proves nothing about the one that loses it"
    );

    replace_atomically(&transcript);
    assert!(
        osm::agent::detect::open_transcripts(child.pid()).is_empty(),
        "the fixture only means anything if the descriptor really was orphaned"
    );

    assert_eq!(
        adapter.is_active_elsewhere(HELD).unwrap(),
        Liveness::Unknown,
        "the agent still has this conversation open through a descriptor whose \
         file has been unlinked; reporting it as inactive is what lets a second \
         client be sent into a live conversation"
    );
}

/// The narrowing half: an orphaned descriptor somewhere else on the machine is
/// not evidence about *this* conversation. Without that, one deleted `.jsonl`
/// anywhere would make every conversation unresumable for ever.
#[test]
fn an_unrelated_orphaned_transcript_does_not_make_everything_unknown() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("claude");
    let held = fixture(&home, HELD);
    fixture(&home, UNHELD);
    let adapter = osm::agent::claude::Claude::with_home(&home);

    let child = holder(&held);
    wait_until_holding(child.pid());
    replace_atomically(&held);

    assert_eq!(
        adapter.is_active_elsewhere(UNHELD).unwrap(),
        Liveness::Inactive,
        "nothing holds this conversation, and another conversation's orphaned \
         descriptor says nothing about it"
    );
}
