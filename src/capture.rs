use crate::agent::{self, detect::PaneProbe, AgentAdapter, AgentKind};
use crate::boot;
use crate::lock::SingleInstance;
use crate::tmux::{PaneRec, SessionRec, Tmux, WindowRec};
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub struct Topology {
    pub sessions: Vec<SessionRec>,
    pub windows: Vec<WindowRec>,
    pub panes: Vec<PaneRec>,
    /// Which incarnation of which tmux server these three lists were read
    /// from, and `None` **only** when no tmux server was running at all — in
    /// which case there is nothing in the three lists either.
    ///
    /// See [`Tmux::running_server_incarnation`]. It is what makes an
    /// unprefixed tmux id in a snapshot mean something: `@1` names a window
    /// only on the server that issued it, and the next server on the same
    /// socket issues `@1` again for something else. It used to also be `None`
    /// when the server *would not say* who it was, which made an unusable
    /// identity indistinguishable from no server and let an old session list
    /// be recorded beside a new server's windows.
    pub server: Option<String>,
    /// The identity the server reported *after* the three list commands.
    ///
    /// A topology is stitched from three separate `list-*` invocations and no
    /// tmux can dump itself atomically, so the identity is read at both ends
    /// and both are kept. Anything other than equality means these rows came
    /// from two different servers and describe neither — see
    /// [`Topology::inconsistency`].
    pub server_at_end: Option<String>,
}

impl Topology {
    /// Why these three lists cannot describe one single server state, or
    /// `None` if they can.
    ///
    /// tmux has no "dump the whole server atomically" command, so a topology
    /// is stitched together from three separate `list-*` invocations. A
    /// session created or destroyed between them produces a graph that is
    /// *structurally* impossible on a live server, and every such shape means
    /// real state is missing:
    ///
    /// * a window whose session is absent — the session list is stale, so
    ///   whatever else that session owns was never recorded;
    /// * a pane whose window is absent — same, one level down;
    /// * a session with no windows — the session was closed after it was
    ///   listed; writing it restores as a *fabricated* empty session;
    /// * a window with no panes — likewise. tmux destroys a window when its
    ///   last pane dies, so this shape never exists on a live server.
    ///
    /// Returning a description rather than a bool so the retry that gives up
    /// can say what it kept seeing.
    pub fn inconsistency(&self) -> Option<String> {
        // Before any structural question: *whose* state is this? A server
        // replaced between the first and last read hands out `$0`/`@0`/`%0`
        // from zero again, so an old session list and a new server's windows
        // can stitch together into a graph that passes every check below and
        // describes nothing that ever existed.
        if self.server != self.server_at_end {
            return Some(format!(
                "the tmux server changed identity while it was being read: {} before the \
                 three list commands and {} after, so these rows are not one server's state",
                self.server.as_deref().unwrap_or("no server"),
                self.server_at_end.as_deref().unwrap_or("no server"),
            ));
        }

        let session_ids: HashSet<&str> = self.sessions.iter().map(|s| s.id.as_str()).collect();
        let window_ids: HashSet<&str> = self.windows.iter().map(|w| w.id.as_str()).collect();

        if let Some(w) = self
            .windows
            .iter()
            .find(|w| !session_ids.contains(w.session_id.as_str()))
        {
            return Some(format!(
                "window {} belongs to session {} which the session list does not contain",
                w.id, w.session_id
            ));
        }
        if let Some(p) = self
            .panes
            .iter()
            .find(|p| !window_ids.contains(p.window_id.as_str()))
        {
            return Some(format!(
                "pane {} belongs to window {} which the window list does not contain",
                p.id, p.window_id
            ));
        }

        let sessions_with_windows: HashSet<&str> =
            self.windows.iter().map(|w| w.session_id.as_str()).collect();
        if let Some(s) = self
            .sessions
            .iter()
            .find(|s| !sessions_with_windows.contains(s.id.as_str()))
        {
            return Some(format!(
                "session {} ({}) has no windows; a live tmux session always has at least one",
                s.id, s.name
            ));
        }

        let windows_with_panes: HashSet<&str> =
            self.panes.iter().map(|p| p.window_id.as_str()).collect();
        if let Some(w) = self
            .windows
            .iter()
            .find(|w| !windows_with_panes.contains(w.id.as_str()))
        {
            return Some(format!(
                "window {} ({}) has no panes; a live tmux window always has at least one",
                w.id, w.name
            ));
        }

        None
    }
}

/// One round of the three `list-*` queries, with no consistency check.
///
/// Exposed for tests and for [`collect`]; production code wants `collect`,
/// which is the version that refuses to hand back a torn graph.
pub fn collect_once(tmux: &Tmux) -> Result<Topology> {
    // Read either side of the three list commands, and both are kept: a
    // server replaced mid-read produced neither of the two identities'
    // topologies, and labelling the rows with one of them anyway is exactly
    // the claim that must never be wrong. The disagreement is reported by
    // `inconsistency`, so `collect` re-reads rather than committing it.
    //
    // `?` and not `.ok()`: a running server that will not identify itself is
    // a hard failure. Swallowing it produced a topology labelled `None`,
    // which every reader downstream took to mean "no server" — and an
    // unattributed graph's tmux ids are satisfied by whichever server is
    // asked next.
    let before = tmux.running_server_incarnation()?;
    let sessions = tmux.list_sessions().context("list sessions")?;
    let windows = tmux.list_windows().context("list windows")?;
    let panes = tmux.list_panes().context("list panes")?;
    let after = tmux.running_server_incarnation()?;
    Ok(Topology {
        sessions,
        windows,
        panes,
        server: before,
        server_at_end: after,
    })
}

/// How many times [`collect`] re-reads the server before giving up. Bounded:
/// a capture that cannot get a clean read must fail loudly and quickly rather
/// than spin inside a tmux hook.
const MAX_COLLECT_ATTEMPTS: u32 = 4;

/// How long to wait between re-reads, giving whatever churn tore the previous
/// read time to settle.
const COLLECT_RETRY_DELAY: Duration = Duration::from_millis(40);

/// The server's topology, guaranteed to describe **one** server state.
///
/// Retries a bounded number of times when the three queries disagree, and
/// errors if they never agree — a torn graph must never be committed and
/// marked `complete`, because the shapes it produces (a session with no
/// windows, a window whose panes were never seen) restore as fabricated or
/// truncated sessions with no signal to the user that anything was lost.
pub fn collect(tmux: &Tmux) -> Result<Topology> {
    let mut last: Option<String> = None;
    for _ in 0..MAX_COLLECT_ATTEMPTS {
        let topo = collect_once(tmux)?;
        match topo.inconsistency() {
            None => return Ok(topo),
            Some(why) => {
                last = Some(why);
                std::thread::sleep(COLLECT_RETRY_DELAY);
            }
        }
    }
    Err(anyhow::anyhow!(
        "tmux topology still inconsistent after {MAX_COLLECT_ATTEMPTS} reads \
         ({}); refusing to record a partial snapshot",
        last.unwrap_or_else(|| "unknown".to_string())
    ))
}

pub fn snapshot(conn: &mut Connection, tmux: &Tmux, reason: &str) -> Result<i64> {
    snapshot_with_retention(conn, tmux, reason, 20)
}

/// Capture, then prune to the newest `keep` snapshots.
pub fn snapshot_with_retention(
    conn: &mut Connection,
    tmux: &Tmux,
    reason: &str,
    keep: usize,
) -> Result<i64> {
    snapshot_inner(conn, tmux, reason, Some(keep))
}

/// Capture and delete nothing.
///
/// Used when retention cannot be determined — a broken config must never be
/// silently treated as `keep = 20`, because a user who set
/// `keep_snapshots = 100` and typo'd an unrelated key would lose 80
/// snapshots to a default they never chose. Capturing is always safe;
/// deleting on a guess is not.
pub fn snapshot_without_pruning(conn: &mut Connection, tmux: &Tmux, reason: &str) -> Result<i64> {
    snapshot_inner(conn, tmux, reason, None)
}

fn snapshot_inner(
    conn: &mut Connection,
    tmux: &Tmux,
    reason: &str,
    keep: Option<usize>,
) -> Result<i64> {
    let topo = collect(tmux)?;
    write_topology(conn, &topo, reason, keep)
}

/// Persist an already-collected topology as one snapshot.
///
/// Rejects an inconsistent graph before touching the database: the snapshot
/// this writes is marked `complete`, which is what makes it eligible to be
/// restored, so it must describe a server state that actually existed.
pub fn write_topology(
    conn: &mut Connection,
    topo: &Topology,
    reason: &str,
    keep: Option<usize>,
) -> Result<i64> {
    let tx = conn.transaction()?;
    let snapshot_id = write_topology_in(&tx, topo, reason)?;
    tx.commit()?;
    if let Some(keep) = keep {
        crate::snapshots::prune(conn, keep, &boot::current_boot_id()?)?;
    }
    Ok(snapshot_id)
}

/// The adapters to probe panes against during capture: whichever agents
/// `agents.enabled` names.
///
/// A config that fails to load must not turn agent detection off — that
/// would read as "every pane's agent quietly stopped being tracked" the
/// moment a config file typo appeared, which is a worse silent failure than
/// falling back to the built-in default list. `status --json` already
/// reports the same config as invalid, so the user has somewhere to see it.
fn agent_adapters_for_capture() -> Vec<Box<dyn AgentAdapter>> {
    let enabled = match crate::paths::config_path().and_then(|p| crate::config::load(&p)) {
        Ok(cfg) => cfg.agents.enabled,
        Err(e) => {
            eprintln!("osm: {e:#}; using the default enabled agent adapters for this capture");
            crate::config::AgentsCfg::default().enabled
        }
    };
    agent::adapters(&enabled)
}

/// One conversation the previous snapshot recorded as bound to a pane,
/// together with the confidence that binding was made at.
#[derive(Debug, Clone)]
struct PrevBinding {
    kind: AgentKind,
    native_id: String,
    confidence: f32,
}

/// Where a bound pane sat: the name of a session the window is linked into,
/// that window's index within that session, and the pane's index within the
/// window.
///
/// This is the only pane identity that survives a restore. A tmux pane id is
/// issued by one server incarnation and means nothing on the next, but a
/// restore puts the session back under its captured *name*, the window back
/// at its captured *index*, and the panes back in their captured order — the
/// same three facts `restore::resume_agents` navigates by.
type Place = (String, u32, u32);

/// Everything the immediate predecessor snapshot knew about which
/// conversation was bound where.
///
/// Only the single newest other snapshot is consulted, never a chain of
/// them: the guard exists to survive one capture landing in the reboot
/// window before resume completes, not to resurrect a binding a human
/// deliberately let lapse two captures ago.
#[derive(Debug, Default)]
struct PreviousBindings {
    /// Which server incarnation that snapshot was read from, so a caller can
    /// tell whether its pane ids still name anything (see
    /// [`carry_bindings_forward`]).
    server: Option<String>,
    /// Keyed by that snapshot's tmux pane ids.
    by_pane: HashMap<String, PrevBinding>,
    /// Keyed by [`Place`]. A window linked into several sessions is recorded
    /// under each of them, since any one of those places locates it.
    by_place: HashMap<Place, PrevBinding>,
}

impl PreviousBindings {
    fn is_empty(&self) -> bool {
        self.by_pane.is_empty()
    }
}

fn previous_agent_bindings(
    tx: &rusqlite::Transaction,
    excluding_snapshot: i64,
) -> Result<PreviousBindings> {
    let mut out = PreviousBindings::default();
    let previous: Option<(i64, Option<String>)> = tx
        .query_row(
            "SELECT id, server FROM snapshots WHERE id <> ?1 ORDER BY taken_at DESC, id DESC LIMIT 1",
            [excluding_snapshot],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((previous, server)) = previous else {
        return Ok(out);
    };
    out.server = server;
    let mut stmt = tx.prepare(
        "SELECT p.tmux_pane_id, p.idx, p.agent_kind, p.agent_session_id, p.agent_confidence,
                s.name, l.idx
         FROM pane_rows p
         JOIN window_rows w ON w.row_id = p.window_row_id
         JOIN session_window_links l ON l.window_row_id = w.row_id
         JOIN session_rows s ON s.row_id = l.session_row_id
         WHERE w.snapshot_id = ?1 AND p.agent_kind IS NOT NULL AND p.agent_session_id IS NOT NULL",
    )?;
    let rows = stmt.query_map([previous], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, u32>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, Option<f32>>(4)?,
            r.get::<_, String>(5)?,
            r.get::<_, u32>(6)?,
        ))
    })?;
    for row in rows {
        let (pane_id, pane_idx, kind, native_id, confidence, session_name, window_idx) = row?;
        let Some(kind) = AgentKind::parse(&kind) else {
            continue; // Unreachable in practice: this column is only ever written from AgentKind::as_str.
        };
        let binding = PrevBinding {
            kind,
            native_id,
            confidence: confidence.unwrap_or(0.0),
        };
        out.by_place
            .insert((session_name, window_idx, pane_idx), binding.clone());
        out.by_pane.insert(pane_id, binding);
    }
    Ok(out)
}

/// Re-key the previous snapshot's bindings onto the panes that exist *now*.
///
/// Carrying a binding forward is only worth doing if it can still be acted
/// on, and a binding is acted on through the pane it names. The map that
/// comes out of [`previous_agent_bindings`] is keyed by the pane ids of the
/// server that snapshot was read from — which, after a restore, is a server
/// that no longer exists. Written down verbatim those keys match no live
/// pane at all (so every binding is silently dropped, which is exactly the
/// loss the guard exists to prevent) or, worse, collide by coincidence with
/// a *different* pane the new server happened to number the same way, and
/// bind someone's conversation to a pane it was never in.
///
/// So pane ids are trusted only when this topology comes from the very same
/// server incarnation the previous snapshot was read from. Otherwise — and
/// as a fallback whenever an id no longer matches — a binding is placed by
/// [`Place`], the identity a restore actually preserves.
///
/// A conversation the previous snapshot recorded against two panes is
/// carried onto both: this reproduces what was recorded, faithfully, rather
/// than inventing a rule for a shape only a mis-detection can produce.
fn carry_bindings_forward(
    prev: &PreviousBindings,
    topo: &Topology,
) -> HashMap<String, (AgentKind, String, f32)> {
    let same_server = prev.server.is_some() && prev.server == topo.server;
    let session_names: HashMap<&str, &str> = topo
        .sessions
        .iter()
        .map(|s| (s.id.as_str(), s.name.as_str()))
        .collect();
    let mut out: HashMap<String, (AgentKind, String, f32)> = HashMap::new();
    for w in &topo.windows {
        // A window whose session is missing cannot be placed; `write_topology_in`
        // refuses such a topology outright before this ever runs.
        let Some(session_name) = session_names.get(w.session_id.as_str()) else {
            continue;
        };
        for p in topo.panes.iter().filter(|p| p.window_id == w.id) {
            if out.contains_key(&p.id) {
                continue; // same pane, seen through another link
            }
            let found = if same_server {
                prev.by_pane.get(&p.id)
            } else {
                None
            }
            .or_else(|| {
                prev.by_place
                    .get(&((*session_name).to_string(), w.idx, p.idx))
            });
            if let Some(b) = found {
                out.insert(p.id.clone(), (b.kind, b.native_id.clone(), b.confidence));
            }
        }
    }
    out
}

/// Every [`Place`] each live pane occupies, one per link: a pane in a window
/// linked into two sessions is at two of them.
///
/// A debt names one pane by place, and this is how a pane in front of us is
/// asked whether it is that one.
fn pane_places(topo: &Topology) -> HashMap<String, Vec<Place>> {
    let session_names: HashMap<&str, &str> = topo
        .sessions
        .iter()
        .map(|s| (s.id.as_str(), s.name.as_str()))
        .collect();
    let mut out: HashMap<String, Vec<Place>> = HashMap::new();
    for w in &topo.windows {
        let Some(session_name) = session_names.get(w.session_id.as_str()) else {
            continue;
        };
        for p in topo.panes.iter().filter(|p| p.window_id == w.id) {
            out.entry(p.id.clone())
                .or_default()
                .push(((*session_name).to_string(), w.idx, p.idx));
        }
    }
    out
}

/// Add the bindings this capture could not see but is still *owed*, without
/// ever displacing one it could.
///
/// # The rule
///
/// A pane keeps whatever this capture detected on it. Where a pane has no
/// fresh binding, the previous snapshot's binding for that place is added —
/// but only when a restore has recorded that the conversation is still owed
/// (see [`crate::debt`]) and the conversation is not already running
/// somewhere else on this server.
///
/// # What it replaced, and why
///
/// The previous rule compared the *set* of bound conversations with the
/// previous snapshot's and threw the whole new map away when more than half
/// of them had gone. Two things were wrong with it, and the second is the
/// serious one:
///
/// * it was a percentage, so the cause ("a restore has not finished putting
///   the conversations back") was inferred from a ratio rather than known;
/// * it *overwrote fresh bindings with stale ones*. A user who exits
///   conversation A and starts B in the same pane produces `prev = {A}`,
///   `next = {B}` — 100% loss by that measure — so the correct binding `{B}`
///   was discarded and A carried forward, every capture, for ever. After a
///   reboot, A was then resumed into B's pane.
///
/// Now a fresh binding is never a candidate for replacement: carrying only
/// ever fills a gap.
///
/// # The two admissible causes
///
/// * `detection_ran` and the conversation is in [`crate::debt`]: a restore has
///   put this pane back and has not (yet) put the conversation into it;
/// * `!detection_ran`: this capture could not read the enabled agents at all,
///   so it has nothing to say about any pane. That is a fact about the
///   capture, recorded here, not a ratio inferred from its results — and the
///   capture goes ahead, because the tmux topology is what the snapshot exists
///   for and losing it over an unreadable agent home would be the larger loss;
/// * the pane is in `unknown_panes`: detection ran, looked at this pane, and
///   could not tell what it holds — its agent has a transcript open whose file
///   has been unlinked, which is what an atomic replacement leaves behind. Not
///   knowing is not the same as there being nothing there, so the previous
///   binding stands until ownership can be established again.
fn carry_owed_bindings(
    tx: &rusqlite::Transaction,
    topo: &Topology,
    detected: HashMap<String, (AgentKind, String, f32)>,
    live_now: &[(AgentKind, String)],
    snapshot_id: i64,
    detection_ran: bool,
    unknown_panes: &HashSet<String>,
) -> Result<HashMap<String, (AgentKind, String, f32)>> {
    let previous = previous_agent_bindings(tx, snapshot_id)?;
    if previous.is_empty() {
        return Ok(detected);
    }
    // The second admissible cause, and the reason it is one: detection did not
    // run. "No pane is running an agent" and "osm could not tell what any pane
    // is running" are opposite statements, and writing the first when the
    // second is true destroys the map. No debt is consulted, because this is
    // not a claim about any conversation — it is a fact about this capture.
    let owed = if detection_ran {
        crate::debt::pending(
            tx,
            &crate::boot::current_boot_id()?,
            crate::boot::now_epoch(),
        )?
    } else {
        HashSet::new()
    };
    if detection_ran && owed.is_empty() && unknown_panes.is_empty() {
        return Ok(detected);
    }
    let running: HashSet<(AgentKind, &str)> =
        live_now.iter().map(|(k, id)| (*k, id.as_str())).collect();

    let places = pane_places(topo);
    let mut bindings = detected;
    let mut carried = 0usize;
    for (pane_id, binding) in carry_bindings_forward(&previous, topo) {
        // Fresh evidence wins, always.
        if bindings.contains_key(&pane_id) {
            continue;
        }
        let conversation = (binding.0, binding.1.clone());
        // It came back somewhere; that pane is where it is, and putting it on
        // this one as well would bind one conversation to two panes.
        if running.contains(&(conversation.0, conversation.1.as_str())) {
            continue;
        }
        // No recorded cause. Whatever happened to this conversation between
        // the two captures, nothing on this machine says a restore still owes
        // it and this capture could read the pane, so the honest record is
        // that the pane holds no conversation.
        // Owed *here*, at one of the places this very pane occupies — never
        // merely somewhere. A conversation-level permission would authorise
        // carrying it onto any pane the previous snapshot happens to map onto,
        // including panes of sessions this restore never touched.
        let owed_here = places.get(&pane_id).is_some_and(|here| {
            here.iter()
                .any(|place| owed.contains(&(place.clone(), conversation.clone())))
        });
        if detection_ran && !owed_here && !unknown_panes.contains(&pane_id) {
            continue;
        }
        bindings.insert(pane_id, binding);
        carried += 1;
    }
    if carried > 0 {
        let cause = if detection_ran {
            "recorded as still owed by this boot's restore, or held open by an agent \
             through a descriptor osm cannot identify, and not detected on any pane"
        } else {
            "not checked at all, because this capture could not read the enabled agents"
        };
        eprintln!(
            "osm: {carried} conversation(s) {cause}; carrying their bindings forward \
             onto the panes that hold their place"
        );
    }
    Ok(bindings)
}

/// Persist an already-collected topology inside a transaction the caller
/// owns.
///
/// Split out so a restore can publish the current boot's topology and retire
/// the snapshot it restored from in **one** transaction. Doing that in two
/// steps leaves a window in which the only `complete` snapshot on the machine
/// has been retired and its replacement has not been written yet — a power
/// loss there costs the user everything.
pub fn write_topology_in(tx: &rusqlite::Transaction, topo: &Topology, reason: &str) -> Result<i64> {
    if let Some(why) = topo.inconsistency() {
        anyhow::bail!("refusing to record an inconsistent tmux topology: {why}");
    }
    // Every row below is written down under an unprefixed tmux id, and an
    // unprefixed id names something only on the server that issued it. With
    // no server to attribute them to, `$0`/`@0`/`%0` are satisfied by the
    // next server to be asked — which is how a carried session could be
    // linked into an unrelated window and the only snapshot that knew better
    // discharged and pruned. An empty topology is the one honest exception:
    // there is no server precisely because there is nothing on it.
    if topo.server.is_none()
        && !(topo.sessions.is_empty() && topo.windows.is_empty() && topo.panes.is_empty())
    {
        anyhow::bail!(
            "refusing to record {} session(s), {} window(s) and {} pane(s) that belong to \
             no identified tmux server incarnation",
            topo.sessions.len(),
            topo.windows.len(),
            topo.panes.len()
        );
    }
    let boot_id = boot::current_boot_id()?;
    let taken_at = boot::now_epoch();

    tx.execute(
        "INSERT INTO snapshots (taken_at, boot_id, reason, state, server)
         VALUES (?1, ?2, ?3, 'building', ?4)",
        rusqlite::params![taken_at, boot_id, reason, topo.server],
    )?;
    let snapshot_id = tx.last_insert_rowid();

    // session native id -> session_rows.row_id
    let mut session_rows: HashMap<String, i64> = HashMap::new();
    for s in &topo.sessions {
        let active = topo
            .windows
            .iter()
            .find(|w| w.session_id == s.id && w.active)
            .map(|w| w.id.clone());
        tx.execute(
            "INSERT INTO session_rows (snapshot_id, tmux_session_id, name, active_window_id)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![snapshot_id, s.id, s.name, active],
        )?;
        session_rows.insert(s.id.clone(), tx.last_insert_rowid());
    }

    // window native id -> window_rows.row_id.
    //
    // `list-windows -a` emits a linked window once per session it is linked
    // into, so the same native window id arrives several times. It is still
    // *one* window with one set of panes: insert the window row on first
    // sight only, and record each sighting as a link carrying that
    // session's own index and active flag. Keying window rows by session
    // instead (the previous schema) made the pane inserts below collide on
    // `UNIQUE (window_row_id, tmux_pane_id)`, which rolled back the whole
    // transaction — so a single `link-window` anywhere on the server broke
    // every capture until the link was removed.
    let mut window_rows: HashMap<String, i64> = HashMap::new();
    for w in &topo.windows {
        // Unreachable: `inconsistency()` above rejects exactly this shape.
        // It used to be a `continue`, which silently dropped the window (and
        // left its session as a row with no windows, restoring later as a
        // fabricated empty session) while still marking the snapshot
        // `complete`.
        let Some(&session_row_id) = session_rows.get(&w.session_id) else {
            anyhow::bail!(
                "window {} references session {} that is not in this snapshot",
                w.id,
                w.session_id
            );
        };
        let window_row_id = match window_rows.get(&w.id) {
            Some(&row_id) => row_id,
            None => {
                let active_pane = topo
                    .panes
                    .iter()
                    .find(|p| p.window_id == w.id && p.active)
                    .map(|p| p.id.clone());
                tx.execute(
                    "INSERT INTO window_rows
                       (snapshot_id, tmux_window_id, name, auto_named, layout,
                        active_pane_id, zoomed)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    rusqlite::params![
                        snapshot_id,
                        w.id,
                        w.name,
                        // Recorded beside the name because it is what says
                        // whether the name means anything: with it on, tmux is
                        // still deriving the name and this capture may well
                        // have caught it mid-flight.
                        w.auto_named as i64,
                        w.layout,
                        active_pane,
                        w.zoomed as i64
                    ],
                )?;
                let row_id = tx.last_insert_rowid();
                window_rows.insert(w.id.clone(), row_id);
                row_id
            }
        };
        tx.execute(
            "INSERT INTO session_window_links (session_row_id, window_row_id, idx, active)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![session_row_id, window_row_id, w.idx, w.active as i64],
        )?;
    }

    // Which conversation, if any, each live pane is bound to. Detected fresh
    // for this capture; a binding it could not see is only ever *added* to,
    // never allowed to displace one it could — see `carry_owed_bindings`.
    let adapters = agent_adapters_for_capture();
    // Each adapter's conversations are read once for the whole capture.
    //
    // A failure here is not "no pane holds a conversation", and it is not a
    // reason to abandon the capture either: the tmux topology is the thing
    // this snapshot exists for, and refusing to record it because an agent's
    // home directory became unreadable would cost the user their sessions over
    // an unrelated problem. What it *is* is a specific, recorded cause for
    // carrying the previous bindings forward — detection did not run.
    let prepared = match agent::detect::prepare(&adapters) {
        Ok(prepared) => Some(prepared),
        Err(e) => {
            eprintln!(
                "osm: could not read the enabled agents' conversations ({e:#}); this \
                 capture cannot tell which pane is running what, so it carries the \
                 previous snapshot's bindings rather than recording that there are none"
            );
            None
        }
    };
    let detection_ran = prepared.is_some();
    let mut detected: HashMap<String, (AgentKind, String, f32)> = HashMap::new();
    // Panes detection looked at and could not answer for. See
    // `agent::detect::lineage_ownership_unknown`.
    let mut unknown_panes: HashSet<String> = HashSet::new();
    if let Some(prepared) = &prepared {
        for p in &topo.panes {
            if detected.contains_key(&p.id) {
                continue; // same pane, seen through another link
            }
            let probe = PaneProbe {
                pane_id: p.id.clone(),
                pane_pid: p.pid,
                cwd: p.cwd.clone(),
                foreground_cmd: p.cmd.clone(),
            };
            if let Some(binding) = agent::detect::bind(&probe, prepared) {
                detected.insert(
                    p.id.clone(),
                    (binding.kind, binding.native_id, binding.confidence),
                );
            } else if agent::detect::lineage_ownership_unknown(&probe, prepared) {
                // Not "this pane holds no conversation": this pane holds a
                // transcript whose file has been unlinked out from under its
                // agent, so nothing can be matched by device and inode. The
                // pane's previous binding is the best evidence anyone has and
                // is kept until ownership can be established again.
                unknown_panes.insert(p.id.clone());
            }
        }
    }

    // What this capture can *see* running, which is the only thing that is
    // ever written down as a fresh binding. A conversation observed live owes
    // nobody anything, so any debt recorded against it is discharged here,
    // from evidence, before the carry decision is made.
    let live_now: Vec<(AgentKind, String)> = detected
        .values()
        .map(|(kind, id, _)| (*kind, id.clone()))
        .collect();
    crate::debt::discharge(tx, &live_now)?;

    let bindings = carry_owed_bindings(
        tx,
        topo,
        detected,
        &live_now,
        snapshot_id,
        detection_ran,
        &unknown_panes,
    )?;

    // `list-panes -a` likewise repeats a linked window's panes once per
    // link. Deduplicate explicitly rather than by INSERT OR IGNORE, so the
    // UNIQUE constraint stays a real guard against genuine duplicates.
    let mut seen_panes: HashSet<(&str, &str)> = HashSet::new();
    for p in &topo.panes {
        // Unreachable for the same reason as the window case above; a
        // `continue` here dropped panes out of an otherwise "complete"
        // snapshot.
        let Some(&window_row_id) = window_rows.get(&p.window_id) else {
            anyhow::bail!(
                "pane {} references window {} that is not in this snapshot",
                p.id,
                p.window_id
            );
        };
        if !seen_panes.insert((p.window_id.as_str(), p.id.as_str())) {
            continue; // same pane, seen through another link
        }
        let (restore_policy, agent_kind, agent_session_id, agent_confidence) =
            match bindings.get(&p.id) {
                Some((kind, native_id, confidence)) => (
                    "agent_resume",
                    Some(kind.as_str()),
                    Some(native_id.as_str()),
                    Some(*confidence),
                ),
                None => ("shell", None, None, None),
            };
        tx.execute(
            "INSERT INTO pane_rows
               (window_row_id, tmux_pane_id, idx, cwd, title, foreground_cmd,
                dead, restore_policy, agent_kind, agent_session_id, agent_confidence)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            rusqlite::params![
                window_row_id,
                p.id,
                p.idx,
                p.cwd,
                p.title,
                p.cmd,
                p.dead as i64,
                restore_policy,
                agent_kind,
                agent_session_id,
                agent_confidence
            ],
        )?;
    }

    // Everything an earlier snapshot still holds and this server has not
    // demonstrably got back.
    let carried = carry_forward(tx, snapshot_id, topo)?;

    tx.execute(
        "UPDATE snapshots SET state='complete', unresolved=?2 WHERE id=?1",
        rusqlite::params![snapshot_id, carried as i64],
    )?;
    Ok(snapshot_id)
}

/// Prefix given to the tmux ids of a carried session, so they can never
/// collide with a live `$0` / `@1` / `%2` and so the same carried window keeps
/// one identity however many generations it travels through — which is what
/// keeps a linked window linked.
const CARRIED_PREFIX: &str = "carried:";

fn carried_id(source: i64, original: &str) -> String {
    if original.starts_with(CARRIED_PREFIX) {
        original.to_string()
    } else {
        format!("{CARRIED_PREFIX}{source}:{original}")
    }
}

/// The tmux id inside a carried one: `carried:7:@0` → `@0`.
///
/// The namespacing exists so a carried row cannot collide with a live one in
/// the same snapshot. It has to be undone to ask the only question that
/// matters about a carried window — is the window it *was* on the server right
/// now? — because both the restore's window map and the live snapshot rows
/// speak in unprefixed ids.
fn original_id(id: &str) -> &str {
    match id.strip_prefix(CARRIED_PREFIX) {
        Some(rest) => rest.split_once(':').map(|(_, o)| o).unwrap_or(rest),
        None => id,
    }
}

/// Every live session in `topo`, as a shape that can be compared against a
/// captured one.
fn live_shapes(topo: &Topology) -> HashMap<String, crate::equiv::SessionShape> {
    // `list-panes -a` emits a linked window's panes once per link, so the same
    // (window, pane) arrives several times.
    let mut panes: HashMap<&str, Vec<crate::equiv::RawPane>> = HashMap::new();
    let mut seen: HashSet<(&str, &str)> = HashSet::new();
    for p in &topo.panes {
        if !seen.insert((p.window_id.as_str(), p.id.as_str())) {
            continue;
        }
        panes
            .entry(p.window_id.as_str())
            .or_default()
            .push(crate::equiv::RawPane {
                id: p.id.clone(),
                idx: p.idx,
                cwd: p.cwd.clone(),
                active: p.active,
            });
    }

    let mut shapes = HashMap::new();
    for s in &topo.sessions {
        let mut windows: Vec<&WindowRec> = topo
            .windows
            .iter()
            .filter(|w| w.session_id == s.id)
            .collect();
        windows.sort_by_key(|w| w.idx);
        let active_window = windows.iter().find(|w| w.active).map(|w| w.idx);
        shapes.insert(
            s.name.clone(),
            crate::equiv::SessionShape {
                name: s.name.clone(),
                windows: windows
                    .into_iter()
                    .map(|w| {
                        crate::equiv::window_shape(
                            w.id.clone(),
                            w.idx,
                            w.name.clone(),
                            Some(w.auto_named),
                            w.layout.clone(),
                            w.zoomed,
                            panes.get(w.id.as_str()).cloned().unwrap_or_default(),
                        )
                    })
                    .collect(),
                active_window,
            },
        );
    }
    shapes
}

/// Which live window each of `source`'s captured windows became, according to
/// the newest restore that ran against **this** tmux server.
///
/// Two restrictions, and both are the difference between a link and a
/// fabrication:
///
/// * **Only attempts whose destination server is the one being captured.** A
///   mapping is a statement about window ids on one server incarnation, and
///   every server started on a socket issues `@0`, `@1`, … from zero again. An
///   attempt that ran before a within-boot tmux restart therefore describes
///   windows that no longer exist, under ids that now belong to something
///   else — and the only check standing behind the mapping is a window name
///   and a pane count, which an unrelated window can easily match. An attempt
///   with no recorded identity (`NULL`: a pre-v7 row, or a server that would
///   not say) matches nothing.
/// * **One attempt, not a merge.** The answers used to be merged across every
///   attempt, newest overwriting oldest, so an older attempt filled the gaps
///   in a newer one's map. A gap in the current attempt's map means *this*
///   restore did not put that window back; an older run's answer about it is
///   about windows that this run may since have rebuilt or replaced. A
///   carried window with no mapping is copied, which costs a link; a carried
///   window mapped onto the wrong live window corrupts one.
fn restore_window_map(
    tx: &rusqlite::Transaction,
    source: i64,
    server: Option<&str>,
) -> Result<HashMap<String, String>> {
    let mut map = HashMap::new();
    let Some(server) = server else {
        return Ok(map);
    };
    let attempt: Option<i64> = tx
        .query_row(
            "SELECT id FROM restore_attempts
             WHERE snapshot_id = ?1 AND destination_server = ?2
             ORDER BY id DESC LIMIT 1",
            rusqlite::params![source, server],
            |r| r.get(0),
        )
        .optional()?;
    let Some(attempt) = attempt else {
        return Ok(map);
    };
    let mut stmt = tx.prepare(
        "SELECT captured_window_id, live_window_id
         FROM restore_window_map WHERE attempt_id = ?1",
    )?;
    let rows = stmt.query_map([attempt], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (captured, live) = row?;
        map.insert(captured, live);
    }
    Ok(map)
}

/// Whether two window rows are the same window, to the extent a snapshot can
/// tell.
///
/// Name and pane count, deliberately not geometry: a window is legitimately
/// rescaled by tmux when it is rebuilt at a different size, and rejecting the
/// identity for that would split a linked window in two — the bug this check
/// is part of fixing. What it does catch is the case that makes an unverified
/// id match unsafe: a tmux server restarted within one boot hands out `@0`
/// again to something entirely unrelated.
fn same_window(tx: &rusqlite::Transaction, a: i64, b: i64) -> Result<bool> {
    let describe = |row: i64| -> Result<(String, i64)> {
        Ok(tx.query_row(
            "SELECT w.name, (SELECT COUNT(*) FROM pane_rows p WHERE p.window_row_id = w.row_id)
             FROM window_rows w WHERE w.row_id = ?1",
            [row],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?)
    };
    Ok(describe(a)? == describe(b)?)
}

/// Copy every session an earlier snapshot is still owed, and this server has
/// not demonstrably got back, into the snapshot being written. Returns whether
/// anything was carried.
///
/// # Why a capture has to do this at all
///
/// A restore picks its source by recency. Boot A's snapshot holds `alpha` and
/// `beta`; boot B restores only `alpha` and hands the source back to
/// `complete`; a hook then captures boot B's *incomplete* topology. On boot C
/// the newest complete snapshot is boot B's, so `beta` is never restored —
/// and retention deletes boot A's snapshot, with no error at any point. The
/// fix is to make every capture a superset: what an earlier snapshot is still
/// owed travels forward until something recovers it.
///
/// # The debt is per session, and it is only ever created by a restore
///
/// Three things went wrong when it was a single flag on the whole snapshot,
/// discharged as soon as a live session held a matching *name*:
///
/// * **A partially restored session resolved itself.** A restore that failed
///   after four of nine panes leaves `beta` live and truncated. The name is
///   there, so the next capture cleared the flag and wrote a newest snapshot
///   holding the four-pane `beta`; boot C then selected *that*, and retention
///   was free to delete the nine-pane original. Resolution now requires
///   [`crate::equiv::difference`] to find no difference at all.
/// * **A mixed live/carried pair split a linked window.** `alpha` and `beta`
///   share one window; the restore delivers `alpha` and not `beta`. Carrying
///   `beta` built it a second window and the link relation was gone, silently.
///   A carried window is now linked to the live window it *became* — see
///   [`restore_window_map`].
/// * **Deleting a session brought it back.** Every session of an unresolved
///   snapshot was carry-forward's business, including ones the user had since
///   closed or renamed, so closing `beta` resurrected it and each rename added
///   another name to carry, generation after generation, without bound. Debt
///   is now created by a restore that failed to deliver a *specific* session,
///   never by a capture, so a session that was delivered and later closed is
///   simply gone — which is what the user asked for.
///
/// A session whose name is live but whose shape differs is neither resolved
/// nor carried: a snapshot holding two sessions with one name could never be
/// restored, so the debt stays on the source, which keeps the source exempt
/// from retention until the session really does come back.
fn carry_forward(tx: &rusqlite::Transaction, into: i64, topo: &Topology) -> Result<bool> {
    let sources: Vec<i64> = tx
        .prepare(
            "SELECT id FROM snapshots
             WHERE unresolved = 1 AND id <> ?1
             ORDER BY taken_at DESC, id DESC",
        )?
        .query_map([into], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    if sources.is_empty() {
        return Ok(false);
    }

    let live = live_shapes(topo);
    // The window rows this capture has just written, by the id the live server
    // calls them. A carried window that *is* one of these is linked to it
    // rather than copied.
    let live_window_rows: HashMap<String, i64> = tx
        .prepare("SELECT tmux_window_id, row_id FROM window_rows WHERE snapshot_id = ?1")?
        .query_map([into], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?
        .collect::<Result<_, _>>()?;

    let mut taken: HashSet<String> = topo.sessions.iter().map(|s| s.name.clone()).collect();
    let mut carried = false;
    for source in sources {
        let map = restore_window_map(tx, source, topo.server.as_deref())?;
        // Unprefixed ids in a snapshot read from *this very server* name
        // windows this capture just read. From any other server — including
        // an earlier tmux on the same socket in the same boot — they name
        // windows that no longer exist, and `@0` there has nothing to do with
        // `@0` here. This used to compare boot ids, which a within-boot tmux
        // restart leaves equal.
        let source_server: Option<String> = tx.query_row(
            "SELECT server FROM snapshots WHERE id = ?1",
            [source],
            |r| r.get(0),
        )?;
        let same_server = match (&topo.server, &source_server) {
            (Some(into_server), Some(from_server)) => into_server == from_server,
            // No identity on one side or the other is never a match: an
            // unverifiable link is copied, not guessed at.
            _ => false,
        };

        type OwedSession = (i64, String, String, Option<String>);
        let sessions: Vec<OwedSession> = tx
            .prepare(
                "SELECT row_id, tmux_session_id, name, active_window_id
                 FROM session_rows WHERE snapshot_id = ?1 AND unresolved = 1
                 ORDER BY row_id",
            )?
            .query_map([source], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?
            .collect::<Result<_, _>>()?;

        // Shared across the sessions of one source, so a window linked into
        // two of them is copied once and linked twice, exactly as it was.
        let mut window_rows: HashMap<i64, (i64, String)> = HashMap::new();
        for (session_row, tmux_session_id, name, active_window_id) in sessions {
            if let Some(live_shape) = live.get(&name) {
                let want =
                    crate::equiv::shape_of_plan(&crate::model::load_session(tx, session_row)?);
                // Two questions, and the second one is not visible from
                // inside this session: is this session back, and are the
                // windows it *shared* with another captured session still
                // shared? See [`shared_link_break`].
                let unrecovered = match crate::equiv::difference(&want, live_shape) {
                    Some(why) => Some(format!(
                        "a live session holds the name but not the captured topology ({why})"
                    )),
                    None => shared_link_break(
                        tx,
                        session_row,
                        &want,
                        live_shape,
                        &live,
                        &map,
                        &live_window_rows,
                    )?,
                };
                match unrecovered {
                    // Verifiably back. Not "a session by that name exists",
                    // and not "a session that looks like it in isolation".
                    None => resolve(tx, session_row)?,
                    // The debt stays where it is: the source keeps
                    // `unresolved`, so retention cannot take it, and a later
                    // capture asks again.
                    Some(why) => eprintln!(
                        "osm: snapshot {source}: session {name:?} is still owed — {why}; \
                         keeping snapshot {source} restorable"
                    ),
                }
                continue;
            }
            if !taken.insert(name.clone()) {
                continue;
            }
            copy_session(
                tx,
                source,
                into,
                session_row,
                &tmux_session_id,
                &name,
                active_window_id.as_deref(),
                &mut window_rows,
                &live_window_rows,
                &map,
                same_server,
            )?;
            // The debt moves with the content: it is the new snapshot that
            // owes this session now.
            resolve(tx, session_row)?;
            carried = true;
        }
        crate::snapshots::refresh_unresolved(tx, source)?;
    }
    Ok(carried)
}

/// Why discharging this session's debt would lose a link relation, or `None`
/// if it would not.
///
/// # A property that exists only *between* sessions
///
/// [`crate::equiv::difference`] judges one session against one live session,
/// which is everything a restore's adoption needs and one thing short of what
/// a discharge needs. `alpha` and `beta` share window `@1`; a restore delivers
/// `alpha` only; the user (or anything else) then creates a `beta` with the
/// captured indices, names, layouts, focus and pane directories but a window
/// of **its own**. Compared session by session that beta is indistinguishable
/// from the captured one, so its debt was discharged, the next snapshot
/// recorded two independent windows, and retention was free to delete the only
/// snapshot that still knew the two were one window. Nothing reported
/// anything.
///
/// So every captured window this session shares with another captured session
/// is checked against the server as it is now, and it takes evidence to pass:
///
/// * the restore's own record of what that window became — scoped to this
///   server incarnation by [`restore_window_map`], and only if that window is
///   still live;
/// * a sharing session that is itself live and itself matches its captured
///   shape, in which case the live window *it* holds in that position is the
///   answer.
///
/// The identity is established **once per captured window**, from whichever of
/// those two the server can still supply, and every piece of evidence there is
/// must agree with the live window this session holds there. No evidence at
/// all is not a pass: an unverifiable link keeps the debt, which keeps the
/// source snapshot exempt from retention, which is the outcome that cannot
/// lose anything.
///
/// What it must *not* do is demand evidence from every historic sharer, which
/// is how the first version of this check over-corrected. `alpha`, `beta` and
/// `gamma` share a window; a restore delivers `alpha` and `gamma` and leaves
/// `beta` owed; the user then closes `gamma`, which is an ordinary thing to do
/// with a session that came back. `beta` returning as a link to the very
/// window `alpha` still holds *is* the captured `beta` — but `gamma` could no
/// longer say so, and asking it per sharer turned one deliberately closed
/// session into a debt nothing could ever discharge and a snapshot retention
/// could never take.
fn shared_link_break(
    tx: &rusqlite::Transaction,
    session_row: i64,
    want: &crate::equiv::SessionShape,
    live_shape: &crate::equiv::SessionShape,
    live: &HashMap<String, crate::equiv::SessionShape>,
    map: &HashMap<String, String>,
    live_window_rows: &HashMap<String, i64>,
) -> Result<Option<String>> {
    // (captured window id, sharing session row, its name), for every window
    // this session does not hold alone.
    type Sharer = (String, i64, String);
    let sharers: Vec<Sharer> = tx
        .prepare(
            "SELECT w.tmux_window_id, other.row_id, other.name
             FROM session_window_links l
             JOIN window_rows w ON w.row_id = l.window_row_id
             JOIN session_window_links l2 ON l2.window_row_id = l.window_row_id
                                         AND l2.session_row_id <> l.session_row_id
             JOIN session_rows other ON other.row_id = l2.session_row_id
             WHERE l.session_row_id = ?1
             ORDER BY w.row_id, other.row_id",
        )?
        .query_map([session_row], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<Result<_, _>>()?;
    if sharers.is_empty() {
        return Ok(None);
    }

    // Only sound because `difference` has already returned `None`: that is
    // exactly the check that says the two sides pair up window by window.
    let held: HashMap<String, String> = crate::equiv::window_pairs(want, live_shape)
        .into_iter()
        .collect();
    let mut sharer_pairs: HashMap<i64, HashMap<String, String>> = HashMap::new();

    // One entry per captured *window*, carrying every session the snapshot
    // shares it with. The query orders by window row, so equal windows arrive
    // together. Grouping is the whole point: a window has one live identity,
    // and any sharer that can still name it names it for all of them.
    let mut by_window: Vec<(String, Vec<(i64, String)>)> = Vec::new();
    for (window, other_row, other_name) in sharers {
        match by_window.last_mut() {
            Some((w, others)) if *w == window => others.push((other_row, other_name)),
            _ => by_window.push((window, vec![(other_row, other_name)])),
        }
    }

    for (window, others) in by_window {
        let names = || {
            others
                .iter()
                .map(|(_, n)| format!("{n:?}"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let Some(here) = held.get(&window) else {
            return Ok(Some(format!(
                "the live session does not account for window {window}, which the \
                 snapshot shares with session(s) {}",
                names()
            )));
        };
        let mut claims: Vec<(String, String)> = Vec::new();
        if let Some(live_id) = map.get(original_id(&window)) {
            if live_window_rows.contains_key(live_id.as_str()) {
                claims.push((
                    format!("the restore that rebuilt it made it {live_id}"),
                    live_id.clone(),
                ));
            }
        }
        for (other_row, other_name) in &others {
            if !sharer_pairs.contains_key(other_row) {
                let other_want =
                    crate::equiv::shape_of_plan(&crate::model::load_session(tx, *other_row)?);
                let pairs = match live.get(other_name.as_str()) {
                    Some(other_live)
                        if crate::equiv::difference(&other_want, other_live).is_none() =>
                    {
                        crate::equiv::window_pairs(&other_want, other_live)
                            .into_iter()
                            .collect()
                    }
                    // This sharing session is gone, or is not the captured one
                    // either. It can say nothing about this window — which is
                    // not the same as contradicting what another sharer says.
                    _ => HashMap::new(),
                };
                sharer_pairs.insert(*other_row, pairs);
            }
            if let Some(there) = sharer_pairs[other_row].get(&window) {
                claims.push((
                    format!("session {other_name:?} holds {there} there"),
                    there.clone(),
                ));
            }
        }
        if claims.is_empty() {
            return Ok(Some(format!(
                "the live session matches the snapshot on its own, but nothing on this \
                 server can confirm that window {window}, shared with session(s) {}, is \
                 still one window",
                names()
            )));
        }
        if let Some((what, _)) = claims.into_iter().find(|(_, id)| id != here) {
            return Ok(Some(format!(
                "window {window} is shared with session(s) {} in the snapshot, but the \
                 live session holds {here} where {what}",
                names()
            )));
        }
    }
    Ok(None)
}

fn resolve(tx: &rusqlite::Transaction, session_row: i64) -> Result<()> {
    tx.execute(
        "UPDATE session_rows SET unresolved = 0 WHERE row_id = ?1",
        [session_row],
    )?;
    Ok(())
}

/// Where one of a carried session's windows ends up in the new snapshot.
enum WindowTarget {
    /// A window row that already exists in the new snapshot: either a *live*
    /// window that is the one this captured window became, or one already
    /// copied for a sibling carried session. Linked to, never copied again —
    /// which is what keeps a linked window one window.
    Existing(i64, String),
    /// Nothing in the new snapshot corresponds to it yet; it is copied.
    Carry,
}

/// Decide whether `tmux_window_id` (as the source snapshot calls it) is a
/// window this capture has just recorded as live.
///
/// Two things can establish that, and both are checked against the row itself
/// with [`same_window`] before it is believed:
///
/// * a restore said so, by recording captured `@0` → live `@7`;
/// * the source snapshot is from this boot, so its unprefixed ids already name
///   windows on this server.
fn resolve_window(
    tx: &rusqlite::Transaction,
    source_window_row: i64,
    tmux_window_id: &str,
    live_window_rows: &HashMap<String, i64>,
    map: &HashMap<String, String>,
    same_server: bool,
) -> Result<WindowTarget> {
    let original = original_id(tmux_window_id);
    let candidate = match map.get(original) {
        Some(live) => Some(live.as_str()),
        None if same_server => Some(original),
        None => None,
    };
    if let Some(live_id) = candidate {
        if let Some(&row) = live_window_rows.get(live_id) {
            if same_window(tx, source_window_row, row)? {
                return Ok(WindowTarget::Existing(row, live_id.to_string()));
            }
        }
    }
    Ok(WindowTarget::Carry)
}

/// One session, its windows, its links and its panes, copied verbatim except
/// for the tmux ids, which are rewritten by [`carried_id`] — unless the window
/// is one the live server is already holding, in which case the carried
/// session is linked into it and the link relation survives the restore that
/// only delivered half of it.
#[allow(clippy::too_many_arguments)]
fn copy_session(
    tx: &rusqlite::Transaction,
    source: i64,
    into: i64,
    session_row: i64,
    tmux_session_id: &str,
    name: &str,
    active_window_id: Option<&str>,
    window_rows: &mut HashMap<i64, (i64, String)>,
    live_window_rows: &HashMap<String, i64>,
    map: &HashMap<String, String>,
    same_server: bool,
) -> Result<()> {
    type LinkedWindow = (
        i64,
        String,
        String,
        Option<i64>,
        String,
        Option<String>,
        i64,
        u32,
        i64,
    );
    let windows: Vec<LinkedWindow> = tx
        .prepare(
            "SELECT w.row_id, w.tmux_window_id, w.name, w.auto_named, w.layout,
                    w.active_pane_id, w.zoomed, l.idx, l.active
             FROM session_window_links l
             JOIN window_rows w ON w.row_id = l.window_row_id
             WHERE l.session_row_id = ?1 ORDER BY l.idx",
        )?
        .query_map([session_row], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
                r.get(8)?,
            ))
        })?
        .collect::<Result<_, _>>()?;

    // Resolved before anything is written, because the session row's
    // `active_window_id` has to name the id the window ends up under — which
    // is the *live* id when the window is linked rather than copied.
    let mut ids: HashMap<String, String> = HashMap::new();
    let mut targets: Vec<WindowTarget> = Vec::new();
    for (window_row, tmux_window_id, ..) in &windows {
        let target = match window_rows.get(window_row) {
            Some((row, id)) => WindowTarget::Existing(*row, id.clone()),
            None => resolve_window(
                tx,
                *window_row,
                tmux_window_id,
                live_window_rows,
                map,
                same_server,
            )?,
        };
        let id = match &target {
            WindowTarget::Existing(_, existing_id) => existing_id.clone(),
            WindowTarget::Carry => carried_id(source, tmux_window_id),
        };
        ids.insert(tmux_window_id.clone(), id);
        targets.push(target);
    }

    tx.execute(
        "INSERT INTO session_rows (snapshot_id, tmux_session_id, name, active_window_id,
                                   unresolved)
         VALUES (?1, ?2, ?3, ?4, 1)",
        rusqlite::params![
            into,
            carried_id(source, tmux_session_id),
            name,
            active_window_id
                .and_then(|id| ids.get(id).cloned())
                // Defensive: a snapshot whose active window is not one of the
                // session's own windows is malformed, but it must not silently
                // become NULL here.
                .or_else(|| active_window_id.map(|id| carried_id(source, id)))
        ],
    )?;
    let new_session_row = tx.last_insert_rowid();

    for (
        (
            window_row,
            tmux_window_id,
            wname,
            auto_named,
            layout,
            active_pane_id,
            zoomed,
            idx,
            active,
        ),
        target,
    ) in windows.into_iter().zip(targets)
    {
        let new_window_row = match target {
            WindowTarget::Existing(row, _) => row,
            WindowTarget::Carry => {
                tx.execute(
                    "INSERT INTO window_rows
                       (snapshot_id, tmux_window_id, name, auto_named, layout,
                        active_pane_id, zoomed)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    rusqlite::params![
                        into,
                        carried_id(source, &tmux_window_id),
                        wname,
                        // Carried verbatim, NULL included: a carried copy must
                        // not claim to know something its source did not.
                        auto_named,
                        layout,
                        active_pane_id.as_deref().map(|id| carried_id(source, id)),
                        zoomed
                    ],
                )?;
                let new_row = tx.last_insert_rowid();
                copy_panes(tx, source, window_row, new_row)?;
                new_row
            }
        };
        window_rows.insert(
            window_row,
            (
                new_window_row,
                ids.get(&tmux_window_id).cloned().unwrap_or_default(),
            ),
        );
        tx.execute(
            "INSERT INTO session_window_links (session_row_id, window_row_id, idx, active)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![new_session_row, new_window_row, idx, active],
        )?;
    }
    Ok(())
}

fn copy_panes(
    tx: &rusqlite::Transaction,
    source: i64,
    from_window: i64,
    to_window: i64,
) -> Result<()> {
    type PaneRow = (
        String,
        u32,
        String,
        Option<String>,
        Option<String>,
        i64,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<f64>,
    );
    let panes: Vec<PaneRow> = tx
        .prepare(
            "SELECT tmux_pane_id, idx, cwd, title, foreground_cmd, dead, restore_policy,
                    restore_argv, agent_kind, agent_session_id, agent_confidence
             FROM pane_rows WHERE window_row_id = ?1 ORDER BY idx",
        )?
        .query_map([from_window], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
                r.get(8)?,
                r.get(9)?,
                r.get(10)?,
            ))
        })?
        .collect::<Result<_, _>>()?;

    for p in panes {
        tx.execute(
            "INSERT INTO pane_rows
               (window_row_id, tmux_pane_id, idx, cwd, title, foreground_cmd, dead,
                restore_policy, restore_argv, agent_kind, agent_session_id,
                agent_confidence)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            rusqlite::params![
                to_window,
                carried_id(source, &p.0),
                p.1,
                p.2,
                p.3,
                p.4,
                p.5,
                p.6,
                p.7,
                p.8,
                p.9,
                p.10
            ],
        )?;
    }
    Ok(())
}

/// Run a closure while holding the restore lock **shared**.
///
/// Returns `Ok(None)` if a restore holds the lock exclusively, otherwise
/// runs `f()` while still holding the lock and returns `Ok(Some(value))`.
///
/// Shared, not exclusive: captures must exclude restores (and be excluded by
/// them) but must not exclude *each other* here — a capture that loses a race
/// against another capture is not "blocked by a restore", and treating it as
/// one is what used to make hook captures vanish. Capture-vs-capture
/// serialisation happens on a separate lock; see
/// [`snapshot_maybe_debounced`].
pub fn with_restore_lock<T>(lock_path: &Path, f: impl FnOnce() -> Result<T>) -> Result<Option<T>> {
    let Some(_guard) = SingleInstance::acquire_shared(lock_path)? else {
        return Ok(None);
    };
    let value = f()?;
    Ok(Some(value))
}

/// How long an event capture waits for another capture to finish before
/// giving up and leaving the work to the dirty flag. Short enough that a
/// tmux hook never feels wedged, long enough to absorb an ordinary capture
/// (a few tens of milliseconds).
const CAPTURE_LOCK_WAIT: Duration = Duration::from_secs(2);

/// How many times one capture may repeat itself to absorb events that
/// arrived while it was running. Bounded so a burst of events cannot pin a
/// hook process in a capture loop; whatever is still pending after the last
/// pass stays flagged for the next capture.
const MAX_CAPTURE_PASSES: u32 = 2;

/// Captures serialise against each other here, not on the restore lock.
fn capture_lock_path(restore_lock: &Path) -> PathBuf {
    restore_lock.with_file_name("capture.lock")
}

/// Marker for "state changed and no capture has recorded it yet".
///
/// Derived from the restore lock's directory rather than passed in, so the
/// already long `snapshot_maybe_debounced` signature does not grow two more
/// paths that every caller would have to agree on.
fn dirty_flag_path(restore_lock: &Path) -> PathBuf {
    restore_lock.with_file_name("capture-dirty")
}

fn is_dirty(path: &Path) -> bool {
    path.exists()
}

/// Best-effort: if the flag cannot be written the capture still happens; the
/// only thing lost is the guarantee that a *deferred* capture is retried.
fn mark_dirty(path: &Path) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::write(path, b"1") {
        eprintln!(
            "osm: could not flag pending capture at {}: {e}",
            path.display()
        );
    }
}

fn clear_dirty(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// Capture using the retention configured by `capture.keep_snapshots`.
///
/// If the config cannot be loaded, the capture still happens but **nothing is
/// pruned**. Substituting the built-in default here would delete snapshots the
/// user explicitly asked to keep, on the strength of an error that may have
/// nothing to do with retention (`status --json` already reports the same
/// config as invalid). Retention resumes by itself once the config parses.
fn snapshot_with_configured_retention(
    conn: &mut Connection,
    tmux: &Tmux,
    reason: &str,
) -> Result<i64> {
    let config_path = crate::paths::config_path()?;
    match crate::config::load(&config_path) {
        Ok(cfg) => snapshot_with_retention(conn, tmux, reason, cfg.capture.keep_snapshots),
        Err(e) => {
            eprintln!(
                "osm: {}: {e:#}; capturing without pruning \
                 (retention stays off until the config is valid)",
                config_path.display()
            );
            snapshot_without_pruning(conn, tmux, reason)
        }
    }
}

/// Capture, unless a restore currently holds the lock.
///
/// Taking the lock (shared) proves no restore is in progress; holding it for
/// the duration of the capture prevents a restore from starting
/// mid-transaction. Deliberately without the capture lock and the pending
/// flag: this is the plain "capture now" entry point, and concurrent
/// captures are the hook path's problem — see [`snapshot_maybe_debounced`],
/// which is what the CLI and the daemon call.
pub fn snapshot_guarded(
    conn: &mut Connection,
    tmux: &Tmux,
    reason: &str,
    lock_path: &Path,
) -> Result<Option<i64>> {
    with_restore_lock(lock_path, || {
        snapshot_with_configured_retention(conn, tmux, reason)
    })
}

/// Result of a capture that may have been skipped by the debounce throttle,
/// blocked by a restore, or deferred behind another capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureOutcome {
    Captured(i64),
    /// Skipped: a capture already happened within the debounce window, and
    /// nothing is flagged as pending.
    Debounced,
    /// Skipped: a restore holds the restore lock exclusively.
    RestoreInProgress,
    /// Not skipped — *deferred*. Another capture held the capture lock past
    /// the wait budget, so this one recorded the pending-work flag and left
    /// the work to whoever captures next: either the holder's coalescing
    /// pass or the next capture, which then ignores its debounce window.
    ///
    /// This case used to be reported as [`Self::RestoreInProgress`] and the
    /// work simply dropped, so a pane split during another capture stayed
    /// uncaptured until the 120s fallback timer — and was lost outright if
    /// the machine rebooted first.
    Deferred,
}

/// True if a capture was recorded at `last_capture_path` within
/// `max_latency_secs` of `now`.
///
/// A missing file, an unreadable file, or contents that don't parse as a
/// timestamp are all treated as "no recent capture" — never as an error and
/// never as a reason to skip. Only a genuinely fresh, valid timestamp
/// throttles.
pub fn captured_recently(last_capture_path: &Path, now: i64, max_latency_secs: u64) -> bool {
    let Ok(text) = std::fs::read_to_string(last_capture_path) else {
        return false;
    };
    let Ok(last) = text.trim().parse::<i64>() else {
        return false;
    };
    let elapsed = now.saturating_sub(last);
    elapsed >= 0 && (elapsed as u64) < max_latency_secs
}

/// Record `now` as the time of a successful capture. Best-effort: a failure
/// to persist the timestamp must never fail the capture that already
/// happened, so callers ignore the error.
///
/// Permissions are set to `0600`, matching the database file, since both
/// live inside the same `0700` state directory and record the same class
/// of local operational data.
pub fn record_capture_time(last_capture_path: &Path, now: i64) -> Result<()> {
    if let Some(parent) = last_capture_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(last_capture_path, now.to_string())?;
    std::fs::set_permissions(
        last_capture_path,
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    )?;
    Ok(())
}

/// Capture, throttled by the debounce window when `debounced` is set.
///
/// Three kinds of contention, three distinct answers:
///
/// * **A restore is running** — it holds the restore lock exclusively, so the
///   shared acquire below fails and the call reports
///   [`CaptureOutcome::RestoreInProgress`]. This is checked before the
///   debounce window, so a genuine restore always beats the debounce.
/// * **Another capture is running** — it holds the capture lock. This call
///   waits [`CAPTURE_LOCK_WAIT`] for it and, if the holder is still going,
///   returns [`CaptureOutcome::Deferred`] leaving the dirty flag set, which
///   makes the holder take another pass (or the next capture ignore its
///   debounce window). The work is never dropped.
/// * **Nothing is running, but a capture just happened** — the debounce
///   window applies, unless the dirty flag says state changed since.
///
/// On any successful capture the capture time is recorded at
/// `last_capture_path`, so a subsequent debounced call sees it. The capture
/// lock and the dirty flag live beside `lock_path`.
#[allow(clippy::too_many_arguments)]
pub fn snapshot_maybe_debounced(
    conn: &mut Connection,
    tmux: &Tmux,
    reason: &str,
    lock_path: &Path,
    last_capture_path: &Path,
    max_latency_secs: u64,
    now: i64,
    debounced: bool,
) -> Result<CaptureOutcome> {
    let Some(_restore_guard) = SingleInstance::acquire_shared(lock_path)? else {
        return Ok(CaptureOutcome::RestoreInProgress);
    };

    let dirty = dirty_flag_path(lock_path);

    // A pending flag beats the debounce window: it means a previous capture
    // was deferred and its state has still not been recorded anywhere.
    if debounced && !is_dirty(&dirty) && captured_recently(last_capture_path, now, max_latency_secs)
    {
        return Ok(CaptureOutcome::Debounced);
    }

    // Flag the work *before* queueing for the lock. If this call loses the
    // race, the flag is what makes the winner capture again afterwards.
    mark_dirty(&dirty);

    let capture_lock = capture_lock_path(lock_path);
    let Some(_capture_guard) = SingleInstance::acquire_blocking(&capture_lock, CAPTURE_LOCK_WAIT)?
    else {
        return Ok(CaptureOutcome::Deferred);
    };

    let mut last_id = None;
    for _ in 0..MAX_CAPTURE_PASSES {
        // Clear before reading tmux, never after: a flag raised from here on
        // describes a change this pass' topology read may have missed.
        clear_dirty(&dirty);
        last_id = Some(snapshot_with_configured_retention(conn, tmux, reason)?);
        let _ = record_capture_time(last_capture_path, now);
        if !is_dirty(&dirty) {
            break;
        }
    }

    // Unreachable with MAX_CAPTURE_PASSES >= 1: the loop always captures at
    // least once before it can break.
    match last_id {
        Some(id) => Ok(CaptureOutcome::Captured(id)),
        None => Err(anyhow::anyhow!("capture loop ran zero passes")),
    }
}
