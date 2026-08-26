//! A capture that cannot identify a pane's transcript must keep the binding it
//! had, not record that the pane holds nothing.
//!
//! # The defect this test exists for
//!
//! An agent replaces its transcript by renaming a new file over the old one
//! and goes on writing to the file it already had open. Discovery indexes the
//! replacement; the agent's descriptor points at an unlinked inode reported as
//! `…/<id>.jsonl (deleted)`. Matching descriptors to conversations by device
//! and inode — which is what makes a binding evidence rather than a guess —
//! then finds nothing, so the pane scored 0.4 for its foreground command
//! alone, fell below the threshold, and was written down as running no
//! conversation at all. The only record of what belonged in that pane was
//! gone, and after a reboot it came back as a bare shell.
//!
//! "osm could not tell" is now kept apart from "there is nothing there": the
//! previous binding stands until ownership can be established again.
//!
//! One test per file: `$OSM_CLAUDE_HOME` is process-global.

mod common;

use osm::{capture, db, tmux::Tmux};
use std::path::Path;
use std::time::{Duration, Instant};

struct Server(Tmux);
impl Drop for Server {
    fn drop(&mut self) {
        common::shutdown(&self.0);
    }
}

const ID: &str = "0cfebf91-81c0-43d5-af63-c9fe7e844e2a";

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
fn a_pane_whose_transcript_was_replaced_keeps_its_binding() {
    let tmp = tempfile::tempdir().unwrap();
    let claude_home = tmp.path().join("claude-home");
    let project_files = claude_home.join("projects/x");
    std::fs::create_dir_all(&project_files).unwrap();
    let transcript = project_files.join(format!("{ID}.jsonl"));
    std::fs::write(&transcript, "{\"cwd\":\"/tmp\"}\n").unwrap();

    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();

    // SAFETY: no other test in this binary reads or writes OSM_CLAUDE_HOME.
    std::env::set_var("OSM_CLAUDE_HOME", &claude_home);

    let t = Tmux::with_socket(&format!("osm-replaced-{}", std::process::id()));
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
        vec![(pane.clone(), ID.to_string())],
        "the pane is running the conversation, so the first snapshot must say so"
    );

    // The agent rewrites its transcript: a new file, renamed over the old one.
    // Its own descriptor now names an inode with no name.
    let staging = project_files.join(format!("{ID}.jsonl.new"));
    std::fs::write(&staging, "{\"cwd\":\"/tmp\",\"replaced\":true}\n").unwrap();
    std::fs::rename(&staging, &transcript).unwrap();
    assert!(
        wait_until(Duration::from_secs(5), || t.list_panes().unwrap()[0].cmd
            == "claude"),
        "the agent must still be the pane's foreground command"
    );

    let second = capture::snapshot(&mut conn, t, "after-replacement").unwrap();
    assert_eq!(
        bindings(&conn, second),
        vec![(pane, ID.to_string())],
        "the agent is still running this conversation; a descriptor osm cannot \
         identify is not evidence that the pane holds nothing"
    );

    std::env::remove_var("OSM_CLAUDE_HOME");
}
