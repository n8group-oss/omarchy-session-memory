//! Helpers shared by the integration suites.
//!
//! Every test binary that declares `mod common;` compiles this whole file, so
//! anything only one suite uses would otherwise be reported as dead code in
//! all the others.
#![allow(dead_code)]

use osm::tmux::Tmux;
use std::path::{Path, PathBuf};

/// Shut down the private tmux server this test owns, and delete the socket
/// file it leaves behind.
///
/// `kill-server` does not remove the socket, so every suite here — each of
/// which names its socket uniquely per run — used to leave one dead file per
/// server per `cargo test`. Removing it belongs here, next to the kill, and
/// nowhere else: a global sweep would be operating on a directory that also
/// holds the developer's live tmux server.
pub fn shutdown(tmux: &Tmux) {
    let _ = tmux.run(&["kill-server"]);
    for path in socket_paths(tmux) {
        let _ = std::fs::remove_file(path);
    }
}

/// Every path tmux could have put *this instance's* socket at.
///
/// The name is the one passed to `-L`, which every suite builds from a label
/// plus this process's id, so it belongs to this test run and to nothing else.
/// Nothing here globs, matches a prefix, or removes a directory: the only file
/// ever named is `/tmp/tmux-<uid>/<our socket name>`. That matters because the
/// directory it lives in is shared with the developer's real tmux server, and
/// the fix for stale sockets is to stop creating them, never to sweep them.
///
/// `/tmp` is the only root searched, deliberately. tmux's socket directory can
/// be moved by an environment variable, but that variable is banned in this
/// project (it does not isolate tmux on every version — see
/// `tests/no_default_server.rs`), so nothing here sets it and reading it would
/// only invite someone to think it did something.
pub fn socket_paths(tmux: &Tmux) -> Vec<PathBuf> {
    let Some(name) = tmux.socket() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir("/tmp") else {
        return Vec::new();
    };
    entries
        .flatten()
        // tmux names its socket directory `tmux-<uid>`; find it by name rather
        // than by asking for our uid, which would need a C binding this
        // project does not have.
        .filter(|e| e.file_name().to_string_lossy().starts_with("tmux-"))
        .map(|e| e.path().join(name))
        .collect()
}

/// Write a stand-in for a real coding agent, as an executable named
/// `claude`, and return the directory holding it.
///
/// The point of a stub is that these tests must not depend on a real Claude
/// Code (or Codex, or OpenCode) install being present — that would make the
/// suite unrunnable in CI and dependent on the developer's own machine, and
/// a real agent would talk to a real API.
///
/// It behaves like the real thing in the two ways detection and delivery
/// actually observe:
///
/// * it resolves `--resume <id>` to that conversation's transcript under
///   `$OSM_CLAUDE_HOME` and holds the file open, so a `/proc/<pid>/fd` scan
///   sees a descendant of the pane owning that transcript — the +0.5 signal
///   `detect::bind` needs. Resolving the id from the argument rather than
///   baking one in is deliberate: handed the *wrong* conversation id the
///   stub exits non-zero instead of running, so a resume that delivered the
///   wrong id cannot pass for a working one.
/// * it runs under the name `claude`, so `#{pane_current_command}` reads
///   `claude` — the +0.4 signal, and the same string `resume::deliver`
///   waits for before it will call a resume confirmed. `exec -a` is what
///   makes that true of the blocking process itself; without it the pane
///   would report `sleep`.
pub fn stub_agent(dir: &std::path::Path) -> PathBuf {
    let bin_dir = dir.join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let path = bin_dir.join("claude");
    std::fs::write(
        &path,
        r#"#!/bin/bash
# Stand-in for Claude Code. See tests/common/mod.rs::stub_agent.
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
exit 66
"#,
    )
    .unwrap();
    let mut perm = std::fs::metadata(&path).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perm, 0o755);
    std::fs::set_permissions(&path, perm).unwrap();
    bin_dir
}

/// Write a Claude transcript fixture for `id` under a fixture home and
/// return the home directory, which is what `$OSM_CLAUDE_HOME` must be set
/// to for both osm and [`stub_agent`] to find it.
pub fn claude_fixture(dir: &std::path::Path, id: &str) -> PathBuf {
    let home = dir.join("claude");
    let transcript = home.join("projects/-tmp").join(format!("{id}.jsonl"));
    std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    std::fs::write(&transcript, "{\"cwd\":\"/tmp\"}\n").unwrap();
    home
}

/// Wait until `pane` reports `cmd` as its foreground command, up to
/// `timeout`. Returns what it last saw, so a failing assertion can say what
/// the pane was actually running instead of only that it was not `cmd`.
///
/// Polled rather than sampled once: a command sent into a pane takes a
/// moment to start, and a freshly created pane briefly reports `tmux`
/// itself before settling to its shell.
pub fn wait_for_pane_cmd(
    tmux: &Tmux,
    pane: &str,
    cmd: &str,
    timeout: std::time::Duration,
) -> String {
    let deadline = std::time::Instant::now() + timeout;
    let mut last = String::new();
    loop {
        if let Ok(panes) = tmux.list_panes() {
            if let Some(p) = panes.iter().find(|p| p.id == pane) {
                last = p.cmd.clone();
                if last == cmd {
                    return last;
                }
            }
        }
        if std::time::Instant::now() >= deadline {
            return last;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Point a test tmux server at a configuration of this test's own, under
/// `config_home` (which the caller also exports as `$XDG_CONFIG_HOME`), and
/// make `bin_dir` — where [`stub_agent`] wrote its stand-in `claude` — the
/// first entry on the `PATH` of every pane that server creates.
///
/// # Why exporting `PATH` in the test process is not enough
///
/// It is inherited by the tmux server, and the server hands its environment
/// to each new pane, so on the developer's machine the stub is found and
/// these tests pass. What then happens is a property of the *shell*, not of
/// tmux: with `default-command` empty, tmux starts a pane's shell as a
/// **login** shell, and Debian's `/etc/profile` does
///
/// ```text
/// if [ "`id -u`" -eq 0 ]; then PATH="/usr/local/sbin:...:/bin"
/// ```
///
/// — an assignment, not a prepend. In the CI container (root on Debian) the
/// inherited `PATH` was therefore thrown away before the shell read its
/// first command, `claude` was "command not found", and the pane sat at
/// `bash` forever; on Arch, where `/etc/profile` *appends* and the developer
/// is not root, nothing was lost and the same test passed. A sleep or a
/// retry could never have fixed it: the command had already failed.
///
/// So the pane's shell is started from `default-command` instead, which tmux
/// runs through `/bin/sh -c` **without** making it a login shell, with the
/// directory prepended explicitly. `--noprofile --norc` is not belt and
/// braces either: without it an interactive bash reads the developer's own
/// `~/.bashrc`, and a version manager activated there (mise, asdf, …)
/// prepends its shims — one of which, on this project's author's machine, is
/// a *real* `claude`. A test that shells out to the real agent is exactly
/// what the stub exists to prevent.
///
/// Writing it as `$XDG_CONFIG_HOME/tmux/tmux.conf` rather than setting the
/// option after the fact is what makes it apply to a server this test never
/// starts by hand — the destination server of a restore is created by
/// `restore_tree` itself.
///
/// It does **not** stop the developer's own configuration loading. tmux's
/// search path is
/// `/etc/tmux.conf:~/.tmux.conf:$XDG_CONFIG_HOME/tmux/tmux.conf:~/.config/tmux/tmux.conf`
/// and it reads *every* entry that exists, so a real `~/.config/tmux/tmux.conf`
/// is still read — as it always has been by these tests — and, being later in
/// the list, would win if it set `default-command` too. That is a loud
/// failure (the stub becomes unreachable and the pane never runs it), not a
/// silent one, so it is left as is rather than papered over.
pub fn tmux_conf_with_path(config_home: &Path, bin_dir: &Path) {
    let dir = config_home.join("tmux");
    std::fs::create_dir_all(&dir).unwrap();
    // Single quotes: tmux passes the value to `sh -c` verbatim, so `$PATH`
    // and the shell's own quoting are resolved there and not by tmux's
    // format expansion.
    std::fs::write(
        dir.join("tmux.conf"),
        format!(
            "set -g default-command 'PATH=\"{}:$PATH\" exec bash --noprofile --norc'\n",
            bin_dir.display()
        ),
    )
    .unwrap();
}

/// Declare a test's machine as one with no compositor.
///
/// A capture that is asked for window placement and cannot read it now fails,
/// rather than recording "no session has a terminal window" over the last good
/// layout and pruning it out of retention. CI runs in a container with no
/// Hyprland and no `hyprctl` at all, and so does any headless server, so the
/// suites whose subject is tmux rather than the desktop say so the documented
/// way: `restore.place_windows = false`.
///
/// `config_home` is the directory a test passes as `XDG_CONFIG_HOME`.
pub fn write_headless_config(config_home: &Path) {
    let dir = config_home.join("osm");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("config.toml"),
        "[restore]\nplace_windows = false\n",
    )
    .unwrap();
}

/// An `hyprctl` that answers like an ordinary desktop with one monitor and no
/// windows open, written into `root/bin`; returns that directory, for a test
/// to put at the front of its `PATH`.
///
/// For the suites that must exercise the *default* configuration — placement
/// on — without a compositor to exercise it against. It is a shell script that
/// echoes: it cannot reach a compositor, and it refuses to dispatch, because
/// a dispatch would move a window on the developer's own desktop.
pub fn stub_hyprctl(root: &Path) -> PathBuf {
    let dir = root.join("bin");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("hyprctl");
    std::fs::write(
        &path,
        r#"#!/bin/sh
# Stand-in for hyprctl. See tests/common/mod.rs.
if [ "$1" = "-j" ] && [ "$2" = "clients" ]; then echo "[]"; exit 0; fi
if [ "$1" = "-j" ] && [ "$2" = "monitors" ]; then
  echo '[{"id":0,"name":"DP-1","description":"stub","x":0,"y":0,"width":1920,"height":1080,"scale":1.0,"transform":0,"focused":true}]'
  exit 0
fi
echo "this stub compositor does not dispatch: $*" >&2
exit 1
"#,
    )
    .unwrap();
    let mut perm = std::fs::metadata(&path).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perm, 0o755);
    std::fs::set_permissions(&path, perm).unwrap();
    dir
}

/// One test's private world: its own state directory, its own configuration,
/// and its own tmux server.
///
/// Every suite here had grown its own copy of this — a `TempDir`, a socket
/// name built from a label and this process's id, `XDG_STATE_HOME` and
/// `XDG_CONFIG_HOME` pointed inside it, and a `Drop` that kills the server.
/// The copies are left where they are; this is the one new suites use.
///
/// Two invariants it exists to make unforgettable:
///
/// * **every** `osm` it runs is given `--socket`, and every `tmux` it runs is
///   this handle's `-L` server, so nothing a test does can reach the
///   developer's real tmux server. The environment variable that moves tmux's
///   socket directory is not used and must not be: it does not isolate tmux
///   on every version, and `tests/no_default_server.rs` rejects it by name.
/// * the configuration it writes declares the machine headless
///   (`restore.place_windows = false`), because a capture that is asked for
///   window placement and cannot read it fails — which is every CI container
///   and any headless server. A suite whose subject *is* the desktop writes
///   its own config over this one.
pub struct Env {
    pub dir: tempfile::TempDir,
    pub socket: String,
}

impl Env {
    pub fn new(label: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        write_headless_config(&dir.path().join("config"));
        Env {
            socket: format!("osm-{}-{}", label, std::process::id()),
            dir,
        }
    }

    /// The tmux handle for this test's own server.
    pub fn server(&self) -> Tmux {
        Tmux::with_socket(&self.socket)
    }

    /// Run one tmux command against this test's server and return its
    /// stdout. Panics with tmux's own stderr on failure, so a broken
    /// fixture says what tmux objected to.
    pub fn tmux(&self, args: &[&str]) -> String {
        self.server()
            .run(args)
            .unwrap_or_else(|e| panic!("tmux {args:?}: {e:#}"))
    }

    /// Run `osm` — always with `--socket`, never against the default
    /// server — and return its output. A non-zero exit panics here rather
    /// than surfacing later as unparseable JSON.
    pub fn osm(&self, args: &[&str]) -> std::process::Output {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_osm"))
            .env("XDG_STATE_HOME", self.dir.path().join("state"))
            .env("XDG_CONFIG_HOME", self.dir.path().join("config"))
            .arg("--socket")
            .arg(&self.socket)
            .args(args)
            .output()
            .unwrap_or_else(|e| panic!("spawn osm {args:?}: {e}"));
        assert!(
            out.status.success(),
            "osm {args:?} failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        shutdown(&self.server());
    }
}

/// Run a command, and stop waiting for it after `timeout`.
///
/// `Command::output()` waits forever. A helper process that is *supposed* to
/// print a result and exit — the QML harnesses in `qml_shell_quoting.rs` and
/// `qml_status_shape.rs` are the ones here — does neither if it throws before
/// its `Qt.exit`, and the suite then hangs rather than fails. A test that can
/// hang is a test that stops the whole run on the machine least able to say
/// why.
///
/// Returns whatever the process produced, and its status; a killed process is
/// reported through that status, so a caller that cannot parse the output
/// fails with the output it did get.
pub fn run_bounded(
    cmd: &mut std::process::Command,
    timeout: std::time::Duration,
) -> std::process::Output {
    let mut child = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn the helper process");
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait().expect("wait on the helper process") {
            Some(_) => break,
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                break;
            }
            None => std::thread::sleep(std::time::Duration::from_millis(20)),
        }
    }
    child
        .wait_with_output()
        .expect("collect the helper process output")
}

/// A `systemctl` that acts on nothing, written into `dir`; returns that
/// directory, for a test to put at the **front** of a child's `PATH`.
///
/// `osm install` and `osm uninstall` are the only commands in this project
/// that run `systemctl --user`, and the units they name are the developer's
/// to keep: this machine has a live `tmux.service` and a live `herdr.service`
/// next to them. So the tests for those two commands do not run the real
/// thing at all. They run this, which records what it was asked to do and
/// exits with a status the test chooses.
///
/// It reads three variables from its environment:
///
/// * `OSM_TEST_LOG` — the file it appends one line per invocation to;
/// * `OSM_TEST_BINARY` — a file whose first three bytes it records alongside
///   that line, which is how a test tells whether the daemon was stopped
///   *before* or *after* its binary was replaced. The two orders produce the
///   same argument lists and only one of them is safe;
/// * `OSM_TEST_IS_ACTIVE`, `OSM_TEST_STOP_RC`, `OSM_TEST_DISABLE_RC` — the
///   exit status to report for `is-active`, `stop` and `disable`, each
///   defaulting to 0;
/// * `OSM_TEST_IS_ACTIVE_STATE` — the state word `is-active` prints on
///   stdout when it exits non-zero, defaulting to `inactive`. The empty
///   string is what a manager that could not be reached looks like: real
///   `systemctl` prints the state and exits 3 when a unit is simply not
///   running, and prints nothing at all when it never got to ask;
/// * `OSM_TEST_SHOW_SLEEP`, `OSM_TEST_IS_ACTIVE_SLEEP` — seconds to hang for
///   instead of answering `show` / `is-active`, which is what an
///   unresponsive user manager does to both questions;
/// * `OSM_TEST_RELOAD_SLEEP`, `OSM_TEST_ENABLE_SLEEP`, `OSM_TEST_STOP_SLEEP`,
///   `OSM_TEST_DISABLE_SLEEP` — the same for the four *actions*. A manager
///   that answers a question and then wedges on the command that follows it
///   is the case a successful probe says nothing about, and it is what hung
///   an install after its files were written and an uninstall before it
///   removed anything;
/// * `OSM_TEST_STAYS_ACTIVE` — when set, `is-active` keeps saying `active`
///   even after a `stop` or a `disable --now` was accepted. `--no-block`
///   returns as soon as the job is *queued*, so a command that succeeded is
///   not a unit that stopped, and only the state says which.
///
/// The stand-in is stateful about exactly one thing: it records that it
/// accepted a `stop` or a `disable` (in `$OSM_TEST_LOG.stopped`) and, unless
/// `OSM_TEST_STAYS_ACTIVE` says otherwise, reports the units inactive
/// afterwards. Without that a caller which confirms a stop by asking would
/// wait out its whole budget in every test that stops anything.
///
/// A test that expects systemd work must assert the log is non-empty. An
/// empty log means this stand-in never ran — and therefore that the real
/// `systemctl` may have been called instead.
pub fn stub_systemctl(dir: &Path) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join("systemctl");
    std::fs::write(
        &path,
        r#"#!/bin/sh
# Stand-in for systemctl. See tests/common/mod.rs::stub_systemctl.
#
# `show` is a question, not an action. It is answered from OSM_TEST_UNIT_PATH
# and deliberately kept out of the log, so a test can still assert that an
# install did or did not *act* on systemd.
#
# OSM_TEST_SHOW_SLEEP makes the question never come back. `exec` rather than a
# plain `sleep`, so the process the engine kills when its budget runs out is
# the sleeping one itself: a grandchild would outlive the kill and go on
# holding the pipe the engine reads.
for arg in "$@"; do
  case "$arg" in
    show)
      if [ -n "${OSM_TEST_SHOW_SLEEP:-}" ]; then exec sleep "$OSM_TEST_SHOW_SLEEP"; fi
      printf '%s\n' "${OSM_TEST_UNIT_PATH:-}"; exit "${OSM_TEST_SHOW_RC:-0}" ;;
  esac
done
seen="ABSENT"
if [ -f "$OSM_TEST_BINARY" ]; then
  seen="$(head -c 3 "$OSM_TEST_BINARY" | tr -dc '[:print:]')"
fi
echo "$* | binary=$seen" >> "$OSM_TEST_LOG"
stopped="$OSM_TEST_LOG.stopped"
for arg in "$@"; do
  case "$arg" in
    is-active)
      if [ -n "${OSM_TEST_IS_ACTIVE_SLEEP:-}" ]; then exec sleep "$OSM_TEST_IS_ACTIVE_SLEEP"; fi
      # A manager that answers, slowly. OSM_TEST_IS_ACTIVE_SLEEP never comes
      # back at all; this one takes its time and then says what it would have
      # said, which is what a confirmation loop with a deadline has to survive.
      if [ -n "${OSM_TEST_IS_ACTIVE_DELAY:-}" ]; then sleep "$OSM_TEST_IS_ACTIVE_DELAY"; fi
      # A stop this stand-in accepted has taken effect, unless the test is
      # about a unit that will not go away.
      if [ -f "$stopped" ] && [ -z "${OSM_TEST_STAYS_ACTIVE:-}" ]; then
        printf 'inactive\n'; exit 3
      fi
      rc="${OSM_TEST_IS_ACTIVE:-0}"
      if [ "$rc" = 0 ]; then printf 'active\n'
      else printf '%s\n' "${OSM_TEST_IS_ACTIVE_STATE-inactive}"; fi
      exit "$rc" ;;
    daemon-reload)
      if [ -n "${OSM_TEST_RELOAD_SLEEP:-}" ]; then exec sleep "$OSM_TEST_RELOAD_SLEEP"; fi
      exit "${OSM_TEST_RELOAD_RC:-0}" ;;
    enable)
      if [ -n "${OSM_TEST_ENABLE_SLEEP:-}" ]; then exec sleep "$OSM_TEST_ENABLE_SLEEP"; fi
      exit "${OSM_TEST_ENABLE_RC:-0}" ;;
    stop)
      if [ -n "${OSM_TEST_STOP_SLEEP:-}" ]; then exec sleep "$OSM_TEST_STOP_SLEEP"; fi
      rc="${OSM_TEST_STOP_RC:-0}"
      if [ "$rc" = 0 ]; then : > "$stopped"; fi
      exit "$rc" ;;
    disable)
      if [ -n "${OSM_TEST_DISABLE_SLEEP:-}" ]; then exec sleep "$OSM_TEST_DISABLE_SLEEP"; fi
      rc="${OSM_TEST_DISABLE_RC:-0}"
      if [ "$rc" = 0 ]; then : > "$stopped"; fi
      exit "$rc" ;;
  esac
done
exit 0
"#,
    )
    .unwrap();
    let mut perm = std::fs::metadata(&path).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perm, 0o755);
    std::fs::set_permissions(&path, perm).unwrap();
    dir.to_path_buf()
}
