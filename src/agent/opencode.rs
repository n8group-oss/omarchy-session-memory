//! OpenCode adapter, driven only through its public CLI.
//!
//! OpenCode's on-disk store has changed shape across versions and is not a
//! stable interface, so discovery goes through `opencode session list
//! --format json` rather than reading files directly. The user's existing
//! bash tooling deliberately does not track OpenCode at all for the same
//! reason.
//!
//! # Automatic capture and resume are unsupported, and say so
//!
//! Everything osm does automatically rests on two questions it must be able to
//! answer from the machine: *which conversation is this pane running*, and
//! *does anything else have this conversation open*. OpenCode's public CLI
//! answers neither, and this adapter is deliberately confined to that CLI.
//!
//! Pretending otherwise was worse than either answer. Binding scored OpenCode
//! at most 0.4 against a 0.75 threshold — it owns no transcript, so the 0.5
//! signal was unreachable — which meant no pane could ever be bound to an
//! OpenCode conversation and its entire capture path was dead code that
//! nothing reported. Meanwhile `is_active_elsewhere` returned a bare `false`,
//! which callers read as "verified not active", so `osm resume ses_x` would
//! happily attach a second client to a conversation already running in another
//! pane. Unknown failed *open* in the one place it must fail closed.
//!
//! So [`AgentAdapter::auto_unsupported_reason`] says no, once, and every
//! consequence follows from it: nothing is bound, nothing is auto-resumed,
//! `osm resume` refuses with [`crate::agent::resume::Outcome::Unsupported`],
//! `osm status --json` lists the kind under `agents.unsupported`, and the
//! README says the same. Discovery still works, so `osm agents --json` lists
//! OpenCode conversations for a human to open by hand.

use super::{AgentAdapter, AgentKind, AgentSession};
use anyhow::Result;
use std::process::Command;

pub struct OpenCode {
    binary: String,
}

impl OpenCode {
    /// Relies on `opencode` being on `PATH`.
    pub fn new() -> Self {
        Self {
            binary: "opencode".to_string(),
        }
    }

    /// For tests: address a stub binary directly.
    pub fn with_binary(binary: &str) -> Self {
        Self {
            binary: binary.to_string(),
        }
    }
}

impl Default for OpenCode {
    fn default() -> Self {
        Self::new()
    }
}

/// Some version managers (mise, asdf, ...) print an activation banner on
/// stdout before a wrapped command's own output. Strip anything before the
/// first `[` or `{` so that banner doesn't break JSON parsing.
fn strip_banner(raw: &str) -> &str {
    match raw.find(['[', '{']) {
        Some(idx) => &raw[idx..],
        None => raw,
    }
}

impl AgentAdapter for OpenCode {
    fn kind(&self) -> AgentKind {
        AgentKind::OpenCode
    }

    /// # Absent is not the same as failed
    ///
    /// OpenCode not being installed is an ordinary, legitimate state and means
    /// "no conversations". Everything else — the binary is there and exits
    /// non-zero, or prints something that is not the JSON it promises — is a
    /// failure to *look*, and used to produce the same empty list. A caller
    /// then reported "nothing to resume" for a machine whose conversations osm
    /// simply could not read, and a restore that put back bare shells could
    /// call itself a success.
    fn discover(&self) -> Result<Vec<AgentSession>> {
        let output = match Command::new(&self.binary)
            .args(["session", "list", "--format", "json"])
            .output()
        {
            Ok(output) => output,
            // Not installed, or not on PATH: no conversations, not a failure.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => {
                return Err(anyhow::Error::new(e)
                    .context(format!("run `{} session list --format json`", self.binary)))
            }
        };
        if !output.status.success() {
            anyhow::bail!(
                "`{} session list --format json` exited {}: {}",
                self.binary,
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let stdout = String::from_utf8(output.stdout)
            .map_err(|_| anyhow::anyhow!("`{} session list` printed invalid UTF-8", self.binary))?;
        let value: serde_json::Value =
            serde_json::from_str(strip_banner(&stdout)).map_err(|e| {
                anyhow::anyhow!("`{} session list` printed invalid JSON: {e}", self.binary)
            })?;
        let Some(entries) = value.as_array() else {
            anyhow::bail!(
                "`{} session list --format json` printed {}, not the array of sessions it \
                 documents",
                self.binary,
                match &value {
                    serde_json::Value::Object(_) => "an object",
                    serde_json::Value::Null => "null",
                    _ => "a scalar",
                }
            );
        };

        let mut found = Vec::new();
        for entry in entries {
            let Some(id) = entry.get("id").and_then(|v| v.as_str()) else {
                continue;
            };
            let project_dir = entry
                .get("directory")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let last_active = entry
                .get("time")
                .and_then(|t| t.get("updated"))
                .and_then(|v| v.as_i64())
                .map(|ms| ms / 1000);
            found.push(AgentSession {
                kind: AgentKind::OpenCode,
                native_id: id.to_string(),
                project_dir,
                store_path: None,
                last_active,
                size_bytes: None,
                alive: false,
            });
        }
        Ok(found)
    }

    /// Always the resume form: `opencode --session <id>`.
    fn resume_argv(&self, id: &str) -> Vec<String> {
        vec![
            "opencode".to_string(),
            "--session".to_string(),
            id.to_string(),
        ]
    }

    /// **Unknowable through this adapter, and reported as such.**
    ///
    /// OpenCode's public CLI exposes no ownership query — no equivalent of
    /// "which pid holds this conversation open" — and this adapter is
    /// deliberately confined to that CLI (see the module docs). The honest
    /// answer is [`super::Liveness::Unknown`], and every caller fails closed
    /// on it. It used to return `false`, which reads identically to "verified
    /// nobody has it", and that is how a second client could be attached to a
    /// live conversation.
    fn is_active_elsewhere(&self, _id: &str) -> Result<super::Liveness> {
        Ok(super::Liveness::Unknown)
    }

    /// Unsupported, for the reasons in the module docs, in the words the user
    /// is shown.
    fn auto_unsupported_reason(&self) -> Option<&'static str> {
        Some(
            "OpenCode's public CLI cannot say which conversation a pane is running, \
             nor whether a conversation is already open elsewhere, so osm will not \
             bind a pane to one or resume one automatically. `osm agents --json` \
             still lists them; open one yourself with `opencode --session <id>`.",
        )
    }
}
