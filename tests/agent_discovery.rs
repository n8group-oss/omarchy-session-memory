use osm::agent::{AgentAdapter, AgentKind};
use std::fs;

fn write(path: &std::path::Path, bytes: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

#[test]
fn claude_finds_conversations_and_reads_their_identity_from_the_filename() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let id = "0cfebf91-81c0-43d5-af63-c9fe7e844ede";
    write(
        &root
            .join("projects/-home-u-app")
            .join(format!("{id}.jsonl")),
        "{\"cwd\":\"/home/u/app\"}\n",
    );
    // A file that is not a transcript must be ignored, not misread.
    write(&root.join("projects/-home-u-app/notes.txt"), "hello\n");

    let a = osm::agent::claude::Claude::with_home(root);
    let found = a.discover().unwrap();

    assert_eq!(found.len(), 1, "one transcript, one session: {found:?}");
    assert_eq!(found[0].kind, AgentKind::Claude);
    assert_eq!(found[0].native_id, id);
    assert!(found[0].store_path.as_ref().unwrap().ends_with(".jsonl"));
    assert!(found[0].size_bytes.unwrap() > 0);
}

#[test]
fn claude_reads_project_dir_from_the_transcripts_cwd_line() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let id = "0cfebf91-81c0-43d5-af63-c9fe7e844ede";
    write(
        &root
            .join("projects/-home-u-app")
            .join(format!("{id}.jsonl")),
        "{\"cwd\":\"/home/u/app\"}\n{\"more\":\"lines\"}\n",
    );

    let found = osm::agent::claude::Claude::with_home(root)
        .discover()
        .unwrap();

    assert_eq!(found.len(), 1);
    assert_eq!(found[0].project_dir.as_deref(), Some("/home/u/app"));
}

#[test]
fn claude_falls_back_to_the_encoded_directory_name_when_the_first_line_has_no_cwd() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let id = "0cfebf91-81c0-43d5-af63-c9fe7e844ede";
    write(
        &root
            .join("projects/-home-u-app")
            .join(format!("{id}.jsonl")),
        "not json at all\n",
    );

    let found = osm::agent::claude::Claude::with_home(root)
        .discover()
        .unwrap();

    assert_eq!(found.len(), 1);
    assert_eq!(
        found[0].project_dir.as_deref(),
        Some("/home/u/app"),
        "unparseable first line falls back to decoding the directory name, not erroring: {:?}",
        found[0]
    );
}

#[test]
fn claude_project_dir_never_reads_past_the_capped_prefix() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let id = "0cfebf91-81c0-43d5-af63-c9fe7e844ede";
    // A first "line" far larger than any sane cap, and no newline at all —
    // the pathological case a bounded read must survive without reading the
    // whole file.
    let huge_first_line = "x".repeat(64 * 1024 * 1024);
    write(
        &root
            .join("projects/-home-u-app")
            .join(format!("{id}.jsonl")),
        &huge_first_line,
    );

    let found = osm::agent::claude::Claude::with_home(root)
        .discover()
        .unwrap();

    assert_eq!(found.len(), 1);
    // Truncated mid-token JSON never parses, so this falls back to the
    // directory-name decode rather than erroring or hanging.
    assert_eq!(found[0].project_dir.as_deref(), Some("/home/u/app"));
}

#[test]
fn codex_reads_project_dir_from_the_rollouts_cwd_line_with_no_fallback() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let id = "b70babcd-65cf-4760-b99b-e8fe1d07d290";
    write(
        &root
            .join("sessions/2026/08/24")
            .join(format!("rollout-2026-08-24T10-00-00-{id}.jsonl")),
        "{\"cwd\":\"/home/u/app\"}\n",
    );

    let found = osm::agent::codex::Codex::with_home(root)
        .discover()
        .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].project_dir.as_deref(), Some("/home/u/app"));

    // A rollout filename carries no project-path encoding (unlike
    // Claude's), so with no cwd on the first line there is nothing to fall
    // back to: None, not an error.
    let id2 = "c81cacde-76d0-5871-c00c-f9f2e18e3a01";
    write(
        &root
            .join("sessions/2026/08/24")
            .join(format!("rollout-2026-08-24T11-00-00-{id2}.jsonl")),
        "not json at all\n",
    );
    let found = osm::agent::codex::Codex::with_home(root)
        .discover()
        .unwrap();
    let no_cwd = found.iter().find(|s| s.native_id == id2).unwrap();
    assert_eq!(no_cwd.project_dir, None);
}

#[test]
fn codex_finds_rollouts_and_extracts_the_uuid_from_the_filename() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let id = "b70babcd-65cf-4760-b99b-e8fe1d07d290";
    write(
        &root
            .join("sessions/2026/08/24")
            .join(format!("rollout-2026-08-24T10-00-00-{id}.jsonl")),
        "{}\n",
    );

    let a = osm::agent::codex::Codex::with_home(root);
    let found = a.discover().unwrap();

    assert_eq!(found.len(), 1);
    assert_eq!(found[0].kind, AgentKind::Codex);
    assert_eq!(found[0].native_id, id);
}

#[test]
fn a_missing_home_is_no_sessions_rather_than_an_error() {
    let tmp = tempfile::tempdir().unwrap();
    let absent = tmp.path().join("nope");
    assert!(osm::agent::claude::Claude::with_home(&absent)
        .discover()
        .unwrap()
        .is_empty());
    assert!(osm::agent::codex::Codex::with_home(&absent)
        .discover()
        .unwrap()
        .is_empty());
}

#[test]
fn resume_argv_uses_the_resume_form_never_the_create_form() {
    // `--session-id` creates a conversation with that id and fails with
    // "Session ID is already in use" for one that exists. Restoring is
    // exactly that case, so the create form must never appear.
    let c = osm::agent::claude::Claude::new();
    let argv = c.resume_argv("abc");
    assert!(argv.iter().any(|a| a == "--resume"), "{argv:?}");
    assert!(!argv.iter().any(|a| a == "--session-id"), "{argv:?}");
    assert_eq!(argv.last().unwrap(), "abc");

    let x = osm::agent::codex::Codex::new();
    let argv = x.resume_argv("abc");
    assert!(argv.iter().any(|a| a == "resume"), "{argv:?}");
    assert_eq!(argv.last().unwrap(), "abc");
}

/// A transcript is recognised because it **is** one of the files discovery
/// found — same device, same inode — never because its path looks like one.
///
/// The rule this replaced asked each adapter whether a path "looked like" one
/// of its transcripts, so any `.jsonl` whose stem happened to be a UUID
/// counted, wherever it was and whoever wrote it. A pane holding
/// `/tmp/<uuid>.jsonl` open therefore scored as though it were running that
/// conversation.
#[test]
fn a_transcript_is_matched_by_file_identity_not_by_a_lookalike_path() {
    let tmp = tempfile::tempdir().unwrap();
    let id = "0cfebf91-81c0-43d5-af63-c9fe7e844ede";
    let real = tmp.path().join("projects/-tmp").join(format!("{id}.jsonl"));
    std::fs::create_dir_all(real.parent().unwrap()).unwrap();
    std::fs::write(&real, "{\"cwd\":\"/tmp\"}\n").unwrap();

    let index = osm::agent::claude::Claude::with_home(tmp.path())
        .transcript_index()
        .unwrap();
    assert_eq!(index.len(), 1);
    assert_eq!(index.id_of(&real), Some(id));

    // Same name, same shape, different file: not this conversation, and not
    // any conversation.
    let lookalike = tmp.path().join(format!("{id}.jsonl"));
    std::fs::write(&lookalike, "{\"cwd\":\"/tmp\"}\n").unwrap();
    assert_eq!(
        index.id_of(&lookalike),
        None,
        "a file that merely has a transcript's name is not a transcript"
    );

    // A second path to the *same* file is the same conversation, which is the
    // half a path comparison gets wrong the other way: a symlinked home or a
    // bind mount is still the user's transcript.
    let link = tmp.path().join("via-symlink.jsonl");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    assert_eq!(
        index.id_of(&link),
        Some(id),
        "another name for the same file is the same conversation"
    );

    // And an adapter with no store path per conversation indexes nothing, so
    // it can match nothing.
    let opencode = osm::agent::opencode::OpenCode::with_binary("/nonexistent/opencode");
    assert!(opencode.transcript_index().unwrap().is_empty());
    assert!(opencode.auto_unsupported_reason().is_some());
}

// --- "cannot look" is not "nothing there" ----------------------------------
//
// A missing store is no conversations and legitimate; a store that is there
// and cannot be read is osm being unable to look. They used to be the same
// empty list, which is how a permissions problem could report "you have no
// conversations" and a restore could put back bare shells and call itself a
// success.
//
// The unreadable case is staged by putting a *file* where the directory
// belongs, so `read_dir` fails with ENOTDIR. `chmod 000` would not do: CI runs
// as root, where mode bits are advisory, and the test would quietly pass by
// succeeding rather than by failing correctly.

#[test]
fn claude_reports_a_projects_directory_it_cannot_read_rather_than_no_sessions() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::write(root.join("projects"), "not a directory\n").unwrap();

    let err = osm::agent::claude::Claude::with_home(root)
        .discover()
        .expect_err("an unreadable store is a failure, not an empty inventory");
    assert!(
        format!("{err:#}").contains("projects"),
        "the error names what could not be read: {err:#}"
    );
}

#[test]
fn codex_reports_a_sessions_path_it_cannot_read_rather_than_no_sessions() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("sessions"), "not a directory\n").unwrap();

    let err = osm::agent::codex::Codex::with_home(tmp.path())
        .discover()
        .expect_err("an unreadable store is a failure, not an empty inventory");
    assert!(
        format!("{err:#}").contains("sessions"),
        "the error names what could not be read: {err:#}"
    );
}
