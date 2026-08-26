//! A restore may only owe conversations for the panes it actually put back.
//!
//! # The case
//!
//! A restore that conflicts on a session puts nothing back there: the live
//! session of that name is somebody else's, and everything the restore knows
//! about the captured one stays in the snapshot. But the debt it wrote down
//! covered every conversation the *snapshot* bound, conflicted sessions
//! included, and a debt was keyed by the conversation alone — so the next
//! capture found the conversation "owed", found a live pane sitting at the
//! captured pane's place, and bound the user's own pane to a conversation the
//! restore had never delivered into it. After the following reboot that pane
//! is where the conversation is resumed.
//!
//! Debt is now recorded only for panes in the attempt's verified
//! delivered-session and pane map, and is owed at a *place*, not merely by a
//! conversation.
//!
//! `auto_resume` is off: this is about what the restore writes down, not about
//! what it delivers.
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
    format!("osm-undelivered-{}-{}", label, std::process::id())
}

const ID: &str = "0cfebf91-81c0-43d5-af63-c9fe7e844b0d";

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
fn a_conflicted_sessions_conversation_is_not_owed_onto_the_live_panes_there() {
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
    common::tmux_conf_with_path(&config_home, &bin);

    let shell_dir = tmp.path().join("shell");
    let agent_dir = tmp.path().join("agent");
    std::fs::create_dir_all(&shell_dir).unwrap();
    std::fs::create_dir_all(&agent_dir).unwrap();

    // ---- what was captured before the reboot ------------------------------
    //
    // Two sessions, so the restore has something to succeed at as well as
    // something to conflict on: a run that delivers nothing at all is
    // `failed`, and a failed run never records any debt, which would make this
    // test pass for the wrong reason.
    let src = Server(Tmux::with_socket(&sock("src")));
    src.0
        .run(&[
            "new-session",
            "-d",
            "-s",
            "other",
            "-n",
            "misc",
            "-c",
            "/tmp",
            "-x",
            "200",
            "-y",
            "50",
        ])
        .unwrap();
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

    // ---- the machine comes back with somebody else's `dev` ----------------
    //
    // Same name, same window, two panes at the same indices — and both of them
    // somewhere else, so the restore refuses it as a conflict and puts nothing
    // back into it. The captured agent pane's place, `dev` window 0 pane 1, is
    // live and belongs to the user.
    let dst = Server(Tmux::with_socket(&sock("dst")));
    dst.0
        .run(&[
            "new-session",
            "-d",
            "-s",
            "dev",
            "-n",
            "code",
            "-c",
            "/tmp",
            "-x",
            "200",
            "-y",
            "50",
        ])
        .unwrap();
    dst.0
        .run(&["split-window", "-t", "=dev:code", "-c", "/"])
        .unwrap();
    let intruder = dst.0.list_panes().unwrap()[1].id.clone();

    let report = osm::restore::run_restore(&mut conn, &dst.0, false).unwrap();
    assert_eq!(
        report.outcome.conflicted.len(),
        1,
        "the live dev must be a conflict, or this test is about nothing: {report:?}"
    );
    assert_eq!(report.outcome.conflicted[0].0, "dev");
    assert_eq!(
        report.outcome.created,
        vec!["other".to_string()],
        "the other session must still be restored, so the run is not `failed` \
         and does record debt: {report:?}"
    );

    // ---- a capture lands in the reboot window -----------------------------
    let after = osm::capture::snapshot(&mut conn, &dst.0, "after-restore").unwrap();
    assert_eq!(
        bindings(&conn, after),
        Vec::<(String, String)>::new(),
        "nothing was put back into the live dev, so no conversation is owed \
         there; binding {ID} to {intruder} claims a pane the restore never \
         touched"
    );

    // And the debt table says the same thing: nothing about a session this
    // attempt did not deliver.
    let owed_rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM agent_resume_debt WHERE session_name = 'dev'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        owed_rows, 0,
        "a session this restore did not deliver must not be owed at all"
    );
}
