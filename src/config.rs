use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::path::Path;

const VALID_TERMINALS: [&str; 5] = ["auto", "ghostty", "alacritty", "kitty", "foot"];

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RestoreCfg {
    pub auto: bool,
    pub terminal: String,
    pub readiness_timeout_secs: u64,
    /// Whether a restore gives each session its terminal window back.
    ///
    /// The **only** switch under which "there is no compositor" counts as a
    /// finished restore rather than work still owed. Left on, a machine whose
    /// Hyprland is merely slow to start gets waited for and, if it never
    /// arrives, the snapshot stays restorable — because a restore that
    /// silently dropped every window used to retire the only record of where
    /// they were. Turn it off on a headless box, or to have tmux back without
    /// terminals.
    pub place_windows: bool,
}

impl Default for RestoreCfg {
    fn default() -> Self {
        Self {
            auto: true,
            terminal: "auto".to_string(),
            readiness_timeout_secs: 30,
            place_windows: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentsCfg {
    pub auto_resume: bool,
    pub auto_resume_max_age_mins: u64,
    pub enabled: Vec<String>,
}

impl Default for AgentsCfg {
    fn default() -> Self {
        Self {
            auto_resume: true,
            auto_resume_max_age_mins: 30,
            enabled: vec![
                "claude".to_string(),
                "codex".to_string(),
                "opencode".to_string(),
            ],
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CaptureCfg {
    pub debounce_max_latency_secs: u64,
    pub fallback_interval_secs: u64,
    pub keep_snapshots: usize,
}

impl Default for CaptureCfg {
    fn default() -> Self {
        Self {
            debounce_max_latency_secs: 5,
            fallback_interval_secs: 120,
            keep_snapshots: 20,
        }
    }
}

/// What osm may derive a conversation's title from.
///
/// The one place transcript content is read at all, and therefore the one
/// thing there is a switch for.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PrivacyCfg {
    /// Whether a conversation whose agent never named it may be titled from
    /// one truncated line of the user's first message.
    ///
    /// On by default, and that is a decision rather than an oversight: Codex
    /// writes no title of its own and accounts for 2383 of the 2494
    /// conversations on the machine this was built for, so off by default
    /// would mean a menu that says nothing about almost every row — and a
    /// blank goal reads as a session with no purpose. Off, a Claude
    /// conversation keeps whatever its agent called it and every Codex
    /// conversation is *untitled*.
    ///
    /// It replaces `store_summaries`, which promised an opt-in paraphrase of
    /// a conversation's contents and was never implemented. A config that
    /// still sets it is rejected by name (`deny_unknown_fields`), which is
    /// the right outcome: a key that silently does nothing is a promise the
    /// user believes.
    pub prompt_titles: bool,
}

impl Default for PrivacyCfg {
    fn default() -> Self {
        Self {
            prompt_titles: true,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub restore: RestoreCfg,
    pub agents: AgentsCfg,
    pub capture: CaptureCfg,
    pub privacy: PrivacyCfg,
}

impl Config {
    /// What to act on when the configuration could not be read at all.
    ///
    /// Not [`Config::default`], and the difference is one field. A config that
    /// fails to load says nothing about anything, so every other key falls
    /// back to its default — including `agents.enabled`, because a typo must
    /// not read as "stop tracking which pane is running what". Privacy cannot
    /// be treated that way. `privacy.prompt_titles` defaults to `true`, so a
    /// user who wrote `prompt_titles = false` **and** misspelled an unrelated
    /// key three sections away had their explicit refusal silently reversed:
    /// osm went back to reading the first line of their messages, and wrote it
    /// into SQLite.
    ///
    /// Nobody consents by accident. An unreadable config is not permission, so
    /// this one field fails closed. The file's real error is reported by `osm
    /// status --json` either way, which is where the user finds out.
    pub fn strict_fallback() -> Config {
        Config {
            privacy: PrivacyCfg {
                prompt_titles: false,
            },
            ..Config::default()
        }
    }
}

pub fn load(path: &Path) -> Result<Config> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };

    let cfg: Config = toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;

    if !VALID_TERMINALS.contains(&cfg.restore.terminal.as_str()) {
        return Err(anyhow!(
            "invalid restore.terminal {:?}; expected one of {:?}",
            cfg.restore.terminal,
            VALID_TERMINALS
        ));
    }

    if cfg.capture.fallback_interval_secs == 0 {
        return Err(anyhow!(
            "invalid capture.fallback_interval_secs {}; must be greater than 0 \
             (0 would spin the daemon in a tight loop)",
            cfg.capture.fallback_interval_secs
        ));
    }

    if cfg.capture.keep_snapshots == 0 {
        return Err(anyhow!(
            "invalid capture.keep_snapshots {}; must be greater than 0 \
             (0 would prune every snapshot on capture)",
            cfg.capture.keep_snapshots
        ));
    }

    if cfg.capture.debounce_max_latency_secs == 0 {
        return Err(anyhow!(
            "invalid capture.debounce_max_latency_secs {}; must be greater than 0",
            cfg.capture.debounce_max_latency_secs
        ));
    }

    if cfg.agents.auto_resume_max_age_mins == 0 {
        return Err(anyhow!(
            "invalid agents.auto_resume_max_age_mins {}; must be greater than 0",
            cfg.agents.auto_resume_max_age_mins
        ));
    }

    if cfg.restore.readiness_timeout_secs == 0 {
        return Err(anyhow!(
            "invalid restore.readiness_timeout_secs {}; must be greater than 0",
            cfg.restore.readiness_timeout_secs
        ));
    }

    Ok(cfg)
}
