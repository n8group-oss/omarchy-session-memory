//! A resume that was confirmed has been *done*, and stops being owed there and
//! then.
//!
//! # The defect this test exists for
//!
//! Debt was discharged in one place only: a later capture observing the
//! conversation running. A restore resuming a dozen panes works through them
//! in turn, waiting on each, which gives the user minutes in which to close
//! the first conversation after it has come back. The capture that follows
//! then finds a bare shell where that conversation was, finds the debt still
//! pending, and carries the conversation the user has just closed forward as
//! though the restore had never delivered it — so the next reboot resurrects
//! it.
//!
//! The pass is driven here from its parts rather than through `run_restore`,
//! because the window in question is between one pane's confirmation and the
//! publication at the end of the run, and there is no way to stand in it from
//! outside.
//!
//! One test per file: `capture`, `restore` and the stub agent on `$PATH` are
//! all process-global.

mod common;

use osm::tmux::Tmux;
use std::collections::HashSet;
use std::time::Duration;

struct Server(Tmux);
impl Drop for Server {
    fn drop(&mut self) {
        common::shutdown(&self.0);
    }
}

fn sock(label: &str) -> String {
    format!("osm-dischargedebt-{}-{}", label, std::process::id())
}

const ID: &str = "0cfebf91-81c0-43d5-af63-c9fe7e844b0e";

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
fn a_conversation_closed_after_its_resume_was_confirmed_is_not_carried() {
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
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    std::env::set_var("PATH", &path);
    std::env::set_var("OSM_CLAUDE_HOME", &home);
    std::env::set_var("XDG_CONFIG_HOME", &config_home);
    common::tmux_conf_with_path(&config_home, &bin);

    let cfg = osm::config::Config {
        agents: osm::config::AgentsCfg {
            auto_resume: true,
            auto_resume_max_age_mins: 30,
            enabled: vec!["claude".to_string()],
        },
        ..Default::default()
    };

    let shell_dir = tmp.path().join("shell");
    let agent_dir = tmp.path().join("agent");
    std::fs::create_dir_all(&shell_dir).unwrap();
    std::fs::create_dir_all(&agent_dir).unwrap();

    // ---- before the reboot -------------------------------------------------
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
        vec![(src_agent_pane, ID.to_string())],
        "the source capture binds the conversation to the pane running it"
    );
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

    // ---- the restore, driven from its parts --------------------------------
    let dst = Server(Tmux::with_socket(&sock("dst")));
    let tree = osm::model::load(&conn, snap).unwrap();
    let outcome = osm::restore::restore_tree(&dst.0, &tree).unwrap();
    assert_eq!(outcome.created, vec!["dev".to_string()], "{outcome:?}");

    // Exactly what `run_restore` writes down before it delivers anything.
    let boot_id = osm::boot::current_boot_id().unwrap();
    let delivered: HashSet<&str> = ["dev"].into_iter().collect();
    let restored_panes: HashSet<&str> = outcome.pane_map.keys().map(String::as_str).collect();
    assert_eq!(
        osm::debt::record(
            &conn,
            snap,
            &boot_id,
            osm::boot::now_epoch(),
            &delivered,
            &restored_panes,
        )
        .unwrap(),
        1,
        "the restore owes this conversation until it puts it back"
    );

    let pass = osm::restore::resume_agents(&dst.0, &conn, snap, &cfg, &outcome);
    assert_eq!(
        pass.outcomes
            .iter()
            .map(|(_, o)| o.as_str())
            .collect::<Vec<_>>(),
        vec!["resumed"],
        "the fixture only means anything if the resume was confirmed: {:?}",
        pass.outcomes
    );

    // ---- the user closes it, while the restore is still working ------------
    let dst_agent_pane = live_pane_at(&dst.0, &agent_dir);
    dst.0
        .run(&["send-keys", "-t", &dst_agent_pane, "C-c"])
        .unwrap();
    assert_eq!(
        common::wait_for_pane_cmd(&dst.0, &dst_agent_pane, "bash", Duration::from_secs(10)),
        "bash",
        "the conversation never exited"
    );

    // ---- and the capture that follows --------------------------------------
    let after = osm::capture::snapshot(&mut conn, &dst.0, "after-restore").unwrap();
    assert_eq!(
        bindings(&conn, after),
        Vec::<(String, String)>::new(),
        "this conversation was put back and the user closed it; carrying it \
         forward as still owed resurrects it on the next boot"
    );
    let still_owed: i64 = conn
        .query_row("SELECT COUNT(*) FROM agent_resume_debt", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        still_owed, 0,
        "a confirmed resume discharges the debt for the pane it was confirmed in"
    );
}
