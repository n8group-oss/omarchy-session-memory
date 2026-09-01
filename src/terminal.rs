//! Choosing a terminal and spawning it attached to a tmux session.
//!
//! Every window osm spawns carries a marker unique to the restore attempt
//! that created it. Placement and cleanup act on that marker and nothing
//! else: a class-name prefix is not ownership, and acting on one would let
//! osm move or close a window the user opened themselves.

use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Ghostty,
    Alacritty,
    Kitty,
    Foot,
}

impl Kind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Kind::Ghostty => "ghostty",
            Kind::Alacritty => "alacritty",
            Kind::Kitty => "kitty",
            Kind::Foot => "foot",
        }
    }

    fn parse(s: &str) -> Option<Kind> {
        match s {
            "ghostty" => Some(Kind::Ghostty),
            "alacritty" => Some(Kind::Alacritty),
            "kitty" => Some(Kind::Kitty),
            "foot" => Some(Kind::Foot),
            _ => None,
        }
    }
}

/// Preference order when the configuration says `auto`.
const AUTO_ORDER: [Kind; 4] = [Kind::Ghostty, Kind::Alacritty, Kind::Kitty, Kind::Foot];

/// Whether `binary` is an executable file in one of the directories `path`
/// names, `path` being a `PATH`-shaped value rather than this process's.
///
/// Taken as an argument so the search can be exercised against a directory
/// holding exactly one terminal. The alternative — a test setting `PATH` —
/// mutates state every other test in the same binary is reading, which is why
/// the fallback below went untested and therefore unimplemented.
fn on_path_in(binary: &str, path: Option<&std::ffi::OsStr>) -> bool {
    let Some(path) = path else {
        return false;
    };
    std::env::split_paths(path).any(|dir| {
        let candidate = dir.join(binary);
        candidate.is_file() && is_executable(&candidate)
    })
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// The terminal to spawn.
///
/// A named terminal is honoured whether or not it is installed — a missing
/// binary is a spawn failure the restore reports, not a silent substitution
/// of something the user did not ask for. Only `auto` searches.
pub fn detect(configured: &str) -> Option<Kind> {
    let path = std::env::var_os("PATH");
    detect_on(configured, path.as_deref())
}

/// [`detect`], searching `path` rather than this process's `PATH`.
pub fn detect_on(configured: &str, path: Option<&std::ffi::OsStr>) -> Option<Kind> {
    if configured == "auto" {
        return AUTO_ORDER
            .into_iter()
            .find(|k| on_path_in(k.as_str(), path));
    }
    Kind::parse(configured)
}

/// The terminal to open for a window that was captured in `captured_kind`.
///
/// An explicitly configured terminal wins outright. `restore.terminal` was
/// inert: placement asked only what the *snapshot* said, so a user whose
/// Ghostty had since been uninstalled could set `restore.terminal = "kitty"`,
/// watch every placement go on failing to spawn ghostty, and find the setting
/// they had used sitting there doing nothing.
///
/// `auto` is the only value that defers to the capture — which is what `auto`
/// means — and it defers to it only as far as the machine allows. A captured
/// class naming a terminal that is *not installed here* is no more an answer
/// than one naming a terminal this crate has never heard of: `auto` is a
/// promise about what is on this machine, and taking the captured name on
/// trust meant a user whose Ghostty had been replaced by Kitty got a spawn
/// failure and no window instead of the fallback the README describes.
pub fn choose(configured: &str, captured_kind: &str) -> Option<Kind> {
    let path = std::env::var_os("PATH");
    choose_on(configured, captured_kind, path.as_deref())
}

/// [`choose`], against the `PATH` given rather than this process's.
pub fn choose_on(
    configured: &str,
    captured_kind: &str,
    path: Option<&std::ffi::OsStr>,
) -> Option<Kind> {
    if configured != "auto" {
        return detect_on(configured, path);
    }
    Kind::parse(captured_kind)
        .filter(|k| on_path_in(k.as_str(), path))
        .or_else(|| detect_on("auto", path))
}

/// Shell-quote for a single-quoted context.
///
/// A session name may contain a space or an apostrophe; Plan 1 shipped a hook
/// that broke on exactly that.
fn sq(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The command that opens a terminal attached to `session` **on `socket`**,
/// marked as owned by this restore attempt.
///
/// # The socket is not optional decoration
///
/// A bare `tmux attach-session -t dev` talks to the *default* server. During
/// a restore against `-L osm-test` that is either a server with no such
/// session — the terminal opens and exits — or, far worse, the developer's
/// real server carrying a session that happens to share the name, which is
/// exactly how a test once put a window on the maintainer's desktop attached
/// to work osm had nothing to do with. `None` means the restore really is
/// working against the default server, which is the only case where omitting
/// `-L` is the truth.
///
/// `-u` for the same reason Plan 3 established it on every tmux invocation:
/// without it tmux decides the terminal is not UTF-8 capable and mangles
/// every non-ASCII character in the session it attaches to.
pub fn spawn_argv(kind: Kind, session: &str, marker: &str, socket: Option<&str>) -> Vec<String> {
    let server = match socket {
        Some(name) => format!(" -L {}", sq(name)),
        None => String::new(),
    };
    let attach = format!("exec tmux -u{server} attach-session -t {}", sq(session));
    let mut argv: Vec<String> = vec![kind.as_str().to_string()];

    match kind {
        Kind::Ghostty => {
            // Without this, a running Ghostty swallows the request and no new
            // window appears at all — observed during a manual recovery.
            argv.push("--gtk-single-instance=false".to_string());
            argv.push(format!("--class={marker}"));
            argv.push("-e".to_string());
        }
        Kind::Alacritty => {
            argv.push("--class".to_string());
            argv.push(marker.to_string());
            argv.push("-e".to_string());
        }
        Kind::Kitty => {
            argv.push("--class".to_string());
            argv.push(marker.to_string());
        }
        Kind::Foot => {
            argv.push(format!("--app-id={marker}"));
        }
    }

    argv.push("bash".to_string());
    argv.push("-lc".to_string());
    argv.push(attach);
    argv
}

/// The marker identifying windows this attempt owns.
pub fn marker_for(attempt_id: i64) -> String {
    format!("osm-restore-{attempt_id}")
}
