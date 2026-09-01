//! The one decision in this project that ends a process the user can see.
//!
//! A placement that cannot prove its terminal attached takes that terminal
//! back, because leaving it means an untracked empty window and another one
//! beside it on every retry. The whole safety of that rests on what
//! "cannot prove" is allowed to mean, so the verdict is driven here directly:
//! no server, no client, no process — and therefore nothing this file can
//! kill by mistake, on a machine whose real tmux sessions are somebody's
//! work.
//!
//! Ownership still has to be real, since it is proven from `/proc`: the
//! "owned" client in every case below is this test process itself, and the
//! stranger's client is pid 1.

use osm::desktop::{self, Attach, ClientPid, Spawned};
use std::time::{Duration, Instant};

/// The process the placement pass would have started — here, this one.
fn ours() -> Spawned {
    Spawned::of(std::process::id()).expect("this process exists")
}

/// A client running inside the terminal we started.
fn owned(session: &str) -> ClientPid {
    ClientPid {
        pid: std::process::id(),
        session: session.to_string(),
    }
}

/// A client of the user's own terminal: attached, and none of our business.
fn strangers(session: &str) -> ClientPid {
    ClientPid {
        pid: 1,
        session: session.to_string(),
    }
}

/// A deadline far enough out that a test asserting on a single poll never
/// races it, and short enough that one that runs the clock out costs little.
fn in_a_moment() -> Instant {
    Instant::now() + Duration::from_millis(200)
}

// ---------------------------------------------------------------------------
// What one answer establishes.
// ---------------------------------------------------------------------------

#[test]
fn a_client_of_ours_on_the_session_is_the_terminal_we_placed() {
    assert_eq!(
        desktop::verdict_of(Some(&[owned("dev")]), &ours(), "dev"),
        Attach::Yes
    );
}

#[test]
fn no_client_of_ours_anywhere_is_the_one_answer_a_terminal_may_be_killed_on() {
    // The control for everything below: this is the only verdict that ends a
    // process, and it must still be reachable.
    assert_eq!(
        desktop::verdict_of(Some(&[]), &ours(), "dev"),
        Attach::No,
        "a readable empty client list is what `never attached` means"
    );
    assert_eq!(
        desktop::verdict_of(Some(&[strangers("dev")]), &ours(), "dev"),
        Attach::No,
        "the user's own terminal holding the session is not proof we placed one"
    );
}

#[test]
fn a_client_of_ours_on_another_session_is_not_an_absent_one() {
    // The finding. `Attach::No` used to mean "no owned client is attached to
    // the session we asked about", which is not the same statement as "no
    // client of ours is attached at all": a `client-attached` hook that
    // switches the spawned client to another session leaves a terminal of
    // ours alive, showing the user that session — and the combined
    // session-and-ownership predicate missed it, so the terminal the user was
    // looking at was killed at the timeout.
    assert_eq!(
        desktop::verdict_of(Some(&[owned("notes")]), &ours(), "dev"),
        Attach::Unknown,
        "a terminal of ours is attached to something and must never be killed"
    );
}

#[test]
fn an_answer_that_never_came_establishes_nothing() {
    assert_eq!(desktop::verdict_of(None, &ours(), "dev"), Attach::Unknown);
}

// ---------------------------------------------------------------------------
// What the poll loop establishes before its deadline.
// ---------------------------------------------------------------------------

#[test]
fn a_client_that_attaches_during_the_budget_ends_the_wait() {
    let me = ours();
    let mut calls = 0;
    let mut poll = || {
        calls += 1;
        if calls >= 3 {
            Some(vec![owned("dev")])
        } else {
            Some(vec![])
        }
    };
    assert_eq!(
        desktop::attach_verdict(
            &me,
            "dev",
            Instant::now() + Duration::from_secs(5),
            &mut poll
        ),
        Attach::Yes
    );
    assert_eq!(calls, 3, "the loop kept polling after it had its answer");
}

#[test]
fn a_server_that_never_answers_leaves_the_terminal_alone() {
    let me = ours();
    let mut poll = || None;
    assert_eq!(
        desktop::attach_verdict(&me, "dev", in_a_moment(), &mut poll),
        Attach::Unknown,
        "a server that stopped answering says nothing about what is on screen"
    );
}

#[test]
fn a_client_of_ours_on_another_session_survives_the_deadline() {
    // The same finding, through the loop that acts on it: the budget runs out
    // and the terminal is still ours, still attached, still showing the user
    // a session. Running out of time does not turn that into "no".
    let me = ours();
    let mut poll = || Some(vec![owned("notes")]);
    assert_eq!(
        desktop::attach_verdict(&me, "dev", in_a_moment(), &mut poll),
        Attach::Unknown,
        "the deadline was allowed to authorise killing a live terminal of ours"
    );
}

// ---------------------------------------------------------------------------
// The verdict is re-asked immediately before the terminal is taken back.
//
// The poll that runs the clock out is not the answer the kill acts on: a
// client that attached while that poll was in flight — the ordinary boundary,
// since the attach and the deadline are unrelated clocks — was attached
// before anything was killed, and the process must be able to say so. Both
// cases below count polls taken at or after the deadline, so the moment they
// describe is exact rather than a wall-clock guess: poll one is the verdict,
// poll two is the answer the kill is taken on.
// ---------------------------------------------------------------------------

#[test]
fn an_attach_that_lands_at_the_deadline_is_not_killed_for_being_late() {
    let me = ours();
    let deadline = Instant::now() + Duration::from_millis(200);
    let mut after_deadline = 0;
    let mut poll = || {
        if Instant::now() >= deadline {
            after_deadline += 1;
        }
        if after_deadline >= 2 {
            Some(vec![owned("dev")])
        } else {
            Some(vec![])
        }
    };
    assert_eq!(
        desktop::attach_verdict(&me, "dev", deadline, &mut poll),
        Attach::Yes,
        "a client attached in the moment the deadline passed had its terminal \
         killed, because the poll that ran the clock out was never re-asked"
    );
}

#[test]
fn a_final_answer_that_never_came_leaves_the_terminal_alone() {
    // The server was answering right up to the deadline and stops at the one
    // moment it matters. "It answered a moment ago" is not a licence to kill:
    // what is on the user's screen now is exactly what nobody can see.
    let me = ours();
    let deadline = Instant::now() + Duration::from_millis(200);
    let mut after_deadline = 0;
    let mut poll = || {
        if Instant::now() >= deadline {
            after_deadline += 1;
        }
        if after_deadline >= 2 {
            None
        } else {
            Some(vec![])
        }
    };
    assert_eq!(
        desktop::attach_verdict(&me, "dev", deadline, &mut poll),
        Attach::Unknown,
        "a terminal was killed on an answer nobody re-read"
    );
}

#[test]
fn a_terminal_that_really_never_attached_is_still_taken_back() {
    // The control for the two above: re-asking must not become "never kill".
    // A terminal that holds nothing, twice over, is still the empty window
    // this pass exists to take back.
    let me = ours();
    let mut calls = 0;
    let mut poll = || {
        calls += 1;
        Some(vec![])
    };
    assert_eq!(
        desktop::attach_verdict(&me, "dev", in_a_moment(), &mut poll),
        Attach::No
    );
    assert!(
        calls >= 2,
        "the kill was authorised by a single poll: {calls}"
    );
}
