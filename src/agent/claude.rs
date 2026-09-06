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
    ///
    /// # Nothing outside the configured store is a conversation
    ///
    /// This used to accept any entry whose stem was a UUID and whose extension
    /// was `.jsonl`, without asking what the entry *was* or where it led. A
    /// symlink named after a UUID — or a project directory that is a symlink —
    /// therefore turned any JSONL-shaped file anywhere on the machine into a
    /// conversation osm would list, bind a pane to, and read a title out of and
    /// persist. Codex's walker refused symlinks from the day it was written;
    /// this one did not.
    ///
    /// So two checks, and both are needed. An entry must be a **regular file**
    /// by `lstat` (`DirEntry::file_type` does not follow), which refuses a
    /// symlinked transcript; and it must **resolve inside the resolved root**,
    /// which refuses a project directory that is a symlink out of the store —
    /// the files behind one of those are perfectly ordinary regular files, so
    /// the first check alone would let them all through.
    ///
    /// The root is resolved **once**, and that is what makes the legitimate
    /// case survive: `~/.claude -> /data/claude` is an ordinary arrangement (a
    /// home on a bigger filesystem, a dotfiles checkout), and the whole store
    /// is then reached through a symlink. Comparing against the *resolved* root
    /// accepts it while still refusing anything that leaves it. See
    /// `tests/agent_symlinked_home.rs` and `tests/agent_hardening.rs`.
    ///
    /// What is *recorded* is still the path osm was told about, never the
    /// resolved one: a `/proc/<pid>/fd` comparison and a restore both speak in
    /// the path the user's agent opens.
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
        let root = match fs::canonicalize(&projects_dir) {
            Ok(root) => root,
            // It was there a moment ago (`read_dir` succeeded), so this is a
            // race with the agent itself, not a store to guess about.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(found),
            Err(e) => {
                return Err(
                    anyhow::Error::new(e).context(format!("resolve {}", projects_dir.display()))
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
                // `lstat`, not `stat`: a symlink named like a transcript is
                // not a transcript, however ordinary the file behind it looks.
                match file_entry.file_type() {
                    Ok(ft) if ft.is_file() => {}
                    _ => continue,
                }
                // And the file this name reaches has to be inside the store.
                // A project directory that is a symlink out of it holds
                // perfectly ordinary regular files; only the resolved path
                // says they are somebody else's.
                match fs::canonicalize(&file_path) {
                    Ok(resolved) if resolved.starts_with(&root) => {}
                    _ => continue,
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
                // The decoder cannot tell a path separator from a hyphen in a
                // real directory name, so it turns `n8group-oss` into
                // `n8group/oss`. Only trust its answer when the directory it
                // names actually exists: a wrong path is worse than none,
                // because a restore would recreate the pane somewhere the
                // user never was.
                let project_dir = super::project_dir_from_transcript(&file_path).or_else(|| {
                    decode_project_dir_name(&project_path)
                        .filter(|d| std::path::Path::new(d).is_dir())
                });
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

    /// The last `ai-title` Claude wrote for this conversation, or one line of
    /// the first thing the user typed when it wrote none.
    ///
    /// Both come out of bounded reads of the transcript — a suffix for the
    /// title, a prefix for the prompt — and both refuse a record that names a
    /// different conversation. See [`super::title`].
    fn title_of(
        &self,
        session: &AgentSession,
        policy: super::title::Policy,
    ) -> Option<super::title::Title> {
        super::title::claude(
            Path::new(session.store_path.as_deref()?),
            &session.native_id,
            policy,
        )
    }

    /// Supported: one `.jsonl` transcript per conversation makes both halves
    /// answerable — which pane is running which conversation, and whether
    /// anything else has it open.
    fn auto_unsupported_reason(&self) -> Option<&'static str> {
        None
    }
}
