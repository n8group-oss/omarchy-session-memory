//! Two engines registering the tmux hooks at the same time.
//!
//! `hooks::install` is uninstall-then-append: it removes every hook carrying
//! the marker and then appends fifteen new ones. Both halves ran unlocked, so
//! two engines could each read a server with no marked hooks and each append
//! its own fifteen. That is not a hypothetical shape — it is this system's
//! ordinary one. `osm.service` re-registers on every new tmux server
//! (`ensure_hooks`, a five-second tick) and a user typing `osm install` or
//! `osm install-hooks` does the same thing at whatever moment they type it.
//!
//! What a duplicate costs: every copy fires its own `osm snapshot
//! --debounced` on every tmux event, and the debounce check happens *before*
//! the capture lock, so a burst of them all decide they are due and all
//! proceed. One `select-pane` becomes N captures on a machine that already
//! had one, and each of them reported `{"installed":15}` on the way in.
//!
//! So the test is a stress run, deliberately. A single-threaded install /
//! uninstall pair proves the arithmetic, which was never in doubt; only
//! concurrent registrations can show whether the mutation is serialised.
//!
//! # Why nothing here can reach the developer's tmux server
//!
//! Every server is this suite's own `-L` socket, and the hooks that get
//! registered on it name the real `osm` binary with no `--socket` — which is
//! what `osm install-hooks` really installs, and the reason this file cannot
//! use the wrapper trick `tests/hooks.rs` uses. Those hooks do fire: killing
//! the server at the end of a test is a `session-closed`.
//!
//! So the server is given the isolation in its own environment, which
//! `run-shell` inherits (verified on tmux 3.7c): `OSM_TMUX_SOCKET` pointing
//! back at this test's socket, and `XDG_STATE_HOME` / `XDG_CONFIG_HOME`
//! pointing into its temporary directory. A hook that fires here talks to
//! this server and writes to this database, and it would do so even in a
//! build that still had the `default-server` feature compiled in.

mod common;

use osm::hooks::HOOK_MARKER;
use std::collections::BTreeMap;

/// How many engines register at once. Larger than any real machine would
/// have, because the window this closes is small: the point is to lose the
/// race reliably, not realistically.
const RACERS: usize = 12;

/// How many bursts. A race that is lost one time in three is a test that
/// passes two runs out of three, and a flaky guard on a defect this expensive
/// is worse than none.
const ROUNDS: usize = 3;

/// Bursts for the mixed install/uninstall race, which converges on its own
/// often enough that three were not sure to catch it: measured against the
/// unlocked code, three rounds passed one run in five and six passed none.
const MIXED_ROUNDS: usize = 6;

/// Give this test's tmux server the isolation its hooks need.
///
/// The hook command `osm install-hooks` registers carries no `--socket` and no
/// XDG directories — it is the real one, and pointing it somewhere else is
/// exactly what would stop this test from testing the real one. `run-shell`
/// runs with the server's environment, so the server is where the isolation
/// goes. Set before any hook is registered, and before any event can fire.
fn isolate_server(env: &common::Env) {
    for (key, value) in [
        ("OSM_TMUX_SOCKET", env.socket.clone()),
        (
            "XDG_STATE_HOME",
            env.dir.path().join("state").display().to_string(),
        ),
        (
            "XDG_CONFIG_HOME",
            env.dir.path().join("config").display().to_string(),
        ),
    ] {
        env.tmux(&["set-environment", "-g", key, &value]);
    }
}

/// The marked hooks on this test's server, counted per event.
fn marked_hooks(env: &common::Env) -> BTreeMap<String, usize> {
    let shown = env.tmux(&["show-hooks", "-g"]);
    let mut out = BTreeMap::new();
    for line in shown.lines().filter(|l| l.contains(HOOK_MARKER)) {
        let head = line.split_whitespace().next().unwrap_or_default();
        let event = head.split_once('[').map_or(head, |(e, _)| e).to_string();
        *out.entry(event).or_insert(0) += 1;
    }
    out
}

fn total(counts: &BTreeMap<String, usize>) -> usize {
    counts.values().sum()
}

/// Run `osm install-hooks` `RACERS` times at once and wait for them all.
///
/// Spawned rather than run one at a time, and the whole batch is launched
/// before any of it is waited on, so the fifteen `set-hook` round trips of one
/// engine overlap the `show-hooks` read of the next.
fn race_installs(env: &common::Env) -> Vec<String> {
    let state = env.dir.path().join("state");
    let config = env.dir.path().join("config");
    let children: Vec<_> = (0..RACERS)
        .map(|_| {
            let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_osm"));
            cmd.env("XDG_STATE_HOME", &state)
                .env("XDG_CONFIG_HOME", &config)
                .args(["--socket", &env.socket, "install-hooks"])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            cmd.spawn().expect("spawn osm install-hooks")
        })
        .collect();

    children
        .into_iter()
        .map(|c| {
            let out = c.wait_with_output().expect("wait for osm install-hooks");
            format!(
                "status={} stdout={} stderr={}",
                out.status,
                String::from_utf8_lossy(&out.stdout).trim(),
                String::from_utf8_lossy(&out.stderr).trim()
            )
        })
        .collect()
}

#[test]
fn concurrent_registrations_leave_exactly_one_hook_per_event() {
    let env = common::Env::new("hookrace");
    // A server to hold the hooks. Hooks are server state and die with it.
    env.tmux(&["new-session", "-d", "-s", "dev", "-c", "/tmp"]);
    isolate_server(&env);

    for round in 1..=ROUNDS {
        let reports = race_installs(&env);
        let counts = marked_hooks(&env);

        let doubled: Vec<String> = counts
            .iter()
            .filter(|(_, n)| **n != 1)
            .map(|(e, n)| format!("{e}: {n}"))
            .collect();
        assert!(
            doubled.is_empty(),
            "round {round}: {RACERS} concurrent registrations left more than one \
             marked hook on an event, so one tmux event now launches that many \
             captures:\n  {}\ntotal marked hooks: {} (should be {})\nengine output:\n  {}",
            doubled.join("\n  "),
            total(&counts),
            osm::hooks::HOOKED_EVENTS.len(),
            reports.join("\n  ")
        );
        assert_eq!(
            total(&counts),
            osm::hooks::HOOKED_EVENTS.len(),
            "round {round}: the server should carry exactly one marked hook per \
             hooked event; it carries {counts:?}"
        );
    }
}

/// Installs and uninstalls racing each other must still leave a whole set or
/// none — never one engine's hooks half-removed by another's uninstall.
///
/// The end state here is deterministic because the last writer wins: the
/// assertion is on the *shape*, which is the thing an unlocked
/// read-modify-write breaks. A count that is neither 0 nor 15, or any event
/// carrying two, means the two mutations interleaved.
#[test]
fn installs_and_uninstalls_racing_leave_a_whole_set_or_none() {
    let env = common::Env::new("hookmix");
    env.tmux(&["new-session", "-d", "-s", "dev", "-c", "/tmp"]);
    isolate_server(&env);

    let state = env.dir.path().join("state");
    let config = env.dir.path().join("config");
    for round in 1..=MIXED_ROUNDS {
        let children: Vec<_> = (0..RACERS)
            .map(|i| {
                let sub = if i % 3 == 2 {
                    "uninstall-hooks"
                } else {
                    "install-hooks"
                };
                let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_osm"));
                cmd.env("XDG_STATE_HOME", &state)
                    .env("XDG_CONFIG_HOME", &config)
                    .args(["--socket", &env.socket, sub])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null());
                cmd.spawn().expect("spawn osm")
            })
            .collect();
        for mut c in children {
            let _ = c.wait();
        }

        let counts = marked_hooks(&env);
        let n = total(&counts);
        assert!(
            n == 0 || n == osm::hooks::HOOKED_EVENTS.len(),
            "round {round}: {n} marked hooks left, which is neither a whole set \
             ({}) nor none: {counts:?}",
            osm::hooks::HOOKED_EVENTS.len()
        );
        assert!(
            counts.values().all(|c| *c == 1),
            "round {round}: an event carries more than one marked hook: {counts:?}"
        );
    }
}

/// The same race, between engines whose **osm state directories differ**.
///
/// The lock used to be `$XDG_STATE_HOME/osm/hooks.lock`, and every process in
/// the tests above was handed the same `XDG_STATE_HOME` — so the guard held
/// in the suite and not on the machine. `XDG_STATE_HOME` says where osm keeps
/// its own data; it says nothing about which tmux server a process is talking
/// to. A daemon running on the ordinary state directory and a
/// `XDG_STATE_HOME=/tmp/whatever osm install-hooks` against the same server
/// took two different locks, neither excluded the other, and the
/// remove-then-append race ran in full: both reported `installed: 15`, and
/// the server was left firing two captures for every tmux event.
///
/// So every racer here gets a state directory of its own, and the only thing
/// they have in common is the server — which is the only thing that ought to
/// decide who waits for whom.
///
/// The server is this suite's own `-L` socket, as everywhere else in this
/// file; nothing here can reach the developer's tmux server or add a hook to
/// it.
#[test]
fn engines_with_different_state_directories_still_serialise_on_the_server() {
    let env = common::Env::new("hookxdg");
    env.tmux(&["new-session", "-d", "-s", "dev", "-c", "/tmp"]);
    isolate_server(&env);

    let config = env.dir.path().join("config");
    for round in 1..=ROUNDS {
        let children: Vec<_> = (0..RACERS)
            .map(|i| {
                // One state directory per engine. Nothing else differs.
                let state = env.dir.path().join(format!("state-{i}"));
                let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_osm"));
                cmd.env("XDG_STATE_HOME", &state)
                    .env("XDG_CONFIG_HOME", &config)
                    .args(["--socket", &env.socket, "install-hooks"])
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped());
                cmd.spawn().expect("spawn osm install-hooks")
            })
            .collect();
        let reports: Vec<String> = children
            .into_iter()
            .map(|c| {
                let out = c.wait_with_output().expect("wait for osm install-hooks");
                format!(
                    "status={} stdout={} stderr={}",
                    out.status,
                    String::from_utf8_lossy(&out.stdout).trim(),
                    String::from_utf8_lossy(&out.stderr).trim()
                )
            })
            .collect();

        let counts = marked_hooks(&env);
        let doubled: Vec<String> = counts
            .iter()
            .filter(|(_, n)| **n != 1)
            .map(|(e, n)| format!("{e}: {n}"))
            .collect();
        assert!(
            doubled.is_empty(),
            "round {round}: {RACERS} engines with different XDG_STATE_HOME values \
             raced on one tmux server and left more than one marked hook on an \
             event, so a single tmux event now launches that many \
             captures:\n  {}\ntotal marked hooks: {} (should be {})\nengine output:\n  {}",
            doubled.join("\n  "),
            total(&counts),
            osm::hooks::HOOKED_EVENTS.len(),
            reports.join("\n  ")
        );
        assert_eq!(
            total(&counts),
            osm::hooks::HOOKED_EVENTS.len(),
            "round {round}: the server should carry exactly one marked hook per \
             hooked event; it carries {counts:?}"
        );
    }
}

/// The lock two engines agree on is the one named by the **server**, not by
/// either engine's state directory.
///
/// A direct statement of the rule the race above exercises: the same socket
/// gives the same lock however the process's own directories are arranged,
/// and two different sockets never share one.
#[test]
fn the_hook_lock_is_named_by_the_socket_and_not_by_the_state_directory() {
    let a = osm::tmux::Tmux::with_socket("osm-lockname-a");
    let same = osm::tmux::Tmux::with_socket("osm-lockname-a");
    let b = osm::tmux::Tmux::with_socket("osm-lockname-b");

    let for_a = osm::hooks::lock_path(&a).unwrap();
    assert_eq!(
        for_a,
        osm::hooks::lock_path(&same).unwrap(),
        "two engines on one server must take one lock"
    );
    assert_ne!(
        for_a,
        osm::hooks::lock_path(&b).unwrap(),
        "two servers must not share a lock: an install on one would block on \
         the other"
    );
    assert!(
        !for_a.starts_with(osm::paths::state_dir().unwrap()),
        "the lock must not live in an osm state directory — that is the \
         namespace this fix moved it out of: {}",
        for_a.display()
    );
}
