//! Agent conversation identity: which AI coding agent a pane is running, and
//! which conversation within it.
//!
//! A binding is only ever recorded when the evidence agrees. There is no
//! "best guess" path: a wrong binding sends a resume command for someone
//! else's conversation into a pane, which is worse than leaving a shell.

use anyhow::Result;
use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

pub mod claude;
pub mod codex;
pub mod detect;
pub mod opencode;
pub mod resume;

/// Resolves an adapter's home directory: an env override first, else
/// `$HOME/<default_sub>`. Never errors — a missing `HOME` just means the
/// adapter finds nothing, which `discover` already treats as empty rather
/// than a failure.
pub(crate) fn default_home(env_override: &str, default_sub: &str) -> PathBuf {
    if let Some(v) = std::env::var_os(env_override) {
        if !v.is_empty() {
            return PathBuf::from(v);
        }
    }
    match std::env::var_os("HOME") {
        Some(home) if !home.is_empty() => PathBuf::from(home).join(default_sub),
        _ => PathBuf::from(default_sub),
    }
}

/// Finds the first RFC-4122-shaped UUID substring (8-4-4-4-12 hex digits
/// separated by dashes) in `s`. Hand-written rather than a regex dependency
/// — the plan is deliberate about not adding one for this.
pub(crate) fn extract_uuid(s: &str) -> Option<String> {
    const GROUPS: [usize; 5] = [8, 4, 4, 4, 12];
    let bytes = s.as_bytes();
    let total_len: usize = GROUPS.iter().sum::<usize>() + (GROUPS.len() - 1);
    if bytes.len() < total_len {
        return None;
    }
    for start in 0..=bytes.len() - total_len {
        let mut pos = start;
        let mut matched = true;
        for (gi, &glen) in GROUPS.iter().enumerate() {
            if gi > 0 {
                if bytes.get(pos) == Some(&b'-') {
                    pos += 1;
                } else {
                    matched = false;
                    break;
                }
            }
            let group_end = pos + glen;
            if bytes[pos..group_end].iter().all(|b| b.is_ascii_hexdigit()) {
                pos = group_end;
            } else {
                matched = false;
                break;
            }
        }
        if matched {
            return Some(s[start..pos].to_string());
        }
    }
    None
}

/// Bytes considered when looking for a transcript's project-directory
/// signal (the `cwd` field on the first JSON line). A transcript can be
/// hundreds of megabytes, and a corrupt one may have no newline at all, so
/// this is a hard cap on the read itself — never "read until newline
/// found".
const FIRST_LINE_CAP_BYTES: usize = 8 * 1024;

/// The `cwd` field from the first line of the transcript at `path`, read as
/// at most [`FIRST_LINE_CAP_BYTES`] bytes regardless of the file's real
/// size or whether it contains a newline at all. Anything that doesn't
/// parse cleanly out of that capped prefix — truncated JSON, no `cwd`
/// field, non-UTF-8 bytes — is "no project directory", not an error.
pub(crate) fn project_dir_from_transcript(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut file = fs::File::open(path).ok()?;
    let mut buf = vec![0u8; FIRST_LINE_CAP_BYTES];
    let n = file.read(&mut buf).ok()?;
    buf.truncate(n);
    if let Some(nl) = buf.iter().position(|&b| b == b'\n') {
        buf.truncate(nl);
    }
    let text = std::str::from_utf8(&buf).ok()?;
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    value
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Minimum agreement required before a pane is bound to a conversation.
/// Below this the pane is recorded unbound and the menu asks a human.
pub const CONFIDENCE_THRESHOLD: f32 = 0.75;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentKind {
    Claude,
    Codex,
    OpenCode,
}

impl AgentKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            AgentKind::Claude => "claude",
            AgentKind::Codex => "codex",
            AgentKind::OpenCode => "opencode",
        }
    }

    /// Exact match only. A near-miss is an unknown kind, not a hint.
    pub fn parse(s: &str) -> Option<AgentKind> {
        match s {
            "claude" => Some(AgentKind::Claude),
            "codex" => Some(AgentKind::Codex),
            "opencode" => Some(AgentKind::OpenCode),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgentSession {
    pub kind: AgentKind,
    pub native_id: String,
    pub project_dir: Option<String>,
    pub store_path: Option<String>,
    pub last_active: Option<i64>,
    pub size_bytes: Option<i64>,
    pub alive: bool,
}

/// What identifies a file on this machine: the device it lives on and its
/// inode number.
///
/// Not its path. The same file has many paths — a symlinked home, a bind
/// mount, `/proc/<pid>/fd/N` itself — and different files can have paths that
/// look alike, which is the half that matters: a conversation must not be
/// recognised because something *named* like a transcript is open.
pub type FileId = (u64, u64);

/// The identity of the file `path` resolves to, or `None` if it cannot be
/// stat'ed (it has been deleted, or is not readable).
///
/// Follows symlinks deliberately: the question is always "which file is this",
/// never "which name was used to reach it".
pub fn file_id(path: &Path) -> Option<FileId> {
    let meta = fs::metadata(path).ok()?;
    Some((meta.dev(), meta.ino()))
}

/// An adapter's transcripts, keyed by the identity of the file itself.
///
/// Built once per detection pass from what `discover` found, and then the only
/// thing an open file descriptor is ever matched against. The rule this
/// replaced asked each adapter whether a *path* "looked like" one of its
/// transcripts — for Claude, any `.jsonl` whose whole stem is a UUID, anywhere
/// on the filesystem — so a pane holding `/tmp/<uuid>.jsonl` open scored as
/// though it were running that conversation. Matching by device and inode
/// against the discovered set is both stricter (a lookalike is not a
/// transcript unless it *is* one of the files discovery found) and looser in
/// the way that is actually needed (a symlinked or bind-mounted home resolves
/// to the same file, and is recognised).
#[derive(Debug, Default, Clone)]
pub struct TranscriptIndex {
    by_file: HashMap<FileId, String>,
    /// Every path each conversation's transcript is known by, kept alongside
    /// the file identity rather than instead of it.
    ///
    /// A path is not an identity — that is the whole point of `by_file` — but
    /// it is the only handle on a file that has been *unlinked*, which is what
    /// an atomic replacement leaves the running agent holding. See
    /// [`detect::live_process_ownership`], which uses these to tell "nobody
    /// holds this conversation" apart from "the descriptor that holds it can
    /// no longer be identified".
    ///
    /// # Why there is more than one path per conversation
    ///
    /// The path discovery walked and the path the kernel reports for an open
    /// descriptor are not the same string whenever anything on the way to the
    /// agent's home is a symlink. `~/.claude -> /data/claude` is an ordinary
    /// arrangement — a home on a different filesystem, a dotfiles checkout —
    /// and it made discovery record `~/.claude/projects/…/<id>.jsonl` while
    /// `/proc/<pid>/fd/N` reads `/data/claude/projects/…/<id>.jsonl
    /// (deleted)`, because `/proc` always answers with the path the kernel
    /// resolved to and never with the one the process used to get there.
    /// A literal comparison of the two missed every time, so a live
    /// conversation on a symlinked home answered "nobody holds this" and a
    /// second client was free to attach to it.
    ///
    /// So each conversation carries its discovered path **and** its
    /// canonical alias, and a deleted descriptor is compared against all of
    /// them. `by_file` is unaffected: device and inode were never confused by
    /// a symlink, which is exactly why identity is kept there and this is only
    /// the fallback for a file that no longer has one.
    by_id: HashMap<String, Vec<PathBuf>>,
}

/// Every path `path` is known by on this machine: the one given, and the one
/// the kernel resolves it to.
///
/// The canonical form is what `/proc/<pid>/fd/N` reports for an open
/// descriptor, so it is the form a *deleted* descriptor has to be matched
/// against. It is taken from the parent directory when the file itself can no
/// longer be resolved — which is precisely the case this exists for, the file
/// having been replaced between discovery and here — since it is the
/// directory components that a symlinked home differs in.
fn path_aliases(path: &Path) -> Vec<PathBuf> {
    let mut out = vec![path.to_path_buf()];
    let canonical = fs::canonicalize(path).ok().or_else(|| {
        let parent = fs::canonicalize(path.parent()?).ok()?;
        Some(parent.join(path.file_name()?))
    });
    if let Some(canonical) = canonical {
        if !out.contains(&canonical) {
            out.push(canonical);
        }
    }
    out
}

impl TranscriptIndex {
    /// Index every session that has a store path this process can stat. A
    /// session whose file has gone between discovery and here is simply
    /// absent: it cannot be the file anything holds open.
    pub fn of(sessions: &[AgentSession]) -> Self {
        let mut by_file = HashMap::new();
        let mut by_id: HashMap<String, Vec<PathBuf>> = HashMap::new();
        for session in sessions {
            let Some(path) = session.store_path.as_deref() else {
                continue;
            };
            let known = by_id.entry(session.native_id.clone()).or_default();
            for alias in path_aliases(Path::new(path)) {
                if !known.contains(&alias) {
                    known.push(alias);
                }
            }
            if let Some(id) = file_id(Path::new(path)) {
                by_file.insert(id, session.native_id.clone());
            }
        }
        Self { by_file, by_id }
    }

    /// The conversation whose transcript `path` **is**, matched by device and
    /// inode. `None` for anything that is not one of the discovered files.
    pub fn id_of(&self, path: &Path) -> Option<&str> {
        self.by_file.get(&file_id(path)?).map(String::as_str)
    }

    /// Every path `id`'s transcript is known by — the one discovery walked and
    /// the canonical alias `/proc` reports for an open descriptor. Empty for a
    /// conversation this index has never heard of.
    pub fn paths_of(&self, id: &str) -> Vec<PathBuf> {
        self.by_id.get(id).cloned().unwrap_or_default()
    }

    /// Whether `path` is a path some conversation in this index is known by —
    /// discovered or canonical — regardless of what, if anything, is at that
    /// path now.
    pub fn knows_path(&self, path: &Path) -> bool {
        self.by_id
            .values()
            .any(|paths| paths.iter().any(|p| p == path))
    }

    pub fn is_empty(&self) -> bool {
        self.by_file.is_empty()
    }

    pub fn len(&self) -> usize {
        self.by_file.len()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Binding {
    pub kind: AgentKind,
    pub native_id: String,
    pub confidence: f32,
}

pub trait AgentAdapter {
    fn kind(&self) -> AgentKind;

    /// Every conversation this agent has on disk.
    fn discover(&self) -> Result<Vec<AgentSession>>;

    /// The exact command that resumes an existing conversation.
    ///
    /// Always the resume form. The create-with-id form fails with
    /// "Session ID is already in use" for a conversation that exists, which
    /// is precisely the case being restored.
    fn resume_argv(&self, id: &str) -> Vec<String>;

    /// Whether some other live process already owns this conversation.
    ///
    /// Three answers, not two: see [`Liveness`]. An adapter that cannot tell
    /// must say [`Liveness::Unknown`] and never `Inactive`.
    fn is_active_elsewhere(&self, id: &str) -> Result<Liveness>;

    /// This agent's transcripts, keyed by the identity of the file itself.
    ///
    /// The default is right for every adapter whose conversations are files it
    /// can point at: index what `discover` found. An adapter with no store
    /// path per conversation gets an empty index, which is the honest answer —
    /// it cannot recognise its own conversations from an open file descriptor,
    /// and therefore cannot prove that a pane is running one.
    fn transcript_index(&self) -> Result<TranscriptIndex> {
        Ok(TranscriptIndex::of(&self.discover()?))
    }

    /// Why osm will not capture or resume this agent's conversations by
    /// itself, or `None` when it will.
    ///
    /// One statement, in one place, behind every consequence: `detect::bind`
    /// records no binding for such an adapter, `resume::deliver` refuses to
    /// send anything and reports [`resume::Outcome::Unsupported`], `osm status
    /// --json` lists it, and the README says the same thing in prose. A
    /// capability that is claimed in one of those and not the others is how a
    /// whole code path came to be unreachable without anything saying so.
    ///
    /// The text is shown to users, so it says what is missing and what still
    /// works by hand.
    fn auto_unsupported_reason(&self) -> Option<&'static str>;
}

/// Whether a conversation is open somewhere else on this machine.
///
/// `Unknown` is a first-class answer and is **not** a synonym for `Inactive`.
/// An adapter with no way to ask must say so, and every caller must fail
/// closed on it: "nobody has this open" and "osm cannot tell whether anybody
/// has this open" differ by exactly one live conversation being handed a
/// second client, which is the outcome
/// [`resume::Outcome::ActiveElsewhere`] exists to prevent. Reporting `false`
/// for "unknowable" put that failure one refactor away from happening, with a
/// doc comment as the only guard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    Active,
    Inactive,
    Unknown,
}

/// Adapters for the enabled kinds, in the order given. Unknown names are
/// ignored rather than erroring: config validation reports those, and a
/// typo must not disable capture entirely.
pub fn adapters(enabled: &[String]) -> Vec<Box<dyn AgentAdapter>> {
    enabled
        .iter()
        .filter_map(|name| match AgentKind::parse(name) {
            Some(AgentKind::Claude) => {
                Some(Box::new(claude::Claude::new()) as Box<dyn AgentAdapter>)
            }
            Some(AgentKind::Codex) => Some(Box::new(codex::Codex::new()) as Box<dyn AgentAdapter>),
            Some(AgentKind::OpenCode) => {
                Some(Box::new(opencode::OpenCode::new()) as Box<dyn AgentAdapter>)
            }
            None => None,
        })
        .collect()
}

/// A conversation a pane is holding open right now.
///
/// `session` is the same conversation as [`AgentSession`] from the adapter's
/// `discover`, with `alive` set — or `None` when the pane holds a transcript
/// the adapter recognises but `discover` no longer lists (the file was
/// deleted or moved out from under the running agent). That case is reported
/// as a live binding with no session rather than dropped: the pane really is
/// running that conversation, and hiding it would make `osm agents` disagree
/// with the pane in front of the user.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveAgent {
    pub pane_id: String,
    pub kind: AgentKind,
    pub native_id: String,
    pub confidence: f32,
    pub session: Option<AgentSession>,
}

/// What `osm agents` reports: every conversation these adapters know about,
/// split by whether a live pane is running it.
///
/// The two lists are disjoint by construction — a conversation that is live
/// is never also offered as resumable, because "resume" for one already
/// running means attaching a second client to it, which is the outcome
/// [`resume::Outcome::ActiveElsewhere`] exists to refuse.
#[derive(Debug, Clone, PartialEq)]
pub struct Inventory {
    /// Bound panes, in the order the probes were given (which is tmux's own
    /// pane order).
    pub live: Vec<LiveAgent>,
    /// Everything else on disk, newest `last_active` first, so a menu can
    /// take the head of the list. Conversations with no recorded
    /// `last_active` sort last.
    pub resumable: Vec<AgentSession>,
    /// Adapters that could not be asked, and why.
    ///
    /// Only ever an adapter osm does not act on automatically — a supported
    /// one's failure is returned as an error, because a capture must not
    /// proceed on a half-read inventory. For the rest, "osm could not look" is
    /// reported *in the listing the user asked for* rather than turned into an
    /// empty list. An installed-but-unusable OpenCode used to be
    /// indistinguishable from having no OpenCode conversations at all.
    pub problems: Vec<(AgentKind, String)>,
}

/// Build the inventory: discover every conversation each adapter knows,
/// bind each probe to one (or to nothing), and partition.
///
/// Binding uses the same confidence-scored [`detect::bind`] capture uses —
/// never a looser rule for display purposes, or `osm agents` would show a
/// pane as running a conversation that capture would refuse to record.
///
/// A discovery failure is propagated rather than swallowed: an adapter that
/// simply is not installed already reports an empty list (see each
/// adapter's `discover`), so an error here is a real one — a home directory
/// that exists but cannot be read — and reporting an empty inventory for it
/// would claim the user has no conversations when the truth is that osm
/// could not look.
pub fn inventory(
    probes: &[detect::PaneProbe],
    adapters: &[Box<dyn AgentAdapter>],
) -> Result<Inventory> {
    // Read each adapter's conversations once, propagating a failure rather
    // than reporting an empty inventory for a store osm could not look in.
    // `prepare` covers the adapters osm acts on automatically.
    let prepared = detect::prepare(adapters)?;
    let mut discovered: Vec<AgentSession> = prepared
        .iter()
        .flat_map(|p| p.sessions.iter().cloned())
        .collect();

    // The rest are asked here, for listing only. Nothing they return can bind
    // a pane, so a failure is reported alongside the listing instead of
    // failing it: the user asked what is on the machine and is entitled to
    // both the part osm could read and the part it could not.
    let mut problems = Vec::new();
    for adapter in adapters {
        if adapter.auto_unsupported_reason().is_none() {
            continue;
        }
        match adapter.discover() {
            Ok(sessions) => discovered.extend(sessions),
            Err(e) => problems.push((adapter.kind(), format!("{e:#}"))),
        }
    }

    let mut live = Vec::new();
    for probe in probes {
        let Some(binding) = detect::bind(probe, &prepared) else {
            continue;
        };
        let session = discovered
            .iter()
            .find(|s| s.kind == binding.kind && s.native_id == binding.native_id)
            .map(|s| AgentSession {
                alive: true,
                ..s.clone()
            });
        live.push(LiveAgent {
            pane_id: probe.pane_id.clone(),
            kind: binding.kind,
            native_id: binding.native_id,
            confidence: binding.confidence,
            session,
        });
    }

    let mut resumable: Vec<AgentSession> = discovered
        .into_iter()
        .filter(|s| {
            !live
                .iter()
                .any(|l| l.kind == s.kind && l.native_id == s.native_id)
        })
        .collect();
    // Newest first; `None` (no recorded activity) sorts last rather than
    // ahead of everything, which is what a plain descending sort on
    // `Option` would do.
    resumable.sort_by(|a, b| {
        b.last_active
            .unwrap_or(i64::MIN)
            .cmp(&a.last_active.unwrap_or(i64::MIN))
            .then_with(|| a.native_id.cmp(&b.native_id))
    });

    Ok(Inventory {
        live,
        resumable,
        problems,
    })
}
