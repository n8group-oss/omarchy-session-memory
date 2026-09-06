//! Placing and removing everything the marketplace plugin cannot.
//!
//! `omarchy plugin add` copies QML. It does not install a binary, it does not
//! install a systemd unit, and it does not register a tmux hook — so the
//! engine has to place those itself, and, far more importantly, has to be
//! able to take them away again. An uninstall that leaves the daemon enabled
//! and the hooks firing at a binary that no longer exists is worse than no
//! uninstall at all.
//!
//! # Nothing here starts a process
//!
//! Not `systemctl`, not `tmux`, not a shell. That is a deliberate boundary,
//! not an oversight: this module is exercised by `tests/install.rs`, and a
//! test that could enable a user unit or set a hook on the default tmux
//! server would be operating on the developer's live machine — this project
//! has already put a window on someone's desktop that way once.
//!
//! So the split is: this module owns everything **under the prefix** — copying
//! the binary, writing and removing the unit files — and reports only what it
//! did itself. The two steps it cannot own, registering the tmux hooks and
//! enabling the units, are performed and reported by `main`, through the same
//! [`crate::hooks`] seam and the same resolved `--socket` tmux every other
//! subcommand uses, and only on a run that is not a dry run.
//!
//! Deleting the user's snapshot database is a third thing this module will
//! not do on its own initiative: [`uninstall`] states the decision,
//! [`uninstall_database`] carries it out, and it is handed the path
//! explicitly rather than going looking for one — while holding the same
//! locks a capture and a restore hold, because unlinking a database somebody
//! is still writing to succeeds and takes their work with it.
//!
//! # Everything is inspectable first
//!
//! Both entry points take `dry_run`. On a dry run they touch nothing and
//! return the same list of lines they would otherwise have acted on, phrased
//! in the conditional — so what the user reads before is what happens after.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// The `ExecStart` path in the shipped unit files, replaced at install time
/// with the binary this install actually placed.
///
/// It is a real, correct path rather than a placeholder token so the units in
/// `systemd/` stay usable by hand — `cp systemd/osm*.service
/// ~/.config/systemd/user/` still works for a default-prefix install.
/// `tests/install.rs` asserts the substitution left no `%h/` behind, so
/// renaming this without renaming it in the units fails loudly instead of
/// installing units that start nothing.
const UNIT_BIN: &str = "%h/.local/bin/osm";

/// The unit files, by their installed name.
///
/// Compiled in rather than read from disk: an installed `osm` has no source
/// tree to read them out of.
const UNITS: [(&str, &str); 2] = [
    ("osm.service", include_str!("../systemd/osm.service")),
    (
        "osm-restore.service",
        include_str!("../systemd/osm-restore.service"),
    ),
];

/// Whether the units this install writes land somewhere systemd already
/// looks.
///
/// Decided once, when the plan is made, and consulted by the dry run and the
/// real run alike — which is the whole point of it being in the plan. The two
/// used to decide separately, so `--dry-run` promised "would enable
/// osm-restore.service and osm.service" for *every* prefix while the real run
/// with a custom prefix printed "systemd not touched": the output a cautious
/// user reads before committing was the one that was wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Systemd {
    /// `<prefix>/share/systemd/user` is one of the directories systemd's user
    /// manager actually loads units from, so they can be enabled and disabled
    /// by name.
    Manage,
    /// A prefix of the user's own choosing. The units are not on systemd's
    /// user unit search path, so enabling them by name would either fail or —
    /// worse — act on a *different* unit of the same name that some earlier
    /// install left in the real search path.
    NotOnSearchPath,
}

/// What an install would place, under one prefix.
#[derive(Debug, Clone)]
pub struct Plan {
    /// The prefix, normalised: absolute, with `.` and `..` folded away and
    /// every existing component resolved through its symlinks. `plan` is the
    /// only place this happens, so every later decision — where the files go,
    /// and whether systemd is touched — is made about the same path.
    pub prefix: PathBuf,
    /// Where the engine binary goes: `<prefix>/bin/osm`.
    pub binary: PathBuf,
    /// The systemd user units, in `<prefix>/share/systemd/user`.
    ///
    /// That directory rather than `~/.config/systemd/user` because it is a
    /// unit search path systemd already reads (`$XDG_DATA_HOME/systemd/user`)
    /// *and* it sits under the prefix — so an install into a temporary
    /// directory writes nothing outside it, which is what makes this
    /// testable at all.
    pub units: Vec<PathBuf>,
    /// Whether the install registers tmux hooks. Always true today; it is a
    /// field so a caller can render the plan without assuming.
    pub hooks: bool,
    /// Whether the units land on systemd's user unit search path.
    pub systemd: Systemd,
}

impl Plan {
    /// The line to print in place of running `systemctl --user <args>`,
    /// printed by the dry run and the real run alike.
    pub fn systemd_skipped(&self, args: &[&str]) -> String {
        format!(
            "systemd not touched: {} is not on systemd's user unit search path; \
             run `systemctl --user {}` yourself if that is what you meant",
            unit_dir(&self.prefix).display(),
            args.join(" ")
        )
    }
}

/// The prefix an install uses when the user names none: `$HOME/.local`.
pub fn default_prefix() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".local"))
}

/// How many symlinks one path may traverse before it is called a loop.
///
/// The kernel's own limit is 40 (`MAXSYMLINKS`), and matching it is the point:
/// a path this refuses is a path `open(2)` would refuse with `ELOOP`.
const MAX_SYMLINKS: usize = 40;

/// Resolve `path` to the one path every later decision is made about.
///
/// Made absolute, then resolved **left to right** the way the kernel resolves
/// it: each component is appended to what has been resolved so far, a symlink
/// is followed at the moment it is reached, and `..` steps out of wherever
/// that leaves us. A component that does not exist is appended and resolution
/// continues, which is the one place this deliberately differs from
/// `canonicalize` — a prefix that has not been created yet is the ordinary
/// case the first time, and `canonicalize` refuses it outright.
///
/// # Why the order matters
///
/// It used to fold `.` and `..` away lexically *first* and resolve symlinks
/// afterwards. That is not what a path means. On this developer's machine
/// `/bin` is a symlink to `usr/bin`, so `/bin/../tmp` is `/usr/tmp` to the
/// kernel — it steps out of the link's *target* — and was `/tmp` to the
/// folder. `/tmp` is a real directory full of other people's files, and this
/// function's answer is what `osm install` writes into and what `osm
/// uninstall` deletes from. A prefix a user typed could name one directory
/// and operate on another.
///
/// # Why not `canonicalize` alone
///
/// It fails on any path whose last components do not exist yet, which is most
/// prefixes the first time they are used. The tail is carried lexically
/// instead; every component that *does* exist is resolved through the
/// filesystem.
pub fn normalize(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .context("resolve a relative prefix against the working directory")?
            .join(path)
    };

    // The root the resolution starts at, and the one an *absolute* symlink
    // target restarts at.
    let mut root = PathBuf::new();
    // The components still to resolve, reversed so `pop` takes the next.
    let mut pending: Vec<std::ffi::OsString> = Vec::new();
    for component in absolute.components() {
        match component {
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                root.push(component.as_os_str())
            }
            std::path::Component::CurDir => {}
            other => pending.push(other.as_os_str().to_os_string()),
        }
    }
    pending.reverse();

    let mut out = root.clone();
    let mut budget = MAX_SYMLINKS;
    while let Some(name) = pending.pop() {
        if name == ".." {
            // At the root this is a no-op, which is what the kernel does with
            // it too. Everywhere else `out` holds a fully resolved path, so
            // stepping out of it is stepping out of the link's target.
            if out != root {
                out.pop();
            }
            continue;
        }
        if name == "." {
            continue;
        }
        let candidate = out.join(&name);
        // `symlink_metadata` does not follow the link, which is the whole
        // point: the target has to go back through this loop so its own `..`
        // and its own symlinks are resolved in order.
        //
        // Its *failure* used to be swallowed by `unwrap_or(false)` — read as
        // "not a symlink", which was then read as "append it and walk on".
        // Three different answers were collapsed into one, and only one of
        // them meant what the code did next.
        let meta = match std::fs::symlink_metadata(&candidate) {
            Ok(m) => Some(m),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            // Anything else — a directory this user may not search, a name
            // the filesystem will not take — says nothing whatever about
            // what is there. Guessing "nothing" made a path osm could not
            // read resolve to one it could.
            Err(e) => {
                return Err(e).with_context(|| {
                    format!(
                        "read {} while resolving the prefix {}",
                        candidate.display(),
                        path.display()
                    )
                })
            }
        };
        let is_link = meta.as_ref().is_some_and(|m| m.file_type().is_symlink());
        if let Some(meta) = &meta {
            if !is_link {
                // Exists and is not a link. Nothing lives under something
                // that is not a directory: the kernel answers `ENOTDIR`, and
                // this walked straight through it. `/etc/passwd/../../tmp/x`
                // folded to `/tmp/x` — a real directory, full of other
                // people's files, that the user never named and that a later
                // `osm uninstall` given the same argument would remove from.
                if !meta.is_dir() && !pending.is_empty() {
                    anyhow::bail!(
                        "{} is not a directory, so nothing can live under it; \
                         the prefix {} does not name a place osm can install into",
                        candidate.display(),
                        path.display()
                    );
                }
                out = candidate;
                continue;
            }
        } else {
            // Does not exist. Appending it is deliberate and is the ordinary
            // first-install case — `canonicalize` refuses such a path
            // outright, which is why this function exists at all.
            //
            // But only when nothing after it walks back out again. A `..`
            // following a component that is not there has nothing to step out
            // of; the kernel answers `ENOENT`, while folding it lexically
            // resolved a prefix naming a directory that does not exist into a
            // different one that does.
            if pending.iter().any(|c| c == "..") {
                anyhow::bail!(
                    "{} does not exist, and the prefix {} steps back out of it; \
                     that path names nothing",
                    candidate.display(),
                    path.display()
                );
            }
            out = candidate;
            continue;
        }
        if budget == 0 {
            anyhow::bail!(
                "too many symlinks resolving {}: {} is part of a symlink loop",
                path.display(),
                candidate.display()
            );
        }
        budget -= 1;
        let target = std::fs::read_link(&candidate)
            .with_context(|| format!("read the symlink {}", candidate.display()))?;
        if target.is_absolute() {
            out = root.clone();
        }
        let mut extra: Vec<std::ffi::OsString> = Vec::new();
        for component in target.components() {
            match component {
                std::path::Component::RootDir | std::path::Component::Prefix(_) => {}
                std::path::Component::CurDir => {}
                other => extra.push(other.as_os_str().to_os_string()),
            }
        }
        for component in extra.into_iter().rev() {
            pending.push(component);
        }
    }
    Ok(out)
}

/// The directories systemd's **user** manager loads unit files from,
/// computed from this process's environment.
///
/// The list and its order are `systemd.unit(5)`'s user unit load path. It is
/// what decides whether writing a unit into a directory means anything at
/// all.
///
/// # This is the fallback, not the answer
///
/// It describes the load path a manager started from *this* environment would
/// have. The manager that will actually load the units is a different process
/// with an environment of its own, set when the user logged in, and the two
/// need not agree: `XDG_DATA_HOME=/tmp/data osm install` changes only the
/// child's environment, while the running manager goes on searching
/// `~/.local/share/systemd/user`. A real install asks the manager instead —
/// see [`plan_with_unit_path`] — and falls back to this when there is no
/// manager to ask.
///
/// A dry run always uses this one. `osm install --dry-run` has to give the
/// same answer on a machine with no user manager running (a container, a
/// headless box, a live ISO) as on one with, and a dry run whose answer
/// depends on whether systemd happens to be up is not an inspection.
fn user_unit_search_paths() -> Vec<PathBuf> {
    // `$SYSTEMD_UNIT_PATH` replaces the load path outright, unless it ends in
    // an empty component — a trailing `:` — in which case the usual path is
    // appended to it (`systemd.unit(5)`). Its entries are unit directories as
    // they stand, not XDG roots with `systemd/user` appended. Ignoring it
    // made this function's answer disagree with the manager's on every
    // machine that sets it.
    if let Ok(raw) = std::env::var("SYSTEMD_UNIT_PATH") {
        if !raw.is_empty() {
            let mut out: Vec<PathBuf> = raw
                .split(':')
                .filter(|e| !e.is_empty())
                .map(PathBuf::from)
                .collect();
            if raw.ends_with(':') {
                out.extend(default_user_unit_search_paths());
            }
            return out;
        }
    }
    default_user_unit_search_paths()
}

/// The user unit load path with no `$SYSTEMD_UNIT_PATH` override applied.
fn default_user_unit_search_paths() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let dir = |var: &str, fallback: Option<PathBuf>| -> Option<PathBuf> {
        match std::env::var_os(var) {
            Some(v) if !v.is_empty() => Some(PathBuf::from(v)),
            _ => fallback,
        }
    };

    let mut out: Vec<PathBuf> = Vec::new();
    let mut push = |p: Option<PathBuf>| {
        if let Some(p) = p {
            out.push(p.join("systemd/user"));
        }
    };

    push(dir(
        "XDG_CONFIG_HOME",
        home.as_ref().map(|h| h.join(".config")),
    ));
    push(Some(PathBuf::from("/etc")));
    push(dir("XDG_RUNTIME_DIR", None));
    push(Some(PathBuf::from("/run")));
    push(dir(
        "XDG_DATA_HOME",
        home.as_ref().map(|h| h.join(".local/share")),
    ));
    let data_dirs = match std::env::var("XDG_DATA_DIRS") {
        Ok(v) if !v.is_empty() => v,
        _ => "/usr/local/share:/usr/share".to_string(),
    };
    for entry in data_dirs.split(':').filter(|e| !e.is_empty()) {
        push(Some(PathBuf::from(entry)));
    }
    push(Some(PathBuf::from("/usr/local/lib")));
    push(Some(PathBuf::from("/usr/lib")));
    out
}

/// Where everything lands under `prefix`, and what that means for systemd.
///
/// The prefix is normalised here, once. Every caller works from the plan
/// afterwards rather than from the path it was handed, so the files and the
/// systemd decision can never be about two different directories.
pub fn plan(prefix: &Path) -> Result<Plan> {
    plan_with_unit_path(prefix, None)
}

/// The paths in a `systemctl --user show -p UnitPath --value` reply, or
/// `None` when it named none.
///
/// systemd renders a string list space-separated, which is how it is read
/// back. An empty reply is `None` rather than an empty list: "the manager
/// said nothing" is not "the manager loads units from nowhere", and only the
/// first of those may fall back to the environment calculation.
pub fn parse_unit_path(raw: &str) -> Option<Vec<PathBuf>> {
    let paths: Vec<PathBuf> = raw.split_whitespace().map(PathBuf::from).collect();
    (!paths.is_empty()).then_some(paths)
}

/// [`plan`], deciding the systemd question against the **running manager's**
/// own `UnitPath` when one was obtained.
///
/// # Why the manager is asked at all
///
/// The load path is a documented function of the environment, and this module
/// computed it from the environment — of the wrong process. `osm install`
/// runs with whatever the user's shell exports; the manager that would load
/// the units was started at login and keeps its own. Set `XDG_DATA_HOME` for
/// one command and the two disagree: the units land in
/// `~/.local/share/systemd/user`, which the manager *does* read, while this
/// code concluded they had landed somewhere systemd never looks, skipped
/// `enable`, and exited 0. The user is told the install succeeded and nothing
/// starts at the next boot.
///
/// `None` — no manager to ask, or a reply that named nothing — keeps the
/// documented environment calculation, which is also what every dry run uses.
/// Asking systemd is `main`'s job: nothing in this module starts a process.
pub fn plan_with_unit_path(prefix: &Path, unit_path: Option<&[PathBuf]>) -> Result<Plan> {
    let prefix = normalize(prefix)?;
    let unit_dir = unit_dir(&prefix);
    // The question is about the directory the units actually land in, not
    // about the prefix. It used to be "is this prefix the default one?",
    // which assumed `$HOME/.local/share` is where systemd looks — true only
    // while `XDG_DATA_HOME` is unset or points there. With
    // `XDG_DATA_HOME=/tmp/data`, an install into the default prefix wrote
    // units to `$HOME/.local/share/systemd/user`, which systemd does not
    // read, and then announced and ran `enable --now` anyway. That command
    // does not fail for want of a unit file here: it acts on whatever unit of
    // that name it can find on the real search path, which on a machine that
    // has installed osm before is an older copy somewhere else.
    let resolved_unit_dir = normalize(&unit_dir)?;
    let search_paths: Vec<PathBuf> = match unit_path {
        Some(live) => live.to_vec(),
        None => user_unit_search_paths(),
    };
    let on_search_path = search_paths
        .iter()
        .filter_map(|p| normalize(p).ok())
        .any(|p| p == resolved_unit_dir);
    let systemd = if on_search_path {
        Systemd::Manage
    } else {
        Systemd::NotOnSearchPath
    };

    Ok(Plan {
        binary: prefix.join("bin/osm"),
        units: UNITS.iter().map(|(name, _)| unit_dir.join(name)).collect(),
        hooks: true,
        systemd,
        prefix,
    })
}

fn unit_dir(prefix: &Path) -> PathBuf {
    prefix.join("share/systemd/user")
}

/// Place the binary and the unit files, or say what that would do.
///
/// The binary copied is the running one ([`std::env::current_exe`]): the
/// thing being installed is the engine the user just ran, not whatever
/// happens to be on `PATH`.
///
/// Does **not** register tmux hooks or touch systemd — see the module docs —
/// and does not report them either. Every line returned here is a claim about
/// something this call did, so a step it does not own cannot appear as a step
/// it took; `main` prints those two in the same run.
pub fn install(plan: &Plan, dry_run: bool) -> Result<Vec<String>> {
    let mut lines = Vec::new();
    let source = std::env::current_exe().context("locate the running osm binary")?;
    let verb = if dry_run {
        "would install"
    } else {
        "installed"
    };

    lines.push(format!(
        "{verb} {} (from {})",
        plan.binary.display(),
        source.display()
    ));
    if !dry_run {
        copy_binary(&source, &plan.binary)?;
    }

    for (path, (_, text)) in plan.units.iter().zip(UNITS.iter()) {
        lines.push(format!(
            "{} {}",
            if dry_run { "would write" } else { "wrote" },
            path.display()
        ));
        if !dry_run {
            let body = text.replace(UNIT_BIN, &plan.binary.display().to_string());
            write_new(path, body.as_bytes())?;
        }
    }

    Ok(lines)
}

/// Remove what [`install`] placed under `prefix`, or say what that would
/// remove.
///
/// `keep_database` only decides what the returned lines *say*. Deleting the
/// user's snapshots is [`uninstall_database`]'s job and nothing else's,
/// precisely so that no call to this function — from a test or from
/// anywhere — can reach a database it was not handed the path to.
///
/// Removing something that is not there is not an error and is not reported
/// as a removal: an uninstall run twice must not claim to have deleted the
/// same file twice.
pub fn uninstall(plan: &Plan, keep_database: bool, dry_run: bool) -> Result<Vec<String>> {
    let mut lines = Vec::new();

    for path in plan.units.iter().chain(std::iter::once(&plan.binary)) {
        if !path.exists() {
            lines.push(format!("not present: {}", path.display()));
            continue;
        }
        if dry_run {
            lines.push(format!("would remove {}", path.display()));
        } else {
            std::fs::remove_file(path).with_context(|| format!("remove {}", path.display()))?;
            lines.push(format!("removed {}", path.display()));
        }
    }

    lines.push(if keep_database {
        "keeping the snapshot database; pass --remove-database to delete it".to_string()
    } else {
        format!(
            "{} the snapshot database",
            if dry_run { "would remove" } else { "removing" }
        )
    });
    Ok(lines)
}

/// How long to wait for a capture or a restore to let go of the engine's
/// locks before giving up on removing the database.
///
/// Bounded, and short: the answer to "somebody is using it" is to say so and
/// exit non-zero, not to block an uninstall behind a restore that may run for
/// half a minute. Rerunning the command is cheap; deleting a database out
/// from under a running capture is not.
const DB_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// The files a SQLite database in WAL mode is made of, besides the database.
///
/// All three go together or none of them do. A `state.db` removed on its own
/// leaves `state.db-wal` beside it, and the next `osm` to create a fresh
/// database there finds a write-ahead log belonging to one that no longer
/// exists.
const DB_SIDECARS: [&str; 3] = ["-wal", "-shm", "-journal"];

/// Delete the snapshot database, but only when `remove` says so.
///
/// The snapshots are the user's own record of where they were working.
/// Removing the tool is not a statement about that record, so the default
/// everywhere is to keep it, and this is the one function in the crate that
/// can delete it — handed an explicit path, never one it went looking for.
///
/// # Why it takes the engine's locks
///
/// This used to unlink the file with no lock held at all. A capture running
/// at that moment carries on writing into the deleted inode — its work
/// disappears when the last descriptor closes — or finishes and recreates
/// `state.db`, leaving the user with a database they asked to have deleted
/// and none of the snapshots that were in it. Neither is reported, because
/// unlinking an open file succeeds.
///
/// So it takes the same two locks a capture takes, in the same order
/// (restore, then capture), and the restore lock **exclusively** — which is
/// how a restore holds it, and which therefore excludes captures both ways.
/// If either is still held after [`DB_LOCK_WAIT`], nothing is deleted and the
/// caller is told; that is a real answer, and the user can stop the daemon
/// and run it again.
///
/// A database that is already gone is a success: the postcondition asked for
/// is "it is not there". The sidecars are still swept in that case, because
/// that is exactly the state a previous half-removal leaves behind.
pub fn uninstall_database(db: &Path, restore_lock: &Path, remove: bool) -> Result<Vec<String>> {
    if !remove {
        return Ok(Vec::new());
    }
    let sidecars: Vec<PathBuf> = DB_SIDECARS
        .iter()
        .map(|suffix| sidecar(db, suffix))
        .collect();
    if !db.exists() && !sidecars.iter().any(|p| p.exists()) {
        return Ok(Vec::new());
    }

    let _restore = crate::lock::SingleInstance::acquire_blocking(restore_lock, DB_LOCK_WAIT)?
        .with_context(|| {
            format!(
                "a capture or a restore still holds {} after {}s, so the snapshot database \
                 was left alone; stop osm.service and run this again",
                restore_lock.display(),
                DB_LOCK_WAIT.as_secs()
            )
        })?;
    let capture_lock = crate::capture::capture_lock_path(restore_lock);
    let _capture = crate::lock::SingleInstance::acquire_blocking(&capture_lock, DB_LOCK_WAIT)?
        .with_context(|| {
            format!(
                "a capture still holds {} after {}s, so the snapshot database was left \
                 alone; stop osm.service and run this again",
                capture_lock.display(),
                DB_LOCK_WAIT.as_secs()
            )
        })?;

    let mut lines = Vec::new();
    // Fold the write-ahead log back into the database before anything is
    // unlinked, so every intermediate state on disk is a complete database or
    // no database — never a database missing its most recent commits.
    if db.exists() {
        if let Err(e) = checkpoint(db) {
            lines.push(format!(
                "could not checkpoint {} before removing it ({e:#}); removing it anyway, \
                 as asked",
                db.display()
            ));
        }
    }
    // Sidecars first, database last, for the same reason: after the
    // checkpoint the sidecars hold nothing the database does not.
    for path in sidecars.iter().chain(std::iter::once(&db.to_path_buf())) {
        if !path.exists() {
            continue;
        }
        std::fs::remove_file(path).with_context(|| format!("remove {}", path.display()))?;
        lines.push(format!("removed {}", path.display()));
    }
    Ok(lines)
}

/// `state.db` plus a suffix, as SQLite names its sidecars — appended to the
/// file name, not joined as a path component.
fn sidecar(db: &Path, suffix: &str) -> PathBuf {
    let mut name = db.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// Fold the write-ahead log back into the database and truncate it.
///
/// Opened directly rather than through [`crate::db::open`]: that runs
/// migrations and can decide to preserve the file aside, which is the last
/// thing to do to a database that is about to be deleted.
fn checkpoint(db: &Path) -> Result<()> {
    let conn = rusqlite::Connection::open(db)
        .with_context(|| format!("open {} to checkpoint it", db.display()))?;
    conn.pragma_update(None, "wal_checkpoint", "TRUNCATE")
        .with_context(|| format!("checkpoint {}", db.display()))?;
    Ok(())
}

/// Copy the engine into place and make it executable.
///
/// Written to a sibling temporary file and renamed, so a copy interrupted
/// half way leaves the previous binary intact rather than a truncated one
/// that systemd will happily try to start. Removing the destination first is
/// also what makes replacing a *running* binary safe: the unlink leaves the
/// running process holding its old inode.
fn copy_binary(source: &Path, dest: &Path) -> Result<()> {
    let dir = dest
        .parent()
        .context("the install prefix has no bin directory")?;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let staged = dir.join(".osm.new");
    let _ = std::fs::remove_file(&staged);
    std::fs::copy(source, &staged)
        .with_context(|| format!("copy {} to {}", source.display(), staged.display()))?;
    let mut perm = std::fs::metadata(&staged)?.permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perm, 0o755);
    std::fs::set_permissions(&staged, perm)?;
    std::fs::rename(&staged, dest).with_context(|| format!("install {}", dest.display()))?;
    Ok(())
}

fn write_new(path: &Path, body: &[u8]) -> Result<()> {
    let dir = path.parent().context("a unit path with no directory")?;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    std::fs::write(path, body).with_context(|| format!("write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// If the shipped units stop carrying the path this module substitutes,
    /// every installed unit silently points at `%h/.local/bin/osm` regardless
    /// of the prefix — and an install into any other prefix produces units
    /// that start nothing.
    #[test]
    fn every_shipped_unit_names_the_path_the_install_substitutes() {
        for (name, text) in UNITS {
            assert!(
                text.contains(UNIT_BIN),
                "{name} does not contain {UNIT_BIN}:\n{text}"
            );
        }
    }
}
