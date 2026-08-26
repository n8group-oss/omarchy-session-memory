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
