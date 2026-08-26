//! What a resume that does not take does to the restore, the snapshot and the
//! pane.
//!
//! # Why this test exists
//!
//! `tests/agent_restore.rs` pins the *predicate* that classifies agent
//! outcomes, and nothing else: delete the downgrade in `run_restore` that
//! consults it and every test in that file stays green while a restore that
//! left a conversation behind reports `succeeded` and retires the only
//! snapshot that knew about it. So this asserts the observable consequences
//! instead — the reported state, the snapshot's row in the database, its
//! retryability, and which pane is (and is not) running the conversation.
//!
//! # The fixture
//!
//! A stub that starts under the name `claude` and holds no conversation at
//! all: the agent appears and the resume is not real. That is the failure this
//! project cannot let pass for a success, and it is deliberately the *hard*
//! version — the pane's foreground command is right for the whole timeout and
//! only the identity is missing.
//!
//! One test per file: `$OSM_CLAUDE_HOME` and `$XDG_CONFIG_HOME` are
//! process-global.

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
    format!("osm-failedresume-{}-{}", label, std::process::id())
}

const ID: &str = "0cfebf91-81c0-43d5-af63-c9fe7e844b0b";

/// A stub that runs under the agent's name and holds nothing, so a resume can
/// never be confirmed. See `tests/agent_resume_wrong_conversation.rs`.
fn stub(dir: &std::path::Path) -> std::path::PathBuf {
    let bin_dir = dir.join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let path = bin_dir.join("claude");
    std::fs::write(&path, "#!/bin/bash\nexec -a claude sleep 100000\n").unwrap();
    let mut perm = std::fs::metadata(&path).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perm, 0o755);
    std::fs::set_permissions(&path, perm).unwrap();
    bin_dir
}

fn pane_at(panes: &[osm::tmux::PaneRec], cwd: &std::path::Path) -> osm::tmux::PaneRec {
    let want = cwd.to_str().unwrap();
    panes
        .iter()
        .find(|p| p.cwd == want)
        .unwrap_or_else(|| panic!("no pane at {want}: {panes:?}"))
        .clone()
}

#[test]
fn a_resume_that_cannot_be_confirmed_keeps_the_snapshot_and_says_so() {
    let tmp = tempfile::tempdir().unwrap();
    let home = common::claude_fixture(tmp.path(), ID);
    // The *real* stub for the source pane, so the capture has a genuine
    // binding to record; the destination gets the one that only pretends.
    let real_bin = common::stub_agent(tmp.path());

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
            real_bin.display(),
            std::env::var("PATH").unwrap_or_default()
        ),
    );
    std::env::set_var("OSM_CLAUDE_HOME", &home);
    std::env::set_var("XDG_CONFIG_HOME", &config_home);
    common::tmux_conf_with_path(&config_home, &real_bin);

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
    let src_agent = pane_at(&src.0.list_panes().unwrap(), &agent_dir).id;
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

    // From here on, `claude` starts and holds nothing.
    let fake_bin = stub(&tmp.path().join("fake"));
    std::env::set_var(
        "PATH",
        format!(
            "{}:{}",
            fake_bin.display(),
            std::env::var("PATH").unwrap_or_default()
        ),
    );
    common::tmux_conf_with_path(&config_home, &fake_bin);

    let dst = Server(Tmux::with_socket(&sock("dst")));
    let report = osm::restore::run_restore(&mut conn, &dst.0, false).unwrap();
    let json = osm::ipc::RestoreJson::from_report(&report);

    // Every session came back — this is not a topology failure — and the
    // restore is still not a success.
    assert_eq!(json.created, vec!["dev".to_string()], "{json:?}");
    assert!(json.conflicts.is_empty(), "{json:?}");
    assert!(json.degraded.is_empty(), "{json:?}");
    assert_eq!(
        report.state, "partial",
        "a pane left without its conversation is work the restore did not do \
         (reason={})",
        report.reason
    );
    assert_eq!(json.agents_resumed, 0, "{json:?}");
    assert_eq!(json.agents_failed.len(), 1, "{json:?}");

    // The snapshot is the only record of that conversation, so it must still
    // be there and still be selectable.
    assert!(json.retryable, "{json:?}");
    let (state, unresolved): (String, i64) = conn
        .query_row(
            "SELECT state, unresolved FROM snapshots WHERE id = ?1",
            [snap],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(state, "complete", "the source snapshot was not retired");
    // The *session* debt is discharged, and correctly: `dev` really is back,
    // with its windows and its panes, so a later capture must not carry it
    // forward and recreate it. What is missing is the conversation, and that
    // is recorded where conversations are recorded.
    assert_eq!(unresolved, 0, "the session itself was delivered");
    let owed: Vec<(String, String)> = conn
        .prepare("SELECT kind, native_id FROM agent_resume_debt")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        owed,
        vec![("claude".to_string(), ID.to_string())],
        "the conversation that did not come back is still owed"
    );
    let published: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM snapshots WHERE id <> ?1",
            [snap],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        published, 0,
        "a restore that did not finish must not publish a snapshot over the \
         one that still knows what is missing"
    );

    let attempt: String = conn
        .query_row(
            "SELECT state FROM restore_attempts ORDER BY id DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(attempt, "partial");

    // And the record of *which pane* the conversation belongs in survives: a
    // capture taken now carries it onto the live pane that took the captured
    // one's place, because the restore recorded that it still owes it.
    let dst_agent = pane_at(&dst.0.list_panes().unwrap(), &agent_dir);
    let after = osm::capture::snapshot(&mut conn, &dst.0, "after-failed-resume").unwrap();
    let bound: Vec<(String, String)> = conn
        .prepare(
            "SELECT p.tmux_pane_id, p.agent_session_id
             FROM pane_rows p JOIN window_rows w ON w.row_id = p.window_row_id
             WHERE w.snapshot_id = ?1 AND p.agent_session_id IS NOT NULL",
        )
        .unwrap()
        .query_map([after], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        bound,
        vec![(dst_agent.id.clone(), ID.to_string())],
        "the conversation is still recorded against the pane it belongs in"
    );
}
