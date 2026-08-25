use crate::tmux::Tmux;
use anyhow::Result;

/// Appears inside every hook command this engine installs, so uninstall can
/// identify its own entries without touching the user's hooks.
///
/// It lives *inside* the quoted shell command (not appended after it) so
/// tmux's `run-shell` parses the whole thing as one argument — the shell then
/// treats the trailing `#osm-hook` as a comment. Putting the marker outside
/// the quotes makes tmux treat it as a stray second argument to `run-shell`,
/// which fails hook installation outright.
pub const HOOK_MARKER: &str = "#osm-hook";

// NOTE: the brief's original list of events named `window-renamed`, but on
// real tmux (3.7c) `show-hooks -g` never lists that name even after it is
// set and firing — it is invisible to the parsing `uninstall` relies on, so
// a hook installed under that name could never be found or removed again.
// `after-rename-window` is the hook tmux actually surfaces in `show-hooks
// -g`, and it fires on the same rename action. Verified manually: both
// names fire on `rename-window`, but only `after-rename-window` appears in
// `show-hooks -g` output.
pub const HOOKED_EVENTS: [&str; 15] = [
    "session-created",
    "session-renamed",
    "session-closed",
    "window-linked",
    "window-unlinked",
    "after-rename-window",
    "after-split-window",
    "after-kill-pane",
    "after-select-pane",
    "after-select-window",
    "after-resize-pane",
    // A `select-layout` or a window resize changes the topology just as
    // surely as a split does, and neither was hooked: `select-layout tiled`
    // was lost outright to a reboot that beat the 120s fallback timer, while
    // the README promised capture "on every tmux change". Both fire and both
    // appear in `show-hooks -g` on tmux 3.3a and 3.7c — verified, since the
    // failure mode this project already hit is an event name tmux accepts
    // and then never lists.
    "after-select-layout",
    "after-resize-window",
    "client-attached",
    "client-detached",
];

/// Quote `s` for the `/bin/sh -c` string tmux's `run-shell` executes.
///
/// Double quotes, not single: the tmux token this ends up inside is itself
/// double-quoted, and tmux's *single* quotes are fully literal — they cannot
/// contain an apostrophe by any escape. Inside shell double quotes only `$`,
/// backtick, backslash and `"` are special, and an apostrophe needs no
/// escaping at all, which is exactly the case that used to break.
fn sh_double_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if matches!(c, '$' | '`' | '"' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// Escape `s` for one tmux double-quoted token.
///
/// tmux processes backslash escapes inside double quotes and treats `#` as
/// the start of a format expansion, so both have to be protected; `##` is
/// tmux's own escape for a literal `#`.
fn tmux_double_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' | '"' => {
                out.push('\\');
                out.push(c);
            }
            '#' => out.push_str("##"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The tmux command installed for `event`.
///
/// Two quoting layers, because there are two parsers: tmux parses this string
/// into `run-shell -b <arg>`, and `/bin/sh -c` then parses `<arg>`. The
/// executable path was previously interpolated raw into a tmux
/// *single*-quoted token, so an install path containing an apostrophe
/// registered without error and then executed a truncated path that does not
/// exist — the hook silently never captured anything. A space alone was
/// enough to split the command.
fn hook_command(osm_bin: &str, event: &str) -> String {
    let shell = format!(
        "{} snapshot --debounced --reason {event} >/dev/null 2>&1 {HOOK_MARKER}",
        sh_double_quote(osm_bin)
    );
    format!("run-shell -b {}", tmux_double_quote(&shell))
}

/// Install one marked hook per event, removing any previous marked entry for
/// that event first so repeated installs never accumulate.
pub fn install(tmux: &Tmux, osm_bin: &str) -> Result<usize> {
    uninstall(tmux)?;
    for event in HOOKED_EVENTS {
        let cmd = hook_command(osm_bin, event);
        tmux.run(&["set-hook", "-g", "-a", event, &cmd])?;
    }
    Ok(HOOKED_EVENTS.len())
}

/// Remove only hooks carrying `HOOK_MARKER`, preserving user-defined entries
/// on the same events.
///
/// `show-hooks -g` indices shift as entries are unset, so a stale listing
/// can no longer be trusted after the first removal. This re-reads the
/// listing before every removal and stops once no marked hook remains.
pub fn uninstall(tmux: &Tmux) -> Result<usize> {
    let mut removed = 0;
    loop {
        let shown = tmux.run(&["show-hooks", "-g"]).unwrap_or_default();
        let Some(line) = shown.lines().find(|l| l.contains(HOOK_MARKER)) else {
            break;
        };
        let Some(head) = line.split_whitespace().next() else {
            break;
        };
        let (event, index) = match head.split_once('[') {
            Some((e, rest)) => (e, rest.trim_end_matches(']')),
            None => (head, ""),
        };
        let target = if index.is_empty() {
            event.to_string()
        } else {
            format!("{event}[{index}]")
        };
        if tmux.run(&["set-hook", "-gu", &target]).is_err() {
            break;
        }
        removed += 1;
    }
    Ok(removed)
}
