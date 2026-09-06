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

// ---------------------------------------------------------------------------
// Nothing outside the configured store may become a conversation, or a title.
//
// Claude discovery walked `~/.claude/projects/*/` and accepted any entry whose
// stem was a UUID and whose extension was `.jsonl`, without ever asking what
// the entry *was* or where it led. A symlink named after a UUID — or a project
// directory that is a symlink — therefore made any JSONL-shaped file anywhere
// on the machine into a conversation osm would list, bind a pane to, and read
// a title out of and persist. The Codex walker had refused symlinks since it
// was written (`tests/agent_hardening.rs::a_symlink_loop_terminates`); the
// Claude one never did.
//
// The legitimate case has to keep working, and it is the reason the check is
// containment rather than "no symlinks anywhere on the path": `~/.claude ->
// /data/claude` is an ordinary arrangement — a home on a bigger filesystem, or
// a dotfiles checkout — and `tests/agent_symlinked_home.rs` exists for it. So
// the configured root is resolved once, and everything found under it is
// required to resolve to somewhere inside that resolved root.
// ---------------------------------------------------------------------------

use osm::agent::claude::Claude;
use osm::agent::title::Policy;

const CLAUDE_ID: &str = "0cfebf91-81c0-43d5-af63-c9fe7e844d30";

/// A JSONL file that is not osm's business, somewhere outside any store, with
/// a title in it that osm must never show.
fn outside_transcript(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("elsewhere/notes.jsonl");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        &path,
        format!(
            "{{\"cwd\":\"/etc\",\"sessionId\":\"{CLAUDE_ID}\"}}\n\
             {{\"type\":\"ai-title\",\"aiTitle\":\"Content from outside the store\",\
               \"sessionId\":\"{CLAUDE_ID}\"}}\n"
        ),
    )
    .unwrap();
    path
}

#[test]
fn a_uuid_named_symlink_out_of_the_store_is_not_a_conversation() {
    let tmp = tempfile::tempdir().unwrap();
    let outside = outside_transcript(tmp.path());
    let home = tmp.path().join("claude");
    let project = home.join("projects/-tmp");
    std::fs::create_dir_all(&project).unwrap();
    // Named exactly as a transcript is named, and pointing out of the store.
    std::os::unix::fs::symlink(&outside, project.join(format!("{CLAUDE_ID}.jsonl"))).unwrap();

    let found = Claude::with_home(&home).discover().unwrap();
    assert!(
        found.is_empty(),
        "a UUID-named symlink pointing out of the configured store was \
         accepted as a conversation, so anything JSONL-shaped anywhere on the \
         machine can be listed, bound to a pane and read for a title: {found:?}"
    );
}

#[test]
fn a_project_directory_that_leaves_the_store_contributes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let outside = outside_transcript(tmp.path());
    let home = tmp.path().join("claude");
    std::fs::create_dir_all(home.join("projects")).unwrap();
    // The file inside is an ordinary regular file; it is the directory that
    // leaves the store, which a per-file symlink check alone would miss.
    std::os::unix::fs::symlink(outside.parent().unwrap(), home.join("projects/-escaped")).unwrap();
    std::fs::rename(
        &outside,
        outside.with_file_name(format!("{CLAUDE_ID}.jsonl")),
    )
    .unwrap();

    let found = Claude::with_home(&home).discover().unwrap();
    assert!(
        found.is_empty(),
        "a project directory that is a symlink out of the store handed osm \
         every JSONL file behind it: {found:?}"
    );
}

/// The arrangement the containment check must not break.
///
/// `~/.claude -> /data/claude` is ordinary, and the whole store is then
/// reached through a symlink. Resolving the *root* once and comparing against
/// the resolved root is what tells this apart from the two cases above.
#[test]
fn a_store_reached_through_a_symlinked_home_is_still_discovered() {
    let tmp = tempfile::tempdir().unwrap();
    let real = tmp.path().join("data-claude");
    let project = real.join("projects/-tmp");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join(format!("{CLAUDE_ID}.jsonl")),
        format!("{{\"cwd\":\"/tmp\",\"sessionId\":\"{CLAUDE_ID}\"}}\n"),
    )
    .unwrap();
    let link = tmp.path().join("claude");
    std::os::unix::fs::symlink(&real, &link).unwrap();

    let found = Claude::with_home(&link).discover().unwrap();
    assert_eq!(
        found.len(),
        1,
        "a home that is itself a symlink is a legitimate arrangement and must \
         still be read: {found:?}"
    );
    assert_eq!(found[0].native_id, CLAUDE_ID);
    assert!(
        found[0]
            .store_path
            .as_deref()
            .is_some_and(|p| p.starts_with(link.to_str().unwrap())),
        "the path osm records is the one it was told about, not a resolved \
         one: a `/proc/<pid>/fd` comparison and a restore both speak in the \
         path the user's agent opens — see tests/agent_symlinked_home.rs: {found:?}"
    );
}

/// And the read itself does not follow one either.
///
/// Discovery refusing a symlink is a check made at one moment. The file is
/// opened later, by name, and between the two the name can come to mean
/// something else — a transcript deleted and replaced by a link is an ordinary
/// race with an agent, and a deliberate one is a swap. A title is persisted,
/// so a single successful read is permanent.
#[test]
fn a_transcript_that_became_a_symlink_yields_no_title() {
    let tmp = tempfile::tempdir().unwrap();
    let outside = outside_transcript(tmp.path());
    let home = tmp.path().join("claude");
    let project = home.join("projects/-tmp");
    std::fs::create_dir_all(&project).unwrap();
    let transcript = project.join(format!("{CLAUDE_ID}.jsonl"));
    std::fs::write(
        &transcript,
        format!(
            "{{\"cwd\":\"/tmp\",\"sessionId\":\"{CLAUDE_ID}\"}}\n\
             {{\"type\":\"ai-title\",\"aiTitle\":\"The real one\",\
               \"sessionId\":\"{CLAUDE_ID}\"}}\n"
        ),
    )
    .unwrap();

    let adapter = Claude::with_home(&home);
    let session = adapter.discover().unwrap().pop().expect("one transcript");
    assert_eq!(
        adapter
            .title_of(&session, Policy::AgentOrFirstPrompt)
            .map(|t| t.text),
        Some("The real one".to_string()),
        "the fixture has to work before its replacement can be shown not to"
    );

    // The name now points somewhere else entirely.
    std::fs::remove_file(&transcript).unwrap();
    std::os::unix::fs::symlink(&outside, &transcript).unwrap();

    assert_eq!(
        adapter.title_of(&session, Policy::AgentOrFirstPrompt),
        None,
        "the title read followed a symlink out of the store, so content from \
         outside it became this conversation's persisted title"
    );
}
