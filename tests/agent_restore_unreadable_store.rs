//! A restore that cannot read the agent's store must not report success over
//! a bare shell.
//!
//! # The defect this test exists for
//!
//! The freshness lookup that decides whether a conversation is recent enough
//! to auto-resume went through `.ok()`:
//!
//! ```ignore
//! let last_active = adapter.discover().ok().and_then(...)
//! ```
//!
//! so "this conversation was last touched three days ago, leave it for a
//! human" and "osm could not read your conversations at all" produced the same
//! outcome: the binding was silently skipped, nothing was reported, and the
//! restore called itself `succeeded`. Success retires the source snapshot —
//! the only record of which conversation belonged in that pane — and the user
//! is left with a shell and no way back.
//!
//! # The fixture
//!
//! `~/.claude/projects` is a *file*. `read_dir` then fails with `ENOTDIR`,
//! which is a genuine "cannot look" for every user, including root — `chmod
//! 000` would not do, because CI runs as root, where mode bits are advisory
//! and the test would pass by succeeding rather than by failing correctly.
//!
//! One test per file: `$OSM_CLAUDE_HOME` and `$XDG_CONFIG_HOME` are
//! process-global.

mod common;

use osm::agent::AgentAdapter;
use osm::tmux::Tmux;
use std::time::Duration;

struct Server(Tmux);
impl Drop for Server {
    fn drop(&mut self) {
        common::shutdown(&self.0);
    }
}

fn sock(label: &str) -> String {
    format!("osm-unreadable-{}-{}", label, std::process::id())
}

const ID: &str = "0cfebf91-81c0-43d5-af63-c9fe7e844b08";

#[test]
fn a_store_that_cannot_be_read_makes_the_restore_partial_not_succeeded() {
    let tmp = tempfile::tempdir().unwrap();
    let home = common::claude_fixture(tmp.path(), ID);
    let bin = common::stub_agent(tmp.path());

    let config_home = tmp.path().join("config");
    std::fs::create_dir_all(config_home.join("osm")).unwrap();
    std::fs::write(
        config_home.join("osm/config.toml"),
        "[agents]\nenabled = [\"claude\"]\nauto_resume = true\nauto_resume_max_age_mins = 30\n",
    )
    .unwrap();
    std::env::set_var(
        "PATH",
        format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        ),
    );
    std::env::set_var("OSM_CLAUDE_HOME", &home);
    std::env::set_var("XDG_CONFIG_HOME", &config_home);
    common::tmux_conf_with_path(&config_home, &bin);

    let shell_dir = tmp.path().join("shell");
    let agent_dir = tmp.path().join("agent");
    for d in [&shell_dir, &agent_dir] {
        std::fs::create_dir_all(d).unwrap();
    }

    let src = Server(Tmux::with_socket(&sock("src")));
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
    let src_agent = src
        .0
        .list_panes()
        .unwrap()
        .into_iter()
        .find(|p| p.cwd == agent_dir.to_str().unwrap())
        .unwrap()
        .id;
    src.0
        .run(&[
            "send-keys",
            "-t",
            &src_agent,
            &format!("claude --resume {ID}"),
            "C-m",
        ])
        .unwrap();
    assert_eq!(
        common::wait_for_pane_cmd(&src.0, &src_agent, "claude", Duration::from_secs(10)),
        "claude",
        "the stub agent never started in the source pane"
    );

    let mut conn = osm::db::open(&tmp.path().join("state.db")).unwrap();
    let snap = osm::capture::snapshot(&mut conn, &src.0, "before-reboot").unwrap();
    let bound: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pane_rows p JOIN window_rows w ON w.row_id = p.window_row_id
             WHERE w.snapshot_id = ?1 AND p.agent_session_id = ?2",
            rusqlite::params![snap, ID],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(bound, 1, "the capture bound the conversation");
    conn.execute(
        "UPDATE snapshots SET boot_id='boot-previous' WHERE id=?1",
        [snap],
    )
    .unwrap();
    drop(src);

    let adapter = osm::agent::claude::Claude::with_home(&home);
    let gone_by = std::time::Instant::now() + Duration::from_secs(10);
    while osm::agent::detect::live_process_ownership(&adapter, ID).unwrap()
        != osm::agent::Liveness::Inactive
        && std::time::Instant::now() < gone_by
    {
        std::thread::sleep(Duration::from_millis(50));
    }

    // The store becomes unreadable between the capture and the restore: an
    // encrypted home not open yet, a network mount not up, a permissions
    // change. `read_dir` on a file fails for everyone, root included.
    std::fs::remove_dir_all(home.join("projects")).unwrap();
    std::fs::write(home.join("projects"), "not a directory\n").unwrap();
    assert!(
        adapter.discover().is_err(),
        "this test's premise: the store cannot be read"
    );

    let dst = Server(Tmux::with_socket(&sock("dst")));
    let report = osm::restore::run_restore(&mut conn, &dst.0, false).unwrap();
    let json = osm::ipc::RestoreJson::from_report(&report);

    assert_eq!(
        report.state, "partial",
        "a restore that could not put the conversation back is not a success \
         (reason={})",
        report.reason
    );
    assert_eq!(json.agents_resumed, 0, "{json:?}");
    assert_eq!(json.agents_failed.len(), 1, "{json:?}");
    assert!(
        json.agents_failed[0].reason.contains(ID),
        "the failure names the conversation that did not come back: {:?}",
        json.agents_failed[0]
    );

    // The snapshot is the only record of that conversation, so it must still
    // be selectable by the next restore — the run that has a chance of
    // finding the store readable again.
    assert!(json.retryable, "{json:?}");
    let state: String = conn
        .query_row("SELECT state FROM snapshots WHERE id = ?1", [snap], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(state, "complete", "the source snapshot was not retired");

    // And no pane is pretending otherwise.
    let panes = dst.0.list_panes().unwrap();
    assert!(
        panes.iter().all(|p| p.cmd != "claude"),
        "nothing was resumed, so nothing may be running: {panes:?}"
    );
}
