//! A conversation must never be delivered into a session this restore did
//! **not** put back.
//!
//! # The defect this test exists for
//!
//! `resume_agents` used to locate a bound pane by navigating from the
//! *captured* names: the session came back under its captured name and the
//! window at its captured index, so `=dev:0` was taken to address the right
//! live window. The only precondition was that a session called `dev` exists.
//!
//! It does exist in the case that matters. When the destination already holds
//! an unrelated session called `dev`, the restore reports a topology conflict
//! and puts nothing back — that is Plan 1's refusal to destroy live work — and
//! the overall state is `partial`. The pre-reboot conversation was then typed
//! into a pane of *the user's live session*, which is the single worst thing
//! this feature can do: someone else's conversation appearing in a pane you
//! are working in.
//!
//! So this test asserts observable state, not counts: after the restore, no
//! pane anywhere on the destination server is running the agent, and the
//! panes of the unrelated `dev` are still the idle shells they were. A count
//! of `agents_failed` would have passed against the broken code too — it
//! reported a failure for a pane it had nonetheless already sent to.
//!
//! # One test per file
//!
//! `capture` and `restore` run in-process and read `$OSM_CLAUDE_HOME` and
//! `$XDG_CONFIG_HOME`, and the restored panes need the stub agent on `$PATH`.
//! All three are process-global; cargo gives each test binary its own
//! process, so this file holds exactly one test.

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
    format!("osm-conflictresume-{}-{}", label, std::process::id())
}

/// Unique to this file: a resume refuses when any process on the machine
/// holds the conversation open, so an id shared with another test binary
/// would make this refuse correctly and fail confusingly.
const ID: &str = "0cfebf91-81c0-43d5-af63-c9fe7e844b03";

fn pane_at<'a>(panes: &'a [osm::tmux::PaneRec], cwd: &std::path::Path) -> &'a osm::tmux::PaneRec {
    let want = cwd.to_str().unwrap();
    panes
        .iter()
        .find(|p| p.cwd == want)
        .unwrap_or_else(|| panic!("no pane at {want}: {panes:?}"))
}

#[test]
fn a_conversation_is_never_sent_into_a_session_the_restore_did_not_deliver() {
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

    let shell_dir = tmp.path().join("shell");
    let agent_dir = tmp.path().join("agent");
    let other_dir = tmp.path().join("other");
    for d in [&shell_dir, &agent_dir, &other_dir] {
        std::fs::create_dir_all(d).unwrap();
    }

    // ---- the machine before the reboot -------------------------------------
    //
    // Two sessions. `other` is what makes this test say something: without a
    // session the restore *does* deliver, the whole attempt would be `failed`
    // and the resume pass would never run at all.
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
    src.0
        .run(&[
            "new-session",
            "-d",
            "-s",
            "other",
            "-n",
            "logs",
            "-c",
            other_dir.to_str().unwrap(),
            "-x",
            "200",
            "-y",
            "50",
        ])
        .unwrap();

    let src_agent_pane = pane_at(&src.0.list_panes().unwrap(), &agent_dir).id.clone();
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
    assert_eq!(
        osm::agent::detect::live_process_ownership(&adapter, ID).unwrap(),
        osm::agent::Liveness::Inactive,
        "the simulated reboot left the agent running, so nothing would be resumed"
    );

    // ---- the machine after the reboot --------------------------------------
    //
    // Someone has already opened a session called `dev`. It is not the
    // captured one — different window name, different directories — but it
    // has at least as many panes, which is what let the broken code address
    // the pane the conversation "belonged" in and send to it.
    let dst = Server(Tmux::with_socket(&sock("dst")));
    let live_a = tmp.path().join("live-a");
    let live_b = tmp.path().join("live-b");
    for d in [&live_a, &live_b] {
        std::fs::create_dir_all(d).unwrap();
    }
    dst.0
        .run(&[
            "new-session",
            "-d",
            "-s",
            "dev",
            "-n",
            "notes",
            "-c",
            live_a.to_str().unwrap(),
            "-x",
            "200",
            "-y",
            "50",
        ])
        .unwrap();
    dst.0
        .run(&[
            "split-window",
            "-t",
            "=dev:notes",
            "-c",
            live_b.to_str().unwrap(),
        ])
        .unwrap();
    for pane in dst.0.list_panes().unwrap() {
        assert_eq!(
            common::wait_for_pane_cmd(&dst.0, &pane.id, "bash", Duration::from_secs(10)),
            "bash",
            "the live session's panes must be idle shells before the restore"
        );
    }

    let report = osm::restore::run_restore(&mut conn, &dst.0, false).unwrap();
    let json = osm::ipc::RestoreJson::from_report(&report);

    // ---- what must be true -------------------------------------------------

    // Nothing on this server is running the conversation. This is the whole
    // assertion; everything below is corroboration.
    let after = dst.0.list_panes().unwrap();
    let running: Vec<&osm::tmux::PaneRec> = after.iter().filter(|p| p.cmd == "claude").collect();
    assert!(
        running.is_empty(),
        "a conversation was delivered into a pane this restore never put back: {running:?}"
    );
    assert_eq!(
        pane_at(&after, &live_a).cmd,
        "bash",
        "the live session's first pane is untouched: {after:?}"
    );
    assert_eq!(
        pane_at(&after, &live_b).cmd,
        "bash",
        "the live session's second pane is untouched: {after:?}"
    );

    assert_eq!(report.state, "partial", "reason={}", report.reason);
    assert_eq!(
        json.conflicts.len(),
        1,
        "the live `dev` is a conflict, not an adoption: {json:?}"
    );
    assert_eq!(json.conflicts[0].session, "dev");
    assert_eq!(json.created, vec!["other".to_string()]);
    assert_eq!(json.agents_resumed, 0, "{json:?}");
    assert_eq!(
        json.agents_failed.len(),
        1,
        "the conversation that did not come back is reported, not swallowed: {json:?}"
    );
    assert!(
        json.agents_failed[0].reason.contains("dev"),
        "the reason names the session that was not delivered: {:?}",
        json.agents_failed[0]
    );

    // And the snapshot is still the only record of that conversation, so it
    // must still be selectable by the next restore.
    assert!(json.retryable, "{json:?}");
    let state: String = conn
        .query_row("SELECT state FROM snapshots WHERE id = ?1", [snap], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(state, "complete", "the source snapshot was not retired");
    let unresolved: i64 = conn
        .query_row(
            "SELECT unresolved FROM snapshots WHERE id = ?1",
            [snap],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(unresolved, 1, "`dev` is still owed");
}
