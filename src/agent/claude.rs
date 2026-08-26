//! Claude Code adapter: transcript discovery, resume argv, liveness.

use super::{AgentAdapter, AgentKind, AgentSession};
use anyhow::Result;
use std::fs;
use std::path::{Path, PathBuf};

pub struct Claude {
    home: PathBuf,
}

impl Claude {
    /// `$OSM_CLAUDE_HOME` if set, else `$HOME/.claude`.
    pub fn new() -> Self {
        Self {
            home: super::default_home("OSM_CLAUDE_HOME", ".claude"),
        }
    }

    /// For tests: address a fixture directory laid out like a Claude home
    /// (i.e. containing `projects/<encoded-cwd>/<uuid>.jsonl`) directly.
    pub fn with_home(home: &Path) -> Self {
        Self {
            home: home.to_path_buf(),
        }
    }
}

impl Default for Claude {
    fn default() -> Self {
        Self::new()
    }
}

/// Best-effort decode of Claude's directory-name encoding of a project
/// path: `/` replaced with `-` (e.g. `-home-u-app` for `/home/u/app`).
/// Lossy for any real path segment that itself contains a `-`, so this is
/// only ever a fallback for when the transcript's own `cwd` field (exact,
/// preferred) isn't available.
fn decode_project_dir_name(project_path: &Path) -> Option<String> {
    let name = project_path.file_name()?.to_str()?;
    if !name.starts_with('-') {
        return None;
    }
    Some(name.replace('-', "/"))
}

fn mtime_secs(metadata: &fs::Metadata) -> Option<i64> {
    metadata
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs() as i64)
}

impl AgentAdapter for Claude {
    fn kind(&self) -> AgentKind {
        AgentKind::Claude
    }

    /// # Absent is not the same as failed
    ///
    /// No `~/.claude/projects` at all — Claude never installed, or a fixture
    /// that points nowhere — is *no conversations*, and legitimate. A
    /// directory that exists and cannot be read is osm being unable to look,
    /// and that is an error. They used to be the same empty list, so a
    /// permissions problem reported "you have no conversations" and a restore
    /// could put back bare shells and call itself a success.
    fn discover(&self) -> Result<Vec<AgentSession>> {
        let mut found = Vec::new();
        let projects_dir = self.home.join("projects");
        let project_entries = match fs::read_dir(&projects_dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(found),
            Err(e) => {
                return Err(
                    anyhow::Error::new(e).context(format!("read {}", projects_dir.display()))
                )
            }
        };
        for project_entry in project_entries.flatten() {
            let project_path = project_entry.path();
            if !project_path.is_dir() {
                continue;
            }
            let file_entries = match fs::read_dir(&project_path) {
                Ok(entries) => entries,
                // Vanished between the two reads: an ordinary race with the
                // agent itself, not a failure to look.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    return Err(
                        anyhow::Error::new(e).context(format!("read {}", project_path.display()))
                    )
                }
            };
            for file_entry in file_entries.flatten() {
                let file_path = file_entry.path();
                if file_path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                    continue;
                }
                let Some(stem) = file_path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                // The whole stem must be the id, not merely contain one —
                // a file that isn't a transcript must be ignored, not
                // misread.
                let Some(id) = super::extract_uuid(stem).filter(|id| id == stem) else {
                    continue;
                };
                let metadata = file_entry.metadata().ok();
                // The transcript's own `cwd` line is exact; the directory
                // name is a lossy fallback used only when that's missing.
                let project_dir = super::project_dir_from_transcript(&file_path)
                    .or_else(|| decode_project_dir_name(&project_path));
                found.push(AgentSession {
                    kind: AgentKind::Claude,
                    native_id: id,
                    project_dir,
                    store_path: file_path.to_str().map(|s| s.to_string()),
                    last_active: metadata.as_ref().and_then(mtime_secs),
                    size_bytes: metadata.as_ref().map(|m| m.len() as i64),
                    alive: false,
                });
            }
        }
        Ok(found)
    }

    /// Always the resume form. `--session-id <id>` *creates* a conversation
    /// with that id and fails with "Session ID is already in use" for one
    /// that already exists — exactly the case being restored.
    fn resume_argv(&self, id: &str) -> Vec<String> {
        vec!["claude".to_string(), "--resume".to_string(), id.to_string()]
    }

    /// Whether any presently-running process holds this conversation's
    /// transcript open — scanned across every pid in `/proc`, not just a
    /// known process tree, since a resume has no live pane process to walk
    /// descendants from. See [`super::detect::live_process_ownership`], which
    /// answers `Unknown` — never `Inactive` — for a scan that could not be
    /// made or a descriptor that could not be identified.
    fn is_active_elsewhere(&self, id: &str) -> Result<super::Liveness> {
        super::detect::live_process_ownership(self, id)
    }

    /// Supported: one `.jsonl` transcript per conversation makes both halves
    /// answerable — which pane is running which conversation, and whether
    /// anything else has it open.
    fn auto_unsupported_reason(&self) -> Option<&'static str> {
        None
    }
}
