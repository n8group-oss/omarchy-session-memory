//! osm refuses to run against a tmux that corrupts what it captures.
//!
//! tmux ≤ 3.6 rewrites newlines and **every non-ASCII byte** in `-F` format
//! output to `_` inside the server, before osm sees the value. A pane in
//! `/home/u/żółć` is therefore captured as `/home/u/______` and restored into
//! that path: the user's directory is not merely mangled in the database, it
//! is gone from the only record of where the pane was. Escaping cannot undo
//! damage done upstream, so the floor is enforced rather than worked around.

mod common;

use osm::tmux::{check_version, parse_version, Tmux, Version, MIN_VERSION};

struct Server(Tmux);

impl Server {
    fn start(label: &str) -> Self {
        Server(Tmux::with_socket(&format!(
            "osm-version-{}-{}",
            label,
            std::process::id()
        )))
    }
    fn t(&self) -> &Tmux {
        &self.0
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        common::shutdown(&self.0);
    }
}

#[test]
fn the_floor_is_three_seven() {
    assert_eq!(MIN_VERSION, Version { major: 3, minor: 7 });
}

#[test]
fn version_lines_tmux_actually_prints_are_parsed() {
    for (line, want) in [
        ("tmux 3.3a", (3, 3)),
        ("tmux 3.4", (3, 4)),
        ("tmux 3.5a", (3, 5)),
        ("tmux 3.7c", (3, 7)),
        ("tmux next-3.8", (3, 8)),
        ("tmux 4.0\n", (4, 0)),
    ] {
        assert_eq!(
            parse_version(line).unwrap(),
            Version {
                major: want.0,
                minor: want.1
            },
            "parsing {line:?}"
        );
    }
}

/// "We could not tell how old this tmux is" is exactly the state in which the
/// corruption above happens unnoticed, so it is an error, never an optimistic
/// pass.
#[test]
fn an_unrecognisable_version_line_is_an_error() {
    for line in ["tmux master", "tmux openbsd-7.4", "", "tmux", "tmux 3."] {
        let err = parse_version(line).unwrap_err().to_string();
        assert!(
            err.contains("cannot tell which tmux version"),
            "{line:?} should be rejected, got {err:?}"
        );
    }
}

/// The versions Debian bookworm, Ubuntu and Debian trixie ship. All of them
/// destroy non-ASCII paths, so all of them are refused — by name, so the
/// operator does not have to go and look up what they are running.
#[test]
fn every_tmux_below_the_floor_is_refused_by_name() {
    for line in ["tmux 3.3a", "tmux 3.4", "tmux 3.5a", "tmux 3.6", "tmux 2.9"] {
        let err = check_version(line).unwrap_err().to_string();
        assert!(
            err.contains(line),
            "the refusal must name the tmux that was found, got {err:?}"
        );
        assert!(
            err.contains("3.7"),
            "the refusal must name the version required, got {err:?}"
        );
        assert!(
            err.contains("żółć"),
            "the refusal must say what an older tmux does to a non-ASCII path, got {err:?}"
        );
    }
}

#[test]
fn a_supported_tmux_is_accepted() {
    for line in ["tmux 3.7", "tmux 3.7c", "tmux next-3.8", "tmux 4.1"] {
        check_version(line).unwrap_or_else(|e| panic!("{line:?} must be accepted: {e:#}"));
    }
}

/// The check is wired to a real server, not just to a string: `-V` answers
/// before anything has been started on the socket, which is the state the boot
/// restore finds.
#[test]
fn the_running_tmux_is_checked_through_the_socket() {
    let src = Server::start("live");
    let reported = src.t().version_string().expect("tmux -V");
    assert!(
        reported.starts_with("tmux "),
        "unexpected -V output {reported:?}"
    );
    let found = src
        .t()
        .require_supported_version()
        .expect("the test suite must run on a supported tmux");
    assert!(found >= MIN_VERSION);
}
