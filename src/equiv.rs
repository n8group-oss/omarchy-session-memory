//! What "this live session **is** the captured one" means, in one place.
//!
//! Two callers need exactly this judgement and they used to make it
//! separately, or not at all:
//!
//! * a restore, deciding whether the session already holding a captured name
//!   may be *adopted* — and therefore whether the snapshot may be retired;
//! * a capture, deciding whether a session an earlier restore did not deliver
//!   has since been recovered — and therefore whether the snapshot that still
//!   holds it may stop being carried forward.
//!
//! The capture side used to answer "the name exists, so yes", which is how a
//! restore that failed after four of nine panes could leave a truncated
//! `beta` live, have the next capture bless it, and let retention delete the
//! nine-pane original. Both callers now build a [`SessionShape`] and ask
//! [`difference`], so neither can drift from the other and neither can settle
//! for a name.
//!
//! # Panes are compared in layout order, never as a multiset
//!
//! The comparison used to sort each side's working directories and compare the
//! two sorted lists. That is a *multiset* comparison: a two-pane window with
//! pane 1 in `a` and pane 2 in `b` matched a live session with those same two
//! directories **swapped**, so restore reported `succeeded`, adopted the wrong
//! session and retired the snapshot that still knew which pane was where.
//!
//! Pane indices are not a stable identity either — `select-layout` renumbers
//! them by geometry — which is why sorting looked like the safe answer. The
//! actual stable identity is the *cell*: [`crate::layout::parse_with_pane_ids`]
//! yields each leaf's pane id in cell order, so both sides can be put in the
//! order their own layout describes and then compared position by position.

use crate::layout::{self, LayoutNode};
use std::collections::HashMap;

/// One pane, reduced to what identity depends on.
#[derive(Debug, Clone)]
pub struct PaneShape {
    /// The owning server's (or snapshot's) pane id. Never compared: it is only
    /// here so panes can be put into layout order.
    pub id: String,
    pub cwd: String,
    pub active: bool,
}

/// One window, reduced to what identity depends on.
#[derive(Debug, Clone)]
pub struct WindowShape {
    /// The owning server's (or snapshot's) window id. Never compared; returned
    /// to the caller so an adoption can pair captured windows with live ones.
    pub id: String,
    pub idx: u32,
    pub name: String,
    /// The raw layout string, kept for error messages only.
    pub layout: String,
    /// The parsed geometry, or `None` when the string is not a layout at all.
    pub geometry: Option<LayoutNode>,
    pub zoomed: bool,
    /// The window's panes **in layout (cell) order**.
    pub panes: Vec<PaneShape>,
    /// Position of the active pane within [`Self::panes`], if one is marked.
    pub active_pane: Option<usize>,
}

/// One session, reduced to what identity depends on.
#[derive(Debug, Clone)]
pub struct SessionShape {
    pub name: String,
    /// Ordered by [`WindowShape::idx`].
    pub windows: Vec<WindowShape>,
    /// The index of the session's current window, if one is marked.
    pub active_window: Option<u32>,
}

/// A pane before it has been put in layout order.
#[derive(Debug, Clone)]
pub struct RawPane {
    pub id: String,
    /// The pane's tmux index, used only as the fallback ordering.
    pub idx: u32,
    pub cwd: String,
    pub active: bool,
}

/// The numeric part of a pane id.
///
/// tmux writes `%3`; a carried snapshot row writes `carried:7:%3` for the same
/// pane, because a carried session's ids are namespaced so they cannot collide
/// with a live server's. The layout string holds the bare `3` in both cases, so
/// the number is what the two are matched on.
pub fn pane_number(id: &str) -> Option<u32> {
    id.rsplit('%').next()?.parse().ok()
}

/// The positions of `panes` in the order `ids` (a layout's leaves) lists them,
/// or `None` if the two cannot be matched up one-to-one.
///
/// Returning `None` rather than a partial order is deliberate: a caller that
/// falls back to index order still gets a correct-but-weaker comparison,
/// whereas a half-applied order would silently compare unrelated panes.
///
/// The result is a **permutation** of `0..panes.len()` or nothing at all.
/// Anything weaker used to be a crash rather than a fallback: a snapshot whose
/// layout string named one pane id twice (corruption, or a hand edit) produced
/// an order such as `[0, 0]`, and the caller — which moves each pane out of its
/// slot exactly once — then unwrapped an already-emptied slot and panicked.
/// That panic landed in the two code paths that exist to protect the user's
/// state: every adoption, and every discharge of carry debt. So both halves are
/// checked here: every layout id must name a pane, and no two ids may name the
/// same one.
fn layout_order(panes: &[RawPane], ids: &[u32]) -> Option<Vec<usize>> {
    if ids.len() != panes.len() {
        return None;
    }
    let mut by_number: HashMap<u32, usize> = HashMap::new();
    for (position, pane) in panes.iter().enumerate() {
        if by_number.insert(pane_number(&pane.id)?, position).is_some() {
            // Two panes claiming one number: the ids are not identities here,
            // so nothing may be inferred from them.
            return None;
        }
    }
    let mut used = vec![false; panes.len()];
    let mut order = Vec::with_capacity(ids.len());
    for id in ids {
        let position = *by_number.get(id)?;
        if std::mem::replace(&mut used[position], true) {
            // One pane claimed by two cells. The layout does not describe this
            // window's panes, so it cannot order them either.
            return None;
        }
        order.push(position);
    }
    Some(order)
}

/// Build a window's shape, putting its panes into the order its own layout
/// describes.
///
/// Falls back to pane-index order when the layout does not parse or its leaves
/// cannot be matched to the panes given. That fallback is never a silent pass:
/// [`difference`] reports an unparsable layout as a mismatch in its own right.
pub fn window_shape(
    id: String,
    idx: u32,
    name: String,
    layout: String,
    zoomed: bool,
    mut panes: Vec<RawPane>,
) -> WindowShape {
    panes.sort_by_key(|p| p.idx);
    let parsed = layout::parse_with_pane_ids(&layout).ok();
    if let Some((_, ids)) = &parsed {
        if let Some(order) = layout_order(&panes, ids) {
            // `layout_order` returned a permutation of `panes`, so indexing is
            // total. Cloning rather than moving out of slots keeps it that way
            // without an `expect` that a corrupted snapshot could reach.
            panes = order.into_iter().map(|from| panes[from].clone()).collect();
        }
    }
    let active_pane = panes.iter().position(|p| p.active);
    WindowShape {
        id,
        idx,
        name,
        layout,
        geometry: parsed.map(|(node, _)| node),
        zoomed,
        panes: panes
            .into_iter()
            .map(|p| PaneShape {
                id: p.id,
                cwd: p.cwd,
                active: p.active,
            })
            .collect(),
        active_pane,
    }
}

/// Why `live` is **not** `want`, or `None` if it genuinely is.
///
/// `want` is the captured side and `live` the one on the server; the wording of
/// every message assumes that, because these strings are what an operator is
/// shown when a restore reports a conflict.
pub fn difference(want: &SessionShape, live: &SessionShape) -> Option<String> {
    if live.windows.len() != want.windows.len() {
        return Some(format!(
            "live session has {} window(s), the snapshot has {}",
            live.windows.len(),
            want.windows.len()
        ));
    }

    // Both sides are ordered by index, so position i is the same window in
    // both if the topologies agree at all.
    for (live_w, want_w) in live.windows.iter().zip(want.windows.iter()) {
        if live_w.idx != want_w.idx {
            return Some(format!(
                "live window at index {} where the snapshot has index {}",
                live_w.idx, want_w.idx
            ));
        }
        if live_w.name != want_w.name {
            return Some(format!(
                "window {} is named {:?} live but {:?} in the snapshot",
                live_w.idx, live_w.name, want_w.name
            ));
        }
        let Some(live_geom) = &live_w.geometry else {
            return Some(format!(
                "window {} has an unparsable live layout {:?}",
                live_w.idx, live_w.layout
            ));
        };
        if live_geom.panes() != want_w.panes.len() {
            return Some(format!(
                "window {} has {} live pane(s), the snapshot has {}",
                live_w.idx,
                live_geom.panes(),
                want_w.panes.len()
            ));
        }
        let Some(want_geom) = &want_w.geometry else {
            return Some(format!(
                "window {} has an unparsable captured layout {:?}",
                live_w.idx, want_w.layout
            ));
        };
        if live_geom != want_geom {
            return Some(format!(
                "window {} has a different pane layout than the snapshot",
                live_w.idx
            ));
        }
        if live_w.zoomed != want_w.zoomed {
            return Some(format!(
                "window {} is {} live but {} in the snapshot",
                live_w.idx,
                if live_w.zoomed {
                    "zoomed"
                } else {
                    "not zoomed"
                },
                if want_w.zoomed {
                    "zoomed"
                } else {
                    "not zoomed"
                }
            ));
        }
        if live_w.panes.len() != want_w.panes.len() {
            return Some(format!(
                "window {} has {} live pane(s), the snapshot has {}",
                live_w.idx,
                live_w.panes.len(),
                want_w.panes.len()
            ));
        }
        // Cell by cell, in the order each side's own layout lists them. The
        // geometry check above proved the two layouts describe the same cells,
        // so position i is the same cell in both.
        for (cell, (live_p, want_p)) in live_w.panes.iter().zip(want_w.panes.iter()).enumerate() {
            if live_p.cwd != want_p.cwd {
                return Some(format!(
                    "window {}: the pane in layout position {} has directory {:?} live \
                     but {:?} in the snapshot",
                    live_w.idx, cell, live_p.cwd, want_p.cwd
                ));
            }
        }
        // The focused pane is part of the captured state: a `dev` whose cursor
        // is in the shell instead of the editor is not the session that was
        // captured, and adopting it retires the snapshot that still knew where
        // the cursor was. Compared as a position in layout order, not as a
        // tmux index: `pane-base-index` is a per-server option, so raw indices
        // would make every cross-server comparison a spurious conflict.
        if want_w.active_pane.is_some() && live_w.active_pane != want_w.active_pane {
            return Some(format!(
                "window {} has the pane in layout position {:?} active live but {:?} \
                 in the snapshot",
                live_w.idx, live_w.active_pane, want_w.active_pane
            ));
        }
    }

    // Likewise the session's current window: `dev` sitting on `logs` when it
    // was captured on `code` is a different session to come back to.
    if want.active_window.is_some() && live.active_window != want.active_window {
        return Some(format!(
            "the live session's current window is {:?} but the snapshot's is {:?}",
            live.active_window, want.active_window
        ));
    }

    None
}

/// Every (captured window id, live window id) pair a matching comparison lines
/// up, which is what lets a linked window be re-linked rather than rebuilt.
///
/// Only meaningful after [`difference`] has returned `None`; the two sides are
/// paired by position, which is exactly what that check validated.
pub fn window_pairs(want: &SessionShape, live: &SessionShape) -> Vec<(String, String)> {
    want.windows
        .iter()
        .zip(live.windows.iter())
        .map(|(w, l)| (w.id.clone(), l.id.clone()))
        .collect()
}

/// A captured session, as a shape.
///
/// The snapshot side of both callers: an adoption compares this against the
/// destination server, and a capture compares it against the topology it just
/// read.
pub fn shape_of_plan(plan: &crate::model::SessionPlan) -> SessionShape {
    let mut windows: Vec<&crate::model::WindowPlan> = plan.windows.iter().collect();
    windows.sort_by_key(|w| w.idx);
    let active_window = plan.active_window_id.as_ref().and_then(|id| {
        plan.windows
            .iter()
            .find(|w| &w.tmux_window_id == id)
            .map(|w| w.idx)
    });
    SessionShape {
        name: plan.name.clone(),
        windows: windows
            .into_iter()
            .map(|w| {
                window_shape(
                    w.tmux_window_id.clone(),
                    w.idx,
                    w.name.clone(),
                    w.layout.clone(),
                    w.zoomed,
                    w.panes
                        .iter()
                        .map(|p| RawPane {
                            id: p.tmux_pane_id.clone(),
                            idx: p.idx,
                            cwd: p.cwd.clone(),
                            active: w.active_pane_id.as_deref() == Some(p.tmux_pane_id.as_str()),
                        })
                        .collect(),
                )
            })
            .collect(),
        active_window,
    }
}
