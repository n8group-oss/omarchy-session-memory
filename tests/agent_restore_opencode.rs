//! A binding osm cannot act on is left alone, and said so — never sent to.
//!
//! # Why this test exists
//!
//! OpenCode was configured, enabled by default, and completely inert:
//! it owns no transcript, so binding could score it at most 0.4 against a 0.75
//! threshold and no pane could ever be bound to one of its conversations. The
//! suite that covered it exercised discovery and argv and never binding or
//! restore, which is exactly why an unreachable capture path passed.
//!
//! Meanwhile the resume side failed *open*: `is_active_elsewhere` answered a
//! bare `false`, which callers read as "verified nobody has this open", so a
//! resume would attach a second client to a conversation already running.
//!
//! Both are now one declaration — `auto_unsupported_reason` — and this test
//! asserts what a restore does with a snapshot that binds one anyway (an older
//! database, or one edited by hand): nothing is sent into the pane, and the
//! outcome says `unsupported` rather than being silently dropped.
//!
//! One test per file: `$XDG_CONFIG_HOME` is process-global.

mod common;

use osm::agent::resume::Outcome;
use osm::tmux::Tmux;
use std::time::Duration;

struct Server(Tmux);
impl Drop for Server {
    fn drop(&mut self) {
        common::shutdown(&self.0);
    }
}

fn sock(label: &str) -> String {
    format!("osm-ocrestore-{}-{}", label, std::process::id())
}

#[test]
fn an_opencode_binding_is_reported_unsupported_and_nothing_is_sent() {
    let tmp = tempfile::tempdir().unwrap();
    let config_home = tmp.path().join("config");
    std::fs::create_dir_all(config_home.join("osm")).unwrap();
    std::fs::write(
        config_home.join("osm/config.toml"),
        "[agents]\nenabled = [\"opencode\"]\nauto_resume = true\nauto_resume_max_age_mins = 30\n",
    )
    .unwrap();
    std::env::set_var("XDG_CONFIG_HOME", &config_home);
    // No stub agent on PATH at all: if anything *were* sent, the pane would
    // show it, and the assertions below would see a pane that is not a shell.
    common::tmux_conf_with_path(&config_home, &tmp.path().join("empty-bin"));

    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();

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
            work.to_str().unwrap(),
            "-x",
            "200",
            "-y",
            "50",
        ])
        .unwrap();

    let mut conn = osm::db::open(&tmp.path().join("state.db")).unwrap();
    let snap = osm::capture::snapshot(&mut conn, &src.0, "before-reboot").unwrap();

    // Capture cannot produce this any more, which is the point — so it is
    // written the way an older database would hold it.
    let changed = conn
        .execute(
            "UPDATE pane_rows SET restore_policy='agent_resume', agent_kind='opencode',
                 agent_session_id='ses_x', agent_confidence=0.9
             WHERE window_row_id IN (SELECT row_id FROM window_rows WHERE snapshot_id = ?1)",
            [snap],
        )
        .unwrap();
    assert_eq!(changed, 1, "one pane, bound by hand");
    conn.execute(
        "UPDATE snapshots SET boot_id='boot-previous' WHERE id=?1",
        [snap],
    )
    .unwrap();
    drop(src);

    let dst = Server(Tmux::with_socket(&sock("dst")));
    let report = osm::restore::run_restore(&mut conn, &dst.0, false).unwrap();

    let outcomes = &report.outcome.agent_outcomes;
    assert_eq!(outcomes.len(), 1, "{outcomes:?}");
    assert_eq!(
        outcomes[0].1,
        Outcome::Unsupported,
        "a binding osm cannot act on is reported as such, not silently dropped \
         and not attempted: {outcomes:?}"
    );

    let json = osm::ipc::RestoreJson::from_report(&report);
    assert_eq!(json.agents_resumed, 0, "{json:?}");
    assert_eq!(
        json.agents_failed.len(),
        0,
        "nothing went wrong — osm was never able to do this — so it is not a \
         failure: {json:?}"
    );

    // The pane is a shell, and stayed one.
    let panes = dst.0.list_panes().unwrap();
    assert_eq!(panes.len(), 1, "{panes:?}");
    assert_eq!(
        common::wait_for_pane_cmd(&dst.0, &panes[0].id, "bash", Duration::from_secs(5)),
        "bash",
        "nothing may be sent into a pane osm cannot verify: {panes:?}"
    );

    // And the same fact, stated where a user reads capabilities.
    let support = osm::ipc::AgentSupport::of(&["opencode".to_string()]);
    assert_eq!(support.unsupported.len(), 1);
    assert_eq!(support.unsupported[0].kind, "opencode");
}
