//! Enforcement: no test may address the **default** tmux server.
//!
//! Reaching the default server once destroyed a live 7-session, 24-pane
//! development environment. Two things stop it now:
//!
//! 1. **Structurally.** `Tmux`'s socket field is private and its socket-less
//!    constructor, `Tmux::default_server()`, exists only under the
//!    `default-server` feature. CI runs the suite with
//!    `--no-default-features`, where that constructor is not compiled at all,
//!    so a test cannot name the default server even if it tries.
//! 2. **By this test**, which runs *with* the tests rather than after them
//!    (the CI grep it replaces ran after the test step, i.e. after any
//!    destruction had already happened) and which knows about this project's
//!    own API — the previous grep only looked for `Command::new("tmux")` and
//!    waved `Tmux::default()` straight through.
//!
//! What neither catches: a test that shells out to tmux through an indirection
//! this file cannot see (a variable holding the string "tmux", a helper script,
//! a `sh -c` line), or a socket name that collides with a real user socket.

use std::fs;
use std::path::{Path, PathBuf};

fn rust_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(rust_files(&path));
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
    out.sort();
    out
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// This file is full of the very strings it hunts for.
fn is_this_file(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|n| n == "no_default_server.rs")
}

#[test]
fn no_test_can_reach_the_default_tmux_server() {
    // Built as one string so the guard does not trip over its own needles.
    let banned_in_tests: [(&str, &str); 4] = [
        (
            "Tmux::default_server",
            "constructs a Tmux aimed at the user's real tmux server",
        ),
        (
            "Tmux::default(",
            "the old socket-less constructor; use Tmux::with_socket",
        ),
        (
            "Tmux { socket",
            "a struct literal would bypass with_socket (the field is private, so this cannot compile — flagged anyway)",
        ),
        (
            "Command::new(\"tmux\")",
            "spawns tmux directly; go through Tmux::with_socket so -L is always passed",
        ),
    ];

    let mut violations: Vec<String> = Vec::new();

    for path in rust_files(&repo_root().join("tests")) {
        if is_this_file(&path) {
            continue;
        }
        let text = fs::read_to_string(&path).unwrap();
        for (line_no, line) in text.lines().enumerate() {
            for (needle, why) in banned_in_tests {
                if line.contains(needle) {
                    violations.push(format!(
                        "{}:{}: {needle} — {why}",
                        path.display(),
                        line_no + 1
                    ));
                }
            }
        }
    }

    // TMUX_TMPDIR does not isolate tmux on every version (it is ignored by
    // the tmux on this developer's machine), so it must not be used as an
    // isolation mechanism anywhere in the project.
    for dir in ["tests", "src"] {
        for path in rust_files(&repo_root().join(dir)) {
            if is_this_file(&path) {
                continue;
            }
            let text = fs::read_to_string(&path).unwrap();
            for (line_no, line) in text.lines().enumerate() {
                if line.contains("TMUX_TMPDIR") {
                    violations.push(format!(
                        "{}:{}: TMUX_TMPDIR — does not isolate tmux on all versions; use -L",
                        path.display(),
                        line_no + 1
                    ));
                }
            }
        }
    }

    assert!(
        violations.is_empty(),
        "these tests could reach the developer's real tmux server:\n{}",
        violations.join("\n")
    );
}

/// Every `Tmux` a test can build carries a socket — there is no other public
/// constructor.
#[test]
fn the_only_public_constructor_takes_a_socket() {
    let t = osm::tmux::Tmux::with_socket("osm-guard-socket");
    assert_eq!(t.socket(), Some("osm-guard-socket"));
}
