use crate::agent::resume::Outcome as AgentOutcome;
use crate::restore::RestoreReport;
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension};
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
    /// Whatever is in it is no longer reachable by `osm`, and nothing else
    /// would ever say so: `open` runs on every subcommand, so a user who
    /// upgraded and typed `osm status` is exactly the person who needs to be
    /// told.
    pub preserved: Option<PreservedDatabase>,
}

/// A database an incompatible schema version pushed aside.
///
/// `present`, `snapshots` and `error` are read from the file itself, so a
/// widget can render what is in it without opening anything: a count means
/// the file was read and holds that many snapshots (`0` included, which is a
/// fact and not an absence), a null count beside an `error` means it could
/// not be read at all, and `present: false` means the user has since removed
/// it. The three used to be one sentence claiming the snapshots were intact,
/// asserted without ever opening the file.
#[derive(Debug, Serialize)]
pub struct PreservedDatabase {
    pub path: String,
    /// The schema version it recorded, `null` if it had none.
    pub schema_version: Option<u32>,
    pub preserved_at: Option<i64>,
    /// Whether anything is at `path` now.
    pub present: bool,
    /// Snapshots counted in it, `null` when it could not be read.
    pub snapshots: Option<i64>,
    /// Why it could not be read, `null` when it was.
    pub error: Option<String>,
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
    /// The newest snapshot on record, or `null` when there has never been
    /// one. Never a fabricated stand-in: a widget that renders a snapshot
    /// that does not exist tells the user their state is safe when it is
    /// not.
    pub snapshot: Option<SnapshotSummary>,
    /// One entry per session in that snapshot, empty when there is no
    /// snapshot. This is what a session list renders, and it is deliberately
    /// the *recorded* state rather than the live one: the widget's subject
    /// is what osm would restore.
    pub sessions: Vec<SessionSummary>,
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
            snapshot: None,
            sessions: Vec::new(),
        }
    }

    /// Attach what [`summarize`] read out of the database.
    ///
    /// Separate from [`Self::new`] because the two have different failure
    /// modes: a status report is produced even when the database cannot be
    /// opened at all, and in that case there is nothing to attach — which
    /// is `None` and `[]`, exactly as it is for a database that opens and
    /// holds no snapshot.
    pub fn with_snapshot(
        mut self,
        snapshot: Option<SnapshotSummary>,
        sessions: Vec<SessionSummary>,
    ) -> Self {
        self.snapshot = snapshot;
        self.sessions = sessions;
        self
    }
}

/// One session in the newest snapshot, as a menu row.
///
/// `windows` and `panes` are that session's own counts. A window linked into
/// two sessions is counted in both, because it really is in both — tmux
/// shows it in each, and so must a list of what a session contains.
///
/// `workspace` and `monitor` are `Option` and mean exactly "unknown" when
/// absent: no placement was recorded for this session, which is the ordinary
/// case on a machine with no compositor. They are never filled in with a
/// default, because "unknown workspace" and "workspace 1" are different
/// facts and only one of them is true.
#[derive(Debug, Serialize)]
pub struct SessionSummary {
    pub name: String,
    pub windows: usize,
    pub panes: usize,
    /// Panes recorded with `restore_policy = 'agent_resume'` — the ones a
    /// restore would hand a resume command to. A pane running an agent osm
    /// could not bind is not one of them, and is not counted.
    pub agents: usize,
    pub workspace: Option<String>,
    pub monitor: Option<String>,
    /// What this session is about, or `null` when nothing in it can say.
    ///
    /// See [`SessionGoal`] for which conversation it is taken from and why.
    pub goal: Option<SessionGoal>,
    /// Every conversation this session held, newest first — the detail behind
    /// the goal. Empty for a session with no agent pane, and never absent: a
    /// reader indexes it unconditionally, and a missing key and an empty list
    /// are different bugs of which only one is visible.
    pub conversations: Vec<SessionConversation>,
}

/// A session's goal: one real title, attributed to the conversation that
/// carries it.
///
/// **The title of the most recently active conversation in the session that
/// has one.** Not a summary of several — osm has no business inventing a
/// sentence nobody wrote — and not the title of the busiest or the biggest,
/// because after a reboot what a person wants back is what they were doing
/// last. A conversation that has no title is skipped rather than allowed to
/// blank the row; the whole list is in `conversations`, in the same order, so
/// the choice can be seen rather than trusted.
///
/// `kind` and `native_id` travel with the text so it can never be read
/// against the wrong conversation, and `source` says whether the agent wrote
/// it or osm derived it from the user's first prompt.
#[derive(Debug, Serialize)]
pub struct SessionGoal {
    pub title: String,
    pub source: String,
    pub kind: String,
    pub native_id: String,
}

/// One conversation a session held, and where it was.
///
/// `window_idx` is the window's index *within this session* and `pane_idx`
/// the pane's within that window — the identity a restore preserves, and the
/// only one that still means anything after a reboot has renumbered every
/// tmux id.
#[derive(Debug, Serialize)]
pub struct SessionConversation {
    pub kind: String,
    pub native_id: String,
    /// `null` when osm could derive no title, which a reader draws as
    /// *untitled* and never as a blank.
    pub title: Option<String>,
    pub title_source: Option<String>,
    pub window_idx: i64,
    pub pane_idx: i64,
    pub last_active: Option<i64>,
}

/// The newest snapshot, as a freshness line.
///
/// `state` is the raw `snapshots.state` token (`complete`, `restored`,
/// `restore_in_progress`, `failed`), not a rendering of it. `age_secs` is
/// clamped at zero: a clock that went backwards must not produce a snapshot
/// taken in the future.
#[derive(Debug, Serialize)]
pub struct SnapshotSummary {
    pub id: i64,
    pub taken_at: i64,
    pub age_secs: i64,
    pub state: String,
    pub sessions: usize,
}

// A seam for `tests::a_concurrent_capture_and_prune_cannot_split_the_summary`,
// compiled only for this crate's own tests.
//
// The defect it exists to reproduce is a race — a capture committing and
// retention deleting between the two reads below — and a race reproduced by
// two threads racing is a test that passes on the days it loses. This runs the
// other process's work at the exact instant that matters, once, and then
// forgets it, so the interleaving is the same on every machine.
#[cfg(test)]
thread_local! {
    static BETWEEN_READS: std::cell::RefCell<Option<Box<dyn Fn()>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn between_reads() {
    let hook = BETWEEN_READS.with(|h| h.borrow_mut().take());
    if let Some(run) = hook {
        run();
    }
}

#[cfg(not(test))]
fn between_reads() {}

/// Everything `osm status` reports out of the database.
///
/// One value rather than several returns, because these fields are read as
/// one answer: a widget puts `snapshots` and `snapshot` on the same line, and
/// a count from one moment beside a snapshot from another is a sentence the
/// database never said.
#[derive(Debug)]
pub struct DatabaseSummary {
    /// Every snapshot on record, `building` ones included — this is the size
    /// of the archive, not the size of what is restorable.
    pub snapshots: i64,
    /// When the newest of them was taken, `None` for an empty database.
    pub newest_snapshot_at: Option<i64>,
    /// A database an incompatible schema version pushed aside, if there is
    /// one.
    pub preserved: Option<crate::db::Preserved>,
    /// The newest snapshot worth restoring, or `None`.
    pub snapshot: Option<SnapshotSummary>,
    /// Its sessions, empty when there is no snapshot.
    pub sessions: Vec<SessionSummary>,
}

/// Read everything `osm status` says about the database, in one place.
///
/// `building` rows are skipped when choosing the newest snapshot. Such a row
/// describes a topology still being written, so its counts are low by
/// construction; reporting it would show the user a session list that shrinks
/// and grows as captures run.
///
/// The whole summary is **one read transaction**, and every field of it comes
/// from inside that transaction. Separate queries are separate moments: `osm
/// status` runs while the daemon is capturing and nothing serialises the two,
/// so a capture can commit and retention can delete the chosen snapshot
/// between any two of them. What comes back is then stitched together from
/// two different databases — a snapshot id from one moment with a session
/// list from another (an existing snapshot reported with zero sessions), or a
/// count from one moment with a snapshot from another (`"snapshots": 0`
/// beside a snapshot out of them). A read transaction pins the database as it
/// was when the first read ran, so every field of the answer describes the
/// same instant.
///
/// `titles` is the user's `privacy.prompt_titles`, and it is read here rather
/// than only where a title is derived. Under
/// [`crate::agent::title::Policy::AgentOnly`] a prompt-derived title already
/// on record is **not reported**: the switch is a revocation, not merely a
/// rule for the next capture. Capture clears those rows as well (see
/// `crate::capture`), but a capture may be minutes away on a quiet machine and
/// the panel polls this every five seconds — so the suppression has to happen
/// at the moment the setting is read, not at the moment the store is next
/// written.
pub fn summarize(
    conn: &Connection,
    now: i64,
    titles: crate::agent::title::Policy,
) -> Result<DatabaseSummary> {
    // DEFERRED, so the read snapshot is taken by the first query below and
    // held until this value is dropped. `unchecked_transaction` is the form
    // that works from a `&Connection`; it would fail if the caller were
    // already inside a transaction, and no caller is — `osm status` opens the
    // database for this one read and nothing else writes on this handle.
    // Read-only, so the rollback on drop is a no-op.
    let read = conn.unchecked_transaction()?;
    let newest = read
        .query_row(
            "SELECT id, taken_at, state FROM snapshots
             WHERE state <> 'building'
             ORDER BY taken_at DESC, id DESC
             LIMIT 1",
            [],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                ))
            },
        )
        .optional()?;

    between_reads();

    // Inside the transaction, with everything else. Read on its own — which
    // is where `main` used to read it, just before calling this — it was a
    // second moment: a capture committing in the gap produced `"snapshots":
    // 0` beside a `"snapshot"` object with an id in it, an empty archive
    // holding something.
    let (snapshots, newest_snapshot_at) =
        read.query_row("SELECT COUNT(*), MAX(taken_at) FROM snapshots", [], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, Option<i64>>(1)?))
        })?;

    let preserved = crate::db::preserved(&read);
    let Some((id, taken_at, state)) = newest else {
        return Ok(DatabaseSummary {
            snapshots,
            newest_snapshot_at,
            preserved,
            snapshot: None,
            sessions: Vec::new(),
        });
    };
    let sessions = session_summaries(&read, id, titles)?;
    Ok(DatabaseSummary {
        snapshots,
        newest_snapshot_at,
        preserved,
        snapshot: Some(SnapshotSummary {
            id,
            taken_at,
            age_secs: (now - taken_at).max(0),
            state,
            sessions: sessions.len(),
        }),
        sessions,
    })
}

/// Every session in one snapshot, with its counts and its recorded placement.
fn session_summaries(
    conn: &Connection,
    snapshot_id: i64,
    titles: crate::agent::title::Policy,
) -> Result<Vec<SessionSummary>> {
    // Placement is read separately and joined in memory rather than as a
    // correlated subquery: a session can have more than one terminal window
    // recorded, and `workspace` is one value. Ordering by `row_id` and
    // keeping the first makes which one deterministic instead of whichever
    // the planner reached first.
    let mut places = conn.prepare(
        "SELECT session_row_id, session_name, workspace_ref, monitor_connector
         FROM terminal_windows
         WHERE snapshot_id = ?1
         ORDER BY row_id",
    )?;
    let mut by_row: std::collections::HashMap<i64, (Option<String>, Option<String>)> =
        std::collections::HashMap::new();
    let mut by_name: std::collections::HashMap<String, (Option<String>, Option<String>)> =
        std::collections::HashMap::new();
    let rows = places.query_map([snapshot_id], |r| {
        Ok((
            r.get::<_, Option<i64>>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
        ))
    })?;
    for row in rows {
        let (session_row_id, session_name, workspace, monitor) = row?;
        // The columns are `NOT NULL`, so a value that was never known is
        // stored as the empty string. Empty is not a workspace name.
        let place = (
            (!workspace.is_empty()).then_some(workspace),
            (!monitor.is_empty()).then_some(monitor),
        );
        if let Some(row_id) = session_row_id {
            by_row.entry(row_id).or_insert_with(|| place.clone());
        } else if !session_name.is_empty() {
            // A window whose session vanished mid-capture still records the
            // name it was attached to; match on that rather than drop it.
            by_name.entry(session_name).or_insert(place);
        }
    }
    drop(places);

    // Every conversation in the snapshot, with what it is about, in one query
    // rather than one per session — and inside the transaction with
    // everything else here, because a session's counts and its goal read from
    // two different moments is a sentence the database never said.
    //
    // A LEFT JOIN: a pane may name a conversation `agent_sessions` has never
    // heard of — one carried forward from a snapshot taken before titles
    // existed, or bound while the agent's store could not be read — and that
    // is a conversation with no title, not a conversation to drop.
    let mut stmt = conn.prepare(
        "SELECT l.session_row_id, l.idx, p.idx, p.agent_kind, p.agent_session_id,
                a.title, a.title_source, a.last_active
           FROM session_rows s
           JOIN session_window_links l ON l.session_row_id = s.row_id
           JOIN pane_rows p ON p.window_row_id = l.window_row_id
      LEFT JOIN agent_sessions a
             ON a.kind = p.agent_kind AND a.native_id = p.agent_session_id
          WHERE s.snapshot_id = ?1
            AND p.agent_kind IS NOT NULL
            AND p.agent_session_id IS NOT NULL
       ORDER BY l.session_row_id, l.idx, p.idx",
    )?;
    let mut by_session: std::collections::HashMap<i64, Vec<SessionConversation>> =
        std::collections::HashMap::new();
    let rows = stmt.query_map([snapshot_id], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            SessionConversation {
                window_idx: r.get::<_, i64>(1)?,
                pane_idx: r.get::<_, i64>(2)?,
                kind: r.get::<_, String>(3)?,
                native_id: r.get::<_, String>(4)?,
                title: r.get::<_, Option<String>>(5)?,
                title_source: r.get::<_, Option<String>>(6)?,
                last_active: r.get::<_, Option<i64>>(7)?,
            },
        ))
    })?;
    for row in rows {
        let (session_row_id, mut conversation) = row?;
        // The revocation, applied to what is being *reported* rather than only
        // to what is being derived. A stored `first_prompt` title was taken
        // from the user's own message; the setting that allowed that has since
        // been switched off, and a report that still carries it is the setting
        // having done nothing. Both fields go, because `title_source` is null
        // exactly when `title` is — a bare source with no text would be a shape
        // no reader has ever been given.
        if titles == crate::agent::title::Policy::AgentOnly
            && conversation.title_source.as_deref()
                == Some(crate::agent::title::Source::FirstPrompt.as_str())
        {
            conversation.title = None;
            conversation.title_source = None;
        }
        let held = by_session.entry(session_row_id).or_default();
        // The same conversation in two panes of one session is one
        // conversation; the pane kept is the first, which this query's order
        // makes the earliest window and pane.
        if held.iter().any(|c: &SessionConversation| {
            c.kind == conversation.kind && c.native_id == conversation.native_id
        }) {
            continue;
        }
        held.push(conversation);
    }
    for held in by_session.values_mut() {
        // Newest first. A conversation with no recorded activity sorts last
        // rather than ahead of everything, which is what a plain descending
        // sort on `Option` would do; ties break on the id so two captures of
        // the same snapshot answer the same way.
        held.sort_by(|a, b| {
            b.last_active
                .unwrap_or(i64::MIN)
                .cmp(&a.last_active.unwrap_or(i64::MIN))
                .then_with(|| a.native_id.cmp(&b.native_id))
        });
    }
    drop(stmt);

    let mut stmt = conn.prepare(
        "SELECT s.row_id, s.name,
                (SELECT COUNT(*) FROM session_window_links l
                  WHERE l.session_row_id = s.row_id),
                (SELECT COUNT(*) FROM session_window_links l
                   JOIN pane_rows p ON p.window_row_id = l.window_row_id
                  WHERE l.session_row_id = s.row_id),
                (SELECT COUNT(*) FROM session_window_links l
                   JOIN pane_rows p ON p.window_row_id = l.window_row_id
                  WHERE l.session_row_id = s.row_id
                    AND p.restore_policy = 'agent_resume')
         FROM session_rows s
         WHERE s.snapshot_id = ?1
         ORDER BY s.name, s.row_id",
    )?;
    let rows = stmt.query_map([snapshot_id], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, i64>(3)?,
            r.get::<_, i64>(4)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (row_id, name, windows, panes, agents) = row?;
        let (workspace, monitor) = by_row
            .get(&row_id)
            .or_else(|| by_name.get(&name))
            .cloned()
            .unwrap_or((None, None));
        let conversations = by_session.remove(&row_id).unwrap_or_default();
        // The newest conversation that has something to say. Skipping the
        // untitled ones is not the same as inventing a title for them: they
        // are all in `conversations`, in this order, saying so.
        let goal = conversations.iter().find_map(|c| {
            c.title.as_ref().map(|title| SessionGoal {
                title: title.clone(),
                source: c.title_source.clone().unwrap_or_default(),
                kind: c.kind.clone(),
                native_id: c.native_id.clone(),
            })
        });
        out.push(SessionSummary {
            name,
            windows: windows as usize,
            panes: panes as usize,
            agents: agents as usize,
            workspace,
            monitor,
            goal,
            conversations,
        });
    }
    Ok(out)
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

/// What a restore did about one session's terminal window.
///
/// `outcome` is a stable token from [`crate::desktop::PlaceOutcome::as_str`]:
/// `placed`, `misplaced`, `never_mapped`, `never_attached`, `spawn_failed`,
/// `no_compositor`, `lost_compositor`, `skipped`, `placement_disabled`,
/// `placement_carried`, `placement_unknown`. Everything except `placed`,
/// `placement_disabled` and `placement_carried` means the restore is
/// `partial` and the snapshot stays restorable.
///
/// Two of them are reported against the pseudo-session `*`, because they are
/// statements about the pass rather than about one window:
/// `placement_carried` (the source snapshot's placement was unknown, so the
/// layout came from an earlier snapshot of the same boot — which one is in
/// `detail`) and `placement_unknown` (it was unknown and nothing earlier knew
/// either, so no window was put back).
///
/// `placed` means the compositor was asked where the window is and said it is
/// on the workspace and monitor the capture recorded. It never means "the
/// dispatch was accepted": `hyprctl dispatch` answers `ok` for a call it
/// took, and two terminals once came back on the active workspace with `ok`
/// from every dispatch and `placed` on both. A window that was dispatched at
/// and did not end up there is `misplaced`, with `detail` naming where it
/// actually is.
///
/// `placed` is the durable claim, not a momentary one: a session only keeps it
/// if the snapshot the restore publishes afterwards still records **that**
/// window — the one this restore spawned, holding that session, on the
/// workspace and monitor it was sent to. Another terminal the user has open
/// on the same session does not stand in for it. A window that was placed and
/// then went away, changed session, or was moved elsewhere downgrades the
/// whole run to `partial`, because the source snapshot is the only remaining
/// record of where it belonged.
#[derive(Debug, Serialize)]
pub struct RestoreWindow {
    pub session: String,
    pub outcome: String,
    pub detail: Option<String>,
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
    /// Every session with a recorded terminal window, and what this restore
    /// did about it. Empty when the source snapshot recorded no placement —
    /// which is every snapshot taken on a machine with no compositor, and was
    /// every snapshot at all until capture started asking one.
    pub windows: Vec<RestoreWindow>,
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
            windows: Vec::new(),
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
            windows: report
                .outcome
                .window_outcomes
                .iter()
                .map(|(session, o)| RestoreWindow {
                    session: session.clone(),
                    outcome: o.as_str().to_string(),
                    detail: o.detail().map(str::to_string),
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
/// `store_path` is a path, never transcript *contents*. `title` is the one
/// place that rule is relaxed and only ever by one short line: see
/// [`crate::agent::title`] and the design's privacy note, which state what a
/// title may be derived from, what it may never carry, and how to switch the
/// relaxation off.
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
    /// One line saying what this conversation is about, or `null` when osm
    /// could derive none. Never a stand-in: a reader that draws *untitled*
    /// for `null` says something true, and one that draws a fabricated title
    /// does not.
    pub title: Option<String>,
    /// Where `title` came from — `agent` (the agent's own name for the
    /// conversation) or `first_prompt` (one truncated line of the user's
    /// opening message). `null` exactly when `title` is.
    pub title_source: Option<String>,
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
        let title_of =
            |kind: crate::agent::AgentKind, id: &str| inv.titles.get(&(kind, id.to_string()));
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
                title: title_of(l.kind, &l.native_id).map(|t| t.text.clone()),
                title_source: title_of(l.kind, &l.native_id).map(|t| t.source.as_str().to_string()),
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
                title: title_of(s.kind, &s.native_id).map(|t| t.text.clone()),
                title_source: title_of(s.kind, &s.native_id).map(|t| t.source.as_str().to_string()),
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

#[cfg(test)]
mod tests {
    //! The one part of [`summarize`] no integration test can reach: what it
    //! does with a recorded terminal window.
    //!
    //! Writing one takes a compositor, and these tests must not ask the
    //! developer's — nor can CI's container answer. So the rows are written
    //! here, directly, against a real database with the real schema.

    use super::*;
    use crate::db;

    /// A snapshot with one session, one window and `panes` panes, of which
    /// `agents` are recorded as resumable conversations. Returns the
    /// snapshot id and the session row id.
    fn fixture(conn: &Connection, name: &str, panes: usize, agents: usize) -> (i64, i64) {
        conn.execute(
            "INSERT INTO snapshots (taken_at, boot_id, reason, state, server)
             VALUES (100, 'boot', 'test', 'complete', 'srv')",
            [],
        )
        .unwrap();
        let snapshot_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO session_rows (snapshot_id, tmux_session_id, name)
             VALUES (?1, '$0', ?2)",
            rusqlite::params![snapshot_id, name],
        )
        .unwrap();
        let session_row_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO window_rows (snapshot_id, tmux_window_id, name, layout)
             VALUES (?1, '@0', 'w', 'layout')",
            rusqlite::params![snapshot_id],
        )
        .unwrap();
        let window_row_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO session_window_links (session_row_id, window_row_id, idx, active)
             VALUES (?1, ?2, 0, 1)",
            rusqlite::params![session_row_id, window_row_id],
        )
        .unwrap();
        for i in 0..panes {
            let policy = if i < agents { "agent_resume" } else { "shell" };
            conn.execute(
                "INSERT INTO pane_rows (window_row_id, tmux_pane_id, idx, cwd, restore_policy)
                 VALUES (?1, ?2, ?3, '/tmp', ?4)",
                rusqlite::params![window_row_id, format!("%{i}"), i as i64, policy],
            )
            .unwrap();
        }
        (snapshot_id, session_row_id)
    }

    fn place(
        conn: &Connection,
        snapshot_id: i64,
        session_row_id: Option<i64>,
        session_name: &str,
        address: &str,
        workspace: &str,
        monitor: &str,
    ) {
        conn.execute(
            "INSERT INTO terminal_windows
               (snapshot_id, hypr_address, window_class, terminal_kind, session_row_id,
                session_name, workspace_kind, workspace_ref, monitor_connector)
             VALUES (?1, ?2, 'kitty', 'kitty', ?3, ?4, 'numbered', ?5, ?6)",
            rusqlite::params![
                snapshot_id,
                address,
                session_row_id,
                session_name,
                workspace,
                monitor
            ],
        )
        .unwrap();
    }

    fn open() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let conn = db::open(&dir.path().join("state.db")).unwrap();
        (dir, conn)
    }

    #[test]
    fn a_recorded_placement_reaches_the_session_row() {
        let (_dir, conn) = open();
        let (snapshot_id, session_row_id) = fixture(&conn, "dev", 3, 1);
        place(
            &conn,
            snapshot_id,
            Some(session_row_id),
            "dev",
            "0xaa",
            "4",
            "DP-1",
        );

        let summary =
            summarize(&conn, 160, crate::agent::title::Policy::AgentOrFirstPrompt).unwrap();
        let (snap, sessions) = (summary.snapshot, summary.sessions);
        let snap = snap.unwrap();
        assert_eq!(snap.id, snapshot_id);
        assert_eq!(snap.age_secs, 60);
        assert_eq!(snap.sessions, 1);
        assert_eq!(sessions[0].windows, 1);
        assert_eq!(sessions[0].panes, 3);
        assert_eq!(sessions[0].agents, 1);
        assert_eq!(sessions[0].workspace.as_deref(), Some("4"));
        assert_eq!(sessions[0].monitor.as_deref(), Some("DP-1"));
    }

    /// The window whose session vanished between the tmux read and the
    /// compositor read has no row to link to, only a name. Dropping it would
    /// leave the session looking as though it had never been placed.
    #[test]
    fn a_placement_with_no_row_link_is_matched_by_name() {
        let (_dir, conn) = open();
        let (snapshot_id, _) = fixture(&conn, "dev", 1, 0);
        place(&conn, snapshot_id, None, "dev", "0xbb", "7", "HDMI-A-1");

        let sessions = summarize(&conn, 100, crate::agent::title::Policy::AgentOrFirstPrompt)
            .unwrap()
            .sessions;
        assert_eq!(sessions[0].workspace.as_deref(), Some("7"));
        assert_eq!(sessions[0].monitor.as_deref(), Some("HDMI-A-1"));
    }

    /// Two windows for one session is one row in a menu. Which one it shows
    /// must not depend on the query planner.
    #[test]
    fn two_windows_for_one_session_report_the_first_recorded() {
        let (_dir, conn) = open();
        let (snapshot_id, session_row_id) = fixture(&conn, "dev", 1, 0);
        place(
            &conn,
            snapshot_id,
            Some(session_row_id),
            "dev",
            "0x01",
            "2",
            "DP-1",
        );
        place(
            &conn,
            snapshot_id,
            Some(session_row_id),
            "dev",
            "0x02",
            "9",
            "DP-2",
        );

        let sessions = summarize(&conn, 100, crate::agent::title::Policy::AgentOrFirstPrompt)
            .unwrap()
            .sessions;
        assert_eq!(sessions[0].workspace.as_deref(), Some("2"));
    }

    /// The columns are `NOT NULL`, so "never known" is stored as the empty
    /// string. Rendering that as a workspace named "" would be a lie with a
    /// shorter name than usual.
    #[test]
    fn an_empty_recorded_workspace_is_unknown_not_a_workspace() {
        let (_dir, conn) = open();
        let (snapshot_id, session_row_id) = fixture(&conn, "dev", 1, 0);
        place(
            &conn,
            snapshot_id,
            Some(session_row_id),
            "dev",
            "0xcc",
            "",
            "",
        );

        let sessions = summarize(&conn, 100, crate::agent::title::Policy::AgentOrFirstPrompt)
            .unwrap()
            .sessions;
        assert!(sessions[0].workspace.is_none());
        assert!(sessions[0].monitor.is_none());
    }

    /// A snapshot still being written is not the newest snapshot: its rows
    /// are incomplete by construction and its counts would be wrong.
    #[test]
    fn a_building_snapshot_is_not_reported() {
        let (_dir, conn) = open();
        let (snapshot_id, _) = fixture(&conn, "dev", 2, 0);
        conn.execute(
            "INSERT INTO snapshots (taken_at, boot_id, reason, state, server)
             VALUES (200, 'boot', 'test', 'building', 'srv')",
            [],
        )
        .unwrap();

        let summary =
            summarize(&conn, 300, crate::agent::title::Policy::AgentOrFirstPrompt).unwrap();
        assert_eq!(summary.snapshot.unwrap().id, snapshot_id);
        assert_eq!(summary.sessions.len(), 1);
        assert_eq!(
            summary.snapshots, 2,
            "the count is of every snapshot on record, `building` included"
        );
    }

    /// A clock that went backwards must not produce a snapshot from the
    /// future, which a widget would render as a negative age.
    #[test]
    fn age_never_goes_negative() {
        let (_dir, conn) = open();
        fixture(&conn, "dev", 1, 0);
        let snap = summarize(&conn, 1, crate::agent::title::Policy::AgentOrFirstPrompt)
            .unwrap()
            .snapshot;
        assert_eq!(snap.unwrap().age_secs, 0);
    }

    /// A capture that commits, and a retention pass that deletes the very
    /// snapshot this summary had already chosen, in the gap between the two
    /// reads.
    ///
    /// `osm status` runs while the daemon is capturing; nothing serialises the
    /// two. Read outside a transaction, the summary is stitched together from
    /// two different database moments, and the seam is where the lie forms:
    /// the identity of a snapshot that no longer exists, with the session list
    /// — and the placement — of the moment after it was deleted. That renders
    /// as "snapshot #N, 0 sessions", which the widget shows as a machine with
    /// nothing recorded on it. The user's snapshot was fine; the read was not.
    ///
    /// The fix is that the whole summary is one read transaction, so the
    /// second read sees the database as it was when the first one ran.
    #[test]
    fn a_concurrent_capture_and_prune_cannot_split_the_summary() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let (dir, conn) = open();
        let (snapshot_id, session_row_id) = fixture(&conn, "dev", 3, 1);
        place(
            &conn,
            snapshot_id,
            Some(session_row_id),
            "dev",
            "0xaa",
            "4",
            "DP-1",
        );

        let db_path = dir.path().join("state.db");
        let fired = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&fired);
        BETWEEN_READS.with(|h| {
            *h.borrow_mut() = Some(Box::new(move || {
                // A second osm process, on its own connection: one capture
                // lands, and retention drops the snapshot before it.
                let other = db::open(&db_path).unwrap();
                fixture(&other, "later", 1, 0);
                other
                    .execute("DELETE FROM snapshots WHERE id = ?1", [snapshot_id])
                    .unwrap();
                flag.store(true, Ordering::SeqCst);
            }));
        });

        let summary =
            summarize(&conn, 160, crate::agent::title::Policy::AgentOrFirstPrompt).unwrap();
        let (snap, sessions) = (summary.snapshot, summary.sessions);
        assert!(
            fired.load(Ordering::SeqCst),
            "the interleaving never ran, so this test proved nothing"
        );

        let snap = snap.expect("a snapshot was on record when the summary began");
        assert_eq!(
            snap.id, snapshot_id,
            "the summary must describe the snapshot it chose"
        );
        assert_eq!(
            sessions.len(),
            1,
            "snapshot #{} is reported with {} sessions; at the moment it was \
             chosen it had 1 — the two reads came from different moments",
            snap.id,
            sessions.len()
        );
        assert_eq!(snap.sessions, 1);
        assert_eq!(sessions[0].panes, 3);
        assert_eq!(
            sessions[0].workspace.as_deref(),
            Some("4"),
            "placement was read from a different database moment"
        );
    }

    /// An empty archive never reports a snapshot out of it.
    ///
    /// `osm status` runs while the daemon is capturing, and nothing
    /// serialises the two. The count used to be an autocommit query of its
    /// own, taken just before the read transaction that produced everything
    /// else, so a capture committing in that gap put two different moments
    /// into one response: `"snapshots": 0` beside a `"snapshot"` object with
    /// an id in it. The archive is empty and here is something out of it —
    /// "unknown rendered as no", which is the defect class this project keeps
    /// having to close.
    ///
    /// The interleaving is run once, at the instant that matters, by the same
    /// seam `a_concurrent_capture_and_prune_cannot_split_the_summary` uses. A
    /// race reproduced by two threads racing is a test that passes on the
    /// days it loses.
    #[test]
    fn an_empty_archive_never_reports_a_snapshot_out_of_it() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        // Empty on purpose: this is the machine that has just been installed,
        // whose first capture lands while the widget is asking.
        let (dir, conn) = open();

        let db_path = dir.path().join("state.db");
        let fired = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&fired);
        BETWEEN_READS.with(|h| {
            *h.borrow_mut() = Some(Box::new(move || {
                let other = db::open(&db_path).unwrap();
                fixture(&other, "dev", 2, 0);
                flag.store(true, Ordering::SeqCst);
            }));
        });

        let summary =
            summarize(&conn, 160, crate::agent::title::Policy::AgentOrFirstPrompt).unwrap();
        assert!(
            fired.load(Ordering::SeqCst),
            "the interleaving never ran, so this test proved nothing"
        );

        assert!(
            summary.snapshot.is_none() || summary.snapshots > 0,
            "the response reports {} snapshot(s) on record and snapshot #{} \
             out of them; the count and the snapshot came from two different \
             moments",
            summary.snapshots,
            summary.snapshot.as_ref().map_or(-1, |s| s.id)
        );
        assert!(
            summary.sessions.is_empty() || summary.snapshots > 0,
            "the response reports {} snapshot(s) on record and {} session(s) \
             recorded in one of them",
            summary.snapshots,
            summary.sessions.len()
        );
        assert_eq!(
            summary.newest_snapshot_at,
            summary.snapshot.as_ref().map(|s| s.taken_at),
            "the newest recorded time and the snapshot reported disagree"
        );

        // And the whole answer is the database as it was when the request
        // began, rather than a mixture: nothing had been captured yet.
        assert_eq!(summary.snapshots, 0);
        assert!(summary.snapshot.is_none());
    }

    /// The other direction of the same defect: a count that could not be read
    /// is not zero.
    ///
    /// Reading it used to be `counts.unwrap_or((0, None))`, so a query that
    /// failed reported an empty archive — a widget would show a machine with
    /// nothing recorded on it, over a database full of snapshots. There is no
    /// count to report when the read failed, and the whole summary now fails
    /// together so the caller has to say so.
    #[test]
    fn a_summary_that_cannot_be_read_fails_rather_than_reporting_zero() {
        let (_dir, conn) = open();
        fixture(&conn, "dev", 1, 0);
        conn.execute("DROP TABLE snapshots", []).unwrap();

        let err = summarize(&conn, 100, crate::agent::title::Policy::AgentOrFirstPrompt)
            .expect_err("an unreadable database is an error");
        assert!(
            format!("{err:#}").contains("snapshots"),
            "the error must name what could not be read: {err:#}"
        );
    }

    #[test]
    fn an_empty_database_reports_no_snapshot_and_no_sessions() {
        let (_dir, conn) = open();
        let summary =
            summarize(&conn, 100, crate::agent::title::Policy::AgentOrFirstPrompt).unwrap();
        assert!(summary.snapshot.is_none());
        assert!(summary.sessions.is_empty());
        assert_eq!(summary.snapshots, 0);
        assert!(summary.newest_snapshot_at.is_none());
        assert!(summary.preserved.is_none());
    }
}
