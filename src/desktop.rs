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

/// The Lua dispatches that place `address`, in the order they must run.
///
/// The shape is verified against a live compositor by `tests/desktop_live.rs`:
/// Hyprland 0.56 dropped the shell-style `[workspace N silent]` rule syntax,
/// which fails with `']' expected near '4'` while looking perfectly correct.
pub fn place_lua(address: &str, p: &Placement, monitors: &[Monitor]) -> Vec<String> {
    let win = lua_str(&format!("address:{address}"));
    let mut out = vec![format!(
        "hl.dsp.window.move({{window={win}, workspace={}, follow=false}})",
        lua_str(&p.workspace_ref)
    )];

    let target = resolve_monitor(p, monitors);
    // Unconditionally, never "only when the name changed". A workspace does
    // not stay on the output it was captured from: with `DP-1` still present
    // but workspace 3 currently living on `eDP-1`, the workspace move alone
    // puts the terminal on the wrong panel, and the connector comparison
    // then skipped the one dispatch that would have corrected it — so the
    // common case, an unchanged monitor layout, was the case that never
    // moved a window to its monitor.
    if let Some(m) = target {
        out.push(format!(
            "hl.dsp.window.move({{window={win}, monitor={}}})",
            lua_str(&m.name)
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
    Ok(placements_with_incarnation(h, tmux)?.map(|(_, ps)| ps))
}

/// [`placements`], and the tmux incarnation the mapping was read from.
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
pub fn placements_with_incarnation(
    h: &dyn crate::hypr::HyprCtl,
    tmux: &crate::tmux::Tmux,
) -> Result<Option<(String, Vec<Placement>)>> {
    // The tmux server's identity is read *before* the compositor and again
    // after the client list, so the whole mapping is known to describe one
    // server. A server replaced in between hands out `$0`, `%0`, … from zero
    // again, and the sessions its clients name are not the sessions this
    // snapshot's topology holds.
    //
    // `Ok(None)` — no server at all — is `None` here too, deliberately.
    // Terminals do not vanish when tmux dies; a window whose session cannot
    // be read is a window whose placement is unknown, and "unknown" must
    // never be written down as "there were none".
    let before = match tmux.running_server_incarnation() {
        Ok(Some(id)) => id,
        _ => return Ok(None),
    };

    let clients_json = match h.clients_json() {
        Ok(j) => j,
        Err(_) => return Ok(None),
    };
    let monitors_json = match h.monitors_json() {
        Ok(j) => j,
        Err(_) => return Ok(None),
    };
    // A malformed reply is an unreachable compositor, not an empty desktop.
    let (windows, monitors) = match (
        crate::hypr::parse_clients(&clients_json),
        crate::hypr::parse_monitors(&monitors_json),
    ) {
        (Ok(w), Ok(m)) => (w, m),
        _ => return Ok(None),
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
        Err(_) => return Ok(None),
    };
    let tmux_clients = match parse_clients_output(&raw) {
        Ok(cs) => cs,
        Err(_) => return Ok(None),
    };
    match tmux.running_server_incarnation() {
        Ok(Some(after)) if after == before => {}
        _ => return Ok(None),
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
    Ok(Some((before, out)))
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

/// Write a snapshot's window placement.
///
/// Called inside the snapshot's own transaction so placement lands with the
/// topology it describes or not at all. `None` means the compositor could
/// not be trusted and nothing is written — the rows already present belong
/// to earlier snapshots and are left alone.
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
    /// where the window was actually sent. `None` when the compositor listed
    /// no monitor to send it to and no monitor dispatch was made.
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
}

impl PlaceOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            PlaceOutcome::Placed(_) => "placed",
            PlaceOutcome::SpawnFailed(_) => "spawn_failed",
            PlaceOutcome::NeverMapped => "never_mapped",
            PlaceOutcome::NeverAttached => "never_attached",
            PlaceOutcome::NoCompositor => "no_compositor",
            PlaceOutcome::LostCompositor(_) => "lost_compositor",
            PlaceOutcome::Skipped(_) => "skipped",
            PlaceOutcome::PlacementDisabled => "placement_disabled",
        }
    }

    /// The detail a report can show beside [`Self::as_str`], if any.
    pub fn detail(&self) -> Option<&str> {
        match self {
            PlaceOutcome::SpawnFailed(d)
            | PlaceOutcome::LostCompositor(d)
            | PlaceOutcome::Skipped(d) => Some(d),
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

    let deadline = std::time::Instant::now() + timeout;
    loop {
        let json = match h.clients_json() {
            Ok(j) => j,
            Err(e) => return lost(format!("the compositor stopped answering: {e:#}")),
        };
        let windows = match crate::hypr::parse_clients(&json) {
            Ok(w) => w,
            Err(e) => return lost(format!("the compositor stopped answering: {e:#}")),
        };
        // Ours because we started the process behind it. The marker is still
        // passed to the terminal — some honour it, and it makes a stray
        // window identifiable by hand — but nothing depends on it.
        if let Some(w) = windows.iter().find(|w| owns_window(&spawned, w.pid)) {
            let monitors = match h
                .monitors_json()
                .ok()
                .and_then(|j| crate::hypr::parse_monitors(&j).ok())
            {
                Some(m) => m,
                None => return lost("the compositor stopped listing its monitors".to_string()),
            };
            // The same monitor `place_lua` will address, read out here so the
            // claim names where the window was actually sent rather than
            // where the capture found it: the two differ whenever a connector
            // was renamed and the panel was matched by description instead.
            let sent_to = resolve_monitor(p, &monitors).map(|m| m.name.clone());
            for lua in place_lua(&w.address, p, &monitors) {
                match h.dispatch(&lua) {
                    // `Ok` only means the call was made. A rejected dispatch
                    // comes back as text on successful stdout, and taking
                    // that for success recorded an unmoved window as placed.
                    Ok(reply) if crate::hypr::dispatch_acknowledged(&reply) => {}
                    Ok(reply) => {
                        return lost(format!(
                            "the compositor did not acknowledge {lua}: {}",
                            reply.trim()
                        ))
                    }
                    Err(e) => return lost(format!("dispatching {lua}: {e:#}")),
                }
            }
            // Moved, but not yet the session's terminal. A window maps and
            // takes dispatches before the shell inside it has run
            // `tmux attach-session`, and one whose attach fails outright maps
            // exactly the same way. Until a client of *this* terminal is
            // attached to *this* session, the session has no terminal window
            // — and saying otherwise published a snapshot with no placement
            // in it and retired the one that had it.
            //
            // The attach gets an interval of its own, counted from the moment
            // the window mapped. Sharing the spawn's deadline meant a
            // terminal that was slow to map — the ordinary case on a cold
            // boot, which is when this runs — was left whatever remained of
            // the budget to attach in, sometimes nothing at all.
            let attach_deadline = std::time::Instant::now() + timeout;
            return match attached(tmux, &spawned, &p.session, attach_deadline) {
                Attach::Yes => PlaceOutcome::Placed(PlacedWindow {
                    address: w.address.clone(),
                    workspace_kind: p.workspace_kind.clone(),
                    workspace_ref: p.workspace_ref.clone(),
                    monitor_connector: sent_to,
                }),
                // A terminal osm started that holds **no session at all** —
                // not merely "not this one", which is a different answer and
                // is reported as [`Attach::Unknown`] below. Left running it
                // is an empty window on the user's desktop that no later
                // attempt can adopt — each retry spawns another beside it —
                // so it is taken back, the same way a lost compositor's is.
                Attach::No => {
                    sp.kill(&spawned);
                    PlaceOutcome::NeverAttached
                }
                // Either the server never gave a readable answer, or a
                // client of ours is attached to some other session. Both
                // mean the same thing here: "this terminal holds nothing" is
                // not something this pass knows, and the window may be
                // showing the user their work. Killing it on a guess is the
                // one mistake worse than leaving it.
                Attach::Unknown => PlaceOutcome::NeverAttached,
            };
        }
        if std::time::Instant::now() >= deadline {
            // Deliberately *not* killed. "No window this attempt owns has
            // appeared yet" is a statement about what osm can see, not about
            // what exists: a terminal that is slow to map on a cold boot, or
            // one that reparented its window out of our lineage, is the
            // user's restored terminal and killing it would take away the
            // very thing this pass exists to give back. Degraded, so the
            // snapshot stays restorable and a human can see the shortfall.
            return PlaceOutcome::NeverMapped;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
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
