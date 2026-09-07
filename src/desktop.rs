//! Map a terminal window to the tmux session attached inside it.
//!
//! # Why the parent chain, not descendants
//!
//! A tmux client's pid is the terminal's *child*; walking up from it reaches
//! the terminal window's own pid. Walking down from the terminal — the
//! obvious-looking alternative — gave wrong answers during the manual
//! recovery this plan is built on, because a terminal can have several
//! children (the shell, the tmux client, whatever the shell is running) and
//! only one of them is the attached tmux client.
//!
//! # Titles lie
//!
//! A Ghostty window attached to session `proj-alpha-2` displays
//! `omarchy:proj-discovery` — the tmux *window* name, not the session.
//! Nothing in this module ever reads a window's title to decide which
//! session it holds.

use anyhow::Context;
use anyhow::Result;
use rusqlite::OptionalExtension;
use std::collections::HashSet;

use crate::hypr::Client;

/// One line of `tmux list-clients -F '#{client_pid} #{client_session}'`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientPid {
    pub pid: u32,
    pub session: String,
}

/// Parses `tmux list-clients -F '#{client_pid} #{client_session}'` output.
///
/// Splits each line on the *first* space only: a session name may contain
/// spaces, a pid never does.
///
/// A line that is not `<pid> <session>` is an **error**, not a line to skip.
/// Dropping it silently turned a half-written or truncated reply into a
/// shorter client list, and a shorter client list is indistinguishable from
/// "those terminals are not there" — which is written down as the layout.
/// The one thing skipped is a blank line, which tmux emits as the whole of
/// its output when no client is attached.
pub fn parse_clients_output(raw: &str) -> Result<Vec<ClientPid>> {
    let mut out = Vec::new();
    for line in raw.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Some((pid_str, session)) = line.split_once(' ') else {
            anyhow::bail!("tmux list-clients printed a line with no session: {line:?}");
        };
        let Ok(pid) = pid_str.parse::<u32>() else {
            anyhow::bail!("tmux list-clients printed {pid_str:?}, which is not a pid: {line:?}");
        };
        out.push(ClientPid {
            pid,
            session: session.to_string(),
        });
    }
    Ok(out)
}

/// The chain a pid stops at without ever revisiting one, and without
/// following it forever.
const MAX_ANCESTOR_DEPTH: usize = 64;

/// The pid and its parent chain, starting with the pid itself.
///
/// Bounded by a visited set (so a corrupted `/proc` cannot loop this forever)
/// and a hard cap of [`MAX_ANCESTOR_DEPTH`]. A process vanishing mid-walk —
/// the ordinary case, not an error — simply ends the chain where it is.
pub fn ancestors(pid: u32) -> Vec<u32> {
    let mut chain = vec![pid];
    let mut seen: HashSet<u32> = HashSet::new();
    seen.insert(pid);

    let mut current = pid;
    while chain.len() < MAX_ANCESTOR_DEPTH {
        let Some(parent) = parent_pid(current) else {
            break;
        };
        if parent == 0 || !seen.insert(parent) {
            break;
        }
        chain.push(parent);
        current = parent;
    }
    chain
}

/// Reads `PPid:` out of `/proc/<pid>/status`. `None` if the process has
/// already vanished or the field cannot be read — never an error, since a
/// process disappearing mid-walk is the normal case.
fn parent_pid(pid: u32) -> Option<u32> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("PPid:") {
            return rest.trim().parse::<u32>().ok();
        }
    }
    None
}

/// Maps each hyprland window to the tmux session attached inside it, by
/// walking every tmux client's pid up its parent chain until it meets a pid
/// that is one of `windows`. Never consults a window's title.
///
/// Returns `(hypr address, session name)` pairs. A window with no tmux
/// client inside it, or a tmux client whose ancestor chain never reaches a
/// known window, is simply absent from the result.
pub fn map_windows(windows: &[Client], tmux_clients: &[ClientPid]) -> Vec<(String, String)> {
    let by_pid: std::collections::HashMap<u32, &str> = windows
        .iter()
        .map(|w| (w.pid, w.address.as_str()))
        .collect();

    let mut out = Vec::new();
    for tc in tmux_clients {
        for a in ancestors(tc.pid) {
            if let Some(address) = by_pid.get(&a) {
                out.push((address.to_string(), tc.session.clone()));
                break;
            }
        }
    }
    out
}

use crate::hypr::Monitor;

/// Where a session's terminal window was.
///
/// Geometry is stored relative to its monitor rather than in pixels: absolute
/// coordinates do not survive a resolution change, a scale change, or the
/// panel being replaced by a different one.
#[derive(Debug, Clone, PartialEq)]
pub struct Placement {
    pub session: String,
    pub address: String,
    pub class: String,
    pub terminal_kind: String,
    pub workspace_kind: String,
    pub workspace_ref: String,
    pub monitor_connector: String,
    pub monitor_desc: Option<String>,
    pub monitor_scale: Option<f32>,
    pub monitor_transform: Option<i32>,
    pub floating: bool,
    pub rel: Option<(f32, f32, f32, f32)>,
}

/// How a workspace must be addressed.
///
/// A special workspace addressed as a number lands somewhere else entirely,
/// so the kind travels with the reference.
pub fn workspace_kind_of(id: i64, name: &str) -> (&'static str, String) {
    if id < 0 || name.starts_with("special:") {
        ("special", name.to_string())
    } else if name.parse::<i64>().is_ok() {
        ("numbered", name.to_string())
    } else {
        ("named", name.to_string())
    }
}

/// Window geometry as a fraction of its monitor.
///
/// Both sides of the division are logical: a client's `at`/`size` and a
/// monitor's `x`/`y` already are, and the monitor's pixel `width`/`height`
/// are converted by [`Monitor::logical_size`]. Mixing the two spaces is what
/// halved a window captured on a scale-2 panel and restored on a scale-1 one.
///
/// `None` when the monitor is unknown or degenerate — never zeros, which
/// would restore the window into the corner at no size.
pub fn relative_geometry(
    client: &crate::hypr::Client,
    monitors: &[Monitor],
) -> Option<(f32, f32, f32, f32)> {
    let m = crate::hypr::connector_of(client.monitor_index, monitors)?;
    let (w, h) = m.logical_size()?;
    Some((
        (client.at.0 - m.x) as f32 / w,
        (client.at.1 - m.y) as f32 / h,
        client.size.0 as f32 / w,
        client.size.1 as f32 / h,
    ))
}

/// The monitor to place a window on.
///
/// Description first: a connector is renamed when a cable moves, but the
/// panel is the same panel. Then the connector. Then the focused monitor —
/// a window sent to a monitor that no longer exists is invisible, which is
/// worse than a window on the wrong one.
pub fn resolve_monitor<'a>(p: &Placement, monitors: &'a [Monitor]) -> Option<&'a Monitor> {
    if let Some(desc) = p.monitor_desc.as_deref().filter(|d| !d.is_empty()) {
        if let Some(m) = monitors.iter().find(|m| m.description == desc) {
            return Some(m);
        }
    }
    if let Some(m) = monitors.iter().find(|m| m.name == p.monitor_connector) {
        return Some(m);
    }
    monitors
        .iter()
        .find(|m| m.focused)
        .or_else(|| monitors.first())
}

/// Fit a relative rectangle inside a monitor, in the **logical** coordinates
/// Hyprland places windows in.
///
/// `None` for a monitor with no usable logical area — see
/// [`Monitor::logical_size`]. A caller that cannot compute geometry must
/// leave the window where the compositor put it rather than resize it to a
/// number derived from a guess.
pub fn clamp_to_usable(rel: (f32, f32, f32, f32), m: &Monitor) -> Option<(i32, i32, i32, i32)> {
    let (mw, mh) = m.logical_size()?;
    let w = (rel.2 * mw).round().clamp(1.0, mw) as i32;
    let h = (rel.3 * mh).round().clamp(1.0, mh) as i32;
    let x = (rel.0 * mw).round().clamp(0.0, (mw - w as f32).max(0.0)) as i32;
    let y = (rel.1 * mh).round().clamp(0.0, (mh - h as f32).max(0.0)) as i32;
    Some((x + m.x, y + m.y, w, h))
}

/// What is still wrong about where `w` is, or `None` when it is exactly
/// where `p` says it belongs.
///
/// This is the question `hyprctl dispatch` does not answer. Its `ok` means
/// the compositor accepted the call; it says nothing about where the window
/// ended up, and on Hyprland 0.56.2 a window can be sent to a workspace,
/// acknowledged, and be somewhere else a moment later because the very next
/// dispatch moved it again. Only reading the desktop back settles it.
///
/// # A monitor that cannot be resolved is a shortfall, not an exemption
///
/// [`resolve_monitor`] falls back to the focused monitor and then to the
/// first, so it answers for any list with a monitor in it. It returns `None`
/// for exactly one input: an **empty** list. This used to be read as "no
/// monitor dispatch was made, so none is owed", and the window was then
/// confirmed on its workspace alone — which is how a terminal could come back
/// on the wrong panel with the run reporting `succeeded`. A desktop osm
/// cannot read is not a desktop osm has satisfied, so it is reported.
pub fn placement_gap(w: &Client, p: &Placement, monitors: &[Monitor]) -> Option<String> {
    let mut wrong = Vec::new();
    if w.workspace_name != p.workspace_ref {
        wrong.push(format!(
            "it is on workspace {:?}, not {:?}",
            w.workspace_name, p.workspace_ref
        ));
    }
    match resolve_monitor(p, monitors) {
        Some(m) => {
            let on = crate::hypr::connector_of(w.monitor_index, monitors);
            if on.map(|c| c.id) != Some(m.id) {
                wrong.push(format!(
                    "it is on monitor {}, not {}",
                    on.map(|c| c.name.as_str())
                        .unwrap_or("(none this compositor lists)"),
                    m.name
                ));
            }
        }
        None => wrong.push(format!(
            "the compositor lists no monitor at all, so nothing can say whether it \
             is on {}",
            p.monitor_connector
        )),
    }
    if wrong.is_empty() {
        None
    } else {
        Some(wrong.join(", and "))
    }
}

/// The Lua dispatches that still have to run for `w` to satisfy `p`, in the
/// order they must run in.
///
/// The shape is verified against a live compositor by `tests/desktop_live.rs`:
/// Hyprland 0.56 dropped the shell-style `[workspace N silent]` rule syntax,
/// which fails with `']' expected near '4'` while looking perfectly correct.
///
/// # Why this takes the window rather than just its address
///
/// Because two of these dispatches contradict each other, so which of them to
/// send depends on where the window already is.
///
/// `hl.dsp.window.move({window=…, monitor='M'})` does not mean "keep this
/// window where it is and change its output". It means "put this window on
/// M", and where on M is **M's active workspace** — measured against the live
/// compositor, with the window's address and pid unchanged across the call:
///
/// ```text
/// move({workspace='9'})   -> ok    +0.0s: ws 9   +0.3s: ws 9   +1s: ws 9
/// move({monitor='HDMI-A-2'}) -> ok +0.0s: ws 2   +0.3s: ws 2   +2s: ws 2
/// ```
///
/// Emitting both unconditionally, workspace first, therefore threw away the
/// workspace on every restore: on a single-monitor machine the second call
/// was pure loss, and both of the maintainer's restored terminals came back
/// on whatever workspace happened to be active, with `ok` from every
/// dispatch and `"outcome":"placed"` on every window.
///
/// So the monitor is corrected only as a step *towards* a workspace move that
/// is going to follow it and overrule it. When the window is already on the
/// workspace it belongs on and only the monitor is wrong — a workspace that
/// currently lives on another output — nothing is emitted at all: the only
/// dispatch available would take the window off its workspace to put it on
/// the right panel, which is a worse answer than the honest report
/// [`placement_gap`] produces. See
/// `tests/desktop_confirm.rs::a_workspace_that_lives_on_the_wrong_monitor_is_reported_not_papered_over`.
pub fn place_lua(w: &Client, p: &Placement, monitors: &[Monitor]) -> Vec<String> {
    let win = lua_str(&format!("address:{}", w.address));
    let mut out = Vec::new();

    let target = resolve_monitor(p, monitors);
    let on_monitor = match target {
        Some(m) => crate::hypr::connector_of(w.monitor_index, monitors).map(|c| c.id) == Some(m.id),
        // Nothing to move it to, and nothing owed.
        None => true,
    };

    if w.workspace_name != p.workspace_ref {
        // First, because the workspace move that follows is what decides
        // where the window actually ends up.
        if let (false, Some(m)) = (on_monitor, target) {
            out.push(format!(
                "hl.dsp.window.move({{window={win}, monitor={}}})",
                lua_str(&m.name)
            ));
        }
        out.push(format!(
            "hl.dsp.window.move({{window={win}, workspace={}, follow=false}})",
            lua_str(&p.workspace_ref)
        ));
    }

    if p.floating {
        out.push(format!(
            "hl.dsp.window.float({{window={win}, action='enable'}})"
        ));
        if let (Some(rel), Some(m)) = (p.rel, target) {
            if let Some((x, y, w, h)) = clamp_to_usable(rel, m) {
                out.push(format!(
                    "hl.dsp.window.move({{window={win}, x={x}, y={y}, relative=false}})"
                ));
                out.push(format!(
                    "hl.dsp.window.resize({{window={win}, x={w}, y={h}, relative=false}})"
                ));
            }
        }
    }
    out
}

/// A Lua single-quoted string literal holding exactly `s`.
///
/// Workspace names are the user's own text — `Bob's`, a Windows-style path,
/// anything a keybind can name — and they were being pasted between quotes
/// untouched. An apostrophe alone broke the dispatch on a perfectly ordinary
/// named workspace, and a crafted name could close the literal and append
/// arbitrary Lua to a call this process makes as the user.
///
/// Escapes the two characters that end or continue a literal, and the
/// control characters a Lua lexer refuses inside one, using the numeric
/// `\ddd` form so nothing depends on the escape table.
pub fn lua_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                // Three digits always: `\9` followed by a literal `5` would
                // otherwise lex as the single escape `\95`.
                out.push_str(&format!("\\{:03}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

/// Every terminal window that has a tmux session attached, and where it is.
///
/// Returns `Ok(None)` when **either** side of the mapping could not be read —
/// distinct from `Ok(Some(vec![]))`, which means the compositor answered, the
/// tmux server answered, and no terminal has a session in it. Capture depends
/// on that distinction: writing "no placement" over a good layout because one
/// of the two was briefly unavailable is exactly how the maintainer's
/// original mapping was lost, and there was no history to recover it from.
///
/// Both halves are held to it, which they were not: the compositor half
/// refused a malformed reply from the start while the tmux half turned a
/// failed `list-clients` into `""` and reported a confident empty layout.
pub fn placements(
    h: &dyn crate::hypr::HyprCtl,
    tmux: &crate::tmux::Tmux,
) -> Result<Option<Vec<Placement>>> {
    Ok(match placements_with_incarnation(h, tmux)? {
        PlacementRead::Mapped(_, ps) => Some(ps),
        PlacementRead::Unreadable(_) => None,
    })
}

/// The outcome of one attempt to read the placement.
///
/// `Unreadable` and `Mapped(_, vec![])` are the two answers that used to be
/// one `Ok(None)`/`Ok(Some(vec![]))` pair, and keeping them apart is the whole
/// subject of this module: "the compositor answered and no terminal has a
/// session in it" is a fact; "one of the two could not be read" is the absence
/// of one, and writing the second down as the first is what destroyed the
/// maintainer's original mapping.
///
/// `Unreadable` carries *why*, which the `Ok(None)` it replaced could not. Six
/// different failures produced that value — a tmux that would not identify
/// itself, a compositor that did not answer, a reply that did not parse, a
/// `list-clients` that errored, output that did not parse, an identity that
/// moved mid-read — and an operator looking at a machine where captures keep
/// coming back placement-blind has no way to act on "one of six things".
#[derive(Debug, Clone, PartialEq)]
pub enum PlacementRead {
    /// The mapping, and the tmux incarnation both halves were read from.
    Mapped(String, Vec<Placement>),
    /// It could not be read, and what went wrong.
    Unreadable(String),
}

/// How long the placement read may spend being retried before it is called
/// unreadable.
///
/// A bound on a *retry*, not a wait for slow work — the same distinction
/// [`MONITOR_READ_BUDGET`] draws, and the same three seconds. On the
/// maintainer's machine `hyprctl` answered in 5ms throughout while 11 captures
/// in 19 came back with no placement, and a standalone reproduction of the
/// read succeeded 6 times out of 6: what fails is one of the transient
/// branches below, hit in the instant it has no answer, not a compositor that
/// has gone away. Three seconds is long enough to cross that instant and short
/// enough that a machine whose compositor really has died does not hold up the
/// daemon's tick.
pub const PLACEMENT_READ_BUDGET: std::time::Duration = std::time::Duration::from_secs(3);

/// How often the placement read is re-attempted while
/// [`PLACEMENT_READ_BUDGET`] lasts.
const PLACEMENT_READ_POLL: std::time::Duration = std::time::Duration::from_millis(100);

/// [`placements_with_incarnation`], re-attempted until it succeeds or
/// `deadline` passes.
///
/// # Why the calls themselves are not shortened
///
/// [`monitors_within`] hands each attempt whatever is left of its budget,
/// because what it is waiting out is a compositor with nothing to report. This
/// is waiting out a *failure*, and the two want opposite things from a slow
/// call: cutting `hyprctl -j clients` down to the remaining budget would fail
/// a compositor that is merely busy — one such call really did take longer
/// than 15 seconds on the maintainer's machine under load — where today it
/// succeeds. So every attempt gets the full [`crate::hypr::CALL_TIMEOUT`], and
/// only the *number* of attempts is bounded. One consequence is deliberate: a
/// call that burns the whole timeout leaves no budget, so a wedged compositor
/// is asked exactly once rather than twice.
///
/// One attempt always happens, whatever `deadline` says. A budget is a bound
/// on waiting, never a reason to skip the question.
///
/// The incarnation equality inside each attempt is untouched, and so is the
/// one [`crate::capture::attach_placements`] makes against the topology: a
/// retried read is still a read of whatever server is there *now*, and if that
/// is not the server the topology came from, the two describe different
/// machines.
pub fn placements_within(
    h: &dyn crate::hypr::HyprCtl,
    tmux: &crate::tmux::Tmux,
    deadline: std::time::Instant,
) -> Result<PlacementRead> {
    loop {
        let why = match placements_with_incarnation(h, tmux)? {
            mapped @ PlacementRead::Mapped(_, _) => return Ok(mapped),
            PlacementRead::Unreadable(why) => why,
        };
        if expired(deadline) {
            return Ok(PlacementRead::Unreadable(why));
        }
        // Checked on both sides of the wait, for the reason `monitors_within`
        // gives: sleeping a flat poll interval and then asking again unchecked
        // turns the bound into the bound plus one whole attempt.
        std::thread::sleep(
            PLACEMENT_READ_POLL.min(deadline.saturating_duration_since(std::time::Instant::now())),
        );
        if expired(deadline) {
            return Ok(PlacementRead::Unreadable(why));
        }
    }
}

/// One attempt at [`placements`], with the tmux incarnation the mapping was
/// read from.
///
/// The identity has to leave this function. Checking it only *inside* the
/// collection proves the client list and the window list describe one server,
/// which is not the same claim as "the server this placement describes is the
/// server the topology beside it came from". A server that answered `collect`
/// and died before this call is replaced by one that satisfies both checks
/// here perfectly well, and the two halves are then stored together as if
/// they were one machine state. Plan 1 applies exactly this rule to its three
/// `list-*` reads; it simply had never been carried across the
/// tmux/compositor boundary. See [`crate::capture::attach_placements`].
///
/// Every failure below is transient as far as this function can tell, so none
/// of them is final: [`placements_within`] is what production calls, and it
/// asks again within a bounded budget before the answer is written down.
pub fn placements_with_incarnation(
    h: &dyn crate::hypr::HyprCtl,
    tmux: &crate::tmux::Tmux,
) -> Result<PlacementRead> {
    // The tmux server's identity is read *before* the compositor and again
    // after the client list, so the whole mapping is known to describe one
    // server. A server replaced in between hands out `$0`, `%0`, … from zero
    // again, and the sessions its clients name are not the sessions this
    // snapshot's topology holds.
    //
    // `Ok(None)` — no server at all — is unreadable here too, deliberately.
    // Terminals do not vanish when tmux dies; a window whose session cannot
    // be read is a window whose placement is unknown, and "unknown" must
    // never be written down as "there were none".
    let before = match tmux.running_server_incarnation() {
        Ok(Some(id)) => id,
        Ok(None) => {
            return Ok(PlacementRead::Unreadable(
                "no tmux server was running, so no window could be matched to a session".into(),
            ))
        }
        Err(e) => {
            return Ok(PlacementRead::Unreadable(format!(
                "the tmux server would not identify itself: {e:#}"
            )))
        }
    };

    let clients_json = match h.clients_json(crate::hypr::CALL_TIMEOUT) {
        Ok(j) => j,
        Err(e) => {
            return Ok(PlacementRead::Unreadable(format!(
                "the compositor would not list its windows: {e:#}"
            )))
        }
    };
    let monitors_json = match h.monitors_json(crate::hypr::CALL_TIMEOUT) {
        Ok(j) => j,
        Err(e) => {
            return Ok(PlacementRead::Unreadable(format!(
                "the compositor would not list its monitors: {e:#}"
            )))
        }
    };
    // A malformed reply is an unreachable compositor, not an empty desktop.
    let windows = match crate::hypr::parse_clients(&clients_json) {
        Ok(w) => w,
        Err(e) => {
            return Ok(PlacementRead::Unreadable(format!(
                "the compositor's window list could not be read: {e:#}"
            )))
        }
    };
    let monitors = match crate::hypr::parse_monitors(&monitors_json) {
        Ok(m) => m,
        Err(e) => {
            return Ok(PlacementRead::Unreadable(format!(
                "the compositor's monitor list could not be read: {e:#}"
            )))
        }
    };

    // The tmux half gets the same treatment as the compositor half, which is
    // the whole point of this module and was applied to only one of them: a
    // `list-clients` that errors, or a reply that does not parse, used to
    // become an empty client list, which becomes an empty *placement* list,
    // which is written down as "no session has a terminal window". The newest
    // snapshot then carries no placement while the last good one ages out of
    // retention.
    let raw = match tmux.run(&["list-clients", "-F", "#{client_pid} #{client_session}"]) {
        Ok(raw) => raw,
        Err(e) => {
            return Ok(PlacementRead::Unreadable(format!(
                "the tmux server would not list its clients: {e:#}"
            )))
        }
    };
    let tmux_clients = match parse_clients_output(&raw) {
        Ok(cs) => cs,
        Err(e) => {
            return Ok(PlacementRead::Unreadable(format!(
                "the tmux client list could not be read: {e:#}"
            )))
        }
    };
    match tmux.running_server_incarnation() {
        Ok(Some(after)) if after == before => {}
        Ok(Some(after)) => {
            return Ok(PlacementRead::Unreadable(format!(
                "the tmux server changed identity while the placement was being \
                 read: {before} became {after}"
            )))
        }
        Ok(None) => {
            return Ok(PlacementRead::Unreadable(
                "the tmux server went away while the placement was being read".into(),
            ))
        }
        Err(e) => {
            return Ok(PlacementRead::Unreadable(format!(
                "the tmux server would not confirm its identity after the placement \
                 was read: {e:#}"
            )))
        }
    }

    let mapped = map_windows(&windows, &tmux_clients);
    let by_address: std::collections::HashMap<&str, &Client> =
        windows.iter().map(|w| (w.address.as_str(), w)).collect();

    let mut out = Vec::new();
    for (address, session) in mapped {
        let Some(w) = by_address.get(address.as_str()) else {
            continue;
        };
        let (kind, reference) = workspace_kind_of(w.workspace_id, &w.workspace_name);
        let m = crate::hypr::connector_of(w.monitor_index, &monitors);
        out.push(Placement {
            session,
            address: w.address.clone(),
            class: w.class.clone(),
            terminal_kind: terminal_kind_of(&w.class),
            workspace_kind: kind.to_string(),
            workspace_ref: reference,
            monitor_connector: m.map(|m| m.name.clone()).unwrap_or_default(),
            monitor_desc: m.map(|m| m.description.clone()).filter(|d| !d.is_empty()),
            monitor_scale: m.map(|m| m.scale),
            monitor_transform: m.map(|m| m.transform),
            floating: w.floating,
            rel: relative_geometry(w, &monitors),
        });
    }
    Ok(PlacementRead::Mapped(before, out))
}

/// The terminal a window class belongs to, for spawning its like again.
pub fn terminal_kind_of(class: &str) -> String {
    let c = class.to_lowercase();
    for k in ["ghostty", "alacritty", "kitty", "foot"] {
        if c.contains(k) {
            return k.to_string();
        }
    }
    class.to_string()
}

/// What a capture found out about window placement.
///
/// Three answers, and the third is the whole reason this type exists.
/// `Option<Vec<Placement>>` could only say "here it is" or "here it is not",
/// so a compositor that hiccupped for one capture and a machine that has no
/// windows produced the same value — and the only safe thing to do with an
/// ambiguity like that was to refuse the capture outright, which cost the
/// user the tmux topology as well. Naming the third state instead lets the
/// snapshot be recorded and lets everything downstream — retention, restore,
/// `osm status` — treat *unknown* as the different fact it is.
///
/// The project's own rule, one level up: unknown placement is not absent
/// placement.
#[derive(Debug, Clone, PartialEq)]
pub enum Placements {
    /// The compositor answered, the tmux server answered, and both described
    /// the same server incarnation. This is every terminal window that had a
    /// session in it — and an **empty** vector is a fact, not an absence:
    /// there were none.
    Known(Vec<Placement>),
    /// One of them could not be read, or the two described different tmux
    /// servers. Carries the last thing that went wrong, for the operator who
    /// has to work out which.
    ///
    /// Emphatically not `Known(vec![])`. Writing this down as an empty layout
    /// is what destroyed the maintainer's original mapping.
    Unknown(String),
    /// The compositor was never asked: `restore.place_windows` is off, or
    /// this capture path has no compositor to ask (a tmux-only capture, a
    /// test). Not a failure, and nothing is owed.
    Off,
}

/// `snapshots.placement_state` for [`Placements::Known`].
pub const PLACEMENT_KNOWN: &str = "known";
/// `snapshots.placement_state` for [`Placements::Unknown`].
pub const PLACEMENT_UNKNOWN: &str = "unknown";
/// `snapshots.placement_state` for [`Placements::Off`].
pub const PLACEMENT_DISABLED: &str = "disabled";

impl Placements {
    /// The token this is stored as in `snapshots.placement_state`.
    pub fn state(&self) -> &'static str {
        match self {
            Placements::Known(_) => PLACEMENT_KNOWN,
            Placements::Unknown(_) => PLACEMENT_UNKNOWN,
            Placements::Off => PLACEMENT_DISABLED,
        }
    }

    /// The windows, when they are known. `None` is *not* "there were none" —
    /// callers that write rows must use this rather than an empty slice, so
    /// that "unknown" can never be flattened into "empty" by accident.
    pub fn known(&self) -> Option<&[Placement]> {
        match self {
            Placements::Known(ps) => Some(ps),
            _ => None,
        }
    }

    /// Why the placement could not be read, or `None` when it could be or was
    /// never asked for.
    pub fn why(&self) -> Option<&str> {
        match self {
            Placements::Unknown(why) => Some(why),
            _ => None,
        }
    }

    pub fn is_unknown(&self) -> bool {
        matches!(self, Placements::Unknown(_))
    }
}

/// What snapshot `snapshot_id` knows about its window placement.
///
/// The raw token, not a rendering of it, and an error rather than a guess for
/// a snapshot that is not there: every caller of this decides something about
/// retention or restore, and a default would decide it wrongly and silently.
pub fn placement_state_of(conn: &rusqlite::Connection, snapshot_id: i64) -> Result<String> {
    Ok(conn.query_row(
        "SELECT placement_state FROM snapshots WHERE id = ?1",
        [snapshot_id],
        |r| r.get(0),
    )?)
}

/// Write a snapshot's window placement.
///
/// Called inside the snapshot's own transaction so placement lands with the
/// topology it describes or not at all. Only ever reached for
/// [`Placements::Known`]: an unknown placement writes nothing, and the rows
/// already present belong to earlier snapshots and are left alone.
pub fn write_placements_in(
    tx: &rusqlite::Transaction,
    snapshot_id: i64,
    placements: &[Placement],
) -> Result<usize> {
    let mut written = 0;
    for p in placements {
        // Link to this snapshot's session row when the session is in it; a
        // window whose session vanished mid-capture is still worth recording
        // for its workspace, so the link is optional rather than required.
        let session_row_id: Option<i64> = tx
            .query_row(
                "SELECT row_id FROM session_rows WHERE snapshot_id = ?1 AND name = ?2",
                rusqlite::params![snapshot_id, p.session],
                |r| r.get(0),
            )
            .optional()?;

        tx.execute(
            "INSERT INTO terminal_windows
               (snapshot_id, hypr_address, window_class, terminal_kind, session_row_id,
                session_name, workspace_kind, workspace_ref, monitor_connector,
                monitor_desc, monitor_scale, monitor_transform, floating,
                rel_x, rel_y, rel_w, rel_h)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)
             ON CONFLICT(snapshot_id, hypr_address) DO NOTHING",
            rusqlite::params![
                snapshot_id,
                p.address,
                p.class,
                p.terminal_kind,
                session_row_id,
                p.session,
                p.workspace_kind,
                p.workspace_ref,
                p.monitor_connector,
                p.monitor_desc,
                p.monitor_scale,
                p.monitor_transform,
                p.floating as i64,
                p.rel.map(|r| r.0),
                p.rel.map(|r| r.1),
                p.rel.map(|r| r.2),
                p.rel.map(|r| r.3),
            ],
        )?;
        written += 1;
    }
    Ok(written)
}

/// Every placement recorded for a snapshot, newest schema shape.
pub fn placements_of(conn: &rusqlite::Connection, snapshot_id: i64) -> Result<Vec<Placement>> {
    let mut stmt = conn.prepare(
        "SELECT tw.hypr_address, tw.window_class, tw.terminal_kind,
                tw.session_name, tw.workspace_kind, tw.workspace_ref,
                tw.monitor_connector, tw.monitor_desc, tw.monitor_scale,
                tw.monitor_transform, tw.floating,
                tw.rel_x, tw.rel_y, tw.rel_w, tw.rel_h
         FROM terminal_windows tw
         WHERE tw.snapshot_id = ?1
         ORDER BY tw.row_id",
    )?;
    let rows = stmt.query_map([snapshot_id], |r| {
        let rel: Option<(f32, f32, f32, f32)> = match (
            r.get::<_, Option<f32>>(11)?,
            r.get::<_, Option<f32>>(12)?,
            r.get::<_, Option<f32>>(13)?,
            r.get::<_, Option<f32>>(14)?,
        ) {
            (Some(a), Some(b), Some(c), Some(d)) => Some((a, b, c, d)),
            _ => None,
        };
        Ok(Placement {
            address: r.get(0)?,
            class: r.get(1)?,
            terminal_kind: r.get(2)?,
            session: r.get(3)?,
            workspace_kind: r.get(4)?,
            workspace_ref: r.get(5)?,
            monitor_connector: r.get(6)?,
            monitor_desc: r.get(7)?,
            monitor_scale: r.get(8)?,
            monitor_transform: r.get(9)?,
            floating: r.get::<_, i64>(10)? == 1,
            rel,
        })
    })?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

/// The layout a restore of one snapshot should put back, and where it came
/// from.
#[derive(Debug, Clone, PartialEq)]
pub enum PlacementForRestore {
    /// The snapshot's own placement — it was `known` (possibly empty) or
    /// `disabled`, and either way it is the answer.
    Own(Vec<Placement>),
    /// The snapshot's placement was `unknown`, and this is the newest earlier
    /// snapshot of the **same boot** that knew, with its id and the time it
    /// was taken.
    ///
    /// `placements` is that snapshot's layout **trimmed to the sessions the
    /// snapshot being restored holds**. The two snapshots are minutes apart
    /// and the machine's sessions move in minutes, so an untrimmed carry
    /// hands the pass a window to owe for a session this restore was never
    /// going to deliver.
    Carried {
        from: i64,
        taken_at: i64,
        placements: Vec<Placement>,
    },
    /// The snapshot's placement was `unknown` and nothing earlier in the same
    /// boot knew either. There is no layout to put back and none may be
    /// invented.
    Unavailable,
}

/// What a restore of `snapshot_id` has to work with.
///
/// # Why an older layout beats no layout
///
/// The tmux topology and the window placement are facts of very different
/// speeds. What sessions and panes exist, and what is running in them, changes
/// by the minute — so the topology must come from the newest snapshot, always.
/// Which workspace and monitor a session's terminal lives on is something the
/// user decided once and rarely revisits; a copy of it from a few minutes
/// earlier in the same boot is very likely still true, and is certainly closer
/// to the truth than opening no terminal at all.
///
/// The alternative — report the shortfall and place nothing — hands the user
/// back their sessions with an empty screen, which is the outcome this whole
/// feature exists to prevent. So the layout is carried forward, and the carry
/// is *reported* ([`PlaceOutcome::PlacementCarried`]) rather than performed
/// silently: a restore that used a layout it did not itself record has to say
/// so.
///
/// # The two directions it will not look
///
/// **Across a boot.** A snapshot's placement describes where windows were
/// during that boot. Reaching further back would resurrect an arrangement the
/// user may have abandoned two reboots ago and present it as this one's.
///
/// **Forward.** The snapshot a restore publishes when it finishes carries
/// placement of its own and is newer than the source; reading forward would
/// let a restore place windows from the layout its own earlier attempt
/// produced — a claim about the machine after the restore rather than before
/// the reboot.
///
/// A snapshot that is `known` is never backfilled, empty or not:
/// `Known(vec![])` is the compositor answering that there are no terminal
/// windows, and putting terminals onto a desktop the user deliberately
/// cleared is not a restore.
pub fn placement_for_restore(
    conn: &rusqlite::Connection,
    snapshot_id: i64,
) -> Result<PlacementForRestore> {
    let (state, boot_id, taken_at) = conn.query_row(
        "SELECT placement_state, boot_id, taken_at FROM snapshots WHERE id = ?1",
        [snapshot_id],
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
            ))
        },
    )?;
    if state != PLACEMENT_UNKNOWN {
        return Ok(PlacementForRestore::Own(placements_of(conn, snapshot_id)?));
    }

    // Same boot, and not later than the source. The *newest* earlier `known`
    // snapshot wins, and it wins whether or not it holds any windows.
    //
    // Requiring rows here — `AND EXISTS (SELECT 1 FROM terminal_windows …)`,
    // which is what this used to say — let the search walk straight through
    // an answered empty desktop. The history that breaks is ordinary: a
    // `known` layout, the user closes every terminal, a `known` capture with
    // no rows, then a capture whose placement could not be read. Skipping the
    // empty answer carried the layout from *before* the user cleared their
    // desktop and put those terminals back.
    //
    // A `known` snapshot with no rows is not a gap in the record. It is the
    // compositor having been asked and having said there were none, and that
    // is the most recent thing anybody knows about where the windows were. It
    // therefore stops the search exactly as a populated one does, and what it
    // carries forward — nothing — is the answer.
    let fallback: Option<(i64, i64)> = conn
        .query_row(
            "SELECT s.id, s.taken_at FROM snapshots s
             WHERE s.boot_id = ?1
               AND s.state <> 'building'
               AND s.placement_state = 'known'
               AND (s.taken_at < ?2 OR (s.taken_at = ?2 AND s.id < ?3))
             ORDER BY s.taken_at DESC, s.id DESC
             LIMIT 1",
            rusqlite::params![boot_id, taken_at, snapshot_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;

    Ok(match fallback {
        Some((from, at)) => {
            // Only the sessions the snapshot being restored actually holds.
            //
            // The borrowed layout is older, and "older" is exactly when the
            // machine held different sessions. Carrying it whole put a window
            // on the list for a session this restore was never going to
            // deliver, and the pass then reported that session `skipped` — a
            // shortfall, which keeps the run partial and leaves the source
            // snapshot restorable for ever for work it never contained.
            //
            // The topology is the newest snapshot's, always; only the
            // placement is borrowed. Intersecting the two is what keeps the
            // borrowed half from making claims the topology does not support.
            let held = sessions_of(conn, snapshot_id)?;
            PlacementForRestore::Carried {
                from,
                taken_at: at,
                placements: placements_of(conn, from)?
                    .into_iter()
                    .filter(|p| held.contains(&p.session))
                    .collect(),
            }
        }
        None => PlacementForRestore::Unavailable,
    })
}

/// The session names a snapshot's topology holds.
fn sessions_of(
    conn: &rusqlite::Connection,
    snapshot_id: i64,
) -> Result<std::collections::HashSet<String>> {
    let mut stmt = conn.prepare("SELECT name FROM session_rows WHERE snapshot_id = ?1")?;
    let names = stmt
        .query_map([snapshot_id], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<std::collections::HashSet<String>>>()?;
    Ok(names)
}

/// The window a placement pass delivered, and where it put it.
///
/// Carried by [`PlaceOutcome::Placed`] because the claim is about *this*
/// window and no other. A session can have more than one terminal attached to
/// it — the user's own, opened by hand, on a workspace of their choosing — and
/// a publication that asks only whether the session has a window in it takes
/// the user's as proof of osm's. It is not: the terminal osm started can exit
/// between the placement pass and the publication, and retiring the source
/// against somebody else's window loses the only record of where osm's window
/// belonged, while the run reports success.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacedWindow {
    /// The Hyprland address of the window this attempt spawned and moved.
    pub address: String,
    /// The workspace it was moved to, in the kind/reference pair it was
    /// addressed by — a special workspace addressed as a number lands
    /// somewhere else entirely.
    pub workspace_kind: String,
    pub workspace_ref: String,
    /// The connector it was moved to, which is the monitor
    /// [`resolve_monitor`] settled on rather than the one the capture named:
    /// a cable that moved makes those two different, and the claim is about
    /// where the window was actually sent.
    ///
    /// [`spawn_and_place`] never leaves this `None`: a compositor that lists
    /// no monitor to resolve against ends the placement as a lost compositor
    /// rather than producing a claim whose monitor nothing can check. The
    /// `Option` survives so that a claim reaching the publication without one
    /// is *reported* rather than silently exempted from the monitor
    /// comparison — see [`crate::restore`]'s `unplaced_windows`, which used to
    /// skip exactly those and retire the user's snapshot against a window it
    /// had never confirmed the panel of.
    pub monitor_connector: Option<String>,
}

/// What happened to one session's window.
#[derive(Debug, Clone, PartialEq)]
pub enum PlaceOutcome {
    /// Spawned and moved to its recorded workspace, with the compositor
    /// acknowledging every dispatch — and which window that was, so the
    /// publication can be held to that window rather than to any window the
    /// session happens to have.
    Placed(PlacedWindow),
    /// The terminal could not be started.
    SpawnFailed(String),
    /// It started, but no window this attempt owns appeared before the
    /// deadline.
    NeverMapped,
    /// Its window appeared, every dispatch was acknowledged, and reading the
    /// desktop back says it is not on the workspace or the monitor it was
    /// captured on. Carries what is wrong, in the compositor's own terms.
    ///
    /// This is what `placed` used to say. On the maintainer's desktop two
    /// terminals captured on workspaces 9 and 10 came back on workspace 2
    /// with `ok` from every dispatch, and the restore reported both as
    /// `placed` — a claim about what osm asked for rather than about what is
    /// on the screen, which is the exact shape of failure this project exists
    /// to refuse. A dispatch is a request; only a read is evidence.
    ///
    /// Unlike [`Self::NeverAttached`], the terminal is **not** taken back. It
    /// is the user's restored terminal, holding their session, on the wrong
    /// workspace; ending it would take away the thing this pass exists to
    /// give back, to punish a fault of osm's. Degraded instead, so the source
    /// snapshot stays restorable and the shortfall is visible.
    Misplaced(String),
    /// Its window appeared and was moved, but no tmux client inside it ever
    /// attached to the session within the interval the attach was given.
    ///
    /// A window maps and accepts dispatches well before the shell inside it
    /// has run `tmux attach-session`, and a terminal whose attach fails maps
    /// all the same. Reporting that as [`Self::Placed`] published a snapshot
    /// in which the session had no terminal window at all — and retired the
    /// snapshot that said where its window belonged.
    ///
    /// Unlike [`Self::NeverMapped`], the terminal **is** taken back — when
    /// the tmux server was readable and said it holds no client of ours.
    /// What is left otherwise is a terminal on the user's desktop showing
    /// nothing, that osm has stopped tracking and cannot adopt on a later
    /// attempt (its ownership proof is a process ancestry only the attempt
    /// that spawned it can satisfy), so every retry adds another one beside
    /// it. A server that never gave a readable answer is a different matter:
    /// "not attached" is then not something this pass knows, the terminal may
    /// be showing the user their session right now, and it is left alone.
    NeverAttached,
    /// Hyprland did not answer within the readiness budget, checked **before**
    /// anything was spawned. Retryable work this restore did not do — not a
    /// completed outcome. Only [`Self::PlacementDisabled`] is that.
    NoCompositor,
    /// The compositor answered, a terminal was started, and then the
    /// compositor stopped answering or refused a dispatch. The terminal this
    /// attempt started has been terminated, so nothing is left on screen that
    /// osm no longer tracks.
    LostCompositor(String),
    /// Deliberately not attempted: no placement recorded, or the session was
    /// not one this attempt delivered.
    Skipped(String),
    /// Placement is switched off (`restore.place_windows = false`). The one
    /// outcome that says "no window, and that is finished work": a headless
    /// machine, or a user who wants their tmux back without terminals.
    PlacementDisabled,
    /// The source snapshot's placement is [`Placements::Unknown`], and the
    /// layout being applied came from an earlier snapshot of the same boot
    /// that knew. Carries which one, and when it was taken.
    ///
    /// Reported against the pseudo-session `*` beside the real per-session
    /// outcomes, because it is a statement about the pass rather than about
    /// one window. **Not** a shortfall: the pass did its job, with the best
    /// record of the desktop that exists. See [`placement_for_restore`] for
    /// why an older layout is preferred to no layout.
    PlacementCarried(String),
    /// The source snapshot's placement is [`Placements::Unknown`] and no
    /// earlier snapshot of the same boot knew either, so this restore has no
    /// layout to put back and did not invent one.
    ///
    /// A shortfall, and deliberately so: the run stays `partial` and the
    /// source snapshot stays selectable. Reporting nothing would have read as
    /// "this snapshot had no terminal windows", retired the source, and left
    /// no record anywhere of where the user's windows belonged.
    PlacementUnknown(String),
}

impl PlaceOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            PlaceOutcome::Placed(_) => "placed",
            PlaceOutcome::SpawnFailed(_) => "spawn_failed",
            PlaceOutcome::NeverMapped => "never_mapped",
            PlaceOutcome::Misplaced(_) => "misplaced",
            PlaceOutcome::NeverAttached => "never_attached",
            PlaceOutcome::NoCompositor => "no_compositor",
            PlaceOutcome::LostCompositor(_) => "lost_compositor",
            PlaceOutcome::Skipped(_) => "skipped",
            PlaceOutcome::PlacementDisabled => "placement_disabled",
            PlaceOutcome::PlacementCarried(_) => "placement_carried",
            PlaceOutcome::PlacementUnknown(_) => "placement_unknown",
        }
    }

    /// The detail a report can show beside [`Self::as_str`], if any.
    pub fn detail(&self) -> Option<&str> {
        match self {
            PlaceOutcome::SpawnFailed(d)
            | PlaceOutcome::LostCompositor(d)
            | PlaceOutcome::Misplaced(d)
            | PlaceOutcome::Skipped(d)
            | PlaceOutcome::PlacementCarried(d)
            | PlaceOutcome::PlacementUnknown(d) => Some(d),
            _ => None,
        }
    }
}

/// A process this restore started, identified so that a *different* process
/// which later reuses the same pid number cannot pass for it.
///
/// A bare number is not an identity. Linux recycles pids, and a restore that
/// waits up to the readiness budget for a window is exactly long enough for
/// the terminal to die and the number to come back as something unrelated —
/// whose window osm would then move, on the user's own desktop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spawned {
    pub pid: u32,
    /// Field 22 of `/proc/<pid>/stat`: the process's start time in clock
    /// ticks since boot. Together with the pid it names one process for as
    /// long as that process exists, and needs no dependency to read.
    pub start_ticks: u64,
}

impl Spawned {
    /// The process running under `pid` right now, or `None` if there is none.
    pub fn of(pid: u32) -> Option<Spawned> {
        Some(Spawned {
            pid,
            start_ticks: start_ticks(pid)?,
        })
    }

    /// Whether the process under this pid is still the one that was started.
    pub fn is_alive(&self) -> bool {
        start_ticks(self.pid) == Some(self.start_ticks)
    }
}

/// Field 22 of `/proc/<pid>/stat`, the process start time in clock ticks.
///
/// Parsed from the last `)` rather than by splitting the whole line: field 2
/// is the executable name in parentheses and may itself contain spaces and
/// parentheses, so a naive split lands on the wrong field for a process
/// called `(my prog)`.
pub fn start_ticks(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = &stat[stat.rfind(')')? + 1..];
    // Field 3 (`state`) is index 0 here, so field 22 is index 19.
    after_comm.split_whitespace().nth(19)?.parse().ok()
}

/// Starting a terminal, and being able to take it back.
///
/// A seam, and not a convenience one. Without it a test that exercises the
/// placement pass execs a real terminal onto the developer's desktop: that
/// happened, spawning a window that tried to attach to a session named
/// `mine` on their *default* tmux server. A test must be unable to do that,
/// not merely discouraged from it.
pub trait Spawner {
    /// Start the process and return its identity.
    ///
    /// That identity is the ownership proof. A window is ours because we
    /// started the process that owns it, not because its class matches a
    /// string we asked for — Ghostty accepts `--class=osm-restore-7` and
    /// reports `com.mitchellh.ghostty` anyway, so a marker in the class is
    /// never seen and every placement would report `NeverMapped`.
    fn spawn(&self, argv: &[String]) -> Result<Spawned>;

    /// Which terminal to open for a window captured as `captured_kind`.
    ///
    /// On this seam, and not called as a free function, because the answer
    /// reads the machine: `auto` searches the `PATH` this process happens to
    /// have. A test that cannot control that can only assert what its own
    /// machine would have answered anyway — which is how the test meant to
    /// prove the *captured* class reaches this decision came to pass with
    /// the captured class replaced by a literal, on a machine with terminals
    /// installed and on one without.
    fn choose_terminal(
        &self,
        configured: &str,
        captured_kind: &str,
    ) -> Option<crate::terminal::Kind> {
        crate::terminal::choose(configured, captured_kind)
    }

    /// Terminate a process this spawner started, because the placement it was
    /// started for cannot be completed.
    ///
    /// Leaving it running is not the safe default: it is a terminal on the
    /// user's desktop, attached to their session, that osm has stopped
    /// tracking and will spawn a second copy of on the next attempt.
    /// Implementations must act only on a process they themselves started.
    fn kill(&self, spawned: &Spawned);
}

/// The real one.
///
/// Keeps each [`std::process::Child`] rather than dropping it, so a terminal
/// that has to be taken back is killed through the handle that cannot name
/// anything else — no signal is ever sent to a pid number this process did
/// not start.
#[derive(Default)]
pub struct RealSpawner {
    children: std::cell::RefCell<std::collections::HashMap<u32, std::process::Child>>,
}

impl RealSpawner {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Spawner for RealSpawner {
    fn spawn(&self, argv: &[String]) -> Result<Spawned> {
        let (bin, args) = argv.split_first().context("empty spawn argv")?;
        let child = std::process::Command::new(bin)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .with_context(|| format!("spawning {bin}"))?;
        let pid = child.id();
        let spawned = Spawned::of(pid)
            .with_context(|| format!("{bin} (pid {pid}) was gone before it could be identified"))?;
        self.children.borrow_mut().insert(pid, child);
        Ok(spawned)
    }

    fn kill(&self, spawned: &Spawned) {
        if let Some(mut child) = self.children.borrow_mut().remove(&spawned.pid) {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Whether `window_pid` belongs to the process tree rooted at `spawned`.
///
/// A terminal may re-exec or fork before opening its window, so the window's
/// pid is often a descendant rather than the pid we started. Walking up from
/// the window is the same direction Task 2 established for finding a
/// session's terminal, and for the same reason: from a leaf there is one
/// path upward, while from the root there are many downward.
///
/// The match is on **identity**, not on the number: an ancestor whose pid
/// equals ours only counts if it is still the process we started, proven by
/// its start time. A terminal that daemonises or is reparented leaves that
/// chain and is therefore *not* claimed — the honest answer, and one that
/// costs a `NeverMapped`. Claiming it instead would mean moving whatever
/// window the recycled pid now belongs to, which on this machine is the
/// user's own.
pub fn owns_window(spawned: &Spawned, window_pid: u32) -> bool {
    ancestors(window_pid)
        .into_iter()
        .any(|a| a == spawned.pid && start_ticks(a) == Some(spawned.start_ticks))
}

/// Spawn a terminal for `p` and place it, or say why not.
///
/// The window is found by the process this attempt started, never by class
/// prefix or title: a prefix match would let osm move a window the user
/// opened, and a title names the tmux *window* rather than the session.
///
/// `socket` is the tmux server the restore itself is working against, and it
/// has to travel this far: a terminal told merely to `tmux attach-session -t
/// dev` talks to the *default* server, where it either finds nothing and
/// exits or — worse — attaches to somebody else's identically named session.
pub fn spawn_and_place(
    h: &dyn crate::hypr::HyprCtl,
    sp: &dyn Spawner,
    tmux: &crate::tmux::Tmux,
    p: &Placement,
    marker: &str,
    configured_terminal: &str,
    timeout: std::time::Duration,
) -> PlaceOutcome {
    let Some(kind) = sp.choose_terminal(configured_terminal, &p.terminal_kind) else {
        return PlaceOutcome::SpawnFailed(format!(
            "no terminal to open: restore.terminal is {configured_terminal:?} and the \
             window was captured as {:?}",
            p.terminal_kind
        ));
    };

    let argv = crate::terminal::spawn_argv(kind, &p.session, marker, tmux.socket());
    let spawned = match sp.spawn(&argv) {
        Ok(s) => s,
        Err(e) => return PlaceOutcome::SpawnFailed(format!("{e:#}")),
    };

    // From here a terminal exists. Every failure below therefore both reports
    // itself as work not done *and* takes that terminal back: a compositor
    // that stops answering after the spawn would otherwise leave a window on
    // screen that osm has forgotten, and spawn another one next time.
    let lost = |why: String| -> PlaceOutcome {
        sp.kill(&spawned);
        PlaceOutcome::LostCompositor(why)
    };

    // ---- the window this attempt owns ------------------------------------
    //
    // A read that fails is "not yet", not "the compositor is gone". It used
    // to end the placement and kill the terminal on the first failure, and a
    // single `hyprctl -j clients` that ran out of time was enough — which is
    // what happened on the maintainer's machine with two terminals starting
    // at once. Several terminals mapping at the same moment is not an
    // exceptional condition during a restore; it is what a restore *is*. So
    // the deadline decides, and the last error is what it reports if nothing
    // ever answered.
    let deadline = std::time::Instant::now() + timeout;
    let mut last_error: Option<String>;
    let found = loop {
        match owned_window(h, &spawned, call_budget(deadline)) {
            // Ours because we started the process behind it. The marker is
            // still passed to the terminal — some honour it, and it makes a
            // stray window identifiable by hand — but nothing depends on it.
            Ok(Some(w)) => break w,
            Ok(None) => last_error = None,
            Err(e) => last_error = Some(format!("{e:#}")),
        }
        if std::time::Instant::now() >= deadline {
            return match last_error {
                Some(e) => lost(format!("the compositor stopped answering: {e}")),
                // Deliberately *not* killed. "No window this attempt owns has
                // appeared yet" is a statement about what osm can see, not
                // about what exists: a terminal that is slow to map on a cold
                // boot, or one that reparented its window out of our lineage,
                // is the user's restored terminal and killing it would take
                // away the very thing this pass exists to give back.
                // Degraded, so the snapshot stays restorable and a human can
                // see the shortfall.
                None => PlaceOutcome::NeverMapped,
            };
        }
        std::thread::sleep(MAP_POLL);
    };

    // An **empty** list is not a machine with no screens. Nothing gets this
    // far without [`crate::hypr::wait_until_reachable`] having already
    // watched this compositor report a monitor, and a terminal has just been
    // spawned onto it; a valid, empty `hyprctl -j monitors` arriving after
    // that is a compositor that cannot be read right now — a DPMS
    // transition, a hotplug, a mode switch, a session switched away from.
    //
    // Accepting it cost the promise this pass exists to keep.
    // `resolve_monitor` had nothing to resolve, so `placement_gap` asked
    // about the workspace alone, the window was confirmed `placed` on
    // whatever panel it happened to map on, and the claim carried no
    // connector — which the publication then had nothing to check either. A
    // restore that put the user's terminal on the wrong monitor reported
    // `succeeded` and retired the snapshot that knew the right one.
    //
    // But refusing *once* is not the same decision. Every failure from here
    // on takes the terminal back — that is what `lost` does — so a single
    // blink was enough to destroy a window that had already mapped, already
    // held the user's session, and would have been placed correctly a
    // hundred milliseconds later. A mode switch on a one-monitor machine
    // produces exactly one such blink, and one monitor is what the
    // maintainer has: this read is on the path of every restore they do. So
    // the list is asked for again, within a bounded budget, and only a
    // compositor that never answers is called lost.
    let monitors = match monitors_within(h, std::time::Instant::now() + MONITOR_READ_BUDGET) {
        Ok(m) => m,
        Err(why) => return lost(why),
    };
    // The same monitor `place_lua` will address, read out here so the claim
    // names where the window was actually sent rather than where the capture
    // found it: the two differ whenever a connector was renamed and the panel
    // was matched by description instead.
    //
    // A `Placed` claim is never made without it. `resolve_monitor` falls back
    // to the focused monitor and then to the first, so on a list with any
    // monitor in it this always answers; the arm below is what keeps that a
    // fact rather than an assumption, because the one outcome that retires
    // the user's snapshot must not be reachable through a monitor nobody
    // resolved.
    let Some(sent_to) = resolve_monitor(p, &monitors).map(|m| m.name.clone()) else {
        return lost(
            "the compositor listed monitors that name no panel to send the window to".to_string(),
        );
    };

    // ---- place it, and make the compositor say it is there ---------------
    let placed = match confirm_placement(h, &spawned, found, p, &monitors, timeout) {
        Confirmation::Placed(w) => w,
        // Not killed: see [`PlaceOutcome::Misplaced`]. The window is the
        // user's restored terminal, holding their session, in the wrong
        // place.
        Confirmation::Misplaced(why) => return PlaceOutcome::Misplaced(why),
        Confirmation::Lost(why) => return lost(why),
    };

    // Moved, but not yet the session's terminal. A window maps and takes
    // dispatches before the shell inside it has run `tmux attach-session`,
    // and one whose attach fails outright maps exactly the same way. Until a
    // client of *this* terminal is attached to *this* session, the session
    // has no terminal window — and saying otherwise published a snapshot with
    // no placement in it and retired the one that had it.
    //
    // The attach gets an interval of its own, counted from the moment the
    // window mapped. Sharing the spawn's deadline meant a terminal that was
    // slow to map — the ordinary case on a cold boot, which is when this runs
    // — was left whatever remained of the budget to attach in, sometimes
    // nothing at all.
    //
    // A window that drifts off its workspace *after* this point is not
    // covered here and is not meant to be: the restore reads the whole
    // desktop back before it publishes anything, and refuses to retire the
    // source snapshot when what it finds disagrees with what it placed. That
    // check is what caught this bug in the first place.
    let attach_deadline = std::time::Instant::now() + timeout;
    match attached(tmux, &spawned, &p.session, attach_deadline) {
        Attach::Yes => PlaceOutcome::Placed(PlacedWindow {
            address: placed.address,
            workspace_kind: p.workspace_kind.clone(),
            workspace_ref: p.workspace_ref.clone(),
            monitor_connector: Some(sent_to),
        }),
        // A terminal osm started that holds **no session at all** — not
        // merely "not this one", which is a different answer and is reported
        // as [`Attach::Unknown`] below. Left running it is an empty window on
        // the user's desktop that no later attempt can adopt — each retry
        // spawns another beside it — so it is taken back, the same way a lost
        // compositor's is.
        Attach::No => {
            sp.kill(&spawned);
            PlaceOutcome::NeverAttached
        }
        // Either the server never gave a readable answer, or a client of ours
        // is attached to some other session. Both mean the same thing here:
        // "this terminal holds nothing" is not something this pass knows, and
        // the window may be showing the user their work. Killing it on a
        // guess is the one mistake worse than leaving it.
        Attach::Unknown => PlaceOutcome::NeverAttached,
    }
}

/// How often the compositor is asked whether this attempt's window has
/// mapped yet.
const MAP_POLL: std::time::Duration = std::time::Duration::from_millis(100);

/// How often the window is read back while a placement is being confirmed.
const CONFIRM_POLL: std::time::Duration = std::time::Duration::from_millis(150);

/// How long a placement is given to actually take effect, once every
/// dispatch asking for it has been acknowledged.
///
/// Short, and deliberately not the readiness budget. Moving a window is
/// synchronous on Hyprland — measured against the live compositor, the window
/// is on its new workspace by the time the next `hyprctl -j clients` returns
/// — so this is not a wait for slow work. It is headroom for a window that
/// something else moves back, and a bound on how long a placement that is
/// never going to succeed may hold up the rest of the restore: with the
/// readiness budget (30s by default) instead, one window that cannot be
/// placed would cost every session behind it half a minute each.
const CONFIRM_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);

/// The least time a compositor call is ever started with.
///
/// A deadline bounds how long osm *waits*; it must not become a reason to cut
/// a call off after a millisecond and report the placement as failed for
/// nothing but arithmetic. So the last call before a deadline gets a whole
/// second, and the worst case is one call's overrun — against the ninety
/// seconds per window this replaced.
const MIN_CALL_BUDGET: std::time::Duration = std::time::Duration::from_secs(1);

/// Whether `deadline` has passed.
fn expired(deadline: std::time::Instant) -> bool {
    std::time::Instant::now() >= deadline
}

/// How long a single compositor call may take if it is started now: whatever
/// is left before `deadline`, floored at [`MIN_CALL_BUDGET`].
///
/// `hypr::run_hyprctl` applies the ceiling, so nothing here can hand out more
/// than [`crate::hypr::CALL_TIMEOUT`].
fn call_budget(deadline: std::time::Instant) -> std::time::Duration {
    deadline
        .saturating_duration_since(std::time::Instant::now())
        .max(MIN_CALL_BUDGET)
}

/// How long the compositor is given to produce a monitor list it can be
/// confirmed against, before the placement is called lost.
///
/// Deliberately its own budget rather than the per-window one. It bounds a
/// *retry*, not a wait for slow work: a compositor that is there answers this
/// on the first call, and the only thing being waited out is the instant in
/// which it has no output to report. Three seconds is long enough to cross a
/// mode switch or a DPMS blank and short enough that a compositor which
/// really has gone away does not hold up the sessions behind this one.
const MONITOR_READ_BUDGET: std::time::Duration = std::time::Duration::from_secs(3);

/// How often the monitor list is re-asked for while [`MONITOR_READ_BUDGET`]
/// lasts.
const MONITOR_READ_POLL: std::time::Duration = std::time::Duration::from_millis(150);

/// The compositor's monitor list, re-asked for until it names a panel or
/// `deadline` passes.
///
/// The error is the sentence [`spawn_and_place`] reports, and it describes
/// the *last* thing that went wrong rather than the first, because that is
/// the state the compositor was left in. The three are kept apart on purpose:
/// "it answered, with nothing", "its answer could not be parsed" and "it did
/// not answer" are different failures and a report that blurs them tells a
/// human nothing about which machine they are looking at.
///
/// One attempt always happens, whatever `deadline` says: a budget is a bound
/// on waiting, never a reason to skip the question. Every attempt *after* that
/// one is checked against the clock twice — before the wait and again after it
/// — and the wait itself is cut to whatever is left. Sleeping a flat
/// [`MONITOR_READ_POLL`] and then asking again unchecked turned this three
/// second bound into four and a bit: the sleep stepped over the deadline and
/// [`call_budget`] handed the read that followed a whole second on top of a
/// deadline that had already gone. Per restored session, on a boot that walks
/// them one after another.
fn monitors_within(
    h: &dyn crate::hypr::HyprCtl,
    deadline: std::time::Instant,
) -> Result<Vec<Monitor>, String> {
    let mut why;
    loop {
        match h.monitors_json(call_budget(deadline)) {
            Ok(json) => match crate::hypr::parse_monitors(&json) {
                Ok(monitors) if !monitors.is_empty() => return Ok(monitors),
                Ok(_) => {
                    why = "the compositor kept listing no monitors at all, after readiness \
                           had already seen one: its window cannot be confirmed on the \
                           monitor it was captured on"
                        .to_string()
                }
                Err(e) => why = format!("the compositor's monitor list could not be read: {e:#}"),
            },
            Err(e) => why = format!("the compositor stopped listing its monitors: {e:#}"),
        }
        if expired(deadline) {
            return Err(why);
        }
        // Never past the deadline, and never *up to* it and then one more
        // read: the second check is what stops a call being started with
        // nothing left, which `call_budget` would still grant a second.
        std::thread::sleep(
            MONITOR_READ_POLL.min(deadline.saturating_duration_since(std::time::Instant::now())),
        );
        if expired(deadline) {
            return Err(why);
        }
    }
}

/// What [`confirm_placement`] established.
enum Confirmation {
    /// The compositor says the window is on its workspace and its monitor.
    /// Carries the window as it was last read — which is not necessarily the
    /// one that went in, see [`owned_window`].
    Placed(Client),
    /// It is not, and the budget for making it so has run out.
    Misplaced(String),
    /// The compositor stopped answering, or refused a dispatch.
    Lost(String),
}

/// The window this attempt's process owns, as the compositor reports it now.
///
/// Ownership, never the address, is the identity. Hyprland reuses `CWindow`
/// allocations, so a window that is destroyed and re-created can hand its
/// address straight to something else — the same addresses recurred across
/// independent runs on the maintainer's machine. A confirmation loop that
/// looked its window up by address could therefore be satisfied by a stranger
/// that happened to inherit it and happened to be on the target workspace,
/// and would report a placement osm never made. Looking it up by the process
/// tree instead follows *our* window when it is re-created, and never claims
/// somebody else's.
fn owned_window(
    h: &dyn crate::hypr::HyprCtl,
    spawned: &Spawned,
    budget: std::time::Duration,
) -> Result<Option<Client>> {
    let json = h.clients_json(budget)?;
    let windows = crate::hypr::parse_clients(&json)?;
    Ok(windows.into_iter().find(|w| owns_window(spawned, w.pid)))
}

/// Dispatch what `p` still needs, then read the desktop back until it agrees
/// — or until the budget runs out and it has to say that it does not.
///
/// # Why a dispatch is not evidence
///
/// `hyprctl dispatch` answers `ok` when the compositor **accepted** the call.
/// That is not a claim about where the window is. Two terminals on the
/// maintainer's desktop, captured on workspaces 9 and 10, came back on
/// workspace 2 with `ok` from every dispatch and `"outcome":"placed"` on both
/// — because the second dispatch osm made undid the first, which no amount of
/// checking the *reply* could ever have revealed. The only thing that can is
/// asking the compositor where the window is.
///
/// Bounded by `timeout` and by [`CONFIRM_BUDGET`], whichever is shorter, so a
/// window that will never land cannot hold up the sessions behind it.
fn confirm_placement(
    h: &dyn crate::hypr::HyprCtl,
    spawned: &Spawned,
    found: Client,
    p: &Placement,
    monitors: &[Monitor],
    timeout: std::time::Duration,
) -> Confirmation {
    let deadline = std::time::Instant::now() + timeout.min(CONFIRM_BUDGET);
    let mut w = found;
    loop {
        for lua in place_lua(&w, p, monitors) {
            // Before the dispatch, not only after the batch of them. Five
            // dispatches were issued back to back and only then was the clock
            // consulted, so a floating window could spend five full
            // `hyprctl` timeouts past its deadline before anything noticed —
            // and every session behind it waited.
            if expired(deadline) {
                return Confirmation::Misplaced(format!(
                    "the budget for confirming the placement ran out with {lua} \
                     still to dispatch"
                ));
            }
            match h.dispatch(&lua, call_budget(deadline)) {
                // `Ok` only means the call was made. A rejected dispatch
                // comes back as text on successful stdout, and taking that
                // for success recorded an unmoved window as placed.
                Ok(reply) if crate::hypr::dispatch_acknowledged(&reply) => {}
                Ok(reply) => {
                    return Confirmation::Lost(format!(
                        "the compositor did not acknowledge {lua}: {}",
                        reply.trim()
                    ))
                }
                // A call that failed *after* the deadline is this restore's
                // own budget running out, not a compositor that has gone —
                // and `Lost` takes the user's terminal away with it. Only a
                // failure with time still on the clock is evidence about the
                // compositor.
                Err(e) if expired(deadline) => {
                    return Confirmation::Misplaced(format!(
                        "the budget for confirming the placement ran out while \
                         dispatching {lua}: {e:#}"
                    ))
                }
                Err(e) => return Confirmation::Lost(format!("dispatching {lua}: {e:#}")),
            }
        }

        // Read it back. Nothing above this line is evidence of anything.
        //
        // `last` is what the most recent read in this pass established:
        // `None` for a pass in which the budget ran out before one could be
        // made at all, `Some(Ok(()))` for a compositor that answered and does
        // not list our window, `Some(Err(_))` for one that gave no readable
        // answer.
        let mut last: Option<std::result::Result<(), String>> = None;
        let now = loop {
            // Same rule as the dispatches: the deadline is checked before the
            // call, so a read cannot start after the budget has gone and then
            // run for a further `hyprctl` timeout.
            if expired(deadline) {
                break None;
            }
            match owned_window(h, spawned, call_budget(deadline)) {
                Ok(Some(w)) => break Some(w),
                Ok(None) => last = Some(Ok(())),
                Err(e) => last = Some(Err(format!("{e:#}"))),
            }
            if expired(deadline) {
                break None;
            }
            std::thread::sleep(CONFIRM_POLL);
        };
        let Some(now) = now else {
            // All three are `Misplaced`, and none of them kills the terminal.
            // A read can only end this loop at the deadline, so a failed one
            // is as likely to be this restore's own budget cutting a slow
            // `hyprctl` short as it is to be a compositor that has gone — and
            // the window on the screen is the user's restored terminal
            // holding their session. `Lost`, which takes it back, is kept for
            // a dispatch the compositor refused or failed while there was
            // still time on the clock: that is evidence about the compositor
            // rather than about the clock.
            return Confirmation::Misplaced(match last {
                Some(Err(e)) => format!(
                    "the budget for confirming the placement ran out, and the last \
                     read of the compositor failed: {e}"
                ),
                Some(Ok(())) => "its window was gone from the compositor before the \
                     placement could be confirmed"
                    .to_string(),
                None => match placement_gap(&w, p, monitors) {
                    Some(gap) => format!(
                        "the budget for confirming the placement ran out with the \
                         compositor last reporting the window at {}: {gap}",
                        w.address
                    ),
                    None => "the budget for confirming the placement ran out before \
                         the compositor could be read back"
                        .to_string(),
                },
            });
        };

        let Some(gap) = placement_gap(&now, p, monitors) else {
            return Confirmation::Placed(now);
        };
        if std::time::Instant::now() >= deadline {
            return Confirmation::Misplaced(format!(
                "the compositor acknowledged every dispatch, and then reported the \
                 window at {}: {gap}",
                now.address
            ));
        }
        w = now;
        std::thread::sleep(CONFIRM_POLL);
    }
}

/// How often the attach is asked about while the deadline runs.
const ATTACH_POLL: std::time::Duration = std::time::Duration::from_millis(100);

/// What [`attached`] was able to establish before its deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attach {
    /// A client of the terminal this attempt started is attached to the
    /// session.
    Yes,
    /// The server answered, readably, and holds **no client of ours at
    /// all** — not merely none on the session asked about. The only answer
    /// on which a terminal may be taken back.
    No,
    /// Nothing was established either way — which is not the same as "no".
    /// Either the server gave no readable answer, or a client of ours is
    /// attached to a *different* session: alive, ours, and showing the user
    /// something.
    Unknown,
}

/// What one reply to `list-clients` establishes.
///
/// `clients` is `None` for a reply that never arrived or did not parse: not
/// "there are no clients", which is the answer a terminal may be killed on.
///
/// # Ownership and session are two questions, asked in that order
///
/// Asking them as one predicate — "a client of ours, on this session" —
/// makes a terminal of ours that is attached to *something else*
/// indistinguishable from one attached to nothing, and the second of those
/// is killed. A `client-attached` hook that switches the spawned client
/// (`switch-client`, a session picker, a wrapper that lands the user
/// somewhere) produces exactly that: a window on the user's screen, holding
/// a session, with a shell and agents inside it, which osm then ended at the
/// timeout. So ownership is settled first, and only a client that is *not*
/// ours is passed over.
pub fn verdict_of(clients: Option<&[ClientPid]>, spawned: &Spawned, session: &str) -> Attach {
    let Some(clients) = clients else {
        return Attach::Unknown;
    };
    let mut ours_elsewhere = false;
    for c in clients {
        // The user's own terminal, holding whatever it holds. Not ours to
        // reason about and never ours to kill.
        if !owns_window(spawned, c.pid) {
            continue;
        }
        if c.session == session {
            return Attach::Yes;
        }
        ours_elsewhere = true;
    }
    if ours_elsewhere {
        // Alive, ours, and showing the user a session — just not the one this
        // placement was for. The placement is unfinished, which the caller
        // reports; the terminal is not an empty window, so it stays.
        Attach::Unknown
    } else {
        Attach::No
    }
}

/// [`attached`], asking `poll` rather than a tmux server.
///
/// The seam exists so the decision that authorises killing a terminal can be
/// driven from a test without a server, a client, or a process to signal.
pub fn attach_verdict(
    spawned: &Spawned,
    session: &str,
    deadline: std::time::Instant,
    poll: &mut dyn FnMut() -> Option<Vec<ClientPid>>,
) -> Attach {
    loop {
        let v = verdict_of(poll().as_deref(), spawned, session);
        if matches!(v, Attach::Yes) {
            return Attach::Yes;
        }
        if std::time::Instant::now() >= deadline {
            // Only [`Attach::No`] authorises anything, so only it is worth
            // asking twice.
            if !matches!(v, Attach::No) {
                return v;
            }
            // The poll that ran the clock out is not the answer a kill is
            // taken on. The attach and the deadline are unrelated clocks, and
            // a client that attached while that poll was in flight was
            // attached before anything was killed — it costs one more
            // `list-clients` to let it say so, and the alternative is ending
            // a window the user is looking at because of a few milliseconds.
            return verdict_of(poll().as_deref(), spawned, session);
        }
        std::thread::sleep(ATTACH_POLL);
    }
}

/// Whether a tmux client attached to `session` is running inside the terminal
/// `spawned` started, polling until `deadline`.
///
/// Ownership is the same process-tree proof placement uses for the window: a
/// client counts only if `spawned` is in its ancestry *and* is still the
/// process this restore started. Any attached client would not do — the user
/// may already have that session open in a terminal of their own, and taking
/// theirs as proof would report a terminal osm never delivered.
///
/// A `list-clients` that fails is simply "not yet": the loop retries, and the
/// deadline decides. What the *final* answer — re-asked immediately before
/// the caller acts on it — establishes is the difference between
/// [`Attach::No`] and [`Attach::Unknown`], and therefore between a terminal
/// the caller may take back and one it may not: a server that has stopped
/// answering says nothing about what is on the user's screen, and neither
/// does a poll that has since been overtaken by the attach it was waiting
/// for.
fn attached(
    tmux: &crate::tmux::Tmux,
    spawned: &Spawned,
    session: &str,
    deadline: std::time::Instant,
) -> Attach {
    attach_verdict(spawned, session, deadline, &mut || {
        tmux.run(&["list-clients", "-F", "#{client_pid} #{client_session}"])
            .ok()
            // A reply that does not parse is not "no clients" — the same rule
            // the placement collection follows. It is retried like any other
            // unreadable answer.
            .and_then(|raw| parse_clients_output(&raw).ok())
    })
}
