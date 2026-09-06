use crate::lock::SingleInstance;
use crate::tmux::Tmux;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::Duration;

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

/// The lock every hook mutation takes, keyed to the **tmux server** whose
/// hooks it protects.
///
/// Hooks live in the tmux server, not on disk, so nothing about the lock file
/// is the state being protected — it is only a name two processes can agree
/// on. The question is therefore *which server*, and the answer has to be
/// derived from the server.
///
/// # Why not the osm state directory
///
/// It was `$XDG_STATE_HOME/osm/hooks.lock`, on the reasoning that every
/// engine mutating hooks is the same user with the same environment. That is
/// not so, and it is not so in the ordinary case. `XDG_STATE_HOME` names
/// where osm keeps *its* data; it has nothing to do with which tmux server a
/// process is talking to, and the two are set independently. A daemon on the
/// normal state directory and a `XDG_STATE_HOME=/tmp/scratch osm
/// install-hooks` against the same server took two different locks and
/// reproduced the unlocked remove-then-append race in full — both reporting
/// `installed: 15`, the server left carrying thirty hooks, and every tmux
/// event firing two captures from then on. The race tests could not see it
/// because they gave every process the same state directory.
///
/// So the name is derived from the socket this handle addresses — the one
/// thing every process talking to a given server necessarily agrees on — and
/// it lives in a per-user runtime directory of osm's own, computed from the
/// uid and from nothing a caller can point somewhere else.
///
/// A lock file left behind after a crash is harmless. `flock` ownership lives
/// in the open file description and dies with the process holding it, so a
/// stale file is an empty file nobody holds.
pub fn lock_path(tmux: &Tmux) -> Result<PathBuf> {
    Ok(lock_dir()?.join(format!("hooks-{}.lock", socket_key(tmux))))
}

/// This process's real user id, read from the kernel rather than from a
/// crate or from the environment.
///
/// `/proc/self` is owned by the user the process runs as. `HOME`, `USER` and
/// the rest are all settable by whoever starts the process, and a lock whose
/// directory a caller can move is the defect this module is closing.
fn uid() -> Result<u32> {
    use std::os::unix::fs::MetadataExt as _;
    Ok(std::fs::metadata("/proc/self")
        .context("read /proc/self to find this process's user id")?
        .uid())
}

/// The socket this handle addresses, as one filename component: the `-L` name
/// it was built with, or `default` for the server with no name.
///
/// The name and not a resolved socket path. tmux's own socket directory can
/// in principle be moved by an environment variable, and this project does
/// not read it anywhere — it is ignored outright by the tmux on the
/// maintainer's machine, so a path computed from it would be a guess about
/// where the socket is rather than a fact. The consequence is bounded and it
/// is bounded in the safe direction: two servers that somehow share a `-L`
/// name in different directories would share this lock, which costs one of
/// them a wait. Nothing is ever *under*-locked, which is the failure this
/// exists to prevent.
fn socket_key(tmux: &Tmux) -> String {
    as_one_filename(tmux.socket().unwrap_or("default"))
}

/// osm's per-user runtime directory, created `0700` if it is not there.
///
/// `/run/user/<uid>` when the system provides one — per-user, private, and
/// cleared when the user logs out, which is the right lifetime for a lock
/// about a running server — and `/tmp/osm-<uid>` otherwise, for a container
/// or a machine with no logind.
///
/// Neither is read from the environment. `XDG_RUNTIME_DIR` would be the
/// idiomatic source and is deliberately not used: an overridable directory is
/// exactly the defect this replaces, and swapping one environment variable
/// for another would reproduce it under a different name.
///
/// `/tmp` is world-writable, so the directory is checked after it is created:
/// a symlink, or a directory belonging to somebody else, is refused rather
/// than locked in.
fn lock_dir() -> Result<PathBuf> {
    use std::os::unix::fs::MetadataExt as _;
    let uid = uid()?;
    let run = PathBuf::from(format!("/run/user/{uid}"));
    let dir = if run.is_dir() {
        run.join("osm")
    } else {
        PathBuf::from(format!("/tmp/osm-{uid}"))
    };
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let meta = std::fs::symlink_metadata(&dir)
        .with_context(|| format!("read {} back after creating it", dir.display()))?;
    if !meta.file_type().is_dir() {
        anyhow::bail!(
            "{} is not a directory; osm keeps the tmux hook lock there and will \
             not follow whatever it points at",
            dir.display()
        );
    }
    if meta.uid() != uid {
        anyhow::bail!(
            "{} belongs to uid {}, not to this user ({uid}); osm will not take the \
             tmux hook lock inside somebody else's directory",
            dir.display(),
            meta.uid()
        );
    }
    std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o700))
        .with_context(|| format!("restrict {} to this user", dir.display()))?;
    Ok(dir)
}

/// `name` as a single filename component.
///
/// A `-L` name is the user's own text and may contain a `/` or worse. Every
/// byte that is not plainly a filename character becomes `%XX`, so two
/// different socket names can never collide on one lock file — and cannot
/// escape the runtime directory either — while the name stays legible to
/// whoever finds it there.
fn as_one_filename(name: &str) -> String {
    let mut out = String::new();
    for b in name.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-' | b'_' => out.push(*b as char),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// How long a hook mutation waits for another one to finish.
///
/// A registration is fifteen `set-hook` round trips plus however many
/// `show-hooks` reads the removal needs — tens of milliseconds. This is
/// generous by two orders of magnitude on purpose: the cost of waiting is a
/// pause, and the cost of giving up early is the duplicate-hook state this
/// lock exists to prevent.
const LOCK_WAIT: Duration = Duration::from_secs(10);

/// Hold the hook lock for the duration of one mutation.
fn hold(lock: &Path) -> Result<SingleInstance> {
    SingleInstance::acquire_blocking(lock, LOCK_WAIT)?.with_context(|| {
        format!(
            "another osm is changing the tmux hooks; the lock at {} was still \
             held after {}s",
            lock.display(),
            LOCK_WAIT.as_secs()
        )
    })
}

/// Install one marked hook per event, removing any previous marked entry for
/// that event first so repeated installs never accumulate.
///
/// # Why this takes a lock
///
/// Removing and appending are two steps, and between them the server has no
/// marked hooks at all. Unlocked, two engines each read that state and each
/// appended their own fifteen: twelve concurrent `osm install-hooks` against
/// one server left **180** marked hooks, each of them reporting `installed:
/// 15`. Every duplicate then fires its own `osm snapshot --debounced` on
/// every tmux event, and the debounce check runs before the capture lock, so
/// they do not collapse into one — a single `select-pane` became twelve
/// captures.
///
/// It is not a rare interleaving. `osm.service` re-registers whenever it sees
/// a tmux server it has not hooked (`ensure_hooks`, every five seconds) and
/// `osm install` registers whenever a user runs it; a user installing while
/// the daemon is running is the documented way to install.
///
/// See `tests/hook_races.rs`.
pub fn install(tmux: &Tmux, osm_bin: &str) -> Result<usize> {
    let _guard = hold(&lock_path(tmux)?)?;
    remove_marked(tmux)?;
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
    let _guard = hold(&lock_path(tmux)?)?;
    remove_marked(tmux)
}

/// The removal itself, with the lock already held.
///
/// Separate from [`uninstall`] because [`install`] does this as its first
/// step and already holds the lock: taking it again would open a second file
/// description on the same file, and `flock` would refuse it — a deadlock in
/// everything but name.
fn remove_marked(tmux: &Tmux) -> Result<usize> {
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
