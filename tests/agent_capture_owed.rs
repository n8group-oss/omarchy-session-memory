//! The other half of `tests/agent_capture.rs`: a binding a capture cannot see
//! **is** carried forward when a restore has recorded that it is still owed.
//!
//! This is the reboot window, in the only form that now justifies carrying
//! anything: the restore has rebuilt the pane, the conversation is not back in
//! it yet, and the restore wrote that down (`osm::debt::record`, which is
//! exactly what `run_restore` calls before it delivers anything). A capture
//! landing there must not record "no agent anywhere" over the one map that
//! says where the conversation belongs.
//!
//! The debt is recorded here through the same public call the restore makes,
//! rather than by driving a whole restore, so that this test isolates the
//! carry rule itself — `tests/agent_carry_forward_after_restore.rs` covers it
//! end to end through a real restore.
//!
//! One test per file: `$OSM_CLAUDE_HOME` is process-global.

mod common;

use osm::agent::AgentKind;
use osm::{capture, db, tmux::Tmux};
use std::path::Path;
use std::time::{Duration, Instant};

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

/// A pane whose foreground process is genuinely named `claude` and which holds
/// `transcript` open, so the pane's process tree has a real, live, findable
/// transcript fd — what `detect::bind` looks for. See the identical helper in
/// `tests/agent_capture.rs` for why a copied shell binary is used.
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

fn binding(conn: &rusqlite::Connection, snapshot_id: i64) -> (Option<String>, Option<String>) {
    conn.query_row(
        "SELECT p.agent_kind, p.agent_session_id
         FROM pane_rows p JOIN window_rows w ON w.row_id = p.window_row_id
         WHERE w.snapshot_id = ?1",
        [snapshot_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .unwrap()
}

#[test]
fn a_binding_a_restore_still_owes_is_carried_over_a_capture_that_sees_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let claude_home = tmp.path().join("claude-home");
    let id = "0cfebf91-81c0-43d5-af63-c9fe7e844e01";
    let project_files = claude_home.join("projects/x");
    std::fs::create_dir_all(&project_files).unwrap();
    let transcript = project_files.join(format!("{id}.jsonl"));
    std::fs::write(&transcript, "{\"cwd\":\"/tmp\"}\n").unwrap();

    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();

    // SAFETY: no other test in this binary reads or writes OSM_CLAUDE_HOME.
    std::env::set_var("OSM_CLAUDE_HOME", &claude_home);

    let t = Tmux::with_socket(&format!("osm-agentowed-{}", std::process::id()));
    t.run(&["new-session", "-d", "-s", "dev", "-c", "/tmp"])
        .unwrap();
    let srv = Server(t);
    let t = &srv.0;

    spawn_stub_claude(t, "=dev:", &bin_dir, &transcript);
    assert!(
        wait_until(Duration::from_secs(5), || pane_cmd(t) == "claude"),
        "stub agent never became the pane's foreground command"
    );

    let mut conn = db::open(&tmp.path().join("state.db")).unwrap();
    let bound = capture::snapshot(&mut conn, t, "bound").unwrap();
    assert_eq!(binding(&conn, bound).1.as_deref(), Some(id));

    // What a restore writes before it delivers anything: this snapshot's
    // panes are back and its conversations are not in them yet.
    let boot_id = osm::boot::current_boot_id().unwrap();
    // A restore that put this whole snapshot back: the session it delivered
    // and the captured pane it built. `record` writes debt only for those.
    let delivered_sessions = ["dev"]
        .into_iter()
        .collect::<std::collections::HashSet<_>>();
    let captured_pane = t.list_panes().unwrap()[0].id.clone();
    let restored_panes = [captured_pane.as_str()]
        .into_iter()
        .collect::<std::collections::HashSet<_>>();
    let owed = osm::debt::record(
        &conn,
        bound,
        &boot_id,
        osm::boot::now_epoch(),
        &delivered_sessions,
        &restored_panes,
    )
    .unwrap();
    assert_eq!(owed, 1, "one conversation is owed");

    // The agent is gone — the bare shell every restored pane is until its
    // resume completes.
    t.run(&["send-keys", "-t", "=dev:", "C-c"]).unwrap();
    assert!(
        wait_until(Duration::from_secs(5), || pane_cmd(t) != "claude"),
        "stub agent never exited"
    );

    let during = capture::snapshot(&mut conn, t, "during-resume-window").unwrap();
    assert_eq!(
        binding(&conn, during),
        (
            Some(AgentKind::Claude.as_str().to_string()),
            Some(id.to_string())
        ),
        "a conversation a restore still owes must keep its pane"
    );

    // Once the debt is out of the reboot window it stops being carried: an
    // undetected binding with nothing owing it is simply gone, rather than
    // being re-recorded for the rest of the machine's life.
    conn.execute(
        "UPDATE agent_resume_debt SET recorded_at = ?1",
        [osm::boot::now_epoch() - osm::debt::WINDOW_SECS - 1],
    )
    .unwrap();
    let after = capture::snapshot(&mut conn, t, "after-the-window").unwrap();
    assert_eq!(
        binding(&conn, after),
        (None, None),
        "an expired debt carries nothing"
    );

    std::env::remove_var("OSM_CLAUDE_HOME");
}
