//! Helpers shared by the integration suites.
//!
//! Every test binary that declares `mod common;` compiles this whole file, so
//! anything only one suite uses would otherwise be reported as dead code in
//! all the others.
#![allow(dead_code)]

use osm::tmux::Tmux;
use std::path::PathBuf;

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
