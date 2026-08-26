//! A live agent that will not show its descriptors must not read as "nobody
//! holds this conversation".
//!
//! # The defect these tests exist for
//!
//! `/proc/<pid>/fd` is not always readable, even for a process of one's own.
//! A non-dumpable process — one that dropped privileges, or that a sandbox set
//! `PR_SET_DUMPABLE` to 0 on — has every inode under its `/proc` directory
//! reparented to root and its descriptor directory denied to its own user. The
//! scan read that denial as an empty list, so a same-user agent that happened
//! to be sandboxed was reported as holding no descriptors, `Inactive` came
//! back with no other uncertainty in play, and `preflight` sent a second
//! client into a conversation somebody was talking to.
//!
//! # Why the fix is a narrower candidate set and not a wider Unknown
//!
//! Answering `Unknown` for *any* process osm cannot inspect would make every
//! resume fail: an ordinary desktop always has a few. On the machine this was
//! written on, six of the user's 470 processes keep their descriptor
//! directories closed, and not one of them is an agent. So a process that
//! hides its descriptors only makes the answer uncertain when the two facts
//! that stay readable say it could be the agent: it runs as the same user, and
//! it runs under the agent's own binary name.
//!
//! # Why these tests name their own process table
//!
//! A descriptor directory this process may not list cannot be produced from
//! safe Rust — `PR_SET_DUMPABLE` needs a libc this project deliberately does
//! not depend on — and a mode-000 directory would not produce one either in a
//! CI container, whose root reads every directory regardless of its mode. So
//! the fixture builds the `/proc` it wants and hands it to
//! `live_process_ownership_in`, whose only other caller passes `/proc`. The
//! last two tests use the real one, and are the control: an ordinary resume,
//! on a real machine with real protected processes, still goes ahead.

mod common;

use osm::agent::{detect, Liveness};
use osm::tmux::Tmux;
use std::path::{Path, PathBuf};
use std::time::Duration;

const ID: &str = "0cfebf91-81c0-43d5-af63-c9fe7e844f01";
const IDLE_ID: &str = "0cfebf91-81c0-43d5-af63-c9fe7e844f02";

fn our_uid() -> u32 {
    std::fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|s| s.parse::<u32>().ok())
        .expect("a real uid out of /proc/self/status")
}

/// A `/proc` of this test's own, with `self/status` reporting the uid this
/// process really runs as — which is what the candidate filter compares
/// against.
fn proc_root(tmp: &Path) -> PathBuf {
    let root = tmp.join("proc");
    let me = root.join("self");
    std::fs::create_dir_all(&me).unwrap();
    write_status(&me, our_uid());
    root
}

fn write_status(dir: &Path, uid: u32) {
    std::fs::write(
        dir.join("status"),
        format!("Name:\tx\nState:\tS (sleeping)\nUid:\t{uid}\t{uid}\t{uid}\t{uid}\n"),
    )
    .unwrap();
}

/// A process in that table whose descriptor directory will not be listed.
///
/// `fd` is written as a plain file rather than a directory with no
/// permissions: the read then fails for *every* user, including the root of a
/// CI container, which is the only way this fixture means the same thing
/// everywhere. What the production code reads from the failure is the same
/// either way — the directory is there and would not be listed — since it
/// distinguishes only "no such directory" (the process has exited) from every
/// other reason a read can fail.
fn hidden_process(root: &Path, pid: u32, uid: u32, name: &str) {
    let dir = root.join(pid.to_string());
    std::fs::create_dir_all(&dir).unwrap();
    write_status(&dir, uid);
    std::fs::write(dir.join("comm"), format!("{name}\n")).unwrap();
    std::fs::write(dir.join("cmdline"), format!("/usr/bin/{name}\0--flag\0")).unwrap();
    std::fs::write(dir.join("fd"), "not a directory").unwrap();
}

/// A conversation on disk, so the index this is asked about is not empty.
fn home_with_transcript(tmp: &Path, id: &str) -> PathBuf {
    let home = tmp.join("claude");
    let transcript = home.join("projects/-tmp").join(format!("{id}.jsonl"));
    std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    std::fs::write(&transcript, "{\"cwd\":\"/tmp\"}\n").unwrap();
    home
}

#[test]
fn a_same_user_agent_that_hides_its_descriptors_makes_ownership_unknown() {
    let tmp = tempfile::tempdir().unwrap();
    let home = home_with_transcript(tmp.path(), ID);
    let adapter = osm::agent::claude::Claude::with_home(&home);
    let root = proc_root(tmp.path());
    hidden_process(&root, 4242, our_uid(), "claude");

    assert_eq!(
        detect::live_process_ownership_in(&adapter, ID, &root).unwrap(),
        Liveness::Unknown,
        "a process of this user, running under the agent's own name, that will \
         not show its descriptors may be holding this very conversation; \
         reporting it as inactive is what lets a second client into it"
    );
}

#[test]
fn a_protected_process_of_another_user_is_not_a_candidate() {
    let tmp = tempfile::tempdir().unwrap();
    let home = home_with_transcript(tmp.path(), ID);
    let adapter = osm::agent::claude::Claude::with_home(&home);
    let root = proc_root(tmp.path());
    // Same name, different user: a system daemon osm cannot read and has no
    // business being uncertain about.
    hidden_process(&root, 4243, our_uid().wrapping_add(1), "claude");

    assert_eq!(
        detect::live_process_ownership_in(&adapter, ID, &root).unwrap(),
        Liveness::Inactive,
        "an agent osm might have to resume is the user's own; letting somebody \
         else's protected process make this uncertain would refuse every resume"
    );
}

#[test]
fn a_protected_process_that_is_not_the_agent_is_not_a_candidate() {
    let tmp = tempfile::tempdir().unwrap();
    let home = home_with_transcript(tmp.path(), ID);
    let adapter = osm::agent::claude::Claude::with_home(&home);
    let root = proc_root(tmp.path());
    // The shapes a real desktop actually has: a password manager's browser
    // helper, a fuse mount helper, a session helper.
    for (pid, name) in [
        (4244u32, "1Password-BrowserSupport"),
        (4245, "fusermount3"),
        (4246, "uwsm-app"),
    ] {
        hidden_process(&root, pid, our_uid(), name);
    }

    assert_eq!(
        detect::live_process_ownership_in(&adapter, ID, &root).unwrap(),
        Liveness::Inactive,
        "these are the processes every desktop keeps closed; if they made the \
         answer uncertain, no conversation on this machine could ever be resumed"
    );
}

#[test]
fn a_process_that_exited_during_the_scan_is_absence_not_uncertainty() {
    let tmp = tempfile::tempdir().unwrap();
    let home = home_with_transcript(tmp.path(), ID);
    let adapter = osm::agent::claude::Claude::with_home(&home);
    let root = proc_root(tmp.path());
    // Listed a moment ago and torn down since: it has no descriptor directory
    // at all. Nothing is being withheld, because there is nothing left.
    let dir = root.join("4247");
    std::fs::create_dir_all(&dir).unwrap();
    write_status(&dir, our_uid());
    std::fs::write(dir.join("comm"), "claude\n").unwrap();

    assert_eq!(
        detect::live_process_ownership_in(&adapter, ID, &root).unwrap(),
        Liveness::Inactive,
        "an agent that has exited holds nothing; reading its absence as \
         uncertainty would make every resume fail after every agent exits"
    );
}

// ---- the control: an ordinary resume, against the real process table -------

#[test]
fn a_conversation_nobody_holds_is_inactive_on_this_real_machine() {
    let tmp = tempfile::tempdir().unwrap();
    let home = home_with_transcript(tmp.path(), IDLE_ID);
    let adapter = osm::agent::claude::Claude::with_home(&home);

    assert_eq!(
        detect::live_process_ownership(&adapter, IDLE_ID).unwrap(),
        Liveness::Inactive,
        "this machine's own protected processes must not make an unheld \
         conversation unresumable"
    );
}

struct Server(Tmux);
impl Drop for Server {
    fn drop(&mut self) {
        common::shutdown(&self.0);
    }
}

#[test]
fn an_ordinary_resume_into_an_idle_pane_still_passes_preflight() {
    let tmp = tempfile::tempdir().unwrap();
    // A real conversation on disk, so the index is not empty and the whole of
    // `/proc` really is scanned — the path the narrowing runs on.
    let home = home_with_transcript(tmp.path(), IDLE_ID);
    let adapter = osm::agent::claude::Claude::with_home(&home);

    let t = Tmux::with_socket(&format!("osm-hiddenholder-{}", std::process::id()));
    t.run(&["new-session", "-d", "-s", "alpha", "-c", "/tmp"])
        .unwrap();
    let srv = Server(t);
    let pane = srv.0.list_panes().unwrap()[0].id.clone();
    assert_eq!(
        common::wait_for_pane_cmd(&srv.0, &pane, "bash", Duration::from_secs(10)),
        "bash",
        "the pane never settled to its shell"
    );

    let out = osm::agent::resume::preflight(
        &srv.0,
        &pane,
        &adapter,
        IDLE_ID,
        std::slice::from_ref(&pane),
    );
    assert_eq!(
        out.as_str(),
        "resumed",
        "an idle pane and a conversation nobody holds is the ordinary case, \
         and it must stay ordinary: {out:?}"
    );
}
