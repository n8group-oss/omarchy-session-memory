//! The restore pass: which sessions get a window, and what counts as failure.

use osm::desktop::{PlaceOutcome, PlacedWindow};
use osm::restore::window_outcomes_are_degraded as degraded;

fn o(session: &str, outcome: PlaceOutcome) -> (String, PlaceOutcome) {
    (session.to_string(), outcome)
}

/// A `Placed` outcome for a window nothing here inspects: these tests are
/// about which outcomes degrade a restore, not about which window was placed.
fn placed() -> PlaceOutcome {
    PlaceOutcome::Placed(PlacedWindow {
        address: "0x1".to_string(),
        workspace_kind: "numbered".to_string(),
        workspace_ref: "3".to_string(),
        monitor_connector: Some("DP-1".to_string()),
    })
}

#[test]
fn a_window_that_never_maps_makes_the_restore_partial() {
    assert!(degraded(&[
        o("dev", placed()),
        o("notes", PlaceOutcome::NeverMapped),
    ]));
}

#[test]
fn a_spawn_failure_is_degraded() {
    assert!(degraded(&[o(
        "dev",
        PlaceOutcome::SpawnFailed("no binary".into())
    )]));
}

#[test]
fn a_skipped_session_is_degraded() {
    // Skipped means a session that had a recorded window and did not get it
    // back. That is work the restore did not do.
    assert!(degraded(&[o(
        "dev",
        PlaceOutcome::Skipped("not delivered".into())
    )]));
}

#[test]
fn all_placed_is_not_degraded() {
    assert!(!degraded(&[o("dev", placed()), o("notes", placed())]));
}

#[test]
fn no_compositor_is_work_this_restore_did_not_do() {
    // It used to be excused as "a headless machine is a legitimate outcome".
    // At this point it is indistinguishable from a restore that beat Hyprland
    // to the finish line, or from a compositor that died — and both of those
    // retired the source snapshot, the only record of where the user's
    // terminals were, for work nobody did.
    assert!(degraded(&[
        o("dev", PlaceOutcome::NoCompositor),
        o("notes", PlaceOutcome::NoCompositor),
    ]));
}

#[test]
fn losing_the_compositor_after_spawning_is_degraded() {
    assert!(degraded(&[o(
        "dev",
        PlaceOutcome::LostCompositor("it stopped answering".into())
    )]));
}

#[test]
fn placement_switched_off_is_the_one_finished_outcome_without_a_window() {
    // The explicit opt-out, and the only thing that lets a machine with no
    // compositor ever retire a snapshot.
    assert!(!degraded(&[
        o("dev", PlaceOutcome::PlacementDisabled),
        o("notes", PlaceOutcome::PlacementDisabled),
    ]));
}

#[test]
fn nothing_to_place_is_not_degraded() {
    assert!(!degraded(&[]));
}

#[test]
fn outcomes_have_stable_machine_readable_names() {
    assert_eq!(placed().as_str(), "placed");
    assert_eq!(PlaceOutcome::NeverMapped.as_str(), "never_mapped");
    assert_eq!(PlaceOutcome::NoCompositor.as_str(), "no_compositor");
    assert_eq!(
        PlaceOutcome::SpawnFailed("x".into()).as_str(),
        "spawn_failed"
    );
    assert_eq!(PlaceOutcome::Skipped("x".into()).as_str(), "skipped");
    assert_eq!(
        PlaceOutcome::LostCompositor("x".into()).as_str(),
        "lost_compositor"
    );
    assert_eq!(
        PlaceOutcome::PlacementDisabled.as_str(),
        "placement_disabled"
    );
}

// ---------------------------------------------------------------------------
// place_windows: which sessions may be acted on at all.
// ---------------------------------------------------------------------------

use osm::desktop::{self, Placement};
use osm::hypr::HyprCtl;
use osm::restore::{place_windows, RestoreOutcome};
use osm::tmux::Tmux;

/// A compositor that answers, and records every dispatch instead of moving a
/// real window. `clients_json` never returns a marked window, so nothing can
/// be "found" and no spawn is ever needed for these tests.
struct Recording {
    dispatched: std::cell::RefCell<Vec<String>>,
}

impl HyprCtl for Recording {
    fn clients_json(&self) -> anyhow::Result<String> {
        Ok("[]".to_string())
    }
    fn monitors_json(&self) -> anyhow::Result<String> {
        Ok(r#"[{"id":0,"name":"DP-1","description":"d","x":0,"y":0,
            "width":1920,"height":1080,"scale":1.0,"transform":0,"focused":true}]"#
            .to_string())
    }
    fn dispatch(&self, lua: &str) -> anyhow::Result<String> {
        self.dispatched.borrow_mut().push(lua.to_string());
        Ok(String::new())
    }
}

/// Records what would have been spawned, and what it was asked to kill.
/// Nothing is executed and nothing is signalled.
///
/// A test that actually execs a terminal puts a window on the developer's
/// desktop — that happened once, attaching to a session named `mine` on
/// their real tmux server. This makes it impossible rather than unlikely.
#[derive(Default)]
struct NoSpawn {
    argv: std::cell::RefCell<Vec<Vec<String>>>,
    killed: std::cell::RefCell<Vec<u32>>,
}

impl osm::desktop::Spawner for NoSpawn {
    fn spawn(&self, argv: &[String]) -> anyhow::Result<osm::desktop::Spawned> {
        self.argv.borrow_mut().push(argv.to_vec());
        // A process that exists and is not a terminal: this one. The
        // compositor fakes never report a window owned by it, so placement
        // times out rather than pretending to have found one.
        Ok(osm::desktop::Spawned::of(std::process::id()).unwrap())
    }
    fn kill(&self, s: &osm::desktop::Spawned) {
        self.killed.borrow_mut().push(s.pid);
    }
}

/// A compositor that is not there.
struct Absent;
impl HyprCtl for Absent {
    fn clients_json(&self) -> anyhow::Result<String> {
        anyhow::bail!("no compositor")
    }
    fn monitors_json(&self) -> anyhow::Result<String> {
        anyhow::bail!("no compositor")
    }
    fn dispatch(&self, _: &str) -> anyhow::Result<String> {
        panic!("dispatch attempted with no compositor")
    }
}

fn seed(conn: &rusqlite::Connection, sessions: &[&str]) -> i64 {
    let with_kinds: Vec<(&str, &str)> = sessions.iter().map(|s| (*s, "ghostty")).collect();
    seed_kinds(conn, &with_kinds)
}

/// [`seed`], recording each session's window as having been captured in a
/// terminal of its own.
///
/// The captured class is a per-window fact, and the placement pass is
/// supposed to carry each window's own class into the terminal decision. A
/// fixture that gives every window the same one cannot tell whether it does.
fn seed_kinds(conn: &rusqlite::Connection, sessions: &[(&str, &str)]) -> i64 {
    conn.execute(
        "INSERT INTO snapshots (taken_at, boot_id, reason, state)
         VALUES (1, 'b', 'test', 'complete')",
        [],
    )
    .unwrap();
    let snap = conn.last_insert_rowid();
    let tx = conn.unchecked_transaction().unwrap();
    let ps: Vec<Placement> = sessions
        .iter()
        .map(|(s, kind)| Placement {
            session: (*s).into(),
            address: format!("0x{s}"),
            class: "com.mitchellh.ghostty".into(),
            terminal_kind: (*kind).into(),
            workspace_kind: "numbered".into(),
            workspace_ref: "3".into(),
            monitor_connector: "DP-1".into(),
            monitor_desc: None,
            monitor_scale: None,
            monitor_transform: None,
            floating: false,
            rel: None,
        })
        .collect();
    desktop::write_placements_in(&tx, snap, &ps).unwrap();
    tx.commit().unwrap();
    snap
}

/// A tmux handle naming a socket **no server runs on**, for the placement
/// tests whose subject is what happens before any terminal attaches.
///
/// Placement now proves a session's terminal really attached before it reports
/// `Placed`, so it needs the server the restore is working against. Nothing
/// here has one: every one of these tests stops at "no window this attempt
/// owns appeared", well before the attach is asked about. The name carries
/// this process's id so it can collide with nothing, and `list-clients` on a
/// socket with no server neither starts one nor leaves a file behind.
fn no_server() -> Tmux {
    Tmux::with_socket(&format!("osm-noserver-{}", std::process::id()))
}

fn cfg() -> osm::config::Config {
    let mut c = osm::config::Config::default();
    // Keep the never-mapped wait short; these tests never spawn anything.
    c.restore.readiness_timeout_secs = 1;
    // An explicitly named terminal, deliberately, even where the test does
    // not care which one. `auto` asks what this machine has installed, and a
    // test may assume nothing about that: CI has no terminal at all, the
    // maintainer's desktop has Ghostty, and a suite that quietly answers
    // differently on the two is a suite that proves nothing on either. An
    // explicit name is honoured whether or not it is installed — that is what
    // makes it a fixture. `auto`'s own behaviour is tested in
    // `tests/terminal_spawn.rs`, against a `PATH` of its own.
    c.restore.terminal = "ghostty".to_string();
    c
}

#[test]
fn a_session_this_attempt_did_not_deliver_is_never_spawned_onto() {
    // The rule Plan 3 established and this inherits: a conflicted session
    // holds someone else's topology. Opening a terminal onto it would
    // compound the mistake, so it is skipped and reported.
    let tmp = tempfile::tempdir().unwrap();
    let conn = osm::db::open(&tmp.path().join("s.db")).unwrap();
    let snap = seed(&conn, &["mine", "someone-elses"]);

    let mut outcome = RestoreOutcome::default();
    outcome.created.push("mine".to_string());
    // `someone-elses` is deliberately absent from every delivered list.

    let h = Recording {
        dispatched: Default::default(),
    };
    let sp = NoSpawn::default();
    let out = place_windows(&h, &sp, &no_server(), &conn, snap, &outcome, 7, &cfg());

    let skipped: Vec<&String> = out
        .iter()
        .filter(|(_, o)| matches!(o, PlaceOutcome::Skipped(_)))
        .map(|(s, _)| s)
        .collect();
    assert_eq!(skipped, vec!["someone-elses"], "{out:?}");
    assert!(
        h.dispatched.borrow().is_empty(),
        "nothing may be dispatched for a session we did not deliver: {:?}",
        h.dispatched.borrow()
    );
    let spawned: Vec<String> = sp.argv.borrow().iter().map(|a| a.join(" ")).collect();
    assert!(
        spawned.iter().all(|a| !a.contains("someone-elses")),
        "a terminal was spawned for a session we did not deliver: {spawned:?}"
    );
}

#[test]
fn an_absent_compositor_reports_no_compositor_without_spawning_anything() {
    // Spawning terminals we then cannot place would leave a trail of windows
    // with nowhere to go.
    let tmp = tempfile::tempdir().unwrap();
    let conn = osm::db::open(&tmp.path().join("s.db")).unwrap();
    let snap = seed(&conn, &["dev", "notes"]);

    let mut outcome = RestoreOutcome::default();
    outcome.created.push("dev".to_string());
    outcome.created.push("notes".to_string());

    let sp = NoSpawn::default();
    let out = place_windows(&Absent, &sp, &no_server(), &conn, snap, &outcome, 7, &cfg());
    assert_eq!(out.len(), 2);
    assert!(
        out.iter()
            .all(|(_, o)| matches!(o, PlaceOutcome::NoCompositor)),
        "{out:?}"
    );
    assert!(
        osm::restore::window_outcomes_are_degraded(&out),
        "an absent compositor is work still owed, not a finished restore"
    );
    assert!(
        sp.argv.borrow().is_empty(),
        "nothing may be spawned with nowhere to put it: {:?}",
        sp.argv.borrow()
    );
}

#[test]
fn placement_switched_off_neither_waits_for_a_compositor_nor_spawns() {
    // The one configuration under which a machine with no Hyprland can finish
    // a restore. Nothing is asked of the compositor at all — `Absent` panics
    // on dispatch and errors on everything else — and nothing is started.
    let tmp = tempfile::tempdir().unwrap();
    let conn = osm::db::open(&tmp.path().join("s.db")).unwrap();
    let snap = seed(&conn, &["dev"]);

    let mut outcome = RestoreOutcome::default();
    outcome.created.push("dev".to_string());

    let mut c = cfg();
    c.restore.place_windows = false;
    let sp = NoSpawn::default();
    let out = place_windows(&Absent, &sp, &no_server(), &conn, snap, &outcome, 7, &c);
    assert_eq!(
        out,
        vec![("dev".to_string(), PlaceOutcome::PlacementDisabled)]
    );
    assert!(!osm::restore::window_outcomes_are_degraded(&out));
    assert!(sp.argv.borrow().is_empty(), "{:?}", sp.argv.borrow());
}

#[test]
fn a_snapshot_with_no_placement_produces_no_outcomes() {
    let tmp = tempfile::tempdir().unwrap();
    let conn = osm::db::open(&tmp.path().join("s.db")).unwrap();
    let snap = seed(&conn, &[]);
    let sp = NoSpawn::default();
    let out = place_windows(
        &Recording {
            dispatched: Default::default(),
        },
        &sp,
        &no_server(),
        &conn,
        snap,
        &RestoreOutcome::default(),
        7,
        &cfg(),
    );
    assert!(out.is_empty(), "{out:?}");
}

#[test]
fn a_window_is_owned_because_we_started_it_not_because_of_its_class() {
    // Ghostty accepts --class=osm-restore-7 and reports com.mitchellh.ghostty
    // anyway. A marker in the class is therefore never seen, and matching on
    // one made every placement report NeverMapped. Ownership is the process
    // tree instead: this process owns itself, and pid 1 is not ours.
    let me = osm::desktop::Spawned::of(std::process::id()).unwrap();
    assert!(
        desktop::owns_window(&me, me.pid),
        "a process owns its own window"
    );
    assert!(
        !desktop::owns_window(&me, 1),
        "init is not a descendant of this test"
    );
    let nobody = osm::desktop::Spawned {
        pid: 9_999_999,
        start_ticks: 1,
    };
    assert!(
        !desktop::owns_window(&nobody, me.pid),
        "a pid that started nothing owns nothing"
    );
}

// ---------------------------------------------------------------------------
// Ownership is a process identity, not a number.
// ---------------------------------------------------------------------------

#[test]
fn a_recycled_pid_number_is_not_the_process_we_started() {
    // Linux reuses pids, and the readiness budget is long enough for a
    // terminal to die and its number to come back as something else. Under a
    // bare number comparison that unrelated process satisfies the check and
    // has its window moved — on this machine, the user's own.
    let real = osm::desktop::Spawned::of(std::process::id()).unwrap();
    let impostor = osm::desktop::Spawned {
        pid: real.pid,
        start_ticks: real.start_ticks + 1,
    };
    assert!(
        !desktop::owns_window(&impostor, real.pid),
        "the same pid number with a different start time is a different process"
    );
    assert!(real.is_alive());
    assert!(!impostor.is_alive());
}

#[test]
fn a_stale_window_pid_owns_nothing() {
    // A window whose process is gone leaves a pid that resolves to nothing.
    let me = osm::desktop::Spawned::of(std::process::id()).unwrap();
    assert!(!desktop::owns_window(&me, 4_194_304));
    assert!(osm::desktop::start_ticks(4_194_304).is_none());
}

#[test]
fn a_reparented_process_is_not_claimed_rather_than_wrongly_claimed() {
    // The parent exits, its child is reparented to init, and the child is
    // therefore no longer in the lineage of anything we started. The honest
    // answer is "not ours" — which costs a NeverMapped. The alternative,
    // claiming it anyway, means dispatching against a window nobody can show
    // belongs to this restore.
    let mut parent = std::process::Command::new("bash")
        .arg("-c")
        // The background child must not inherit the stdout pipe, or reading
        // it below blocks until that child exits rather than until the shell
        // does.
        .arg("sleep 30 >/dev/null 2>&1 </dev/null & echo $! ; exit 0")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("bash");
    let started = osm::desktop::Spawned::of(parent.id()).expect("the shell exists");
    let mut out = String::new();
    {
        use std::io::Read as _;
        parent.stdout.take().unwrap().read_to_string(&mut out).ok();
    }
    parent.wait().expect("the shell exits");
    let orphan: u32 = out.trim().parse().expect("a pid on stdout");

    assert!(
        !desktop::owns_window(&started, orphan),
        "a reparented process is not in our lineage and must not be claimed"
    );

    // Clean up the only thing this test started that outlives it. Named by
    // the pid it printed and nothing else — no pattern, no sweep.
    let _ = std::process::Command::new("bash")
        .arg("-c")
        .arg(format!("kill {orphan} 2>/dev/null || true"))
        .status();
}

#[test]
fn a_compositor_that_refuses_a_dispatch_does_not_report_a_placed_window() {
    // `hyprctl` reports a rejected dispatch as text on *successful* stdout.
    // Checking only `Result::is_err()` recorded an unmoved window as placed —
    // and then retired the snapshot that said where it belonged.
    struct Refuses;
    impl HyprCtl for Refuses {
        fn clients_json(&self) -> anyhow::Result<String> {
            // One window, owned by this process, so placement gets as far as
            // dispatching.
            Ok(format!(
                r#"[{{"address":"0xA","pid":{},"class":"com.mitchellh.ghostty",
                   "title":"t","workspace":{{"id":3,"name":"3"}},"monitor":0,
                   "at":[0,0],"size":[10,10],"floating":false}}]"#,
                std::process::id()
            ))
        }
        fn monitors_json(&self) -> anyhow::Result<String> {
            Ok(r#"[{"id":0,"name":"DP-1","description":"d","x":0,"y":0,
                "width":1920,"height":1080,"scale":1.0,"transform":0,"focused":true}]"#
                .to_string())
        }
        fn dispatch(&self, _lua: &str) -> anyhow::Result<String> {
            Ok("error: attempt to call a nil value".to_string())
        }
    }

    let p = Placement {
        session: "dev".into(),
        address: "0xA".into(),
        class: "com.mitchellh.ghostty".into(),
        terminal_kind: "ghostty".into(),
        workspace_kind: "numbered".into(),
        workspace_ref: "3".into(),
        monitor_connector: "DP-1".into(),
        monitor_desc: None,
        monitor_scale: None,
        monitor_transform: None,
        floating: false,
        rel: None,
    };
    let sp = NoSpawn::default();
    let outcome = desktop::spawn_and_place(
        &Refuses,
        &sp,
        &no_server(),
        &p,
        "osm-restore-7",
        // Named, not `auto`: see `cfg`.
        "ghostty",
        std::time::Duration::from_secs(1),
    );
    assert!(
        matches!(outcome, PlaceOutcome::LostCompositor(_)),
        "a refused dispatch was reported as {outcome:?}"
    );
    assert!(
        osm::restore::window_outcomes_are_degraded(&[("dev".to_string(), outcome)]),
        "a refused dispatch must keep the snapshot restorable"
    );
    assert_eq!(
        *sp.killed.borrow(),
        vec![std::process::id()],
        "a terminal osm cannot place must not be left on the user's desktop"
    );
}

#[test]
fn a_compositor_that_disappears_after_the_spawn_takes_the_terminal_back() {
    // The post-spawn half of the same problem: the compositor answered long
    // enough to be called reachable, a terminal was started, and then it
    // stopped answering. Reporting that as a finished restore retired the
    // snapshot; leaving the terminal running left a window osm no longer
    // tracks and would spawn a second copy of on the next attempt.
    struct AnswersOnce {
        calls: std::cell::Cell<u32>,
    }
    impl HyprCtl for AnswersOnce {
        fn clients_json(&self) -> anyhow::Result<String> {
            self.calls.set(self.calls.get() + 1);
            anyhow::bail!("compositor gone")
        }
        fn monitors_json(&self) -> anyhow::Result<String> {
            anyhow::bail!("compositor gone")
        }
        fn dispatch(&self, _lua: &str) -> anyhow::Result<String> {
            panic!("nothing may be dispatched to a compositor that is not there")
        }
    }

    let p = Placement {
        session: "dev".into(),
        address: "0xA".into(),
        class: "com.mitchellh.ghostty".into(),
        terminal_kind: "ghostty".into(),
        workspace_kind: "numbered".into(),
        workspace_ref: "3".into(),
        monitor_connector: "DP-1".into(),
        monitor_desc: None,
        monitor_scale: None,
        monitor_transform: None,
        floating: false,
        rel: None,
    };
    let sp = NoSpawn::default();
    let h = AnswersOnce {
        calls: std::cell::Cell::new(0),
    };
    let outcome = desktop::spawn_and_place(
        &h,
        &sp,
        &no_server(),
        &p,
        "osm-restore-7",
        // Named, not `auto`: see `cfg`.
        "ghostty",
        std::time::Duration::from_secs(1),
    );
    assert!(
        matches!(outcome, PlaceOutcome::LostCompositor(_)),
        "{outcome:?}"
    );
    assert!(osm::restore::window_outcomes_are_degraded(&[(
        "dev".to_string(),
        outcome
    )]));
    assert_eq!(*sp.killed.borrow(), vec![std::process::id()]);
}

#[test]
fn the_spawned_terminal_attaches_on_the_restores_own_socket() {
    // Without `-L` the terminal talks to the *default* server, where it
    // either finds nothing or attaches to an identically named session
    // belonging to whoever is using this machine. That is exactly how a test
    // once put a window on the maintainer's desktop attached to a session
    // called `mine` on their real server.
    let tmp = tempfile::tempdir().unwrap();
    let conn = osm::db::open(&tmp.path().join("s.db")).unwrap();
    let snap = seed(&conn, &["mine"]);

    let mut outcome = RestoreOutcome::default();
    outcome.created.push("mine".to_string());

    let h = Recording {
        dispatched: Default::default(),
    };
    let sp = NoSpawn::default();
    let socket = format!("osm-sock-{}", std::process::id());
    let _ = place_windows(
        &h,
        &sp,
        &Tmux::with_socket(&socket),
        &conn,
        snap,
        &outcome,
        7,
        &cfg(),
    );

    let cmd = sp
        .argv
        .borrow()
        .first()
        .and_then(|a| a.last().cloned())
        .expect("a terminal was asked for");
    assert!(
        cmd.contains(&format!("tmux -u -L '{socket}' attach-session")),
        "the terminal was not pointed at this restore's server: {cmd}"
    );
}

#[test]
fn an_explicitly_configured_terminal_wins_over_the_captured_one() {
    // `restore.terminal` was inert. Placement asked only what the snapshot
    // said the window's class had been, so a user whose Ghostty was gone
    // could set `restore.terminal = "kitty"`, watch every placement go on
    // trying to spawn ghostty, and find the setting they had used doing
    // nothing at all.
    //
    // Asserted where it shows: the argv the spawner was handed by
    // `place_windows`, not the return value of `terminal::detect`.
    let tmp = tempfile::tempdir().unwrap();
    let conn = osm::db::open(&tmp.path().join("s.db")).unwrap();
    // `seed` records every window as having been a ghostty.
    let snap = seed(&conn, &["dev"]);

    let mut outcome = RestoreOutcome::default();
    outcome.created.push("dev".to_string());

    let mut c = cfg();
    c.restore.terminal = "kitty".to_string();
    let h = Recording {
        dispatched: Default::default(),
    };
    let sp = NoSpawn::default();
    let _ = place_windows(&h, &sp, &no_server(), &conn, snap, &outcome, 7, &c);

    let argv = sp.argv.borrow();
    let first = argv.first().expect("a terminal was asked for");
    assert_eq!(
        first.first().map(String::as_str),
        Some("kitty"),
        "the configured terminal was ignored in favour of the captured one: {first:?}"
    );
    assert!(
        first.last().is_some_and(|cmd| cmd.contains("-t 'dev'")),
        "and it must still be pointed at the session: {first:?}"
    );
}

/// A directory holding `binaries`, each an executable file, and nothing else.
///
/// The whole `PATH` the terminal decision below is made against. Nothing here
/// reads this process's own `PATH`: on the maintainer's desktop it has their
/// real terminals on it, on a CI runner it has none, and a test that consults
/// it is a test that asks the machine what it thinks rather than asserting
/// anything.
fn path_with(binaries: &[&str]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for b in binaries {
        let p = dir.path().join(b);
        std::fs::write(&p, "#!/bin/sh\nexit 0\n").unwrap();
        let mut perm = std::fs::metadata(&p).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perm, 0o755);
        std::fs::set_permissions(&p, perm).unwrap();
    }
    dir
}

/// A [`NoSpawn`] whose terminal decision is made against a `PATH` this test
/// owns rather than the machine's.
///
/// Still spawns nothing and signals nothing: `choose_terminal` is the only
/// behaviour it adds.
struct Resolving {
    argv: std::cell::RefCell<Vec<Vec<String>>>,
    path: std::ffi::OsString,
}

impl Resolving {
    fn on(dir: &std::path::Path) -> Resolving {
        Resolving {
            argv: Default::default(),
            path: dir.into(),
        }
    }
}

impl osm::desktop::Spawner for Resolving {
    fn spawn(&self, argv: &[String]) -> anyhow::Result<osm::desktop::Spawned> {
        self.argv.borrow_mut().push(argv.to_vec());
        Ok(osm::desktop::Spawned::of(std::process::id()).unwrap())
    }
    fn kill(&self, _: &osm::desktop::Spawned) {}
    fn choose_terminal(&self, configured: &str, captured: &str) -> Option<osm::terminal::Kind> {
        osm::terminal::choose_on(configured, captured, Some(&self.path))
    }
}

#[test]
fn auto_opens_the_terminal_each_window_was_captured_in() {
    // `auto` is the default and means "whatever the session had, as far as
    // this machine allows". The plumbing that has to exist for that to mean
    // anything is that `place_windows` carries **each window's own** captured
    // class into the terminal decision and opens exactly what comes back.
    //
    // The previous version of this test could not see that plumbing at all.
    // Its fixture captured every window as a ghostty and its expectation
    // independently recomputed `choose("auto", "ghostty")`, so replacing the
    // captured class in `spawn_and_place` with the literal `"ghostty"` left
    // it passing — on a machine with terminals installed and on a CI runner
    // with none. Two windows captured in two different terminals, resolved
    // against a `PATH` holding exactly those two, is what makes the
    // substitution show: with the class hard-coded, both windows open the
    // same terminal.
    let dir = path_with(&["kitty", "foot"]);

    let tmp = tempfile::tempdir().unwrap();
    let conn = osm::db::open(&tmp.path().join("s.db")).unwrap();
    let snap = seed_kinds(&conn, &[("dev", "kitty"), ("notes", "foot")]);

    let mut outcome = RestoreOutcome::default();
    outcome.created.push("dev".to_string());
    outcome.created.push("notes".to_string());

    let mut c = cfg();
    c.restore.terminal = "auto".to_string();
    assert_eq!(
        osm::config::Config::default().restore.terminal,
        "auto",
        "the default is what is under test"
    );

    let h = Recording {
        dispatched: Default::default(),
    };
    let sp = Resolving::on(dir.path());
    let _ = place_windows(&h, &sp, &no_server(), &conn, snap, &outcome, 7, &c);

    // Matched by the session each terminal was told to attach to, not by the
    // order the placements came back in: which window is opened with which
    // terminal is the whole question.
    let argv = sp.argv.borrow();
    let opened = |session: &str| -> Vec<String> {
        argv.iter()
            .find(|a| {
                a.last()
                    .is_some_and(|cmd| cmd.contains(&format!("-t '{session}'")))
            })
            .unwrap_or_else(|| panic!("no terminal was opened for {session}: {argv:?}"))
            .clone()
    };

    let dev = opened("dev");
    let notes = opened("notes");
    assert_eq!(
        dev.first().map(String::as_str),
        Some("kitty"),
        "the window captured in kitty was opened in something else: {dev:?}"
    );
    assert_eq!(
        notes.first().map(String::as_str),
        Some("foot"),
        "the window captured in foot was opened in something else — the \
         captured class never reached the terminal decision: {notes:?}"
    );

    // And the argv is that terminal's, not merely named after it: each one
    // carries the ownership marker the way that terminal takes it.
    assert!(
        dev.iter().any(|a| a == "--class") && dev.iter().any(|a| a == "osm-restore-7"),
        "kitty was not given the marker as kitty takes it: {dev:?}"
    );
    assert!(
        notes.iter().any(|a| a == "--app-id=osm-restore-7"),
        "foot was not given the marker as foot takes it: {notes:?}"
    );
}

/// The control: `auto` is not free to substitute when the captured terminal
/// is installed, and an explicit `restore.terminal` overrides it outright.
/// Both are decided against the same `PATH` this test owns.
#[test]
fn what_auto_defers_to_and_what_overrides_it() {
    let dir = path_with(&["kitty", "foot"]);

    let tmp = tempfile::tempdir().unwrap();
    let conn = osm::db::open(&tmp.path().join("s.db")).unwrap();
    // Captured in a ghostty this machine no longer has: `auto` falls back to
    // what is installed, and the README promises exactly that.
    let snap = seed_kinds(&conn, &[("dev", "ghostty")]);

    let mut outcome = RestoreOutcome::default();
    outcome.created.push("dev".to_string());

    let mut c = cfg();
    c.restore.terminal = "auto".to_string();
    let h = Recording {
        dispatched: Default::default(),
    };
    let sp = Resolving::on(dir.path());
    let _ = place_windows(&h, &sp, &no_server(), &conn, snap, &outcome, 7, &c);
    assert_eq!(
        sp.argv.borrow().first().and_then(|a| a.first()).cloned(),
        Some("kitty".to_string()),
        "a captured terminal that is not installed must fall back, not fail: {:?}",
        sp.argv.borrow()
    );

    // Named outright, and honoured whether or not it is on that `PATH`.
    c.restore.terminal = "alacritty".to_string();
    let sp = Resolving::on(dir.path());
    let _ = place_windows(&h, &sp, &no_server(), &conn, snap, &outcome, 7, &c);
    assert_eq!(
        sp.argv.borrow().first().and_then(|a| a.first()).cloned(),
        Some("alacritty".to_string()),
        "an explicit restore.terminal was substituted: {:?}",
        sp.argv.borrow()
    );
}
