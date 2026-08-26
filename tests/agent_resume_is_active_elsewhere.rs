//! Real `is_active_elsewhere` behaviour, for all three adapters.
//!
//! Claude and Codex answer by looking for a live process that still holds
//! the conversation's transcript file open — the same fd-inspection
//! `detect::open_transcripts` already does for pane binding, just applied
//! across every pid in `/proc` instead of one pane's descendants. These
//! tests spawn a real child process that opens a fixture transcript and
//! blocks, so the detector has a genuine open fd to find, not a mock.
//!
//! The fixtures sit in the layout each adapter's `discover` walks, because
//! that is now what makes them transcripts: an open descriptor is matched
//! against the files discovery found, by device and inode, never by the shape
//! of its name.

use osm::agent::codex::Codex;
use osm::agent::{AgentAdapter, Liveness};
use std::path::Path;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

/// A child process that opens `path` on fd 3 and then blocks reading its
/// own stdin, so the fd stays open for as long as the test needs it to.
/// Always killed on drop, so a failing assertion never leaks a process.
struct Holder(Child);
impl Drop for Holder {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn hold_open(path: &Path) -> Holder {
    let child = Command::new("sh")
        .arg("-c")
        .arg(format!("exec 3<{:?}; read line", path))
        .stdin(std::process::Stdio::piped())
        .spawn()
        .expect("spawn fd-holding fixture process");
    Holder(child)
}

/// Polls up to `timeout` for `f` to return true, so the test does not race
/// the child process actually reaching its `exec 3<...`.
fn wait_until(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    loop {
        if f() {
            return true;
        }
        if start.elapsed() > timeout {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn claude_reports_active_elsewhere_while_a_process_holds_the_transcript_open() {
    let tmp = tempfile::tempdir().unwrap();
    let id = "0cfebf91-81c0-43d5-af63-c9fe7e844ede";
    // In the layout `discover` actually walks. A transcript is recognised
    // because it *is* one of the files discovery found — matched by device
    // and inode — so a file dropped anywhere with a transcript-shaped name is
    // deliberately not one.
    let transcript = tmp.path().join("projects/-tmp").join(format!("{id}.jsonl"));
    std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    std::fs::write(&transcript, "{\"cwd\":\"/tmp\"}\n").unwrap();

    let a = osm::agent::claude::Claude::with_home(tmp.path());
    assert_eq!(
        a.is_active_elsewhere(id).unwrap(),
        Liveness::Inactive,
        "nothing holds it open yet"
    );

    let _holder = hold_open(&transcript);
    let became_active = wait_until(Duration::from_secs(5), || {
        a.is_active_elsewhere(id).unwrap() == Liveness::Active
    });
    assert!(
        became_active,
        "held-open transcript was never reported active"
    );
}

#[test]
fn claude_reports_not_active_for_an_id_nothing_has_open() {
    let a = osm::agent::claude::Claude::with_home(Path::new("/nonexistent"));
    assert_eq!(
        a.is_active_elsewhere("0cfebf91-81c0-43d5-af63-c9fe7e844ede")
            .unwrap(),
        Liveness::Inactive
    );
}

#[test]
fn codex_reports_active_elsewhere_while_a_process_holds_the_rollout_open() {
    let tmp = tempfile::tempdir().unwrap();
    let id = "0cfebf91-81c0-43d5-af63-c9fe7e844ede";
    let transcript = tmp
        .path()
        .join("sessions/2026/01/01")
        .join(format!("rollout-20260101-000000-{id}.jsonl"));
    std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    std::fs::write(&transcript, "{\"cwd\":\"/tmp\"}\n").unwrap();

    let a = Codex::with_home(tmp.path());
    assert_eq!(a.is_active_elsewhere(id).unwrap(), Liveness::Inactive);

    let _holder = hold_open(&transcript);
    let became_active = wait_until(Duration::from_secs(5), || {
        a.is_active_elsewhere(id).unwrap() == Liveness::Active
    });
    assert!(became_active, "held-open rollout was never reported active");
}

#[test]
fn codex_reports_not_active_for_an_id_nothing_has_open() {
    let a = Codex::with_home(Path::new("/nonexistent"));
    assert_eq!(
        a.is_active_elsewhere("0cfebf91-81c0-43d5-af63-c9fe7e844ede")
            .unwrap(),
        Liveness::Inactive
    );
}

/// OpenCode's CLI exposes no ownership query at all, so the honest answer is
/// `Unknown` — and `Unknown` is not `Inactive`.
///
/// It used to answer `false`, which every caller reads as "verified nobody has
/// this open", and `osm resume` would then attach a second client to a
/// conversation already running in another pane. The distinction is the whole
/// protection, so it is asserted as a value rather than as a boolean nobody
/// can tell apart from a real answer.
#[test]
fn opencode_reports_unknown_liveness_never_inactive() {
    let a = osm::agent::opencode::OpenCode::with_binary("definitely-not-a-real-binary");
    assert_eq!(
        a.is_active_elsewhere("anything").unwrap(),
        Liveness::Unknown
    );
    assert_ne!(
        a.is_active_elsewhere("anything").unwrap(),
        Liveness::Inactive,
        "unknowable must never be reported as verified-not-active"
    );
}

/// And the refusal that follows from it: a resume whose exclusivity cannot be
/// checked is `Unsupported`, so nothing is sent into the pane.
#[test]
fn a_resume_whose_exclusivity_is_unknowable_is_refused_before_anything_is_sent() {
    let a = osm::agent::opencode::OpenCode::with_binary("definitely-not-a-real-binary");
    assert!(
        a.auto_unsupported_reason().is_some(),
        "an adapter that cannot answer must say it is unsupported"
    );
    // `deliver` refuses before it touches tmux at all, which is why no server
    // is needed here: there is nothing to send and nowhere to send it.
    let tmux = osm::tmux::Tmux::with_socket("osm-never-started-unsupported");
    let outcome = osm::agent::resume::deliver(
        &tmux,
        "%0",
        &a,
        "ses_x",
        Duration::from_millis(1),
        "not-a-real-incarnation",
    );
    assert_eq!(outcome, osm::agent::resume::Outcome::Unsupported);
}
