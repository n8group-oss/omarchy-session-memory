//! osm must capture non-ASCII paths verbatim even when its own environment
//! names no locale.
//!
//! Requiring tmux 3.7 removes only half of the corruption. tmux decides
//! whether to hand a command client raw UTF-8 or to sanitise it by looking at
//! *that client's* `LC_ALL` / `LC_CTYPE` / `LANG` for the substring "UTF-8".
//! With none of them set, tmux 3.7c still rewrites every non-ASCII byte and
//! every newline in `-F` output to `_` before osm sees it — so a pane in
//! `/home/u/żółć` is captured as `/home/u/______` and restored into that path,
//! on a fully supported tmux.
//!
//! That is not an exotic environment. A systemd user unit starts with almost
//! nothing in it, and a tmux hook runs `osm` with whatever the server has;
//! neither reliably carries a locale. It is the ordinary state of nearly every
//! capture this engine makes.
//!
//! The test therefore drives the real binary with a cleared environment,
//! which is the only way to observe what production actually does.

mod common;

use osm::tmux::Tmux;
use std::process::Command;

struct Env {
    dir: tempfile::TempDir,
    socket: String,
}

impl Env {
    fn new(label: &str) -> Self {
        Env {
            dir: tempfile::tempdir().unwrap(),
            socket: format!("osm-locale-{}-{}", label, std::process::id()),
        }
    }

    fn tmux(&self) -> Tmux {
        Tmux::with_socket(&self.socket)
    }

    /// The shipped binary, with **no locale in its environment at all** and
    /// its state redirected into this test's directory.
    fn osm_without_a_locale(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_osm"));
        // `env_clear`, not `env_remove` of the three locale variables: tmux
        // has several ways to decide a client is UTF-8 capable, and the point
        // of this test is the environment a systemd user unit actually starts
        // with, which is close to empty. Removing three names while leaving
        // the developer's interactive shell environment in place tested
        // nothing — it passed with the fix reverted.
        cmd.env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", self.dir.path())
            .env("XDG_STATE_HOME", self.dir.path().join("state"))
            .env("XDG_CONFIG_HOME", self.dir.path().join("config"))
            .arg("--socket")
            .arg(&self.socket);
        cmd
    }

    fn db(&self) -> std::path::PathBuf {
        self.dir.path().join("state/osm/state.db")
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        common::shutdown(&self.tmux());
    }
}

fn captured_cwds(db: &std::path::Path) -> Vec<String> {
    let conn = rusqlite::Connection::open(db).unwrap();
    let mut cwds: Vec<String> = conn
        .prepare("SELECT cwd FROM pane_rows")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    cwds.sort();
    cwds
}

#[test]
fn a_non_ascii_directory_survives_a_capture_made_with_no_locale_set() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("żółć");
    std::fs::create_dir(&dir).unwrap();
    let dir = dir.to_str().unwrap().to_string();

    let env = Env::new("utf8");
    env.tmux()
        .run(&["new-session", "-d", "-s", "dev", "-c", &dir])
        .unwrap();

    let out = env
        .osm_without_a_locale()
        .arg("snapshot")
        .output()
        .expect("run osm snapshot");
    assert!(
        out.status.success(),
        "osm snapshot failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert_eq!(
        captured_cwds(&env.db()),
        vec![dir],
        "a capture made with no locale in the environment must still record \
         the directory the pane is actually in"
    );
}

/// Same vector, one level up: a newline in a directory name is rewritten to
/// `_` by exactly the same code path in tmux.
#[test]
fn a_directory_containing_a_newline_survives_a_capture_made_with_no_locale_set() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("nl\ndir");
    std::fs::create_dir(&dir).unwrap();
    let dir = dir.to_str().unwrap().to_string();

    let env = Env::new("newline");
    env.tmux()
        .run(&["new-session", "-d", "-s", "dev", "-c", &dir])
        .unwrap();

    let out = env
        .osm_without_a_locale()
        .arg("snapshot")
        .output()
        .expect("run osm snapshot");
    assert!(
        out.status.success(),
        "osm snapshot failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert_eq!(captured_cwds(&env.db()), vec![dir]);
}
