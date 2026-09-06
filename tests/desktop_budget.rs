//! A deadline that does not bound the calls it waits on is not a deadline.
//!
//! # Why this file exists
//!
//! Confirming a placement is a loop of `hyprctl` calls: the dispatches that
//! ask for the move, then a read that says whether it happened. Each of those
//! is a subprocess with its own timeout — fifteen seconds, deliberately
//! generous, because a busy compositor during a boot restore is the ordinary
//! case and calling a slow reply a dead compositor costs the user their
//! terminal.
//!
//! The confirmation deadline was checked only *between* passes. A floating
//! window emits five dispatches — monitor, workspace, float, move, resize —
//! and they were issued back to back with no clock consulted, and then a read
//! on top: five fifteens and one more, about ninety seconds for one window
//! that was never going to land, with every session behind it waiting its
//! turn. On the machine this runs on that is a boot.
//!
//! So the compositor here is slow rather than broken — every call takes
//! [`DELAY`] and every dispatch is acknowledged, exactly as Hyprland answers
//! a call it accepted — and the window never moves. The subject is how much
//! of it osm does before its own budget stops it.

use osm::desktop::{self, Placement, Spawned, Spawner};
use osm::hypr::HyprCtl;
use osm::tmux::Tmux;
use std::cell::RefCell;
use std::time::Duration;

/// How long each compositor call takes. Long enough that the confirmation
/// budget below cannot fit five of them, short enough to keep the test quick.
const DELAY: Duration = Duration::from_millis(300);

/// The whole budget one window is given. `spawn_and_place` spends part of it
/// finding the window and reads the monitors once, then confirms within the
/// same figure — so the confirmation itself gets 900ms, which is three calls
/// and not five.
const BUDGET: Duration = Duration::from_millis(900);

/// A compositor that answers everything, slowly, and moves nothing.
///
/// Not a contrived failure: `ok` from a dispatch has never meant the window
/// went anywhere (see `tests/desktop_confirm.rs`), and a compositor that is
/// slow while a boot restore starts several terminals at once is the
/// condition the fifteen-second per-call timeout exists for.
#[derive(Default)]
struct Slow {
    dispatched: RefCell<Vec<String>>,
    /// The budget each *dispatch* was given, which is the number the
    /// confirmation deadline has to reach.
    budgets: RefCell<Vec<Duration>>,
}

impl HyprCtl for Slow {
    fn clients_json(&self, budget: Duration) -> anyhow::Result<String> {
        // A real `hyprctl` is killed at its budget. This one is quicker than
        // any budget it is given, so the timings below are the delays and
        // nothing else.
        std::thread::sleep(DELAY.min(budget));
        // One window, ours by process lineage, on workspace 2 of monitor 1 —
        // neither the workspace nor the monitor it is wanted on, so there is
        // always something left to dispatch and the loop never converges.
        Ok(format!(
            r#"[{{"address":"0xA","pid":{},"class":"com.mitchellh.ghostty",
               "title":"t","workspace":{{"id":2,"name":"2"}},"monitor":1,
               "at":[0,0],"size":[10,10],"floating":false}}]"#,
            std::process::id()
        ))
    }

    fn monitors_json(&self, budget: Duration) -> anyhow::Result<String> {
        std::thread::sleep(DELAY.min(budget));
        Ok(r#"[{"id":0,"name":"DP-1","description":"d","x":0,"y":0,
              "width":1920,"height":1080,"scale":1.0,"transform":0,"focused":true},
             {"id":1,"name":"eDP-1","description":"built-in","x":1920,"y":0,
              "width":1920,"height":1080,"scale":1.0,"transform":0,"focused":false}]"#
            .to_string())
    }

    fn dispatch(&self, lua: &str, budget: Duration) -> anyhow::Result<String> {
        self.budgets.borrow_mut().push(budget);
        std::thread::sleep(DELAY.min(budget));
        self.dispatched.borrow_mut().push(lua.to_string());
        // Exactly what Hyprland answers to a dispatch it accepted.
        Ok("ok".to_string())
    }
}

/// Records what it was asked to start and kill, and does neither.
#[derive(Default)]
struct NoSpawn {
    killed: RefCell<Vec<u32>>,
}

impl Spawner for NoSpawn {
    fn spawn(&self, _argv: &[String]) -> anyhow::Result<Spawned> {
        // A process that exists and is not a terminal: this one, which is
        // what the fake compositor reports the window as belonging to.
        Ok(Spawned::of(std::process::id()).unwrap())
    }
    fn kill(&self, s: &Spawned) {
        self.killed.borrow_mut().push(s.pid);
    }
}

/// A placement that needs every dispatch `place_lua` can emit: a different
/// monitor, a different workspace, floating, and a geometry.
fn want() -> Placement {
    Placement {
        session: "dev".into(),
        address: "0xA".into(),
        class: "com.mitchellh.ghostty".into(),
        terminal_kind: "ghostty".into(),
        workspace_kind: "numbered".into(),
        workspace_ref: "3".into(),
        monitor_connector: "DP-1".into(),
        monitor_desc: None,
        monitor_scale: Some(1.0),
        monitor_transform: Some(0),
        floating: true,
        rel: Some((0.1, 0.1, 0.5, 0.5)),
    }
}

/// A tmux handle for a server that is not running.
///
/// Nothing here reaches the attach check — the confirmation gives up first —
/// but `spawn_and_place` needs a handle to build the terminal's argv from,
/// and it must carry a `-L` name so that nothing in this file could address
/// the developer's own server even if the flow changed.
fn no_server() -> Tmux {
    Tmux::with_socket(&format!("osm-budget-{}", std::process::id()))
}

#[test]
fn the_confirmation_budget_bounds_the_dispatches_it_waits_on() {
    // Five dispatches are owed. Three of them fit in the budget. A deadline
    // checked only after the batch issues all five and then reads on top of
    // them, which is what turned one unplaceable window into a minute and a
    // half of a boot.
    let h = Slow::default();
    let sp = NoSpawn::default();
    let started = std::time::Instant::now();
    let outcome = desktop::spawn_and_place(
        &h,
        &sp,
        &no_server(),
        &want(),
        "osm-restore-7",
        // Named, never `auto`: `auto` asks what this machine has installed.
        "ghostty",
        BUDGET,
    );
    let took = started.elapsed();

    let dispatched = h.dispatched.borrow().clone();
    assert!(
        dispatched.len() <= 3,
        "the confirmation issued {} dispatches on a {:?} budget with each call \
         taking {:?}; the deadline is not being checked before each one:\n  {}",
        dispatched.len(),
        BUDGET,
        DELAY,
        dispatched.join("\n  ")
    );
    assert!(
        !dispatched.is_empty(),
        "nothing was dispatched at all, so this test is not about the budget"
    );
    assert_eq!(
        outcome.as_str(),
        "misplaced",
        "a window that never reached its workspace was reported as {outcome:?} \
         after {took:?}"
    );
    assert!(
        sp.killed.borrow().is_empty(),
        "a window osm ran out of budget on is still the user's restored \
         terminal holding their session: it must not be taken away"
    );
    assert!(
        osm::restore::window_outcomes_are_degraded(&[("dev".to_string(), outcome)]),
        "a placement that was not confirmed must keep the snapshot restorable"
    );

    // And the budget reached the calls themselves, not only the loop around
    // them. A dispatch handed the flat per-call ceiling can run for that
    // whole ceiling *after* the deadline has passed, which is the other half
    // of the ninety seconds.
    let budgets = h.budgets.borrow().clone();
    assert!(
        budgets.iter().all(|b| *b < osm::hypr::CALL_TIMEOUT),
        "a dispatch was given the flat per-call ceiling ({:?}) rather than what \
         was left of the confirmation budget: {budgets:?}",
        osm::hypr::CALL_TIMEOUT
    );
}
