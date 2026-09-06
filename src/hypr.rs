//! A validated seam over `hyprctl`.
//!
//! Every Hyprland interaction in this crate goes through the [`HyprCtl`]
//! trait so the logic that consumes it is testable without a compositor, and
//! every reply is validated as the shape it must be *before* it is treated as
//! data.
//!
//! # A malformed reply must never read as an empty desktop
//!
//! The maintainer's original workspace layout was lost exactly this way: a
//! capture ran while the tmux server was down, its output was not the array
//! `hyprctl` promises, and the caller read that as "no windows" and wrote it
//! over a seven-session layout with no history to recover from. So
//! [`parse_clients`] and [`parse_monitors`] return `Err` for anything that is
//! not a JSON array — including `{}`, `null`, a bare string, and empty input.
//! An empty desktop and a broken compositor must never look the same.

use anyhow::{anyhow, bail, Context, Result};
use std::io::Read as _;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// A window as `hyprctl -j clients` reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct Client {
    pub address: String,
    pub pid: u32,
    pub class: String,
    pub title: String,
    pub workspace_id: i64,
    pub workspace_name: String,
    /// The monitor this window is on, as the *index* the compositor reports.
    ///
    /// A client does not name its monitor. Verified against Hyprland 0.56:
    /// `hyprctl -j clients` has no `monitorName` key at all — only `monitor`,
    /// a number. The connector (`DP-1`), description and serial live solely in
    /// `hyprctl -j monitors`. Resolve this through [`connector_of`]; storing
    /// the raw index where placement later looks for a connector matches
    /// nothing and silently drops every window onto the focused output.
    pub monitor_index: i64,
    pub at: (i32, i32),
    pub size: (i32, i32),
    pub floating: bool,
}

/// A monitor as `hyprctl -j monitors` reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct Monitor {
    /// The index a client's `monitor` field refers to.
    pub id: i64,
    pub name: String,
    pub description: String,
    pub make: String,
    pub model: String,
    pub serial: String,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub scale: f32,
    pub transform: i32,
    pub focused: bool,
}

impl Monitor {
    /// The monitor's size in the coordinate space window geometry uses.
    ///
    /// `hyprctl monitors` reports `width`/`height` in **pixels** and `scale`
    /// separately, while a client's `at`/`size` and a monitor's `x`/`y` are
    /// **logical**. Dividing one by the other is not a rounding error: a
    /// window captured on a 3840x2160 panel at scale 2 occupies half the
    /// logical space its pixel dimensions suggest, so a capture that used
    /// pixels restored it at half size once the scale changed to 1. A rotated
    /// output additionally swaps the axes -- the odd `transform` values are
    /// the 90 degrees and 270 degrees ones, plain and flipped alike -- so a
    /// portrait panel clamped windows against landscape bounds.
    ///
    /// `None` for a monitor that cannot describe a usable area: zero or
    /// negative pixel dimensions, or a scale that is not a positive finite
    /// number. Never a substituted 1.0 -- guessing a scale silently produces
    /// geometry that is wrong by exactly the factor nobody can see.
    pub fn logical_size(&self) -> Option<(f32, f32)> {
        if self.width <= 0 || self.height <= 0 {
            return None;
        }
        if !self.scale.is_finite() || self.scale <= 0.0 {
            return None;
        }
        let (w, h) = if self.transform.rem_euclid(2) == 1 {
            (self.height, self.width)
        } else {
            (self.width, self.height)
        };
        Some((w as f32 / self.scale, h as f32 / self.scale))
    }
}

/// The one seam through which this crate talks to Hyprland.
///
/// Substituted with a stub in every test that does not need a live
/// compositor; only [`Live`] shells out.
/// Every method takes the **budget** the caller can afford to wait, and
/// [`Live`] holds the `hyprctl` process to it.
///
/// It is a parameter rather than a constant because the callers have
/// deadlines and the calls have to fit inside them. A placement confirmation
/// is bounded by [`crate::desktop`]'s confirmation budget, and a floating
/// window issues five sequential dispatches and then a read: with a flat
/// per-call timeout of [`CALL_TIMEOUT`] each, one window could hold the
/// restore for a minute and a half *after* its deadline had passed, and every
/// session behind it waited its turn. On a boot restoring seven sessions that
/// is the difference between seconds and ten minutes.
pub trait HyprCtl {
    fn clients_json(&self, budget: Duration) -> Result<String>;
    fn monitors_json(&self, budget: Duration) -> Result<String>;
    /// Issue one dispatch and return the compositor's reply **verbatim**.
    ///
    /// `Ok` means the call was made, not that the compositor honoured it: a
    /// rejected dispatch is reported as text on successful stdout. Every
    /// caller must put the reply through [`dispatch_acknowledged`] before
    /// treating the window as moved.
    fn dispatch(&self, lua: &str, budget: Duration) -> Result<String>;
}

/// The longest any single `hyprctl` invocation may run, whatever budget it is
/// given. A hung compositor must not hang osm.
///
/// # Why not five seconds
///
/// It was five, and a restore of two sessions on the maintainer's machine hit
/// it: `hyprctl -j clients did not finish within 5s`, with two Ghostty
/// windows starting at once. That is not a hung compositor — it is a busy
/// one, doing exactly the work this restore asked it to do, at the one moment
/// a restore ever runs. A boot restores every session the user had, so
/// several terminals map within a few seconds of each other and every one of
/// them competes with these reads.
///
/// The two mistakes are not symmetrical. Waiting longer than necessary costs
/// seconds during a boot nobody is watching. Calling a slow reply a dead
/// compositor costs the user their restored terminal: the placement pass
/// kills the terminal it started when it decides the compositor is gone. So
/// the budget is generous, and the loops that use it treat a single failed
/// read as "not yet" rather than as an answer — see
/// [`crate::desktop::spawn_and_place`].
/// This is a **ceiling**, not the time every call gets: a caller working
/// against a deadline passes what it can still afford and [`run_hyprctl`]
/// takes the smaller of the two. See [`HyprCtl`].
pub const CALL_TIMEOUT: Duration = Duration::from_secs(15);

/// Shells out to the real `hyprctl` binary.
pub struct Live;

impl Live {
    pub fn new() -> Self {
        Live
    }
}

impl Default for Live {
    fn default() -> Self {
        Self::new()
    }
}

/// Runs `hyprctl <args>`, killing it and returning an error if it does not
/// finish within `budget` — or within [`CALL_TIMEOUT`], whichever is shorter.
///
/// The cap is applied here rather than trusted to callers, so no caller can
/// leave one `hyprctl` running longer than the ceiling by passing a large
/// budget.
fn run_hyprctl(args: &[&str], budget: Duration) -> Result<String> {
    let budget = budget.min(CALL_TIMEOUT);
    let mut child = Command::new("hyprctl")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawn `hyprctl {}`", args.join(" ")))?;

    let deadline = Instant::now() + budget;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut stdout = String::new();
                let mut stderr = String::new();
                if let Some(mut out) = child.stdout.take() {
                    let _ = out.read_to_string(&mut stdout);
                }
                if let Some(mut err) = child.stderr.take() {
                    let _ = err.read_to_string(&mut stderr);
                }
                if !status.success() {
                    bail!(
                        "`hyprctl {}` exited {status}: {}",
                        args.join(" "),
                        stderr.trim()
                    );
                }
                return Ok(stdout);
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    bail!(
                        "`hyprctl {}` did not finish within {budget:?}",
                        args.join(" ")
                    );
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => {
                let _ = child.kill();
                bail!("waiting on `hyprctl {}`: {e}", args.join(" "));
            }
        }
    }
}

impl HyprCtl for Live {
    fn clients_json(&self, budget: Duration) -> Result<String> {
        run_hyprctl(&["-j", "clients"], budget)
    }

    fn monitors_json(&self, budget: Duration) -> Result<String> {
        run_hyprctl(&["-j", "monitors"], budget)
    }

    fn dispatch(&self, lua: &str, budget: Duration) -> Result<String> {
        run_hyprctl(&["dispatch", lua], budget)
    }
}

/// `true` when Hyprland is reachable and answers with a plausible monitor
/// list. Used to distinguish "no compositor" (legitimate, e.g. headless) from
/// "compositor answered something we cannot trust".
pub fn reachable(h: &dyn HyprCtl) -> bool {
    match h.monitors_json(CALL_TIMEOUT) {
        Ok(json) => matches!(parse_monitors(&json), Ok(ms) if !ms.is_empty()),
        Err(_) => false,
    }
}

/// How often [`wait_until_reachable`] re-asks a compositor that is not
/// answering yet.
const READY_POLL: Duration = Duration::from_millis(200);

/// Waits up to `timeout` for [`reachable`] to become true.
///
/// A restore that runs while Hyprland is still starting used to see "no
/// compositor" and report the placement it never did as a completed outcome.
/// The wait is bounded by the same readiness budget the rest of the restore
/// uses, and returns `false` rather than erroring so the caller can decide
/// what an absent compositor means.
pub fn wait_until_reachable(h: &dyn HyprCtl, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if reachable(h) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(READY_POLL);
    }
}

/// Whether a `dispatch` reply says the compositor actually did it.
///
/// Hyprland answers a dispatch it performed with exactly `ok`, and reports a
/// rejected one — an unknown field, a malformed argument — as text on
/// *successful* stdout. Checking only `Result::is_err()` therefore recorded a
/// refused dispatch as a placed window. Verified against Hyprland 0.56.2:
/// `hl.dsp.window.move({window='address:0xdeadbeef', …})` prints `ok`, while
/// a bad call prints `error: …` (and exits non-zero, which is caught anyway).
pub fn dispatch_acknowledged(reply: &str) -> bool {
    reply.trim().eq_ignore_ascii_case("ok")
}

/// Parses `hyprctl -j clients` output. Errors on anything that is not a JSON
/// array at the top level — see the module docs for why.
pub fn parse_clients(raw: &str) -> Result<Vec<Client>> {
    let value: serde_json::Value = serde_json::from_str(raw)
        .map_err(|e| anyhow!("hyprctl clients did not print valid JSON: {e}"))?;
    let arr = value
        .as_array()
        .ok_or_else(|| anyhow!("hyprctl clients printed {}, not an array", describe(&value)))?;

    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        let address = item
            .get("address")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let pid = item.get("pid").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        let class = item
            .get("class")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let title = item
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let workspace_id = item
            .get("workspace")
            .and_then(|w| w.get("id"))
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let workspace_name = item
            .get("workspace")
            .and_then(|w| w.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        // A client reports only a number. Verified against Hyprland 0.56:
        // there is no `monitorName` key on a client at all. Resolve it with
        // `connector_of` against the monitor list.
        let monitor_index = item.get("monitor").and_then(|v| v.as_i64()).unwrap_or(-1);
        let at = item
            .get("at")
            .and_then(|v| v.as_array())
            .map(|a| {
                let x = a.first().and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                let y = a.get(1).and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                (x, y)
            })
            .unwrap_or((0, 0));
        let size = item
            .get("size")
            .and_then(|v| v.as_array())
            .map(|a| {
                let w = a.first().and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                let h = a.get(1).and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                (w, h)
            })
            .unwrap_or((0, 0));
        let floating = item
            .get("floating")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        out.push(Client {
            address,
            pid,
            class,
            title,
            workspace_id,
            workspace_name,
            monitor_index,
            at,
            size,
            floating,
        });
    }
    Ok(out)
}

/// Parses `hyprctl -j monitors` output. Errors on anything that is not a JSON
/// array at the top level.
pub fn parse_monitors(raw: &str) -> Result<Vec<Monitor>> {
    let value: serde_json::Value = serde_json::from_str(raw)
        .map_err(|e| anyhow!("hyprctl monitors did not print valid JSON: {e}"))?;
    let arr = value.as_array().ok_or_else(|| {
        anyhow!(
            "hyprctl monitors printed {}, not an array",
            describe(&value)
        )
    })?;

    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        let name = item
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let description = item
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let make = item
            .get("make")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let model = item
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let serial = item
            .get("serial")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let x = item.get("x").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
        let y = item.get("y").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
        let width = item.get("width").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
        let height = item.get("height").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
        let scale = item.get("scale").and_then(|v| v.as_f64()).unwrap_or(1.0) as f32;
        let transform = item.get("transform").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
        let focused = item
            .get("focused")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        out.push(Monitor {
            id: item.get("id").and_then(|v| v.as_i64()).unwrap_or(-1),
            name,
            description,
            make,
            model,
            serial,
            x,
            y,
            width,
            height,
            scale,
            transform,
            focused,
        });
    }
    Ok(out)
}

/// A short, human-readable name for a JSON value's shape, for error messages.
fn describe(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Object(_) => "an object",
        serde_json::Value::Null => "null",
        serde_json::Value::String(_) => "a string",
        serde_json::Value::Bool(_) => "a bool",
        serde_json::Value::Number(_) => "a number",
        serde_json::Value::Array(_) => "an array",
    }
}

/// The monitor a client's `monitor` index refers to.
///
/// Exists because a client reports only a number: the connector name,
/// description and serial that placement matches on are in the monitor list
/// and nowhere else. Returns `None` for an index no monitor claims, which is
/// the honest answer when a monitor was unplugged between capture and now.
pub fn connector_of(index: i64, monitors: &[Monitor]) -> Option<&Monitor> {
    monitors.iter().find(|m| m.id == index)
}
