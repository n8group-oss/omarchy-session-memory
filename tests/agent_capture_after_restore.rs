//! The capture that lands *immediately after a restore* must record the
//! conversations against the panes that exist now.
//!
//! # The defect this test exists for
//!
//! A restore rebuilds every pane on a different tmux server, and a tmux
//! server mints its own `%N`. The regression guard in `capture` used to
//! compare **pane ids** between the previous snapshot and the one being
//! written, so after any restore *none* of the previous ids appeared in the
//! new capture, every binding looked lost, and the guard fired
//! unconditionally — on a machine where every conversation had come back
//! perfectly. It then carried the previous map forward keyed by the dead
//! server's pane ids, which match no live pane, so the snapshot published by
//! the restore itself recorded **no agent binding at all**. The one record
//! of which conversation belonged where was destroyed by the very operation
//! that had just put it back.
//!
//! Task 8's unit tests all passed throughout, because they only ever asked
//! the helper function about pane ids. So this test asserts on observable
//! state instead: which conversation the database binds to which *live* pane
//! id, checked against the pane the agent is actually running in.
//!
//! # Why the source server's pane ids are burned first
//!
//! A two-pane window on a fresh server is `%0, %1` on both sides, so a
//! source and destination built the same way collide by coincidence and the
//! broken code would look correct. A scratch session is therefore created
//! and killed on the source first, pushing the captured panes to higher
//! ids; the test then asserts outright that the source and destination pane
//! ids differ before it asserts anything about the snapshot.
//!
//! # One test per file
//!
//! `capture` and `restore` run in-process and read `$OSM_CLAUDE_HOME` and
//! `$XDG_CONFIG_HOME`, and the restored panes need the stub agent on
//! `$PATH`. All three are process-global; cargo gives each test binary its
//! own process, so this file holds exactly one test.

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
    format!("osm-capafter-{}-{}", label, std::process::id())
}

/// Unique to this file: a resume refuses when any process on the machine
/// holds the conversation open, so an id shared with another test binary
/// would make this refuse correctly and fail confusingly.
const ID: &str = "0cfebf91-81c0-43d5-af63-c9fe7e844b02";

/// Every `(pane id, conversation)` this snapshot binds.
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

/// The id of the live pane whose working directory is `cwd`.
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
fn a_capture_after_a_restore_binds_the_conversation_to_the_new_pane() {
    let tmp = tempfile::tempdir().unwrap();
    let home = common::claude_fixture(tmp.path(), ID);
    let bin = common::stub_agent(tmp.path());

    // Only the Claude adapter, so this never shells out to whatever
    // `opencode` happens to be installed on the machine running the test.
    let config_home = tmp.path().join("config");
    std::fs::create_dir_all(config_home.join("osm")).unwrap();
    std::fs::write(
        config_home.join("osm/config.toml"),
        "[agents]\nenabled = [\"claude\"]\nauto_resume = true\nauto_resume_max_age_mins = 30\n",
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

    // Two panes, each in a directory of its own, so a pane can be named
    // after the restore without relying on ids or order.
    let shell_dir = tmp.path().join("shell");
    let agent_dir = tmp.path().join("agent");
    std::fs::create_dir_all(&shell_dir).unwrap();
    std::fs::create_dir_all(&agent_dir).unwrap();

    let src = Server(Tmux::with_socket(&sock("src")));

    // Burn the low pane ids on the source, so its captured panes cannot
    // coincide with the ids a freshly built destination hands out. Without
    // this the broken code passes this test by accident.
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
    // Killed only now: it is the *last* session on the server that takes the
    // server down with it, and nothing but `dev` may be captured.
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

    // Simulate the reboot: the snapshot belongs to a previous boot, and the
    // tmux server — with the running agent in it — is gone.
    conn.execute(
        "UPDATE snapshots SET boot_id='boot-previous' WHERE id=?1",
        [snap],
    )
    .unwrap();
    drop(src);

    // And the agent really did go down with it, or `preflight` would refuse
    // the resume as `active_elsewhere` and this test would assert nothing.
    let adapter = osm::agent::claude::Claude::with_home(&home);
    let gone_by = std::time::Instant::now() + Duration::from_secs(10);
    while osm::agent::detect::live_process_ownership(&adapter, ID).unwrap()
        != osm::agent::Liveness::Inactive
        && std::time::Instant::now() < gone_by
    {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        osm::agent::detect::live_process_ownership(&adapter, ID).unwrap(),
        osm::agent::Liveness::Inactive,
        "the simulated reboot left the agent running, so nothing would be resumed"
    );

    let dst = Server(Tmux::with_socket(&sock("dst")));
    let report = osm::restore::run_restore(&mut conn, &dst.0, false).unwrap();
    // The whole report on failure, not just the state: a `partial` says
    // nothing about *which* of half a dozen things went wrong, and this test
    // once failed exactly that way.
    assert_eq!(
        report.state,
        "succeeded",
        "reason={} json={}",
        report.reason,
        serde_json::to_string(&osm::ipc::RestoreJson::from_report(&report)).unwrap()
    );
    assert_eq!(
        osm::ipc::RestoreJson::from_report(&report).agents_resumed,
        1,
        "the conversation was resumed"
    );

    let dst_agent_pane = live_pane_at(&dst.0, &agent_dir);
    assert_eq!(
        common::wait_for_pane_cmd(&dst.0, &dst_agent_pane, "claude", Duration::from_secs(10)),
        "claude",
        "the conversation did not come back in the pane it was captured in"
    );
    assert_ne!(
        dst_agent_pane, src_agent_pane,
        "this test only means something if the destination server numbered \
         the pane differently from the source"
    );

    // The snapshot the restore itself published, in the same transaction
    // that retired the source. It is the only record of the machine's state
    // from here on, so it is the one that must be right.
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
        "the snapshot published by the restore must bind the conversation to \
         the live pane now running it — not to the dead server's pane id, and \
         not to nothing"
    );

    // And an ordinary capture taken afterwards says the same thing, rather
    // than the guard firing a second time and undoing it.
    let after = osm::capture::snapshot(&mut conn, &dst.0, "after-restore").unwrap();
    assert_eq!(
        bindings(&conn, after),
        vec![(dst_agent_pane, ID.to_string())],
        "the next capture keeps the conversation on the live pane"
    );
}
