//! The monitor re-read stops when its own budget says so.
//!
//! # Why this file exists
//!
//! After the window has mapped, `spawn_and_place` asks the compositor for its
//! monitor list and will not accept an empty one: a machine that has just been
//! seen to have a screen does not stop having one, and accepting the blank
//! answer put a restored terminal on the wrong monitor and then retired the
//! snapshot that knew the right one. So the list is asked for again, inside a
//! three-second budget — the length of a mode switch, not of a wait for slow
//! work.
//!
//! Three seconds is what it says. Four and a bit is what it did. The loop
//! checked its deadline only *after* a read, then slept a flat 150 ms and
//! started the next read with no check at all — and `call_budget` never hands
//! out less than a second, so that unchecked read was granted a full second on
//! top of a deadline that had already passed. A compositor that answers
//! quickly just before the deadline therefore bought 150 ms of sleep plus a
//! second of overrun, per session, on a boot restore that walks several of
//! them in turn.
//!
//! # What this actually verifies
//!
//! A fake compositor with a clock: every monitor read answers 50 ms before the
//! budget it was handed runs out — so the last read inside the deadline lands
//! just short of it, which is precisely the moment the flat sleep used to step
//! over — except a read handed the floor budget, which is the grant made to a
//! call started with nothing left; that one is a compositor that has wedged
//! and it uses the whole second. The subject is the wall clock: how long
//! `spawn_and_place` takes to give up on a three-second read, and whether it
//! ever starts a read after the deadline it is working to.
//!
//! # What it does not claim
//!
//! Not that a *call already running* cannot overrun the deadline. It can, by
//! design: `MIN_CALL_BUDGET` gives the last call before a deadline a whole
//! second rather than cutting it off for arithmetic. What must not happen is a
//! call being *started* after the deadline has passed.

use osm::desktop::{self, Placement, Spawned, Spawner};
use osm::hypr::HyprCtl;
use osm::tmux::Tmux;
use std::cell::RefCell;
use std::time::{Duration, Instant};

/// `MONITOR_READ_BUDGET` in `src/desktop.rs`, which is private to it.
const MONITOR_BUDGET: Duration = Duration::from_secs(3);

/// `MIN_CALL_BUDGET` in `src/desktop.rs`, likewise. A call handed exactly this
/// much is a call started with nothing, or nearly nothing, left.
const MIN_CALL_BUDGET: Duration = Duration::from_secs(1);

/// How long before its budget runs out an answering compositor replies.
const EARLY: Duration = Duration::from_millis(50);

/// A compositor that answers every monitor read at the last moment, with no
/// monitors in it.
///
/// An empty list is what a real Hyprland returns across a mode switch or a
/// DPMS blank — a valid answer that names no panel — and it is the answer the
/// retry loop exists for. This one never stops giving it, which is the case
/// the budget has to bound.
#[derive(Default)]
struct LateAndBlank {
    /// When each monitor read was started.
    reads: RefCell<Vec<Instant>>,
}

impl HyprCtl for LateAndBlank {
    fn clients_json(&self, _budget: Duration) -> anyhow::Result<String> {
        // Instant, and ours by process lineage: the window is found on the
        // first look so that everything measured below is the monitor read.
        Ok(format!(
            r#"[{{"address":"0xA","pid":{},"class":"com.mitchellh.ghostty",
               "title":"t","workspace":{{"id":3,"name":"3"}},"monitor":0,
               "at":[0,0],"size":[10,10],"floating":true}}]"#,
            std::process::id()
        ))
    }

    fn monitors_json(&self, budget: Duration) -> anyhow::Result<String> {
        self.reads.borrow_mut().push(Instant::now());
        // A call given more than the floor still has real time in front of the
        // deadline; it answers just before that time is up. A call given
        // exactly the floor was started with nothing left, and a wedged
        // compositor spends all of it.
        std::thread::sleep(if budget > MIN_CALL_BUDGET {
            budget.saturating_sub(EARLY)
        } else {
            budget
        });
        Ok("[]".to_string())
    }

    fn dispatch(&self, lua: &str, _budget: Duration) -> anyhow::Result<String> {
        panic!("nothing may be dispatched: the monitor list was never readable ({lua})");
    }
}

/// Records what it was asked to start and kill, and does neither.
#[derive(Default)]
struct NoSpawn {
    killed: RefCell<Vec<u32>>,
}

impl Spawner for NoSpawn {
    fn spawn(&self, _argv: &[String]) -> anyhow::Result<Spawned> {
        Ok(Spawned::of(std::process::id()).unwrap())
    }
    fn kill(&self, s: &Spawned) {
        self.killed.borrow_mut().push(s.pid);
    }
}

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

/// A tmux handle for a server that is not running, named after this process so
/// that nothing here could address the developer's own server.
fn no_server() -> Tmux {
    Tmux::with_socket(&format!("osm-monitor-budget-{}", std::process::id()))
}

#[test]
fn the_monitor_read_never_starts_a_call_after_its_deadline() {
    let h = LateAndBlank::default();
    let sp = NoSpawn::default();
    let started = Instant::now();
    let outcome = desktop::spawn_and_place(
        &h,
        &sp,
        &no_server(),
        &want(),
        "osm-restore-7",
        // Named, never `auto`: `auto` asks what this machine has installed.
        "ghostty",
        // Long, so that the only budget under test is the monitor read's own.
        Duration::from_secs(30),
    );
    let took = started.elapsed();

    let reads = h.reads.borrow().clone();
    assert!(
        !reads.is_empty(),
        "the monitor list was never read, so this test is not about its budget"
    );
    assert_eq!(
        outcome.as_str(),
        "lost_compositor",
        "a compositor that only ever listed no monitors must be reported lost, \
         not {outcome:?}"
    );

    // The deadline is set when the first read is made, so the first read's own
    // start is the clock every later one has to beat.
    let deadline = reads[0] + MONITOR_BUDGET;
    for (n, read) in reads.iter().enumerate().skip(1) {
        assert!(
            *read < deadline,
            "monitor read {} was started {:?} *after* the {:?} budget had \
             already run out; `call_budget` then granted it a further second, \
             which is how a three-second bound became four and a bit — per \
             restored session, on a boot that walks them in turn",
            n + 1,
            read.saturating_duration_since(deadline),
            MONITOR_BUDGET
        );
    }

    // And the whole of it stayed near the figure the budget names. The slack
    // is for a call already in flight when the deadline passes, which is
    // allowed; a second-long call *started* afterwards is not, and does not
    // fit here.
    assert!(
        took < MONITOR_BUDGET + Duration::from_millis(500),
        "giving up on an unreadable monitor list took {took:?} against a {:?} \
         budget: the flat retry sleep steps over the deadline and the read \
         after it is started anyway",
        MONITOR_BUDGET
    );

    assert!(
        !sp.killed.borrow().is_empty(),
        "a compositor that stopped answering leaves a terminal osm can no \
         longer track; it must be taken back"
    );
}
