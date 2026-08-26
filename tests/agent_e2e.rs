//! Capture a pane running an agent, simulate a reboot, restore, and require
//! the conversation to come back **in the pane it came from**.
//!
//! # Why this test exists in this shape
//!
//! `restore::resume_agents` has no captured→live pane id mapping to work
//! from: the destination tmux server mints its own `%N`. It matches the
//! window's live panes to the captured ones *positionally, in creation
//! order* — the same invariant `fill_window` relies on when it recreates
//! them. Nothing exercised that against a real multi-pane window, and if it
//! is wrong the resume lands in the wrong pane: someone else's conversation
//! appearing in a pane you were working in, which is the worst thing this
//! feature can do.
//!
//! So the fixture is a four-pane window whose agent is in the **last** pane,
//! and the assertion is about *which* pane came back running it — identified
//! by its captured working directory, which is independent of both pane ids
//! and pane order. A positional match that is off by any amount fails here.
//!
//! # Why a stub agent
//!
//! `tests/common::stub_agent` stands in for Claude Code, so this runs
//! without a real agent installed (and without one talking to a real API) —
//! which is also what makes it safe in CI. It is faithful in the two ways
//! this machinery observes an agent: it holds the conversation's transcript
//! open, and it runs under the name `claude`. Handed the wrong conversation
//! id it exits instead of running, so a resume that delivered the wrong id
//! could not pass this test either.
//!
//! # One test, and why it may set process-wide environment
//!
//! `capture` and `restore` run in-process here and read `$OSM_CLAUDE_HOME`
//! and `$XDG_CONFIG_HOME`; the restored panes are created by tmux and need
//! the stub on `$PATH`. Both are process-global. This file therefore holds
//! exactly one test — cargo gives each test binary its own process — so
//! nothing else is running when it mutates them.

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
    format!("osm-e2eagent-{}-{}", label, std::process::id())
}

/// Unique to this file: a resume refuses when *any* process on the machine
/// holds the conversation open (`is_active_elsewhere`), so an id shared with
/// another test binary would make this refuse correctly and fail confusingly.
const ID: &str = "0cfebf91-81c0-43d5-af63-c9fe7e844b01";

/// `(pane_current_path, pane_current_command)` for every pane of
/// `dev:code`, in the order tmux lists them.
fn panes_by_cwd(t: &Tmux) -> Vec<(String, String)> {
    t.list_panes()
        .unwrap()
        .into_iter()
        .map(|p| (p.cwd, p.cmd))
        .collect()
}

fn cmd_in(panes: &[(String, String)], cwd: &std::path::Path) -> String {
    let want = cwd.to_str().unwrap();
    panes
        .iter()
        .find(|(path, _)| path == want)
        .unwrap_or_else(|| panic!("no pane at {want}: {panes:?}"))
        .1
        .clone()
}

#[test]
fn the_conversation_comes_back_in_the_pane_it_was_captured_in() {
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

    // The stub goes *first* on PATH: the pane must run it and not a real
    // `claude` that may be installed on this machine.
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

    // Four panes, each in a directory of its own so a pane can be
    // identified after the restore without relying on ids or order.
    let dirs: Vec<std::path::PathBuf> = (0..4)
        .map(|i| {
            let d = tmp.path().join(format!("p{i}"));
            std::fs::create_dir_all(&d).unwrap();
            d
        })
        .collect();

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
            dirs[0].to_str().unwrap(),
            "-x",
            "200",
            "-y",
            "50",
        ])
        .unwrap();
    for dir in &dirs[1..] {
        src.0
            .run(&[
                "split-window",
                "-t",
                "dev:code",
                "-c",
                dir.to_str().unwrap(),
            ])
            .unwrap();
    }
    let src_panes = src.0.list_panes().unwrap();
    assert_eq!(src_panes.len(), 4, "four panes: {src_panes:?}");

    // The agent goes in the *last* pane, never the first: a resume that
    // always targeted the window's first pane would otherwise pass.
    let agent_dir = dirs.last().unwrap().clone();
    let agent_pane = src_panes
        .iter()
        .find(|p| p.cwd == agent_dir.to_str().unwrap())
        .expect("a pane in the last directory")
        .id
        .clone();
    assert_ne!(
        agent_pane, src_panes[0].id,
        "the conversation must not be in the window's first pane"
    );
    src.0
        .run(&[
            "send-keys",
            "-t",
            &agent_pane,
            &format!("claude --resume {ID}"),
            "C-m",
        ])
        .unwrap();
    assert_eq!(
        common::wait_for_pane_cmd(&src.0, &agent_pane, "claude", Duration::from_secs(10)),
        "claude",
        "the stub agent never started in the source pane"
    );

    let mut conn = osm::db::open(&tmp.path().join("state.db")).unwrap();
    let snap = osm::capture::snapshot(&mut conn, &src.0, "e2e-agent").unwrap();

    // Capture bound exactly the agent's pane, and bound it to this
    // conversation — not to some other pane of the same window.
    let bound: Vec<(String, String, Option<String>)> = conn
        .prepare(
            "SELECT p.tmux_pane_id, p.agent_session_id, p.restore_policy
             FROM pane_rows p
             JOIN window_rows w ON w.row_id = p.window_row_id
             WHERE w.snapshot_id = ?1 AND p.agent_session_id IS NOT NULL",
        )
        .unwrap()
        .query_map([snap], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(bound.len(), 1, "one pane, one conversation: {bound:?}");
    assert_eq!(bound[0].0, agent_pane, "the bound pane is the agent's");
    assert_eq!(bound[0].1, ID);
    assert_eq!(bound[0].2.as_deref(), Some("agent_resume"));

    // The premise `resume_agents` restores on: captured pane order is
    // creation order, which here is the order the directories were used.
    let captured_order: Vec<String> = conn
        .prepare(
            "SELECT p.cwd FROM pane_rows p
             JOIN window_rows w ON w.row_id = p.window_row_id
             WHERE w.snapshot_id = ?1 ORDER BY p.idx",
        )
        .unwrap()
        .query_map([snap], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let expected_order: Vec<String> = dirs
        .iter()
        .map(|d| d.to_str().unwrap().to_string())
        .collect();
    assert_eq!(
        captured_order, expected_order,
        "captured pane order is creation order"
    );

    // Simulate the reboot: the snapshot belongs to a previous boot, and the
    // tmux server — with the running agent in it — is gone.
    conn.execute(
        "UPDATE snapshots SET boot_id='boot-previous' WHERE id=?1",
        [snap],
    )
    .unwrap();
    drop(src);

    // And the agent really did go down with it. If it had not, `preflight`
    // would refuse the resume as `active_elsewhere` — correctly — and this
    // test would be asserting nothing.
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
    let json = osm::ipc::RestoreJson::from_report(&report);
    assert_eq!(
        json.agents_failed.len(),
        0,
        "no pane was left as a shell that should hold a conversation: {:?}",
        json.agents_failed
    );
    assert_eq!(json.agents_resumed, 1, "the conversation was resumed");
    assert_eq!(report.state, "succeeded", "reason={}", report.reason);

    let restored = panes_by_cwd(&dst.0);
    assert_eq!(restored.len(), 4, "{restored:?}");
    assert_eq!(
        cmd_in(&restored, &agent_dir),
        "claude",
        "the conversation must come back in the pane it was captured in, \
         identified by that pane's working directory: {restored:?}"
    );
    for dir in &dirs[..dirs.len() - 1] {
        assert_eq!(
            cmd_in(&restored, dir),
            "bash",
            "a pane that held no conversation must come back as a shell, \
             not running someone else's: {restored:?}"
        );
    }
}
