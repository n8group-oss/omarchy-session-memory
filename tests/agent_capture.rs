//! Recording agent bindings during capture, and what may — and may not —
//! survive a capture that sees no agent running.
//!
//! # What this file pins
//!
//! A capture writes down what it detects. It may *add* a binding it could not
//! detect only when something on the machine has recorded that the
//! conversation is still owed: a restore has put the pane back and the
//! conversation is not in it yet (see `osm::debt`, and
//! `tests/agent_capture_owed.rs` for that case end to end).
//!
//! With nothing owed, an agent that is no longer running is an agent that is
//! no longer running, and the snapshot says so. The rule this replaced said
//! the opposite: it compared the *set* of bound conversations against the
//! previous snapshot's and discarded the whole new map whenever more than half
//! of them had gone — so a user who closed their only tracked conversation
//! made every subsequent capture re-record it, indefinitely, and a reboot then
//! resumed it into whatever pane had taken its place. That is what the test
//! below now forbids.

mod common;

use osm::agent::AgentKind;
use osm::{capture, db, tmux::Tmux};
use std::path::Path;
use std::time::{Duration, Instant};

// --- End-to-end: the guard protecting a real capture, not just the helper.

struct Server(Tmux);
impl Drop for Server {
    fn drop(&mut self) {
        common::shutdown(&self.0);
    }
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

fn chmod_exec(path: &Path) {
    let mut perm = std::fs::metadata(path).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perm, 0o755);
    std::fs::set_permissions(path, perm).unwrap();
}

/// A pane whose foreground process is genuinely named `claude` (tmux reports
/// the *interpreter's* basename for a `#!/bin/sh` script, never the
/// filename, so this copies a real shell binary under that name and execs
/// into it — the same technique `tests/hostile_names.rs` uses for a command
/// name containing the field separator) and which holds `transcript` open on
/// fd 3, so the pane's process tree has a real, live, findable transcript
/// fd — exactly what `detect::bind` looks for.
fn spawn_stub_claude(t: &Tmux, pane_target: &str, bin_dir: &Path, transcript: &Path) {
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
        pane_target,
        &format!("{}", runner.display()),
        "C-m",
    ])
    .unwrap();
}

fn pane_cmd(t: &Tmux) -> String {
    t.list_panes().unwrap()[0].cmd.clone()
}

fn snapshot_binding(
    conn: &rusqlite::Connection,
    snapshot_id: i64,
) -> (Option<String>, Option<String>, Option<String>) {
    conn.query_row(
        "SELECT p.agent_kind, p.agent_session_id, p.restore_policy
         FROM pane_rows p JOIN window_rows w ON w.row_id = p.window_row_id
         WHERE w.snapshot_id = ?1",
        [snapshot_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .unwrap()
}

/// A conversation the user closed is not resurrected.
///
/// A real capture with a live agent bound, then a second real capture after
/// the agent process is gone and **nothing has recorded that anyone owes it**.
/// The second snapshot must record the pane as holding no conversation.
///
/// This is the exact case the old set-comparison rule got wrong, and it got it
/// wrong permanently: having carried the closed conversation forward once, the
/// next capture saw the same 100% "loss" and carried it again. The record the
/// user could not get rid of was then resumed into whatever had taken that
/// pane's place after a reboot.
#[test]
fn a_conversation_that_is_gone_with_nothing_owing_it_is_not_carried_forward() {
    let tmp = tempfile::tempdir().unwrap();
    let claude_home = tmp.path().join("claude-home");
    let id = "0cfebf91-81c0-43d5-af63-c9fe7e844ede";
    let project_dir = "/tmp";
    let project_files = claude_home.join("projects/x");
    std::fs::create_dir_all(&project_files).unwrap();
    let transcript = project_files.join(format!("{id}.jsonl"));
    std::fs::write(&transcript, format!("{{\"cwd\":\"{project_dir}\"}}\n")).unwrap();

    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();

    // SAFETY: no other test in this binary reads or writes OSM_CLAUDE_HOME.
    std::env::set_var("OSM_CLAUDE_HOME", &claude_home);

    let t = Tmux::with_socket(&format!("osm-agentcap-{}", std::process::id()));
    t.run(&["new-session", "-d", "-s", "dev", "-c", project_dir])
        .unwrap();
    let srv = Server(t);
    let t = &srv.0;

    spawn_stub_claude(t, "=dev:", &bin_dir, &transcript);
    assert!(
        wait_until(Duration::from_secs(5), || pane_cmd(t) == "claude"),
        "stub agent never became the pane's foreground command (got {:?})",
        pane_cmd(t)
    );

    let db_path = tmp.path().join("state.db");
    let mut conn = db::open(&db_path).unwrap();

    let bound_snapshot = capture::snapshot(&mut conn, t, "bound").unwrap();
    let (kind, session, policy) = snapshot_binding(&conn, bound_snapshot);
    assert_eq!(kind.as_deref(), Some(AgentKind::Claude.as_str()));
    assert_eq!(session.as_deref(), Some(id));
    assert_eq!(policy.as_deref(), Some("agent_resume"));

    // Kill the stub agent (SIGINT the foreground process group) so the pane
    // falls back to its idle shell, exactly like a reboot leaves it.
    t.run(&["send-keys", "-t", "=dev:", "C-c"]).unwrap();
    assert!(
        wait_until(Duration::from_secs(5), || pane_cmd(t) != "claude"),
        "stub agent never exited"
    );

    // Nothing on this machine owes this conversation: no restore has run, so
    // `agent_resume_debt` is empty.
    let owed: i64 = conn
        .query_row("SELECT COUNT(*) FROM agent_resume_debt", [], |r| r.get(0))
        .unwrap();
    assert_eq!(owed, 0, "this test's premise: nothing is owed");

    let unbound_snapshot = capture::snapshot(&mut conn, t, "unbound").unwrap();
    let (kind, session, policy) = snapshot_binding(&conn, unbound_snapshot);
    assert_eq!(
        kind, None,
        "with nothing owing it, a conversation that is no longer running must \
         not be re-recorded: {session:?}"
    );
    assert_eq!(session, None);
    assert_eq!(
        policy.as_deref(),
        Some("shell"),
        "the pane holds a shell, and the snapshot says so"
    );

    // And it stays gone, rather than reappearing one capture later.
    let again = capture::snapshot(&mut conn, t, "unbound-again").unwrap();
    assert_eq!(snapshot_binding(&conn, again).1, None);

    std::env::remove_var("OSM_CLAUDE_HOME");
}
