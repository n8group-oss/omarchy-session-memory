//! Reinstalling on top of an engine that is already running.
//!
//! `systemctl enable --now` starts a unit that is *inactive*. It does nothing
//! to one that is already running — so an install over a live engine left the
//! old daemon process running against a new binary, new hook commands and a
//! new CLI. That is not a cosmetic mismatch: two versions opening the same
//! database across a schema change take turns preserving each other's file
//! aside and starting a fresh one, and the user's snapshots become
//! unreachable without a single error being printed.
//!
//! Two more things this pins down, both of which made `--dry-run` a promise
//! the real run did not keep:
//!
//! * the prefix is normalised **once**, so `$HOME/.local/../.local` is the
//!   default prefix — it writes to the default location either way, and used
//!   to fail the lexical comparison that decides whether systemd is touched;
//! * the dry run and the real run share one plan, so a dry run cannot promise
//!   to enable units that the real run would decline to touch.
//!
//! # Isolation
//!
//! Nothing here may reach the developer's systemd. Two independent guards:
//! `HOME` is a temporary directory (so the default prefix is inside it), and
//! `PATH` begins with a directory holding a **stand-in `systemctl`** that only
//! appends to a log. Every test that expects systemd work asserts that log is
//! non-empty, so a stand-in that failed to shadow the real thing fails the
//! test rather than silently letting the real one through.

mod common;

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A temporary world with its own `HOME`, its own state directory, and a
/// stand-in `systemctl` on `PATH`.
struct World {
    dir: tempfile::TempDir,
    socket: String,
}

impl Drop for World {
    fn drop(&mut self) {
        // No test here starts a tmux server, but `osm install` runs `tmux -V`
        // and a future edit might. Kill and unlink whatever is on this
        // socket, exactly as every other suite does.
        common::shutdown(&osm::tmux::Tmux::with_socket(&self.socket));
    }
}

impl World {
    fn new(label: &str) -> Self {
        let world = World {
            dir: tempfile::tempdir().unwrap(),
            socket: format!("osm-{}-{}", label, std::process::id()),
        };
        common::write_headless_config(&world.dir.path().join("config"));
        common::stub_systemctl(&world.dir.path().join("fakebin"));
        world
    }

    fn home(&self) -> PathBuf {
        // Canonical, because the engine normalises the prefix it is given and
        // a comparison against an un-normalised temporary path would be
        // testing the test.
        self.dir.path().canonicalize().unwrap()
    }

    fn default_prefix(&self) -> PathBuf {
        self.home().join(".local")
    }

    fn log(&self) -> PathBuf {
        self.dir.path().join("systemctl.log")
    }

    /// The runtime directory this world's `osm` is given.
    fn runtime_dir(&self) -> PathBuf {
        self.dir.path().join("xdg-runtime")
    }

    /// Put a systemd user manager in this world.
    ///
    /// `systemctl --user` talks to `$XDG_RUNTIME_DIR/systemd/private`, and
    /// whether that socket is there is how the engine tells a machine with no
    /// user manager — a container, a headless box, an ssh login with no
    /// session, all ordinary and all entitled to an install — from a manager
    /// that is right there and did not answer. A plain file is enough:
    /// nothing connects to it, the question is only whether it exists.
    fn pretend_user_manager(&self) {
        let dir = self.runtime_dir().join("systemd");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("private"), b"").unwrap();
    }

    /// The directory a completed install would have written its units into.
    fn unit_dir(&self) -> PathBuf {
        self.default_prefix().join("share/systemd/user")
    }

    /// A unit file a completed install would have written.
    fn unit(&self) -> PathBuf {
        self.unit_dir().join("osm.service")
    }

    /// Where the stand-in `systemctl` looks when it records what the binary
    /// looked like at the moment it was called. That is how a test can tell
    /// whether the daemon was stopped *before* or *after* its binary was
    /// replaced — the two orders are indistinguishable from the argument list
    /// alone.
    fn watched_binary(&self) -> PathBuf {
        self.default_prefix().join("bin/osm")
    }

    /// Run `osm`, always with `--socket` and a private `XDG_STATE_HOME`, with
    /// the stand-in `systemctl` first on `PATH`.
    fn osm(&self, args: &[&str]) -> Output {
        self.osm_with(&[], args)
    }

    /// The same, plus environment variables of the caller's choosing.
    ///
    /// Only `XDG_DATA_HOME` uses this today, and it is the reason the escape
    /// hatch exists: it is what decides where systemd's user manager looks
    /// for units, and it cannot be set in the test process — these tests run
    /// as threads in one binary, and an environment variable set by one of
    /// them would be read by all the others.
    fn osm_with(&self, extra: &[(&str, &OsStr)], args: &[&str]) -> Output {
        let path = format!(
            "{}:{}",
            self.dir.path().join("fakebin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_osm"));
        cmd.env("HOME", self.dir.path())
            .env("PATH", path)
            .env("XDG_STATE_HOME", self.dir.path().join("state"))
            .env("XDG_CONFIG_HOME", self.dir.path().join("config"))
            // A fake `HOME` with the developer's real `XDG_DATA_HOME` beside
            // it is not a machine that exists. The engine decides whether to
            // touch systemd by asking where systemd reads units from, and
            // this developer's shell exports
            // `XDG_DATA_HOME=$HOME/.local/share` against their *real* home —
            // so every install into this temporary world looked like one
            // whose units systemd would never load. Removed rather than set,
            // because unset is what a stock machine has and the fallback to
            // `$HOME/.local/share` is then exercised too.
            .env_remove("XDG_DATA_HOME")
            // The system-wide halves of the search path, pointed inside this
            // world for the same reason. Nothing under a temporary prefix
            // could ever match `/usr/share/systemd/user`, but a test that
            // reads the developer's environment at all is a test that can
            // change its answer when they change their shell.
            .env("XDG_DATA_DIRS", self.dir.path().join("xdg-data-dirs"))
            .env("XDG_RUNTIME_DIR", self.dir.path().join("xdg-runtime"))
            .env("OSM_TEST_LOG", self.log())
            .env("OSM_TEST_BINARY", self.watched_binary())
            .arg("--socket")
            .arg(&self.socket)
            .args(args);
        for (key, value) in extra {
            cmd.env(key, value);
        }
        cmd.output().expect("spawn osm")
    }

    fn systemctl_log(&self) -> Vec<String> {
        std::fs::read_to_string(self.log())
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

/// Pretend an engine is already installed and running here.
fn pretend_installed(world: &World) {
    let bin = world.watched_binary();
    std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
    std::fs::write(&bin, b"OLD").unwrap();
}

/// A dry run is only useful if it is the same decision the real run makes.
/// It used to promise "would enable osm-restore.service and osm.service"
/// whatever the prefix, while the real run with a custom prefix printed
/// "systemd not touched" — so the one output a cautious user reads before
/// committing was the one that was wrong.
#[test]
fn a_dry_run_promises_only_the_systemd_work_the_real_run_would_do() {
    let world = World::new("dryplan");
    let custom = world.dir.path().join("elsewhere");

    let out = world.osm(&["install", "--dry-run", "--prefix", custom.to_str().unwrap()]);
    let text = stdout(&out);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        !text.contains("would enable"),
        "a dry run for a prefix outside systemd's search path promised an \
         enable the real run would refuse to do:\n{text}"
    );
    assert!(
        text.contains("systemd not touched"),
        "and it must say what it will do instead:\n{text}"
    );

    // The default prefix is the case where systemd *is* touched, so the same
    // dry run there must say so — otherwise this test would pass on a build
    // that simply never mentions systemd.
    let out = world.osm(&["install", "--dry-run"]);
    let text = stdout(&out);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        text.contains("would enable"),
        "a dry run into the default prefix must promise the enable that \
         follows:\n{text}"
    );
    assert!(
        world.systemctl_log().is_empty(),
        "a dry run ran systemctl: {:?}",
        world.systemctl_log()
    );
}

/// `$HOME/.local/../.local` **is** `$HOME/.local`. It installs into the
/// default location by any measure that matters — the files land there — but
/// a lexical comparison against `$HOME/.local` says otherwise, so the install
/// wrote the units into systemd's search path and then reported that systemd
/// was not touched because they were somewhere else.
#[test]
fn a_prefix_that_normalises_to_the_default_is_treated_as_the_default() {
    let world = World::new("normalise");
    let scenic = world.home().join(".local/../.local");
    // The destination has to exist for this to be the interesting case: a
    // path with a `..` in it is only equivalent when the component it steps
    // out of is real.
    std::fs::create_dir_all(world.default_prefix()).unwrap();

    let out = world.osm(&["install", "--dry-run", "--prefix", scenic.to_str().unwrap()]);
    let text = stdout(&out);
    assert!(out.status.success(), "{}", stderr(&out));

    let want = world.default_prefix().join("bin/osm");
    assert!(
        text.contains(&want.display().to_string()),
        "the prefix was not normalised; the plan still names the scenic \
         route:\n{text}"
    );
    assert!(
        text.contains("would enable"),
        "a prefix that resolves to the default one writes into systemd's \
         search path, so the install must enable the units rather than \
         report that it left systemd alone:\n{text}"
    );
}

/// The finding itself: `enable --now` does not restart a running unit, so the
/// old daemon kept running against the new binary.
///
/// Asserted through the order of events rather than through the argument list
/// alone — an install that stopped the daemon *after* replacing its binary
/// would produce the same commands and none of the safety.
#[test]
fn installing_over_a_running_daemon_stops_it_before_replacing_the_binary() {
    let world = World::new("replace");
    pretend_installed(&world);

    let out = world.osm(&["install"]);
    let text = stdout(&out);
    assert!(out.status.success(), "{}\n{text}", stderr(&out));

    let log = world.systemctl_log();
    assert!(
        !log.is_empty(),
        "the stand-in systemctl never ran, so this test proves nothing about \
         systemd — and the real one may have been called instead"
    );

    let stop = log
        .iter()
        .position(|l| l.contains("stop") && l.contains("osm.service"))
        .unwrap_or_else(|| panic!("the install never stopped the running daemon: {log:?}\n{text}"));
    let enable = log
        .iter()
        .position(|l| l.contains("enable"))
        .unwrap_or_else(|| panic!("the install never enabled the units: {log:?}\n{text}"));
    assert!(
        stop < enable,
        "the daemon must be stopped before it is started again: {log:?}"
    );
    assert!(
        log[stop].contains("binary=OLD"),
        "the daemon was stopped only after its binary had already been \
         replaced, which is the failure this test exists for: {log:?}"
    );
    assert!(
        std::fs::read(world.watched_binary()).unwrap() != b"OLD",
        "the new binary was never installed"
    );
}

/// If the old daemon cannot be stopped, the new binary must not be written
/// over the one it is running. Reporting the failed stop as a line of text
/// and carrying on is how two versions end up sharing a database.
#[test]
fn an_install_that_cannot_stop_the_running_daemon_changes_nothing() {
    let world = World::new("stopfail");
    pretend_installed(&world);

    // Through `World::osm_with`, not a hand-built `Command`: the world is
    // what makes the temporary `HOME` coherent — including removing the
    // developer's own `XDG_DATA_HOME`, without which this install's units
    // are not on any search path and the systemd branch under test is never
    // reached.
    let out = world.osm_with(&[("OSM_TEST_STOP_RC", OsStr::new("1"))], &["install"]);

    assert!(
        !out.status.success(),
        "an install that could not stop the running daemon reported success:\n{}",
        stdout(&out)
    );
    assert_eq!(
        std::fs::read(world.watched_binary()).unwrap(),
        b"OLD",
        "the binary was replaced underneath a daemon that is still running"
    );
    let units: Vec<PathBuf> = ["osm.service", "osm-restore.service"]
        .iter()
        .map(|n| world.default_prefix().join("share/systemd/user").join(n))
        .collect();
    for unit in &units {
        assert!(
            !Path::new(unit).exists(),
            "{} was written even though the install could not proceed",
            unit.display()
        );
    }
}

// ---------------------------------------------------------------------------
// Where systemd actually looks.
//
// `<prefix>/share/systemd/user` is only a systemd search path while
// `XDG_DATA_HOME` is unset or points at `$HOME/.local/share`. The install
// decided by asking "is this prefix the default one?", which quietly assumed
// the first. With `XDG_DATA_HOME` set elsewhere it wrote units into
// `$HOME/.local/share/systemd/user`, which the user manager does not read,
// and then announced `would enable` and ran `systemctl --user enable --now`.
//
// That command does not fail for want of a unit file: it acts on whatever
// unit of that name it can find on the *real* search path — on a machine that
// has installed osm before, an older copy, now running against a database
// this build has migrated.
// ---------------------------------------------------------------------------

/// The decision has to be about the directory the units land in.
#[test]
fn an_overridden_xdg_data_home_takes_the_units_off_systemds_search_path() {
    let world = World::new("xdgdata");
    let elsewhere = world.dir.path().join("data");

    let out = world.osm_with(
        &[("XDG_DATA_HOME", elsewhere.as_os_str())],
        &["install", "--dry-run"],
    );
    let text = stdout(&out);
    assert!(out.status.success(), "{}", stderr(&out));

    let units = world.default_prefix().join("share/systemd/user");
    assert!(
        text.contains(&units.display().to_string()),
        "the units still go under the default prefix, which is the point:\n{text}"
    );
    assert!(
        !text.contains("would enable"),
        "systemd reads $XDG_DATA_HOME/systemd/user ({}), not {} — so enabling \
         these units by name would act on some other unit of that name, or on \
         nothing:\n{text}",
        elsewhere.join("systemd/user").display(),
        units.display()
    );
    assert!(
        text.contains("systemd not touched"),
        "and it must say what it will do instead:\n{text}"
    );
}

/// The real run, not only the dry run: it must not *act* on systemd.
///
/// It does ask systemd one question — where the running manager loads units
/// from — and the stand-in keeps that out of its log for exactly this reason:
/// what matters here is that nothing was enabled, reloaded or started.
#[test]
fn an_install_off_the_search_path_does_not_act_on_systemd() {
    let world = World::new("xdgreal");
    let elsewhere = world.dir.path().join("data");

    let out = world.osm_with(&[("XDG_DATA_HOME", elsewhere.as_os_str())], &["install"]);
    assert!(out.status.success(), "{}", stderr(&out));

    assert!(
        world.default_prefix().join("bin/osm").exists(),
        "the install still placed the binary: {}",
        stdout(&out)
    );
    assert!(
        world.systemctl_log().is_empty(),
        "an install whose units are not on systemd's search path ran \
         systemctl anyway: {:?}",
        world.systemctl_log()
    );
}

/// The other direction, so the fix cannot be "never manage systemd".
///
/// `XDG_DATA_HOME` pointing at `$HOME/.local/share` is where systemd looks by
/// default, and an install into the default prefix lands exactly there.
#[test]
fn an_xdg_data_home_that_matches_the_default_still_manages_systemd() {
    let world = World::new("xdgsame");
    let data = world.home().join(".local/share");
    std::fs::create_dir_all(&data).unwrap();

    let out = world.osm_with(
        &[("XDG_DATA_HOME", data.as_os_str())],
        &["install", "--dry-run"],
    );
    let text = stdout(&out);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        text.contains("would enable"),
        "the units land on systemd's search path here, so the install must \
         enable them:\n{text}"
    );
}

/// A prefix outside `$HOME` whose unit directory *is* a systemd search path.
///
/// The old rule could only ever say `Manage` for one prefix. Point
/// `XDG_DATA_HOME` at `<prefix>/share` and `<prefix>/share/systemd/user` is a
/// directory systemd reads, whatever the prefix is called — and an install
/// that writes there and then declines to enable leaves the user with units
/// systemd can see and a message saying it left systemd alone.
#[test]
fn a_custom_prefix_on_the_search_path_is_managed() {
    let world = World::new("xdgcustom");
    let prefix = world.dir.path().join("opt/osm");
    let data = prefix.join("share");

    let out = world.osm_with(
        &[("XDG_DATA_HOME", data.as_os_str())],
        &["install", "--dry-run", "--prefix", prefix.to_str().unwrap()],
    );
    let text = stdout(&out);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        text.contains("would enable"),
        "{} is $XDG_DATA_HOME/systemd/user, so these units are the ones \
         systemd would load:\n{text}",
        data.join("systemd/user").display()
    );
}

/// The prefix a user types must be the prefix that is acted on.
///
/// `/etc/passwd/../../tmp/osm-prefix-proof` is refused by the kernel with
/// `ENOTDIR` — `passwd` is a regular file and nothing lives under it. The
/// engine folded it to `/tmp/osm-prefix-proof` and offered to install there:
/// a real directory, full of other people's files, that the user never named
/// and that a later `osm uninstall` given the same argument would remove
/// from.
///
/// A dry run, so nothing is written anywhere by this test whatever the
/// outcome.
#[test]
fn a_prefix_the_kernel_would_refuse_is_not_folded_into_a_different_directory() {
    let world = World::new("enotdir");
    let scenic = "/etc/passwd/../../tmp/osm-prefix-proof";

    let out = world.osm(&["install", "--dry-run", "--prefix", scenic]);
    let text = format!("{}{}", stdout(&out), stderr(&out));

    assert!(
        !out.status.success(),
        "a prefix the kernel refuses was accepted:\n{text}"
    );
    // Naming the path the user typed is right — that is what the refusal is
    // about. Proposing to write into the directory it folds to is not.
    assert!(
        !text.contains("/tmp/osm-prefix-proof/bin"),
        "the engine proposed installing into a directory the user never \
         named:\n{text}"
    );
    assert!(
        !text.contains("would install") && !text.contains("would write"),
        "the engine offered a plan for a prefix it should have refused:\n{text}"
    );
    assert!(
        !std::path::Path::new("/tmp/osm-prefix-proof").exists(),
        "a dry run created something"
    );
}

// ---------------------------------------------------------------------------
// Where systemd actually looks is a question for systemd.
//
// The install decides whether to enable the units by asking whether they
// landed on the user manager's unit load path. That path was computed from
// *this* process's environment — and the manager that would load them is a
// different process, started at login, with an environment of its own.
// `XDG_DATA_HOME=/tmp/data osm install` changes only the child's: the units
// go to `~/.local/share/systemd/user`, which the manager does read, while the
// install concludes otherwise, skips `enable`, says so, and exits 0. Nothing
// starts at the next boot and nothing looked wrong.
// ---------------------------------------------------------------------------

/// The manager reads the directory the units landed in, so they are enabled —
/// whatever this process's `XDG_DATA_HOME` says.
#[test]
fn the_running_managers_unit_path_decides_that_the_units_are_enabled() {
    let world = World::new("livepath");
    // Points the environment calculation away from the units...
    let elsewhere = world.dir.path().join("data");
    // ...while the manager that is actually running reads exactly where they
    // land.
    let unit_dir = world.default_prefix().join("share/systemd/user");

    let out = world.osm_with(
        &[
            ("XDG_DATA_HOME", elsewhere.as_os_str()),
            ("OSM_TEST_UNIT_PATH", unit_dir.as_os_str()),
        ],
        &["install"],
    );
    let text = stdout(&out);
    assert!(out.status.success(), "{}\n{text}", stderr(&out));

    let log = world.systemctl_log();
    assert!(
        log.iter().any(|l| l.contains("enable")),
        "the units were written into a directory the running manager reads and \
         the install did not enable them; it reported success anyway:\n{text}\n{log:?}"
    );
}

/// And the other direction, so the fix cannot be "always enable": a directory
/// the running manager does **not** read is left alone, even when this
/// process's environment says it would be read.
#[test]
fn the_running_managers_unit_path_decides_that_the_units_are_left_alone() {
    let world = World::new("livepathno");

    let out = world.osm_with(
        &[("OSM_TEST_UNIT_PATH", OsStr::new("/nowhere/systemd/user"))],
        &["install"],
    );
    let text = stdout(&out);
    assert!(out.status.success(), "{}\n{text}", stderr(&out));

    let log = world.systemctl_log();
    assert!(
        !log.iter().any(|l| l.contains("enable")),
        "the units are not on the running manager's unit path and the install \
         enabled them anyway — which acts on whatever unit of that name systemd \
         can find elsewhere:\n{text}\n{log:?}"
    );
    assert!(
        text.contains("systemd not touched"),
        "and it must say what it did instead:\n{text}"
    );
}

/// `$SYSTEMD_UNIT_PATH` replaces systemd's unit load path outright
/// (`systemd.unit(5)`). The environment calculation ignored it, so on a
/// machine that sets it the dry run described a machine that does not exist.
#[test]
fn the_environment_calculation_honours_systemd_unit_path() {
    let world = World::new("unitpathenv");
    let custom = world.dir.path().join("elsewhere");
    let unit_dir = custom.join("share/systemd/user");

    // A prefix of the user's own choosing, which the default load path never
    // contains — but this override says it is the only place systemd looks.
    let out = world.osm_with(
        &[("SYSTEMD_UNIT_PATH", unit_dir.as_os_str())],
        &["install", "--dry-run", "--prefix", custom.to_str().unwrap()],
    );
    let text = stdout(&out);
    assert!(out.status.success(), "{}\n{text}", stderr(&out));
    assert!(
        text.contains("would enable"),
        "SYSTEMD_UNIT_PATH names the directory these units land in, so systemd \
         does load them:\n{text}"
    );

    // The same override pointing somewhere else *replaces* the default path,
    // so the default prefix is no longer on it.
    let out = world.osm_with(
        &[("SYSTEMD_UNIT_PATH", OsStr::new("/nowhere/systemd/user"))],
        &["install", "--dry-run"],
    );
    let text = stdout(&out);
    assert!(out.status.success(), "{}\n{text}", stderr(&out));
    assert!(
        text.contains("systemd not touched"),
        "SYSTEMD_UNIT_PATH replaces the load path; the default prefix is not on \
         this one:\n{text}"
    );
}

/// A trailing empty component means "and then the usual path", which is the
/// half of `SYSTEMD_UNIT_PATH` that adds rather than replaces.
#[test]
fn a_trailing_colon_in_systemd_unit_path_appends_the_usual_load_path() {
    let world = World::new("unitpathappend");

    let out = world.osm_with(
        &[("SYSTEMD_UNIT_PATH", OsStr::new("/nowhere/systemd/user:"))],
        &["install", "--dry-run"],
    );
    let text = stdout(&out);
    assert!(out.status.success(), "{}\n{text}", stderr(&out));
    assert!(
        text.contains("would enable"),
        "the trailing colon appends the usual load path, which the default \
         prefix is on:\n{text}"
    );
}

// ---------------------------------------------------------------------------
// The systemd probe: bounded, and never silently wrong.
// ---------------------------------------------------------------------------
//
// The install asks the *running* manager where it loads units from, because
// computing that from this process's environment answers about the wrong
// process. That question was asked with an unbounded `Command::output()`, and
// every way of not getting an answer — a spawn that failed, a non-zero exit,
// an empty reply — was folded into "use the environment calculation instead".
//
// Both halves are bugs, and the second is the dangerous one. Combine a
// transient probe failure with an `XDG_DATA_HOME` the manager does not share
// and the plan says `NotOnSearchPath`: the pre-replacement daemon stop is
// skipped, the binary is overwritten underneath a running engine, the units
// are never enabled, and the command exits 0. The user is told the install
// succeeded and nothing starts at the next boot.

/// A probe that never comes back must not hang the install.
#[test]
fn an_install_whose_systemd_probe_never_answers_changes_nothing() {
    let world = World::new("probehang");
    pretend_installed(&world);

    let started = std::time::Instant::now();
    let out = world.osm_with(&[("OSM_TEST_SHOW_SLEEP", OsStr::new("120"))], &["install"]);
    let elapsed = started.elapsed();

    assert!(
        !out.status.success(),
        "an install that could not find out where systemd looks reported \
         success:\n{}\n{}",
        stdout(&out),
        stderr(&out)
    );
    assert!(
        elapsed < std::time::Duration::from_secs(60),
        "the probe was not bounded: the install took {elapsed:?}"
    );
    assert_eq!(
        std::fs::read(world.watched_binary()).unwrap(),
        b"OLD".to_vec(),
        "the binary was replaced underneath a daemon nobody could tell was \
         running"
    );
    assert!(
        !world.unit().exists(),
        "units were written by an install that could not decide whether \
         systemd would ever load them"
    );
    assert!(
        world.systemctl_log().is_empty(),
        "systemd was acted on by an install that gave up asking it a \
         question: {:?}",
        world.systemctl_log()
    );
    assert!(
        stderr(&out).contains("systemd"),
        "the refusal must name what could not be determined:\n{}",
        stderr(&out)
    );
}

/// A probe that fails while the manager is plainly there is uncertainty, not
/// an answer.
#[test]
fn an_install_whose_systemd_probe_fails_with_a_manager_present_changes_nothing() {
    let world = World::new("probefail");
    world.pretend_user_manager();
    pretend_installed(&world);

    let out = world.osm_with(&[("OSM_TEST_SHOW_RC", OsStr::new("1"))], &["install"]);

    assert!(
        !out.status.success(),
        "a failed probe was turned into an answer and the install reported \
         success:\n{}\n{}",
        stdout(&out),
        stderr(&out)
    );
    assert_eq!(
        std::fs::read(world.watched_binary()).unwrap(),
        b"OLD".to_vec(),
        "the binary was replaced on the strength of a guess"
    );
    assert!(
        !world.unit().exists(),
        "units were written on the strength of a guess"
    );
    assert!(
        world.systemctl_log().is_empty(),
        "systemd was acted on after refusing to answer: {:?}",
        world.systemctl_log()
    );
}

/// The other half of the same distinction, and the reason it has to be a
/// distinction at all: a machine with no user manager is a real machine, and
/// it is still entitled to an install.
#[test]
fn a_machine_with_no_user_manager_still_installs() {
    let world = World::new("nomanager");
    // No `pretend_user_manager`: nothing is listening on this world's
    // runtime directory, which is what a container or a headless box looks
    // like. `systemctl --user` exits non-zero there, exactly as the stub does.
    let out = world.osm_with(&[("OSM_TEST_SHOW_RC", OsStr::new("1"))], &["install"]);

    assert!(
        out.status.success(),
        "a machine with no systemd user manager was refused an install:\n{}\n{}",
        stdout(&out),
        stderr(&out)
    );
    assert!(
        world.unit().exists(),
        "nothing was installed:\n{}",
        stdout(&out)
    );
}

// ---------------------------------------------------------------------------
// The other systemd question the install turns on: is the daemon running?
// ---------------------------------------------------------------------------
//
// `unit_is_active` decides whether the running daemon is stopped before its
// binary is replaced. It asked with an unbounded `Command::status()` and
// folded every way of not getting an answer into `false` — so an unresponsive
// user manager held the install open indefinitely, and a probe that could not
// be answered became the *fact* "the daemon is not running": the stop is
// skipped, the binary is overwritten underneath a live daemon, and two
// versions of the engine share one database. That is the hazard the
// pre-replacement stop exists to prevent, reached by a different route.
//
// Unknown is never rendered as no.

/// A probe that never comes back must not hang the install — and must not be
/// read as "the daemon is not running".
#[test]
fn an_install_whose_is_active_probe_never_answers_changes_nothing() {
    let world = World::new("activehang");
    world.pretend_user_manager();
    pretend_installed(&world);

    let started = std::time::Instant::now();
    let out = world.osm_with(
        &[
            // The unit path question answers, so this test is about the next
            // one and not about the probe already bounded.
            ("OSM_TEST_UNIT_PATH", world.unit_dir().as_os_str()),
            ("OSM_TEST_IS_ACTIVE_SLEEP", OsStr::new("120")),
        ],
        &["install"],
    );
    let elapsed = started.elapsed();

    assert!(
        !out.status.success(),
        "an install that could not find out whether the daemon was running \
         reported success:\n{}\n{}",
        stdout(&out),
        stderr(&out)
    );
    assert!(
        elapsed < std::time::Duration::from_secs(60),
        "the probe was not bounded: the install took {elapsed:?}"
    );
    assert_eq!(
        std::fs::read(world.watched_binary()).unwrap(),
        b"OLD".to_vec(),
        "the binary was replaced underneath a daemon nobody could tell was \
         running"
    );
    assert!(
        !world.unit().exists(),
        "units were written by an install that stopped before it could act"
    );

    let log = world.systemctl_log();
    assert!(
        log.iter().any(|l| l.contains("is-active")),
        "the stand-in systemctl never saw the probe, so this test proves \
         nothing — and the real one may have been asked instead: {log:?}"
    );
    for verb in ["stop", "enable", "daemon-reload"] {
        assert!(
            !log.iter().any(|l| l.contains(verb)),
            "systemd was told to {verb} by an install that gave up asking it \
             whether the daemon was running: {log:?}"
        );
    }
    assert!(
        stderr(&out).contains("osm.service") && stderr(&out).contains("nothing was installed"),
        "the refusal must name what could not be determined and what did not \
         happen:\n{}",
        stderr(&out)
    );
}

/// A probe that fails while the manager is plainly there is uncertainty, not
/// an answer — and the answer it used to be coerced into is the dangerous
/// one.
#[test]
fn an_install_that_cannot_tell_whether_the_daemon_is_running_changes_nothing() {
    let world = World::new("activefail");
    world.pretend_user_manager();
    pretend_installed(&world);

    let out = world.osm_with(
        &[
            ("OSM_TEST_UNIT_PATH", world.unit_dir().as_os_str()),
            // A non-zero exit that says nothing: what `systemctl` does on the
            // way out of a bus it could not talk to. A unit that is merely
            // not running says `inactive` and exits 3.
            ("OSM_TEST_IS_ACTIVE", OsStr::new("1")),
            ("OSM_TEST_IS_ACTIVE_STATE", OsStr::new("")),
        ],
        &["install"],
    );

    assert!(
        !out.status.success(),
        "an unanswerable probe was turned into \"the daemon is not running\" \
         and the install reported success:\n{}\n{}",
        stdout(&out),
        stderr(&out)
    );
    assert_eq!(
        std::fs::read(world.watched_binary()).unwrap(),
        b"OLD".to_vec(),
        "the binary was replaced underneath a daemon the probe could not be \
         asked about"
    );
    assert!(
        !world.unit().exists(),
        "units were written on the strength of a guess"
    );
    let log = world.systemctl_log();
    for verb in ["stop", "enable", "daemon-reload"] {
        assert!(
            !log.iter().any(|l| l.contains(verb)),
            "systemd was told to {verb} after refusing to answer: {log:?}"
        );
    }
}

/// The other direction, so the fix cannot be "always refuse": a manager that
/// says the unit is not running has answered, and a first install must not be
/// blocked by it.
#[test]
fn an_install_told_the_daemon_is_not_running_does_not_try_to_stop_it() {
    let world = World::new("activeno");
    world.pretend_user_manager();
    pretend_installed(&world);

    // Exit 3 with `inactive` on stdout is what systemd reports for a unit
    // that is simply not running.
    let out = world.osm_with(
        &[
            ("OSM_TEST_UNIT_PATH", world.unit_dir().as_os_str()),
            ("OSM_TEST_IS_ACTIVE", OsStr::new("3")),
        ],
        &["install"],
    );
    let text = stdout(&out);
    assert!(out.status.success(), "{}\n{text}", stderr(&out));

    let log = world.systemctl_log();
    assert!(
        !log.iter().any(|l| l.contains("stop")),
        "the manager said the unit was not running and the install tried to \
         stop it anyway: {log:?}"
    );
    assert!(
        log.iter().any(|l| l.contains("enable")),
        "the install never enabled the units: {log:?}\n{text}"
    );
    assert!(
        std::fs::read(world.watched_binary()).unwrap() != b"OLD",
        "the new binary was never installed:\n{text}"
    );
}

/// And on a machine with no user manager at all there is nothing for a
/// systemd unit to be running under, so a probe that says nothing there is
/// not uncertainty. A container and a headless box are real places to install
/// this and are still entitled to it.
#[test]
fn a_machine_with_no_user_manager_needs_no_answer_about_the_daemon() {
    let world = World::new("activenomgr");
    // No `pretend_user_manager`, and no unit path answer either: nothing is
    // listening, which is what `systemctl --user` finds in a container.
    let out = world.osm_with(
        &[
            ("OSM_TEST_IS_ACTIVE", OsStr::new("1")),
            ("OSM_TEST_IS_ACTIVE_STATE", OsStr::new("")),
        ],
        &["install"],
    );

    assert!(
        out.status.success(),
        "a machine with no systemd user manager was refused an install:\n{}\n{}",
        stdout(&out),
        stderr(&out)
    );
    assert!(
        world.unit().exists(),
        "nothing was installed:\n{}",
        stdout(&out)
    );
}

// ---------------------------------------------------------------------------
// The systemd actions: bounded too, and never claiming more than they know.
// ---------------------------------------------------------------------------
//
// A probe that answered says the manager was responsive a moment ago. It does
// not bound the command that follows it. A manager can accept `enable --now`
// and then wait forever on `osm-restore.service` — a `Type=oneshot` unit whose
// `TimeoutStartSec` is `infinity` by systemd's own default — or wedge during a
// `daemon-reload` or a stop job. `Command::output()` on any of those hung the
// install indefinitely, and it hung it *after* the files were written: no
// output, and no way out but Ctrl-C.
//
// Not being able to cancel a queued job is a reason not to pretend the job
// finished. It is not a reason to block the CLI on it.

/// A `daemon-reload` that never comes back must not hang the install.
#[test]
fn an_install_whose_daemon_reload_never_returns_does_not_hang() {
    let world = World::new("reloadhang");
    world.pretend_user_manager();

    let started = std::time::Instant::now();
    let out = world.osm_with(
        &[
            ("OSM_TEST_UNIT_PATH", world.unit_dir().as_os_str()),
            ("OSM_TEST_IS_ACTIVE", OsStr::new("3")),
            ("OSM_TEST_RELOAD_SLEEP", OsStr::new("45")),
        ],
        &["install"],
    );
    let elapsed = started.elapsed();

    assert!(
        elapsed < std::time::Duration::from_secs(40),
        "the daemon-reload was not bounded: the install took {elapsed:?}"
    );
    assert!(
        !out.status.success(),
        "an install whose daemon-reload never came back reported success:\n{}\n{}",
        stdout(&out),
        stderr(&out)
    );
    let text = format!("{}{}", stdout(&out), stderr(&out));
    assert!(
        text.contains("daemon-reload") && text.contains("did not come back"),
        "the install must say which command it gave up on:\n{text}"
    );
}

/// An `enable --now` that never comes back must not hang the install either
/// — and this is the one the units make unbounded by design.
#[test]
fn an_install_whose_enable_never_returns_does_not_hang() {
    let world = World::new("enablehang");
    world.pretend_user_manager();

    let started = std::time::Instant::now();
    let out = world.osm_with(
        &[
            ("OSM_TEST_UNIT_PATH", world.unit_dir().as_os_str()),
            ("OSM_TEST_IS_ACTIVE", OsStr::new("3")),
            ("OSM_TEST_ENABLE_SLEEP", OsStr::new("45")),
        ],
        &["install"],
    );
    let elapsed = started.elapsed();

    assert!(
        elapsed < std::time::Duration::from_secs(40),
        "the enable was not bounded: the install took {elapsed:?}"
    );
    assert!(
        !out.status.success(),
        "an install whose enable never came back reported success:\n{}\n{}",
        stdout(&out),
        stderr(&out)
    );
}

/// The pre-replacement stop is issued without blocking and is bounded, so a
/// stop job the manager will not run cannot hold the install open — and
/// nothing is replaced, because the daemon is still there.
#[test]
fn an_install_whose_stop_never_returns_changes_nothing() {
    let world = World::new("stophang");
    world.pretend_user_manager();
    pretend_installed(&world);

    let started = std::time::Instant::now();
    let out = world.osm_with(
        &[
            ("OSM_TEST_UNIT_PATH", world.unit_dir().as_os_str()),
            ("OSM_TEST_STOP_SLEEP", OsStr::new("45")),
        ],
        &["install"],
    );
    let elapsed = started.elapsed();

    assert!(
        elapsed < std::time::Duration::from_secs(40),
        "the stop was not bounded: the install took {elapsed:?}"
    );
    assert!(
        !out.status.success(),
        "an install that never stopped the running daemon reported success:\n{}\n{}",
        stdout(&out),
        stderr(&out)
    );
    assert_eq!(
        std::fs::read(world.watched_binary()).unwrap(),
        b"OLD".to_vec(),
        "the binary was replaced underneath a daemon that was never stopped"
    );
    assert!(
        !world.unit().exists(),
        "units were written by an install that could not stop the daemon"
    );
}

/// A stop `systemctl` accepted is not a daemon that stopped. `--no-block`
/// returns when the job is queued; the install must confirm the unit is
/// actually inactive before it replaces the binary that unit is running.
#[test]
fn an_install_whose_daemon_never_goes_inactive_changes_nothing() {
    let world = World::new("stopstuck");
    world.pretend_user_manager();
    pretend_installed(&world);

    let out = world.osm_with(
        &[
            ("OSM_TEST_UNIT_PATH", world.unit_dir().as_os_str()),
            // The stop is accepted, and the unit goes on running anyway.
            ("OSM_TEST_STAYS_ACTIVE", OsStr::new("1")),
        ],
        &["install"],
    );

    assert!(
        !out.status.success(),
        "an install whose daemon never stopped reported success:\n{}\n{}",
        stdout(&out),
        stderr(&out)
    );
    assert_eq!(
        std::fs::read(world.watched_binary()).unwrap(),
        b"OLD".to_vec(),
        "the binary was replaced underneath a daemon that is still active: an \
         accepted stop command was read as a stopped unit"
    );
    assert!(
        !world.unit().exists(),
        "units were written by an install that could not stop the daemon"
    );
    let log = world.systemctl_log();
    assert!(
        log.iter().any(|l| l.contains("stop")),
        "the stand-in never saw a stop, so this test proves nothing: {log:?}"
    );
    assert!(
        stderr(&out).contains("osm.service") && stderr(&out).contains("nothing was installed"),
        "the refusal must name the unit and what did not happen:\n{}",
        stderr(&out)
    );
}

/// The start is queued, not waited for — so the install must say *queued*.
///
/// `enable --now osm-restore.service` starts a `Type=oneshot` unit whose
/// `ExecStart` is a whole restore, and there is no budget for waiting on that
/// which would not fail a legitimate run. Reporting the queued start as a
/// completed one is the same untruth as rendering "unknown" as "no".
#[test]
fn an_install_reports_the_start_as_queued_rather_than_started() {
    let world = World::new("queued");
    world.pretend_user_manager();

    let out = world.osm_with(
        &[
            ("OSM_TEST_UNIT_PATH", world.unit_dir().as_os_str()),
            ("OSM_TEST_IS_ACTIVE", OsStr::new("3")),
        ],
        &["install"],
    );
    let text = stdout(&out);
    assert!(out.status.success(), "{}\n{text}", stderr(&out));

    let log = world.systemctl_log();
    assert!(
        log.iter()
            .any(|l| l.contains("enable") && l.contains("--no-block")),
        "the start must be queued rather than waited on: {log:?}"
    );
    assert!(
        text.contains("queued"),
        "the install waited for nothing and must not imply the engine is \
         running; it has to say the start was queued:\n{text}"
    );
}

/// A `daemon-reload` the manager refused stops the install there, before the
/// enable.
///
/// The loop used to run every command whatever the one before it did, so a
/// failed reload was followed by `enable --now`: systemd then starts the units
/// from whatever definitions it still has cached — which is precisely the
/// stale-unit hazard the reload exists to close, reached by ignoring the
/// reload's answer.
#[test]
fn an_install_whose_daemon_reload_fails_never_enables_the_units() {
    let world = World::new("reloadrefused");
    world.pretend_user_manager();

    let out = world.osm_with(
        &[
            ("OSM_TEST_UNIT_PATH", world.unit_dir().as_os_str()),
            // Nothing is running, so the pre-replacement stop is skipped and
            // the only systemd work left is the reload and the enable.
            ("OSM_TEST_IS_ACTIVE", OsStr::new("3")),
            ("OSM_TEST_RELOAD_RC", OsStr::new("1")),
        ],
        &["install"],
    );

    assert!(
        !out.status.success(),
        "an install whose daemon-reload was refused reported success:\n{}\n{}",
        stdout(&out),
        stderr(&out)
    );
    let log = world.systemctl_log();
    assert!(
        log.iter().any(|l| l.contains("daemon-reload")),
        "the stand-in never saw the reload, so this test proves nothing: {log:?}"
    );
    assert!(
        !log.iter().any(|l| l.contains("enable")),
        "the install enabled — and started — the units on definitions systemd \
         never reloaded: {log:?}"
    );
    assert!(
        stderr(&out).contains("daemon-reload"),
        "the refusal must name the command it stopped at:\n{}",
        stderr(&out)
    );
}

/// An action killed at its budget has not said the engine is stopped.
///
/// `enable --now` that was still running when its budget ran out may well
/// have been taken by the manager — killing `systemctl` does not cancel a job
/// it queued — so the units may be enabled and the start job may be running.
/// The install used to end "the engine is not running" here regardless,
/// having asked nothing.
#[test]
fn an_install_whose_enable_times_out_does_not_claim_the_engine_is_stopped() {
    let world = World::new("enableunknown");
    world.pretend_user_manager();

    let out = world.osm_with(
        &[
            ("OSM_TEST_UNIT_PATH", world.unit_dir().as_os_str()),
            // The unit says *inactive*, so a build that answered this from a
            // probe rather than from the action it could not complete would
            // print exactly the sentence this must not print.
            ("OSM_TEST_IS_ACTIVE", OsStr::new("3")),
            ("OSM_TEST_ENABLE_SLEEP", OsStr::new("45")),
        ],
        &["install"],
    );

    assert!(
        !out.status.success(),
        "an install whose enable never came back reported success:\n{}\n{}",
        stdout(&out),
        stderr(&out)
    );
    let text = format!("{}{}", stdout(&out), stderr(&out));
    assert!(
        !text.contains("the engine is not running"),
        "an enable the manager may have taken was reported as a daemon that \
         is certainly not running:\n{text}"
    );
    assert!(
        text.contains("unknown"),
        "a command nothing came back from leaves the state of the engine \
         unknown, and the install has to say so:\n{text}"
    );
}
