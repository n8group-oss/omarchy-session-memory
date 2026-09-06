//! A dispatch the compositor acknowledged is not a window that moved.
//!
//! # Why this file exists
//!
//! On the maintainer's desktop — Hyprland 0.56.2, Ghostty 1.3.1 — two
//! restored terminals captured on workspaces 9 and 10 both came back on
//! workspace 2, the one that happened to be active, while every per-window
//! outcome read `"outcome":"placed"`. Every `hyprctl` stub in this project
//! passed throughout, because each one acknowledged a dispatch and then went
//! on reporting the window wherever the fixture said it was. A compositor
//! fake that cannot disagree with osm proves nothing about placement.
//!
//! So the fake here **is** a compositor: it keeps the window's workspace and
//! monitor, and applies the two dispatches placement makes with the semantics
//! the real one has. Two of those semantics are the whole subject:
//!
//!   * `hl.dsp.window.move({window=…, workspace='N'})` puts the window on
//!     workspace N.
//!   * `hl.dsp.window.move({window=…, monitor='M'})` puts it on **M's active
//!     workspace**, whatever workspace it was on before.
//!
//! Both were verified by hand against the live compositor: the workspace move
//! stuck at +0.3s, +1s and +3s, and the monitor move that followed it took
//! the window straight back to workspace 2 in the same instant, with `ok`
//! from both calls and the window's address and pid unchanged throughout.

use osm::desktop::{self, PlaceOutcome, Placement, Spawned, Spawner};
use osm::hypr::HyprCtl;
use osm::tmux::Tmux;
use std::cell::RefCell;

mod common;

// ---------------------------------------------------------------------------
// A compositor that can disagree.
// ---------------------------------------------------------------------------

/// What the fake does with a dispatch it acknowledges.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Honours {
    /// Moves the window, the way Hyprland does.
    Yes,
    /// Answers `ok` and leaves the window exactly where it was. Not a
    /// contrived failure: a compositor is free to accept a call and have the
    /// window end up somewhere else — a client that remaps during startup
    /// does precisely that — and `ok` never said otherwise.
    No,
}

/// One window, its workspace, and its monitor.
struct Desk {
    /// The address the window answers to, which a re-created window changes.
    address: RefCell<String>,
    workspace: RefCell<String>,
    monitor: RefCell<i64>,
    honours: Honours,
    /// An unrelated window that took over the address ours used to have.
    /// Empty until [`Desk::reuse_address_after_first_dispatch`] fires.
    impostor: RefCell<Option<(String, String)>>,
    reuse_address: bool,
    dispatched: RefCell<Vec<String>>,
    /// How many more `monitors_json` reads answer with the real panel list
    /// before this compositor starts reporting a valid, **empty** one.
    /// `None` — the ordinary case — means it never stops answering.
    monitor_reads_left: std::cell::Cell<Option<u32>>,
    /// How many of the next `monitors_json` reads are spoiled, and how.
    /// Unlike [`Desk::monitors_until`] this is *transient*: once the count
    /// runs out the compositor answers properly again, which is what a
    /// mode switch, a DPMS blank or a momentarily busy `hyprctl` looks like.
    spoiled_reads_left: std::cell::Cell<u32>,
    spoil: std::cell::Cell<Spoil>,
}

/// How a deliberately spoiled `hyprctl -j monitors` read answers.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Spoil {
    /// A valid, well-formed, **empty** array — what the compositor returns
    /// while it has no output at that instant.
    Empty,
    /// The call does not come back with an answer at all: `hyprctl` exited
    /// non-zero, or its reply was not JSON.
    Unreadable,
}

/// `DP-1` is showing workspace 2; `eDP-1` is showing workspace 1.
fn active_workspace_of(monitor: &str) -> &'static str {
    match monitor {
        "eDP-1" => "1",
        _ => "2",
    }
}

fn monitor_id_of(monitor: &str) -> i64 {
    match monitor {
        "eDP-1" => 1,
        _ => 0,
    }
}

impl Desk {
    fn new(workspace: &str, monitor: i64, honours: Honours) -> Desk {
        Desk {
            address: RefCell::new("0xA".to_string()),
            workspace: RefCell::new(workspace.to_string()),
            monitor: RefCell::new(monitor),
            honours,
            impostor: RefCell::new(None),
            reuse_address: false,
            dispatched: RefCell::new(Vec::new()),
            monitor_reads_left: std::cell::Cell::new(None),
            spoiled_reads_left: std::cell::Cell::new(0),
            spoil: std::cell::Cell::new(Spoil::Empty),
        }
    }

    /// Spoil the next `reads` monitor queries and answer every one after
    /// them properly.
    ///
    /// Takes `&self` so a test can arm it *after* readiness has already
    /// watched this compositor report a monitor — which is the only
    /// arrangement that says anything about what placement does with a
    /// transient failure, as opposed to what it does on a machine that never
    /// had a screen.
    fn spoil_next_monitor_reads(&self, reads: u32, how: Spoil) {
        self.spoiled_reads_left.set(reads);
        self.spoil.set(how);
    }

    /// Answer `reads` monitor queries with the real panel list and every one
    /// after that with `[]`.
    ///
    /// Not a contrived failure. `hyprctl -j monitors` returns an empty array
    /// whenever the compositor has no output *at that instant* — a DPMS
    /// transition, a hotplug, a lid closing, a session being switched away
    /// from — and it is well-formed JSON, so nothing in the parsing layer
    /// objects to it. Readiness has already seen a monitor by the time
    /// placement runs, which is exactly what makes the later empty list a
    /// contradiction rather than a machine with no screens.
    fn monitors_until(self, reads: u32) -> Desk {
        self.monitor_reads_left.set(Some(reads));
        self
    }

    /// The window is destroyed and re-created at a new address on the first
    /// dispatch, and a window belonging to somebody else inherits the old
    /// one — already sitting on the target workspace, so that an address
    /// match alone would confirm the placement.
    fn reuse_address_after_first_dispatch(mut self, target: &str) -> Desk {
        self.reuse_address = true;
        *self.impostor.borrow_mut() = Some(("0xA".to_string(), target.to_string()));
        self
    }

    fn workspace(&self) -> String {
        self.workspace.borrow().clone()
    }

    fn client_json(&self, address: &str, pid: u32, ws: &str, monitor: i64) -> String {
        format!(
            r#"{{"address":"{address}","pid":{pid},"class":"com.mitchellh.ghostty",
               "title":"omarchy:lies","workspace":{{"id":9,"name":"{ws}"}},
               "monitor":{monitor},"at":[0,0],"size":[100,100],"floating":false}}"#
        )
    }
}

impl HyprCtl for Desk {
    fn clients_json(&self, _budget: std::time::Duration) -> anyhow::Result<String> {
        // Ours is owned by this process, which is what `NoSpawn` reports as
        // the terminal it started.
        let mut windows = vec![self.client_json(
            &self.address.borrow(),
            std::process::id(),
            &self.workspace.borrow(),
            *self.monitor.borrow(),
        )];
        if let Some((addr, ws)) = self.impostor.borrow().as_ref() {
            if *addr != *self.address.borrow() {
                // pid 1 is in nobody's lineage but init's, so this window is
                // not one this attempt started.
                windows.push(self.client_json(addr, 1, ws, 0));
            }
        }
        Ok(format!("[{}]", windows.join(",")))
    }

    fn monitors_json(&self, _budget: std::time::Duration) -> anyhow::Result<String> {
        let spoiled = self.spoiled_reads_left.get();
        if spoiled > 0 {
            self.spoiled_reads_left.set(spoiled - 1);
            return match self.spoil.get() {
                Spoil::Empty => Ok("[]".to_string()),
                Spoil::Unreadable => anyhow::bail!("hyprctl: no such file or directory"),
            };
        }
        if let Some(left) = self.monitor_reads_left.get() {
            if left == 0 {
                // Valid JSON, valid shape, no monitors. Every parser in this
                // crate accepts it.
                return Ok("[]".to_string());
            }
            self.monitor_reads_left.set(Some(left - 1));
        }
        Ok(
            r#"[{"id":0,"name":"DP-1","description":"Dell Inc. AW3423DWF","x":0,"y":0,
                "width":3440,"height":1440,"scale":1.0,"transform":0,"focused":true},
               {"id":1,"name":"eDP-1","description":"built-in","x":3440,"y":0,
                "width":1920,"height":1080,"scale":1.0,"transform":0,"focused":false}]"#
                .to_string(),
        )
    }

    fn dispatch(&self, lua: &str, _budget: std::time::Duration) -> anyhow::Result<String> {
        let first = self.dispatched.borrow().is_empty();
        self.dispatched.borrow_mut().push(lua.to_string());

        if self.honours == Honours::Yes {
            if let Some(ws) = between(lua, "workspace='", "'") {
                *self.workspace.borrow_mut() = ws;
            } else if let Some(m) = between(lua, "monitor='", "'") {
                // The call that undid the workspace move on the real thing.
                *self.monitor.borrow_mut() = monitor_id_of(&m);
                *self.workspace.borrow_mut() = active_workspace_of(&m).to_string();
            }
        }
        if first && self.reuse_address {
            *self.address.borrow_mut() = "0xB".to_string();
        }
        // Exactly what Hyprland 0.56.2 answers to a dispatch it accepted.
        Ok("ok".to_string())
    }
}

fn between(s: &str, open: &str, close: &str) -> Option<String> {
    let start = s.find(open)? + open.len();
    let rest = &s[start..];
    let end = rest.find(close)?;
    Some(rest[..end].to_string())
}

/// Records what it was asked to start and to kill, and does neither.
#[derive(Default)]
struct NoSpawn {
    killed: RefCell<Vec<u32>>,
}

impl Spawner for NoSpawn {
    fn spawn(&self, _argv: &[String]) -> anyhow::Result<Spawned> {
        // A process that exists and is not a terminal: this one. The fake
        // compositor reports its window as belonging to it, so the ownership
        // proof placement uses is satisfied without anything being executed.
        Ok(Spawned::of(std::process::id()).unwrap())
    }
    fn kill(&self, s: &Spawned) {
        self.killed.borrow_mut().push(s.pid);
    }
}

// ---------------------------------------------------------------------------
// A tmux server for the attach half.
// ---------------------------------------------------------------------------

/// A private tmux server with one session and one attached client, so
/// placement can get past the attach check and reach the outcome under test.
///
/// The client is a **child of this test process**, which is also the process
/// `NoSpawn` reports as the terminal it started — so the ownership proof
/// `attached` makes is satisfied honestly rather than bypassed.
struct Server {
    tmux: Tmux,
    client: std::process::Child,
}

impl Server {
    fn start(label: &str, session: &str) -> Server {
        let tmux = Tmux::with_socket(&format!("osm-confirm-{}-{}", label, std::process::id()));
        tmux.run(&["new-session", "-d", "-s", session, "-n", "w", "-c", "/tmp"])
            .expect("a private tmux server");
        let client = tmux
            .spawn(&["-C", "attach-session", "-t", session])
            .expect("a control-mode client");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let attached = tmux
                .run(&["list-clients", "-F", "#{client_session}"])
                .map(|raw| raw.lines().any(|l| l.trim() == session))
                .unwrap_or(false);
            if attached {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the control-mode client never attached"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        Server { tmux, client }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.client.kill();
        let _ = self.client.wait();
        common::shutdown(&self.tmux);
    }
}

/// A placement asking for `workspace` on `monitor`.
fn want(session: &str, workspace: &str, monitor: &str) -> Placement {
    Placement {
        session: session.to_string(),
        address: "0xA".into(),
        class: "com.mitchellh.ghostty".into(),
        terminal_kind: "ghostty".into(),
        workspace_kind: "numbered".into(),
        workspace_ref: workspace.to_string(),
        monitor_connector: monitor.to_string(),
        monitor_desc: None,
        monitor_scale: None,
        monitor_transform: None,
        floating: false,
        rel: None,
    }
}

fn place(h: &Desk, sp: &NoSpawn, t: &Tmux, p: &Placement) -> PlaceOutcome {
    desktop::spawn_and_place(
        h,
        sp,
        t,
        p,
        "osm-restore-7",
        // Named, never `auto`: `auto` asks what this machine has installed.
        "ghostty",
        std::time::Duration::from_secs(2),
    )
}

// ---------------------------------------------------------------------------
// The tests.
// ---------------------------------------------------------------------------

#[test]
fn a_compositor_that_accepts_the_dispatch_and_moves_nothing_is_not_a_placed_window() {
    // The failure this whole file is about, in its purest form: every call
    // acknowledged, the window still on the workspace it mapped on. `placed`
    // has to mean "it is there", not "I asked" — the second reading is how a
    // restore comes to retire the only record of where the user's terminals
    // belonged in exchange for work it never did.
    let server = Server::start("stubborn", "dev");
    let desk = Desk::new("2", 0, Honours::No);
    let sp = NoSpawn::default();
    let outcome = place(&desk, &sp, &server.tmux, &want("dev", "3", "DP-1"));

    assert_eq!(
        outcome.as_str(),
        "misplaced",
        "the window never left workspace 2 and the restore called it {:?}; \
         dispatches: {:?}",
        outcome,
        desk.dispatched.borrow()
    );
    assert!(
        osm::restore::window_outcomes_are_degraded(&[("dev".to_string(), outcome)]),
        "a window that is not where it belongs must keep the snapshot restorable"
    );
    assert!(
        sp.killed.borrow().is_empty(),
        "a terminal on the wrong workspace is still the user's restored \
         terminal, holding their session: it must not be taken away"
    );
}

#[test]
fn the_monitor_dispatch_does_not_undo_the_workspace_move() {
    // The mechanism, reproduced. Placement emitted the workspace move and
    // then, unconditionally, a move to the target monitor — and on Hyprland
    // 0.56.2 a window sent to a monitor lands on that monitor's *active*
    // workspace. With one monitor, which is the maintainer's machine, the
    // second dispatch therefore undid the first every single time.
    let server = Server::start("undo", "dev");
    let desk = Desk::new("2", 0, Honours::Yes);
    let sp = NoSpawn::default();
    let outcome = place(&desk, &sp, &server.tmux, &want("dev", "3", "DP-1"));

    assert_eq!(
        desk.workspace(),
        "3",
        "the window did not end up on the workspace it was captured on; \
         dispatches: {:?}",
        desk.dispatched.borrow()
    );
    assert_eq!(
        outcome.as_str(),
        "placed",
        "a window that really is on its workspace must be reported as placed: \
         {outcome:?}"
    );
}

#[test]
fn a_window_already_on_its_monitor_is_not_sent_to_it_again() {
    // The dispatch that cannot help and can only hurt. The window is already
    // on `DP-1`; asking for it again buys nothing and costs the workspace.
    let server = Server::start("nomon", "dev");
    let desk = Desk::new("2", 0, Honours::Yes);
    let sp = NoSpawn::default();
    let _ = place(&desk, &sp, &server.tmux, &want("dev", "3", "DP-1"));

    assert!(
        !desk
            .dispatched
            .borrow()
            .iter()
            .any(|d| d.contains("monitor='DP-1'")),
        "the window was sent to the monitor it was already on: {:?}",
        desk.dispatched.borrow()
    );
}

#[test]
fn a_workspace_that_lives_on_the_wrong_monitor_is_reported_not_papered_over() {
    // The window is on the workspace it was captured on, and that workspace
    // is currently on the wrong output. Sending the *window* to the right
    // output would take it off its workspace — the exact trade the old code
    // made silently, and the reason both of the maintainer's terminals came
    // back on workspace 2.
    //
    // So nothing is dispatched that would do that, the window keeps its
    // workspace, and the shortfall is reported rather than dressed up as
    // success. A restore that says `misplaced` leaves the snapshot
    // restorable; one that says `placed` retires it.
    let server = Server::start("wrongmon", "dev");
    let desk = Desk::new("3", 0, Honours::Yes);
    let sp = NoSpawn::default();
    let outcome = place(&desk, &sp, &server.tmux, &want("dev", "3", "eDP-1"));

    assert_eq!(
        desk.workspace(),
        "3",
        "the window was taken off its own workspace to satisfy the monitor: {:?}",
        desk.dispatched.borrow()
    );
    assert_eq!(
        outcome.as_str(),
        "misplaced",
        "a window on the wrong monitor was reported as {outcome:?}"
    );
    let detail = outcome.detail().unwrap_or_default().to_string();
    assert!(
        detail.contains("eDP-1"),
        "the report must name what is wrong: {detail:?}"
    );
}

#[test]
fn a_window_that_inherited_our_address_cannot_confirm_our_placement() {
    // Hyprland reuses `CWindow` allocations, and the same addresses recurred
    // across independent runs on the maintainer's machine
    // (`0x55a554de9f80` was one window's during a capture and a different
    // window's in the next run entirely). So an address is not an identity
    // across a window's destruction, and a confirmation loop that looks the
    // window up by address can be satisfied by a stranger sitting on the
    // target workspace.
    //
    // Here ours is re-created at `0xB` and somebody else's window takes over
    // `0xA`, already on workspace 3. The placement must follow the window it
    // owns — proven by process lineage, the same proof used to find it in the
    // first place — and report that one's address.
    let server = Server::start("reuse", "dev");
    let desk = Desk::new("2", 0, Honours::Yes).reuse_address_after_first_dispatch("3");
    let sp = NoSpawn::default();
    let outcome = place(&desk, &sp, &server.tmux, &want("dev", "3", "DP-1"));

    match outcome {
        PlaceOutcome::Placed(w) => assert_eq!(
            w.address, "0xB",
            "the placement was confirmed against a window it did not place"
        ),
        other => panic!("expected the re-created window to be placed: {other:?}"),
    }
}

#[test]
fn a_monitor_list_that_empties_after_readiness_is_not_a_placed_window() {
    // Readiness is what proves a compositor is there: it waits for
    // `hyprctl -j monitors` to come back as a non-empty array, and only then
    // is a single terminal spawned. So by the time placement reads the
    // monitors again, a monitor has already been observed on this compositor.
    //
    // Here the next read is `[]` — well-formed, parseable, and empty. What
    // that used to buy: `resolve_monitor` returned nothing, `placement_gap`
    // asked about the workspace and nothing else, the window was confirmed
    // `placed` on workspace 3 of whatever panel it happened to map on, and
    // the claim carried no connector at all — so the publication skipped the
    // monitor check too and the run reported `succeeded`, retiring the
    // snapshot that held the panel the terminal really belonged on.
    //
    // A desktop osm cannot read is not a desktop osm has satisfied.
    let server = Server::start("emptymon", "dev");
    let desk = Desk::new("2", 0, Honours::Yes).monitors_until(1);

    assert!(
        osm::hypr::wait_until_reachable(&desk, std::time::Duration::from_secs(2)),
        "readiness must see a monitor, or this proves nothing about what \
         happens after it"
    );

    let sp = NoSpawn::default();
    let outcome = place(&desk, &sp, &server.tmux, &want("dev", "3", "DP-1"));

    assert_ne!(
        outcome.as_str(),
        "placed",
        "the compositor listed no monitor to confirm against and the restore \
         called it {outcome:?}; dispatches: {:?}",
        desk.dispatched.borrow()
    );
    assert!(
        osm::restore::window_outcomes_are_degraded(&[("dev".to_string(), outcome.clone())]),
        "a placement that could not be confirmed must keep the snapshot \
         restorable: {outcome:?}"
    );
    let detail = outcome.detail().unwrap_or_default().to_string();
    assert!(
        detail.contains("monitor"),
        "the report must say what could not be read: {detail:?}"
    );
}

#[test]
fn a_placed_window_always_names_the_monitor_it_was_sent_to() {
    // The claim the publication is held to. A `Placed` that carries no
    // connector is a claim nothing can check — `unplaced_windows` skipped the
    // monitor comparison for exactly those — so the one outcome that retires
    // the user's snapshot must never be reachable without a monitor resolved
    // and verified.
    let server = Server::start("named", "dev");
    let desk = Desk::new("2", 0, Honours::Yes);
    let sp = NoSpawn::default();

    match place(&desk, &sp, &server.tmux, &want("dev", "3", "DP-1")) {
        PlaceOutcome::Placed(w) => assert_eq!(
            w.monitor_connector.as_deref(),
            Some("DP-1"),
            "a placed window must name the panel it was sent to"
        ),
        other => panic!("expected a placed window: {other:?}"),
    }
}

#[test]
fn a_monitor_list_that_empties_and_comes_back_still_places_the_window() {
    // The other side of `a_monitor_list_that_empties_after_readiness_is_not_a
    // _placed_window`. Refusing to confirm against a monitor list nobody
    // could read is right; treating the *first* unreadable answer as proof
    // that the compositor is gone is not, because the reply that reaches this
    // code is the terminal's death warrant — `lost` kills it.
    //
    // `hyprctl -j monitors` returns `[]` for as long as the compositor has no
    // output at that instant, and a single-monitor machine changing mode has
    // exactly such an instant. On the maintainer's machine — one monitor —
    // this read is on the path of every restore, so one blink destroyed a
    // window that was otherwise perfectly restored and left the boot degraded
    // for no reason.
    let server = Server::start("blipempty", "dev");
    let desk = Desk::new("2", 0, Honours::Yes);
    assert!(
        osm::hypr::wait_until_reachable(&desk, std::time::Duration::from_secs(2)),
        "readiness must see a monitor, or this proves nothing about what \
         happens after it"
    );
    // Two, not one: a fix that retries exactly once would pass with one.
    desk.spoil_next_monitor_reads(2, Spoil::Empty);

    let sp = NoSpawn::default();
    let outcome = place(&desk, &sp, &server.tmux, &want("dev", "3", "DP-1"));

    match &outcome {
        PlaceOutcome::Placed(w) => assert_eq!(
            w.monitor_connector.as_deref(),
            Some("DP-1"),
            "the placement was confirmed without naming the panel it was sent to"
        ),
        other => panic!(
            "a compositor that blinked and answered must not cost the user \
             their restored terminal: {other:?}; dispatches: {:?}",
            desk.dispatched.borrow()
        ),
    }
    assert!(
        sp.killed.borrow().is_empty(),
        "a terminal holding the user's session was killed over a monitor read \
         that recovered: {:?}",
        sp.killed.borrow()
    );
}

#[test]
fn a_monitor_read_that_fails_and_recovers_still_places_the_window() {
    // The same blink, arriving as an error rather than as an empty list: a
    // `hyprctl` that could not be started, exited non-zero, or answered
    // something that is not JSON. Both used to reach `lost` on the first
    // occurrence and both take the terminal with them.
    let server = Server::start("blipfail", "dev");
    let desk = Desk::new("2", 0, Honours::Yes);
    assert!(
        osm::hypr::wait_until_reachable(&desk, std::time::Duration::from_secs(2)),
        "readiness must see a monitor, or this proves nothing about what \
         happens after it"
    );
    desk.spoil_next_monitor_reads(2, Spoil::Unreadable);

    let sp = NoSpawn::default();
    let outcome = place(&desk, &sp, &server.tmux, &want("dev", "3", "DP-1"));

    match &outcome {
        PlaceOutcome::Placed(w) => assert_eq!(
            w.monitor_connector.as_deref(),
            Some("DP-1"),
            "the placement was confirmed without naming the panel it was sent to"
        ),
        other => panic!(
            "a monitor read that failed once and then answered must not cost \
             the user their restored terminal: {other:?}; dispatches: {:?}",
            desk.dispatched.borrow()
        ),
    }
    assert!(
        sp.killed.borrow().is_empty(),
        "a terminal holding the user's session was killed over a monitor read \
         that recovered: {:?}",
        sp.killed.borrow()
    );
}
