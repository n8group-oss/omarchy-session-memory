//! A symlinked agent home must not make a live conversation look abandoned.
//!
//! # The defect these tests exist for
//!
//! `~/.claude -> /data/claude` is an ordinary arrangement: a home on a bigger
//! filesystem, or a dotfiles checkout. Discovery walks the path it was given
//! and records `<home>/projects/…/<id>.jsonl`; `/proc/<pid>/fd/N` always
//! answers with the path the *kernel* resolved to, so the same descriptor
//! reads `<real>/projects/…/<id>.jsonl (deleted)` once the transcript has been
//! replaced under the running agent.
//!
//! Device and inode never cared about that, which is why identity lives there
//! — but a descriptor whose file has been unlinked has no identity left, and
//! the path is the only handle on it. The comparison was literal, so it missed
//! every time on a symlinked home, and both halves of the failure followed:
//!
//! * `is_active_elsewhere` answered `Inactive` for a conversation a live
//!   process was holding, which is the one answer that lets a second client be
//!   sent into it;
//! * a capture could not tell that the pane's ownership was merely
//!   unidentifiable, recorded the pane as holding nothing, and discarded the
//!   only record of which conversation belonged there.
//!
//! Both tests here use a symlinked home; the transcript is opened through the
//! symlink, exactly as an agent started from `~/.claude` opens it. They use
//! different homes and different conversation ids so neither can see the
//! other's descriptors when this binary runs its tests in parallel.

mod common;

use osm::agent::{AgentAdapter, Liveness};
use osm::{capture, db, tmux::Tmux};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Server(Tmux);
impl Drop for Server {
    fn drop(&mut self) {
        common::shutdown(&self.0);
    }
}

const LIVENESS_ID: &str = "0cfebf91-81c0-43d5-af63-c9fe7e844d01";
const CAPTURE_ID: &str = "0cfebf91-81c0-43d5-af63-c9fe7e844d02";

/// A real directory, and a symlink standing in for the home the user (and
/// osm) actually names. Returns the symlink, which is the only path either
/// side is ever told about.
fn symlinked_home(tmp: &Path, name: &str, id: &str) -> (PathBuf, PathBuf) {
    let real = tmp.join(format!("{name}-real"));
    let transcript = real.join("projects/-tmp").join(format!("{id}.jsonl"));
    std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    std::fs::write(&transcript, "{\"cwd\":\"/tmp\"}\n").unwrap();
    let link = tmp.join(name);
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let through_link = link.join("projects/-tmp").join(format!("{id}.jsonl"));
    (link, through_link)
}

/// Write a new file beside `path` and rename it over the top — how a
/// transcript is replaced without ever being left half-written. The inode
/// anyone had open is unlinked by it.
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
    cmd.arg0("claude").arg("-c").arg(format!(
        "exec 3<{:?}; while :; do sleep 60; done",
        transcript
    ));
    Holder(cmd.spawn().unwrap())
}

fn wait_until(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    loop {
        if f() {
            return true;
        }
        if start.elapsed() > timeout {
            return false;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
}

#[test]
fn a_live_conversation_under_a_symlinked_home_is_never_free_to_resume() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, transcript) = symlinked_home(tmp.path(), "claude-liveness", LIVENESS_ID);
    let adapter = osm::agent::claude::Claude::with_home(&home);

    // The agent opens its transcript through the symlink, which is the path it
    // was configured with — and the kernel records the resolved one.
    let child = holder(&transcript);
    assert!(
        wait_until(Duration::from_secs(5), || {
            !osm::agent::detect::open_transcripts(child.pid()).is_empty()
        }),
        "the holder never opened its transcript"
    );
    assert_eq!(
        adapter.is_active_elsewhere(LIVENESS_ID).unwrap(),
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
        adapter.is_active_elsewhere(LIVENESS_ID).unwrap(),
        Liveness::Unknown,
        "a live process is holding this conversation through a descriptor the \
         kernel names by its resolved path; missing that because discovery \
         walked the symlink is what lets a second client into it"
    );
}

fn chmod_exec(path: &Path) {
    let mut perm = std::fs::metadata(path).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perm, 0o755);
    std::fs::set_permissions(path, perm).unwrap();
}

/// See `tests/agent_capture.rs` for why the stub is a copied shell binary: the
/// pane's foreground command has to *be* `claude`, which is what
/// `#{pane_current_command}` reads.
fn spawn_stub_claude(t: &Tmux, bin_dir: &Path, transcript: &Path) {
    let sh = ["/bin/sh", "/usr/bin/sh"]
        .into_iter()
        .find(|p| Path::new(p).exists())
        .expect("a sh binary");
    let claude_bin = bin_dir.join("claude");
    std::fs::copy(sh, &claude_bin).unwrap();
    chmod_exec(&claude_bin);
    let runner = bin_dir.join("run.sh");
    std::fs::write(
        &runner,
        format!(
            "#!/bin/sh\nexec {:?} -c 'exec 3<\"$1\"; read line' -- {:?}\n",
            claude_bin, transcript
        ),
    )
    .unwrap();
    chmod_exec(&runner);
    t.run(&[
        "send-keys",
        "-t",
        "=dev:",
        &format!("{}", runner.display()),
        "C-m",
    ])
    .unwrap();
}

fn bindings(conn: &rusqlite::Connection, snapshot_id: i64) -> Vec<(String, String)> {
    conn.prepare(
        "SELECT p.tmux_pane_id, p.agent_session_id
         FROM pane_rows p JOIN window_rows w ON w.row_id = p.window_row_id
         WHERE w.snapshot_id = ?1 AND p.agent_session_id IS NOT NULL
         ORDER BY p.tmux_pane_id",
    )
    .unwrap()
    .query_map([snapshot_id], |r| Ok((r.get(0)?, r.get(1)?)))
    .unwrap()
    .collect::<Result<_, _>>()
    .unwrap()
}

#[test]
fn a_pane_under_a_symlinked_home_keeps_its_binding_when_its_transcript_is_replaced() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, transcript) = symlinked_home(tmp.path(), "claude-capture", CAPTURE_ID);
    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();

    // SAFETY: the other test in this binary uses `Claude::with_home` and never
    // reads this variable.
    std::env::set_var("OSM_CLAUDE_HOME", &home);

    let t = Tmux::with_socket(&format!("osm-symlinkhome-{}", std::process::id()));
    t.run(&["new-session", "-d", "-s", "dev", "-n", "code", "-c", "/tmp"])
        .unwrap();
    let srv = Server(t);
    let t = &srv.0;
    let pane = t.list_panes().unwrap()[0].id.clone();

    spawn_stub_claude(t, &bin_dir, &transcript);
    assert!(
        wait_until(Duration::from_secs(5), || t.list_panes().unwrap()[0].cmd
            == "claude"),
        "the conversation never started"
    );

    let mut conn = db::open(&tmp.path().join("state.db")).unwrap();
    let first = capture::snapshot(&mut conn, t, "running").unwrap();
    assert_eq!(
        bindings(&conn, first),
        vec![(pane.clone(), CAPTURE_ID.to_string())],
        "the pane is running the conversation, so the first snapshot must say so"
    );

    replace_atomically(&transcript);
    assert!(
        wait_until(Duration::from_secs(5), || t.list_panes().unwrap()[0].cmd
            == "claude"),
        "the agent must still be the pane's foreground command"
    );

    let second = capture::snapshot(&mut conn, t, "after-replacement").unwrap();
    assert_eq!(
        bindings(&conn, second),
        vec![(pane, CAPTURE_ID.to_string())],
        "the agent is still running this conversation through a descriptor \
         named by its resolved path; a home reached through a symlink must not \
         turn that into a pane that holds nothing"
    );

    std::env::remove_var("OSM_CLAUDE_HOME");
}
