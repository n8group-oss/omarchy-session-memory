//! The predicate that decides whether a restore's agent outcomes count as
//! work left undone.
//!
//! These pin the *classification* — which outcomes mean "a pane that should
//! hold a conversation is a bare shell" — and nothing more. On their own they
//! cannot fail for the defect that matters: deleting the downgrade in
//! `run_restore` that consults this predicate leaves every one of them green.
//! What a failed resume does to the restore's state, to the snapshot's fate
//! and to the pane is asserted end to end in
//! `tests/agent_restore_failed_resume.rs`.

use osm::agent::resume::Outcome;

#[test]
fn a_failed_resume_counts_as_work_the_restore_did_not_do() {
    // A pane left as a bare shell is work the restore did not do. Reporting
    // success would retire the only snapshot that knows the conversation.
    let outcomes = vec![
        ("%1".to_string(), Outcome::Resumed),
        ("%2".to_string(), Outcome::Failed("no start".into())),
    ];
    assert!(osm::restore::agent_outcomes_are_degraded(&outcomes));
}

#[test]
fn all_resumed_is_not_degraded() {
    let outcomes = vec![("%1".to_string(), Outcome::Resumed)];
    assert!(!osm::restore::agent_outcomes_are_degraded(&outcomes));
}

#[test]
fn active_elsewhere_is_reported_but_not_a_failure() {
    // The conversation is alive in another pane. Leaving this one as a shell
    // is correct, and forcing a second attach would be harmful.
    let outcomes = vec![("%1".to_string(), Outcome::ActiveElsewhere)];
    assert!(!osm::restore::agent_outcomes_are_degraded(&outcomes));
}

#[test]
fn no_agents_at_all_is_not_degraded() {
    assert!(!osm::restore::agent_outcomes_are_degraded(&[]));
}

/// An agent osm was never able to act on is not a failure of the restore:
/// nothing went wrong, and there was never anything it could have done. It is
/// still *reported*, which is a different thing from being counted.
#[test]
fn unsupported_is_reported_but_not_a_failure() {
    let outcomes = vec![("%1".to_string(), Outcome::Unsupported)];
    assert!(!osm::restore::agent_outcomes_are_degraded(&outcomes));
}
