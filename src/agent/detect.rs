//! Pane → agent binding: process-tree walk, open-fd inspection, confidence
//! scoring.
//!
//! Nothing here is allowed to guess. A binding is only ever recorded when
//! the evidence reaches [`super::CONFIDENCE_THRESHOLD`], and a tie between
//! two adapters records nothing — see [`bind`].

use super::{AgentAdapter, AgentSession, Binding, Liveness, TranscriptIndex, CONFIDENCE_THRESHOLD};
use anyhow::{Context, Result};
use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};

/// A hard ceiling on how many pids a single [`descendants`] walk will ever
/// visit. Without it a fork bomb (or a hostile process tree) could make the
/// walk run forever; with it, the walk always terminates in bounded work no
/// matter what the tree looks like.
const MAX_DESCENDANTS: usize = 4096;

/// A live tmux pane, as seen right now: which process leads it, where it is,
/// and what is running in its foreground.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneProbe {
    pub pane_id: String,
    pub pane_pid: u32,
    pub cwd: String,
    pub foreground_cmd: String,
}

/// Every pid reachable from `pid` through the Linux process tree, `pid`
/// itself included, in breadth-first order.
///
/// Reads `/proc/<pid>/task/*/children` iteratively — never recursively, so
/// there is no call stack to overflow — with a visited set so a cycle in
/// the tree (should the kernel's own invariants ever be violated, or a pid
/// get reused mid-walk) cannot loop forever, and a hard cap of
/// [`MAX_DESCENDANTS`] pids so a fork bomb cannot make this unbounded.
///
/// A process vanishing mid-walk is the normal case, not an error: every
/// `/proc` read that fails (the pid exited, a race with the kernel reusing
/// the slot, no permission) is simply treated as "no children here" rather
/// than propagated.
pub fn descendants(pid: u32) -> Vec<u32> {
    let mut visited: HashSet<u32> = HashSet::new();
    let mut queue: VecDeque<u32> = VecDeque::new();
    let mut out = Vec::new();

    visited.insert(pid);
    queue.push_back(pid);

    while let Some(p) = queue.pop_front() {
        out.push(p);
        if visited.len() >= MAX_DESCENDANTS {
            break;
        }
        for child in children_of(p) {
            if visited.len() >= MAX_DESCENDANTS {
                break;
            }
            if visited.insert(child) {
                queue.push_back(child);
            }
        }
    }

    out
}

/// The direct children of `pid`, read across every thread's `children`
/// file (a multi-threaded process lists its children under each of its
/// task ids, not just the main one). Any failure — the process has exited,
/// a thread has exited, no permission — yields no children rather than an
/// error: a vanished process is the ordinary outcome of walking a live
/// tree, not a fault.
fn children_of(pid: u32) -> Vec<u32> {
    let mut out = Vec::new();
    let task_dir = PathBuf::from(format!("/proc/{pid}/task"));
    let Ok(entries) = std::fs::read_dir(&task_dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let children_path = entry.path().join("children");
        let Ok(contents) = std::fs::read_to_string(&children_path) else {
            continue;
        };
        for tok in contents.split_whitespace() {
            if let Ok(cpid) = tok.parse::<u32>() {
                out.push(cpid);
            }
        }
    }
    out
}

/// What one process's open descriptors say about the transcripts it holds.
///
/// The second list is the whole reason this is not just a `Vec`: a descriptor
/// whose file has been unlinked is **not** evidence that the process holds no
/// transcript, and reading it as such is how a live conversation came to be
/// reported as owned by nobody.
#[derive(Debug, Default, Clone)]
pub struct HeldTranscripts {
    /// Targets that still name a file, so they can be identified by device and
    /// inode.
    pub present: Vec<PathBuf>,
    /// Targets the kernel marks `" (deleted)"`, carrying the path the file
    /// **used to** have. The process still has the file; nothing on the
    /// filesystem can be stat'ed to say which one it is.
    pub deleted: Vec<PathBuf>,
}

/// The transcript-shaped descriptors `pid` currently holds: every
/// `/proc/<pid>/fd/*` symlink whose target ends in `.jsonl`, split by whether
/// the file behind it still has that name.
///
/// # Why the deleted half is collected rather than skipped
///
/// A transcript is replaced atomically — write a new file, rename it over the
/// old one — and the agent that had the old one open goes on writing to it.
/// The kernel then reports that descriptor as `…/<id>.jsonl (deleted)`, whose
/// *extension* is `jsonl (deleted)`, so a filter on `.jsonl` dropped it
/// silently. Discovery, meanwhile, indexes the new file. Every question about
/// who owns that conversation was then answered "nobody": a resume was allowed
/// into a conversation that is very much alive, and a capture recorded the
/// pane running it as holding nothing.
///
/// Following the stripped path would be worse than dropping it, which is why
/// it is kept separate: the name now resolves to the *replacement* file, so
/// stat'ing it answers a question about a different inode than the one this
/// process holds.
///
/// A path that genuinely ends in `" (deleted)"` is indistinguishable from a
/// deleted one here — `/proc` does not escape it — and is read as deleted,
/// which is the fail-closed half of the ambiguity.
///
/// A process without that directory (already gone), or an fd that closes
/// mid-read (a race inherent to inspecting a live process), is simply
/// skipped — the normal case, not an error.
pub fn held_transcripts(pid: u32) -> HeldTranscripts {
    match held_transcripts_in(Path::new("/proc"), pid) {
        Descriptors::Listed(held) => held,
        // The lineage walk that reaches here already knows *which* process it
        // is asking about, and answers "no evidence" rather than "nothing
        // there" when it comes up empty — see `bind`. The scan that needs the
        // distinction is `live_process_ownership`, which has no lineage to
        // narrow by and so reads this three ways instead of two.
        Descriptors::Gone | Descriptors::Unreadable => HeldTranscripts::default(),
    }
}

/// What one process's descriptor directory could be made to say.
///
/// Three answers, because two of them were the same answer once and it is the
/// same mistake as [`Liveness`]: a directory that **will not be listed** is
/// not a directory that is empty. A live agent that is non-dumpable — it
/// dropped privileges, or a sandbox set `PR_SET_DUMPABLE` to 0 — has its
/// `/proc/<pid>/fd` reparented to root and denied to its own user, and reading
/// that as "this process holds no transcripts" is how a conversation someone
/// is talking to right now came to be reported as owned by nobody.
#[derive(Debug)]
pub enum Descriptors {
    /// The directory was listed; this is everything transcript-shaped in it.
    Listed(HeldTranscripts),
    /// There is no such directory: the process has exited. Nothing is being
    /// hidden, because there is nothing left to hide.
    Gone,
    /// The directory is there and would not be listed. This process may hold
    /// anything at all, including the conversation being asked about.
    Unreadable,
}

/// [`held_transcripts`], against a process table the caller names.
///
/// `proc_root` is `/proc` everywhere in the shipped binary. It is a parameter
/// so that a test can present a descriptor directory this process may not
/// list without creating a process it may not inspect — which cannot be done
/// portably from safe Rust, and certainly not from a CI container whose root
/// can read every directory on the machine regardless of its mode.
pub fn held_transcripts_in(proc_root: &Path, pid: u32) -> Descriptors {
    let mut out = HeldTranscripts::default();
    let fd_dir = proc_root.join(pid.to_string()).join("fd");
    let entries = match std::fs::read_dir(&fd_dir) {
        Ok(entries) => entries,
        // Told apart deliberately. A process that has exited between the
        // listing of `proc_root` and this read is the ordinary case and says
        // nothing about anybody's conversation; a directory that exists and
        // refuses to be read is the case this enum exists for.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Descriptors::Gone,
        Err(_) => return Descriptors::Unreadable,
    };
    for entry in entries.flatten() {
        let Ok(target) = std::fs::read_link(entry.path()) else {
            continue;
        };
        if target.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            out.present.push(target);
            continue;
        }
        let raw = target.to_string_lossy();
        if let Some(path) = raw.strip_suffix(" (deleted)") {
            let path = PathBuf::from(path);
            if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                out.deleted.push(path);
            }
        }
    }
    Descriptors::Listed(out)
}

/// The real uid of `pid`, from `<proc_root>/<pid>/status`.
///
/// `status` and not the ownership of `/proc/<pid>` itself, which is the trap:
/// the kernel reparents every inode under a **non-dumpable** process's
/// directory to root, so `metadata()` reports `uid 0` for exactly the
/// processes this is asked about, and a uid filter written that way would
/// exclude every one of them. `status` stays readable and keeps telling the
/// truth.
fn real_uid(proc_root: &Path, pid: u32) -> Option<u32> {
    uid_from_status(&proc_root.join(pid.to_string()).join("status"))
}

fn uid_from_status(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path)
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|s| s.parse::<u32>().ok())
}

/// The name `pid` runs under, taken from `cmdline` and falling back to `comm`.
///
/// Wider than [`process_name`] on purpose, and only used to decide whether a
/// process osm *cannot* inspect is worth being uncertain about: `comm` is
/// truncated to 15 bytes, which is a name this could get wrong for a long
/// binary name but never for `claude`, `codex` or `opencode`. Being wrong here
/// costs a refused resume, not a double attach.
fn any_process_name(proc_root: &Path, pid: u32) -> Option<String> {
    let dir = proc_root.join(pid.to_string());
    let argv0 = std::fs::read(dir.join("cmdline"))
        .ok()
        .and_then(|raw| {
            let argv0 = raw.split(|&b| b == 0).next().unwrap_or(&[]).to_vec();
            (!argv0.is_empty()).then(|| String::from_utf8_lossy(&argv0).into_owned())
        })
        .and_then(|name| {
            Path::new(&name)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
        });
    argv0.or_else(|| {
        Some(
            std::fs::read_to_string(dir.join("comm"))
                .ok()?
                .trim()
                .to_string(),
        )
        .filter(|s| !s.is_empty())
    })
}

/// The transcript-shaped paths `pid` currently holds open and that still name
/// a file. See [`held_transcripts`] for the half this deliberately leaves out.
pub fn open_transcripts(pid: u32) -> Vec<PathBuf> {
    held_transcripts(pid).present
}

/// Conversation ids this pane's agent lineage names in its own argv.
///
/// A process running `claude --resume <uuid>` is not evidence *about* a
/// conversation; it is the process saying which one it is running. That is
/// the strongest signal available, and on the maintainer's machine it is the
/// only one: measured across 20 live panes, 20 named the id in argv and
/// **none** held the transcript open. Claude Code appends and closes rather
/// than keeping the file open, so a detector resting on open descriptors
/// finds nothing at all.
///
/// Only the flags that name an existing conversation count. `--session-id`
/// on a fresh launch mints an id that is equally a statement of identity, so
/// both are read; a bare `claude` with no id yields nothing.
pub fn argv_conversation_ids(pane_pid: u32, binary: &str) -> Vec<String> {
    const FLAGS: [&str; 3] = ["--resume", "--session-id", "--session"];
    let mut out: Vec<String> = Vec::new();
    for pid in descendants(pane_pid) {
        if process_name(pid).as_deref() != Some(binary) {
            continue;
        }
        let Ok(raw) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
            continue;
        };
        let args: Vec<String> = raw
            .split(|b| *b == 0)
            .filter(|a| !a.is_empty())
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .collect();
        for (i, a) in args.iter().enumerate() {
            // `--resume <id>` and `--resume=<id>` are both real spellings.
            let candidate = if let Some(eq) = a.find('=') {
                FLAGS.contains(&&a[..eq]).then(|| a[eq + 1..].to_string())
            } else if FLAGS.contains(&a.as_str()) {
                args.get(i + 1).cloned()
            } else {
                None
            };
            if let Some(id) = candidate {
                if !id.is_empty() && !id.starts_with('-') && !out.contains(&id) {
                    out.push(id);
                }
            }
        }
    }
    out
}

/// The name a process runs under, as **tmux** reads it: the basename of
/// `argv[0]` from `/proc/<pid>/cmdline`.
///
/// Deliberately the same source tmux's own `#{pane_current_command}` uses on
/// Linux, so "the pane is running `claude`" and "this pid is `claude`" cannot
/// disagree about what a process is called. Falls back to `/proc/<pid>/comm`
/// only when `cmdline` is empty, which for a userspace process means it is
/// mid-exec or already gone.
pub fn process_name(pid: u32) -> Option<String> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let argv0 = raw.split(|&b| b == 0).next().unwrap_or(&[]);
    let name = if argv0.is_empty() {
        std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .ok()?
            .trim()
            .to_string()
    } else {
        String::from_utf8_lossy(argv0).into_owned()
    };
    if name.is_empty() {
        return None;
    }
    Some(
        Path::new(&name)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or(name),
    )
}

/// The transcripts held open by the **agent's own process lineage** inside
/// `pane_pid`'s tree: a descendant running under the name `binary`, or a
/// descendant of *that*.
///
/// # Why the whole pane tree is not the lineage
///
/// The evidence being collected is "this pane is running conversation X",
/// and the two halves of it — the agent's identity and the open transcript —
/// have to come from the same process, or they are two facts about two
/// unrelated programs. A pane running conversation A with a background job
/// tailing conversation B's transcript has both halves present and belongs to
/// neither: the old rule scored it 0.9 and bound the pane to **B**. Anything
/// a user leaves running in a pane — a `tail -f`, an editor with a transcript
/// open, a second agent's log follower — could rewrite the binding of the pane
/// they were actually working in.
pub fn agent_lineage_transcripts(pane_pid: u32, binary: &str) -> Vec<PathBuf> {
    lineage_transcripts(pane_pid, binary).present
}

/// [`agent_lineage_transcripts`], keeping the descriptors whose files have
/// been unlinked as well as the ones that can still be identified.
pub fn lineage_transcripts(pane_pid: u32, binary: &str) -> HeldTranscripts {
    let mut out = HeldTranscripts::default();
    let mut seen: HashSet<u32> = HashSet::new();
    for pid in descendants(pane_pid) {
        if process_name(pid).as_deref() != Some(binary) {
            continue;
        }
        for descendant in descendants(pid) {
            if seen.insert(descendant) {
                let held = held_transcripts(descendant);
                out.present.extend(held.present);
                out.deleted.extend(held.deleted);
            }
        }
    }
    out
}

/// Whether this pane's agent lineage holds a transcript osm cannot identify —
/// the reason [`bind`] returned nothing being "could not tell" rather than
/// "nothing there".
///
/// The case is the same replacement [`live_process_ownership`] fails closed
/// on, seen from the capture side: the agent goes on writing to the file it
/// opened, discovery indexes the file that has taken its name, and the
/// descriptor joining them cannot be matched by device and inode. Scoring that
/// as no evidence records the pane as holding no conversation and throws away
/// the only binding anyone had for it.
///
/// Narrow on purpose: only a descriptor whose former path is a path this
/// detection pass actually discovered counts. An unrelated deleted `.jsonl`
/// somewhere else on the machine is not this adapter's transcript and must not
/// make every pane unreadable.
pub fn lineage_ownership_unknown(probe: &PaneProbe, prepared: &[Prepared]) -> bool {
    prepared.iter().any(|p| {
        if p.adapter.auto_unsupported_reason().is_some() {
            return false;
        }
        lineage_transcripts(probe.pane_pid, p.adapter.kind().as_str())
            .deleted
            .iter()
            .any(|path| p.index.knows_path(path))
    })
}

/// Whether some presently-running process on this machine holds `adapter`'s
/// transcript for `id` open.
///
/// Three answers, never two — see [`super::Liveness`]. "Nobody has this open"
/// and "osm cannot tell whether anybody has this open" differ by exactly one
/// live conversation being handed a second client.
///
/// Unlike [`bind`], there is no pane process to walk descendants from here
/// — the caller (resume) has only a conversation id, from a binding
/// persisted across a reboot, with no live pane behind it yet. So this
/// scans every pid in `/proc` rather than one process's descendants, reusing
/// [`held_transcripts`] per pid exactly as [`bind`] does. Lineage is
/// deliberately *not* required here either: the question is only "is anything
/// at all holding this conversation open", and answering yes for a process
/// that merely has it open is the fail-closed answer — it refuses a resume
/// that might otherwise attach a second client.
///
/// Reading `/proc/<pid>/fd` is the entire interaction: nothing here
/// signals, writes to, or otherwise touches any process it finds. A pid
/// this process cannot read (already exited, no permission) is simply
/// skipped, the same as everywhere else in this module.
///
/// # The three shapes of "cannot tell"
///
/// * `/proc` itself will not be listed. Nothing was scanned, so nothing may be
///   concluded from having found nothing.
/// * Some process holds a descriptor whose file **was** this conversation's
///   transcript at the path discovery indexes for it, and has since been
///   unlinked — the ordinary result of the file being replaced atomically
///   under a running agent. That descriptor cannot be matched by device and
///   inode, because the name now belongs to the replacement. The holder may
///   well be the agent, so the answer is `Unknown`, not `Inactive`.
/// * A process that **could** be this agent will not show its descriptors at
///   all — see [`Descriptors::Unreadable`] and [`could_hold`]. It was
///   previously returned as holding nothing, which is the same sentence as
///   "nobody has this conversation open" and had the same consequence: a
///   second client sent into a live conversation.
///
/// A discovery failure is an error, never a verdict: "osm could not look" is
/// not "nothing is holding it" either.
pub fn live_process_ownership(adapter: &dyn AgentAdapter, id: &str) -> Result<Liveness> {
    live_process_ownership_in(adapter, id, Path::new("/proc"))
}

/// [`live_process_ownership`], against a process table the caller names. See
/// [`held_transcripts_in`] for why `proc_root` is a parameter.
pub fn live_process_ownership_in(
    adapter: &dyn AgentAdapter,
    id: &str,
    proc_root: &Path,
) -> Result<Liveness> {
    let index = adapter
        .transcript_index()
        .with_context(|| format!("look for a live holder of {id}"))?;
    if index.is_empty() {
        return Ok(Liveness::Inactive);
    }
    // The paths this conversation's transcript is known by. A descriptor
    // holding an unlinked file that used to be one of them is this
    // conversation's, or was a moment ago; one holding an unlinked file from
    // anywhere else says nothing about it.
    let mine: Vec<PathBuf> = index.paths_of(id);
    let binary = adapter.kind().as_str();
    let us = uid_from_status(&proc_root.join("self").join("status"));
    let Ok(entries) = std::fs::read_dir(proc_root) else {
        return Ok(Liveness::Unknown);
    };
    let mut unknown = false;
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        match held_transcripts_in(proc_root, pid) {
            Descriptors::Listed(held) => {
                for path in held.present {
                    if index.id_of(&path) == Some(id) {
                        return Ok(Liveness::Active);
                    }
                }
                if held.deleted.iter().any(|path| mine.contains(path)) {
                    unknown = true;
                }
            }
            Descriptors::Gone => {}
            Descriptors::Unreadable => {
                if could_hold(proc_root, pid, binary, us) {
                    unknown = true;
                }
            }
        }
    }
    Ok(if unknown {
        Liveness::Unknown
    } else {
        Liveness::Inactive
    })
}

/// Whether a process whose descriptors cannot be read could plausibly be
/// holding `binary`'s transcripts.
///
/// # Why this is a filter and not simply "yes"
///
/// Every uncertainty is paid for by a refused resume, so widening it is not
/// free: answering `Unknown` for any process osm cannot inspect would make
/// *every* resume fail on an ordinary desktop, because an ordinary desktop
/// always has a few. Measured on the machine this was written on: 470
/// processes belonging to the user, six of whose descriptor directories are
/// closed — a password manager's browser helper, a `fusermount3`, a session
/// helper. None of them is an agent, and none of them may be allowed to make
/// a conversation unresumable.
///
/// So the candidate set is narrowed rather than the uncertainty widened, on
/// the two facts that remain readable for a process that hides everything
/// else:
///
/// * it runs as the same user. Everything protected that osm cannot read
///   *because it belongs to somebody else* — every system daemon — is out, and
///   an agent osm might have to resume is by definition the user's own.
/// * it runs under the agent's own binary name. An agent holding this
///   conversation is a process called `claude` (or `codex`, or `opencode`);
///   the same rule `bind` scores a pane's foreground command by.
///
/// A process that satisfies both and will not show its descriptors is the
/// exact shape of a live agent that has been made non-dumpable, and is the
/// only thing that gets to make this answer `Unknown`.
///
/// `us` is `None` only when this process cannot read its own `status`, in
/// which case the uid half is skipped rather than guessed — the name filter
/// still stands, and skipping it would restore the failure this exists to
/// prevent.
fn could_hold(proc_root: &Path, pid: u32, binary: &str, us: Option<u32>) -> bool {
    if let Some(us) = us {
        if real_uid(proc_root, pid) != Some(us) {
            return false;
        }
    }
    any_process_name(proc_root, pid).as_deref() == Some(binary)
}

/// One adapter, with everything read from disk for a single detection pass.
///
/// Built once by [`prepare`] and shared across every pane probed, so a
/// capture of thirty panes reads each agent's home once rather than thirty
/// times — and, more importantly, so a discovery *failure* is raised once, to
/// a caller that can decide what to do about it, instead of being swallowed
/// per pane into "this pane holds no conversation".
pub struct Prepared<'a> {
    pub adapter: &'a dyn AgentAdapter,
    pub sessions: Vec<AgentSession>,
    pub index: TranscriptIndex,
}

/// Read the conversations of every adapter osm acts on automatically, once.
///
/// Adapters whose automatic capture and resume are unsupported
/// ([`AgentAdapter::auto_unsupported_reason`]) are skipped entirely, and not
/// even asked: nothing they could return would be allowed to bind a pane, so
/// running their discovery would only give a capture a way to fail for a
/// reason that cannot affect its result. `osm agents` asks them separately,
/// because there the user *is* asking.
///
/// For the rest, a discovery failure is propagated rather than treated as an
/// empty inventory. An adapter that simply is not installed already reports an
/// empty list of its own accord; an error here means the store is there and
/// could not be read, which is not the same as the user having no
/// conversations.
pub fn prepare<'a>(adapters: &'a [Box<dyn AgentAdapter>]) -> Result<Vec<Prepared<'a>>> {
    let mut out = Vec::with_capacity(adapters.len());
    for adapter in adapters {
        if adapter.auto_unsupported_reason().is_some() {
            continue;
        }
        let sessions = adapter
            .discover()
            .with_context(|| format!("discover {} conversations", adapter.kind().as_str()))?;
        let index = TranscriptIndex::of(&sessions);
        out.push(Prepared {
            adapter: adapter.as_ref(),
            sessions,
            index,
        });
    }
    Ok(out)
}

/// Binds `probe` to the single conversation the evidence agrees on, or
/// nothing.
///
/// Evidence is additive per adapter and must reach [`CONFIDENCE_THRESHOLD`]:
/// - the foreground command equals the adapter's binary name: **+0.4**
/// - the pane's agent lineage names exactly one conversation, by holding its
///   transcript open or by naming it in argv: **+0.5**
/// - a process in the pane's **agent lineage** holds one (and only one) of
///   that adapter's *discovered* transcripts open: **+0.5**
/// - that transcript's recorded project directory equals the pane's cwd:
///   **+0.1**
///
/// Two things make the middle signal evidence rather than coincidence, and
/// both were missing:
///
/// * the descriptor must belong to the same process lineage as the agent
///   identity — see [`agent_lineage_transcripts`], and the pane-running-A-with-
///   a-tail-on-B case it exists for;
/// * the open file must **be** a discovered transcript, matched by device and
///   inode, not merely have a transcript-shaped name — see
///   [`TranscriptIndex`].
///
/// An adapter osm does not capture automatically
/// ([`AgentAdapter::auto_unsupported_reason`]) is never a candidate: there is
/// nothing it could have checked, so a binding for it would be a guess with a
/// number attached.
///
/// If a lineage holds transcripts open for more than one distinct
/// conversation under the same adapter, that adapter's transcript signal is
/// ambiguous and does not count — better to under-score than to guess which
/// one the pane means. If more than one adapter reaches the threshold, that
/// is a tie between kinds and nothing is recorded either.
pub fn bind(probe: &PaneProbe, prepared: &[Prepared]) -> Option<Binding> {
    if prepared.is_empty() {
        return None;
    }

    let mut candidates: Vec<Binding> = Vec::new();

    for p in prepared {
        if p.adapter.auto_unsupported_reason().is_some() {
            continue;
        }
        let binary = p.adapter.kind().as_str();
        let mut score = 0.0_f32;
        if probe.foreground_cmd == binary {
            score += 0.4;
        }

        // Distinct conversations this adapter's lineage in this pane names,
        // whether by holding a transcript open or by saying so in its argv.
        //
        // Both are the process identifying itself, so both weigh the same and
        // neither binds alone -- the foreground command must still agree.
        // argv exists because open descriptors are not a signal Claude Code
        // provides: it appends and closes.
        let mut owned_ids: Vec<String> = Vec::new();
        for path in agent_lineage_transcripts(probe.pane_pid, binary) {
            if let Some(id) = p.index.id_of(&path) {
                if !owned_ids.iter().any(|seen| seen == id) {
                    owned_ids.push(id.to_string());
                }
            }
        }
        for id in argv_conversation_ids(probe.pane_pid, binary) {
            // Only a conversation this adapter actually knows about. An id in
            // argv that names nothing on disk is a typo or another tool's
            // flag, not a conversation to bind to.
            if p.sessions.iter().any(|s| s.native_id == id) && !owned_ids.contains(&id) {
                owned_ids.push(id);
            }
        }

        // Exactly one distinct conversation is the only unambiguous case.
        // Zero means no transcript evidence; two or more means the lineage
        // holds open conversations that disagree with each other, which is
        // not evidence for any single one of them.
        let Some(id) = (owned_ids.len() == 1).then(|| owned_ids.remove(0)) else {
            continue;
        };
        score += 0.5;

        if let Some(session) = p.sessions.iter().find(|s| s.native_id == id) {
            if session.project_dir.as_deref() == Some(probe.cwd.as_str()) {
                score += 0.1;
            }
        }

        if score >= CONFIDENCE_THRESHOLD {
            candidates.push(Binding {
                kind: p.adapter.kind(),
                native_id: id,
                confidence: score,
            });
        }
    }

    match candidates.len() {
        1 => candidates.pop(),
        // Zero: nothing reached the threshold. Two or more: a tie between
        // adapter kinds — recording either would be a guess.
        _ => None,
    }
}

/// Which conversation, if any, `probe`'s pane is running under `adapter`
/// specifically — the shape a resume needs to *confirm* what it just started.
///
/// A convenience over [`prepare`] + [`bind`] for a single adapter, kept here
/// so confirmation cannot drift from detection: it is the same scoring, the
/// same threshold, the same lineage and the same file-identity match capture
/// uses. A looser rule for confirmation would be a resume that reports success
/// on weaker evidence than the capture which recorded the binding.
pub fn bound_conversation(
    probe: &PaneProbe,
    adapter: &dyn AgentAdapter,
) -> Result<Option<Binding>> {
    let sessions = adapter
        .discover()
        .with_context(|| format!("discover {} conversations", adapter.kind().as_str()))?;
    let index = TranscriptIndex::of(&sessions);
    let prepared = [Prepared {
        adapter,
        sessions,
        index,
    }];
    Ok(bind(probe, &prepared))
}
