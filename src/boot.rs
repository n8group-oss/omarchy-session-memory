use anyhow::{Context, Result};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn current_boot_id() -> Result<String> {
    let raw = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .context("read /proc/sys/kernel/random/boot_id")?;
    Ok(raw.trim().to_string())
}

pub fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
