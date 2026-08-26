use crate::agent::resume::Outcome as AgentOutcome;
use crate::restore::RestoreReport;
use serde::Serialize;

pub const PROTOCOL_VERSION: u32 = 1;

/// The process exit status that goes with a `RestoreJson.state`.
///
/// `osm restore` used to exit 0 on every path, so `osm-restore.service` was
/// reported as a clean success whether the user's sessions came back or not.
/// Only the outcomes that either restored everything *and secured it* or had
/// nothing to restore are a success; a partial restore, an outright failure, a
/// restore that never got the lock, and one whose replacement snapshot could
/// not be written all leave work undone.
pub fn exit_code_for_state(state: &str) -> i32 {
    match state {
        "succeeded" | "nothing_to_restore" | "dry_run" => 0,
        _ => 1,
    }
}

/// Capture freshness, and whether captures are failing right now.
#[derive(Debug, Serialize)]
pub struct CaptureStatus {
    /// Epoch seconds of the last capture that wrote a snapshot.
    pub last_success_at: Option<i64>,
    /// Seconds since that capture, `null` if there has never been one.
    pub age_secs: Option<i64>,
    /// True when no successful capture has landed inside the freshness
    /// budget — including when there has never been one at all.
    pub stale: bool,
    pub stale_after_secs: u64,
    pub last_error: Option<String>,
    pub last_error_at: Option<i64>,
    /// Failures since the last success. Non-zero means captures are failing
    /// now, which an old `last_success_at` alone cannot distinguish from an
    /// idle machine.
    pub consecutive_failures: u32,
}

#[derive(Debug, Serialize)]
pub struct DatabaseStatus {
    pub path: String,
    pub reachable: bool,
    pub error: Option<String>,
    pub snapshots: Option<i64>,
    /// Epoch seconds of the newest snapshot on record.
    pub newest_snapshot_at: Option<i64>,
    /// Set when a database written under a different schema version was moved
    /// aside so this build could start a fresh one. `null` in the ordinary
    /// case.
    ///
    /// The snapshots in it are intact but no longer reachable by `osm`, and
    /// nothing else would ever say so: `open` runs on every subcommand, so a
    /// user who upgraded and typed `osm status` is exactly the person who
    /// needs to be told.
    pub preserved: Option<PreservedDatabase>,
}

/// A database an incompatible schema version pushed aside.
#[derive(Debug, Serialize)]
pub struct PreservedDatabase {
    pub path: String,
    /// The schema version it recorded, `null` if it had none.
    pub schema_version: Option<u32>,
    pub preserved_at: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct TmuxStatus {
    /// The `-L` socket in use, `null` for the default server.
    pub socket: Option<String>,
    pub reachable: bool,
    pub error: Option<String>,
}

/// One JSON object, `protocol_version` first.
///
/// `ready` used to be true whenever the config merely parsed, so a service
/// whose captures had been failing for a week reported itself healthy. It now
/// means "the engine can run": the config parses, the database opens, and osm
/// is willing to talk to this tmux at all (it refuses below 3.7 — see
/// [`crate::tmux::MIN_VERSION`] — and every capture and restore would fail).
/// Whether it is actually doing its job is `capture` — freshness and the
/// current failure streak — which a widget must read as well.
#[derive(Debug, Serialize)]
pub struct StatusReport {
    pub protocol_version: u32,
    pub engine_version: String,
    pub ready: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub capture: CaptureStatus,
    pub database: DatabaseStatus,
    pub tmux: TmuxStatus,
    pub agents: AgentSupport,
}

impl StatusReport {
    pub fn new(
        ready: bool,
        message: Option<String>,
        capture: CaptureStatus,
        database: DatabaseStatus,
        tmux: TmuxStatus,
        agents: AgentSupport,
    ) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            engine_version: env!("CARGO_PKG_VERSION").to_string(),
            ready,
            message,
            capture,
            database,
            tmux,
            agents,
        }
    }
}

/// One agent kind osm will not capture or resume by itself, and why.
#[derive(Debug, Serialize)]
pub struct UnsupportedAgent {
    pub kind: String,
    pub reason: String,
}

/// Which of the configured agents osm actually acts on.
///
/// `enabled` is what the config asks for; `unsupported` is the subset osm will
/// not bind a pane to or resume automatically, each with the reason a user is
/// shown. The two used to be indistinguishable, and one of them — OpenCode —
/// had an entire capture path that could never bind anything, with nothing
/// anywhere saying so. A capability osm does not have is now stated where the
/// user looks for capabilities.
#[derive(Debug, Serialize)]
pub struct AgentSupport {
    pub enabled: Vec<String>,
    pub unsupported: Vec<UnsupportedAgent>,
}

impl AgentSupport {
    pub fn of(enabled: &[String]) -> Self {
        let unsupported = crate::agent::adapters(enabled)
            .iter()
            .filter_map(|a| {
                a.auto_unsupported_reason().map(|reason| UnsupportedAgent {
                    kind: a.kind().as_str().to_string(),
                    reason: reason.to_string(),
                })
            })
            .collect();
        Self {
            enabled: enabled.to_vec(),
            unsupported,
        }
    }
}

/// One session whose restore returned an error.
#[derive(Debug, Serialize)]
pub struct RestoreFailure {
    pub session: String,
    pub error: String,
}

/// One session that exists on the destination under the captured name but
/// holds a different topology, so it was neither adopted nor overwritten.
#[derive(Debug, Serialize)]
pub struct RestoreConflict {
    pub session: String,
    pub detail: String,
}

/// One pane that could not be started in its captured working directory.
#[derive(Debug, Serialize)]
pub struct RestoreDegraded {
    pub session: String,
    pub window: String,
    pub pane_index: u32,
    pub captured_cwd: String,
    pub used_cwd: String,
}

/// One window rebuilt without its captured layout, because the captured
/// string did not parse as a tmux layout or tmux refused it. The window has
/// all of its panes; only the geometry is missing.
#[derive(Debug, Serialize)]
pub struct RestoreSkippedLayout {
    pub session: String,
    pub window: String,
    pub layout: String,
    pub reason: String,
}

/// One pane whose conversation was not resumed, and why.
///
/// Only the outcomes [`crate::restore::agent_outcomes_are_degraded`] treats
/// as a failure appear here — `active_elsewhere` and `unsupported` are
/// reported nowhere in this JSON, because leaving that pane as a shell was
/// the correct outcome, not a shortfall. `pane` is `"session:window.idx"`,
/// matching the labels [`crate::restore::resume_agents`] produces.
#[derive(Debug, Serialize)]
pub struct AgentResumeFailure {
    pub pane: String,
    pub reason: String,
}

/// The `osm restore` wire contract.
///
/// Every field is emitted on **every** exit path, including the ones that do
/// no work at all — a consumer may index `.created[]` or read `.attempt_id`
/// unconditionally without first branching on `state`. Nothing here is
/// `skip_serializing_if`; absent values are JSON `null` or `[]`.
///
/// `state` and `reason` are stable snake_case tokens, never prose:
///
/// | `state`              | `reason`                    | meaning                                        |
/// |----------------------|-----------------------------|------------------------------------------------|
/// | `succeeded`          | `ok`                        | every session created or verifiably adopted, and this boot's replacement snapshot published |
/// | `unsecured`          | `post_restore_capture_failed` | the sessions are back, but the replacement snapshot could not be written, so nothing durable records them; the source stays restorable |
/// | `partial`            | `partial_restore`           | some sessions restored, some did not, or some came back degraded |
/// | `failed`             | `restore_failed`            | nothing restored, or the attempt itself errored|
/// | `nothing_to_restore` | `no_previous_boot_snapshot` | no completed snapshot from an earlier boot     |
/// | `dry_run`            | `dry_run`                   | a source was selected but nothing was touched  |
/// | `blocked`            | `restore_lock_unavailable`  | the restore lock stayed held past the deadline |
///
/// The process exit status follows `state`: 0 for `succeeded`,
/// `nothing_to_restore` and `dry_run`; non-zero for `partial`, `failed`,
/// `unsecured` and `blocked`, so `osm-restore.service` reflects whether the
/// user's state actually came back *and* whether it is safe. See
/// [`exit_code_for_state`].
///
/// `unsecured` is deliberately not a flavour of `succeeded`. Reporting a
/// restore whose durability step failed as a success produced a document that
/// contradicted itself (`state: "succeeded"` beside `retryable: true`) and,
/// because the exit status followed the state, told systemd there was nothing
/// to restart — while the user's sessions existed only on a running tmux
/// server with no snapshot behind them.
///
/// The three distinct do-nothing causes are deliberately *not* collapsed into
/// one `"skipped"` state: "there is nothing to restore", "you asked me not to
/// restore", and "I could not get the lock in time" are different facts and a
/// widget must be able to tell them apart.
#[derive(Debug, Serialize)]
pub struct RestoreJson {
    pub protocol_version: u32,
    pub state: String,
    pub reason: String,
    pub snapshot_id: Option<i64>,
    pub attempt_id: Option<i64>,
    pub created: Vec<String>,
    pub adopted: Vec<String>,
    pub skipped: Vec<String>,
    pub failed: Vec<RestoreFailure>,
    /// Sessions whose name was already taken on the destination by a session
    /// with a *different* shape. Nothing was created and nothing was
    /// destroyed for these; the snapshot stays restorable.
    pub conflicts: Vec<RestoreConflict>,
    /// Panes started somewhere other than their captured directory because
    /// that directory did not exist. A non-empty list means `state` is
    /// `partial`, never `succeeded`.
    pub degraded: Vec<RestoreDegraded>,
    /// Windows rebuilt without their captured geometry. Like `degraded`, a
    /// non-empty list means `state` is `partial`, never `succeeded`: the
    /// snapshot holds the only copy of that layout and must stay retryable.
    pub skipped_layouts: Vec<RestoreSkippedLayout>,
    /// Whether the snapshot is still selectable, so a later `osm restore`
    /// will pick it up again. Only a fully verified success retires a
    /// snapshot; a partial or failed restore stays retryable, because a
    /// transient problem must not permanently cost the user the only copy of
    /// their pre-reboot state.
    pub retryable: bool,
    /// How many panes were handed a resume command and confirmed to have
    /// started it — [`crate::agent::resume::Outcome::Resumed`] only. Does not
    /// count a pane left alone because its conversation was already alive
    /// elsewhere, or one this run never attempted (auto-resume off, no
    /// adapter enabled for its kind, or its conversation too old to
    /// auto-resume).
    pub agents_resumed: usize,
    /// Every pane whose conversation should have come back but did not —
    /// the same set [`crate::restore::agent_outcomes_are_degraded`] uses to
    /// decide `state`. A non-empty list here on a `partial` restore is why
    /// it is `partial` rather than `succeeded`.
    pub agents_failed: Vec<AgentResumeFailure>,
}

impl RestoreJson {
    fn empty(state: &str, reason: &str) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            state: state.to_string(),
            reason: reason.to_string(),
            snapshot_id: None,
            attempt_id: None,
            created: Vec::new(),
            adopted: Vec::new(),
            skipped: Vec::new(),
            failed: Vec::new(),
            conflicts: Vec::new(),
            degraded: Vec::new(),
            skipped_layouts: Vec::new(),
            // Nothing was attempted, so whatever source exists is untouched.
            retryable: true,
            agents_resumed: 0,
            agents_failed: Vec::new(),
        }
    }

    /// The restore lock was still held when the wait budget ran out.
    ///
    /// Note the token: the holder may well have been a *capture* (the tmux
    /// hooks take the same lock), so this must never claim that another
    /// restore is running.
    pub fn lock_unavailable() -> Self {
        Self::empty("blocked", "restore_lock_unavailable")
    }

    pub fn from_report(report: &RestoreReport) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            state: report.state.clone(),
            reason: report.reason.clone(),
            snapshot_id: report.snapshot_id,
            attempt_id: report.attempt_id,
            created: report.outcome.created.clone(),
            adopted: report.outcome.adopted.clone(),
            skipped: report.outcome.skipped.clone(),
            failed: report
                .outcome
                .failed
                .iter()
                .map(|(session, error)| RestoreFailure {
                    session: session.clone(),
                    error: error.clone(),
                })
                .collect(),
            conflicts: report
                .outcome
                .conflicted
                .iter()
                .map(|(session, detail)| RestoreConflict {
                    session: session.clone(),
                    detail: detail.clone(),
                })
                .collect(),
            degraded: report
                .outcome
                .degraded
                .iter()
                .map(|d| RestoreDegraded {
                    session: d.session.clone(),
                    window: d.window.clone(),
                    pane_index: d.pane_index,
                    captured_cwd: d.captured.clone(),
                    used_cwd: d.used.clone(),
                })
                .collect(),
            skipped_layouts: report
                .outcome
                .skipped_layouts
                .iter()
                .map(|l| RestoreSkippedLayout {
                    session: l.session.clone(),
                    window: l.window.clone(),
                    layout: l.layout.clone(),
                    reason: l.reason.clone(),
                })
                .collect(),
            retryable: report.retryable,
            agents_resumed: report
                .outcome
                .agent_outcomes
                .iter()
                .filter(|(_, o)| matches!(o, AgentOutcome::Resumed))
                .count(),
            agents_failed: report
                .outcome
                .agent_outcomes
                .iter()
                .filter_map(|(pane, o)| match o {
                    AgentOutcome::Failed(reason) => Some(AgentResumeFailure {
                        pane: pane.clone(),
                        reason: reason.clone(),
                    }),
                    // `OwnershipUnknown` belongs here and not with the two
                    // below: nothing was delivered and a pane that should
                    // hold a conversation holds a shell, which is precisely
                    // what this list is for. Reporting it as "nothing to do"
                    // is how a restore came to call itself a success having
                    // sent nothing.
                    AgentOutcome::PaneBusy
                    | AgentOutcome::PaneMissing
                    | AgentOutcome::OwnershipUnknown => Some(AgentResumeFailure {
                        pane: pane.clone(),
                        reason: o.as_str().to_string(),
                    }),
                    AgentOutcome::Resumed
                    | AgentOutcome::ActiveElsewhere
                    | AgentOutcome::Unsupported => None,
                })
                .collect(),
        }
    }
}

/// One conversation in `osm agents --json`.
///
/// The same shape in both lists so a consumer can read one field set
/// regardless of which array an entry came from: `pane` and `confidence`
/// are `null` for a resumable conversation (nothing is running it, so there
/// is no pane and nothing was scored), and `alive` restates in one boolean
/// which list the entry is in, for a consumer that flattens them.
///
/// `store_path` is a path, never transcript *contents*: nothing in osm ever
/// reads what was said in a conversation (see the design's privacy note).
#[derive(Debug, Serialize)]
pub struct AgentEntry {
    pub kind: String,
    pub native_id: String,
    /// The tmux pane holding this conversation open right now, for a `live`
    /// entry; `null` in `resumable`.
    pub pane: Option<String>,
    /// What [`crate::agent::detect::bind`] scored this pane at. `null` in
    /// `resumable`: nothing was bound, so nothing was scored.
    pub confidence: Option<f32>,
    pub project_dir: Option<String>,
    pub store_path: Option<String>,
    pub last_active: Option<i64>,
    pub size_bytes: Option<i64>,
    pub alive: bool,
}

/// The `osm agents` wire contract: every conversation the enabled adapters
/// know about, split by whether a pane is running it.
///
/// The two arrays are disjoint. A live conversation is deliberately absent
/// from `resumable`: "resume" for one that is already running means
/// attaching a second client to it, which is exactly what
/// [`crate::agent::resume::Outcome::ActiveElsewhere`] refuses.
#[derive(Debug, Serialize)]
pub struct AgentsJson {
    pub protocol_version: u32,
    pub live: Vec<AgentEntry>,
    pub resumable: Vec<AgentEntry>,
    /// Agents osm could not read, and why — never silently dropped into an
    /// empty listing. An entry here means the two arrays above are incomplete
    /// for that kind, which is a different thing from that kind having no
    /// conversations.
    pub problems: Vec<AgentProblem>,
}

/// One agent osm could not read.
#[derive(Debug, Serialize)]
pub struct AgentProblem {
    pub kind: String,
    pub error: String,
}

impl AgentsJson {
    pub fn from_inventory(inv: &crate::agent::Inventory) -> Self {
        let live = inv
            .live
            .iter()
            .map(|l| AgentEntry {
                kind: l.kind.as_str().to_string(),
                native_id: l.native_id.clone(),
                pane: Some(l.pane_id.clone()),
                confidence: Some(l.confidence),
                project_dir: l.session.as_ref().and_then(|s| s.project_dir.clone()),
                store_path: l.session.as_ref().and_then(|s| s.store_path.clone()),
                last_active: l.session.as_ref().and_then(|s| s.last_active),
                size_bytes: l.session.as_ref().and_then(|s| s.size_bytes),
                alive: true,
            })
            .collect();
        let resumable = inv
            .resumable
            .iter()
            .map(|s| AgentEntry {
                kind: s.kind.as_str().to_string(),
                native_id: s.native_id.clone(),
                pane: None,
                confidence: None,
                project_dir: s.project_dir.clone(),
                store_path: s.store_path.clone(),
                last_active: s.last_active,
                size_bytes: s.size_bytes,
                alive: false,
            })
            .collect();
        Self {
            protocol_version: PROTOCOL_VERSION,
            live,
            resumable,
            problems: inv
                .problems
                .iter()
                .map(|(kind, error)| AgentProblem {
                    kind: kind.as_str().to_string(),
                    error: error.clone(),
                })
                .collect(),
        }
    }
}

/// The `osm resume` wire contract.
///
/// `outcome` is [`crate::agent::resume::Outcome::as_str`] — a stable
/// snake_case token, never the `Debug` form. `reason` carries the detail of
/// a `failed` outcome and is `null` for every other one.
///
/// The exit status follows `outcome`: 0 only for `resumed`. Every other
/// outcome means the conversation is not in the pane, which a caller
/// scripting this (a keybinding, a menu entry) must be able to see without
/// parsing the JSON — including `active_elsewhere`, which is a correct
/// refusal but still not the thing that was asked for.
#[derive(Debug, Serialize)]
pub struct ResumeJson {
    pub protocol_version: u32,
    pub kind: String,
    pub native_id: String,
    pub pane: String,
    pub outcome: String,
    pub reason: Option<String>,
}

impl ResumeJson {
    pub fn new(
        kind: crate::agent::AgentKind,
        native_id: &str,
        pane: &str,
        outcome: &AgentOutcome,
    ) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            kind: kind.as_str().to_string(),
            native_id: native_id.to_string(),
            pane: pane.to_string(),
            outcome: outcome.as_str().to_string(),
            reason: match outcome {
                AgentOutcome::Failed(reason) => Some(reason.clone()),
                _ => None,
            },
        }
    }
}
