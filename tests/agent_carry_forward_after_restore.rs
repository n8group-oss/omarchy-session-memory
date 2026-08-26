//! When the guard *does* fire on a rebuilt server, what it carries forward
//! has to land on a pane that exists.
//!
//! # The case
//!
//! This is the reboot window the guard was written for, in its real setting:
//! the machine has restarted, the sessions are back, and the panes are still
//! bare shells because no conversation has been resumed into them yet. A
//! capture taken right there sees no agents at all, the guard correctly
//! refuses to write that down, and carries the previous snapshot's bindings
//! forward.
//!
//! The previous snapshot's bindings are keyed by the pane ids of a tmux
//! server that no longer exists. Carried forward verbatim they name nothing:
//! the snapshot comes out with no binding at all, which is precisely the
//! loss the guard exists to prevent — it just takes a longer route to it.
//! Worse, a new server that happened to reuse one of those numbers would
//! have someone else's conversation bound to it.
//!
//! So the carried bindings are re-placed by the identity a restore actually
//! preserves — session name, window index, pane index — and this test
//! asserts the observable result: the conversation is bound to the *live*
//! pane in the directory it was captured in.
//!
//! `auto_resume` is off here so the restored panes are deterministically
//! shells; that is the same state a capture racing an in-flight resume
//! observes, without having to win a race to observe it.
//!
//! One test per file: `capture` and `restore` run in-process and read
//! process-global `$OSM_CLAUDE_HOME` and `$XDG_CONFIG_HOME`.

mod common;

use osm::tmux::Tmux;
use std::time::Duration;

struct Server(Tmux);
impl Drop for Server {
    fn drop(&mut self) {
        common::shutdown(&self.0);
    }
}

fn sock(label: &str) -> String {
    format!("osm-carryfwd-{}-{}", label, std::process::id())
}

/// Unique to this file, for the same reason as every other agent suite: a
/// conversation id shared with another test binary makes a resume refuse as
/// active elsewhere.
const ID: &str = "0cfebf91-81c0-43d5-af63-c9fe7e844b03";

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

fn live_pane_at(t: &Tmux, cwd: &std::path::Path) -> String {
    let want = cwd.to_str().unwrap();
    let panes = t.list_panes().unwrap();
    panes
        .iter()
        .find(|p| p.cwd == want)
        .unwrap_or_else(|| panic!("no live pane at {want}: {panes:?}"))
        .id
        .clone()
}

#[test]
fn a_carried_binding_lands_on_the_live_pane_that_took_the_captured_ones_place() {
    let tmp = tempfile::tempdir().unwrap();
    let home = common::claude_fixture(tmp.path(), ID);
    let bin = common::stub_agent(tmp.path());

    let config_home = tmp.path().join("config");
    std::fs::create_dir_all(config_home.join("osm")).unwrap();
    std::fs::write(
        config_home.join("osm/config.toml"),
        "[agents]\nenabled = [\"claude\"]\nauto_resume = false\n",
    )
    .unwrap();

    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    std::env::set_var("PATH", &path);
    std::env::set_var("OSM_CLAUDE_HOME", &home);
    std::env::set_var("XDG_CONFIG_HOME", &config_home);
    // Exporting PATH above is *not* enough for the panes: tmux starts
    // their shells as login shells, and Debian's /etc/profile assigns PATH
    // rather than prepending to it. See `common::tmux_conf_with_path`.
    common::tmux_conf_with_path(&config_home, &bin);

    let shell_dir = tmp.path().join("shell");
    let agent_dir = tmp.path().join("agent");
    std::fs::create_dir_all(&shell_dir).unwrap();
    std::fs::create_dir_all(&agent_dir).unwrap();

    let src = Server(Tmux::with_socket(&sock("src")));
    // Push the captured pane ids clear of the numbers a fresh destination
    // server hands out, so a carried binding that kept the old id cannot
    // match a live pane by coincidence.
    src.0
        .run(&["new-session", "-d", "-s", "scratch", "-c", "/tmp"])
        .unwrap();
    for _ in 0..3 {
        src.0.run(&["split-window", "-t", "=scratch:"]).unwrap();
    }
    src.0
        .run(&[
            "new-session",
            "-d",
            "-s",
            "dev",
            "-n",
            "code",
            "-c",
            shell_dir.to_str().unwrap(),
            "-x",
            "200",
            "-y",
            "50",
        ])
        .unwrap();
    src.0
        .run(&[
            "split-window",
            "-t",
            "=dev:code",
            "-c",
            agent_dir.to_str().unwrap(),
        ])
        .unwrap();
    // Only now: killing the last session would take the server with it.
    src.0.run(&["kill-session", "-t", "=scratch"]).unwrap();

    let src_agent_pane = live_pane_at(&src.0, &agent_dir);
    src.0
        .run(&[
            "send-keys",
            "-t",
            &src_agent_pane,
            &format!("claude --resume {ID}"),
            "C-m",
        ])
        .unwrap();
    assert_eq!(
        common::wait_for_pane_cmd(&src.0, &src_agent_pane, "claude", Duration::from_secs(10)),
        "claude",
        "the stub agent never started in the source pane"
    );

    let mut conn = osm::db::open(&tmp.path().join("state.db")).unwrap();
    let snap = osm::capture::snapshot(&mut conn, &src.0, "before-reboot").unwrap();
    assert_eq!(
        bindings(&conn, snap),
        vec![(src_agent_pane.clone(), ID.to_string())],
        "the source capture binds the conversation to the pane running it"
    );

    conn.execute(
        "UPDATE snapshots SET boot_id='boot-previous' WHERE id=?1",
        [snap],
    )
    .unwrap();
    drop(src);

    let dst = Server(Tmux::with_socket(&sock("dst")));
    let report = osm::restore::run_restore(&mut conn, &dst.0, false).unwrap();
    assert_eq!(report.state, "succeeded", "reason={}", report.reason);

    let dst_agent_pane = live_pane_at(&dst.0, &agent_dir);
    assert_ne!(
        dst_agent_pane, src_agent_pane,
        "this test only means something if the destination server numbered \
         the pane differently from the source"
    );
    // Nothing was resumed, so the pane really is the bare shell the guard is
    // there to keep from erasing the record.
    assert_eq!(
        common::wait_for_pane_cmd(&dst.0, &dst_agent_pane, "bash", Duration::from_secs(10)),
        "bash",
        "auto_resume is off, so the restored pane must be an idle shell"
    );

    // The snapshot the restore published: the guard fired (no agent is
    // running anywhere), so this is the carried map, and it must name the
    // pane that now holds the captured one's place.
    let published: i64 = conn
        .query_row(
            "SELECT id FROM snapshots WHERE state='complete' ORDER BY id DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_ne!(published, snap, "the restore published a fresh snapshot");
    assert_eq!(
        bindings(&conn, published),
        vec![(dst_agent_pane.clone(), ID.to_string())],
        "a carried binding must name a pane that exists, so it can still be \
         acted on — not the dead server's pane id, and not nothing"
    );

    // And it survives the next capture too, rather than decaying one
    // generation later.
    let after = osm::capture::snapshot(&mut conn, &dst.0, "after-restore").unwrap();
    assert_eq!(
        bindings(&conn, after),
        vec![(dst_agent_pane, ID.to_string())],
        "the carried binding must not be lost by the following capture"
    );
}
