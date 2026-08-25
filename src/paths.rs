use anyhow::{Context, Result};
use std::path::PathBuf;

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}

pub fn state_dir() -> Result<PathBuf> {
    match std::env::var_os("XDG_STATE_HOME") {
        Some(v) if !v.is_empty() => Ok(PathBuf::from(v).join("osm")),
        _ => Ok(home()?.join(".local/state/osm")),
    }
}

pub fn db_path() -> Result<PathBuf> {
    Ok(state_dir()?.join("state.db"))
}

pub fn config_path() -> Result<PathBuf> {
    match std::env::var_os("XDG_CONFIG_HOME") {
        Some(v) if !v.is_empty() => Ok(PathBuf::from(v).join("osm/config.toml")),
        _ => Ok(home()?.join(".config/osm/config.toml")),
    }
}
