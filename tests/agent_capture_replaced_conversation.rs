//! Exiting one conversation and starting another **in the same pane** must
//! record the new one.
//!
//! # The defect this test exists for
//!
//! The rule that protected the reboot window compared the *set* of bound
//! conversations with the previous capture's and refused the new map when more
//! than half of them had gone. The most ordinary thing a person does with a
//! coding agent — quit A, start B, same pane — reads as 100% loss under that
//! measure. So the correct fresh binding `{B}` was thrown away and A was
//! carried onto B's pane instead; the next capture saw the same thing and did
//! it again. After a reboot, A was resumed into B's pane.
//!
//! The fresh binding is now never displaced by carried data: carrying only
//! ever fills a pane the capture detected nothing on. This test proves it in
//! the harshest arrangement — A is not merely absent, it is genuinely *owed*
//! by a recorded restore debt, which is the only condition under which
//! carrying is allowed at all. Even then B wins, because B is there.
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

const A: &str = "0cfebf91-81c0-43d5-af63-c9fe7e844e0a";
const B: &str = "0cfebf91-81c0-43d5-af63-c9fe7e844e0b";

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
/// `label` distinguishes the two runners so the second conversation is
/// started by a script of its own.
fn spawn_stub_claude(t: &Tmux, bin_dir: &Path, transcript: &Path, label: &str) {
    let sh = ["/bin/sh", "/usr/bin/sh"]
        .into_iter()
        .find(|p| Path::new(p).exists())
        .expect("a sh binary");
    let claude_bin = bin_dir.join("claude");
    if !claude_bin.exists() {
        std::fs::copy(sh, &claude_bin).unwrap();
        chmod_exec(&claude_bin);
    }
    let runner = bin_dir.join(format!("run-{label}.sh"));
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

/// Every `(pane id, conversation)` the snapshot binds — every one, so a
/// conversation smuggled onto some *other* pane would show up here too.
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
fn a_new_conversation_in_the_same_pane_replaces_the_one_that_left() {
    let tmp = tempfile::tempdir().unwrap();
    let claude_home = tmp.path().join("claude-home");
    let project_files = claude_home.join("projects/x");
    std::fs::create_dir_all(&project_files).unwrap();
    let transcript_a = project_files.join(format!("{A}.jsonl"));
    let transcript_b = project_files.join(format!("{B}.jsonl"));
    std::fs::write(&transcript_a, "{\"cwd\":\"/tmp\"}\n").unwrap();
    std::fs::write(&transcript_b, "{\"cwd\":\"/tmp\"}\n").unwrap();

    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();

    // SAFETY: no other test in this binary reads or writes OSM_CLAUDE_HOME.
    std::env::set_var("OSM_CLAUDE_HOME", &claude_home);

    let t = Tmux::with_socket(&format!("osm-agentswap-{}", std::process::id()));
    t.run(&["new-session", "-d", "-s", "dev", "-c", "/tmp"])
        .unwrap();
    let srv = Server(t);
    let t = &srv.0;
    let pane = t.list_panes().unwrap()[0].id.clone();

    spawn_stub_claude(t, &bin_dir, &transcript_a, "a");
    assert!(
        wait_until(Duration::from_secs(5), || pane_cmd(t) == "claude"),
        "conversation A never started"
    );

    let mut conn = db::open(&tmp.path().join("state.db")).unwrap();
    let first = capture::snapshot(&mut conn, t, "conversation-a").unwrap();
    assert_eq!(bindings(&conn, first), vec![(pane.clone(), A.to_string())]);

    // The harshest setting for the rule: A is genuinely owed, which is the
    // only condition under which a binding may be carried at all.
    let boot_id = osm::boot::current_boot_id().unwrap();
    let delivered_sessions = ["dev"]
        .into_iter()
        .collect::<std::collections::HashSet<_>>();
    let restored_panes = [pane.as_str()]
        .into_iter()
        .collect::<std::collections::HashSet<_>>();
    osm::debt::record(
        &conn,
        first,
        &boot_id,
        osm::boot::now_epoch(),
        &delivered_sessions,
        &restored_panes,
    )
    .unwrap();

    // The user quits A and starts B in the same pane.
    t.run(&["send-keys", "-t", "=dev:", "C-c"]).unwrap();
    assert!(
        wait_until(Duration::from_secs(5), || pane_cmd(t) != "claude"),
        "conversation A never exited"
    );
    spawn_stub_claude(t, &bin_dir, &transcript_b, "b");
    assert!(
        wait_until(Duration::from_secs(5), || pane_cmd(t) == "claude"),
        "conversation B never started"
    );

    let second = capture::snapshot(&mut conn, t, "conversation-b").unwrap();
    assert_eq!(
        bindings(&conn, second),
        vec![(pane.clone(), B.to_string())],
        "the pane is running B, so the snapshot must say B — a fresh binding \
         is never displaced by a carried one"
    );

    // And it does not drift back: the previous snapshot now says B too, so
    // there is nothing left to carry, but a rule that re-derived A from the
    // one before it would show up here.
    let third = capture::snapshot(&mut conn, t, "still-b").unwrap();
    assert_eq!(bindings(&conn, third), vec![(pane, B.to_string())]);

    std::env::remove_var("OSM_CLAUDE_HOME");
}
