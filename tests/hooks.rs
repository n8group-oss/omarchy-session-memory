//! The tmux hooks, checked against a real tmux server rather than against
//! `show-hooks` text.
//!
//! Two ways a hook can be wrong while looking right:
//!
//! * It installs under a name tmux accepts but never lists — an earlier
//!   version of this project shipped `window-renamed`, which sets without
//!   error and never appears in `show-hooks -g`, so `uninstall` could never
//!   find it again.
//! * It installs with a command that cannot execute. The binary path used to
//!   be interpolated raw into a tmux *single*-quoted token, so an install
//!   path containing a space or an apostrophe registered fine and then ran a
//!   truncated path that does not exist. A test that only reads `show-hooks`
//!   output cannot tell the difference.
//!
//! So the tests below install hooks from a path containing both a space and
//! an apostrophe, fire every event that can be fired without a terminal, and
//! require a real snapshot to land in a real database each time.

mod common;

use osm::{hooks, tmux::Tmux};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

struct Env {
    dir: tempfile::TempDir,
    socket: String,
    tmux: Tmux,
}

impl Drop for Env {
    fn drop(&mut self) {
        common::shutdown(&self.tmux);
    }
}

impl Env {
    fn new(label: &str) -> Self {
        let socket = format!("osm-hooks-{}-{}", label, std::process::id());
        let tmux = Tmux::with_socket(&socket);
        Env {
            dir: tempfile::tempdir().unwrap(),
            socket,
            tmux,
        }
    }

    fn t(&self) -> &Tmux {
        &self.tmux
    }

    fn db(&self) -> PathBuf {
        self.dir.path().join("state/osm/state.db")
    }

    /// A wrapper around the real `osm` binary, installed at a path holding
    /// **a space and an apostrophe** — the two characters that broke the
    /// unquoted hook command.
    ///
    /// It also pins the engine's XDG directories and tmux socket, which a
    /// hook process could not otherwise inherit, and re-issues the capture
    /// without `--debounced` so each event under test records its own
    /// snapshot instead of being throttled by the one before it (debouncing
    /// itself is covered by `tests/debounce.rs`). It refuses to do any of
    /// that unless the hook handed it exactly the argument list the engine
    /// installs, so a mangled command cannot pass by accident.
    fn wrapper(&self) -> PathBuf {
        let dir = self.dir.path().join("bin dir/it's here");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("osm-wrapper");
        let body = format!(
            "#!/bin/sh\n\
             [ \"$1\" = snapshot ] || exit 64\n\
             [ \"$2\" = --debounced ] || exit 65\n\
             [ \"$3\" = --reason ] || exit 66\n\
             XDG_STATE_HOME=\"{state}\" XDG_CONFIG_HOME=\"{config}\" \
             exec \"{bin}\" --socket \"{socket}\" snapshot --reason \"$4\"\n",
            state = self.dir.path().join("state").display(),
            config = self.dir.path().join("config").display(),
            bin = env!("CARGO_BIN_EXE_osm"),
            socket = self.socket,
        );
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        path
    }
}

fn hook_text(t: &Tmux) -> String {
    t.run(&["show-hooks", "-g"]).unwrap()
}

/// Snapshot reasons recorded so far. A missing database simply means no
/// capture has happened yet.
fn reasons(db: &Path) -> Vec<String> {
    if !db.exists() {
        return Vec::new();
    }
    let conn = osm::db::open(db).unwrap();
    let mut stmt = conn.prepare("SELECT reason FROM snapshots").unwrap();
    let rows: Vec<String> = stmt
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    rows
}

/// The newest snapshot's rowid, or 0 when nothing has been captured.
///
/// A *monotonic* marker, unlike a row count: retention keeps only the newest
/// `keep_snapshots` (20 by default), so once a test has fired twenty events
/// every further capture prunes an older row and the count stops growing.
/// That is exactly what happened on tmux 3.3a, which fires more hooks per
/// command than 3.7c and so reached the cap inside this test — the capture
/// had happened, the count just could not show it.
fn latest_snapshot_id(db: &Path) -> i64 {
    if !db.exists() {
        return 0;
    }
    let conn = osm::db::open(db).unwrap();
    conn.query_row("SELECT COALESCE(MAX(id), 0) FROM snapshots", [], |r| {
        r.get(0)
    })
    .unwrap()
}

/// Hooks run in the background (`run-shell -b`), so the capture lands some
/// time after the tmux command returns. Waits until a snapshot newer than
/// `was` exists, or gives up.
///
/// Deliberately identifies captures by rowid rather than by matching
/// `reason`. One tmux command fires several hooks — `new-session` fires both
/// `session-created` and `window-linked` — and the debounce correctly
/// collapses them into a single capture, which records whichever hook reached
/// the binary first. That ordering differs across tmux versions: on 3.7c
/// `session-created` wins, on 3.3a `window-linked` does. Asserting the reason
/// would therefore test tmux's hook ordering rather than this project's
/// behaviour. What matters is that firing the event produced a capture at
/// all.
fn wait_for_snapshot_after(db: &Path, was: i64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if latest_snapshot_id(db) > was {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

#[test]
fn install_adds_one_marked_hook_per_event() {
    let env = Env::new("install");
    env.t()
        .run(&["new-session", "-d", "-s", "alpha", "-c", "/tmp"])
        .unwrap();
    let n = hooks::install(env.t(), "/usr/bin/osm").unwrap();
    assert_eq!(n, hooks::HOOKED_EVENTS.len());

    let text = hook_text(env.t());
    for event in hooks::HOOKED_EVENTS {
        // At the start of a line, not merely somewhere in the text: an event
        // tmux accepts but never lists (`window-renamed` was one) would
        // otherwise pass on a substring of some other hook's command, and
        // `uninstall` could never find it again.
        assert!(
            text.lines()
                .any(|l| l.starts_with(event) && l[event.len()..].starts_with(['[', ' '])),
            "tmux does not list a hook for {event}; it cannot be uninstalled:\n{text}"
        );
    }
    assert_eq!(
        text.matches(hooks::HOOK_MARKER).count(),
        hooks::HOOKED_EVENTS.len()
    );
}

#[test]
fn install_is_idempotent() {
    let env = Env::new("idem");
    env.t()
        .run(&["new-session", "-d", "-s", "alpha", "-c", "/tmp"])
        .unwrap();
    hooks::install(env.t(), "/usr/bin/osm").unwrap();
    hooks::install(env.t(), "/usr/bin/osm").unwrap();
    let text = hook_text(env.t());
    assert_eq!(
        text.matches(hooks::HOOK_MARKER).count(),
        hooks::HOOKED_EVENTS.len(),
        "re-installing must not duplicate hooks"
    );
}

#[test]
fn uninstall_removes_only_our_hooks() {
    let env = Env::new("uninstall");
    env.t()
        .run(&["new-session", "-d", "-s", "alpha", "-c", "/tmp"])
        .unwrap();
    env.t()
        .run(&[
            "set-hook",
            "-g",
            "-a",
            "session-created",
            "run-shell 'echo user-hook >> /dev/null'",
        ])
        .unwrap();

    hooks::install(env.t(), "/usr/bin/osm").unwrap();
    let removed = hooks::uninstall(env.t()).unwrap();
    assert_eq!(removed, hooks::HOOKED_EVENTS.len());

    let text = hook_text(env.t());
    assert!(!text.contains(hooks::HOOK_MARKER), "our hooks are gone");
    assert!(text.contains("user-hook"), "the user's hook survives");
}

/// Item 8: an install path with a space *and* an apostrophe.
///
/// Before the quoting fix this installed without error and then executed a
/// path that does not exist, so the hook captured nothing — invisibly,
/// because hooks send all output to `/dev/null`.
#[test]
fn a_hook_installed_from_a_path_with_a_space_and_an_apostrophe_captures() {
    let env = Env::new("quoting");
    let t = env.t();
    t.run(&["new-session", "-d", "-s", "base", "-c", "/tmp"])
        .unwrap();

    let wrapper = env.wrapper();
    let path = wrapper.to_str().unwrap();
    assert!(path.contains(' ') && path.contains('\''), "{path}");
    hooks::install(t, path).unwrap();

    let before = latest_snapshot_id(&env.db());
    t.run(&["new-session", "-d", "-s", "alpha", "-c", "/tmp"])
        .unwrap();

    // Count rather than match the reason: `new-session` fires several hooks
    // and the debounce records whichever arrives first, which is not stable
    // across tmux versions. What this test is about is whether a hook whose
    // command contains a space and an apostrophe executes at all.
    assert!(
        wait_for_snapshot_after(&env.db(), before),
        "the hook never captured anything; recorded reasons: {:?}",
        reasons(&env.db())
    );
}

/// Every hooked event that can be fired without a terminal, fired for real
/// and required to produce a snapshot.
///
/// `client-attached` and `client-detached` are the two exceptions: firing
/// them needs a tmux client on a pty, which this suite has no way to create
/// without shelling out to tmux behind the project's own `Tmux` type (see
/// `tests/no_default_server.rs`). They are still covered by
/// `install_adds_one_marked_hook_per_event`, which is what catches an event
/// name tmux silently ignores.
#[test]
fn firing_each_hooked_event_produces_a_snapshot() {
    let env = Env::new("fire");
    let t = env.t();
    t.run(&[
        "new-session",
        "-d",
        "-s",
        "base",
        "-n",
        "keep",
        "-c",
        "/tmp",
    ])
    .unwrap();
    hooks::install(t, env.wrapper().to_str().unwrap()).unwrap();

    // Each step: fire the event, then wait for its snapshot before firing
    // the next, so the captures never queue up behind each other.
    let fire = |event: &str, args: &[&[&str]]| {
        let before = latest_snapshot_id(&env.db());
        for a in args {
            t.run(a)
                .unwrap_or_else(|e| panic!("firing {event} with {a:?}: {e:#}"));
        }
        assert!(
            wait_for_snapshot_after(&env.db(), before),
            "firing {event} produced no snapshot; newest snapshot id stayed at \
             {before}; recorded reasons: {:?}",
            reasons(&env.db())
        );
    };

    fire(
        "session-created",
        &[&[
            "new-session",
            "-d",
            "-s",
            "alpha",
            "-n",
            "main",
            "-c",
            "/tmp",
        ]],
    );
    fire(
        "after-split-window",
        &[&["split-window", "-d", "-t", "alpha:main", "-c", "/tmp"]],
    );
    fire(
        "after-select-pane",
        &[&["select-pane", "-t", "alpha:main", "-U"]],
    );
    fire(
        "after-resize-pane",
        &[&["resize-pane", "-t", "alpha:main", "-y", "5"]],
    );
    // The two events the hook set was missing: a `select-layout tiled` or a
    // window resize changes the topology and used to be lost to a reboot
    // that beat the 120s fallback timer.
    fire(
        "after-select-layout",
        &[&["select-layout", "-t", "alpha:main", "tiled"]],
    );
    fire(
        "after-resize-window",
        &[&["resize-window", "-t", "alpha:main", "-x", "120", "-y", "40"]],
    );
    fire(
        "after-rename-window",
        &[&["rename-window", "-t", "alpha:main", "renamed"]],
    );
    fire("after-kill-pane", &[&["kill-pane", "-t", "alpha:renamed"]]);
    fire(
        "after-select-window",
        &[
            &[
                "new-window",
                "-d",
                "-t",
                "alpha",
                "-n",
                "second",
                "-c",
                "/tmp",
            ],
            &["select-window", "-t", "alpha:second"],
        ],
    );
    fire(
        "window-linked",
        &[&["link-window", "-d", "-s", "alpha:second", "-t", "base:"]],
    );

    let linked_idx = t
        .run(&[
            "list-windows",
            "-t",
            "base",
            "-F",
            "#{window_index} #{window_name}",
        ])
        .unwrap()
        .lines()
        .find(|l| l.ends_with(" second"))
        .and_then(|l| l.split_whitespace().next().map(str::to_string))
        .expect("the linked window must be in base");
    fire(
        "window-unlinked",
        &[&["unlink-window", "-t", &format!("base:{linked_idx}")]],
    );

    fire(
        "session-renamed",
        &[&["rename-session", "-t", "alpha", "alpha2"]],
    );
    // Last, and never the last session: killing the server out from under a
    // background capture would fail the capture, not the hook.
    fire("session-closed", &[&["kill-session", "-t", "alpha2"]]);
}
