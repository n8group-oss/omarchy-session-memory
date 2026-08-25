use crate::equiv;
use crate::layout;
use crate::model::{SessionPlan, SnapshotTree, WindowPlan};
use crate::tmux::Tmux;
use crate::{boot, model, snapshots};
use anyhow::{Context, Error, Result};
use rusqlite::Connection;
use std::collections::{HashMap, HashSet};
use std::path::Path;

#[derive(Debug, Default)]
pub struct RestoreOutcome {
    pub created: Vec<String>,
    pub adopted: Vec<String>,
    pub skipped: Vec<String>,
    /// (session name, error text) for sessions whose restore_session call
    /// returned an error. Kept distinct from `skipped`, which is reserved
    /// for genuine name conflicts (Plan 2).
    pub failed: Vec<(String, String)>,
    /// (session name, what differs) for sessions that exist on the
    /// destination under the captured name but hold a *different* topology.
    ///
    /// Neither adopted (that would report someone else's empty session as
    /// the user's six-window one) nor rebuilt over (that would destroy live
    /// work). The snapshot stays restorable so a later run can try again
    /// once the conflicting session is gone.
    pub conflicted: Vec<(String, String)>,
    /// Panes whose captured working directory did not exist at restore time
    /// and which were therefore created somewhere else.
    ///
    /// Silently substituting `$HOME` and calling the restore a success is how
    /// an unmounted volume, an encrypted home that is not open yet, or a
    /// network mount that is slow at graphical-session start turns into
    /// permanently lost locations: the pane looks restored, the snapshot is
    /// retired, and the real directory is nowhere on record any more.
    pub degraded: Vec<MissingDir>,
    /// Windows whose captured layout was **not** applied, either because it
    /// did not parse as a tmux layout at all or because tmux refused it.
    ///
    /// The panes are all there; only the geometry is missing. Handing an
    /// unvalidated string to `select-layout` is not a safe alternative: what
    /// tmux does with a bad layout is version-dependent, and the blast radius
    /// of the bad cases is the entire server — including every session this
    /// same restore had already rebuilt. Skipping keeps the restore going and,
    /// like [`MissingDir`], marks it degraded so the snapshot stays retryable
    /// instead of being retired as a success.
    pub skipped_layouts: Vec<SkippedLayout>,
    /// Captured window id → the window id it now has on the destination
    /// server, for every window this restore created or verifiably adopted.
    ///
    /// Persisted in `restore_window_map`, because nothing else on the machine
    /// can reconstruct it. A window linked into two sessions where only one of
    /// them came back has to be *linked* into the other when a later capture
    /// carries it forward, and the only fact that says the live `@7` is the
    /// captured `@0` is this restore's own record of having made it.
    pub window_map: HashMap<String, String>,
    /// The server incarnation every window id above belongs to, or `None`
    /// when this restore cannot attribute its work to one server.
    ///
    /// See [`ServerWatch`]: a mapping is a statement about ids on one server,
    /// and there is no such statement to make when two of them were involved.
    pub server: Option<String>,
    /// Set when the destination server changed identity *while* the restore
    /// ran, with what changed. Everything this attempt did is then reported
    /// as failed — see [`ServerWatch`] for why nothing else is honest.
    pub server_changed: Option<String>,
}

/// Watches the destination server's identity for the whole of a restore.
///
/// # Why the end of the run is not late enough, and not early enough either
///
/// A restore builds its sessions one after another against a server that can
/// die between any two of them. The sessions built before the death are gone
/// with it; the ones built after are on a **different** server, which hands
/// out `@0`, `@1`, … from zero again. Recording each session as delivered the
/// moment its own tmux calls returned, and then comparing the identity once at
/// the end, missed both halves of that:
///
/// * the comparison only fired when an identity *changed*. No server before
///   the work and one after it — the ordinary boot case, and equally what a
///   crash-and-restart in the middle looks like — was accepted outright;
/// * neither case took the sessions built on the dead server back out of
///   `created`, so their debt was discharged, this boot's topology was
///   published, and the source snapshot — the only remaining record of them —
///   was retired as a success.
///
/// So the identity is established *once*, the first time a server exists, and
/// re-read after every session. Any move from it — to another identity, or to
/// no server at all — ends the run, and the whole attempt is reported as
/// failed: the sessions that are genuinely live will be adopted by the next
/// restore, and the ones that are not stay owed. Only work done under one
/// continuously verified incarnation is ever persisted as a mapping.
struct ServerWatch<'a> {
    tmux: &'a Tmux,
    /// The incarnation this restore's work belongs to, once one has been
    /// seen. `None` while no server exists yet, which is where a boot restore
    /// starts.
    anchor: Option<String>,
    /// Why the identity can no longer be trusted, once it cannot.
    changed: Option<String>,
}

impl<'a> ServerWatch<'a> {
    /// Fails outright when a server is running and will not identify itself.
    ///
    /// Not a soft start with no anchor: an unusable identity does not become
    /// usable later, so every session this restore went on to build would be
    /// work it could never attribute — and the run would then have to be
    /// failed anyway, after doing all of it.
    fn new(tmux: &'a Tmux) -> Result<Self> {
        Ok(Self {
            tmux,
            anchor: tmux.running_server_incarnation()?,
            changed: None,
        })
    }

    /// Read the identity again. `false` once it has moved — and it never
    /// moves back.
    ///
    /// `built` says whether the session just handled was one this restore
    /// tried to *create*, which is the only thing that makes "there is still
    /// no server" a failure rather than an ordinary state.
    fn check(&mut self, built: bool) -> bool {
        if self.changed.is_some() {
            return false;
        }
        let now = match self.tmux.running_server_incarnation() {
            Ok(now) => now,
            // A server that is there and will not say who it is. Reading this
            // as "no server" is what made a mid-restore restart invisible to
            // a run whose destination had, say, `@osm-server-id mine` in its
            // configuration: nothing changed, so nothing was ever reported.
            Err(e) => {
                self.changed = Some(format!(
                    "the destination tmux server would not report a usable identity \
                     ({e:#}), so nothing this restore did can be attributed to one server"
                ));
                return false;
            }
        };
        match (self.anchor.as_deref(), now) {
            // Work was done and there is no server at all: whatever this
            // restore just built died with the server it built on, before an
            // identity was ever established for it.
            (None, None) if built => {
                self.changed = Some(
                    "this restore built a session and there is no tmux server running at \
                     all, so what it built is not there"
                        .to_string(),
                );
                false
            }
            // The first server this restore has seen. Establishing the
            // identity here rather than accepting whatever is running at the
            // end is the whole point: at the end, the server that answers may
            // be the *second* one.
            (None, now) => {
                self.anchor = now;
                true
            }
            (Some(a), Some(b)) if a == b => true,
            (Some(a), Some(b)) => {
                self.changed = Some(format!(
                    "the tmux server was replaced while this restore was running: it \
                     built on {a} and is now {b}, so no single server holds what it did"
                ));
                false
            }
            (Some(a), None) => {
                self.changed = Some(format!(
                    "the tmux server {a} this restore was building on is gone, so \
                     nothing it built is still there"
                ));
                false
            }
        }
    }
}

/// One window that was rebuilt without its captured geometry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedLayout {
    pub session: String,
    pub window: String,
    /// The captured layout string, verbatim, so a corrupted row can be found.
    pub layout: String,
    /// Why it was not applied.
    pub reason: String,
}

/// Everything a restore got *almost* right, collected as it goes.
///
/// Threaded through the restore rather than returned, because a degradation is
/// discovered several calls deep (a pane's directory, a window's layout) and
/// must not turn into an error that abandons the rest of the session.
#[derive(Debug, Default)]
struct Degradations {
    dirs: Vec<MissingDir>,
    layouts: Vec<SkippedLayout>,
}

/// One pane that could not be created in its captured directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingDir {
    pub session: String,
    pub window: String,
    pub pane_index: u32,
    /// The directory the pane was in when it was captured.
    pub captured: String,
    /// Where the pane was actually created instead.
    pub used: String,
}

/// Where a pane can actually be started, and whether that is the directory
/// it was captured in.
///
/// `used` is the captured directory when it still exists; otherwise a
/// fallback that certainly does, because `new-window -c <gone>` fails
/// outright and would take the whole session down with it. The `bool` is the
/// part that used to be thrown away: the caller records it so the restore
/// reports itself as degraded instead of complete.
fn usable_cwd(cwd: &str) -> (String, bool) {
    if Path::new(cwd).is_dir() {
        (cwd.to_string(), false)
    } else {
        (
            std::env::var("HOME").unwrap_or_else(|_| "/".to_string()),
            true,
        )
    }
}

/// A generous fallback used when no window in a session has a layout string
/// that parses. A too-large session is harmless (tmux rescales it once a
/// client attaches); a too-small one loses panes permanently, so this must
/// never be tmux's own 80x24 default.
const FALLBACK_WIDTH: u32 = 360;
const FALLBACK_HEIGHT: u32 = 100;
const MIN_WIDTH: u32 = 80;
const MIN_HEIGHT: u32 = 24;

/// The window size a captured layout describes, or `None` when the string is
/// not a layout at all.
///
/// Goes through the same strict parser that gates `select-layout`, so a
/// corrupted row cannot size the session off a prefix that happens to look
/// numeric while the rest of it is garbage.
fn layout_dims(layout: &str) -> Option<(u32, u32)> {
    layout::parse(layout).ok().map(|node| node.size())
}

/// The size to create a session's tmux window(s) at: the max width and max
/// height across every window's captured layout in this session, so no
/// window is created too small to hold its captured pane count — floored at
/// 80x24 (tmux's own minimum), falling back to a generous default if no
/// layout in the session parses at all.
fn session_dims(session: &SessionPlan) -> (u32, u32) {
    let mut max: Option<(u32, u32)> = None;
    for window in &session.windows {
        if let Some((w, h)) = layout_dims(&window.layout) {
            max = Some(match max {
                Some((mw, mh)) => (mw.max(w), mh.max(h)),
                None => (w, h),
            });
        }
    }
    match max {
        Some((w, h)) => (w.max(MIN_WIDTH), h.max(MIN_HEIGHT)),
        None => (FALLBACK_WIDTH, FALLBACK_HEIGHT),
    }
}

fn live_session_names(tmux: &Tmux) -> HashSet<String> {
    // tmux reports "cannot connect" identically whether the destination
    // server has a genuine problem or simply hasn't been started yet — the
    // latter is the ordinary state of a brand-new restore destination (no
    // session has been created on this socket at all). Treating any
    // enumeration failure as "zero live sessions" is required for that
    // common case to work, and matches the precedent already set by
    // `Tmux::server_running`.
    tmux.list_sessions()
        .map(|v| v.into_iter().map(|s| s.name).collect())
        .unwrap_or_default()
}

/// One window as the destination server currently holds it.
struct LiveWindow {
    /// The destination server's own `@id` for this window, which is what a
    /// later `link-window` has to target.
    id: String,
    idx: u32,
    name: String,
    layout: String,
    /// Whether this is the session's current window.
    active: bool,
    /// Whether one of its panes is zoomed.
    zoomed: bool,
}

fn live_windows(tmux: &Tmux, session: &str) -> Result<Vec<LiveWindow>> {
    const FIELDS: [&str; 6] = [
        "window_id",
        "window_index",
        "window_active",
        "window_zoomed_flag",
        "window_name",
        "window_layout",
    ];
    let out = tmux.run(&[
        "list-windows",
        "-t",
        session,
        "-F",
        &crate::tmux::record_format(&FIELDS),
    ])?;
    let mut windows = Vec::new();
    for f in crate::tmux::parse_records(&out, FIELDS.len())? {
        windows.push(LiveWindow {
            id: f[0].clone(),
            idx: f[1].parse().context("window index")?,
            active: f[2] == "1",
            zoomed: f[3] == "1",
            name: f[4].clone(),
            layout: f[5].clone(),
        });
    }
    windows.sort_by_key(|w| w.idx);
    Ok(windows)
}

/// Every live pane in `session`, grouped by window index.
///
/// The pane **id** is collected as well as the index, and that is the point:
/// `select-layout` renumbers pane indices by geometry, so an index is not an
/// identity, while the id is what the window's own layout string names at each
/// cell. See [`equiv`] for why comparing the two sides as sorted lists of
/// directories — which is what this used to feed — matched a window against
/// itself with two panes swapped.
fn live_panes(tmux: &Tmux, session: &str) -> Result<HashMap<u32, Vec<equiv::RawPane>>> {
    const FIELDS: [&str; 5] = [
        "window_index",
        "pane_id",
        "pane_index",
        "pane_active",
        "pane_current_path",
    ];
    let out = tmux.run(&[
        "list-panes",
        "-s",
        "-t",
        session,
        "-F",
        &crate::tmux::record_format(&FIELDS),
    ])?;
    let mut by_window: HashMap<u32, Vec<equiv::RawPane>> = HashMap::new();
    for f in crate::tmux::parse_records(&out, FIELDS.len())? {
        by_window
            .entry(f[0].parse().context("window index")?)
            .or_default()
            .push(equiv::RawPane {
                id: f[1].clone(),
                idx: f[2].parse().context("pane index")?,
                active: f[3] == "1",
                cwd: f[4].clone(),
            });
    }
    Ok(by_window)
}

/// The destination server's `session`, as a shape [`equiv::difference`] can
/// compare against a captured one.
fn live_shape(tmux: &Tmux, session: &str) -> Result<equiv::SessionShape> {
    let windows = live_windows(tmux, session)?;
    let mut panes = live_panes(tmux, session)?;
    let active_window = windows.iter().find(|w| w.active).map(|w| w.idx);
    Ok(equiv::SessionShape {
        name: session.to_string(),
        windows: windows
            .into_iter()
            .map(|w| {
                let own = panes.remove(&w.idx).unwrap_or_default();
                equiv::window_shape(w.id, w.idx, w.name, w.layout, w.zoomed, own)
            })
            .collect(),
        active_window,
    })
}

/// The result of inspecting a live session that holds a captured session's
/// name.
enum Adoption {
    /// It really is the captured session. Carries every
    /// (captured window id, live window id) pair the comparison matched up,
    /// which is what lets a linked window be re-linked rather than rebuilt.
    Match(Vec<(String, String)>),
    /// It is something else. Carries what differs.
    Mismatch(String),
}

/// Why the live session named `plan.name` is **not** the captured one, or
/// `None` if it genuinely matches.
///
/// A name match alone used to count as a successful restore. That is the
/// difference between "your six windows are back" and "something else opened
/// a session called `dev` with one empty window and your six windows are
/// gone forever" — reported identically, with the snapshot then retired.
///
/// Matching the *structure* alone is not enough either, and fails in a way
/// this engine creates itself: a restore whose captured directory was
/// unavailable puts the panes in `$HOME` and reports itself degraded. That
/// session has the captured window count, the captured names and indices and
/// the captured pane geometry — a perfect structural match — so a
/// structure-only comparison adopts it on the next run and calls the restore
/// a success. That next run is exactly the one that had a chance of fixing it,
/// because the volume is mounted by then; adopting instead retires the
/// snapshot and makes the degradation permanent. So the working directories
/// are part of the comparison — pane by pane, in layout order, not as a
/// sorted bag of values; see [`equiv`].
///
/// Repairing such a session in place is deliberately *not* attempted. The only
/// way to move a live pane to another directory is to respawn it, which kills
/// whatever is running in it — an editor with unsaved work, a build, a shell
/// with history. Restore never destroys live work, so it reports and retries.
fn adoption_mismatch(tmux: &Tmux, plan: &SessionPlan) -> Adoption {
    let live = match live_shape(tmux, &plan.name) {
        Ok(shape) => shape,
        Err(e) => return Adoption::Mismatch(format!("could not inspect the live session: {e:#}")),
    };
    let want = equiv::shape_of_plan(plan);
    match equiv::difference(&want, &live) {
        Some(why) => Adoption::Mismatch(why),
        None => Adoption::Match(equiv::window_pairs(&want, &live)),
    }
}

pub fn restore_tree(tmux: &Tmux, tree: &SnapshotTree) -> Result<RestoreOutcome> {
    let mut outcome = RestoreOutcome::default();
    let mut degradations = Degradations::default();
    let mut watch = ServerWatch::new(tmux)?;
    let live = live_session_names(tmux);

    // Captured window id -> the window id created for it on the destination
    // server. Shared across every session in the tree: a window that was
    // linked into several sessions is *one* window, and must be re-linked on
    // the destination rather than rebuilt as independent copies that then
    // drift apart.
    let mut created_windows: HashMap<String, String> = HashMap::new();

    for session in &tree.sessions {
        // Whether this iteration may have left something on the destination
        // server. An adoption creates nothing, so it cannot be the reason a
        // server has to exist; a creation can, even when it then fails.
        let built = !live.contains(&session.name);
        if !built {
            // Adoption is what makes restore idempotent, and it must stay
            // non-destructive — but "a session with this name exists" is not
            // evidence that the captured state is back.
            match adoption_mismatch(tmux, session) {
                Adoption::Match(pairs) => match seed_window_map(&mut created_windows, pairs) {
                    Ok(()) => outcome.adopted.push(session.name.clone()),
                    // The live session matches on its own, but the window it
                    // holds is not the one another session in this same tree
                    // already accounted for — so the captured link relation
                    // is not what is live. Reported rather than adopted: the
                    // snapshot is the only remaining record that those
                    // sessions shared a window.
                    Err(why) => outcome.conflicted.push((session.name.clone(), why)),
                },
                Adoption::Mismatch(why) => outcome.conflicted.push((session.name.clone(), why)),
            }
        } else {
            // A single session's restore failing must not abort the rest: the
            // sessions tmux already created for earlier entries in this loop
            // are real, live side effects and every other session in the tree
            // still deserves its own attempt.
            match restore_session(tmux, session, &mut created_windows, &mut degradations)
                .with_context(|| format!("restore session {}", session.name))
            {
                Ok(()) => outcome.created.push(session.name.clone()),
                // `{:#}` (anyhow's "alternate" Display) renders the full cause
                // chain ("restore session alpha: tmux [...] failed: no space
                // for a new pane"), not just the outermost context line — the
                // operator needs the real tmux error, not just which session it
                // happened in.
                Err(e) => outcome
                    .failed
                    .push((session.name.clone(), format!("{e:#}"))),
            }
        }
        // After every session — adopted ones included, since an adoption is
        // a claim about live window ids just as much as a creation is — and
        // not once at the end: the answer is only worth anything while it is
        // still the same server that handled the last one.
        if !watch.check(built) {
            break;
        }
    }

    // Belt and braces for a shape `check` already forbids: work that no
    // incarnation can be named for is work this run cannot claim.
    let unattributable = (!outcome.created.is_empty() || !outcome.adopted.is_empty())
        && watch.anchor.is_none()
        && watch.changed.is_none();
    let lost_to = watch.changed.clone().or_else(|| {
        unattributable.then(|| {
            "this restore delivered sessions that no tmux server incarnation can be named \
             for, so none of them can be shown to be anywhere"
                .to_string()
        })
    });

    if let Some(why) = lost_to {
        // Not one of these sessions can be said to be on the server that is
        // running now, and the ones that are will be adopted by the next
        // restore. Reporting them as delivered is what discharged their debt
        // and retired the only snapshot that still held them.
        let mut lost = std::mem::take(&mut outcome.created);
        lost.append(&mut outcome.adopted);
        lost.sort();
        for name in lost {
            outcome.failed.push((name, why.clone()));
        }
        // `created_windows` is deliberately never moved into the outcome: a
        // mapping records which live window a captured one became, half of
        // these name windows on a server that is gone and the other half
        // windows on one that never held the captured session, and nothing
        // downstream can tell them apart.
        outcome.server_changed = Some(why);
        outcome.degraded = degradations.dirs;
        outcome.skipped_layouts = degradations.layouts;
        return Ok(outcome);
    }
    outcome.server = watch.anchor;

    // The map is only a claim until the server agrees with it. Checked
    // globally, after every session has had its turn, because a link is a
    // relation *between* sessions and no single session's restore can see
    // whether it holds.
    let succeeded: HashSet<&str> = outcome
        .created
        .iter()
        .chain(outcome.adopted.iter())
        .map(String::as_str)
        .collect();
    for (name, detail) in validate_links(tmux, tree, &created_windows, &succeeded) {
        outcome.created.retain(|s| s != &name);
        outcome.adopted.retain(|s| s != &name);
        outcome.conflicted.push((name, detail));
    }

    outcome.degraded = degradations.dirs;
    outcome.skipped_layouts = degradations.layouts;
    outcome.window_map = created_windows;
    Ok(outcome)
}

/// The sessions this restore actually put back, in full.
///
/// `created` and `adopted` minus anything a degradation was reported against:
/// a session whose pane came back in `$HOME` instead of its captured directory,
/// or whose window lost its layout, is *not* the captured session, and the
/// snapshot is the only remaining record of the difference. Those keep their
/// debt so a later capture carries them and a later restore tries again.
fn delivered_sessions(outcome: &RestoreOutcome) -> Vec<&str> {
    let degraded: HashSet<&str> = outcome
        .degraded
        .iter()
        .map(|d| d.session.as_str())
        .chain(outcome.skipped_layouts.iter().map(|l| l.session.as_str()))
        .collect();
    outcome
        .created
        .iter()
        .chain(outcome.adopted.iter())
        .map(String::as_str)
        .filter(|name| !degraded.contains(name))
        .collect()
}

/// Record every (captured window id, live window id) pair an adoption
/// matched, or say which one contradicts what the map already holds.
///
/// One captured window id must map to exactly one live window. Two entries
/// mean two sessions that shared a window in the snapshot are holding
/// separate windows now.
fn seed_window_map(
    map: &mut HashMap<String, String>,
    pairs: Vec<(String, String)>,
) -> Result<(), String> {
    for (captured, live) in pairs {
        match map.get(&captured) {
            Some(existing) if existing != &live => {
                return Err(format!(
                    "window {captured} is one window shared with another session in the \
                     snapshot, but this session holds {live} where that one holds {existing}"
                ))
            }
            Some(_) => {}
            None => {
                map.insert(captured, live);
            }
        }
    }
    Ok(())
}

/// Check, against the live server, that every window the snapshot links into
/// more than one session really is one window in all of them.
///
/// Only linked windows are inspected, and only in sessions the restore
/// claims to have delivered: an unshared window cannot fail this by
/// construction, a restore of a server with no links must not pay for a check
/// that cannot fire, and a session that already failed has its own error.
fn validate_links(
    tmux: &Tmux,
    tree: &SnapshotTree,
    created_windows: &HashMap<String, String>,
    succeeded: &HashSet<&str>,
) -> Vec<(String, String)> {
    // captured window id -> the sessions the snapshot links it into
    let mut sharers: HashMap<&str, Vec<&str>> = HashMap::new();
    for session in &tree.sessions {
        for window in &session.windows {
            sharers
                .entry(window.tmux_window_id.as_str())
                .or_default()
                .push(session.name.as_str());
        }
    }

    let mut problems = Vec::new();
    for (captured, sessions) in sharers {
        if sessions.len() < 2 {
            continue;
        }
        let Some(live_id) = created_windows.get(captured) else {
            // No session got far enough to place this window; whatever went
            // wrong is already reported against that session.
            continue;
        };
        for session in sessions {
            // Only sessions this restore claims to have delivered. One that
            // failed already carries its own error, may not exist on the
            // destination at all, and must not have a second, less
            // informative problem stapled to it — nor be allowed to turn a
            // partial restore into an aborted one by failing this lookup.
            if !succeeded.contains(session) {
                continue;
            }
            // An inspection failure is reported against the session rather
            // than propagated: `restore_tree` must not abandon sessions it
            // already rebuilt because one lookup went wrong at the end.
            let held = match window_index_in(tmux, session, live_id) {
                Ok(idx) => idx.is_some(),
                Err(e) => {
                    problems.push((
                        session.to_string(),
                        format!("could not verify the shared window {captured}: {e:#}"),
                    ));
                    continue;
                }
            };
            if !held {
                problems.push((
                    session.to_string(),
                    format!(
                        "window {captured} is shared with another session in the snapshot, \
                         but the live session does not hold {live_id}"
                    ),
                ));
            }
        }
    }
    problems
}

/// The index tmux currently reports for `window_id` inside `session`.
///
/// A linked window belongs to several sessions at once, so `select-window -t
/// @id` is ambiguous — it resolves to whichever session tmux finds first,
/// which may not be the one whose active window we are setting. Targeting
/// `session:index` is unambiguous.
fn window_index_in(tmux: &Tmux, session: &str, window_id: &str) -> Result<Option<String>> {
    let out = tmux.run(&[
        "list-windows",
        "-t",
        session,
        "-F",
        "#{window_index} #{window_id}",
    ])?;
    for line in out.lines() {
        if let Some((idx, id)) = line.trim().split_once(' ') {
            if id == window_id {
                return Ok(Some(idx.to_string()));
            }
        }
    }
    Ok(None)
}

/// Move `window_id` to index `idx` within `session`, unless it is already
/// there.
///
/// The guard is required, not an optimisation: `move-window` onto a window's
/// own index fails with "same index", which would abort the restore of a
/// session whose captured first index happens to equal the destination
/// server's `base-index` — i.e. the common case.
fn place_window(tmux: &Tmux, session: &str, window_id: &str, idx: u32) -> Result<()> {
    let want = idx.to_string();
    if window_index_in(tmux, session, window_id)?.as_deref() == Some(want.as_str()) {
        return Ok(());
    }
    tmux.run(&[
        "move-window",
        "-d",
        "-s",
        window_id,
        "-t",
        &format!("{session}:{want}"),
    ])?;
    Ok(())
}

fn restore_session(
    tmux: &Tmux,
    session: &SessionPlan,
    created_windows: &mut HashMap<String, String>,
    degraded: &mut Degradations,
) -> Result<()> {
    if session.windows.is_empty() {
        tmux.run(&["new-session", "-d", "-s", &session.name])?;
        return Ok(());
    }

    // A detached session with no attached client defaults to tmux's own
    // 80x24. If a captured window has 5+ panes that fit its wider original
    // terminal, splitting at 80x24 fails partway through ("no space for a
    // new pane") and the captured layout then fails to apply because the
    // pane count no longer matches it. Size the session up front from the
    // captured layout(s) so every pane has room to be created; `new-window`
    // takes no -x/-y of its own and simply inherits this session size.
    let (width, height) = session_dims(session);
    let width = width.to_string();
    let height = height.to_string();

    // dst_window_ids[i] is the destination window for session.windows[i],
    // tracked positionally so the active-window selection below can map a
    // captured (source) window id to the destination one.
    let mut dst_window_ids: Vec<String> = Vec::new();
    // Set when this session's first captured window turns out to be a link
    // to a window another session already built: a tmux session cannot be
    // created *around* an existing window, so it is created with a throwaway
    // window that is killed once at least one real window is linked in.
    let mut placeholder: Option<String> = None;
    let mut session_exists = false;

    // An index no captured window claims. `new-session` always puts its
    // initial window at the *destination* server's `base-index`, which has
    // nothing to do with the captured layout, so that window has to be moved
    // out of the way before the real windows are placed at their own
    // indices. Captured indices are unique per session (the schema enforces
    // it), so one past the largest is always free.
    let parking = session.windows.iter().map(|w| w.idx).max().unwrap_or(0) + 1;

    for window in &session.windows {
        // Windows go to the index they were captured at, not to whatever
        // tmux would pick next. Restoring 1 and 9 as 1 and 2 breaks every
        // script and every piece of muscle memory that says `session:9`, and
        // does it silently.
        let target = format!("{}:{}", session.name, window.idx);
        // Ruling 1: capture the real window id tmux assigns, and target every
        // later operation on this window by that id — never by "session:name",
        // since duplicate window names are legal and name-targeting would
        // silently operate on the wrong window.
        if let Some(existing) = created_windows.get(&window.tmux_window_id).cloned() {
            if !session_exists {
                let id = tmux
                    .run(&[
                        "new-session",
                        "-d",
                        "-s",
                        &session.name,
                        "-x",
                        &width,
                        "-y",
                        &height,
                        "-P",
                        "-F",
                        "#{window_id}",
                    ])?
                    .trim()
                    .to_string();
                // Park it: the placeholder must not be sitting on an index a
                // real window is about to claim.
                place_window(tmux, &session.name, &id, parking)?;
                placeholder = Some(id);
                session_exists = true;
            }
            tmux.run(&["link-window", "-d", "-s", &existing, "-t", &target])?;
            dst_window_ids.push(existing);
            continue;
        }

        let first = window.panes.first();
        let captured_cwd = first.map(|p| p.cwd.as_str()).unwrap_or("/");
        let (cwd, missing) = usable_cwd(captured_cwd);
        if missing {
            degraded.dirs.push(MissingDir {
                session: session.name.clone(),
                window: window.name.clone(),
                pane_index: first.map(|p| p.idx).unwrap_or(0),
                captured: captured_cwd.to_string(),
                used: cwd.clone(),
            });
        }
        let window_id = if session_exists {
            tmux.run(&[
                "new-window",
                "-d",
                "-t",
                &target,
                "-n",
                &window.name,
                "-c",
                &cwd,
                "-P",
                "-F",
                "#{window_id}",
            ])?
            .trim()
            .to_string()
        } else {
            session_exists = true;
            let id = tmux
                .run(&[
                    "new-session",
                    "-d",
                    "-s",
                    &session.name,
                    "-x",
                    &width,
                    "-y",
                    &height,
                    "-n",
                    &window.name,
                    "-c",
                    &cwd,
                    "-P",
                    "-F",
                    "#{window_id}",
                ])?
                .trim()
                .to_string();
            // `new-session` has no way to name the initial window's index,
            // so it lands on the destination's base-index and is moved here.
            place_window(tmux, &session.name, &id, window.idx)?;
            id
        };

        fill_window(tmux, &window_id, window, &session.name, degraded)?;
        created_windows.insert(window.tmux_window_id.clone(), window_id.clone());
        dst_window_ids.push(window_id);
    }

    // Safe by construction: a placeholder only exists when the first window
    // was a link, and that link has been made by now, so the session keeps
    // at least one window and does not die with the placeholder.
    if let Some(placeholder_id) = placeholder {
        tmux.run(&["kill-window", "-t", &placeholder_id])?;
    }

    if let Some(active_id) = &session.active_window_id {
        if let Some(i) = session
            .windows
            .iter()
            .position(|w| &w.tmux_window_id == active_id)
        {
            if let Some(idx) = window_index_in(tmux, &session.name, &dst_window_ids[i])? {
                tmux.run(&["select-window", "-t", &format!("{}:{}", session.name, idx)])?;
            }
        }
    }

    Ok(())
}

fn fill_window(
    tmux: &Tmux,
    window_id: &str,
    window: &WindowPlan,
    session: &str,
    degraded: &mut Degradations,
) -> Result<()> {
    // Ruling 2: restore the active pane, not just the active window/zoom.
    // Collect created pane ids in creation order — the window's initial
    // pane, then each split in captured order — and map the captured
    // active_pane_id to a position in that order, NOT to a tmux pane index
    // (select-layout renumbers indices by geometry; creation order and pane
    // identity stay stable).
    let initial_pane_id = tmux
        .run(&["list-panes", "-t", window_id, "-F", "#{pane_id}"])?
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_string();
    let mut created_ids = vec![initial_pane_id];

    for pane in window.panes.iter().skip(1) {
        let (cwd, missing) = usable_cwd(&pane.cwd);
        if missing {
            degraded.dirs.push(MissingDir {
                session: session.to_string(),
                window: window.name.clone(),
                pane_index: pane.idx,
                captured: pane.cwd.clone(),
                used: cwd.clone(),
            });
        }
        let pane_id = tmux
            .run(&[
                "split-window",
                "-t",
                window_id,
                "-c",
                &cwd,
                "-P",
                "-F",
                "#{pane_id}",
            ])?
            .trim()
            .to_string();
        created_ids.push(pane_id);
    }

    if !window.layout.is_empty() {
        // The captured layout is the one snapshot field replayed into tmux as
        // opaque text, so it is validated here first — see [`crate::layout`]
        // for what an unvalidated string can do to the whole server on the
        // tmux versions most users run.
        //
        // A layout captured at a different terminal size still applies; tmux
        // rescales it to the current window dimensions. Apply only after
        // every pane in this window exists.
        let skip = match layout::parse(&window.layout) {
            Err(e) => Some(format!("captured layout does not parse: {e}")),
            // Even a well-formed layout can be refused — the cell count may
            // not match the panes this window ended up with, or the cells may
            // not tile the window. That is a per-window problem and must not
            // abort the session, let alone the sessions after it: the panes
            // exist either way, and the alternative is losing windows that
            // have nothing wrong with them.
            Ok(_) => tmux
                .run(&["select-layout", "-t", window_id, &window.layout])
                .err()
                .map(|e| format!("tmux refused the captured layout: {e:#}")),
        };
        if let Some(reason) = skip {
            degraded.layouts.push(SkippedLayout {
                session: session.to_string(),
                window: window.name.clone(),
                layout: window.layout.clone(),
                reason,
            });
        }
    }

    if let Some(active_pane_id) = &window.active_pane_id {
        if let Some(i) = window
            .panes
            .iter()
            .position(|p| &p.tmux_pane_id == active_pane_id)
        {
            tmux.run(&["select-pane", "-t", &created_ids[i]])?;
        }
    }

    if window.zoomed {
        tmux.run(&["resize-pane", "-t", window_id, "-Z"])?;
    }

    Ok(())
}

#[derive(Debug)]
pub struct RestoreReport {
    pub snapshot_id: Option<i64>,
    pub attempt_id: Option<i64>,
    pub outcome: RestoreOutcome,
    /// `succeeded` | `partial` | `failed` | `nothing_to_restore` | `dry_run`.
    ///
    /// The three do-nothing outcomes used to collapse into one `"skipped"`,
    /// which told a consumer nothing about *why*. See
    /// [`crate::ipc::RestoreJson`] for the full contract.
    pub state: String,
    /// Stable snake_case token explaining `state`; never prose.
    pub reason: String,
    /// Whether the snapshot is still selectable, so a later `osm restore`
    /// picks it up again. False only after a fully verified success.
    pub retryable: bool,
}

fn record_object(
    conn: &Connection,
    attempt_id: i64,
    kind: &str,
    reference: &str,
    state: &str,
) -> Result<()> {
    record_object_detail(conn, attempt_id, kind, reference, state, None)
}

fn record_object_detail(
    conn: &Connection,
    attempt_id: i64,
    kind: &str,
    reference: &str,
    state: &str,
    detail: Option<&str>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO restore_objects (attempt_id, kind, ref, state, detail)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(attempt_id, kind, ref) DO UPDATE SET state = excluded.state, detail = excluded.detail",
        rusqlite::params![attempt_id, kind, reference, state, detail],
    )?;
    Ok(())
}

/// Runs the actual restore work for an attempt that already has a `running`
/// row in `restore_attempts`: seeds `pending` per-object rows, runs
/// `restore_tree`, and records the per-object outcome. Returns the computed
/// terminal attempt state (never "skipped" — that's decided by the caller
/// before an attempt row exists at all) alongside the outcome.
///
/// Deliberately does NOT write the attempt/snapshot terminal transition
/// itself: `run_restore` does that once, in one place, for both this
/// function's `Ok` and `Err` outcomes, so the transition can never be
/// skipped by a stray early return in here.
fn run_restore_attempt(
    conn: &Connection,
    tmux: &Tmux,
    tree: &SnapshotTree,
    attempt_id: i64,
) -> Result<(String, RestoreOutcome)> {
    for session in &tree.sessions {
        record_object(conn, attempt_id, "session", &session.name, "pending")?;
    }

    let outcome = restore_tree(tmux, tree)?;
    // The incarnation that was verified for the whole of the work, or `None`
    // if no single one was: see [`ServerWatch`]. Recording the identity of
    // whatever server happens to be answering at the end would hand a later
    // capture exactly the false match this column exists to prevent.
    let server = outcome.server.clone();

    // Which server incarnation the live ids below belong to. Without it a
    // mapping is eligible forever: a tmux restart within one boot hands out
    // the same `@1` to something unrelated, and a later capture would link a
    // carried session into it on the strength of a matching window name and
    // pane count. A `NULL` here makes this attempt's mappings unusable, which
    // costs a link rather than inventing one.
    conn.execute(
        "UPDATE restore_attempts SET destination_server = ?2 WHERE id = ?1",
        rusqlite::params![attempt_id, server],
    )?;

    // Written before the per-object rows, and for every window regardless of
    // how its session ended up: a session whose restore failed partway can
    // still have left real windows on the server, and a capture that later
    // carries that session forward has to know which ones. A mapping whose
    // window is not actually live is harmless — the capture looks the id up
    // among the windows it just saw and finds nothing.
    for (captured, live) in &outcome.window_map {
        conn.execute(
            "INSERT INTO restore_window_map (attempt_id, captured_window_id, live_window_id)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(attempt_id, captured_window_id)
             DO UPDATE SET live_window_id = excluded.live_window_id",
            rusqlite::params![attempt_id, captured, live],
        )?;
    }

    for name in &outcome.created {
        record_object(conn, attempt_id, "session", name, "done")?;
    }
    for name in &outcome.adopted {
        record_object(conn, attempt_id, "session", name, "adopted")?;
    }
    for name in &outcome.skipped {
        record_object(conn, attempt_id, "session", name, "skipped")?;
    }
    for (name, detail) in &outcome.failed {
        record_object_detail(conn, attempt_id, "session", name, "failed", Some(detail))?;
    }
    for (name, detail) in &outcome.conflicted {
        record_object_detail(
            conn,
            attempt_id,
            "session",
            name,
            "conflicted",
            Some(detail),
        )?;
    }

    // Degradations are recorded per *object*, not per session: the session
    // itself was created, so it keeps its `done` row, and the degraded row
    // says which window or pane inside it is not what was captured.
    for d in &outcome.degraded {
        record_object_detail(
            conn,
            attempt_id,
            "pane",
            &format!("{}:{}.{}", d.session, d.window, d.pane_index),
            "degraded",
            Some(&format!(
                "captured directory {:?} is gone; started in {:?} instead",
                d.captured, d.used
            )),
        )?;
    }
    for l in &outcome.skipped_layouts {
        record_object_detail(
            conn,
            attempt_id,
            "window",
            &format!("{}:{}", l.session, l.window),
            "degraded",
            Some(&format!("layout {:?} not applied: {}", l.layout, l.reason)),
        )?;
    }

    let ok_count = outcome.created.len() + outcome.adopted.len();
    let bad_count = outcome.failed.len() + outcome.skipped.len() + outcome.conflicted.len();
    // A restore that put the panes back somewhere other than where they were,
    // or without the geometry they had, is not a success. Counting it as one
    // is what retires the snapshot — and the snapshot is the only remaining
    // record of the captured directory or layout, so retiring it turns a
    // temporary problem (a volume not mounted yet, a layout this tmux cannot
    // apply to a window it has not finished building) into a permanent loss.
    // `partial` keeps it selectable for the next run.
    let degraded_count = outcome.degraded.len() + outcome.skipped_layouts.len();
    // A restore whose server was replaced under it verified nothing, however
    // few objects it happened to touch — including none at all.
    let state = if outcome.server_changed.is_some() {
        "failed"
    } else if bad_count == 0 && degraded_count == 0 {
        "succeeded"
    } else if ok_count > 0 {
        "partial"
    } else {
        "failed"
    };

    Ok((state.to_string(), outcome))
}

/// Best-effort: marks the attempt failed and hands the snapshot back to
/// `complete` after `run_restore_attempt` returned an error too early for any
/// per-object bookkeeping to be trustworthy. A secondary failure here (e.g.
/// the DB itself is unwritable) is logged and swallowed rather than masking
/// the original error, since there is no further cleanup step to fall back to.
///
/// The snapshot goes back to `complete`, not to `failed`: see
/// [`retire_or_return`].
fn finalize_failed_attempt(conn: &Connection, attempt_id: i64, snapshot_id: i64, cause: &Error) {
    // `{:#}` renders the full cause chain, not just the outermost context —
    // see the matching note in restore_tree.
    if let Err(e) = record_object_detail(
        conn,
        attempt_id,
        "session",
        "*",
        "failed",
        Some(&format!("{cause:#}")),
    ) {
        eprintln!("osm: restore attempt {attempt_id}: failed to record failure detail: {e}");
    }
    if let Err(e) = conn.execute(
        "UPDATE restore_attempts SET state = 'failed', finished_at = ?2 WHERE id = ?1",
        rusqlite::params![attempt_id, boot::now_epoch()],
    ) {
        eprintln!("osm: restore attempt {attempt_id}: failed to mark attempt failed: {e}");
    }
    if let Err(e) = retire_or_return(conn, snapshot_id, "failed") {
        eprintln!(
            "osm: restore attempt {attempt_id}: failed to return snapshot {snapshot_id} \
             to the restorable pool: {e}"
        );
    }
}

/// The snapshot's fate once an attempt reaches a terminal state.
///
/// Only a fully verified success retires a snapshot. Anything else hands it
/// back to `complete`, which is the one state [`snapshots::select_restore_source`]
/// will pick, so the next `osm restore` tries again.
///
/// This is the difference between a transient failure — a directory not yet
/// mounted, tmux briefly unavailable, the database momentarily unwritable —
/// costing one boot and costing the user their state forever. `restored` and
/// `failed` are both invisible to the selector, so the previous behaviour
/// permanently retired the *only* copy of a pre-reboot layout on the first
/// hiccup. Retrying is safe because restore is idempotent: a session that is
/// already there and matches is adopted rather than rebuilt (see
/// [`adoption_mismatch`]), and one that is there but differs is left alone.
fn retire_or_return(conn: &Connection, snapshot_id: i64, attempt_state: &str) -> Result<()> {
    if attempt_state == "succeeded" {
        snapshots::set_state(conn, snapshot_id, "restored")?;
        // Everything in it is verifiably back, so nothing is owed.
        snapshots::set_unresolved(conn, snapshot_id, false)?;
        return Ok(());
    }
    snapshots::set_state(conn, snapshot_id, "complete")?;
    // Handing it back to `complete` is not enough on its own: `complete` is
    // ordered by recency, so the very capture that records this boot's
    // incomplete topology would supersede it, and retention would then delete
    // it. `unresolved` says the difference still matters, which exempts it
    // from pruning until a capture has carried the missing sessions forward.
    //
    // Recomputed from the per-session debt rather than set outright: the
    // sessions this attempt *did* deliver had their debt discharged in
    // `run_restore`, and only the rest keep the snapshot alive. Setting the
    // flag wholesale is what used to make a later capture carry sessions the
    // restore had put back — and, once the user closed one of them, put it
    // back again.
    snapshots::refresh_unresolved(conn, snapshot_id)?;
    Ok(())
}

/// True when the snapshot is still selectable by a later restore.
pub fn is_retryable(attempt_state: &str) -> bool {
    attempt_state != "succeeded"
}

/// Rebuilds tmux topology from the newest snapshot of a previous boot.
///
/// # The caller must already hold the restore lock
///
/// Not merely to serialise restores: the first thing this does is reclaim
/// snapshots wedged by a restore that was killed mid-run, and that reclaim is
/// only sound while we hold the lock (see
/// [`snapshots::reclaim_orphaned_restores`]).
pub fn run_restore(conn: &mut Connection, tmux: &Tmux, dry_run: bool) -> Result<RestoreReport> {
    let boot_id = boot::current_boot_id()?;

    // Before selecting anything: a restore that died between
    // `set_state(restore_in_progress)` and its terminal write left the
    // snapshot in a state `select_restore_source` cannot see. Reclaiming it
    // first is what makes the newest pre-reboot snapshot reachable again
    // instead of "nothing to restore" forever.
    let reclaimed = snapshots::reclaim_orphaned_restores(conn)?;
    if reclaimed > 0 {
        eprintln!("osm: reclaimed {reclaimed} snapshot(s) from an interrupted restore");
    }

    let Some(snapshot_id) = snapshots::select_restore_source(conn, &boot_id)? else {
        return Ok(RestoreReport {
            snapshot_id: None,
            attempt_id: None,
            outcome: RestoreOutcome::default(),
            state: "nothing_to_restore".to_string(),
            reason: "no_previous_boot_snapshot".to_string(),
            retryable: false,
        });
    };

    let tree = model::load(conn, snapshot_id)?;

    if dry_run {
        return Ok(RestoreReport {
            snapshot_id: Some(snapshot_id),
            attempt_id: None,
            outcome: RestoreOutcome::default(),
            state: "dry_run".to_string(),
            reason: "dry_run".to_string(),
            // Nothing was touched, so the source is still there for a real
            // run.
            retryable: true,
        });
    }

    snapshots::set_state(conn, snapshot_id, "restore_in_progress")?;
    // Marked unresolved from the moment the restore begins, not when it
    // finishes reporting: a restore killed mid-run never reaches the
    // reporting code at all, and until something proves otherwise this
    // snapshot holds sessions nothing has recovered. Only a verified success
    // clears it.
    snapshots::set_unresolved(conn, snapshot_id, true)?;
    conn.execute(
        "INSERT INTO restore_attempts (snapshot_id, started_at, state)
         VALUES (?1, ?2, 'running')",
        rusqlite::params![snapshot_id, boot::now_epoch()],
    )?;
    let attempt_id = conn.last_insert_rowid();

    // From here on, the attempt row exists and must never be left at
    // `running`: every path below — success, partial, or this function's
    // own internal error — ends in exactly one terminal-state write.
    let (state, mut outcome) = match run_restore_attempt(conn, tmux, &tree, attempt_id) {
        Ok(pair) => pair,
        Err(e) => {
            finalize_failed_attempt(conn, attempt_id, snapshot_id, &e);
            return Ok(RestoreReport {
                snapshot_id: Some(snapshot_id),
                attempt_id: Some(attempt_id),
                outcome: RestoreOutcome::default(),
                state: "failed".to_string(),
                reason: "restore_failed".to_string(),
                retryable: true,
            });
        }
    };

    // A verified success is the only path that retires the source, and it
    // must not retire it into a gap. Every hook fired *by* the restore loses
    // the race for the exclusive lock this function's caller holds and
    // returns without capturing, and the daemon is still sleeping out its
    // first interval, so between the retirement and the next capture there is
    // no `complete` snapshot on the machine at all — a power loss in that
    // window costs the user everything. Setting the dirty flag does not close
    // it; only writing the replacement does.
    //
    // The debt this attempt discharged is discharged *inside* that same
    // transaction: the publication writes a fresh snapshot whose
    // carry-forward asks the source what it is still owed, and a session this
    // restore verifiably delivered must not be carried into the very snapshot
    // that already holds it live. Every other path discharges it below.
    let mut state = state;
    let mut unsecured = false;
    if state == "succeeded" {
        let published = publish_current_boot(
            conn,
            tmux,
            attempt_id,
            snapshot_id,
            outcome.server.as_deref(),
            &delivered_sessions(&outcome),
        );
        match published {
            Ok(()) => {}
            // The sessions really are on the server; only the record of them
            // is missing. Its own terminal state, not a success with a
            // footnote. Storing this as `succeeded` produced a report that
            // contradicted itself — `state: "succeeded", retryable: true` —
            // and, worse, exited 0, so `osm-restore.service` was a clean
            // success and systemd had no reason to restart the one thing that
            // could still have secured the user's state.
            Err(PublishFailure::Other(e)) => {
                eprintln!(
                    "osm: the sessions were restored but this boot's snapshot could not be \
                     published ({e:#}); snapshot {snapshot_id} stays restorable"
                );
                unsecured = true;
                state = "unsecured".to_string();
            }
            // The opposite case, and the one that used to be reported as a
            // success: the server that did the work is not the server
            // answering now. Whatever it built died with it, the topology
            // about to be published is a different server's, and the source
            // is the only remaining record of the sessions that are gone.
            Err(PublishFailure::ServerMoved(why)) => {
                eprintln!(
                    "osm: {why}; snapshot {snapshot_id} stays restorable and nothing it \
                     holds has been discharged"
                );
                disown_attempt(conn, attempt_id, &mut outcome, &why)?;
                state = "failed".to_string();
            }
        }
    }

    // `publish_current_boot` wrote the attempt's terminal state and the
    // source's fate itself, in one transaction; every other path writes them
    // here.
    if state != "succeeded" {
        snapshots::resolve_sessions(conn, snapshot_id, &delivered_sessions(&outcome))?;
        snapshots::refresh_unresolved(conn, snapshot_id)?;
        conn.execute(
            "UPDATE restore_attempts SET state = ?2, finished_at = ?3 WHERE id = ?1",
            rusqlite::params![attempt_id, state, boot::now_epoch()],
        )?;
        if unsecured {
            // The sessions are back, but the only durable record of them is
            // still the source, so the source stays selectable. Nothing is
            // owed: every session in it is live and was verified, so there is
            // nothing for a capture to carry forward.
            snapshots::set_state(conn, snapshot_id, "complete")?;
            snapshots::refresh_unresolved(conn, snapshot_id)?;
        } else {
            retire_or_return(conn, snapshot_id, &state)?;
        }
    }

    let reason = match state.as_str() {
        "succeeded" => "ok",
        "unsecured" => "post_restore_capture_failed",
        "partial" => "partial_restore",
        _ => "restore_failed",
    };

    let retryable = is_retryable(&state);
    Ok(RestoreReport {
        snapshot_id: Some(snapshot_id),
        attempt_id: Some(attempt_id),
        outcome,
        state,
        reason: reason.to_string(),
        retryable,
    })
}

/// Why this boot's snapshot could not be published, and the two answers are
/// opposites.
enum PublishFailure {
    /// The server answering now is not the incarnation this restore built on.
    /// Nothing it built is on the machine, and the topology that would have
    /// been published belongs to somebody else.
    ServerMoved(String),
    /// Everything else: the collection or the write failed on its own terms,
    /// and the sessions this restore built really are still on the server.
    Other(Error),
}

/// Capture the tmux server as the restore left it and publish it as this
/// boot's snapshot, discharging the debt this attempt delivered and retiring
/// the source — all in the same transaction.
///
/// The caller holds the restore lock, so nothing else can capture right now —
/// which is exactly why this has to. The collection itself happens outside
/// the transaction, since it shells out to tmux several times and a
/// transaction held across that would block every other writer for the
/// duration.
///
/// # Why the identity is a parameter and not something read here
///
/// `expected` is the incarnation [`ServerWatch`] verified continuously for the
/// whole of the work, and the topology collected below has to have come from
/// **that** server for any of this to be true. It is not a formality: a server
/// that dies after the last per-session check and is replaced before this call
/// leaves `collect` reading the *replacement's* topology, while the very same
/// transaction marks the attempt succeeded and retires the source. Every
/// session that died with the first server is then in neither durable
/// snapshot — the published one never saw them, and the source that held them
/// has been retired — and the run reports a success.
///
/// `None` is a legitimate value and means "this restore saw no server at all",
/// which must then still be true here: a server that has appeared since is as
/// much a mismatch as a different one.
fn publish_current_boot(
    conn: &mut Connection,
    tmux: &Tmux,
    attempt_id: i64,
    snapshot_id: i64,
    expected: Option<&str>,
    delivered: &[&str],
) -> std::result::Result<(), PublishFailure> {
    let topo = match crate::capture::collect(tmux) {
        Ok(topo) => topo,
        // A collection that fails because the destination is no longer the
        // server this restore built on is not a reporting problem — it is the
        // work being gone. The commonest shape of it is the plainest: the
        // server died after the last per-session check and nothing replaced
        // it, so there is no topology to read at all.
        Err(e) => {
            let now = tmux.running_server_incarnation();
            let still_ours = matches!(&now, Ok(now) if now.as_deref() == expected);
            return Err(if still_ours {
                PublishFailure::Other(e)
            } else {
                PublishFailure::ServerMoved(format!(
                    "the tmux server this restore built on ({}) could not be read back \
                     ({e:#}) and is not the server answering now ({}), so nothing it \
                     built can be shown to be there",
                    expected.unwrap_or("no server"),
                    match &now {
                        Ok(Some(id)) => id.clone(),
                        Ok(None) => "no server".to_string(),
                        Err(e) => format!("a server that would not identify itself: {e:#}"),
                    },
                ))
            });
        }
    };
    if topo.server.as_deref() != expected {
        return Err(PublishFailure::ServerMoved(format!(
            "the tmux server this restore built on ({}) is not the one that answered \
             when its work came to be written down ({}), so the topology on the machine \
             now is not the one it made",
            expected.unwrap_or("no server"),
            topo.server.as_deref().unwrap_or("no server"),
        )));
    }
    (|| -> Result<()> {
        let tx = conn.transaction()?;
        // Before `write_topology_in`, whose carry-forward asks the source what
        // it is still owed.
        snapshots::resolve_sessions(&tx, snapshot_id, delivered)?;
        snapshots::refresh_unresolved(&tx, snapshot_id)?;
        crate::capture::write_topology_in(&tx, &topo, "post_restore")?;
        tx.execute(
            "UPDATE restore_attempts SET state = 'succeeded', finished_at = ?2 WHERE id = ?1",
            rusqlite::params![attempt_id, boot::now_epoch()],
        )?;
        tx.execute(
            "UPDATE snapshots SET state = 'restored', unresolved = 0 WHERE id = ?1",
            [snapshot_id],
        )?;
        tx.execute(
            "UPDATE session_rows SET unresolved = 0 WHERE snapshot_id = ?1",
            [snapshot_id],
        )?;
        tx.commit()?;
        Ok(())
    })()
    .map_err(PublishFailure::Other)
}

/// Take back every claim an attempt made about work it turns out cannot be
/// attributed to one server.
///
/// The per-object rows, the `destination_server` stamp and the outcome itself
/// were all written on the strength of a restore that looked complete. Once
/// the server that did the work is gone, the sessions will be adopted by the
/// next restore if they are genuinely live and stay owed if they are not — but
/// only if nothing here says they were delivered.
///
/// The `restore_window_map` rows are left where they are on purpose: with
/// `destination_server` back to `NULL` no capture will ever select them (see
/// [`crate::capture`]), and they are the only remaining evidence of what the
/// dead server was asked to build.
fn disown_attempt(
    conn: &Connection,
    attempt_id: i64,
    outcome: &mut RestoreOutcome,
    why: &str,
) -> Result<()> {
    let mut lost = std::mem::take(&mut outcome.created);
    lost.append(&mut outcome.adopted);
    lost.sort();
    for name in lost {
        record_object_detail(conn, attempt_id, "session", &name, "failed", Some(why))?;
        outcome.failed.push((name, why.to_string()));
    }
    outcome.window_map.clear();
    outcome.server = None;
    outcome.server_changed = Some(why.to_string());
    conn.execute(
        "UPDATE restore_attempts SET destination_server = NULL WHERE id = ?1",
        [attempt_id],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    //! The one property of a restore that no integration test can reach:
    //! what [`publish_current_boot`] does when the server changes between the
    //! last per-session identity check and the publication itself.
    //!
    //! That window is a handful of database writes and three `list-*` calls
    //! wide, with no tmux command in it to hang a hook on, so it is driven
    //! here — from inside the crate, against the private function, with a real
    //! tmux server really replaced in the middle.
    //!
    //! Every server started here is private, addressed by a `-L` socket name
    //! carrying this process's id. Nothing in this module can name the default
    //! server: `Tmux::default_server` does not exist in a
    //! `--no-default-features` build, which is how the suite is run.

    use super::*;
    use crate::{capture, db};

    struct Server(Tmux);

    impl Server {
        fn start(label: &str) -> Self {
            Server(Tmux::with_socket(&format!(
                "osm-publish-{}-{}",
                label,
                std::process::id()
            )))
        }
        fn t(&self) -> &Tmux {
            &self.0
        }
        /// Start a server on this socket, waiting out the instant in which a
        /// killed predecessor is still unlinking its socket.
        fn session(&self, name: &str) {
            let args = ["new-session", "-d", "-s", name, "-c", "/tmp"];
            let mut started = self.0.run(&args);
            for _ in 0..50 {
                if started.is_ok() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
                started = self.0.run(&args);
            }
            started.expect("a server on the socket this test owns");
        }
        /// Kill this server and remove the socket file it owns — never a
        /// glob, never a directory.
        fn kill(&self) {
            let _ = self.0.run(&["kill-server"]);
            let Some(name) = self.0.socket() else { return };
            let Ok(entries) = std::fs::read_dir("/tmp") else {
                return;
            };
            for entry in entries.flatten() {
                if entry.file_name().to_string_lossy().starts_with("tmux-") {
                    let _ = std::fs::remove_file(entry.path().join(name));
                }
            }
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.kill();
        }
    }

    fn owed(conn: &Connection, snap: i64) -> Vec<String> {
        let mut names: Vec<String> = conn
            .prepare("SELECT name FROM session_rows WHERE snapshot_id=?1 AND unresolved=1")
            .unwrap()
            .query_map([snap], |r| r.get(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        names.sort();
        names
    }

    fn reasons(conn: &Connection) -> Vec<String> {
        conn.prepare("SELECT reason FROM snapshots ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap()
    }

    /// A restore that really ran, and a destination server that is then really
    /// replaced before its work is written down.
    ///
    /// Publishing then collects the *replacement's* topology — a decoy session
    /// that has nothing to do with the snapshot — while the same transaction
    /// marks the attempt succeeded and retires the source. Both durable
    /// records would then be missing every session that died with the first
    /// server, and the run would report a success.
    #[test]
    fn a_server_replaced_between_the_last_check_and_the_publication_publishes_nothing() {
        let src = Server::start("src");
        src.session("alpha");
        src.t()
            .run(&["new-session", "-d", "-s", "beta", "-c", "/tmp"])
            .unwrap();

        let tmp = tempfile::tempdir().unwrap();
        let mut conn = db::open(&tmp.path().join("state.db")).unwrap();
        let snap = capture::snapshot(&mut conn, src.t(), "test").unwrap();
        conn.execute("UPDATE snapshots SET boot_id='boot-a' WHERE id=?1", [snap])
            .unwrap();
        conn.execute(
            "UPDATE session_rows SET unresolved=1 WHERE snapshot_id=?1",
            [snap],
        )
        .unwrap();
        src.kill();

        let dst = Server::start("dst");
        let tree = model::load(&conn, snap).unwrap();
        let outcome = restore_tree(dst.t(), &tree).unwrap();
        assert_eq!(
            outcome.created,
            vec!["alpha".to_string(), "beta".to_string()],
            "the fixture must actually restore both sessions: {outcome:?}"
        );
        let anchor = outcome
            .server
            .clone()
            .expect("a restore that built two sessions must name the server it built them on");

        conn.execute(
            "INSERT INTO restore_attempts (snapshot_id, started_at, state, destination_server)
             VALUES (?1, ?2, 'running', ?3)",
            rusqlite::params![snap, boot::now_epoch(), anchor],
        )
        .unwrap();
        let attempt = conn.last_insert_rowid();

        // The window the whole finding lives in: the last `ServerWatch::check`
        // is behind us and nothing has been written down yet.
        dst.kill();
        dst.session("decoy");
        assert_ne!(
            dst.t().server_incarnation().unwrap(),
            anchor,
            "the fixture must actually have replaced the server"
        );

        let delivered = delivered_sessions(&outcome);
        let failure = publish_current_boot(
            &mut conn,
            dst.t(),
            attempt,
            snap,
            outcome.server.as_deref(),
            &delivered,
        )
        .expect_err("a topology from another server is not this restore's work");
        match &failure {
            PublishFailure::ServerMoved(why) => {
                assert!(
                    why.contains(&anchor),
                    "the refusal must name the incarnation the work belonged to: {why}"
                );
            }
            PublishFailure::Other(e) => {
                panic!("a replaced server is not an ordinary publication failure: {e:#}")
            }
        }

        assert_eq!(
            reasons(&conn),
            vec!["test".to_string()],
            "nothing may be published for a server that did not do the work"
        );
        let state: String = conn
            .query_row("SELECT state FROM snapshots WHERE id=?1", [snap], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            state, "complete",
            "the source is the only remaining record of alpha and beta, so it must not \
             be retired"
        );
        assert_eq!(
            owed(&conn, snap),
            vec!["alpha".to_string(), "beta".to_string()],
            "no session may have its debt discharged by a server that never held it"
        );
        let attempt_state: String = conn
            .query_row(
                "SELECT state FROM restore_attempts WHERE id=?1",
                [attempt],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            attempt_state, "running",
            "publication writes the terminal state only when it publishes"
        );
    }

    /// The control for the test above: with the same server still answering,
    /// the publication goes through and does every part of its job.
    #[test]
    fn a_publication_into_the_server_that_did_the_work_still_succeeds() {
        let src = Server::start("ok-src");
        src.session("alpha");

        let tmp = tempfile::tempdir().unwrap();
        let mut conn = db::open(&tmp.path().join("state.db")).unwrap();
        let snap = capture::snapshot(&mut conn, src.t(), "test").unwrap();
        conn.execute("UPDATE snapshots SET boot_id='boot-a' WHERE id=?1", [snap])
            .unwrap();
        conn.execute(
            "UPDATE session_rows SET unresolved=1 WHERE snapshot_id=?1",
            [snap],
        )
        .unwrap();
        src.kill();

        let dst = Server::start("ok-dst");
        let tree = model::load(&conn, snap).unwrap();
        let outcome = restore_tree(dst.t(), &tree).unwrap();
        assert_eq!(outcome.created, vec!["alpha".to_string()], "{outcome:?}");

        conn.execute(
            "INSERT INTO restore_attempts (snapshot_id, started_at, state) VALUES (?1, ?2, 'running')",
            rusqlite::params![snap, boot::now_epoch()],
        )
        .unwrap();
        let attempt = conn.last_insert_rowid();

        let delivered = delivered_sessions(&outcome);
        publish_current_boot(
            &mut conn,
            dst.t(),
            attempt,
            snap,
            outcome.server.as_deref(),
            &delivered,
        )
        .unwrap_or_else(|e| match e {
            PublishFailure::ServerMoved(why) => panic!("the server never moved: {why}"),
            PublishFailure::Other(e) => panic!("publication failed: {e:#}"),
        });

        assert_eq!(
            reasons(&conn),
            vec!["test".to_string(), "post_restore".to_string()]
        );
        assert!(
            owed(&conn, snap).is_empty(),
            "a delivered session is not owed"
        );
        let state: String = conn
            .query_row("SELECT state FROM snapshots WHERE id=?1", [snap], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(state, "restored");
    }
}
