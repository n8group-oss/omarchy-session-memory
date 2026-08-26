//! A resume osm refused because it could not tell whether the conversation
//! was already alive is work the restore did not do.
//!
//! # The defect this test exists for
//!
//! `preflight` fails closed on [`osm::agent::Liveness::Unknown`] — it sends
//! nothing — which is right. What it *reported* was
//! `resume::Outcome::Unsupported`, and `Unsupported` is deliberately not
//! degrading: it means "nothing here was ever capable of this", so a run
//! carrying one is still a full success. The consequences of calling this that
//! were the whole failure:
//!
//! * the attempt was recorded `succeeded` although nothing was delivered;
//! * `succeeded` is the one state that retires the source snapshot — the only
//!   record of which pane held which conversation — so it was retired;
//! * the debt this restore had recorded carried the binding onto the restored
//!   *shell* at publication, and because the resume pass confirmed nothing,
//!   `resumed` was empty and the publication's own cross-check had nothing to
//!   notice the missing conversation with;
//! * `retryable` was false, so the answer that may well be different a minute
//!   later was never asked again.
//!
//! # The fixture
//!
//! The real one, not a mock: a process running under the name `claude` holds
//! the conversation's transcript open and the file is then replaced
//! atomically underneath it, which is how an agent rewrites a transcript. The
//! kernel reports that descriptor as `…/<id>.jsonl (deleted)`, discovery has
//! meanwhile indexed the replacement, and no device/inode match can join them
//! — so ownership is genuinely indeterminate while a live process really does
//! hold the conversation.
//!
//! One test per file: `$OSM_CLAUDE_HOME` and `$XDG_CONFIG_HOME` are
//! process-global.

mod common;

use osm::tmux::Tmux;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Server(Tmux);
impl Drop for Server {
    fn drop(&mut self) {
        common::shutdown(&self.0);
    }
}

fn sock(label: &str) -> String {
    format!("osm-ownunknown-{}-{}", label, std::process::id())
}

const ID: &str = "0cfebf91-81c0-43d5-af63-c9fe7e844c17";

/// Kills its child on drop, including when an assertion panics — the child
/// would otherwise hold the harness's stdout open and hang `cargo test`.
struct Holder(Child);
impl Drop for Holder {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
impl Holder {
    fn pid(&self) -> u32 {
        self.0.id()
    }
}

/// A process that runs under the agent's own name and holds `transcript`
/// open, the way a live conversation's agent does.
fn holder(transcript: &Path) -> Holder {
    use std::os::unix::process::CommandExt;
    let mut cmd = Command::new("/bin/sh");
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // The loop keeps `sh` from exec'ing into `sleep` and taking the descriptor
    // out from under the name this test is about.
    cmd.arg0("claude").arg("-c").arg(format!(
        "exec 3<{:?}; while :; do sleep 60; done",
        transcript
    ));
    Holder(cmd.spawn().unwrap())
}

/// Write a new file beside `path` and rename it over the top — how a
/// transcript is replaced without ever being left half-written. The inode
/// anyone had open is unlinked by it.
fn replace_atomically(path: &Path) {
    let staging = path.with_extension("jsonl.new");
    std::fs::write(&staging, "{\"cwd\":\"/tmp\",\"replaced\":true}\n").unwrap();
    std::fs::rename(&staging, path).unwrap();
}

fn pane_at(panes: &[osm::tmux::PaneRec], cwd: &Path) -> osm::tmux::PaneRec {
    let want = cwd.to_str().unwrap();
    panes
        .iter()
        .find(|p| p.cwd == want)
        .unwrap_or_else(|| panic!("no pane at {want}: {panes:?}"))
        .clone()
}

#[test]
fn a_resume_refused_for_indeterminate_ownership_keeps_the_snapshot_and_says_so() {
    let tmp = tempfile::tempdir().unwrap();
    let home = common::claude_fixture(tmp.path(), ID);
    let transcript = home.join("projects/-tmp").join(format!("{ID}.jsonl"));
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

    // ---- before the reboot: a pane really is running the conversation ------
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
    let captured: Vec<(String, String, u32)> = conn
        .prepare(
            "SELECT p.tmux_pane_id, p.agent_session_id, p.idx
             FROM pane_rows p JOIN window_rows w ON w.row_id = p.window_row_id
             WHERE w.snapshot_id = ?1 AND p.agent_session_id IS NOT NULL",
        )
        .unwrap()
        .query_map([snap], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        captured
            .iter()
            .map(|(pane, id, _)| (pane.clone(), id.clone()))
            .collect::<Vec<_>>(),
        vec![(src_agent.clone(), ID.to_string())],
        "the fixture only means anything if the snapshot knows which pane held \
         the conversation"
    );
    // The pane index tmux gave it, read back rather than assumed: `base-index`
    // is a user setting and this label is built from whatever it is.
    let label = format!("dev:code.{}", captured[0].2);
    conn.execute(
        "UPDATE snapshots SET boot_id='boot-previous' WHERE id=?1",
        [snap],
    )
    .unwrap();
    drop(src);

    let adapter = osm::agent::claude::Claude::with_home(&home);
    let gone_by = Instant::now() + Duration::from_secs(10);
    while osm::agent::detect::live_process_ownership(&adapter, ID).unwrap()
        != osm::agent::Liveness::Inactive
        && Instant::now() < gone_by
    {
        std::thread::sleep(Duration::from_millis(50));
    }

    // ---- and afterwards, ownership cannot be established -------------------
    let child = holder(&transcript);
    let holding_by = Instant::now() + Duration::from_secs(5);
    while osm::agent::detect::open_transcripts(child.pid()).is_empty() {
        assert!(
            Instant::now() < holding_by,
            "the holder never opened the transcript"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    replace_atomically(&transcript);
    assert_eq!(
        osm::agent::detect::live_process_ownership(&adapter, ID).unwrap(),
        osm::agent::Liveness::Unknown,
        "the fixture only means anything if ownership is genuinely \
         indeterminate at the moment the restore runs"
    );

    // ---- the restore -------------------------------------------------------
    let dst = Server(Tmux::with_socket(&sock("dst")));
    let report = osm::restore::run_restore(&mut conn, &dst.0, false).unwrap();
    let json = osm::ipc::RestoreJson::from_report(&report);

    // Every session came back — this is not a topology failure — and the
    // restore is still not a success.
    assert_eq!(json.created, vec!["dev".to_string()], "{json:?}");
    assert!(json.conflicts.is_empty(), "{json:?}");
    assert!(json.degraded.is_empty(), "{json:?}");
    assert_eq!(
        report
            .outcome
            .agent_outcomes
            .iter()
            .map(|(pane, o)| (pane.as_str(), o.as_str()))
            .collect::<Vec<_>>(),
        vec![(label.as_str(), "ownership_unknown")],
        "an indeterminate owner is its own outcome, never `unsupported`"
    );
    assert_eq!(
        report.state, "partial",
        "nothing was delivered, so a pane that should hold a conversation \
         holds a shell (reason={})",
        report.reason
    );
    assert_eq!(json.agents_resumed, 0, "{json:?}");
    assert_eq!(
        json.agents_failed
            .iter()
            .map(|f| (f.pane.as_str(), f.reason.as_str()))
            .collect::<Vec<_>>(),
        vec![(label.as_str(), "ownership_unknown")],
        "{json:?}"
    );

    // The snapshot is the only record of that conversation, so it must still
    // be there and still be selectable: the process whose descriptor could not
    // be identified may well be gone by the next attempt.
    assert!(json.retryable, "{json:?}");
    assert!(
        osm::restore::is_retryable(&report.state),
        "state {:?} must leave the source selectable",
        report.state
    );
    let (state, unresolved): (String, i64) = conn
        .query_row(
            "SELECT state, unresolved FROM snapshots WHERE id = ?1",
            [snap],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(state, "complete", "the source snapshot was retired");
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
        "a restore that delivered nothing must not publish a snapshot over the \
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

    // The pane the conversation belongs in is a bare shell, and the record of
    // which pane that is survives: a capture taken now carries the binding
    // onto the live pane that took the captured one's place.
    let dst_agent = pane_at(&dst.0.list_panes().unwrap(), &agent_dir);
    assert_eq!(
        dst_agent.cmd, "bash",
        "nothing was delivered, so this pane is a shell"
    );
    let after = osm::capture::snapshot(&mut conn, &dst.0, "after-refused-resume").unwrap();
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
