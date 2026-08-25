use osm::config;

#[test]
fn missing_file_yields_defaults() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = config::load(&tmp.path().join("absent.toml")).unwrap();
    assert!(cfg.restore.auto);
    assert_eq!(cfg.restore.terminal, "auto");
    assert_eq!(cfg.restore.readiness_timeout_secs, 30);
    assert!(cfg.agents.auto_resume);
    assert_eq!(cfg.agents.auto_resume_max_age_mins, 30);
    assert_eq!(cfg.agents.enabled, vec!["claude", "codex", "opencode"]);
    assert_eq!(cfg.capture.debounce_max_latency_secs, 5);
    assert_eq!(cfg.capture.fallback_interval_secs, 120);
    assert_eq!(cfg.capture.keep_snapshots, 20);
    assert!(!cfg.privacy.store_summaries);
}

#[test]
fn partial_file_overrides_only_named_keys() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("config.toml");
    std::fs::write(
        &path,
        "[restore]\nterminal = \"kitty\"\n\n[capture]\nkeep_snapshots = 5\n",
    )
    .unwrap();

    let cfg = config::load(&path).unwrap();
    assert_eq!(cfg.restore.terminal, "kitty");
    assert!(cfg.restore.auto, "unspecified keys keep their defaults");
    assert_eq!(cfg.capture.keep_snapshots, 5);
    assert_eq!(cfg.capture.fallback_interval_secs, 120);
}

#[test]
fn invalid_toml_is_a_readable_error() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("config.toml");
    std::fs::write(&path, "[restore\nauto = true\n").unwrap();
    let err = config::load(&path).unwrap_err();
    assert!(err.to_string().contains("config.toml"));
}

#[test]
fn unknown_terminal_value_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("config.toml");
    std::fs::write(&path, "[restore]\nterminal = \"emacs\"\n").unwrap();
    let err = config::load(&path).unwrap_err();
    assert!(err.to_string().contains("terminal"));
}

#[test]
fn zero_valued_intervals_are_rejected() {
    let cases: &[(&str, &str)] = &[
        (
            "capture.fallback_interval_secs",
            "[capture]\nfallback_interval_secs = 0\n",
        ),
        ("capture.keep_snapshots", "[capture]\nkeep_snapshots = 0\n"),
        (
            "capture.debounce_max_latency_secs",
            "[capture]\ndebounce_max_latency_secs = 0\n",
        ),
        (
            "agents.auto_resume_max_age_mins",
            "[agents]\nauto_resume_max_age_mins = 0\n",
        ),
        (
            "restore.readiness_timeout_secs",
            "[restore]\nreadiness_timeout_secs = 0\n",
        ),
    ];

    for (key, toml) in cases {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, toml).unwrap();
        let result = config::load(&path);
        let err = match result {
            Ok(_) => panic!("expected {key}=0 to be rejected, but it loaded successfully"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains(key),
            "error for {key}=0 should name the offending key, got: {err}"
        );
    }
}
