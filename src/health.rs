//! Persisted capture health.
//!
//! Everything that could tell a user "captures stopped working" used to be
//! thrown away: the tmux hooks send all output to `/dev/null`, the daemon
//! discarded every capture result, and `status --json` reported `ready: true`
//! whenever the config merely parsed. A capture that failed on every event
//! left the service looking healthy while the newest snapshot aged out — the
//! exact shape of the linked-window bug, which broke every capture on the
//! server and was noticed only by accident.
//!
//! So each capture path records what happened here, next to the database, and
//! `osm status --json` reads it back.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Freshness budget when the configured fallback interval is unusable or very
/// small: a capture older than this means something is wrong even on an idle
/// machine, because the daemon's timer alone should have produced one.
const MIN_STALE_AFTER_SECS: u64 = 60;

/// How many fallback-timer intervals may pass with no successful capture
/// before the state is called stale. Three, so a single missed tick — a
/// capture that lost the lock race, a busy machine — is not an alarm.
const STALE_AFTER_INTERVALS: u64 = 3;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CaptureHealth {
    /// Epoch seconds of the last capture that actually wrote a snapshot.
    pub last_success_at: Option<i64>,
    pub last_error_at: Option<i64>,
    /// The most recent capture error, full cause chain.
    pub last_error: Option<String>,
    /// Failures since the last success. Non-zero means captures are failing
    /// *now*, which a merely old `last_success_at` cannot distinguish from an
    /// idle machine.
    pub consecutive_failures: u32,
}

pub fn path(state_dir: &Path) -> PathBuf {
    state_dir.join("capture-health.json")
}

/// A missing, unreadable or unparsable file reads as "nothing recorded yet" —
/// never as an error. Health reporting must not be able to break the thing it
/// reports on.
pub fn load(path: &Path) -> CaptureHealth {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn store(path: &Path, health: &CaptureHealth) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string(health)?)
        .with_context(|| format!("write {}", path.display()))?;
    // 0600, matching the database and the last-capture stamp: same state
    // directory, same class of local operational data.
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    Ok(())
}

/// Record a capture that wrote a snapshot, clearing the failure streak.
pub fn record_success(path: &Path, now: i64) -> Result<()> {
    let mut health = load(path);
    health.last_success_at = Some(now);
    health.consecutive_failures = 0;
    store(path, &health)
}

/// Record a capture that failed. Returns the new consecutive-failure count,
/// which is what the daemon uses to decide when to make itself visible to
/// systemd.
pub fn record_failure(path: &Path, now: i64, error: &str) -> Result<u32> {
    let mut health = load(path);
    health.last_error_at = Some(now);
    health.last_error = Some(error.to_string());
    health.consecutive_failures = health.consecutive_failures.saturating_add(1);
    let count = health.consecutive_failures;
    store(path, &health)?;
    Ok(count)
}

/// How old a successful capture may be before the state is called stale.
pub fn stale_after_secs(fallback_interval_secs: u64) -> u64 {
    fallback_interval_secs
        .saturating_mul(STALE_AFTER_INTERVALS)
        .max(MIN_STALE_AFTER_SECS)
}

/// `(age_secs, stale)` for a health record at time `now`.
///
/// Never having captured at all counts as stale: a service that has produced
/// nothing has nothing to restore from, which is exactly the condition worth
/// showing.
pub fn freshness(health: &CaptureHealth, now: i64, stale_after: u64) -> (Option<i64>, bool) {
    match health.last_success_at {
        None => (None, true),
        Some(at) => {
            let age = now.saturating_sub(at).max(0);
            (Some(age), age as u64 > stale_after)
        }
    }
}
