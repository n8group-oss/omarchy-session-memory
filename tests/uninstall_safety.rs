//! An uninstall that fails must say so, and must not have deleted anything
//! first.
//!
//! Two ways this went wrong, both of which ended with exit status 0:
//!
//! * a failed `systemctl --user disable --now` was turned into a line of
//!   text, after which the uninstall removed both units and the binary — so
//!   a daemon that was still running lost its `ExecStop` (the shutdown
//!   capture), kept running against a deleted binary, and the user was told
//!   the engine was gone;
//! * `--remove-database` unlinked a **WAL** database with no lock held at
//!   all. A capture running at that moment carries on writing into the
//!   deleted inode — its work vanishes when the last descriptor closes — or
//!   recreates `state.db` a moment later, leaving the user with a database
//!   they asked to have deleted and none of the snapshots that were in it.
//!
//! # Isolation
//!
//! `HOME` is a temporary directory, so the default prefix is inside it. The
//! `systemctl` these tests reach is a stand-in first on `PATH` which only
//! writes to a log; every test that expects it to run asserts that log is
//! non-empty, so a stand-in that failed to shadow the real one fails the test
//! instead of quietly letting the real one act on the developer's units. The
//! database deleted is this test's own, under a private `XDG_STATE_HOME`.

mod common;

use std::path::PathBuf;
use std::process::{Command, Output};

struct World {
    dir: tempfile::TempDir,
    socket: String,
}

impl Drop for World {
    fn drop(&mut self) {
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
        self.dir.path().canonicalize().unwrap()
    }

    fn default_prefix(&self) -> PathBuf {
        self.home().join(".local")
    }

    fn state(&self) -> PathBuf {
        self.dir.path().join("state/osm")
    }

    fn db(&self) -> PathBuf {
        self.state().join("state.db")
    }

    fn log(&self) -> PathBuf {
        self.dir.path().join("systemctl.log")
    }

    fn osm(&self, args: &[&str], extra: &[(&str, &str)]) -> Output {
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
            // A fake `HOME` needs a fake `XDG_DATA_HOME` beside it, or the
            // engine asks where systemd reads units from and gets an answer
            // about the developer's real home. It decides whether to touch
            // systemd from that, so with this developer's shell — which
            // exports `XDG_DATA_HOME` — the uninstall here would skip the
            // systemd half entirely and this suite would test nothing.
            // Removed rather than set: unset is what a stock machine has, and
            // the fallback to `$HOME/.local/share` is then exercised too. The
            // same treatment is in `tests/install_replace.rs`.
            .env_remove("XDG_DATA_HOME")
            .env("XDG_DATA_DIRS", self.dir.path().join("xdg-data-dirs"))
            .env("XDG_RUNTIME_DIR", self.dir.path().join("xdg-runtime"))
            .env("OSM_TEST_LOG", self.log())
            .env("OSM_TEST_BINARY", self.default_prefix().join("bin/osm"))
            .arg("--socket")
            .arg(&self.socket);
        for (k, v) in extra {
            cmd.env(k, v);
        }
        cmd.args(args).output().expect("spawn osm")
    }

    fn systemctl_log(&self) -> Vec<String> {
        std::fs::read_to_string(self.log())
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// Everything a real install leaves under the default prefix.
    fn pretend_installed(&self) -> Vec<PathBuf> {
        let bin = self.default_prefix().join("bin/osm");
        let units: Vec<PathBuf> = ["osm.service", "osm-restore.service"]
            .iter()
            .map(|n| self.default_prefix().join("share/systemd/user").join(n))
            .collect();
        for path in std::iter::once(&bin).chain(units.iter()) {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"OLD").unwrap();
        }
        std::iter::once(bin).chain(units).collect()
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// The units come first, and they are not optional. Removing the files while
/// the daemon may still be running is how the shutdown capture is lost: the
/// `ExecStop` that runs `osm snapshot --reason shutdown` cannot run a binary
/// that is no longer there.
#[test]
fn an_uninstall_that_cannot_stop_the_units_deletes_nothing_and_says_so() {
    let world = World::new("stopfail");
    let placed = world.pretend_installed();

    let out = world.osm(&["uninstall"], &[("OSM_TEST_DISABLE_RC", "1")]);

    assert!(
        !world.systemctl_log().is_empty(),
        "the stand-in systemctl never ran; this test proves nothing"
    );
    assert!(
        !out.status.success(),
        "an uninstall that could not stop the units reported success:\n{}",
        stdout(&out)
    );
    for path in &placed {
        assert!(
            path.exists(),
            "{} was deleted even though the units could not be stopped:\n{}",
            path.display(),
            stdout(&out)
        );
    }
}

/// `--remove-database` has to hold the same locks a capture and a restore
/// hold, or it is racing them for the user's snapshots.
///
/// A capture takes the restore lock **shared** for the whole of its work, so
/// holding it that way here is exactly what a capture in progress looks like.
/// The uninstall must refuse rather than unlink the file out from under it.
#[test]
fn removing_the_database_refuses_while_a_capture_holds_the_lock() {
    let world = World::new("dblock");
    // A real database, made the way the engine makes it.
    drop(osm::db::open(&world.db()).unwrap());
    let lock = world.state().join("restore.lock");
    let held = osm::lock::SingleInstance::acquire_shared(&lock)
        .unwrap()
        .expect("nothing else can be holding this test's own lock");

    let prefix = world.dir.path().join("elsewhere");
    let out = world.osm(
        &[
            "uninstall",
            "--remove-database",
            "--prefix",
            prefix.to_str().unwrap(),
        ],
        &[],
    );

    assert!(
        world.db().exists(),
        "the database was unlinked while a capture still held the lock:\n{}",
        stdout(&out)
    );
    assert!(
        !out.status.success(),
        "an uninstall that could not remove the database reported success:\n{}",
        stdout(&out)
    );
    drop(held);
}

/// A WAL database is three files. Removing only `state.db` leaves
/// `state.db-wal` and `state.db-shm` behind, and the next `osm` to open a
/// fresh database there finds sidecars belonging to a database that no longer
/// exists.
#[test]
fn removing_the_database_takes_its_wal_sidecars_with_it() {
    let world = World::new("sidecars");
    // Held open for the whole test, so the sidecars are on disk when the
    // uninstall runs — which is the state a live engine is always in. A
    // connection closed cleanly checkpoints and removes them itself, and
    // would make this test pass without the fix.
    let conn = osm::db::open(&world.db()).unwrap();
    conn.execute_batch("CREATE TABLE sidecar_probe (x); INSERT INTO sidecar_probe VALUES (1);")
        .unwrap();

    let wal = world.state().join("state.db-wal");
    let shm = world.state().join("state.db-shm");
    assert!(
        wal.exists(),
        "premise: a WAL database has a -wal sidecar while it is open"
    );

    let prefix = world.dir.path().join("elsewhere");
    let out = world.osm(
        &[
            "uninstall",
            "--remove-database",
            "--prefix",
            prefix.to_str().unwrap(),
        ],
        &[],
    );
    assert!(
        out.status.success(),
        "uninstall failed: {}\n{}",
        String::from_utf8_lossy(&out.stderr),
        stdout(&out)
    );

    for path in [world.db(), wal, shm] {
        assert!(
            !path.exists(),
            "{} survived --remove-database:\n{}",
            path.display(),
            stdout(&out)
        );
    }
    drop(conn);
}

/// The uninstall asks systemd the same question the install does, and mutates
/// just as much on the answer: it stops the units, removes the hooks, and
/// deletes the binary. A probe that never came back used to hang it, and a
/// probe that failed used to be silently converted into "systemd is not
/// involved" — which skips the stop and then deletes the binary the running
/// daemon's `ExecStop` needs to take the shutdown capture.
#[test]
fn an_uninstall_whose_systemd_probe_never_answers_removes_nothing() {
    let world = World::new("probehang");
    let placed = world.pretend_installed();

    let started = std::time::Instant::now();
    let out = world.osm(&["uninstall"], &[("OSM_TEST_SHOW_SLEEP", "120")]);
    let elapsed = started.elapsed();

    assert!(
        !out.status.success(),
        "an uninstall that could not find out where systemd looks reported \
         success:\n{}",
        stdout(&out)
    );
    assert!(
        elapsed < std::time::Duration::from_secs(60),
        "the probe was not bounded: the uninstall took {elapsed:?}"
    );
    for path in &placed {
        assert!(
            path.exists(),
            "{} was deleted by an uninstall that could not decide whether \
             systemd was managing it:\n{}",
            path.display(),
            stdout(&out)
        );
    }
    assert!(
        world.systemctl_log().is_empty(),
        "systemd was acted on by an uninstall that gave up asking it a \
         question: {:?}",
        world.systemctl_log()
    );
}

/// A `daemon-reload` that never comes back must not hang the uninstall —
/// which would hold it open *before* anything is removed, with no output and
/// no way out but Ctrl-C.
#[test]
fn an_uninstall_whose_daemon_reload_never_returns_removes_nothing() {
    let world = World::new("reloadhang");
    let placed = world.pretend_installed();

    let started = std::time::Instant::now();
    let out = world.osm(&["uninstall"], &[("OSM_TEST_RELOAD_SLEEP", "45")]);
    let elapsed = started.elapsed();

    assert!(
        elapsed < std::time::Duration::from_secs(40),
        "the daemon-reload was not bounded: the uninstall took {elapsed:?}"
    );
    assert!(
        !out.status.success(),
        "an uninstall whose daemon-reload never came back reported success:\n{}",
        stdout(&out)
    );
    for path in &placed {
        assert!(
            path.exists(),
            "{} was deleted by an uninstall that never reloaded systemd:\n{}",
            path.display(),
            stdout(&out)
        );
    }
}

/// The same for the stop itself.
#[test]
fn an_uninstall_whose_disable_never_returns_removes_nothing() {
    let world = World::new("disablehang");
    let placed = world.pretend_installed();

    let started = std::time::Instant::now();
    let out = world.osm(&["uninstall"], &[("OSM_TEST_DISABLE_SLEEP", "45")]);
    let elapsed = started.elapsed();

    assert!(
        elapsed < std::time::Duration::from_secs(40),
        "the stop was not bounded: the uninstall took {elapsed:?}"
    );
    assert!(
        !out.status.success(),
        "an uninstall whose stop never came back reported success:\n{}",
        stdout(&out)
    );
    for path in &placed {
        assert!(
            path.exists(),
            "{} was deleted by an uninstall that never stopped the units:\n{}",
            path.display(),
            stdout(&out)
        );
    }
}

/// A `disable --now` that `systemctl` accepted is not a unit that stopped.
///
/// With `--no-block` the command returns as soon as the job is queued, so the
/// only thing that says the daemon is gone is the daemon's own state. An
/// uninstall that removes the binary while `osm.service` is still active
/// leaves it running against a deleted file and loses the `ExecStop` shutdown
/// capture — the exact failure the stop-first ordering exists to prevent.
#[test]
fn an_uninstall_whose_units_never_go_inactive_removes_nothing() {
    let world = World::new("stopstuck");
    let placed = world.pretend_installed();

    let out = world.osm(&["uninstall"], &[("OSM_TEST_STAYS_ACTIVE", "1")]);

    assert!(
        !world.systemctl_log().is_empty(),
        "the stand-in systemctl never ran; this test proves nothing"
    );
    assert!(
        !out.status.success(),
        "an uninstall whose units are still running reported success:\n{}",
        stdout(&out)
    );
    for path in &placed {
        assert!(
            path.exists(),
            "{} was deleted while the units were still active: an accepted \
             disable was read as a stopped unit:\n{}",
            path.display(),
            stdout(&out)
        );
    }
}

/// The thirty-second confirmation deadline is thirty seconds.
///
/// It used to be checked only *after* a whole round of `is-active` calls and
/// the poll sleep, so a probe that started a moment before it expired still
/// ran for its own five-second budget — and so did the one for the second
/// unit. Against a manager that answers slowly, giving up therefore took the
/// stop budget plus a whole round: about forty seconds here, and about
/// thirty-five for an install, while README.md promised thirty.
#[test]
fn confirming_the_stop_gives_up_at_its_deadline_not_a_round_later() {
    let world = World::new("slowprobe");
    let placed = world.pretend_installed();

    let started = std::time::Instant::now();
    let out = world.osm(
        &["uninstall"],
        &[
            // The disable is accepted and the units go on running, so every
            // round finds both of them standing and the loop runs to its
            // deadline.
            ("OSM_TEST_STAYS_ACTIVE", "1"),
            // Each `is-active` answers after 3.45s, so a round of two of them
            // plus the poll sleep is 7s — which does not divide thirty. A
            // deadline checked only between rounds is therefore overrun by
            // most of a round: the fourth round starts at 28s and the fifth
            // answer lands at about 35.
            ("OSM_TEST_IS_ACTIVE_DELAY", "3.45"),
        ],
    );
    let elapsed = started.elapsed();

    assert!(
        !out.status.success(),
        "an uninstall whose units never went inactive reported success:\n{}",
        stdout(&out)
    );
    assert!(
        elapsed < std::time::Duration::from_secs(33),
        "confirming the stop overran its thirty-second deadline: the \
         uninstall took {elapsed:?}"
    );
    // And it did wait: a build that gave up at once would pass the line
    // above and be a different bug.
    assert!(
        elapsed > std::time::Duration::from_secs(25),
        "the confirmation gave up after {elapsed:?}, well inside the thirty \
         seconds a queued stop is allowed"
    );
    for path in &placed {
        assert!(
            path.exists(),
            "{} was deleted while the units were still active:\n{}",
            path.display(),
            stdout(&out)
        );
    }
}
