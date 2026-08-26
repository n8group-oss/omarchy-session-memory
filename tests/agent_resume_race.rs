//! Two `osm resume` runs for the same conversation must not both deliver it.
//!
//! # The race this test exists for
//!
//! `preflight`'s exclusivity check asks "is any process holding this
//! conversation's transcript open" by scanning `/proc`. Two runs started at
//! the same moment both finish that scan before either agent has opened the
//! file, so both see nothing, both pass, and both send — the exact double
//! attach the check exists to prevent. The window is the whole of an agent's
//! startup, which is not small, and nothing observable distinguishes the two
//! runs, so no amount of care inside the check can close it.
//!
//! # Two tests, deterministic and end-to-end
//!
//! The first holds the conversation's lock from outside and asserts that a
//! resume refuses and *sends nothing* — no timing involved. The second runs
//! two real `osm resume` processes against the same conversation and two
//! different panes and asserts observable state: exactly one pane is running
//! the agent afterwards, and exactly one process reported `resumed`.
//!
//! One conversation id per file, unique to it: a resume refuses when any
//! process on the machine holds that conversation open, so an id shared with
//! another test binary would make these refuse correctly and fail confusingly.

mod common;

use osm::agent::AgentKind;
use osm::tmux::Tmux;
use std::process::Command;
use std::time::Duration;

struct Server(Tmux);
impl Drop for Server {
    fn drop(&mut self) {
        common::shutdown(&self.0);
    }
}

const HELD: &str = "0cfebf91-81c0-43d5-af63-c9fe7e844b09";
const RACED: &str = "0cfebf91-81c0-43d5-af63-c9fe7e844b0a";

struct Fixture {
    _tmp: tempfile::TempDir,
    home: std::path::PathBuf,
    bin: std::path::PathBuf,
    config_home: std::path::PathBuf,
    state_home: std::path::PathBuf,
    server: Server,
}

/// A private tmux server, a fixture Claude home, a stub agent, and a state
/// directory of this test's own.
///
/// Nothing here touches this process's environment: the panes get `PATH` and
/// `$OSM_CLAUDE_HOME` by exporting them *in the pane*, the way
/// `tests/agent_cli.rs` does, and `osm` gets everything on its command line.
/// Two tests in this file can therefore run at the same time.
fn fixture(label: &str, id: &str, panes: usize) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let home = common::claude_fixture(tmp.path(), id);
    let bin = common::stub_agent(tmp.path());
    let config_home = tmp.path().join("config");
    std::fs::create_dir_all(config_home.join("osm")).unwrap();
    std::fs::write(
        config_home.join("osm/config.toml"),
        "[agents]\nenabled = [\"claude\"]\n",
    )
    .unwrap();
    let state_home = tmp.path().join("state");
    std::fs::create_dir_all(&state_home).unwrap();

    let server = Server(Tmux::with_socket(&format!(
        "osm-resumerace-{}-{}",
        label,
        std::process::id()
    )));
    server
        .0
        .run(&["new-session", "-d", "-s", "dev", "-c", "/tmp"])
        .unwrap();
    for _ in 1..panes {
        server
            .0
            .run(&["split-window", "-t", "=dev:", "-c", "/tmp"])
            .unwrap();
    }
    for pane in server.0.list_panes().unwrap() {
        server
            .0
            .run(&[
                "send-keys",
                "-t",
                &pane.id,
                &format!(
                    "export PATH={}:$PATH OSM_CLAUDE_HOME={}",
                    bin.display(),
                    home.display()
                ),
                "C-m",
            ])
            .unwrap();
        assert_eq!(
            common::wait_for_pane_cmd(&server.0, &pane.id, "bash", Duration::from_secs(10)),
            "bash",
            "the pane never settled back to an idle shell"
        );
    }

    Fixture {
        _tmp: tmp,
        home,
        bin,
        config_home,
        state_home,
        server,
    }
}

impl Fixture {
    /// `osm resume <id>` aimed at this fixture's private everything.
    fn resume(&self, id: &str, pane: &str) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_osm"));
        cmd.arg("--socket")
            .arg(self.server.0.socket().unwrap())
            .args(["resume", id])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .env("TMUX_PANE", pane)
            .env("OSM_CLAUDE_HOME", &self.home)
            .env("XDG_CONFIG_HOME", &self.config_home)
            .env("XDG_STATE_HOME", &self.state_home)
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    self.bin.display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            );
        cmd
    }

    fn panes(&self) -> Vec<osm::tmux::PaneRec> {
        self.server.0.list_panes().unwrap()
    }
}

fn json(out: &std::process::Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not JSON ({e}): {:?} / stderr {:?}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

#[test]
fn a_conversation_another_process_holds_is_refused_and_nothing_is_sent() {
    let f = fixture("held", HELD, 1);
    let pane = f.panes()[0].id.clone();

    // Exactly the lock `osm resume` will reach for, taken the way another
    // `osm` process takes it — asked for by name rather than assembled from a
    // path this test invents, so a change to where the lock lives cannot make
    // the two silently stop contending. `XDG_STATE_HOME` is passed to the
    // child, so it has to be set here too for both to name the same file; it
    // is the only process-global in this file, and the other test never reads
    // it in-process.
    std::env::set_var("XDG_STATE_HOME", &f.state_home);
    let path = osm::agent::resume::conversation_lock_path(AgentKind::Claude, HELD).unwrap();
    let held = osm::lock::SingleInstance::acquire(&path)
        .unwrap()
        .expect("nothing else holds this conversation");

    let out = f.resume(HELD, &pane).output().expect("run osm");
    let v = json(&out);
    assert_eq!(
        v["outcome"], "active_elsewhere",
        "another process has this conversation: {v} / {out:?}"
    );
    assert!(!out.status.success(), "a refusal exits non-zero");
    assert_eq!(
        common::wait_for_pane_cmd(&f.server.0, &pane, "claude", Duration::from_secs(2)),
        "bash",
        "nothing may be sent into the pane while another process holds the \
         conversation"
    );

    // Released, and the same resume now works — so the refusal above is about
    // the lock and not about the fixture being broken.
    drop(held);
    let out = f.resume(HELD, &pane).output().expect("run osm");
    let v = json(&out);
    assert_eq!(v["outcome"], "resumed", "{v} / {out:?}");
    assert_eq!(
        common::wait_for_pane_cmd(&f.server.0, &pane, "claude", Duration::from_secs(10)),
        "claude"
    );
}

#[test]
fn two_simultaneous_resumes_of_one_conversation_deliver_it_once() {
    let f = fixture("race", RACED, 2);
    let panes = f.panes();
    assert_eq!(panes.len(), 2, "{panes:?}");

    // Both spawned before either is waited on, so their preflights overlap —
    // which is the whole condition. Without the lock both scans complete
    // before either agent opens the transcript and both send.
    let a = f.resume(RACED, &panes[0].id).spawn().expect("run osm");
    let b = f.resume(RACED, &panes[1].id).spawn().expect("run osm");
    let a = a.wait_with_output().unwrap();
    let b = b.wait_with_output().unwrap();

    // The assertion that matters, first: one conversation, one pane. A second
    // client attached to the same conversation is what this refuses to allow,
    // and it is a fact about the machine rather than about what the two
    // processes said.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let running = loop {
        let running: Vec<osm::tmux::PaneRec> = f
            .panes()
            .into_iter()
            .filter(|p| p.cmd == "claude")
            .collect();
        if running.len() == 1 || std::time::Instant::now() >= deadline {
            break running;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(
        running.len(),
        1,
        "the conversation must be in exactly one pane: {running:?}"
    );

    let outcomes: Vec<String> = [&a, &b]
        .iter()
        .map(|o| json(o)["outcome"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        outcomes.iter().filter(|o| *o == "resumed").count(),
        1,
        "exactly one run may report the conversation resumed: {outcomes:?} \
         / {a:?} / {b:?}"
    );
}
