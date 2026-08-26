//! Panic- and hang-safety of the discovery helpers.
//!
//! Both paths take attacker-shaped input in the ordinary course of business:
//! filenames come from disk, and a home directory can contain symlinks the
//! user did not intend.

use osm::agent::{codex::Codex, AgentAdapter};

#[test]
fn a_deeply_nested_session_tree_does_not_blow_the_stack() {
    let tmp = tempfile::tempdir().unwrap();
    let mut deep = tmp.path().join("sessions");
    for _ in 0..600 {
        deep = deep.join("d");
    }
    std::fs::create_dir_all(&deep).unwrap();
    std::fs::write(
        deep.join("rollout-2026-08-24T10-00-00-b70babcd-65cf-4760-b99b-e8fe1d07d290.jsonl"),
        "{}\n",
    )
    .unwrap();

    // Must return rather than recurse without bound.
    let found = Codex::with_home(tmp.path()).discover().unwrap();
    assert!(
        found.is_empty(),
        "a tree past the depth cap yields nothing rather than recursing: {found:?}"
    );
}

#[test]
fn a_symlink_loop_terminates() {
    let tmp = tempfile::tempdir().unwrap();
    let sessions = tmp.path().join("sessions");
    let sub = sessions.join("a");
    std::fs::create_dir_all(&sub).unwrap();
    // a/loop -> sessions  : following this forever is the hazard
    std::os::unix::fs::symlink(&sessions, sub.join("loop")).unwrap();
    std::fs::write(
        sessions.join("rollout-2026-08-24T10-00-00-b70babcd-65cf-4760-b99b-e8fe1d07d290.jsonl"),
        "{}\n",
    )
    .unwrap();

    let found = Codex::with_home(tmp.path()).discover().unwrap();
    assert_eq!(
        found.len(),
        1,
        "the real rollout is found and the loop does not hang"
    );
}

#[test]
fn uuid_extraction_never_panics_on_hostile_filenames() {
    // Byte offsets are used to slice a &str, so a multi-byte character
    // straddling a candidate boundary is the panic risk. It cannot match
    // (a continuation byte is not an ASCII hex digit) but assert it.
    let id = "b70babcd-65cf-4760-b99b-e8fe1d07d290";
    let hostile = [
        String::new(),
        "-".repeat(200),
        "żółć".repeat(50),
        format!("żółć{id}żółć"),
        format!("{id}żółć"),
        "b70babcd-65cf-4760-b99b-e8fe1d07d29".to_string(), // one short
        format!("ż{id}"),
        "\u{1f}".repeat(64),
        format!("rollout-2026-08-24T10-00-00-{id}"),
    ];
    for h in hostile {
        // The point is that this returns rather than panicking.
        let _ = osm::agent::codex::rollout_id_for_test(&h);
    }

    assert_eq!(
        osm::agent::codex::rollout_id_for_test(&format!("rollout-x-{id}")),
        Some(id.to_string())
    );
}
