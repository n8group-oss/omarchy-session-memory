//! Which conversation a pane is bound to must come from the **agent's own**
//! process lineage, not from anything that happens to be running in the pane.
//!
//! # The defect this test exists for
//!
//! `bind` combined two signals from two unrelated programs. The pane's
//! foreground command being `claude` was worth 0.4; *any* descendant of the
//! pane holding a transcript open was worth 0.5; together they cleared the
//! 0.75 threshold and the binding took the id from whichever transcript the
//! descendant had. A pane running conversation A with a background job tailing
//! conversation B's transcript therefore scored 0.9 and bound to **B** — and
//! everything downstream trusts that: capture writes it into the snapshot, and
//! after a reboot the restore delivers B into the pane that was running A.
//!
//! Anything a person leaves running in a pane could do this: a `tail -f` on a
//! log, an editor with a transcript open, a second agent's watcher.
//!
//! # One test, in phases
//!
//! `bind` reads the open descriptors of the probe's *process*, and the probe
//! here is this test binary. Two tests holding transcripts open at once would
//! each see the other's, so this file holds one test that runs its cases in
//! sequence, each cleaning up before the next begins.

use osm::agent::detect::{self, PaneProbe};
use osm::agent::AgentAdapter;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const A: &str = "0cfebf91-81c0-43d5-af63-c9fe7e844d01";
const B: &str = "0cfebf91-81c0-43d5-af63-c9fe7e844d02";
/// Never written into the fixture home: a name in the right shape, and
/// nothing else.
const LOOKALIKE: &str = "0cfebf91-81c0-43d5-af63-c9fe7e844d03";

fn fixture(home: &Path, id: &str) -> std::path::PathBuf {
    let path = home.join("projects/-tmp").join(format!("{id}.jsonl"));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "{\"cwd\":\"/tmp\"}\n").unwrap();
    path
}

/// A child holding `transcript` open, running under `argv0`.
///
/// `arg0` is what makes the difference the fix turns on: a child called
/// `claude` is the agent's lineage, a child called `sh` is a bystander. It is
/// also exactly what tmux reads for `#{pane_current_command}`, so the two
/// halves of the evidence are judged by the same name.
/// Kills its child on drop, including when an assertion panics.
///
/// Not tidiness: the child inherits the test harness's stdout, so one left
/// running keeps that pipe open and `cargo test` waits on it for ever — a
/// failing assertion would hang the suite instead of reporting. Its stdio is
/// also detached below for the same reason, belt and braces.
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

fn holder(argv0: &str, transcript: &Path) -> Holder {
    use std::os::unix::process::CommandExt;
    let mut cmd = Command::new("/bin/sh");
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd.arg0(argv0).arg("-c").arg(format!(
        // The loop is not decoration. `sh -c 'exec 3<f; sleep 60'` lets dash
        // apply its last-command optimisation and *exec* into `sleep`, which
        // keeps the descriptor but renames the process — so the child stops
        // being called `claude` and the case this test is about disappears.
        "exec 3<{:?}; while :; do sleep 60; done",
        transcript
    ));
    Holder(cmd.spawn().unwrap())
}

/// Wait until `pid` is visibly holding a transcript open, so the assertions
/// below are about `bind`'s rules and not about process startup.
fn wait_until_holding(pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while detect::open_transcripts(pid).is_empty() {
        assert!(
            Instant::now() < deadline,
            "the child never opened its transcript"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn reap(child: Holder) {
    let pid = child.pid();
    drop(child);
    // The fd goes with the process, and the next phase must not see it.
    let deadline = Instant::now() + Duration::from_secs(5);
    while !detect::open_transcripts(pid).is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn probe() -> PaneProbe {
    PaneProbe {
        pane_id: "%1".to_string(),
        pane_pid: std::process::id(),
        cwd: "/tmp".to_string(),
        foreground_cmd: "claude".to_string(),
    }
}

#[test]
fn only_the_agents_own_lineage_says_which_conversation_a_pane_is_running() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("claude");
    let a = fixture(&home, A);
    let b = fixture(&home, B);

    // Aimed at the fixture home explicitly, so nothing here reads the
    // developer's real `~/.claude` or depends on the environment.
    let adapters: Vec<Box<dyn AgentAdapter>> =
        vec![Box::new(osm::agent::claude::Claude::with_home(&home))];
    let prepared = detect::prepare(&adapters).unwrap();
    assert_eq!(prepared[0].index.len(), 2, "both fixtures are discovered");

    // --- Phase 1: a bystander holds B, and nothing is the agent ------------
    //
    // The pane's foreground command says `claude`, and something in the pane
    // has a transcript open — but not the agent, and not this conversation.
    // 0.4 + 0.5 = 0.9 used to bind this pane to B.
    let bystander = holder("sh", &b);
    wait_until_holding(bystander.pid());
    let bound = detect::bind(&probe(), &prepared);
    assert!(
        bound.is_none(),
        "a descriptor held outside the agent's lineage is not evidence of \
         anything, and must not bind the pane: {bound:?}"
    );
    reap(bystander);

    // --- Phase 2: the agent holds A while a bystander holds B --------------
    //
    // Now the evidence for A is real and the evidence for B is noise. The old
    // rule pooled both and saw two conversations, which it called ambiguous
    // and refused — so even where it did not bind the *wrong* one, an
    // unrelated background job could silently unbind a pane that was plainly
    // running an agent.
    let agent = holder("claude", &a);
    let bystander = holder("sh", &b);
    wait_until_holding(agent.pid());
    wait_until_holding(bystander.pid());
    let bound = detect::bind(&probe(), &prepared).expect("the pane is running A");
    assert_eq!(bound.native_id, A, "the agent's own transcript decides");
    assert_eq!(bound.kind, osm::agent::AgentKind::Claude);
    assert!(bound.confidence >= osm::agent::CONFIDENCE_THRESHOLD);
    reap(agent);
    reap(bystander);

    // --- Phase 3: the agent holds a file that merely *looks* like one -------
    //
    // A `.jsonl` whose whole stem is a UUID, held open by the agent itself,
    // but not a file discovery ever found — anyone can create one anywhere.
    // Recognising a transcript by the shape of its name meant this bound the
    // pane to a conversation that does not exist.
    let lookalike = tmp.path().join(format!("{LOOKALIKE}.jsonl"));
    std::fs::write(&lookalike, "{\"cwd\":\"/tmp\"}\n").unwrap();
    let impostor = holder("claude", &lookalike);
    wait_until_holding(impostor.pid());
    let bound = detect::bind(&probe(), &prepared);
    assert!(
        bound.is_none(),
        "a file is a transcript because it *is* one of the files discovery \
         found, not because of what it is called: {bound:?}"
    );
    reap(impostor);
}
