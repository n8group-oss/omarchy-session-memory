//! Codex adapter: rollout discovery, resume argv, active-writer refusal.

use super::{AgentAdapter, AgentKind, AgentSession};
use anyhow::Result;
use std::fs;
use std::path::{Path, PathBuf};

pub struct Codex {
    home: PathBuf,
}

impl Codex {
    /// `$OSM_CODEX_HOME` if set, else `$HOME/.codex`.
    pub fn new() -> Self {
        Self {
            home: super::default_home("OSM_CODEX_HOME", ".codex"),
        }
    }

    /// For tests: address a fixture directory laid out like a Codex home
    /// (i.e. containing `sessions/YYYY/MM/DD/rollout-*-<uuid>.jsonl`)
    /// directly.
    pub fn with_home(home: &Path) -> Self {
        Self {
            home: home.to_path_buf(),
        }
    }
}

impl Default for Codex {
    fn default() -> Self {
        Self::new()
    }
}

fn mtime_secs(metadata: &fs::Metadata) -> Option<i64> {
    metadata
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs() as i64)
}

/// Walks `sessions/**` looking for `rollout-*.jsonl` files, at any depth
/// (Codex nests them under `YYYY/MM/DD`, but that layout isn't load-bearing
/// here — only the filename is).
/// Codex lays sessions out as `sessions/YYYY/MM/DD/rollout-*.jsonl`, so three
/// levels is the real depth. The cap exists because this walks a directory in
/// the user's home that osm does not control.
const MAX_WALK_DEPTH: usize = 32;

fn walk_rollouts(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    walk_rollouts_at(dir, out, 0)
}

fn walk_rollouts_at(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) -> Result<()> {
    if depth >= MAX_WALK_DEPTH {
        return Ok(());
    }
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        // Gone between the parent listing and this read: an ordinary race
        // with Codex itself. Anything else — no permission, an I/O error — is
        // osm being unable to look, which is not "no conversations".
        // Absent, or gone between the parent listing and this read: no
        // conversations here, and an ordinary race with Codex itself.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(anyhow::Error::new(e).context(format!("read {}", dir.display()))),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // `file_type` from the dir entry does NOT follow symlinks, unlike
        // `path.is_dir()`. A symlink pointing back up its own tree would
        // otherwise recurse until the stack ran out.
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if ft.is_symlink() {
            continue;
        }
        if ft.is_dir() {
            walk_rollouts_at(&path, out, depth + 1)?;
            continue;
        }
        let is_rollout = path
            .file_stem()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.starts_with("rollout-"))
            && path.extension().and_then(|e| e.to_str()) == Some("jsonl");
        if is_rollout {
            out.push(path);
        }
    }
    Ok(())
}

/// If `stem` is a Codex rollout filename (`rollout-<timestamp>-<uuid>`),
/// its trailing conversation id.
/// Test-only view of [`rollout_id`], so hostile-filename cases can be
/// exercised without making the parser itself public API.
#[doc(hidden)]
pub fn rollout_id_for_test(stem: &str) -> Option<String> {
    rollout_id(stem)
}

fn rollout_id(stem: &str) -> Option<String> {
    if !stem.starts_with("rollout-") {
        return None;
    }
    super::extract_uuid(stem).filter(|id| stem.ends_with(id.as_str()))
}

impl AgentAdapter for Codex {
    fn kind(&self) -> AgentKind {
        AgentKind::Codex
    }

    fn discover(&self) -> Result<Vec<AgentSession>> {
        let mut found = Vec::new();
        // A missing `sessions` is no conversations and legitimate; one that is
        // there and cannot be walked is osm being unable to look, which
        // `walk_rollouts` reports as an error rather than as an empty list.
        let sessions_dir = self.home.join("sessions");
        let mut paths = Vec::new();
        walk_rollouts(&sessions_dir, &mut paths)?;
        for path in paths {
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let Some(id) = rollout_id(stem) else {
                continue;
            };
            let metadata = fs::metadata(&path).ok();
            // No directory-name fallback here (unlike Claude): a Codex
            // rollout filename carries no project-path encoding, only a
            // timestamp and the conversation id.
            let project_dir = super::project_dir_from_transcript(&path);
            found.push(AgentSession {
                kind: AgentKind::Codex,
                native_id: id,
                project_dir,
                store_path: path.to_str().map(|s| s.to_string()),
                last_active: metadata.as_ref().and_then(mtime_secs),
                size_bytes: metadata.as_ref().map(|m| m.len() as i64),
                alive: false,
            });
        }
        Ok(found)
    }

    /// Always the resume form — `codex resume <id>`, never a form that
    /// would attempt to create a conversation with an id that already
    /// exists on disk.
    fn resume_argv(&self, id: &str) -> Vec<String> {
        vec!["codex".to_string(), "resume".to_string(), id.to_string()]
    }

    /// Whether any presently-running process holds this conversation's
    /// rollout file open — scanned across every pid in `/proc`, not just a
    /// known process tree, since a resume has no live pane process to walk
    /// descendants from. See [`super::detect::live_process_ownership`], which
    /// answers `Unknown` — never `Inactive` — for a scan that could not be
    /// made or a descriptor that could not be identified.
    fn is_active_elsewhere(&self, id: &str) -> Result<super::Liveness> {
        super::detect::live_process_ownership(self, id)
    }

    /// One line of the first thing the user typed.
    ///
    /// Codex writes no title of its own, and the role-`user` messages at the
    /// head of a rollout are its own preamble rather than the user's words —
    /// see [`super::title::codex`], which reads the record that says a person
    /// typed something and refuses a rollout whose metadata names another
    /// conversation.
    fn title_of(
        &self,
        session: &AgentSession,
        policy: super::title::Policy,
    ) -> Option<super::title::Title> {
        super::title::codex(
            Path::new(session.store_path.as_deref()?),
            &session.native_id,
            policy,
        )
    }

    /// Supported: one rollout file per conversation answers both halves.
    fn auto_unsupported_reason(&self) -> Option<&'static str> {
        None
    }
}
