//! A resume must be bound to the server incarnation the restore was verified
//! against, and must be refused if that server is replaced before delivery.
//!
//! # The defect this test exists for
//!
//! Incarnation protection used to end when `restore_tree` returned. Between
//! that point and the moment a conversation is typed into a pane the engine
//! makes tmux calls, waits on a pane, and reads a transcript directory — long
//! enough for a server to die and be replaced on the same socket. The
//! replacement hands out `%0`, `%1`, … from zero again, so the pane ids the
//! restore recorded name *the replacement's* panes: whatever the user has
//! opened since. The publication step later noticed the identity had moved
//! and refused to retire the snapshot, which is correct and useless — the
//! conversation had already been delivered into somebody else's pane.
//!
//! # How the replacement is staged
//!
//! `run_restore` gives no seam to kill a server halfway through, and inventing
//! one would make this a test of the seam. So the restore's own product is
//! used directly: a [`RestoreOutcome`] holding the incarnation of the server
//! that did the work and the captured→live pane mapping it recorded, exactly
//! as `restore_tree` returns it. The server is then killed and a replacement
//! started on the same socket, arranged so that the pane id the mapping names
//! exists there too and belongs to something else.
//!
//! The assertion is observable state: the replacement's pane is still an idle
//! shell. An `agents_failed` count would not have caught this — the broken
//! code delivered *and* reported.
//!
//! # One test per file
//!
//! `$OSM_CLAUDE_HOME` and `$PATH` are process-global and the pane needs the
//! stub agent on the latter, so this file holds exactly one test.

mod common;

use osm::restore::RestoreOutcome;
use osm::tmux::Tmux;
use std::time::Duration;

struct Server(Tmux);
impl Drop for Server {
    fn drop(&mut self) {
        common::shutdown(&self.0);
    }
}

/// Deliberately the *same* socket for both incarnations: a replacement that
/// took a different socket could not be mistaken for the original in the
/// first place.
fn sock() -> String {
    format!("osm-replaced-{}", std::process::id())
}

const ID: &str = "0cfebf91-81c0-43d5-af63-c9fe7e844b04";

#[test]
fn a_resume_is_refused_when_the_verified_server_has_been_replaced() {
    let tmp = tempfile::tempdir().unwrap();
    let home = common::claude_fixture(tmp.path(), ID);
    let bin = common::stub_agent(tmp.path());

    let config_home = tmp.path().join("config");
    std::fs::create_dir_all(config_home.join("osm")).unwrap();
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

    let cfg = osm::config::Config {
        agents: osm::config::AgentsCfg {
            auto_resume: true,
            auto_resume_max_age_mins: 30,
            enabled: vec!["claude".to_string()],
        },
        ..Default::default()
    };

    let agent_dir = tmp.path().join("agent");
    let stranger_dir = tmp.path().join("stranger");
    for d in [&agent_dir, &stranger_dir] {
        std::fs::create_dir_all(d).unwrap();
    }

    // ---- the server the restore was verified against -----------------------
    let first = Server(Tmux::with_socket(&sock()));
    first
        .0
        .run(&[
            "new-session",
            "-d",
            "-s",
            "dev",
            "-n",
            "code",
            "-c",
            agent_dir.to_str().unwrap(),
            "-x",
            "200",
            "-y",
            "50",
        ])
        .unwrap();
    let first_pane = first.0.list_panes().unwrap()[0].id.clone();
    let first_server = first
        .0
        .running_server_incarnation()
        .unwrap()
        .expect("the first server is running");

    // A snapshot that binds the conversation to this pane. Written by
    // `capture` rather than by hand, so the row shape is the real one.
    let mut conn = osm::db::open(&tmp.path().join("state.db")).unwrap();
    first
        .0
        .run(&[
            "send-keys",
            "-t",
            &first_pane,
            &format!("claude --resume {ID}"),
            "C-m",
        ])
        .unwrap();
    assert_eq!(
        common::wait_for_pane_cmd(&first.0, &first_pane, "claude", Duration::from_secs(10)),
        "claude",
        "the stub agent never started"
    );
    let snap = osm::capture::snapshot(&mut conn, &first.0, "before").unwrap();
    let captured_pane: String = conn
        .query_row(
            "SELECT p.tmux_pane_id FROM pane_rows p
             JOIN window_rows w ON w.row_id = p.window_row_id
             WHERE w.snapshot_id = ?1 AND p.agent_session_id IS NOT NULL",
            [snap],
            |r| r.get(0),
        )
        .unwrap();

    // Exactly what `restore_tree` hands back for a clean run on this server.
    let outcome = RestoreOutcome {
        created: vec!["dev".to_string()],
        server: Some(first_server.clone()),
        pane_map: [(captured_pane, first_pane.clone())].into_iter().collect(),
        ..Default::default()
    };

    // ---- the server dies and a replacement takes the socket ----------------
    common::shutdown(&first.0);
    std::mem::forget(first);

    // The agent went down with it, or the resume would be refused as
    // `active_elsewhere` for an unrelated and correct reason.
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
        osm::agent::Liveness::Inactive
    );

    let second = Server(Tmux::with_socket(&sock()));
    second
        .0
        .run(&[
            "new-session",
            "-d",
            "-s",
            "dev",
            "-n",
            "code",
            "-c",
            stranger_dir.to_str().unwrap(),
            "-x",
            "200",
            "-y",
            "50",
        ])
        .unwrap();
    let second_pane = second.0.list_panes().unwrap()[0].id.clone();
    assert_eq!(
        second_pane, first_pane,
        "this test only means something if the replacement reissued the same \
         pane id the mapping names"
    );
    assert_ne!(
        second.0.running_server_incarnation().unwrap().unwrap(),
        first_server,
        "the replacement must be a different incarnation"
    );
    assert_eq!(
        common::wait_for_pane_cmd(&second.0, &second_pane, "bash", Duration::from_secs(10)),
        "bash",
        "the replacement's pane starts as an idle shell"
    );

    // ---- the resume pass, still holding the first server's verdict ---------
    let outcomes = osm::restore::resume_agents(&second.0, &conn, snap, &cfg, &outcome).outcomes;

    let panes = second.0.list_panes().unwrap();
    assert!(
        panes.iter().all(|p| p.cmd != "claude"),
        "a conversation was delivered into a pane belonging to a server this \
         restore never verified: {panes:?}"
    );
    assert_eq!(
        panes.iter().find(|p| p.id == second_pane).unwrap().cwd,
        stranger_dir.to_str().unwrap(),
        "the replacement's pane is the stranger's, not the captured one"
    );

    assert_eq!(outcomes.len(), 1, "{outcomes:?}");
    match &outcomes[0].1 {
        osm::agent::resume::Outcome::Failed(why) => assert!(
            why.contains("replaced") || why.contains("gone"),
            "the reason says the server moved: {why}"
        ),
        other => panic!("expected Failed, got {other:?}"),
    }
}
