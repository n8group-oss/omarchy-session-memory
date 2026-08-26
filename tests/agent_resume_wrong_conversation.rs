//! A resume is confirmed by the conversation being back, never by a process
//! with the right *name* appearing.
//!
//! # The defect this test exists for
//!
//! Delivery polled `#{pane_current_command}` until it equalled the adapter's
//! binary name and then reported `Resumed`. That is satisfied by
//! `claude --resume <id>` starting, rejecting the id and exiting — the poll
//! runs every 100 ms and the process exists for far longer than that. The
//! restore then counted the conversation as resumed, and a fully "successful"
//! restore retires the snapshot that was the only record of it. The user is
//! left with a shell and no way back.
//!
//! # The fixture
//!
//! One stub, two behaviours, chosen by whether the conversation exists:
//!
//! * a known id: hold the transcript open and stay — a real resume;
//! * an unknown id: run *indefinitely under the name `claude`* while holding
//!   nothing. That is deliberately harsher than the real failure, which exits:
//!   here the name is present for the whole timeout and only the identity is
//!   missing, so nothing but an identity check can tell the two apart.
//!
//! Both cases are asserted, in that order, so a confirmation that simply
//! always failed could not pass this test either.
//!
//! One test per file: `$OSM_CLAUDE_HOME` and `$XDG_CONFIG_HOME` are
//! process-global.

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

const KNOWN: &str = "0cfebf91-81c0-43d5-af63-c9fe7e844b06";
const UNKNOWN: &str = "0cfebf91-81c0-43d5-af63-c9fe7e844b07";

/// Like `common::stub_agent`, but an unrecognised conversation makes it run
/// under the agent's name for ever instead of exiting.
fn stub(dir: &std::path::Path) -> std::path::PathBuf {
    let bin_dir = dir.join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let path = bin_dir.join("claude");
    std::fs::write(
        &path,
        r#"#!/bin/bash
id=""
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--resume" ]; then id="$2"; shift 2 || shift; else shift; fi
done
[ -n "$id" ] || exit 64
for f in "$OSM_CLAUDE_HOME"/projects/*/"$id".jsonl; do
  if [ -f "$f" ]; then
    exec 9< "$f"
    exec -a claude sleep 100000
  fi
done
# The conversation is not ours. A real agent prints an error and exits; this
# one keeps the *name* and drops only the identity, which is the harder case.
exec -a claude sleep 100000
"#,
    )
    .unwrap();
    let mut perm = std::fs::metadata(&path).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perm, 0o755);
    std::fs::set_permissions(&path, perm).unwrap();
    bin_dir
}

#[test]
fn a_process_with_the_agents_name_is_not_a_resumed_conversation() {
    let tmp = tempfile::tempdir().unwrap();
    let home = common::claude_fixture(tmp.path(), KNOWN);
    let bin = stub(tmp.path());

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

    let adapter = osm::agent::claude::Claude::with_home(&home);
    let s = Server(Tmux::with_socket(&format!(
        "osm-wrongconv-{}",
        std::process::id()
    )));
    s.0.run(&["new-session", "-d", "-s", "dev", "-c", "/tmp"])
        .unwrap();
    s.0.run(&["split-window", "-t", "=dev:", "-c", "/tmp"])
        .unwrap();
    let panes = s.0.list_panes().unwrap();
    let (bad_pane, good_pane) = (panes[0].id.clone(), panes[1].id.clone());
    for p in &panes {
        assert_eq!(
            common::wait_for_pane_cmd(&s.0, &p.id, "bash", Duration::from_secs(10)),
            "bash"
        );
    }
    let server = s.0.running_server_incarnation().unwrap().unwrap();

    // --- the conversation the agent will not resume -------------------------
    let out = osm::agent::resume::deliver(
        &s.0,
        &bad_pane,
        &adapter,
        UNKNOWN,
        Duration::from_secs(3),
        &server,
    );
    // The premise: a process called `claude` really is the pane's foreground
    // command, so the old check would have said `Resumed`.
    assert_eq!(
        common::wait_for_pane_cmd(&s.0, &bad_pane, "claude", Duration::from_secs(5)),
        "claude",
        "the stub must be running, or this test proves nothing"
    );
    match &out {
        Outcome::Failed(why) => assert!(
            why.contains(UNKNOWN),
            "the reason names the conversation that did not come back: {why}"
        ),
        other => panic!(
            "a pane running a process named `claude` that holds no conversation \
             must not be reported as resumed: {other:?}"
        ),
    }

    // --- and the control: a conversation that really does come back ---------
    let out = osm::agent::resume::deliver(
        &s.0,
        &good_pane,
        &adapter,
        KNOWN,
        Duration::from_secs(15),
        &server,
    );
    assert_eq!(
        out,
        Outcome::Resumed,
        "a conversation whose transcript the pane's agent holds open is resumed"
    );
}
