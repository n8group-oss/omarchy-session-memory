//! The hooks have to survive a tmux server that does not.
//!
//! `set-hook -g` is server state. It lives in the tmux server process and
//! dies with it — nothing writes it to disk, and no unit re-applies it. So
//! the fifteen hooks an install registers are gone the moment the user's
//! last client detaches and the server exits, or the moment tmux is
//! restarted for any other reason, and from then on the only thing capturing
//! anything is the daemon's fallback timer. That is 120 seconds by default:
//! everything a user does in the two minutes before a reboot is lost, while
//! `osm status` reports a perfectly healthy engine.
//!
//! Nothing else in the suite could have caught it. `tests/hooks.rs` installs
//! hooks and fires events on **one** server, which is exactly the case that
//! works.
//!
//! Two halves, so both directions are covered:
//!
//! * the daemon re-registers the hooks when a server appears or is replaced,
//!   and the events on the *new* server capture again;
//! * `osm install` no longer fails half way through when there is no server
//!   to register them on — which is the ordinary state of a machine that
//!   installs the engine before starting tmux, and which used to leave the
//!   binary and both units on disk behind a non-zero exit.
//!
//! # Isolation
//!
//! Every tmux server here is a scratch server on its own `-L` socket, killed
//! and unlinked by `common::Env`'s `Drop`. The daemon under test is spawned
//! with `--socket` and a private `XDG_STATE_HOME`, and the hooks it installs
//! run `osm` as a child of the *tmux server*, which inherits neither — so the
//! server is handed the same three values through `set-environment -g` before
//! any hook can fire. Without that, a hook child would capture into the
//! developer's real database.

mod common;

use osm::tmux::Tmux;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// A spawned `osm daemon`, killed when the test ends however it ends.
struct Daemon {
    child: Child,
    log: std::path::PathBuf,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Daemon {
    /// What the daemon has said on stdout and stderr so far, for a failing
    /// assertion to quote. A daemon that refused to start at all is the most
    /// likely reason for a test here to fail, and its own message says why.
    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }
}

/// The engine's own directories, and the tmux socket, for this test only.
struct World {
    env: common::Env,
}

impl World {
    fn new(label: &str) -> Self {
        let env = common::Env::new(label);
        let world = World { env };
        // The fallback timer must never fire during these tests: a timer
        // capture would be indistinguishable from the event-driven capture
        // that is the whole subject here. `debounce_max_latency_secs` is the
        // floor the config allows, so one event's capture cannot swallow the
        // next one's.
        std::fs::write(
            world.config_home().join("osm/config.toml"),
            "[restore]\nplace_windows = false\n\
             [capture]\nfallback_interval_secs = 3600\ndebounce_max_latency_secs = 1\n",
        )
        .unwrap();
        world
    }

    fn state_home(&self) -> std::path::PathBuf {
        self.env.dir.path().join("state")
    }

    fn config_home(&self) -> std::path::PathBuf {
        self.env.dir.path().join("config")
    }

    fn db(&self) -> std::path::PathBuf {
        self.state_home().join("osm/state.db")
    }

    fn server(&self) -> Tmux {
        self.env.server()
    }

    /// An `osm` command line carrying this test's socket, state directory,
    /// configuration and `HOME` — the last so that even a code path that
    /// resolved a default prefix or a default state directory lands inside
    /// the temporary directory and not in the developer's home.
    fn osm(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_osm"));
        cmd.env("HOME", self.env.dir.path())
            .env("XDG_STATE_HOME", self.state_home())
            .env("XDG_CONFIG_HOME", self.config_home())
            .arg("--socket")
            .arg(&self.env.socket);
        cmd
    }

    /// Hand the tmux server the three values a hook child needs, none of
    /// which it can inherit from a test process that has them set only on the
    /// commands it spawns.
    ///
    /// `set-environment -g` reaches `run-shell` children — verified against
    /// tmux 3.7c — so a hook firing on this server captures into this test's
    /// database, against this test's server, and never the real ones.
    fn pin_server_environment(&self) {
        for (key, value) in [
            ("XDG_STATE_HOME", self.state_home()),
            ("XDG_CONFIG_HOME", self.config_home()),
            ("HOME", self.env.dir.path().to_path_buf()),
            (
                "OSM_TMUX_SOCKET",
                std::path::PathBuf::from(&self.env.socket),
            ),
        ] {
            self.env
                .tmux(&["set-environment", "-g", key, &value.display().to_string()]);
        }
    }

    fn spawn_daemon(&self) -> Daemon {
        let log = self.env.dir.path().join("daemon.log");
        let out = std::fs::File::create(&log).unwrap();
        let err = out.try_clone().unwrap();
        let child = self
            .osm()
            .arg("daemon")
            .stdout(Stdio::from(out))
            .stderr(Stdio::from(err))
            .spawn()
            .expect("spawn osm daemon");
        Daemon { child, log }
    }
}

/// How many of this engine's own hooks are set on `t` right now.
///
/// Counted from `show-hooks -g`, which is the same listing `hooks::uninstall`
/// relies on, so a hook set under a name tmux never lists counts as absent
/// here too — which is what it is.
fn marked_hooks(t: &Tmux) -> usize {
    t.run(&["show-hooks", "-g"])
        .map(|s| {
            s.lines()
                .filter(|l| l.contains(osm::hooks::HOOK_MARKER))
                .count()
        })
        .unwrap_or(0)
}

/// Every snapshot's reason, oldest first. A missing database means nothing
/// has been captured yet, which is a legitimate state and not an error.
fn reasons(db: &Path) -> Vec<String> {
    if !db.exists() {
        return Vec::new();
    }
    let Ok(conn) = osm::db::open(db) else {
        return Vec::new();
    };
    let mut stmt = conn
        .prepare("SELECT reason FROM snapshots ORDER BY id")
        .unwrap();
    let rows: Vec<String> = stmt
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    rows
}

/// Poll `f` until it is true or `timeout` runs out.
fn wait_for(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if f() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The daemon has to notice a server it did not see when it started, and a
/// server that replaced the one it did see. Both are the same event from its
/// side: the incarnation on the socket is not the one it registered hooks on.
///
/// The reboot this protects: the user's tmux server is restarted (or simply
/// exits when the last client detaches and comes back later), every hook goes
/// with it, and the next two minutes of work exist only in a running server
/// nothing is watching.
#[test]
fn the_daemon_registers_the_hooks_again_after_the_tmux_server_restarts() {
    let world = World::new("hookheal");
    let want = osm::hooks::HOOKED_EVENTS.len();

    world.env.tmux(&["new-session", "-d", "-s", "one"]);
    world.pin_server_environment();

    let daemon = world.spawn_daemon();

    assert!(
        wait_for(Duration::from_secs(30), || marked_hooks(&world.server())
            == want),
        "the daemon never registered its hooks on the server that was already \
         running: {} of {want} present\n--- daemon log ---\n{}",
        marked_hooks(&world.server()),
        daemon.log()
    );

    // The server goes away and a different one takes its place on the same
    // socket — a new process, a new incarnation, and none of the hooks.
    let _ = world.server().run(&["kill-server"]);
    assert!(
        wait_for(Duration::from_secs(10), || !world.server().server_running()),
        "the scratch tmux server did not go away"
    );
    world.env.tmux(&["new-session", "-d", "-s", "two"]);
    world.pin_server_environment();
    assert_eq!(
        marked_hooks(&world.server()),
        0,
        "a fresh tmux server cannot already carry hooks; this test is not \
         testing what it claims to"
    );

    assert!(
        wait_for(Duration::from_secs(30), || marked_hooks(&world.server())
            == want),
        "the hooks did not come back after the tmux server was replaced: {} of \
         {want} present, so every change until the next fallback capture is \
         lost\n--- daemon log ---\n{}",
        marked_hooks(&world.server()),
        daemon.log()
    );

    // Present in `show-hooks` is not the same as firing: an earlier version of
    // this project shipped a hook name tmux accepted and never ran. So make a
    // change on the *replacement* server and require a capture for it.
    let before = reasons(&world.db()).len();
    world.env.tmux(&["split-window", "-d", "-t", "two"]);
    assert!(
        wait_for(Duration::from_secs(20), || reasons(&world.db()).len()
            > before),
        "no capture followed a split on the replacement server: reasons={:?}\
         \n--- daemon log ---\n{}",
        reasons(&world.db()),
        daemon.log()
    );
    let last = reasons(&world.db()).pop().unwrap();
    assert!(
        osm::hooks::HOOKED_EVENTS.contains(&last.as_str()),
        "the capture was not event-driven (reason={last:?}); the fallback timer \
         is configured for an hour, so this should have been a hooked event"
    );
}

/// Installing before tmux is ever started is the ordinary case — the plugin
/// is installed once, and the user starts a terminal afterwards. It used to
/// write the binary and both unit files and *then* fail, because there was no
/// server to set a hook on, so a successful install reported failure and a
/// script that checked the exit status stopped there.
#[test]
fn install_succeeds_when_no_tmux_server_is_running() {
    let world = World::new("noserver");
    let prefix = world.env.dir.path().join("prefix");

    assert!(
        !world.server().server_running(),
        "this test's premise is that no server is running on its socket"
    );

    let out = world
        .osm()
        .args(["install", "--prefix"])
        .arg(&prefix)
        .output()
        .expect("spawn osm install");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();

    assert!(
        out.status.success(),
        "install failed with no tmux server running ({}): {stderr}\n{stdout}",
        out.status
    );
    assert!(
        prefix.join("bin/osm").exists(),
        "the binary was not installed: {stdout}"
    );
    assert!(
        stdout.to_lowercase().contains("hook"),
        "an install that could not register hooks must say so rather than \
         leave the user thinking they are set: {stdout}"
    );
    assert!(
        !world.server().server_running(),
        "installing must not start a tmux server"
    );
}
