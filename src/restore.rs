use crate::agent::{self, resume, AgentKind};
use crate::config::Config;
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
    /// Captured pane id → the pane id it now has on the destination server,
    /// for every pane this restore created or verifiably adopted.
    ///
    /// The only identity a resume may be addressed by. A captured pane id
    /// names a pane on a server that no longer exists, and nothing else on
    /// the machine can say which live pane took its place — so the restore
    /// that made it writes it down, once, at the moment it made it. For a
    /// created window that is `fill_window`'s own creation order; for an
    /// adopted one it is the layout-cell order [`adoption_mismatch`] already
    /// validated the session against (see [`equiv::pane_pairs`]).
    ///
    /// Emptied, exactly like [`Self::window_map`], when the run cannot be
    /// attributed to a single server incarnation: half these ids would name
    /// panes on a server that is gone and the other half panes on one that
    /// never held the captured session, and nothing downstream can tell them
    /// apart.
    pub pane_map: HashMap<String, String>,
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
    /// `(pane label, outcome)` for every pane this restore tried — or
    /// deliberately declined — to hand a resume command to, in the shape
    /// `"session:window.pane_idx"`. Empty when nothing was bound to a
    /// conversation, when `agents.auto_resume` is off, or before this field
    /// is populated (every path above [`resume_agents`] leaves it empty by
    /// [`Default`]).
    ///
    /// Populated by [`resume_agents`], called from [`run_restore`] after
    /// every pane exists and the captured layouts are applied — see that
    /// call site for why the ordering matters.
    pub agent_outcomes: Vec<(String, resume::Outcome)>,
    /// `(session name, outcome)` for every session with a recorded terminal
    /// window that this restore tried — or deliberately declined — to give
    /// one back. Empty when the source snapshot recorded no placement at all,
    /// which is every snapshot taken on a machine with no compositor.
    ///
    /// Populated by [`place_windows`], called from [`run_restore`] after the
    /// resume pass and before the source's fate is decided — see that call
    /// site.
    pub window_outcomes: Vec<(String, crate::desktop::PlaceOutcome)>,
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
    /// Whether tmux owns the name (`automatic-rename` on) rather than the user.
    auto_named: bool,
}

fn live_windows(tmux: &Tmux, session: &str) -> Result<Vec<LiveWindow>> {
    const FIELDS: [&str; 7] = [
        "window_id",
        "window_index",
        "window_active",
        "window_zoomed_flag",
        // A window *option*, so its real hyphenated name — see
        // `crate::tmux::WINDOW_FIELDS`.
        "automatic-rename",
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
            auto_named: f[4] == "1",
            name: f[5].clone(),
            layout: f[6].clone(),
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
                equiv::window_shape(
                    w.id,
                    w.idx,
                    w.name,
                    Some(w.auto_named),
                    w.layout,
                    w.zoomed,
                    own,
                )
            })
            .collect(),
        active_window,
    })
}

/// The result of inspecting a live session that holds a captured session's
/// name.
enum Adoption {
    /// It really is the captured session. Carries every
    /// (captured window id, live window id) pair the comparison matched up —
    /// which is what lets a linked window be re-linked rather than rebuilt —
    /// and every (captured pane id, live pane id) pair, in the layout-cell
    /// order the comparison itself walked.
    Match(Vec<(String, String)>, Vec<(String, String)>),
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
        None => Adoption::Match(
            equiv::window_pairs(&want, &live),
            equiv::pane_pairs(&want, &live),
        ),
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
    // Captured pane id -> the live pane it became, for every pane this run
    // created or adopted. Recorded here, where it is known, because it cannot
    // be reconstructed anywhere else — see `RestoreOutcome::pane_map`.
    let mut created_panes: HashMap<String, String> = HashMap::new();

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
                Adoption::Match(window_pairs, pane_pairs) => {
                    match seed_window_map(&mut created_windows, window_pairs) {
                        Ok(()) => {
                            created_panes.extend(pane_pairs);
                            outcome.adopted.push(session.name.clone())
                        }
                        // The live session matches on its own, but the window it
                        // holds is not the one another session in this same tree
                        // already accounted for — so the captured link relation
                        // is not what is live. Reported rather than adopted: the
                        // snapshot is the only remaining record that those
                        // sessions shared a window.
                        Err(why) => outcome.conflicted.push((session.name.clone(), why)),
                    }
                }
                Adoption::Mismatch(why) => outcome.conflicted.push((session.name.clone(), why)),
            }
        } else {
            // A single session's restore failing must not abort the rest: the
            // sessions tmux already created for earlier entries in this loop
            // are real, live side effects and every other session in the tree
            // still deserves its own attempt.
            match restore_session(
                tmux,
                session,
                &mut created_windows,
                &mut created_panes,
                &mut degradations,
            )
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
        // Neither `created_windows` nor `created_panes` is moved into the
        // outcome: a mapping records which live window or pane a captured one
        // became, half of these name objects on a server that is gone and the
        // other half objects on one that never held the captured session, and
        // nothing downstream can tell them apart. An empty `pane_map` is what
        // stops `resume_agents` sending anything at all on this path.
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
    outcome.pane_map = created_panes;
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

/// One window this attempt reported as placed: the session it was opened for,
/// and which window it is.
struct PlacedClaim<'a> {
    session: &'a str,
    window: &'a crate::desktop::PlacedWindow,
}

/// The windows this attempt reported as placed.
///
/// The claim the publication is then held to: a window that was spawned,
/// found, moved, and attached, and that the snapshot published afterwards
/// must still hold — that window, holding that session, where it was put.
/// Only `Placed` counts; every other outcome already says the work was not
/// done.
fn placed_windows(outcome: &RestoreOutcome) -> Vec<PlacedClaim<'_>> {
    outcome
        .window_outcomes
        .iter()
        .filter_map(|(session, o)| match o {
            crate::desktop::PlaceOutcome::Placed(window) => Some(PlacedClaim {
                session: session.as_str(),
                window,
            }),
            _ => None,
        })
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
    created_panes: &mut HashMap<String, String>,
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
        // Only a name somebody actually chose is applied. A captured name that
        // tmux owned (`automatic-rename` on) is one it derived from whatever
        // happened to be in the window's foreground at capture time — usually
        // `bash`, and, when the capture caught a window in the moment after it
        // was created, `tmux`. Replaying it through `-n` would be wrong twice
        // over: it names the window after a process that is not running in it
        // any more, and `-n` *switches `automatic-rename` off*, so that stale
        // derived name would then stick permanently to a window the user had
        // always let tmux name. Omitting `-n` leaves the flag on and tmux
        // re-derives the name — arriving at what the user saw before the
        // reboot instead of a fossil of the instant of capture.
        //
        // `None` — a snapshot row from before the flag was recorded — keeps
        // the old behaviour and applies the name. It may be a name the user
        // chose, and losing one of those across a reboot is the failure this
        // whole engine exists to prevent; freezing a stale `bash` on a window
        // is a cosmetic cost the user can undo in one command.
        //
        // Nothing downstream depends on the applied name: every tmux command
        // in this restore targets windows by id, and equivalence does not
        // compare an auto name at all (see `equiv`).
        let name_arg: Vec<&str> = if window.auto_named == Some(true) {
            Vec::new()
        } else {
            vec!["-n", window.name.as_str()]
        };
        let window_id = if session_exists {
            let mut args: Vec<&str> = vec!["new-window", "-d", "-t", target.as_str()];
            args.extend_from_slice(&name_arg);
            args.extend_from_slice(&["-c", cwd.as_str(), "-P", "-F", "#{window_id}"]);
            tmux.run(&args)?.trim().to_string()
        } else {
            session_exists = true;
            let mut args: Vec<&str> = vec![
                "new-session",
                "-d",
                "-s",
                session.name.as_str(),
                "-x",
                width.as_str(),
                "-y",
                height.as_str(),
            ];
            args.extend_from_slice(&name_arg);
            args.extend_from_slice(&["-c", cwd.as_str(), "-P", "-F", "#{window_id}"]);
            let id = tmux.run(&args)?.trim().to_string();
            // `new-session` has no way to name the initial window's index,
            // so it lands on the destination's base-index and is moved here.
            place_window(tmux, &session.name, &id, window.idx)?;
            id
        };

        fill_window(
            tmux,
            &window_id,
            window,
            &session.name,
            created_panes,
            degraded,
        )?;
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
    created_panes: &mut HashMap<String, String>,
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

    // Written down here, next to the creation that established it: pane
    // `window.panes[i]` was rebuilt as `created_ids[i]`, and after this
    // function returns nothing can tell that from the server. `zip` rather
    // than indexing, so a window whose splits ran out of room records the
    // panes it did create instead of panicking on the ones it did not.
    for (captured, live) in window.panes.iter().zip(created_ids.iter()) {
        created_panes.insert(captured.tmux_pane_id.clone(), live.clone());
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

/// Whether `outcomes` contain work the restore did not do.
///
/// A pane left as a bare shell when it should hold a conversation is exactly
/// that: [`resume::Outcome::Failed`], [`resume::Outcome::PaneBusy`] and
/// [`resume::Outcome::PaneMissing`] all mean the conversation did not come
/// back, so the restore must not be reported as a full success — the
/// snapshot is the only remaining record of which pane held which
/// conversation, and retiring it over a shell that should have been a
/// conversation loses that permanently.
///
/// [`resume::Outcome::OwnershipUnknown`] is a failure too, and it is the one
/// that hid behind `Unsupported`. osm could not establish whether the
/// conversation was already open somewhere, so it deliberately sent nothing —
/// leaving a bare shell in a pane that should hold a conversation, which is
/// exactly the state above. It is also *retryable* in a way the others are
/// not: the descriptor that could not be identified belongs to a process that
/// may well be gone by the next attempt.
///
/// [`resume::Outcome::Resumed`] is success. [`resume::Outcome::ActiveElsewhere`]
/// is deliberately **not** a failure: the conversation is alive in another
/// pane already, and leaving this one as a shell instead of forcing a second
/// attach is the correct, safe outcome. [`resume::Outcome::Unsupported`] is
/// likewise not a failure — nothing here was capable of resuming it, which
/// is not the restore's fault.
pub fn agent_outcomes_are_degraded(outcomes: &[(String, resume::Outcome)]) -> bool {
    outcomes.iter().any(|(_, outcome)| {
        matches!(
            outcome,
            resume::Outcome::Failed(_)
                | resume::Outcome::PaneBusy
                | resume::Outcome::PaneMissing
                | resume::Outcome::OwnershipUnknown
        )
    })
}

/// The pane rows a snapshot bound to a conversation, grouped by the window
/// they belong to.
///
/// One entry per window that has at least one bound pane. Only the bound
/// panes are carried: since the restore now records which live pane each
/// captured one became (see [`RestoreOutcome::pane_map`]), a pane's identity
/// no longer has to be inferred from where its siblings landed.
struct BoundWindow {
    /// **Every** session this window is linked into in the snapshot, in link
    /// order. All of them, not one of them: a resume may only be delivered
    /// into a window whose sessions this restore actually put back, and a
    /// window linked into a delivered session and a conflicted one is not
    /// that. Picking "any one session" is how a window belonging to a
    /// conflicted session could be treated as delivered.
    ///
    /// Each entry is `(session name, this window's index within it)` — the two
    /// halves of the place a debt is recorded at, which is what lets a
    /// confirmed resume discharge the debt for *this* pane and no other.
    sessions: Vec<(String, u32)>,
    window_name: String,
    /// `(captured pane id, captured idx, kind, native id)`, ordered by `idx`.
    panes: Vec<BoundPane>,
}

/// One captured pane that is bound to a conversation.
type BoundPane = (String, u32, AgentKind, String);

fn agent_bound_windows(conn: &Connection, snapshot_id: i64) -> Result<Vec<BoundWindow>> {
    let mut w_stmt = conn.prepare(
        "SELECT DISTINCT w.row_id
         FROM pane_rows p JOIN window_rows w ON w.row_id = p.window_row_id
         WHERE w.snapshot_id = ?1 AND p.agent_kind IS NOT NULL AND p.agent_session_id IS NOT NULL
         ORDER BY w.row_id",
    )?;
    let window_row_ids: Vec<i64> = w_stmt
        .query_map([snapshot_id], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    drop(w_stmt);

    let mut out = Vec::new();
    for window_row_id in window_row_ids {
        let window_name: String = conn.query_row(
            "SELECT name FROM window_rows WHERE row_id = ?1",
            [window_row_id],
            |r| r.get(0),
        )?;
        let mut s_stmt = conn.prepare(
            "SELECT s.name, l.idx
             FROM session_window_links l
             JOIN session_rows s ON s.row_id = l.session_row_id
             WHERE l.window_row_id = ?1
             ORDER BY l.row_id",
        )?;
        let sessions: Vec<(String, u32)> = s_stmt
            .query_map([window_row_id], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<_, _>>()?;
        drop(s_stmt);

        let mut p_stmt = conn.prepare(
            "SELECT tmux_pane_id, idx, agent_kind, agent_session_id
             FROM pane_rows
             WHERE window_row_id = ?1 AND agent_kind IS NOT NULL AND agent_session_id IS NOT NULL
             ORDER BY idx",
        )?;
        let panes: Vec<Option<BoundPane>> = p_stmt
            .query_map([window_row_id], |r| {
                let pane_id: String = r.get(0)?;
                let idx: u32 = r.get(1)?;
                let kind: String = r.get(2)?;
                let native_id: String = r.get(3)?;
                // Unreachable in practice: this column is only ever written
                // from `AgentKind::as_str`.
                Ok(AgentKind::parse(&kind).map(|k| (pane_id, idx, k, native_id)))
            })?
            .collect::<std::result::Result<_, _>>()?;
        drop(p_stmt);

        out.push(BoundWindow {
            sessions,
            window_name,
            panes: panes.into_iter().flatten().collect(),
        });
    }
    Ok(out)
}

/// How long [`resume_agents`] waits for one pane's conversation to start
/// before giving up on it. The same budget `osm resume` uses for a single
/// pane, so a conversation that comes back by hand also comes back on a
/// boot restore.
const AGENT_RESUME_TIMEOUT: std::time::Duration = resume::DEFAULT_TIMEOUT;

/// Resume every conversation this snapshot bound to a pane, into the panes
/// **this restore attempt verifiably put them in**.
///
/// # The invariant
///
/// > A resume may only be delivered to a pane that this attempt verifiably
/// > created or adopted, on the server incarnation verified for the whole
/// > run, identified by a mapping the restore itself recorded.
///
/// Every clause is load-bearing and each one closes a way a conversation
/// could otherwise be typed into somebody else's pane:
///
/// * **verifiably created or adopted** — the pane must belong to a session in
///   [`delivered_sessions`]. Navigating by the captured session *name* and
///   window *index* instead meant that an unrelated live `dev`, which made
///   this restore report a topology conflict and put nothing back, was still
///   a `dev` that `=dev:0` resolved against; the captured conversation went
///   into a pane of the user's live session. Conflicted, failed, skipped and
///   degraded sessions are not delivered, so nothing in them is ever sent
///   anything.
/// * **on the server incarnation verified for the whole run** — every
///   delivery is bound to `outcome.server`, and the check happens *inside the
///   same tmux operation as the send* (see [`resume::deliver`]). Protection
///   that ends when `restore_tree` returns is protection that ends before the
///   conversation is delivered: a server dying in between is replaced on the
///   same socket, reissues the same pane ids, and receives the input.
///   Publication notices afterwards, which is too late for a pane that has
///   already been typed into.
/// * **a mapping the restore itself recorded** — [`RestoreOutcome::pane_map`],
///   written where each pane was created or matched. Pane identity was
///   previously reconstructed from `%N` creation order, which is not identity
///   at all for an *adopted* window: adoption validates a session in
///   layout-cell order, and a window whose panes were created in one order
///   and rearranged into another has the two disagreeing — so the
///   conversation landed in the wrong pane of the right window.
///
/// # Placement
///
/// Called from [`run_restore`] after `run_restore_attempt` returns — i.e.
/// after every pane in this restore exists and every window's captured
/// layout has been applied — and before the source snapshot's fate
/// (`retire_or_return` / `publish_current_boot`) is decided. Resuming any
/// earlier would target panes that are still being split and laid out;
/// deciding the snapshot's fate before this runs would let a restore that
/// only put back bare shells retire the one record of what should have been
/// running in them.
///
/// # What is and is not attempted
///
/// Nothing is attempted at all when `agents.auto_resume` is off. A binding
/// whose kind has no adapter enabled in `agents.enabled` right now is
/// skipped — not reported, since nothing here was ever going to attempt it,
/// consistent with [`resume::Outcome::Unsupported`] not being a failure. A
/// binding whose conversation is not fresh enough (`last_active` older than
/// `agents.auto_resume_max_age_mins`) is likewise skipped rather than
/// attempted and failed: it is left as a shell on purpose, for a human to
/// resume later through Plan 4's menu, not because anything went wrong.
pub fn resume_agents(
    tmux: &Tmux,
    conn: &Connection,
    snapshot_id: i64,
    cfg: &Config,
    outcome: &RestoreOutcome,
) -> ResumePass {
    if !cfg.agents.auto_resume {
        return ResumePass::default();
    }

    let windows = match agent_bound_windows(conn, snapshot_id) {
        Ok(w) => w,
        Err(e) => {
            eprintln!(
                "osm: could not read the agent bindings for snapshot {snapshot_id} ({e:#}); \
                 no conversation will be resumed this run"
            );
            return ResumePass::default();
        }
    };
    if windows.iter().all(|w| w.panes.is_empty()) {
        return ResumePass::default();
    }

    // The one incarnation this restore's whole run was verified against. With
    // none there is no server any of this work can be said to be on, so there
    // is no pane a conversation may safely be put into.
    let Some(server) = outcome.server.as_deref() else {
        let mut out = Vec::new();
        for bw in &windows {
            for (_, idx, _, native_id) in &bw.panes {
                out.push((
                    label_of(bw, *idx),
                    resume::Outcome::Failed(format!(
                        "this restore cannot be attributed to one tmux server \
                         incarnation, so {native_id} was not delivered anywhere"
                    )),
                ));
            }
        }
        return ResumePass {
            outcomes: out,
            resumed: Vec::new(),
        };
    };

    let delivered: HashSet<&str> = delivered_sessions(outcome).into_iter().collect();
    let adapters = agent::adapters(&cfg.agents.enabled);
    let live_pane_ids: Vec<String> = tmux
        .list_panes()
        .map(|panes| panes.into_iter().map(|p| p.id).collect())
        .unwrap_or_default();
    let max_age_secs = (cfg.agents.auto_resume_max_age_mins * 60) as i64;
    let now = boot::now_epoch();

    let mut out = Vec::new();
    let mut resumed = Vec::new();
    let boot_id = boot::current_boot_id().ok();
    for bw in &windows {
        // Fail closed on the whole window. A window is one window however
        // many sessions hold it, so if any of them is a session this restore
        // did not deliver, this restore did not verifiably put this window
        // back and may not type into its panes.
        let undelivered: Vec<&str> = bw
            .sessions
            .iter()
            .map(|(name, _)| name.as_str())
            .filter(|name| !delivered.contains(name))
            .collect();
        for (captured_pane, idx, kind, native_id) in &bw.panes {
            let label = label_of(bw, *idx);
            if !undelivered.is_empty() {
                out.push((
                    label,
                    resume::Outcome::Failed(format!(
                        "this restore did not deliver session(s) {:?}, which hold the \
                         window {:?} that {native_id} was captured in, so nothing was \
                         sent to any pane",
                        undelivered, bw.window_name
                    )),
                ));
                continue;
            }
            // The only identity available. A pane this attempt did not create
            // or adopt has no entry, and there is no second way to look one up
            // — that is the point.
            let Some(live_pane_id) = outcome.pane_map.get(captured_pane) else {
                out.push((
                    label,
                    resume::Outcome::Failed(format!(
                        "this restore has no record of putting captured pane \
                         {captured_pane} back, so it does not know which live pane \
                         {native_id} belongs in"
                    )),
                ));
                continue;
            };
            let Some(adapter) = adapters.iter().find(|a| a.kind() == *kind) else {
                // Not attempted, not reported: nothing here was ever capable
                // of resuming this kind, same as `Outcome::Unsupported`.
                continue;
            };
            if let Some(why) = adapter.auto_unsupported_reason() {
                // Said out loud rather than silently skipped: the user has a
                // pane that should hold a conversation and will not, and the
                // restore must not report itself as having put everything
                // back. `Unsupported` is not counted as a failure — nothing
                // went wrong — but it is in the record.
                eprintln!("osm: not resuming {native_id} into {label}: {why}");
                out.push((label, resume::Outcome::Unsupported));
                continue;
            }

            // A discovery failure is not "the conversation is too old to be
            // worth resuming". Reading them as the same thing is how an
            // unreadable agent home became a restore that reported success
            // over a bare shell: the freshness lookup went through `.ok()`,
            // the binding was silently skipped, and nothing anywhere said the
            // conversation had not come back.
            let discovered = match adapter.discover() {
                Ok(sessions) => sessions,
                Err(e) => {
                    out.push((
                        label,
                        resume::Outcome::Failed(format!(
                            "could not read the {} conversations to find {native_id} \
                             ({e:#}); this pane was left as a shell",
                            adapter.kind().as_str()
                        )),
                    ));
                    continue;
                }
            };
            let last_active = discovered
                .into_iter()
                .find(|s| &s.native_id == native_id)
                .and_then(|s| s.last_active);
            let fresh = matches!(last_active, Some(t) if now.saturating_sub(t) <= max_age_secs);
            if !fresh {
                // Older than the auto-resume window, or no longer
                // discoverable at all: left as a shell on purpose, for a
                // human to pick up later — not an outcome to report here.
                continue;
            }

            // A pane this restore built moments ago still reports `tmux` as
            // its foreground command until the shell has finished exec'ing —
            // 60 times out of 60 when asked immediately — and `preflight`
            // reads that field to decide whether a pane is idle. Without this
            // wait a restore refuses its own fresh pane as busy, reports
            // itself `partial`, and leaves the conversation behind, more often
            // the busier the machine is. Waiting on the observable condition,
            // not on a duration: a pane that is genuinely running something is
            // still refused below.
            resume::wait_until_idle(tmux, live_pane_id, resume::SETTLE_TIMEOUT);

            // Through `resume_into`, which holds one lock on this
            // conversation from before the exclusivity check until after the
            // identity is confirmed. A boot restore and a person typing
            // `osm resume` are two processes that can reach for the same
            // conversation at the same moment.
            let outcome = resume::resume_into(
                tmux,
                live_pane_id,
                adapter.as_ref(),
                native_id,
                &live_pane_ids,
                AGENT_RESUME_TIMEOUT,
                server,
            );
            // A confirmed resume pays the debt for the pane it was confirmed
            // in, there and then. Waiting for a later capture to observe the
            // conversation leaves the promise standing over a pane that is
            // already back: a user who closes this conversation while the
            // restore is still working through the panes after it makes the
            // publication find a bare shell with the debt still pending, and
            // the conversation they just closed is carried forward and
            // resurrected on the next boot.
            if outcome == resume::Outcome::Resumed {
                resumed.push(ResumedConversation {
                    kind: *kind,
                    native_id: native_id.clone(),
                    live_pane_id: live_pane_id.clone(),
                    label: label.clone(),
                });
                if let Some(boot_id) = &boot_id {
                    for (session_name, window_idx) in &bw.sessions {
                        let place = (session_name.clone(), *window_idx, *idx);
                        if let Err(e) = crate::debt::discharge_at(
                            conn,
                            boot_id,
                            &place,
                            &(*kind, native_id.clone()),
                        ) {
                            eprintln!(
                                "osm: {native_id} is back in {label} but the debt for it \
                                 could not be discharged ({e:#}); a capture may carry it \
                                 forward as though it were still owed"
                            );
                        }
                    }
                }
            }
            out.push((label, outcome));
        }
    }
    ResumePass {
        outcomes: out,
        resumed,
    }
}

/// What one restore's resume pass did: what to report, and what it actually
/// put back.
///
/// The second half is not derivable from the first. An outcome is a label and
/// a verdict, for a human and for `osm restore --json`; the publication needs
/// the *conversations* it confirmed, to check that the topology it is about to
/// write down still holds them.
#[derive(Debug, Default)]
pub struct ResumePass {
    pub outcomes: Vec<(String, resume::Outcome)>,
    /// Every conversation confirmed back into its pane, in the order they were
    /// resumed.
    pub resumed: Vec<ResumedConversation>,
}

/// A conversation this restore put back, **and the live pane it was confirmed
/// in**.
///
/// The pane is not decoration. A resume is a statement about one pane, and the
/// publication's job is to check that the statement still holds before the
/// source — the only other record of it — is retired. Carrying only `(kind,
/// id)` made the check answer a weaker question: "is this conversation running
/// anywhere on the server". A conversation confirmed in pane P that then exits
/// P and is started by the user in pane Q satisfies that, so the source was
/// retired and the attempt succeeded although the binding this restore built
/// is gone and nothing anywhere records that P should have held it.
#[derive(Debug, Clone, PartialEq)]
pub struct ResumedConversation {
    pub kind: AgentKind,
    pub native_id: String,
    /// The live pane [`resume::deliver`] confirmed the conversation in — not
    /// merely the pane it was sent to.
    pub live_pane_id: String,
    /// The `session:window.pane` label the pane is reported under, so a
    /// failure can name the place a human would recognise.
    pub label: String,
}

/// The `session:window.pane_idx` label a pane is reported under.
///
/// The first session the window is linked into, which for the overwhelmingly
/// common unlinked window is its only one. Which name it is affects nothing
/// but the string an operator reads: whether the resume happens at all is
/// decided against *every* session in [`BoundWindow::sessions`].
fn label_of(bw: &BoundWindow, idx: u32) -> String {
    let session = bw
        .sessions
        .first()
        .map(|(name, _)| name.as_str())
        .unwrap_or("?");
    format!("{}:{}.{}", session, bw.window_name, idx)
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
    run_restore_with(
        conn,
        tmux,
        dry_run,
        &crate::hypr::Live::new(),
        &crate::desktop::RealSpawner::new(),
    )
}

/// [`run_restore`] against a given compositor and terminal spawner.
///
/// The seam exists so a test can drive the whole command — selection, tmux
/// rebuild, resume, placement, publication — without a compositor and without
/// executing a terminal. Production has exactly one caller, [`run_restore`],
/// and it passes the real pair.
pub fn run_restore_with(
    conn: &mut Connection,
    tmux: &Tmux,
    dry_run: bool,
    h: &dyn crate::hypr::HyprCtl,
    sp: &dyn crate::desktop::Spawner,
) -> Result<RestoreReport> {
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

    // Every pane this restore is going to build now exists and every
    // window's captured layout has been applied — `run_restore_attempt` has
    // returned, which is exactly what makes this the right moment. Resuming
    // any earlier would target panes still being split and laid out;
    // deciding the snapshot's fate (below) any earlier would let a restore
    // that only put back bare shells retire the one record of what should
    // have been running in them. Skipped when nothing was actually restored
    // (`state == "failed"`): by construction that means no pane exists to
    // resume anything into.
    let mut state = state;
    // The conversations the resume pass confirmed back into their panes, which
    // the publication below has to find still there before it may retire the
    // snapshot that is the only other record of them.
    let mut resumed: Vec<ResumedConversation> = Vec::new();
    // Read once, out here: the publication below needs `restore.place_windows`
    // as much as the resume and placement passes do, and a restore must not
    // decide the same question two different ways within one run.
    let cfg = match crate::paths::config_path().and_then(|p| crate::config::load(&p)) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("osm: {e:#}; using the default config for this restore");
            Config::default()
        }
    };
    if state != "failed" {
        // Written down *before* anything is delivered, and regardless of
        // whether `auto_resume` is on: from here until the conversations are
        // back in their panes (or a human gives up on them), every pane this
        // restore rebuilt is a bare shell, and a capture landing in that
        // window would otherwise record "no agent anywhere" over the only
        // record of which conversation belonged where. This is the recorded
        // cause that lets such a capture carry the bindings instead — see
        // `crate::debt`, and note that it is the *restore* that knows this,
        // which is why it is the restore that says so.
        // Only about the panes this attempt verifiably put back. A binding in
        // a session it conflicted on is not owed by anybody: nothing was
        // restored there, and recording it let a later capture carry that
        // conversation onto a pane belonging to whoever the live session
        // belongs to.
        let delivered: HashSet<&str> = delivered_sessions(&outcome).into_iter().collect();
        let restored_panes: HashSet<&str> = outcome.pane_map.keys().map(String::as_str).collect();
        if let Err(e) = crate::debt::record(
            conn,
            snapshot_id,
            &boot_id,
            boot::now_epoch(),
            &delivered,
            &restored_panes,
        ) {
            eprintln!(
                "osm: could not record which conversations this restore still owes \
                 ({e:#}); a capture taken before they come back may drop their bindings"
            );
        }
        // The verified outcome goes in, not just the connection: which panes
        // this attempt actually delivered — and on which server incarnation —
        // is the whole of what makes a resume safe to send. See
        // `resume_agents`' invariant.
        let pass = resume_agents(tmux, conn, snapshot_id, &cfg, &outcome);
        resumed = pass.resumed;
        let agent_outcomes = pass.outcomes;
        // Consistent with Plan 1's rule that a snapshot retires only after
        // verified success: a pane left as a shell when it should hold a
        // conversation is work this restore did not do, so a run that would
        // otherwise be `succeeded` is downgraded to `partial` and the
        // snapshot stays retryable. A run that is already `partial` (or, one
        // paragraph up, `failed`) needs no further downgrading.
        if state == "succeeded" && agent_outcomes_are_degraded(&agent_outcomes) {
            state = "partial".to_string();
        }
        outcome.agent_outcomes = agent_outcomes;

        // Last, and before the source's fate is decided. Last because a
        // terminal attaches to a session and resizes it, so every pane must
        // already exist and every captured layout must already be applied;
        // before the decision because a session whose window did not come
        // back is work this restore did not do, and the snapshot that records
        // where that window was is the only copy of it.
        let window_outcomes =
            place_windows(h, sp, tmux, conn, snapshot_id, &outcome, attempt_id, &cfg);
        // Not written to `restore_objects`: that table's `kind` already means
        // a *tmux* window and its `state` is a closed set of tmux-restore
        // outcomes. Placement reports through `RestoreReport` and the JSON
        // contract instead, rather than overloading a column whose CHECK
        // constraint would have to be widened to admit it.
        if state == "succeeded" && window_outcomes_are_degraded(&window_outcomes) {
            state = "partial".to_string();
        }
        outcome.window_outcomes = window_outcomes;
    }

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
    let mut unsecured = false;
    if state == "succeeded" {
        let published = publish_current_boot(
            conn,
            tmux,
            h,
            attempt_id,
            snapshot_id,
            outcome.server.as_deref(),
            &delivered_sessions(&outcome),
            &resumed,
            cfg.restore.place_windows,
            &placed_windows(&outcome),
        );
        match published {
            Ok(Published {
                unobserved,
                unplaced,
            }) if !unobserved.is_empty() || !unplaced.is_empty() => {
                // A conversation this restore confirmed is not in the topology
                // it just wrote down. Its debt is paid — the resume really did
                // happen — so nothing will carry it, and carrying it anyway
                // would be guessing about a pane whose conversation has since
                // gone. The honest report is a partial restore whose source
                // stays restorable, so the record of what belonged there
                // survives for a human to act on.
                eprintln!(
                    "osm: {}; snapshot {snapshot_id} stays restorable",
                    unobserved
                        .into_iter()
                        .chain(unplaced)
                        .collect::<Vec<_>>()
                        .join("; ")
                );
                state = "partial".to_string();
            }
            Ok(_) => {}
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
#[allow(clippy::too_many_arguments)]
fn publish_current_boot(
    conn: &mut Connection,
    tmux: &Tmux,
    h: &dyn crate::hypr::HyprCtl,
    attempt_id: i64,
    snapshot_id: i64,
    expected: Option<&str>,
    delivered: &[&str],
    resumed: &[ResumedConversation],
    place: bool,
    placed: &[PlacedClaim<'_>],
) -> std::result::Result<Published, PublishFailure> {
    // With the compositor, not without it. This snapshot replaces the source
    // that is about to be retired, so if it carried no placement the windows
    // this restore just put back would be recorded nowhere and the next
    // reboot would have nothing to place.
    //
    // Unless placement is switched off, in which case there is no compositor
    // to ask and nothing to record: the same single answer the rest of the
    // run used.
    let collected = if place {
        crate::capture::collect_with_desktop(tmux, h)
    } else {
        crate::capture::collect(tmux)
    };
    let topo = match collected {
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
    // Off disk before the transaction opens, for the reason
    // [`crate::capture::Detection`] gives: it walks the agent stores, and
    // holding the database's write lock across that starved every other osm
    // process out of it.
    let detection = crate::capture::Detection::of(&topo);
    (|| -> Result<Published> {
        let tx = conn.transaction()?;
        // Before `write_topology_in`, whose carry-forward asks the source what
        // it is still owed.
        snapshots::resolve_sessions(&tx, snapshot_id, delivered)?;
        snapshots::refresh_unresolved(&tx, snapshot_id)?;
        let published_id =
            crate::capture::write_topology_in(&tx, &topo, "post_restore", &detection)?;
        // In the same transaction as the topology it describes, and before
        // the source is retired. `write_topology_in` writes the tmux half
        // alone — the placement write lives in `write_topology`, which this
        // path does not go through — so every successful restore used to
        // publish a replacement snapshot with no `terminal_windows` at all,
        // retire the source that held them, and leave the next reboot with no
        // placement to restore. The feature undid itself once per boot.
        if let Some(ps) = topo.placements.as_deref() {
            crate::desktop::write_placements_in(&tx, published_id, ps)?;
        }
        // Every conversation the resume pass confirmed has had its debt paid,
        // so nothing will carry it forward. If the topology just written does
        // not hold it either, this restore's work is not all there: the user
        // closed it, or it exited on its own, between the confirmation and
        // here. Retiring the source on that would leave no record anywhere of
        // where it belonged.
        let unobserved = unobserved_conversations(&tx, published_id, resumed)?;
        // And the same question about windows. A session this restore reported
        // `Placed` whose terminal the published snapshot does not hold is a
        // window that is not there — the terminal exited, or its attach was
        // lost between the placement pass and here. Retiring the source on
        // that would leave no record anywhere of where that window belonged.
        let unplaced = unplaced_windows(&tx, published_id, placed)?;
        if unobserved.is_empty() && unplaced.is_empty() {
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
        }
        tx.commit()?;
        Ok(Published {
            unobserved,
            unplaced,
        })
    })()
    .map_err(PublishFailure::Other)
}

/// What a publication that went through has to say for itself.
#[derive(Debug, Default)]
struct Published {
    /// Conversations the resume pass confirmed back into a pane that the
    /// published topology binds nowhere. Empty is the ordinary case, and the
    /// only one in which the source may be retired: the attempt's terminal
    /// state and the source's fate are then written here, in the same
    /// transaction as the topology. Otherwise the caller downgrades the run
    /// and writes them on the ordinary non-success path, which keeps the
    /// source selectable.
    unobserved: Vec<String>,
    /// Windows this restore reported `Placed` that the published topology does
    /// not hold — gone, holding a different session, or somewhere else on the
    /// desktop. Same rule, same consequence: the run is downgraded and the
    /// source stays selectable, because the source is the only remaining
    /// record of where that window was.
    unplaced: Vec<String>,
}

/// One terminal window in the topology a publication wrote down.
struct HeldWindow {
    address: String,
    session: String,
    workspace_kind: String,
    workspace_ref: String,
    monitor_connector: String,
}

/// The windows in `placed` that snapshot `published_id` does not hold **as
/// this restore left them**, each with what it says instead.
///
/// `Placed` is a statement about a moment: a window osm started had mapped,
/// held a client attached to the session, and had been moved. The snapshot
/// published afterwards is the durable claim, and it is the one the source is
/// retired against. Anything that happened in between — the terminal exiting,
/// the client detaching, the window being sent elsewhere — has to show up here
/// rather than be assumed away.
///
/// # Why the address, and not the session name
///
/// A session can have more than one terminal attached to it. Suppose `dev`
/// already has the user's own terminal open on another workspace; osm spawns
/// and verifies its own, and that one exits before the publication. The
/// snapshot then records the user's window for `dev`, a check that asks only
/// "does `dev` have a window?" answers yes, and the source — the only record
/// of where the window osm failed to deliver belonged — is retired against a
/// window osm never placed, while the run reports success. The claim is about
/// one window, so the check is too: that address, holding that session, on the
/// workspace and monitor this restore sent it to.
fn unplaced_windows(
    tx: &rusqlite::Transaction,
    published_id: i64,
    placed: &[PlacedClaim<'_>],
) -> Result<Vec<String>> {
    if placed.is_empty() {
        return Ok(Vec::new());
    }
    let mut stmt = tx.prepare(
        "SELECT hypr_address, session_name, workspace_kind, workspace_ref, monitor_connector
         FROM terminal_windows WHERE snapshot_id = ?1",
    )?;
    let held: Vec<HeldWindow> = stmt
        .query_map([published_id], |r| {
            Ok(HeldWindow {
                address: r.get(0)?,
                session: r.get(1)?,
                workspace_kind: r.get(2)?,
                workspace_ref: r.get(3)?,
                monitor_connector: r.get(4)?,
            })
        })?
        .collect::<std::result::Result<_, _>>()?;

    let mut out = Vec::new();
    for c in placed {
        let want = c.window;
        let Some(row) = held.iter().find(|h| h.address == want.address) else {
            // What the snapshot holds for that session instead, when it holds
            // anything: an operator reading this needs to know whether the
            // session lost its terminal outright or is showing one osm never
            // placed.
            let instead: Vec<&str> = held
                .iter()
                .filter(|h| h.session == c.session)
                .map(|h| h.address.as_str())
                .collect();
            out.push(if instead.is_empty() {
                format!(
                    "{}'s window {} was placed, but the topology this restore \
                     published records no terminal window for it",
                    c.session, want.address
                )
            } else {
                format!(
                    "{}'s window {} was placed, but the topology this restore \
                     published does not hold it — only {}, which this restore \
                     did not place",
                    c.session,
                    want.address,
                    instead.join(", ")
                )
            });
            continue;
        };
        if row.session != c.session {
            out.push(format!(
                "{}'s window {} was placed, but the topology this restore published \
                 has it holding {} instead",
                c.session, want.address, row.session
            ));
            continue;
        }
        if row.workspace_kind != want.workspace_kind || row.workspace_ref != want.workspace_ref {
            out.push(format!(
                "{}'s window {} was placed on {} workspace {}, but the topology this \
                 restore published has it on {} workspace {}",
                c.session,
                want.address,
                want.workspace_kind,
                want.workspace_ref,
                row.workspace_kind,
                row.workspace_ref
            ));
            continue;
        }
        // The monitor, always. This used to be checked "only when a monitor
        // was actually dispatched" — skipped for a claim that named none, on
        // the reasoning that a compositor listing no monitor was never asked
        // to move the window and so owes nothing.
        //
        // That exemption is what let a wrong-monitor restore report success.
        // Readiness proves a monitor exists before anything is spawned; a
        // later `hyprctl -j monitors` coming back as a valid, empty array
        // left the placement with nothing to resolve, and the claim it
        // produced then arrived here with no connector — so this check waved
        // it through, the source snapshot that recorded the right panel was
        // retired, and the user's terminal was left on whichever panel it
        // happened to map on.
        //
        // `spawn_and_place` no longer makes such a claim. This is the second
        // lock on the same door: a claim nothing can verify is a shortfall,
        // whatever produced it.
        match want.monitor_connector.as_deref() {
            Some(sent_to) if row.monitor_connector == sent_to => {}
            Some(sent_to) => out.push(format!(
                "{}'s window {} was moved to {sent_to}, but the topology this \
                 restore published has it on {}",
                c.session, want.address, row.monitor_connector
            )),
            None => out.push(format!(
                "{}'s window {} was reported placed without naming the monitor it \
                 was sent to, so nothing can check it against the {} the topology \
                 this restore published has it on",
                c.session, want.address, row.monitor_connector
            )),
        }
    }
    Ok(out)
}

/// The conversations in `resumed` that snapshot `published_id` does not bind
/// **to the pane they were confirmed in**, each with what the snapshot says
/// instead, in the order they were resumed.
///
/// The pane is the whole check. A restore's claim is never "this conversation
/// is running somewhere" — it is "this conversation is back in *this* pane",
/// and the source snapshot is retired on the strength of it. Asking only
/// whether `(kind, id)` appears anywhere in the published topology let a
/// conversation that exited the pane this restore built and was started by
/// hand in another one stand in for the binding that is gone, so the source
/// was retired and the run reported success with the promise unkept.
fn unobserved_conversations(
    tx: &rusqlite::Transaction,
    published_id: i64,
    resumed: &[ResumedConversation],
) -> Result<Vec<String>> {
    if resumed.is_empty() {
        return Ok(Vec::new());
    }
    let mut stmt = tx.prepare(
        "SELECT p.agent_kind, p.agent_session_id, p.tmux_pane_id
         FROM pane_rows p JOIN window_rows w ON w.row_id = p.window_row_id
         WHERE w.snapshot_id = ?1
           AND p.agent_kind IS NOT NULL AND p.agent_session_id IS NOT NULL",
    )?;
    let bound: Vec<(String, String, String)> = stmt
        .query_map([published_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<std::result::Result<_, _>>()?;
    let mut out = Vec::new();
    for r in resumed {
        let kind = r.kind.as_str().to_string();
        if bound
            .iter()
            .any(|(k, id, pane)| *k == kind && *id == r.native_id && *pane == r.live_pane_id)
        {
            continue;
        }
        // Where it went instead, when it went anywhere: an operator reading
        // this needs to know whether the conversation is gone or merely
        // somewhere else.
        let elsewhere: Vec<&str> = bound
            .iter()
            .filter(|(k, id, _)| *k == kind && *id == r.native_id)
            .map(|(_, _, pane)| pane.as_str())
            .collect();
        out.push(if elsewhere.is_empty() {
            format!(
                "{} was resumed and confirmed in {} ({}) but is not running \
                 anywhere in the topology this restore published",
                r.native_id, r.live_pane_id, r.label
            )
        } else {
            format!(
                "{} was resumed and confirmed in {} ({}) but the topology this \
                 restore published has it in {}, so the binding this restore \
                 built is not there",
                r.native_id,
                r.live_pane_id,
                r.label,
                elsewhere.join(", ")
            )
        });
    }
    Ok(out)
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

/// Whether any window outcome means the restore did not finish its work.
///
/// Exactly one outcome is a finished restore without a window:
/// `PlacementDisabled`, which is the user having said so in
/// `restore.place_windows`. Everything else is work still owed.
///
/// `NoCompositor` used to be on the finished side, on the reasoning that a
/// headless machine is a legitimate outcome. It is — but it is
/// indistinguishable, at that point, from a restore that ran a few seconds
/// before Hyprland finished starting, or from a compositor that died. Both of
/// those retired the source snapshot, which is the only record of where the
/// user's terminals were, on the strength of work nobody did. The headless
/// case now has its own explicit switch, and this one waits for the
/// compositor first (see [`crate::hypr::wait_until_reachable`]).
pub fn window_outcomes_are_degraded(outcomes: &[(String, crate::desktop::PlaceOutcome)]) -> bool {
    use crate::desktop::PlaceOutcome::*;
    outcomes.iter().any(|(_, o)| {
        matches!(
            o,
            SpawnFailed(_)
                | NeverMapped
                | Misplaced(_)
                | NeverAttached
                | Skipped(_)
                | NoCompositor
                | LostCompositor(_)
        )
    })
}

/// Give each delivered session its terminal window back.
///
/// Runs after sessions exist and agents have been resumed, before the source
/// snapshot is retired.
///
/// Only sessions in [`delivered_sessions`] are touched — the same rule the
/// agent pass follows. A conflicted session is one the restore refused to
/// adopt because it holds someone else's topology; spawning a window onto it
/// would compound that, and a skipped or failed session has nothing to show.
#[allow(clippy::too_many_arguments)]
pub fn place_windows(
    h: &dyn crate::hypr::HyprCtl,
    sp: &dyn crate::desktop::Spawner,
    tmux: &Tmux,
    conn: &Connection,
    snapshot_id: i64,
    outcome: &RestoreOutcome,
    attempt_id: i64,
    cfg: &crate::config::Config,
) -> Vec<(String, crate::desktop::PlaceOutcome)> {
    use crate::desktop::PlaceOutcome;

    let placements = match crate::desktop::placements_of(conn, snapshot_id) {
        Ok(p) => p,
        Err(e) => {
            return vec![(
                "*".to_string(),
                PlaceOutcome::Skipped(format!("reading placement: {e:#}")),
            )]
        }
    };
    if placements.is_empty() {
        return Vec::new();
    }

    let timeout = std::time::Duration::from_secs(cfg.restore.readiness_timeout_secs);

    // The one way to a finished restore with no window: the user asked for
    // it. Checked before the compositor is even contacted.
    if !cfg.restore.place_windows {
        return placements
            .iter()
            .map(|p| (p.session.clone(), PlaceOutcome::PlacementDisabled))
            .collect();
    }

    // Ask once, before spawning anything, and *wait* — a restore at boot
    // routinely beats Hyprland to the finish line, and treating those few
    // seconds as "there is no compositor here" retired the snapshot that
    // held the layout. A compositor that is not there must also not leave a
    // trail of terminals with nowhere to put them, which is why this happens
    // before the first spawn rather than per session.
    if !crate::hypr::wait_until_reachable(h, timeout) {
        return placements
            .iter()
            .map(|p| (p.session.clone(), PlaceOutcome::NoCompositor))
            .collect();
    }

    let delivered: std::collections::HashSet<&str> =
        delivered_sessions(outcome).into_iter().collect();
    let marker = crate::terminal::marker_for(attempt_id);

    let mut out = Vec::new();
    for p in &placements {
        if !delivered.contains(p.session.as_str()) {
            out.push((
                p.session.clone(),
                PlaceOutcome::Skipped("this attempt did not deliver that session".into()),
            ));
            continue;
        }
        out.push((
            p.session.clone(),
            crate::desktop::spawn_and_place(
                h,
                sp,
                tmux,
                p,
                &marker,
                &cfg.restore.terminal,
                timeout,
            ),
        ));
    }
    out
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

    /// No compositor at all, so publication records tmux and nothing else.
    ///
    /// These tests are about which *server* a publication belongs to; asking
    /// a real Hyprland here would make them depend on the developer's
    /// desktop, and dispatching from one is forbidden outright.
    struct NoDesktop;
    impl crate::hypr::HyprCtl for NoDesktop {
        fn clients_json(&self, _budget: std::time::Duration) -> Result<String> {
            anyhow::bail!("no compositor in this test")
        }
        fn monitors_json(&self, _budget: std::time::Duration) -> Result<String> {
            anyhow::bail!("no compositor in this test")
        }
        fn dispatch(&self, _: &str, _budget: std::time::Duration) -> Result<String> {
            panic!("no test here may dispatch: it would move a real window")
        }
    }

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

    /// A snapshot holding one terminal window, so [`unplaced_windows`] has a
    /// published topology to check a claim against.
    fn snapshot_holding_window(
        conn: &Connection,
        session: &str,
        address: &str,
        workspace: &str,
        connector: &str,
    ) -> i64 {
        conn.execute(
            "INSERT INTO snapshots (taken_at, boot_id, reason, state)
             VALUES (?1, 'boot-a', 'test', 'complete')",
            rusqlite::params![boot::now_epoch()],
        )
        .unwrap();
        let snap = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO terminal_windows
               (snapshot_id, hypr_address, window_class, terminal_kind, session_name,
                workspace_kind, workspace_ref, monitor_connector)
             VALUES (?1, ?2, 'com.mitchellh.ghostty', 'ghostty', ?3, 'numbered', ?4, ?5)",
            rusqlite::params![snap, address, session, workspace, connector],
        )
        .unwrap();
        snap
    }

    /// A `Placed` claim that names no monitor cannot be checked, so it must be
    /// reported rather than waved through.
    ///
    /// This is the publication half of the empty-monitor-list hole. A
    /// compositor that answered readiness and then returned a valid, empty
    /// `hyprctl -j monitors` left `resolve_monitor` with nothing to resolve,
    /// so the claim carried no connector — and this check skipped the monitor
    /// comparison for exactly those claims, on the reasoning that no monitor
    /// dispatch had been made. The window was then accepted on its workspace
    /// alone, on whatever panel it happened to map on, and the source snapshot
    /// — the only record of the panel it belonged on — was retired against it
    /// while the run reported `succeeded`.
    ///
    /// `spawn_and_place` no longer produces such a claim. This is the second
    /// lock on the same door: a claim nobody can verify is a shortfall here,
    /// whatever produced it.
    #[test]
    fn a_placed_claim_that_names_no_monitor_is_reported_not_accepted() {
        let tmp = tempfile::tempdir().unwrap();
        let mut conn = db::open(&tmp.path().join("state.db")).unwrap();
        let snap = snapshot_holding_window(&conn, "alpha", "0x5eedf00d", "3", "HDMI-A-1");

        // Everything the check *can* compare agrees: same address, same
        // session, same workspace. Only the monitor is unverifiable.
        let window = crate::desktop::PlacedWindow {
            address: "0x5eedf00d".to_string(),
            workspace_kind: "numbered".to_string(),
            workspace_ref: "3".to_string(),
            monitor_connector: None,
        };
        let tx = conn.transaction().unwrap();
        let unplaced = unplaced_windows(
            &tx,
            snap,
            &[PlacedClaim {
                session: "alpha",
                window: &window,
            }],
        )
        .unwrap();

        assert_eq!(
            unplaced.len(),
            1,
            "a claim whose monitor nothing can check was accepted as a placement: \
             {unplaced:?}"
        );
        assert!(
            unplaced[0].contains("monitor"),
            "the report must say what could not be checked: {unplaced:?}"
        );
    }

    /// The control: a claim that names its monitor, and a published topology
    /// that agrees with it, is a placement.
    #[test]
    fn a_placed_claim_the_published_topology_agrees_with_is_accepted() {
        let tmp = tempfile::tempdir().unwrap();
        let mut conn = db::open(&tmp.path().join("state.db")).unwrap();
        let snap = snapshot_holding_window(&conn, "alpha", "0x5eedf00d", "3", "DP-1");

        let window = crate::desktop::PlacedWindow {
            address: "0x5eedf00d".to_string(),
            workspace_kind: "numbered".to_string(),
            workspace_ref: "3".to_string(),
            monitor_connector: Some("DP-1".to_string()),
        };
        let tx = conn.transaction().unwrap();
        assert!(unplaced_windows(
            &tx,
            snap,
            &[PlacedClaim {
                session: "alpha",
                window: &window,
            }],
        )
        .unwrap()
        .is_empty());
    }

    /// And a claim the topology contradicts on the monitor alone is reported,
    /// which is the check that was being skipped.
    #[test]
    fn a_placed_claim_on_a_monitor_the_topology_contradicts_is_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let mut conn = db::open(&tmp.path().join("state.db")).unwrap();
        let snap = snapshot_holding_window(&conn, "alpha", "0x5eedf00d", "3", "HDMI-A-1");

        let window = crate::desktop::PlacedWindow {
            address: "0x5eedf00d".to_string(),
            workspace_kind: "numbered".to_string(),
            workspace_ref: "3".to_string(),
            monitor_connector: Some("DP-1".to_string()),
        };
        let tx = conn.transaction().unwrap();
        let unplaced = unplaced_windows(
            &tx,
            snap,
            &[PlacedClaim {
                session: "alpha",
                window: &window,
            }],
        )
        .unwrap();
        assert_eq!(unplaced.len(), 1, "{unplaced:?}");
        assert!(unplaced[0].contains("DP-1") && unplaced[0].contains("HDMI-A-1"));
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
            &NoDesktop,
            attempt,
            snap,
            outcome.server.as_deref(),
            &delivered,
            &[],
            // No compositor in these tests, and nothing was placed: the
            // publication's window check is vacuous here, which is what lets
            // them stay about the server-identity window they exist for.
            false,
            &[],
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
            &NoDesktop,
            attempt,
            snap,
            outcome.server.as_deref(),
            &delivered,
            &[],
            false,
            &[],
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

    /// A window this restore reported `Placed` that the topology it then
    /// wrote down records no terminal for.
    ///
    /// `Placed` is a statement about a moment — a window osm started had
    /// mapped, held a client attached to the session, and had been moved. The
    /// snapshot published afterwards is the durable claim, and it is the one
    /// the source is retired against. Here the compositor stops answering
    /// between the two, so the publication records no window for `alpha`
    /// while the source that knows where `alpha`'s window belonged is about to
    /// be retired. The only honest answer is to leave it selectable.
    #[test]
    fn a_placed_window_the_published_snapshot_does_not_hold_keeps_the_source() {
        let src = Server::start("unplaced-src");
        src.session("alpha");

        let tmp = tempfile::tempdir().unwrap();
        let mut conn = db::open(&tmp.path().join("state.db")).unwrap();
        let snap = capture::snapshot(&mut conn, src.t(), "test").unwrap();
        conn.execute("UPDATE snapshots SET boot_id='boot-a' WHERE id=?1", [snap])
            .unwrap();
        src.kill();

        let dst = Server::start("unplaced-dst");
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
        // The window the placement pass would have reported: an address it
        // spawned, moved to workspace 3 of DP-1.
        let window = crate::desktop::PlacedWindow {
            address: "0x5eedf00d".to_string(),
            workspace_kind: "numbered".to_string(),
            workspace_ref: "3".to_string(),
            monitor_connector: Some("DP-1".to_string()),
        };
        let published = publish_current_boot(
            &mut conn,
            dst.t(),
            // Placement was on and a window was placed; by the time the
            // publication reads the desktop the compositor has stopped
            // answering, so the snapshot it writes holds no window at all.
            &NoDesktop,
            attempt,
            snap,
            outcome.server.as_deref(),
            &delivered,
            &[],
            true,
            &[PlacedClaim {
                session: "alpha",
                window: &window,
            }],
        )
        .unwrap_or_else(|e| match e {
            PublishFailure::ServerMoved(why) => panic!("the server never moved: {why}"),
            PublishFailure::Other(e) => panic!("publication failed: {e:#}"),
        });

        assert_eq!(
            published.unplaced.len(),
            1,
            "a placed window absent from the published snapshot went unreported: {published:?}"
        );
        let state: String = conn
            .query_row("SELECT state FROM snapshots WHERE id=?1", [snap], |r| {
                r.get(0)
            })
            .unwrap();
        assert_ne!(
            state, "restored",
            "the source is the only record of where alpha's window was; it must not be retired"
        );
        let attempt_state: String = conn
            .query_row(
                "SELECT state FROM restore_attempts WHERE id=?1",
                [attempt],
                |r| r.get(0),
            )
            .unwrap();
        assert_ne!(
            attempt_state, "succeeded",
            "a restore whose window is not in the snapshot it published is not a success"
        );
    }

    /// A conversation this restore confirmed back into a pane, and that the
    /// topology it then wrote down does not hold.
    ///
    /// The confirmation discharged its debt — the resume really did happen —
    /// so nothing will carry the binding forward, and carrying it anyway would
    /// be guessing about a pane whose conversation has since gone. What must
    /// not happen is the third thing: publishing a topology without it *and*
    /// retiring the snapshot that still knows where it belonged, leaving no
    /// record of it anywhere while the run reports success.
    #[test]
    fn a_resume_the_published_topology_does_not_hold_keeps_the_source() {
        let src = Server::start("unobs-src");
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

        let dst = Server::start("unobs-dst");
        let tree = model::load(&conn, snap).unwrap();
        let outcome = restore_tree(dst.t(), &tree).unwrap();
        assert_eq!(outcome.created, vec!["alpha".to_string()], "{outcome:?}");

        conn.execute(
            "INSERT INTO restore_attempts (snapshot_id, started_at, state) VALUES (?1, ?2, 'running')",
            rusqlite::params![snap, boot::now_epoch()],
        )
        .unwrap();
        let attempt = conn.last_insert_rowid();

        // Nothing is running in the destination's panes, so a conversation
        // reported as confirmed cannot be in the topology about to be written.
        let live_pane = dst.t().list_panes().unwrap()[0].id.clone();
        let resumed = vec![ResumedConversation {
            kind: AgentKind::Claude,
            native_id: "gone-since".to_string(),
            live_pane_id: live_pane.clone(),
            label: "alpha:0.0".to_string(),
        }];
        let delivered = delivered_sessions(&outcome);
        let published = publish_current_boot(
            &mut conn,
            dst.t(),
            &NoDesktop,
            attempt,
            snap,
            outcome.server.as_deref(),
            &delivered,
            &resumed,
            false,
            &[],
        )
        .unwrap_or_else(|e| match e {
            PublishFailure::ServerMoved(why) => panic!("the server never moved: {why}"),
            PublishFailure::Other(e) => panic!("publication failed: {e:#}"),
        });

        assert_eq!(published.unobserved.len(), 1, "{:?}", published.unobserved);
        let why = &published.unobserved[0];
        assert!(
            why.contains("gone-since") && why.contains(&live_pane),
            "the publication has to say which conversation it could not find, \
             and where it was supposed to be: {why}"
        );
        assert_eq!(
            reasons(&conn),
            vec!["test".to_string(), "post_restore".to_string()],
            "the topology on the machine is still written down: it is real"
        );
        let state: String = conn
            .query_row("SELECT state FROM snapshots WHERE id=?1", [snap], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            state, "complete",
            "the source is the only record of where that conversation belonged, \
             so it must stay restorable"
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
            "a run missing part of its work is not a success; the caller writes \
             the downgraded terminal state"
        );
    }

    /// The conversation is running, and in the wrong pane.
    ///
    /// A restore's claim is never "this conversation is somewhere on the
    /// server" — it is "this conversation is back in *this* pane", and the
    /// source snapshot, the only other record of that binding, is retired on
    /// the strength of it. Checking only `(kind, id)` let a conversation that
    /// left the pane this restore built and was started by hand in another one
    /// stand in for the binding that is gone: the source was retired, the
    /// attempt succeeded, and nothing anywhere recorded that the pane the
    /// restore built should have been holding it.
    ///
    /// The second half of this test is its own control: the same publication,
    /// with the pane the conversation is *actually* in, still goes through and
    /// still retires the source. A check that refused both would be no better
    /// than the one it replaced.
    #[test]
    fn a_resume_the_published_topology_holds_in_another_pane_keeps_the_source() {
        const ID: &str = "0cfebf91-81c0-43d5-af63-c9fe7e845a01";

        let src = Server::start("wrongpane-src");
        src.session("alpha");
        src.t()
            .run(&["split-window", "-t", "=alpha:", "-c", "/tmp"])
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

        let dst = Server::start("wrongpane-dst");
        let tree = model::load(&conn, snap).unwrap();
        let outcome = restore_tree(dst.t(), &tree).unwrap();
        assert_eq!(outcome.created, vec!["alpha".to_string()], "{outcome:?}");

        // A conversation on disk, and a real process holding it open in the
        // *second* pane. The stub is a copy of the shell binary named `claude`,
        // so `#{pane_current_command}` reads `claude` — see
        // `tests/agent_capture_replaced_transcript.rs` for why that has to be
        // the process's own name.
        let home = tmp.path().join("claude-home");
        let transcript = home.join("projects/-tmp").join(format!("{ID}.jsonl"));
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::write(&transcript, "{\"cwd\":\"/tmp\"}\n").unwrap();
        let bin = tmp.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let sh = ["/bin/sh", "/usr/bin/sh"]
            .into_iter()
            .find(|p| std::path::Path::new(p).exists())
            .expect("a sh binary");
        let claude_bin = bin.join("claude");
        std::fs::copy(sh, &claude_bin).unwrap();
        let runner = bin.join("run.sh");
        std::fs::write(
            &runner,
            format!(
                "#!/bin/sh\nexec {:?} -c 'exec 3<\"$1\"; read line' -- {:?}\n",
                claude_bin, transcript
            ),
        )
        .unwrap();
        for path in [&claude_bin, &runner] {
            let mut perm = std::fs::metadata(path).unwrap().permissions();
            std::os::unix::fs::PermissionsExt::set_mode(&mut perm, 0o755);
            std::fs::set_permissions(path, perm).unwrap();
        }

        // SAFETY: nothing else in this module reads OSM_CLAUDE_HOME, and a
        // capture running concurrently would only discover this one fixture
        // conversation, which no pane of its own holds.
        std::env::set_var("OSM_CLAUDE_HOME", &home);

        let panes = dst.t().list_panes().unwrap();
        assert_eq!(panes.len(), 2, "the fixture needs two panes: {panes:?}");
        let built = panes[0].id.clone();
        let elsewhere = panes[1].id.clone();
        dst.t()
            .run(&[
                "send-keys",
                "-t",
                &elsewhere,
                runner.to_str().unwrap(),
                "C-m",
            ])
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let now = dst.t().list_panes().unwrap();
            let cmd = now.iter().find(|p| p.id == elsewhere).unwrap().cmd.clone();
            if cmd == "claude" {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the conversation never started in {elsewhere} (it is running {cmd})"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        conn.execute(
            "INSERT INTO restore_attempts (snapshot_id, started_at, state) VALUES (?1, ?2, 'running')",
            rusqlite::params![snap, boot::now_epoch()],
        )
        .unwrap();
        let attempt = conn.last_insert_rowid();
        let delivered = delivered_sessions(&outcome);

        // ---- the finding: confirmed in one pane, running in another --------
        let published = publish_current_boot(
            &mut conn,
            dst.t(),
            &NoDesktop,
            attempt,
            snap,
            outcome.server.as_deref(),
            &delivered,
            &[ResumedConversation {
                kind: AgentKind::Claude,
                native_id: ID.to_string(),
                live_pane_id: built.clone(),
                label: "alpha:0.0".to_string(),
            }],
            false,
            &[],
        )
        .unwrap_or_else(|e| match e {
            PublishFailure::ServerMoved(why) => panic!("the server never moved: {why}"),
            PublishFailure::Other(e) => panic!("publication failed: {e:#}"),
        });

        // The published topology really does hold the conversation — in the
        // pane the restore did not build it in.
        let first_published: i64 = conn
            .query_row("SELECT MAX(id) FROM snapshots", [], |r| r.get(0))
            .unwrap();
        let bound: Vec<(String, String)> = conn
            .prepare(
                "SELECT p.tmux_pane_id, p.agent_session_id
                 FROM pane_rows p JOIN window_rows w ON w.row_id = p.window_row_id
                 WHERE w.snapshot_id = ?1 AND p.agent_session_id IS NOT NULL",
            )
            .unwrap()
            .query_map([first_published], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(
            bound,
            vec![(elsewhere.clone(), ID.to_string())],
            "the fixture only means anything if the conversation is bound to \
             the other pane"
        );

        assert_eq!(
            published.unobserved.len(),
            1,
            "the binding this restore built is gone; a conversation running in \
             some other pane must not stand in for it: {:?}",
            published.unobserved
        );
        let why = &published.unobserved[0];
        assert!(
            why.contains(ID) && why.contains(&built) && why.contains(&elsewhere),
            "the publication has to name the conversation, the pane it was \
             confirmed in and the pane it is in instead: {why}"
        );
        let state: String = conn
            .query_row("SELECT state FROM snapshots WHERE id=?1", [snap], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            state, "complete",
            "the source is the only record that {built} should hold this \
             conversation, so it must stay restorable"
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
            "the binding this restore built is gone, so the publication must \
             not mark the attempt succeeded"
        );
        assert!(
            is_retryable("partial"),
            "the state the caller downgrades to must leave the source selectable"
        );

        // ---- the control: the same check, with the pane it is really in ----
        let published = publish_current_boot(
            &mut conn,
            dst.t(),
            &NoDesktop,
            attempt,
            snap,
            outcome.server.as_deref(),
            &delivered,
            &[ResumedConversation {
                kind: AgentKind::Claude,
                native_id: ID.to_string(),
                live_pane_id: elsewhere.clone(),
                label: "alpha:0.1".to_string(),
            }],
            false,
            &[],
        )
        .unwrap_or_else(|e| match e {
            PublishFailure::ServerMoved(why) => panic!("the server never moved: {why}"),
            PublishFailure::Other(e) => panic!("publication failed: {e:#}"),
        });
        assert!(
            published.unobserved.is_empty(),
            "a conversation confirmed in the pane it is running in is observed: \
             {:?}",
            published.unobserved
        );
        let state: String = conn
            .query_row("SELECT state FROM snapshots WHERE id=?1", [snap], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(state, "restored", "a verified restore retires its source");
        let attempt_state: String = conn
            .query_row(
                "SELECT state FROM restore_attempts WHERE id=?1",
                [attempt],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(attempt_state, "succeeded");

        std::env::remove_var("OSM_CLAUDE_HOME");
    }
}
