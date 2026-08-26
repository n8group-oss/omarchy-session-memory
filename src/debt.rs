//! Pending resume debt: the conversations a restore has put a *pane* back for
//! without (yet) putting the conversation back into it.
//!
//! # Why this exists rather than a heuristic
//!
//! A capture that lands between a restore building its panes and its resumes
//! completing sees bare shells everywhere. Written down verbatim that erases
//! the only record of which conversation belonged in which pane — the record
//! the resume itself needs.
//!
//! The first answer to that compared the *set* of bound conversations between
//! two captures and refused the new map when more than half of them had gone.
//! It inferred the cause from a ratio, and the inference is wrong for the most
//! ordinary thing a user does: exit conversation A, start conversation B in
//! the same pane. That reads as 100% loss, so the correct fresh binding `{B}`
//! is thrown away and A is carried forward — for ever, since the next capture
//! sees the same thing. After a reboot, A is then resumed into B's pane.
//! Closing your only tracked agent resurrected it.
//!
//! So the cause is *recorded*, per object, by the operation that creates it —
//! the same conclusion Plan 1 reached for session carry-forward. A binding is
//! carried only when a restore said, in writing, "I put this pane back and
//! this conversation is not in it yet", on this boot, recently enough for that
//! statement still to be about the reboot window.
//!
//! # A binding is never overwritten by carried data
//!
//! Debt only ever *adds* a binding to a pane the current capture detected
//! nothing on, and never for a conversation the current capture found
//! somewhere else. Fresh evidence always wins; carrying is what happens where
//! there is none.

use crate::agent::AgentKind;
use anyhow::Result;
use rusqlite::Connection;
use std::collections::HashSet;

/// How long a recorded debt is still evidence about the reboot window.
///
/// Long enough to cover a real restore: every pane is resumed in turn and one
/// pane's resume alone may wait [`crate::agent::resume::DEFAULT_TIMEOUT`] for
/// its agent to appear, so a machine with a dozen conversations can be several
/// minutes from its first pane to its last. Short enough that a conversation
/// that never came back stops being carried instead of being carried for ever:
/// once the window closes, an undetected binding is simply gone, which is what
/// "the user closed it" and "it failed to come back" both actually look like
/// from here, and the snapshot that still holds it stays retryable in the
/// meantime.
pub const WINDOW_SECS: i64 = 15 * 60;

/// One conversation, as debt names it.
pub type Conversation = (AgentKind, String);

/// Where a debt was taken: the session's name, the window's index within that
/// session, and the pane's index within the window.
///
/// The same three facts a restore actually puts back, and the same ones
/// `capture` re-finds a pane by (`capture::Place`). A debt is a statement
/// about one pane, and it has to stay one: a permission attached to the
/// conversation alone authorises *every* previous occurrence of that
/// conversation anywhere on the machine that can be mapped by place — panes of
/// sessions this restore conflicted on and never touched included.
pub type Place = (String, u32, u32);

/// One pending debt: a conversation, and the one pane it is owed at.
pub type Debt = (Place, Conversation);

/// Write down every conversation `snapshot_id` binds to a pane **this attempt
/// verifiably put back** as owed by this boot, at `now`.
///
/// Called by a restore once its panes exist and before it delivers anything,
/// so that a capture racing the resume pass finds the debt already recorded.
/// Re-recording an existing row refreshes `recorded_at`: a second restore
/// attempt for the same snapshot is a fresh statement about the same panes,
/// and dating it from the first attempt would let the window expire mid-run.
///
/// One row per *link*, not per pane: a window linked into two sessions is
/// owed under each of the places that locates it, which is exactly how
/// `capture` will look it up again.
///
/// # Why the delivered map is a parameter
///
/// A debt is a promise this restore is making about a pane it built, and it
/// may only be made about panes it did build. `delivered_sessions` is the set
/// of session names the attempt created or adopted and `restored_panes` the
/// captured pane ids in its own pane map — the same two facts
/// `restore::resume_agents` refuses to deliver without. A binding in a session
/// this attempt conflicted on is not owed by anybody: nothing was put back
/// there, the live session belongs to someone else, and recording it as owed
/// let a later capture carry that conversation onto a pane of *theirs*.
///
/// Every call also [`prune`]s first. Debt used to be pruned only where it was
/// read, so a machine that restored partially several times without ever
/// capturing accumulated one boot-scoped set per attempt indefinitely.
pub fn record(
    conn: &Connection,
    snapshot_id: i64,
    boot_id: &str,
    now: i64,
    delivered_sessions: &HashSet<&str>,
    restored_panes: &HashSet<&str>,
) -> Result<usize> {
    prune(conn, boot_id, now)?;
    let mut stmt = conn.prepare(
        "SELECT p.agent_kind, p.agent_session_id, s.name, l.idx, p.idx, p.tmux_pane_id
         FROM pane_rows p
         JOIN window_rows w ON w.row_id = p.window_row_id
         JOIN session_window_links l ON l.window_row_id = w.row_id
         JOIN session_rows s ON s.row_id = l.session_row_id
         WHERE w.snapshot_id = ?1
           AND p.agent_kind IS NOT NULL AND p.agent_session_id IS NOT NULL",
    )?;
    let rows: Vec<(String, String, String, u32, u32, String)> = stmt
        .query_map([snapshot_id], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        })?
        .collect::<std::result::Result<_, _>>()?;
    drop(stmt);

    let mut written = 0;
    for (kind, native_id, session_name, window_idx, pane_idx, captured_pane) in rows {
        // Not this attempt's to promise anything about.
        if !delivered_sessions.contains(session_name.as_str())
            || !restored_panes.contains(captured_pane.as_str())
        {
            continue;
        }
        // Unreachable in practice: the column is only ever written from
        // `AgentKind::as_str`. A row that is not one of ours is skipped rather
        // than recorded as debt nothing could ever discharge.
        if AgentKind::parse(&kind).is_none() {
            continue;
        }
        conn.execute(
            "INSERT INTO agent_resume_debt
               (boot_id, kind, native_id, session_name, window_idx, pane_idx, recorded_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(boot_id, kind, native_id, session_name, window_idx, pane_idx)
             DO UPDATE SET recorded_at = excluded.recorded_at",
            rusqlite::params![
                boot_id,
                kind,
                native_id,
                session_name,
                window_idx,
                pane_idx,
                now
            ],
        )?;
        written += 1;
    }
    Ok(written)
}

/// What is still owed on `boot_id` as of `now`: each conversation **together
/// with the pane it is owed at**.
///
/// The place is not decoration. Returning conversations alone discarded the
/// session, window and pane a debt was taken for, so one pane's permission
/// authorised carrying that conversation onto any pane the previous snapshot
/// could be mapped onto — including panes in sessions the restore never
/// created or adopted. A caller matches a candidate pane's own place against
/// these.
///
/// Rows from another boot, and rows recorded more than [`WINDOW_SECS`] before
/// `now`, are not returned — and are deleted on the way past, so the table
/// cannot grow without bound on a long-running machine. The boundary is
/// inclusive: a debt taken on exactly [`WINDOW_SECS`] ago is still owed.
pub fn pending(conn: &Connection, boot_id: &str, now: i64) -> Result<HashSet<Debt>> {
    prune(conn, boot_id, now)?;
    let mut stmt = conn.prepare(
        "SELECT DISTINCT kind, native_id, session_name, window_idx, pane_idx
         FROM agent_resume_debt
         WHERE boot_id = ?1 AND recorded_at >= ?2",
    )?;
    let cutoff = now - WINDOW_SECS;
    let rows: Vec<(String, String, String, u32, u32)> = stmt
        .query_map(rusqlite::params![boot_id, cutoff], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .collect::<std::result::Result<_, _>>()?;
    Ok(rows
        .into_iter()
        .filter_map(|(kind, id, session, window_idx, pane_idx)| {
            AgentKind::parse(&kind).map(|k| ((session, window_idx, pane_idx), (k, id)))
        })
        .collect())
}

/// Forget the debt for every conversation in `live`.
///
/// A conversation a capture can *see* running is not owed by anyone: whatever
/// the restore was going to do about it has either happened or stopped
/// mattering. Discharging here, from observed evidence, is what keeps a
/// conversation from being carried onto a second pane after it has come back
/// on its own.
pub fn discharge(conn: &Connection, live: &[Conversation]) -> Result<()> {
    for (kind, native_id) in live {
        conn.execute(
            "DELETE FROM agent_resume_debt WHERE kind = ?1 AND native_id = ?2",
            rusqlite::params![kind.as_str(), native_id],
        )?;
    }
    Ok(())
}

/// Forget the debt taken at exactly `place` for `conversation`.
///
/// What a **confirmed** resume discharges, at the moment it is confirmed. The
/// alternative — waiting for a later capture to observe the conversation
/// running — leaves the promise standing over a pane that is already back, and
/// a restore resuming a dozen panes in turn gives the user minutes in which to
/// close the first one. Publication then finds a bare shell there with the
/// debt still pending, carries the conversation the user has just closed, and
/// resurrects it after the next reboot.
///
/// Exactly this place, never the conversation everywhere: the pane whose
/// resume was confirmed is the pane whose debt is paid.
pub fn discharge_at(
    conn: &Connection,
    boot_id: &str,
    place: &Place,
    conversation: &Conversation,
) -> Result<usize> {
    let (session_name, window_idx, pane_idx) = place;
    let (kind, native_id) = conversation;
    Ok(conn.execute(
        "DELETE FROM agent_resume_debt
         WHERE boot_id = ?1 AND kind = ?2 AND native_id = ?3
           AND session_name = ?4 AND window_idx = ?5 AND pane_idx = ?6",
        rusqlite::params![
            boot_id,
            kind.as_str(),
            native_id,
            session_name,
            window_idx,
            pane_idx
        ],
    )?)
}

/// Delete debt that is no longer about the reboot window: another boot's, or
/// recorded more than [`WINDOW_SECS`] before `now`. Returns how many rows
/// went.
pub fn prune(conn: &Connection, boot_id: &str, now: i64) -> Result<usize> {
    Ok(conn.execute(
        "DELETE FROM agent_resume_debt WHERE boot_id <> ?1 OR recorded_at < ?2",
        rusqlite::params![boot_id, now - WINDOW_SECS],
    )?)
}
