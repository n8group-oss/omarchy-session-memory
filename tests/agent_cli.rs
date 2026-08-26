//! `osm agents --json` and `osm resume <native-id>`, exercised through the
//! built binary against a private tmux server.
//!
//! Every invocation passes `--socket`, and every server is named for this
//! process and this test: nothing here can reach the developer's own tmux.

mod common;

use osm::tmux::Tmux;
use std::process::Command;
use std::time::Duration;

struct Server(Tmux);
impl Drop for Server {
    fn drop(&mut self) {
        common::shutdown(&self.0);
    }
}

fn sock(label: &str) -> String {
    format!("osm-agcli-{}-{}", label, std::process::id())
}

fn server(label: &str) -> Server {
    let t = Tmux::with_socket(&sock(label));
    t.run(&["new-session", "-d", "-s", "dev", "-c", "/tmp"])
        .unwrap();
    Server(t)
}

/// A fixture config enabling only the Claude adapter, and the
/// `XDG_CONFIG_HOME` that points osm at it.
///
/// Not a detail: with the default set of adapters, `osm agents` would shell
/// out to whatever `opencode` is installed on the machine running the test
/// and report the developer's own conversations, so every assertion below
/// would depend on who ran it.
fn config_home(dir: &std::path::Path) -> std::path::PathBuf {
    let config_home = dir.join("config");
    let path = config_home.join("osm/config.toml");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "[agents]\nenabled = [\"claude\"]\n").unwrap();
    config_home
}

/// The built `osm`, aimed at this test's private tmux server and this
/// test's fixture Claude home — never the developer's.
fn osm(tmux: &Tmux, dir: &std::path::Path, home: &std::path::Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_osm"));
    cmd.arg("--socket")
        .arg(
            tmux.socket()
                .expect("tests always run against a named socket"),
        )
        .args(args)
        .env("OSM_CLAUDE_HOME", home)
        .env("XDG_CONFIG_HOME", config_home(dir));
    cmd
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

/// A conversation id per test, never one shared between them.
///
/// `is_active_elsewhere` — precondition 4 of a resume — asks whether *any*
/// process on the machine holds that conversation's transcript open, which
/// is machine-global by design and does not stop at a test boundary. Two
/// tests sharing an id means one holding its stub agent open makes the
/// other's resume correctly refuse with `active_elsewhere`, and the
/// suite fails only when they happen to overlap.
const ID_RESUMABLE: &str = "0cfebf91-81c0-43d5-af63-c9fe7e844c01";
const ID_LIVE: &str = "0cfebf91-81c0-43d5-af63-c9fe7e844c02";
const ID_RESUME: &str = "0cfebf91-81c0-43d5-af63-c9fe7e844c03";
const ID_NOPANE: &str = "0cfebf91-81c0-43d5-af63-c9fe7e844c04";
const ID_UNKNOWN_FIXTURE: &str = "0cfebf91-81c0-43d5-af63-c9fe7e844c05";
const ID_BUSY: &str = "0cfebf91-81c0-43d5-af63-c9fe7e844c06";
/// Deliberately absent from every fixture.
const ID_ABSENT: &str = "b70babcd-65cf-4760-b99b-e8fe1d07d290";

#[test]
fn agents_reports_a_conversation_no_pane_is_running_as_resumable() {
    let tmp = tempfile::tempdir().unwrap();
    let home = common::claude_fixture(tmp.path(), ID_RESUMABLE);
    let s = server("resumable");

    let out = osm(&s.0, tmp.path(), &home, &["agents", "--json"])
        .output()
        .expect("run osm");
    assert!(out.status.success(), "{out:?}");
    let v = json(&out);

    assert_eq!(v["live"].as_array().unwrap().len(), 0, "{v}");
    let resumable = v["resumable"].as_array().unwrap();
    assert_eq!(resumable.len(), 1, "{v}");
    assert_eq!(resumable[0]["native_id"], ID_RESUMABLE);
    assert_eq!(resumable[0]["kind"], "claude");
    assert_eq!(resumable[0]["alive"], false);
    assert_eq!(resumable[0]["pane"], serde_json::Value::Null);
}

#[test]
fn agents_reports_the_pane_a_conversation_is_running_in() {
    let tmp = tempfile::tempdir().unwrap();
    let home = common::claude_fixture(tmp.path(), ID_LIVE);
    let bin = common::stub_agent(tmp.path());
    let s = server("live");
    let pane = s.0.list_panes().unwrap()[0].id.clone();

    // The environment goes on the command line rather than into the tmux
    // server or into this process: the pane needs it only for the one
    // process it starts, and this binary's other tests must not see it.
    s.0.run(&[
        "send-keys",
        "-t",
        &pane,
        &format!(
            "OSM_CLAUDE_HOME={} {}/claude --resume {ID_LIVE}",
            home.display(),
            bin.display()
        ),
        "C-m",
    ])
    .unwrap();
    assert_eq!(
        common::wait_for_pane_cmd(&s.0, &pane, "claude", Duration::from_secs(10)),
        "claude",
        "the stub agent never started"
    );

    let v = json(
        &osm(&s.0, tmp.path(), &home, &["agents", "--json"])
            .output()
            .expect("run osm"),
    );
    let live = v["live"].as_array().unwrap();
    assert_eq!(live.len(), 1, "{v}");
    assert_eq!(live[0]["native_id"], ID_LIVE);
    assert_eq!(live[0]["pane"], pane);
    assert_eq!(live[0]["alive"], true);
    assert!(
        live[0]["confidence"].as_f64().unwrap() >= 0.75,
        "a reported binding is one that met the threshold: {v}"
    );
    assert_eq!(
        v["resumable"].as_array().unwrap().len(),
        0,
        "a conversation already running must not also be offered as resumable: {v}"
    );
}

#[test]
fn resume_puts_the_conversation_into_the_current_pane() {
    let tmp = tempfile::tempdir().unwrap();
    let home = common::claude_fixture(tmp.path(), ID_RESUME);
    let bin = common::stub_agent(tmp.path());
    let s = server("resume");
    let pane = s.0.list_panes().unwrap()[0].id.clone();

    // `osm resume` sends the adapter's own argv (`claude --resume <id>`), so
    // it is the pane's shell that has to be able to find `claude`. Exported
    // in the pane rather than in this process, which must not mutate its own
    // environment while the other tests in this binary are running.
    s.0.run(&[
        "send-keys",
        "-t",
        &pane,
        &format!(
            "export PATH={}:$PATH OSM_CLAUDE_HOME={}",
            bin.display(),
            home.display()
        ),
        "C-m",
    ])
    .unwrap();
    assert_eq!(
        common::wait_for_pane_cmd(&s.0, &pane, "bash", Duration::from_secs(10)),
        "bash",
        "the pane never settled back to an idle shell"
    );

    let out = osm(&s.0, tmp.path(), &home, &["resume", ID_RESUME])
        .env("TMUX_PANE", &pane)
        .output()
        .expect("run osm");

    let v = json(&out);
    assert_eq!(v["outcome"], "resumed", "{v} / {out:?}");
    assert_eq!(v["native_id"], ID_RESUME);
    assert_eq!(v["kind"], "claude");
    assert_eq!(v["pane"], pane);
    assert!(out.status.success(), "a resume that took exits 0: {out:?}");
    assert_eq!(
        s.0.list_panes().unwrap()[0].cmd,
        "claude",
        "the pane is running the conversation, not a shell"
    );
}

#[test]
fn resume_outside_tmux_refuses_rather_than_guessing_a_pane() {
    let tmp = tempfile::tempdir().unwrap();
    let home = common::claude_fixture(tmp.path(), ID_NOPANE);
    let s = server("nopane");
    let pane = s.0.list_panes().unwrap()[0].id.clone();
    // A pane reports `tmux` itself as its foreground command for a moment
    // after it is created, before it settles to its shell.
    common::wait_for_pane_cmd(&s.0, &pane, "bash", Duration::from_secs(10));

    // No TMUX_PANE: there is no "current pane", and picking one would mean
    // sending a conversation into a pane someone is working in.
    let out = osm(&s.0, tmp.path(), &home, &["resume", ID_NOPANE])
        .env_remove("TMUX_PANE")
        .output()
        .expect("run osm");
    assert!(!out.status.success(), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("TMUX_PANE"), "{stderr}");
    assert_eq!(
        s.0.list_panes().unwrap()[0].cmd,
        "bash",
        "nothing was sent into any pane"
    );
}

#[test]
fn resume_of_an_unknown_conversation_names_the_id_it_could_not_find() {
    let tmp = tempfile::tempdir().unwrap();
    let home = common::claude_fixture(tmp.path(), ID_UNKNOWN_FIXTURE);
    let s = server("unknown");
    let pane = s.0.list_panes().unwrap()[0].id.clone();
    common::wait_for_pane_cmd(&s.0, &pane, "bash", Duration::from_secs(10));

    let out = osm(&s.0, tmp.path(), &home, &["resume", ID_ABSENT])
        .env("TMUX_PANE", &pane)
        .output()
        .expect("run osm");

    assert!(!out.status.success(), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains(ID_ABSENT), "{stderr}");
    assert_eq!(
        s.0.list_panes().unwrap()[0].cmd,
        "bash",
        "nothing was sent into the pane"
    );
}

#[test]
fn resume_into_a_busy_pane_reports_pane_busy_and_exits_non_zero() {
    let tmp = tempfile::tempdir().unwrap();
    let home = common::claude_fixture(tmp.path(), ID_BUSY);
    let s = server("busy");
    let pane = s.0.list_panes().unwrap()[0].id.clone();
    s.0.run(&["send-keys", "-t", &pane, "sleep 30", "C-m"])
        .unwrap();
    assert_eq!(
        common::wait_for_pane_cmd(&s.0, &pane, "sleep", Duration::from_secs(10)),
        "sleep",
        "the fixture never started"
    );

    let out = osm(&s.0, tmp.path(), &home, &["resume", ID_BUSY])
        .env("TMUX_PANE", &pane)
        .output()
        .expect("run osm");

    let v = json(&out);
    assert_eq!(v["outcome"], "pane_busy", "{v}");
    assert!(
        !out.status.success(),
        "a resume that did not happen must not exit 0: {out:?}"
    );
}
