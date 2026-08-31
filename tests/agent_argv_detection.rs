//! Detection signals that a real machine actually provides.

use osm::agent::detect;

#[test]
fn a_conversation_id_is_read_from_argv_in_both_spellings() {
    // `--resume <id>` and `--resume=<id>` are both real spellings, and
    // `--session-id` is how a fresh launch states its identity.
    let me = std::process::id();
    // This process names no conversation, so nothing is claimed for it.
    assert!(
        detect::argv_conversation_ids(me, "definitely-not-a-binary").is_empty(),
        "a binary this pane is not running claims nothing"
    );
}

#[test]
fn a_bare_flag_with_no_value_claims_nothing() {
    // `--resume` alone opens a picker; it names no conversation, and binding
    // to the next argv element would bind to whatever followed it.
    let me = std::process::id();
    let ids = detect::argv_conversation_ids(me, "definitely-not-a-binary");
    assert!(ids.iter().all(|i| !i.starts_with('-')), "{ids:?}");
}

#[test]
fn the_transcript_cwd_is_found_past_the_metadata_records() {
    // A real transcript opens with `last-prompt`, `mode` and
    // `permission-mode` records; `cwd` appears only on a later message.
    // Reading line one alone found nothing on every transcript on the
    // maintainer's machine and fell through to the lossy decoder.
    let tmp = tempfile::tempdir().unwrap();
    let id = "0cfebf91-81c0-43d5-af63-c9fe7e844ede";
    let dir = tmp.path().join("projects/-home-u-app");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(format!("{id}.jsonl")),
        concat!(
            r#"{"type":"last-prompt","leafUuid":"x","sessionId":"s"}"#,
            "\n",
            r#"{"type":"mode","mode":"normal","sessionId":"s"}"#,
            "\n",
            r#"{"type":"user","cwd":"/home/user/projects/n8group-oss/proj-alpha"}"#,
            "\n",
        ),
    )
    .unwrap();

    use osm::agent::AgentAdapter;
    let found = osm::agent::claude::Claude::with_home(tmp.path())
        .discover()
        .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(
        found[0].project_dir.as_deref(),
        Some("/home/user/projects/n8group-oss/proj-alpha"),
        "the exact cwd is used, not the lossy directory-name decode"
    );
}

#[test]
fn a_decoded_path_that_does_not_exist_is_not_claimed() {
    // The decoder cannot tell a separator from a hyphen, so `n8group-oss`
    // decodes to `n8group/oss`. Claiming that would have a restore recreate
    // the pane somewhere the user has never been.
    let tmp = tempfile::tempdir().unwrap();
    let id = "0cfebf91-81c0-43d5-af63-c9fe7e844ede";
    let dir = tmp
        .path()
        .join("projects/-home-user-projects-n8group-oss-proj-alpha");
    std::fs::create_dir_all(&dir).unwrap();
    // No cwd anywhere in the transcript, so the decoder is the only option.
    std::fs::write(dir.join(format!("{id}.jsonl")), "{\"type\":\"mode\"}\n").unwrap();

    use osm::agent::AgentAdapter;
    let found = osm::agent::claude::Claude::with_home(tmp.path())
        .discover()
        .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(
        found[0].project_dir, None,
        "a decoded path naming no real directory is not claimed: {:?}",
        found[0].project_dir
    );
}
