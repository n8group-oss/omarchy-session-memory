//! A pane the restore has just built is not "busy" because tmux has not
//! finished exec'ing its shell.
//!
//! # The defect this test exists for
//!
//! tmux reports a pane's foreground command as the name of its pty's
//! foreground process group, and for a moment after a pane is created that
//! group is **tmux itself**. Probed on tmux 3.7c, a `list-panes` issued
//! immediately after `new-session` reports `tmux` 60 times out of 60 — this
//! is not a rare race, it is the normal state of a pane for its first few
//! milliseconds.
//!
//! `preflight` reads that field to decide whether a pane is an idle shell, and
//! a restore preflights the panes it has only just built. So a restore could
//! refuse its own fresh pane as `pane_busy`, downgrade itself to `partial`,
//! and leave the conversation unresumed — for no reason but asking too early.
//! It showed up as an agent suite that passed thirty times and failed once,
//! which is the shape of a defect that reaches users on the machines where it
//! matters most: a boot restore on a machine that is busy booting.
//!
//! Three things are asserted separately: a fresh pane *is* waited out; a pane
//! that is busy now and idle shortly is waited for, and refused by `preflight`
//! at the moment it is asked; and a pane that is genuinely running something is
//! *not* waited into a false pass.

mod common;

use osm::agent::resume;
use osm::tmux::Tmux;
use std::time::Duration;

struct Server(Tmux);
impl Drop for Server {
    fn drop(&mut self) {
        common::shutdown(&self.0);
    }
}

fn server(label: &str) -> Server {
    Server(Tmux::with_socket(&format!(
        "osm-freshpane-{}-{}",
        label,
        std::process::id()
    )))
}

#[test]
fn a_freshly_created_pane_is_waited_out_rather_than_called_busy() {
    let s = server("fresh");
    s.0.run(&["new-session", "-d", "-s", "dev", "-n", "code", "-c", "/tmp"])
        .unwrap();
    let pane = s.0.list_panes().unwrap()[0].id.clone();

    assert!(
        resume::wait_until_idle(&s.0, &pane, resume::SETTLE_TIMEOUT),
        "a pane whose shell is still starting must be waited for, not refused"
    );

    // And the wait is the *observable* condition, not a duration: the pane
    // really is an idle shell by the time it returns, so `preflight` accepts
    // it.
    let live: Vec<String> =
        s.0.list_panes()
            .unwrap()
            .into_iter()
            .map(|p| p.id)
            .collect();
    let adapter = osm::agent::claude::Claude::with_home(std::path::Path::new("/nonexistent"));
    assert_eq!(
        resume::preflight(
            &s.0,
            &pane,
            &adapter,
            "0cfebf91-81c0-43d5-af63-c9fe7e844b0c",
            &live
        ),
        resume::Outcome::Resumed,
        "every precondition holds once the pane has settled"
    );
}

#[test]
fn a_pane_that_is_genuinely_running_something_is_still_refused() {
    let s = server("busy");
    s.0.run(&["new-session", "-d", "-s", "dev", "-n", "code", "-c", "/tmp"])
        .unwrap();
    let pane = s.0.list_panes().unwrap()[0].id.clone();
    assert!(resume::wait_until_idle(&s.0, &pane, resume::SETTLE_TIMEOUT));

    s.0.run(&["send-keys", "-t", &pane, "sleep 60", "C-m"])
        .unwrap();
    assert_eq!(
        common::wait_for_pane_cmd(&s.0, &pane, "sleep", Duration::from_secs(10)),
        "sleep",
        "the pane is running something"
    );

    // Short budget on purpose: this must not be waited out, and the test must
    // not take the full settle timeout to say so.
    assert!(
        !resume::wait_until_idle(&s.0, &pane, Duration::from_millis(500)),
        "a pane with work in it is busy, and waiting must not turn that into \
         permission to type into it"
    );
    let live: Vec<String> =
        s.0.list_panes()
            .unwrap()
            .into_iter()
            .map(|p| p.id)
            .collect();
    let adapter = osm::agent::claude::Claude::with_home(std::path::Path::new("/nonexistent"));
    assert_eq!(
        resume::preflight(
            &s.0,
            &pane,
            &adapter,
            "0cfebf91-81c0-43d5-af63-c9fe7e844b0d",
            &live
        ),
        resume::Outcome::PaneBusy
    );
}

/// The waiting itself, asserted without depending on how long tmux's own
/// transient lasts: a pane that is busy *now* and will be idle shortly is
/// waited for, and `preflight` refuses it at the moment it is asked.
///
/// The earlier version of this test asserted the transient directly — create a
/// pane, look immediately, expect to see `tmux`. That is true 60 times out of
/// 60 on a loaded machine and not reliably true on an idle one, so it was a
/// test that could fail for a reason that was not a defect. This asserts the
/// behaviour the fix adds instead, with a delay this test controls.
#[test]
fn a_pane_that_is_about_to_become_idle_is_waited_for() {
    let s = server("settling");
    s.0.run(&["new-session", "-d", "-s", "dev", "-n", "code", "-c", "/tmp"])
        .unwrap();
    let pane = s.0.list_panes().unwrap()[0].id.clone();
    assert!(resume::wait_until_idle(&s.0, &pane, resume::SETTLE_TIMEOUT));

    s.0.run(&["send-keys", "-t", &pane, "sleep 1", "C-m"])
        .unwrap();
    assert_eq!(
        common::wait_for_pane_cmd(&s.0, &pane, "sleep", Duration::from_secs(10)),
        "sleep",
        "the pane is busy at the moment the wait begins"
    );

    // Asked right now, `preflight` refuses it — which is what a restore did to
    // its own freshly built panes.
    let live: Vec<String> =
        s.0.list_panes()
            .unwrap()
            .into_iter()
            .map(|p| p.id)
            .collect();
    let adapter = osm::agent::claude::Claude::with_home(std::path::Path::new("/nonexistent"));
    let id = "0cfebf91-81c0-43d5-af63-c9fe7e844b0e";
    assert_eq!(
        resume::preflight(&s.0, &pane, &adapter, id, &live),
        resume::Outcome::PaneBusy,
        "this test's premise"
    );

    // Waited for, it becomes idle and every precondition holds.
    assert!(
        resume::wait_until_idle(&s.0, &pane, resume::SETTLE_TIMEOUT),
        "a pane that is about to be idle must be waited for"
    );
    assert_eq!(
        resume::preflight(&s.0, &pane, &adapter, id, &live),
        resume::Outcome::Resumed
    );
}
