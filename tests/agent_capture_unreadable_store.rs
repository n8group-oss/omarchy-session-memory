//! A capture that cannot read the agents still records the tmux topology, and
//! does not record "no pane is running an agent".
//!
//! Two opposite mistakes are available here and both are expensive:
//!
//! * treat an unreadable agent home as a fatal capture error, and the user
//!   loses their *tmux* snapshots — the thing this engine exists for — because
//!   of an unrelated problem in `~/.claude`. Every hook capture would fail and
//!   the newest snapshot would age out of retention;
//! * treat it as "no conversations", and the capture writes that over the only
//!   record of which conversation belonged in which pane.
//!
//! So the capture goes ahead, and "detection did not run" is a recorded cause
//! for carrying the previous bindings forward — a fact about this capture, not
//! a ratio inferred from its results.
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

const ID: &str = "0cfebf91-81c0-43d5-af63-c9fe7e844e0c";

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

/// See `tests/agent_capture.rs` for why the stub is a copied shell binary.
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
fn a_capture_that_cannot_read_the_agents_keeps_the_topology_and_the_bindings() {
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

    let t = Tmux::with_socket(&format!("osm-agentunread-{}", std::process::id()));
    t.run(&["new-session", "-d", "-s", "dev", "-c", "/tmp"])
        .unwrap();
    let srv = Server(t);
    let t = &srv.0;

    spawn_stub_claude(t, &bin_dir, &transcript);
    assert!(
        wait_until(Duration::from_secs(5), || pane_cmd(t) == "claude"),
        "stub agent never became the pane's foreground command"
    );

    let mut conn = db::open(&tmp.path().join("state.db")).unwrap();
    let bound = capture::snapshot(&mut conn, t, "bound").unwrap();
    assert_eq!(binding(&conn, bound).1.as_deref(), Some(ID));

    // The store becomes unreadable. `read_dir` on a file fails with ENOTDIR
    // for everyone, root included — `chmod 000` would not, and CI runs as
    // root.
    std::fs::remove_dir_all(claude_home.join("projects")).unwrap();
    std::fs::write(claude_home.join("projects"), "not a directory\n").unwrap();

    let after = capture::snapshot(&mut conn, t, "store-unreadable")
        .expect("an unreadable agent home must not cost the user their tmux snapshot");
    assert_ne!(after, bound, "a fresh snapshot was written");

    // The topology is there in full.
    let panes: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pane_rows p JOIN window_rows w ON w.row_id = p.window_row_id
             WHERE w.snapshot_id = ?1",
            [after],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(panes, 1, "the pane was recorded");

    // And so is the binding: this capture had nothing to say about it, which
    // is not the same as saying there is none.
    assert_eq!(
        binding(&conn, after).1.as_deref(),
        Some(ID),
        "a capture that could not look must not record that it looked and found \
         nothing"
    );

    std::env::remove_var("OSM_CLAUDE_HOME");
}
