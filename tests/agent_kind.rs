use osm::agent::{AgentKind, CONFIDENCE_THRESHOLD};

#[test]
fn kinds_round_trip_through_their_string_form() {
    for k in [AgentKind::Claude, AgentKind::Codex, AgentKind::OpenCode] {
        assert_eq!(AgentKind::parse(k.as_str()), Some(k));
    }
}

#[test]
fn an_unknown_kind_is_rejected_rather_than_defaulted() {
    assert_eq!(AgentKind::parse("aider"), None);
    assert_eq!(AgentKind::parse(""), None);
    assert_eq!(
        AgentKind::parse("CLAUDE"),
        None,
        "matching is exact, not case-folded"
    );
}

#[test]
fn the_registry_returns_only_enabled_adapters_in_config_order() {
    let all = osm::agent::adapters(&[
        "codex".to_string(),
        "claude".to_string(),
        "opencode".to_string(),
    ]);
    let kinds: Vec<&str> = all.iter().map(|a| a.kind().as_str()).collect();
    assert_eq!(kinds, vec!["codex", "claude", "opencode"]);

    let one = osm::agent::adapters(&["claude".to_string()]);
    assert_eq!(one.len(), 1);
    assert_eq!(one[0].kind(), AgentKind::Claude);

    let none = osm::agent::adapters(&[]);
    assert!(none.is_empty(), "no adapters when none are enabled");

    let bogus = osm::agent::adapters(&["aider".to_string()]);
    assert!(bogus.is_empty(), "an unknown name enables nothing");
}

#[test]
fn the_confidence_threshold_is_a_deliberate_value() {
    assert!(
        (0.5..1.0).contains(&CONFIDENCE_THRESHOLD),
        "a threshold outside (0.5, 1.0) either accepts guesses or accepts nothing"
    );
}
