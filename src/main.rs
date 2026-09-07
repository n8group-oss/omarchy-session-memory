use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "osm", version, about = "Omarchy session memory engine")]
struct Cli {
    /// Target a non-default tmux server, passed as `-L <NAME>` to every tmux
    /// invocation this process makes. Falls back to `OSM_TMUX_SOCKET` if
    /// unset. Tests and tooling must always set this (or the env var) rather
    /// than touch the developer's real tmux server — see README.
    ///
    /// Not wired through clap's `env` feature (avoids adding a dependency
    /// feature); the fallback is applied explicitly in `main`.
    #[arg(long, global = true)]
    socket: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Report engine health as JSON
    Status {
        /// Accepted for forward compatibility and ignored: `status` output is
        /// always JSON. Reserved for a future human-readable default.
        #[arg(long)]
        json: bool,
    },
    /// Capture the current tmux topology
    Snapshot {
        #[arg(long, default_value = "manual")]
        reason: String,
        /// Skip this capture if one already happened within
        /// capture.debounce_max_latency_secs of now
        #[arg(long)]
        debounced: bool,
    },
    /// Rebuild tmux topology from the newest snapshot of a previous boot
    Restore {
        #[arg(long)]
        dry_run: bool,
    },
    /// Install the engine: the binary, both systemd user units, and the
    /// tmux hooks. Inspect it first with `--dry-run`.
    Install {
        /// Where to install. Defaults to `$HOME/.local`, so the binary lands
        /// at `~/.local/bin/osm` and the units at
        /// `~/.local/share/systemd/user`.
        #[arg(long)]
        prefix: Option<PathBuf>,
        /// Print what would change and touch nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Remove the engine: stop and disable the units, remove them, remove the
    /// tmux hooks, and remove the binary. Keeps your snapshots.
    Uninstall {
        /// The prefix the engine was installed into. Defaults to
        /// `$HOME/.local`.
        #[arg(long)]
        prefix: Option<PathBuf>,
        /// Also delete the snapshot database. Off by default: the snapshots
        /// are your own record of where you were working, and removing the
        /// tool says nothing about whether you want to keep it.
        #[arg(long)]
        remove_database: bool,
        /// Print what would change and touch nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Install the engine's tmux hooks
    InstallHooks,
    /// Remove the engine's tmux hooks, preserving user hooks
    UninstallHooks,
    /// Run the capture daemon (fallback-interval loop)
    Daemon,
    /// Report every AI coding-agent conversation as JSON: the ones a pane is
    /// running right now, and the ones that are only resumable
    Agents {
        /// Accepted for forward compatibility and ignored: `agents` output
        /// is always JSON, exactly as `status` is.
        #[arg(long)]
        json: bool,
    },
    /// Resume one conversation into the current pane (`$TMUX_PANE`)
    Resume {
        /// The agent's own id for the conversation, as printed by
        /// `osm agents --json` under `native_id`.
        native_id: String,
    },
}

/// The tmux server to use when neither `--socket` nor `OSM_TMUX_SOCKET` is
/// set: the user's default server.
///
/// Only reachable in a build that enabled the `default-server` feature (the
/// default). Test runs use `--no-default-features`, where
/// `Tmux::default_server()` does not exist and this refuses instead — so no
/// code compiled into a test binary can address the real tmux server.
#[cfg(feature = "default-server")]
fn default_server_tmux() -> Result<osm::tmux::Tmux> {
    Ok(osm::tmux::Tmux::default_server())
}

#[cfg(not(feature = "default-server"))]
fn default_server_tmux() -> Result<osm::tmux::Tmux> {
    anyhow::bail!(
        "this osm was built without the `default-server` feature; \
         pass --socket <NAME> or set OSM_TMUX_SOCKET"
    )
}

/// How many captures in a row may fail before the daemon exits non-zero.
///
/// The unit is `Restart=on-failure`, so systemd records the failure, surfaces
/// it in `systemctl --user status osm.service`, and restarts — which is the
/// right answer for a transient cause and a visible one for a persistent
/// cause. Silently looping forever is what let a broken capture go unnoticed.
const MAX_CONSECUTIVE_CAPTURE_FAILURES: u32 = 3;

/// How often the daemon looks at which tmux server is on the socket, so it can
/// put its hooks back on a server that has just appeared or just replaced the
/// one it hooked.
///
/// A tmux hook is server state: it lives in the server process and dies with
/// it. Nothing on disk holds it, no unit re-applies it, and `osm install` can
/// only set it on the server that happened to be running at the time — so
/// after any tmux restart the engine's only remaining source of captures is
/// the fallback timer, two minutes wide by default, with `osm status`
/// reporting perfect health throughout.
///
/// Five seconds, and separate from `capture.fallback_interval_secs`, because
/// this is the window in which changes are unwatched. It costs one
/// `display-message` round trip per tick, which is what the identity read
/// already is everywhere else.
const HOOK_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

/// Put this engine's tmux hooks on the server that is running now, unless
/// they are already this daemon's doing on this very incarnation.
///
/// `hooked` is the incarnation the hooks were last registered on, and the
/// comparison against it is what keeps this from being a hook-repair loop:
/// a user who runs `osm uninstall-hooks` on a server that stays up keeps
/// their decision, because the incarnation has not changed. Only a *new*
/// server gets hooks it never had.
///
/// Nothing here is fatal. No tmux server is an ordinary state (it is what a
/// machine looks like before the first terminal opens), and a server that
/// cannot be identified is a problem the capture path reports far more
/// loudly than a hook registration could.
fn ensure_hooks(tmux: &osm::tmux::Tmux, osm_bin: &str, hooked: &mut Option<String>) {
    match tmux.running_server_incarnation() {
        // No server. Forget which one was hooked, so the next one to appear
        // is hooked even if it somehow reports the same identity.
        Ok(None) => *hooked = None,
        Ok(Some(incarnation)) => {
            if hooked.as_deref() == Some(incarnation.as_str()) {
                return;
            }
            match osm::hooks::install(tmux, osm_bin) {
                Ok(n) => {
                    eprintln!("osm: registered {n} tmux hook(s) on tmux server {incarnation}");
                    *hooked = Some(incarnation);
                }
                // Left unrecorded on purpose: the next tick tries again.
                Err(e) => eprintln!("osm: could not register the tmux hooks: {e:#}"),
            }
        }
        Err(e) => eprintln!("osm: could not identify the tmux server: {e:#}"),
    }
}

/// The configuration the agent subcommands read, falling back to the
/// defaults when it cannot be loaded.
///
/// A broken config must not make `osm agents` and `osm resume` unusable:
/// the field they need is which adapters are enabled, and the default set
/// is the right answer for a user who never wrote a config at all. The
/// problem is still said out loud on stderr, and `osm status --json`
/// reports it as a real problem — the same treatment `daemon` gives its
/// timings.
///
/// **Except privacy.** `osm::config::Config::strict_fallback` is the default
/// set with `privacy.prompt_titles` forced off: a config that would not load
/// is not consent, and the plain default reads prompts. A user who wrote
/// `prompt_titles = false` and misspelled an unrelated key had `osm agents`
/// go back to reading the first line of their messages.
fn agent_config() -> osm::config::Config {
    match osm::paths::config_path().and_then(|p| osm::config::load(&p)) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!(
                "osm: {e:#}; using the default agent configuration, and deriving no title \
                 from anyone's first prompt until the config loads"
            );
            osm::config::Config::strict_fallback()
        }
    }
}

/// What systemd's user manager says it loads units from, or `None` when there
/// is no manager to ask.
///
/// The install decides whether to enable the units by asking whether they
/// landed somewhere systemd looks. That was computed from *this* process's
/// environment, and the manager that would load them is a different process
/// with an environment of its own: `XDG_DATA_HOME=/tmp/data osm install`
/// changed only the child's, the manager went on searching
/// `~/.local/share/systemd/user` — where the units had in fact just been
/// written — and the install skipped `enable`, said so, and exited 0. Nothing
/// started at the next boot and nothing had gone wrong as far as the user
/// could see.
///
/// A **dry run never asks**. It has to give the same answer on a machine with
/// no user manager (a container, a headless box, a live ISO) as on one with,
/// and an inspection whose answer depends on whether systemd happens to be up
/// is not one. It keeps the documented environment calculation, which is also
/// the fallback here.
///
/// A manager that cannot be reached, exits non-zero, or names nothing is
/// `None`: the environment calculation is a description of the same rule, and
/// it is the best available answer when the authority is absent. It is never
/// used to *override* an answer the manager gave.
fn live_unit_path(dry_run: bool) -> UnitPath {
    if dry_run {
        return UnitPath::NoManager;
    }
    let mut cmd = std::process::Command::new("systemctl");
    cmd.args(["--user", "show", "-p", "UnitPath", "--value"]);
    match bounded_output(cmd, SYSTEMD_PROBE_BUDGET) {
        // No `systemctl` on this machine at all. There is nothing to ask and
        // nothing uncertain about it: the documented environment calculation
        // is the answer, and always was.
        Bounded::Unstartable(e) => {
            eprintln!("osm: no systemctl to ask where systemd loads units from ({e})");
            UnitPath::NoManager
        }
        // A question that never came back has said nothing about this
        // machine — including whether it has a manager — and it has also just
        // held the install open for as long as it was allowed to. Both halves
        // are why it is bounded and why the bound is not an answer.
        Bounded::Overran => UnitPath::Unknown(format!(
            "`systemctl --user show -p UnitPath` did not answer within \
             {SYSTEMD_PROBE_BUDGET:?}"
        )),
        Bounded::Done(out) if out.status.success() => {
            match osm::install::parse_unit_path(&String::from_utf8_lossy(&out.stdout)) {
                Some(paths) => UnitPath::Answered(paths),
                // It exited 0 and named nothing. On a manager that is running
                // this cannot happen — `UnitPath` is never empty — so it is
                // read the same way as a refusal: an answer only if there is
                // no manager behind it.
                None => uncertain_unless_managerless("it named no unit directories"),
            }
        }
        Bounded::Done(out) => uncertain_unless_managerless(&format!(
            "it exited {}{}",
            out.status,
            match String::from_utf8_lossy(&out.stderr).trim() {
                "" => String::new(),
                e => format!(": {e}"),
            }
        )),
    }
}

/// What a probe that did not produce a unit path means, given whether there
/// is a manager on this machine to have produced one.
///
/// This is the whole distinction the install turns on. "systemctl exited
/// non-zero" is what a machine with **no** user manager looks like — a
/// container, a headless box, an ssh login with no session, all of them real
/// places to install the binary and all of them entitled to the documented
/// environment calculation. It is *also* what a manager that is right there
/// and could not be reached looks like. Only the first may fall back.
fn uncertain_unless_managerless(why: &str) -> UnitPath {
    if user_manager_present() {
        UnitPath::Unknown(format!(
            "`systemctl --user show -p UnitPath` {why}, while a user manager \
             is listening at {}",
            user_manager_socket().display()
        ))
    } else {
        UnitPath::NoManager
    }
}

/// Whether systemd's user manager is there to be asked.
///
/// It listens on `$XDG_RUNTIME_DIR/systemd/private`, which is the socket
/// `systemctl --user` itself connects to; nothing is connected to here, the
/// question is only whether the manager exists. Asked of the filesystem
/// rather than inferred from the probe's exit status, because the exit status
/// cannot tell the two cases apart — see [`uncertain_unless_managerless`].
fn user_manager_present() -> bool {
    user_manager_socket().exists()
}

/// Where that socket is.
///
/// `$XDG_RUNTIME_DIR` when the environment has one, and `/run/user/<uid>` —
/// what systemd itself would have set it to — when it does not, so a login
/// that dropped the variable is not mistaken for a machine with no manager.
fn user_manager_socket() -> PathBuf {
    let runtime = match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => {
            use std::os::unix::fs::MetadataExt as _;
            let uid = std::fs::metadata("/proc/self")
                .map(|m| m.uid())
                .unwrap_or(0);
            PathBuf::from(format!("/run/user/{uid}"))
        }
    };
    runtime.join("systemd/private")
}

/// How long a systemd *question* may take before it is called uncertain.
///
/// There are two of them — where the manager loads units from, and whether
/// `osm.service` is running — and each is a single question to a local
/// manager over a unix socket, which a healthy one answers in milliseconds.
/// The budget exists so that a manager which has stopped answering cannot
/// hold an install or an uninstall open forever, which an unbounded
/// `Command` did: no output, and no way out but Ctrl-C.
///
/// Inside [`stop_units`] it is a ceiling rather than the budget: a probe
/// there gets whatever is left of the stop deadline, or this, whichever is
/// less.
///
/// The systemd *actions* have budgets of their own: [`SYSTEMD_ACTION_BUDGET`]
/// and [`SYSTEMD_STOP_BUDGET`].
const SYSTEMD_PROBE_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);

/// How long one `systemctl --user` *action* may take to be accepted.
///
/// Every action this binary runs is either a `daemon-reload` or is issued
/// `--no-block`, so what is being waited for is the manager taking the
/// command — not the job it starts. A healthy manager does that in
/// milliseconds; a budget this wide can only be exceeded by one that has
/// stopped answering.
///
/// That case used to be unbounded, on the reasoning that killing `systemctl`
/// would not cancel a job it had already queued. It would not — and that is
/// an argument for not *claiming* the job finished, which
/// [`START_QUEUED`] and [`stop_units`] handle. It is not an argument for
/// blocking the CLI forever: an install hung here has already written its
/// files, and an uninstall hung here has not yet removed anything, and in
/// both cases the user is left with no output and no way out but Ctrl-C.
const SYSTEMD_ACTION_BUDGET: std::time::Duration = std::time::Duration::from_secs(15);

/// How long a queued stop is given to actually take effect.
///
/// Wider than [`SYSTEMD_ACTION_BUDGET`] because there is real work behind it:
/// `osm.service` runs a shutdown capture in its `ExecStop`. Narrow enough
/// that a wedged unit cannot hold an uninstall open — and when it runs out,
/// the unit is *not* known to have stopped, so nothing is removed. Refusing
/// is recoverable (run the command again); deleting the binary out from under
/// a live daemon is not.
///
/// It is also the *whole* of the wait. Each `is-active` inside it is capped
/// by what is left of it, so the confirmation gives up here rather than here
/// plus one probe per unit — which is what it used to do, checking the
/// deadline only between rounds.
const SYSTEMD_STOP_BUDGET: std::time::Duration = std::time::Duration::from_secs(30);

/// What the running systemd user manager said about where it loads units
/// from.
///
/// Three answers rather than two, because the two that used to be one are
/// what made a broken install look like a finished one. A probe that failed
/// was converted into "there is no manager", which is a *fact* the plan then
/// acted on: with an `XDG_DATA_HOME` the manager does not share, the fallback
/// says `NotOnSearchPath`, and the install skips the pre-replacement daemon
/// stop, overwrites the binary underneath a running engine, never enables the
/// units, and exits 0.
enum UnitPath {
    /// The manager named the directories it loads units from.
    Answered(Vec<PathBuf>),
    /// There is definitely no manager to ask. The documented environment
    /// calculation is the right answer, and is used.
    NoManager,
    /// The question was not answered, and this machine may well have a
    /// manager that would have answered it differently. Nothing may be
    /// installed or removed on this: see [`UnitPath::refuse`].
    Unknown(String),
}

impl UnitPath {
    /// The unit path to plan against, or the refusal to abort with.
    ///
    /// `verb` is what will not be done — "installed", "removed" — because a
    /// user who is told what did *not* happen can act on it, and the whole
    /// failure being fixed here is a command that said nothing at all.
    fn to_plan_input(&self, verb: &str) -> Result<Option<&[PathBuf]>> {
        match self {
            UnitPath::Answered(paths) => Ok(Some(paths.as_slice())),
            UnitPath::NoManager => Ok(None),
            UnitPath::Unknown(why) => anyhow::bail!(
                "could not find out where systemd's user manager loads units from: {why}. \
                 Nothing was {verb}: guessing this wrong is not visible — the units go to \
                 a directory systemd never reads, the enable is skipped, and the command \
                 exits 0 while nothing starts at the next boot. Check the manager \
                 (`systemctl --user show -p UnitPath`) and run this again; \
                 `--dry-run` shows the plan without asking it."
            ),
        }
    }
}

/// What running a bounded command produced.
enum Bounded {
    /// It ran to completion.
    Done(std::process::Output),
    /// It was still running when the budget ran out, and was killed.
    Overran,
    /// It could not be started at all.
    Unstartable(std::io::Error),
}

/// Run `cmd`, killing it if it does not finish within `budget`.
///
/// The pipes are read only after the command has exited, never after a kill:
/// a child that left a grandchild holding the write end would make that read
/// block, which is the hang this function exists to bound. Everything it is
/// used for prints a few hundred bytes, well inside a pipe buffer, so the
/// child cannot block on writing while this waits.
fn bounded_output(mut cmd: std::process::Command, budget: std::time::Duration) -> Bounded {
    use std::io::Read as _;
    let mut child = match cmd
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return Bounded::Unstartable(e),
    };
    let deadline = std::time::Instant::now() + budget;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut stdout = Vec::new();
                let mut stderr = Vec::new();
                if let Some(mut o) = child.stdout.take() {
                    let _ = o.read_to_end(&mut stdout);
                }
                if let Some(mut e) = child.stderr.take() {
                    let _ = e.read_to_end(&mut stderr);
                }
                return Bounded::Done(std::process::Output {
                    status,
                    stdout,
                    stderr,
                });
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Bounded::Overran;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Bounded::Overran;
            }
        }
    }
}

/// One `systemctl --user` invocation: what to print about it, and what it
/// came back as.
struct Systemctl {
    line: String,
    outcome: ActionOutcome,
}

/// What one `systemctl --user` action came back as.
///
/// Three answers rather than two, for the reason [`UnitState`] has three. A
/// command killed at its budget said *nothing* — and folding that into "it
/// failed" is how an install that could not tell ended with the sentence "the
/// engine is not running": a claim about a daemon nobody had asked about,
/// which an enable the manager had already taken may have started a moment
/// later.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ActionOutcome {
    /// It ran and returned 0: the manager took the command.
    Accepted,
    /// It ran and refused. The units are on the search path and systemd would
    /// not act on them, and an install or an uninstall that carries on
    /// regardless is claiming work it did not do.
    Refused,
    /// It did not come back inside its budget and was killed. Whether the
    /// manager took it is unknown, and so is everything downstream of it.
    Unknown,
    /// It could not be started at all. A machine with no user systemd (a
    /// container, an ssh login with no session bus) is a real place to
    /// install the binary, and the line says the units were not enabled — so
    /// this deliberately does not stop anything.
    Unstartable,
}

impl Systemctl {
    /// Whether the caller must stop here: the command was refused, or nothing
    /// came back from it. Either way, what would follow acts on a manager
    /// state nobody has established.
    fn blocked(&self) -> bool {
        matches!(
            self.outcome,
            ActionOutcome::Refused | ActionOutcome::Unknown
        )
    }
}

/// Run one `systemctl --user` command and report both halves of the outcome.
///
/// Says nothing about *whether* it should be run: that decision belongs to
/// the plan (see [`osm::install::Systemd`]), which the dry run consults too.
/// It used to be made here, separately from the dry run's version of it, and
/// the two disagreed.
///
/// # Why this is bounded, and what the bound does not do
///
/// A successful probe does not bound the command that follows it. A manager
/// answers `is-active` and then wedges on the `daemon-reload` or the stop job
/// after it, and an unbounded `Command::output()` on that hung the install
/// *after* its files were written, and the uninstall *before* it removed
/// anything: no output, and no way out but Ctrl-C.
///
/// What a bound here cannot do is cancel the job. Killing `systemctl` leaves
/// whatever it already queued queued in the manager, which is precisely why
/// this must not be the only thing an action's report rests on:
///
/// * a **start** is issued `--no-block`, so what this waits for is the
///   manager accepting the enable, not `osm-restore.service` finishing a
///   whole restore — a job for which systemd leaves `TimeoutStartSec` at
///   `infinity`, and for which no deadline could tell a large session tree
///   from a wedged one. The install then reports the start as queued, which
///   is all it knows: see [`START_QUEUED`].
/// * a **stop** is issued `--no-block` too, and the caller then confirms the
///   effect by asking [`unit_state`] until the units say they are inactive:
///   see [`stop_units`]. An accepted stop command is not a stopped unit, and
///   an uninstall about to delete the binary that unit is running needs the
///   second thing, not the first.
/// * a `daemon-reload` has no job behind it at all. It is bounded and that is
///   the whole of it; one that did not come back is reported as a failure,
///   because the install cannot say the manager knows about its units.
fn systemctl_user(args: &[&str], budget: std::time::Duration) -> Systemctl {
    let mut cmd = std::process::Command::new("systemctl");
    cmd.arg("--user").args(args);
    match bounded_output(cmd, budget) {
        Bounded::Done(out) if out.status.success() => Systemctl {
            line: format!("systemctl --user {}", args.join(" ")),
            outcome: ActionOutcome::Accepted,
        },
        Bounded::Done(out) => Systemctl {
            line: format!(
                "systemctl --user {} FAILED: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            ),
            outcome: ActionOutcome::Refused,
        },
        // Killed: an *unknown*, and no caller may carry on through it — but
        // it is not the same thing as a refusal, and what the caller says
        // afterwards depends on which it was. "The manager never took the
        // command" is precisely what this does not know. Anything it had
        // already queued is still queued, and the line says so, because that
        // is what the user has to go and look at.
        Bounded::Overran => Systemctl {
            line: format!(
                "systemctl --user {} did not come back within {budget:?} and was killed; \
                 anything it had already queued in the manager is still queued",
                args.join(" ")
            ),
            outcome: ActionOutcome::Unknown,
        },
        Bounded::Unstartable(e) => Systemctl {
            line: format!("systemctl --user {} could not run: {e}", args.join(" ")),
            outcome: ActionOutcome::Unstartable,
        },
    }
}

/// Ask systemd to stop `units`, then wait — bounded — until it says each of
/// them actually is inactive.
///
/// The two halves are separate on purpose. The command is bounded so a
/// manager that will not take it cannot hold the CLI open. But killing
/// `systemctl` does not cancel a job it already queued, and `--no-block`
/// returns as soon as the job *is* queued — so a command that succeeded says
/// nothing about whether the daemon has stopped, and every caller here is
/// about to replace or delete the binary that daemon is running. The only
/// thing that can answer is the unit's own state, which [`unit_state`] gives
/// in three values. `Unknown` is not `Inactive`: a unit nothing could speak
/// for is one this refuses to call stopped.
///
/// `Err(why)` names what is still standing, for a caller whose next line is a
/// refusal the user has to act on.
fn stop_units(argv: &[&str], units: &[&str], lines: &mut Vec<String>) -> Result<(), String> {
    let step = systemctl_user(argv, SYSTEMD_ACTION_BUDGET);
    let outcome = step.outcome;
    let blocked = step.blocked();
    lines.push(step.line);
    if blocked {
        return Err(match outcome {
            // Killed at its budget. What the manager did with the stop is
            // precisely what is not known, so "did not accept" would be a
            // claim of its own.
            ActionOutcome::Unknown => format!(
                "nothing came back from `systemctl --user {}` inside its budget, so whether \
                 systemd took the stop is unknown",
                argv.join(" ")
            ),
            _ => format!(
                "systemd did not accept `systemctl --user {}`",
                argv.join(" ")
            ),
        });
    }

    let deadline = std::time::Instant::now() + SYSTEMD_STOP_BUDGET;
    let expired = |why: &str| format!("{SYSTEMD_STOP_BUDGET:?} after the stop was queued, {why}");
    let mut why = String::new();
    loop {
        let mut standing = Vec::new();
        for unit in units {
            // Before each probe, and capping it — not after a whole round of
            // them. Checked afterwards, a probe that started a moment before
            // the deadline still ran for its own full budget, and so did the
            // one after it: confirming a stop took the stop budget *plus* one
            // probe per unit, about thirty-five seconds for an install and
            // forty for an uninstall against a manager that had stopped
            // answering, while the README promised thirty.
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                if why.is_empty() {
                    why = format!(
                        "nothing could say whether {unit} is still running: there was no \
                         time left to ask"
                    );
                }
                return Err(expired(&why));
            }
            match unit_state_within(unit, left.min(SYSTEMD_PROBE_BUDGET)) {
                UnitState::Inactive => {}
                UnitState::Active => {
                    why = format!("{unit} is still running");
                    standing.push(*unit);
                }
                UnitState::Unknown(reason) => {
                    why = format!("nothing could say whether {unit} is still running: {reason}");
                    standing.push(*unit);
                }
            }
        }
        if standing.is_empty() {
            lines.push(format!("{} confirmed inactive", units.join(" and ")));
            return Ok(());
        }
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return Err(expired(&why));
        }
        // Never past the deadline either: a poll interval slept through in
        // full is the same overrun in miniature.
        std::thread::sleep(left.min(std::time::Duration::from_millis(100)));
    }
}

/// What the running user manager says about `unit`.
///
/// Three answers rather than two, for the same reason [`UnitPath`] has three.
/// This is the question that decides whether a running daemon is stopped
/// before its binary is replaced, and it used to be asked with an unbounded
/// `Command::status()` whose every failure — a spawn that did not happen, a
/// manager that could not be reached, a probe that never came back — was
/// `unwrap_or(false)`: the *fact* "the daemon is not running". The stop is
/// then skipped, the binary is overwritten underneath a live daemon, and two
/// versions of the engine share one database across a schema change. That is
/// precisely the hazard the pre-replacement stop exists to prevent, reached
/// by a different route.
///
/// The state word on stdout is read rather than the exit status, which is why
/// `--quiet` is gone: a unit that is simply not running exits non-zero, and so
/// does a `systemctl` that never got as far as asking. Only exit 0 is
/// unambiguous on its own.
fn unit_state(unit: &str) -> UnitState {
    unit_state_within(unit, SYSTEMD_PROBE_BUDGET)
}

/// The same question with a ceiling of the caller's choosing.
///
/// [`stop_units`] gives it whatever is left of its own deadline, so a probe
/// cannot outlive the confirmation it is part of.
fn unit_state_within(unit: &str, budget: std::time::Duration) -> UnitState {
    let mut cmd = std::process::Command::new("systemctl");
    cmd.args(["--user", "is-active", unit]);
    match bounded_output(cmd, budget) {
        Bounded::Unstartable(e) => {
            state_unless_managerless(unit, &format!("it could not be run ({e})"))
        }
        // A question that never came back has said nothing — including
        // whether anything is running — and has also just held the install
        // open for as long as it was allowed to.
        Bounded::Overran => UnitState::Unknown(format!(
            "`systemctl --user is-active {unit}` did not answer within {budget:?}"
        )),
        // The one answer the exit status gives on its own: systemctl exits 0
        // only when a named unit is active.
        Bounded::Done(out) if out.status.success() => UnitState::Active,
        Bounded::Done(out) => {
            let said = String::from_utf8_lossy(&out.stdout).trim().to_string();
            match said.as_str() {
                // Running, whatever systemctl declines to call active: a unit
                // part-way into or out of a start still has a process holding
                // the binary this install is about to replace.
                "activating" | "deactivating" | "reloading" | "refreshing" | "maintenance" => {
                    UnitState::Active
                }
                // Not running, said by the manager itself.
                "inactive" | "failed" | "unknown" => UnitState::Inactive,
                // Anything else is the manager not having answered: an empty
                // reply is what a failed bus connection leaves behind, and a
                // word this does not know is a state it cannot act on.
                _ => state_unless_managerless(
                    unit,
                    &format!(
                        "it exited {}{}{}",
                        out.status,
                        match said.as_str() {
                            "" => String::new(),
                            other => format!(" saying {other:?}"),
                        },
                        match String::from_utf8_lossy(&out.stderr).trim() {
                            "" => String::new(),
                            e => format!(": {e}"),
                        }
                    ),
                ),
            }
        }
    }
}

/// What a probe that named no state means, given whether there is a manager
/// on this machine to have been running the unit.
///
/// The same distinction [`uncertain_unless_managerless`] draws, for the same
/// reason: with no user manager there is no systemd-started daemon to stop,
/// so "nothing answered" is a fact there and a guess everywhere else.
fn state_unless_managerless(unit: &str, why: &str) -> UnitState {
    if user_manager_present() {
        UnitState::Unknown(format!(
            "`systemctl --user is-active {unit}` {why}, while a user manager \
             is listening at {}",
            user_manager_socket().display()
        ))
    } else {
        UnitState::Inactive
    }
}

/// Whether the daemon is running, or whether that could not be established.
enum UnitState {
    /// It is running, or is on its way into or out of running. Either way a
    /// process is holding the binary.
    Active,
    /// It is not running, and the manager — or the absence of one — said so.
    Inactive,
    /// Nobody could say. Nothing may be replaced on this: see the refusal at
    /// the call site.
    Unknown(String),
}

/// The two units this project installs, in the order a stop must consider
/// them: the daemon before the restore it depends on.
const MANAGED_UNITS: [&str; 2] = ["osm.service", "osm-restore.service"];

/// The `systemctl --user` argument list an install runs: both units enabled
/// and their start **queued**, the restore before the daemon it precedes.
///
/// `--no-block` is the whole of the difference between a bounded install and
/// one that can never return. Without it `systemctl` waits for the start job,
/// and `osm-restore.service` is a `Type=oneshot` unit whose `ExecStart` is a
/// full restore — for which systemd leaves `TimeoutStartSec` at `infinity`,
/// by its own default. With it, the enable is done when this returns and the
/// start is a job in the manager, which is exactly what the install then
/// says: see [`START_QUEUED`].
const ENABLE_UNITS: [&str; 5] = [
    "enable",
    "--now",
    "--no-block",
    "osm-restore.service",
    "osm.service",
];

/// The `systemctl --user` argument list an uninstall runs: both units stopped
/// and disabled, the daemon before the restore it depends on.
///
/// `--no-block` here for the same reason, and with a consequence the install
/// does not have: an uninstall is *about* to delete the binary these units
/// run, so a queued stop is not enough. What the command returns is that the
/// job was accepted; what [`stop_units`] then waits for is the units actually
/// being inactive.
const DISABLE_UNITS: [&str; 5] = [
    "disable",
    "--now",
    "--no-block",
    "osm.service",
    "osm-restore.service",
];

/// What an install prints once `enable --now --no-block` has been accepted.
///
/// It says *queued*, and it says osm did not wait, because that is what
/// happened. There is no budget for waiting on a restore that would not fail
/// a legitimate run, so the choice is between waiting forever and reporting
/// honestly — and a queued start described as a started one is the same class
/// of untruth as rendering "unknown" as "no".
const START_QUEUED: &str =
    "both units are enabled and their start is queued with systemd; osm did not wait for \
     it, so it cannot report the engine as started — `systemctl --user status \
     osm.service osm-restore.service` says whether it is";

/// What `osm status` says about a preserved database, given what is in it.
///
/// This notice used to end "its snapshots are intact but osm no longer reads
/// them" in every case, with nothing having opened the file. On a machine
/// whose preserved database held **nothing** — a backup taken from an
/// already-empty state directory — the panel then told its owner, on every
/// five-second poll, that snapshots he had never lost were sitting somewhere
/// unreadable. A fact nobody checked is not a fact, and asserting one is the
/// same defect as rendering unknown as no.
///
/// Four sentences, because there are four situations and a reader acts
/// differently in each: work is in there, nothing is in there, nobody could
/// tell, and it is not there any more. The empty one says the file can go —
/// and osm does not remove it, because it is the user's file and the only
/// copy of a decision osm is not entitled to make.
///
/// # A count is a count
///
/// The first sentence used to end "which are intact but osm no longer reads
/// them". Nothing had earned the word *intact*. What was established is
/// `SELECT COUNT(*) FROM snapshots` — one query against one table — and a
/// preserved file whose `snapshots` table reads perfectly may have nothing
/// behind those rows at all: no sessions, no windows, no panes, or a newest
/// row still marked `building` because the capture that was writing it never
/// finished. Actually checking would mean an integrity check and a walk of
/// every relation, on a file of unknown size, on a status the panel runs every
/// five seconds — for a database osm has already stopped reading. So the
/// notice says what was measured and stops there.
///
/// The **zero** case is different, and keeps its plain wording: a count of
/// zero does establish that there is nothing in the table, which is the whole
/// of the claim it makes.
fn preserved_notice(path: &str, held: &osm::db::PreservedContents) -> String {
    match held {
        osm::db::PreservedContents::Snapshots(1) => format!(
            "database: an incompatible schema was preserved at {path}; it contains 1 snapshot \
             record, which osm no longer reads — that is a row count and nothing more, so \
             whether the rest of that snapshot is still in the file is unknown"
        ),
        osm::db::PreservedContents::Snapshots(n) if *n > 0 => format!(
            "database: an incompatible schema was preserved at {path}; it contains {n} snapshot \
             records, which osm no longer reads — that is a row count and nothing more, so \
             whether the rest of them is still in the file is unknown"
        ),
        osm::db::PreservedContents::Snapshots(_) => format!(
            "database: an incompatible schema was preserved at {path}; it holds no snapshots, \
             so nothing of yours is in it and you can delete it — osm will not"
        ),
        osm::db::PreservedContents::Unreadable(why) => format!(
            "database: an incompatible schema was preserved at {path}, and osm could not read \
             it ({why}); whether it holds any snapshots is unknown"
        ),
        osm::db::PreservedContents::Gone => format!(
            "database: an incompatible schema was preserved at {path}, and nothing is there \
             now; if you removed it, there is nothing left to do"
        ),
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    // `--socket` wins; otherwise fall back to OSM_TMUX_SOCKET; otherwise the
    // default tmux server. Every `Tmux` this binary constructs must be built
    // from this one value — a call site that builds its own `Tmux::default()`
    // silently reintroduces the hazard this flag exists to close.
    let socket = cli
        .socket
        .clone()
        .or_else(|| std::env::var("OSM_TMUX_SOCKET").ok());
    // Resolved lazily, per command: `status` needs no tmux server at all, and
    // must keep working in a build without the `default-server` feature (the
    // configuration the test suite uses) even when no socket is given.
    let resolve_tmux = || -> Result<osm::tmux::Tmux> {
        let tmux = match &socket {
            Some(name) => osm::tmux::Tmux::with_socket(name),
            None => default_server_tmux()?,
        };
        // The version floor is checked here, once, for every subcommand that
        // touches tmux — before a capture can write a corrupted path or a
        // restore can rebuild one. `status` catches this error and reports it
        // under `tmux.error` instead of dying, which is the one place a broken
        // environment should still produce output.
        tmux.require_supported_version()?;
        Ok(tmux)
    };
    match cli.command {
        // `json` is deliberately ignored — see the flag's doc comment. Output
        // is always JSON, so accepting the flag keeps `osm status --json`
        // working without pretending there is a second output mode.
        Command::Status { json: _ } => {
            let mut problems: Vec<String> = Vec::new();
            // Things the user must be told but which do not stop the engine
            // from working. They reach `message`, never `ready`: a preserved
            // database is a permanent fact about the state directory, and
            // wiring it into `ready` would leave a widget red forever.
            let mut notices: Vec<String> = Vec::new();

            // `{:#}` (anyhow's alternate Display) renders the whole cause
            // chain. Plain `{}` shows only the outermost context, so a TOML
            // syntax error reached the user as the bare, useless "parse
            // /home/…/config.toml" with no hint of what is wrong.
            let cfg = match osm::config::load(&osm::paths::config_path()?) {
                Ok(cfg) => Some(cfg),
                Err(e) => {
                    problems.push(format!("config: {e:#}"));
                    None
                }
            };
            let fallback_interval = cfg
                .as_ref()
                .map(|c| c.capture.fallback_interval_secs)
                .unwrap_or_else(|| osm::config::CaptureCfg::default().fallback_interval_secs);

            let state_dir = osm::paths::state_dir()?;
            let health = osm::health::load(&osm::health::path(&state_dir));
            let stale_after = osm::health::stale_after_secs(fallback_interval);
            let now = osm::boot::now_epoch();
            let (age_secs, stale) = osm::health::freshness(&health, now, stale_after);
            let capture = osm::ipc::CaptureStatus {
                last_success_at: health.last_success_at,
                age_secs,
                stale,
                stale_after_secs: stale_after,
                last_error: health.last_error.clone(),
                last_error_at: health.last_error_at,
                consecutive_failures: health.consecutive_failures,
            };

            // What `privacy.prompt_titles` allows this report to *say*, which
            // is not the same question as what it allowed a past capture to
            // read. A prompt-derived title already on record is suppressed
            // here the moment the key reads `false`, because the panel polls
            // this every five seconds and the capture that will clear the
            // stored copy may be minutes away.
            //
            // A config that did not load answers `AgentOnly` here, for the
            // same reason `agent_config` does: the default reads prompts, and
            // an unreadable file is not permission to.
            let titles = osm::agent::title::Policy::of(
                &cfg.as_ref()
                    .map(|c| c.privacy.clone())
                    .unwrap_or_else(|| osm::config::Config::strict_fallback().privacy),
            );

            let db_path = osm::paths::db_path()?;
            // What the widget renders: the newest snapshot and its sessions.
            // Only ever set from a database that opened; a database that did
            // not stays `None` and `[]`, which is the same shape as "there is
            // no snapshot yet" and is read the same way.
            let mut snapshot = None;
            let mut sessions = Vec::new();
            let database = match osm::db::open(&db_path) {
                // Every database-derived field of the response comes from
                // this one call, and therefore from one moment. The count
                // used to be a query of its own taken just before it, so a
                // capture committing in the gap produced `"snapshots": 0`
                // beside a `"snapshot"` object with an id in it — an empty
                // archive holding something.
                Ok(conn) => match osm::ipc::summarize(&conn, now, titles) {
                    Ok(summary) => {
                        snapshot = summary.snapshot;
                        sessions = summary.sessions;
                        // Read, not assumed. What is in the preserved file is
                        // a different file's business, so it is answered by
                        // opening that file — see `preserved_notice`.
                        let preserved = summary.preserved.and_then(|p| {
                            let held = osm::db::preserved_contents(std::path::Path::new(&p.path));
                            // A file that is no longer there has nothing left
                            // to say. The record's whole job was to tell the
                            // user where their snapshots went; once they have
                            // acted on it — deleting an empty backup is what
                            // the zero-snapshot notice invites — repeating it
                            // on every five-second poll is an alarm about a
                            // situation that is over. Forget it and say
                            // nothing. A file osm merely could not *read* is a
                            // different answer and is kept.
                            if matches!(held, osm::db::PreservedContents::Gone) {
                                let _ = osm::db::forget_preserved(&conn);
                                return None;
                            }
                            notices.push(preserved_notice(&p.path, &held));
                            Some(osm::ipc::PreservedDatabase {
                                path: p.path,
                                schema_version: p.schema_version,
                                preserved_at: p.preserved_at,
                                present: true,
                                snapshots: match held {
                                    osm::db::PreservedContents::Snapshots(n) => Some(n),
                                    _ => None,
                                },
                                error: match held {
                                    osm::db::PreservedContents::Unreadable(why) => Some(why),
                                    _ => None,
                                },
                            })
                        });
                        if let Some(r) = &summary.repaired {
                            notices.push(format!(
                                "the snapshot database had to be repaired: {} row(s) \
                                 belonging to snapshots that were no longer on record \
                                 were cleared. Something deleted those snapshots \
                                 without taking their rows with them; until they were \
                                 cleared, every capture that was handed one of their \
                                 ids failed on a UNIQUE constraint and no snapshot \
                                 could be recorded at all",
                                r.rows
                            ));
                        }
                        if summary.orphan_rows > 0 {
                            problems.push(format!(
                                "database: {} row(s) belong to snapshots that are not \
                                 on record and could not be cleared; capture will fail \
                                 as soon as one of their ids comes round again",
                                summary.orphan_rows
                            ));
                        }
                        osm::ipc::DatabaseStatus {
                            path: db_path.display().to_string(),
                            reachable: true,
                            error: None,
                            snapshots: Some(summary.snapshots),
                            newest_snapshot_at: summary.newest_snapshot_at,
                            preserved,
                            orphan_rows: Some(summary.orphan_rows),
                            repaired: summary.repaired.map(|r| osm::ipc::RepairedRows {
                                at: r.at,
                                rows: r.rows,
                            }),
                        }
                    }
                    // The file opened and could not be read. `ready` stays
                    // about whether osm can run, so this is a notice rather
                    // than a problem — but every count in this block is now
                    // unknown, and `null` is what unknown is. It used to be
                    // `0`, which reads as an empty archive over a database
                    // full of snapshots.
                    Err(e) => {
                        notices.push(format!("database: {e:#}"));
                        osm::ipc::DatabaseStatus {
                            path: db_path.display().to_string(),
                            reachable: true,
                            error: Some(format!("{e:#}")),
                            snapshots: None,
                            newest_snapshot_at: None,
                            preserved: None,
                            orphan_rows: None,
                            repaired: None,
                        }
                    }
                },
                Err(e) => {
                    problems.push(format!("database: {e:#}"));
                    osm::ipc::DatabaseStatus {
                        path: db_path.display().to_string(),
                        reachable: false,
                        error: Some(format!("{e:#}")),
                        snapshots: None,
                        newest_snapshot_at: None,
                        preserved: None,
                        orphan_rows: None,
                        repaired: None,
                    }
                }
            };

            // Read-only (`list-sessions`), and only against the server this
            // process was told to use — the same one every other subcommand
            // talks to.
            let tmux = match resolve_tmux() {
                Ok(t) => osm::ipc::TmuxStatus {
                    socket: t.socket().map(str::to_string),
                    reachable: t.server_running(),
                    error: None,
                },
                // Not merely "the server is not running" — this is osm
                // refusing to talk to this tmux at all (too old, or not
                // installed). Every capture and every restore will fail, so it
                // belongs in `ready`, not only in the tmux block a widget may
                // never read.
                Err(e) => {
                    problems.push(format!("tmux: {e:#}"));
                    osm::ipc::TmuxStatus {
                        socket: socket.clone(),
                        reachable: false,
                        error: Some(format!("{e:#}")),
                    }
                }
            };

            // `ready` is "the engine can run", not "the engine is working" —
            // capture freshness is reported separately and a widget must read
            // both.
            let ready = problems.is_empty();
            problems.extend(notices);
            let message = (!problems.is_empty()).then(|| problems.join("; "));
            // Which agents osm will actually act on, not merely which the
            // config lists. An adapter whose automatic capture and resume are
            // unsupported says so here, with its reason.
            let enabled = cfg
                .as_ref()
                .map(|c| c.agents.enabled.clone())
                .unwrap_or_else(|| osm::config::AgentsCfg::default().enabled);
            let agents = osm::ipc::AgentSupport::of(&enabled);
            let report =
                osm::ipc::StatusReport::new(ready, message, capture, database, tmux, agents)
                    .with_snapshot(snapshot, sessions);
            println!("{}", serde_json::to_string(&report)?);
        }
        Command::Snapshot { reason, debounced } => {
            let tmux = resolve_tmux()?;
            let state_dir = osm::paths::state_dir()?;
            let lock_path = state_dir.join("restore.lock");
            let last_capture_path = state_dir.join("last-capture");
            // Only the debounce *window* falls back here — a wrong throttle
            // at worst captures too often. Retention deliberately does not
            // fall back; see snapshot_with_configured_retention.
            let max_latency_secs = osm::config::load(&osm::paths::config_path()?)
                .map(|c| c.capture.debounce_max_latency_secs)
                .unwrap_or_else(|_| osm::config::CaptureCfg::default().debounce_max_latency_secs);
            let health_path = osm::health::path(&state_dir);
            let now = osm::boot::now_epoch();
            // Record the result before propagating it. A capture that fails
            // on every tmux hook is invisible by construction — hooks send
            // all output to /dev/null — so the only place it can be seen is
            // here, and then in `osm status --json`.
            let result = (|| -> Result<osm::capture::CaptureOutcome> {
                let mut conn = osm::db::open(&osm::paths::db_path()?)?;
                osm::capture::snapshot_maybe_debounced(
                    &mut conn,
                    &tmux,
                    // The compositor, every time. An ordinary capture is the
                    // only thing that ever records where a session's terminal
                    // window is, so a capture path that skips it leaves the
                    // whole of workspace placement unreachable in production
                    // while its unit tests stay green.
                    Some(&osm::hypr::Live::new()),
                    &reason,
                    &lock_path,
                    &last_capture_path,
                    max_latency_secs,
                    now,
                    debounced,
                )
            })();
            match &result {
                Ok(osm::capture::CaptureOutcome::Captured(_)) => {
                    let _ = osm::health::record_success(&health_path, now);
                }
                Err(e) => {
                    let _ = osm::health::record_failure(&health_path, now, &format!("{e:#}"));
                }
                // Debounced, blocked by a restore, or deferred behind another
                // capture: nothing was attempted and nothing failed.
                Ok(_) => {}
            }
            let outcome = result?;
            match outcome {
                osm::capture::CaptureOutcome::Captured(id) => {
                    println!("{}", serde_json::json!({ "snapshot_id": id }))
                }
                osm::capture::CaptureOutcome::Debounced => println!(
                    "{}",
                    serde_json::json!({ "snapshot_id": null, "reason": "debounced" })
                ),
                osm::capture::CaptureOutcome::RestoreInProgress => println!(
                    "{}",
                    serde_json::json!({ "snapshot_id": null, "reason": "restore in progress" })
                ),
                // Truthful about *which* contention this was, and about the
                // fact that the work is not lost: the pending-capture flag
                // makes the next capture run regardless of its debounce
                // window.
                osm::capture::CaptureOutcome::Deferred => println!(
                    "{}",
                    serde_json::json!({
                        "snapshot_id": null,
                        "reason": "capture in progress",
                        "retry_pending": true
                    })
                ),
            }
        }
        Command::Restore { dry_run } => {
            let tmux = resolve_tmux()?;
            let config_path = osm::paths::config_path()?;
            let lock_path = osm::paths::state_dir()?.join("restore.lock");

            // The restore lock is shared with capture, and the tmux hooks fire
            // on `session-created` — i.e. exactly as the first terminal of a
            // fresh login opens, which is exactly when the boot restore runs.
            // A non-blocking acquire turned that sub-second overlap into a
            // cancelled boot restore (osm-restore.service is oneshot with no
            // Restart), so wait for the holder instead. The bound is the
            // readiness budget the user already configured.
            let wait_secs = match osm::config::load(&config_path) {
                Ok(cfg) => cfg.restore.readiness_timeout_secs,
                Err(e) => {
                    let fallback = osm::config::RestoreCfg::default().readiness_timeout_secs;
                    eprintln!(
                        "osm: {}: {e:#}; waiting the default {fallback}s for the restore lock",
                        config_path.display()
                    );
                    fallback
                }
            };
            let wait = std::time::Duration::from_secs(wait_secs);

            let Some(_guard) = osm::lock::SingleInstance::acquire_blocking(&lock_path, wait)?
            else {
                // Not necessarily another restore: a capture holds the same
                // lock. Say only what is actually known.
                eprintln!(
                    "osm: restore lock at {} still held after {wait_secs}s; giving up",
                    lock_path.display()
                );
                let json = osm::ipc::RestoreJson::lock_unavailable();
                println!("{}", serde_json::to_string(&json)?);
                std::process::exit(osm::ipc::exit_code_for_state(&json.state));
            };
            let mut conn = osm::db::open(&osm::paths::db_path()?)?;
            let report = osm::restore::run_restore(&mut conn, &tmux, dry_run)?;
            println!(
                "{}",
                serde_json::to_string(&osm::ipc::RestoreJson::from_report(&report))?
            );
            // The JSON is written first and always: a consumer must still get
            // the full contract on an unsuccessful restore. The exit status
            // exists for systemd, which reads neither stdout nor the
            // database — without it `osm-restore.service` reported a clean
            // success whether or not the user's sessions came back.
            let code = osm::ipc::exit_code_for_state(&report.state);
            if code != 0 {
                if report.state == "unsecured" {
                    eprintln!(
                        "osm: the sessions were restored but this boot's snapshot could not \
                         be written (reason={}); nothing durable records them yet, so the \
                         previous snapshot stays restorable and this exits non-zero",
                        report.reason
                    );
                } else {
                    eprintln!(
                        "osm: restore did not fully succeed (state={}, reason={}); \
                         the snapshot stays restorable and a later run will retry",
                        report.state, report.reason
                    );
                }
                std::process::exit(code);
            }
        }
        Command::Install { prefix, dry_run } => {
            let prefix = match prefix {
                Some(p) => p,
                None => osm::install::default_prefix()?,
            };
            // One plan, normalised once, consulted by the dry run and the real
            // run alike — so what the user reads before is what happens after.
            //
            // The probe is resolved *before* anything is written, and a probe
            // that could not answer stops the command here rather than
            // becoming a guess the plan is built on.
            let probe = live_unit_path(dry_run);
            let plan =
                osm::install::plan_with_unit_path(&prefix, probe.to_plan_input("installed")?)?;
            let bin = plan.binary.display().to_string();
            let mut lines: Vec<String> = Vec::new();

            if dry_run {
                lines.extend(osm::install::install(&plan, true)?);
                if plan.hooks {
                    lines.push(format!("would register tmux hooks at {bin}"));
                }
                lines.push(match plan.systemd {
                    osm::install::Systemd::Manage => {
                        "would enable osm-restore.service and osm.service, and queue their \
                         start with systemd without waiting for it"
                            .to_string()
                    }
                    osm::install::Systemd::NotOnSearchPath => plan.systemd_skipped(&ENABLE_UNITS),
                });
                for line in lines {
                    println!("{line}");
                }
                return Ok(());
            }

            // Before a single file is written. `enable --now` starts an
            // *inactive* unit and does nothing to a running one, so an install
            // over a live engine used to leave the old daemon process running
            // against a new binary, new hook commands and a new CLI — two
            // versions sharing one database, which across a schema change take
            // turns preserving each other's file aside until the user's
            // snapshots are unreachable.
            //
            // A stop that fails is fatal here, while nothing has changed yet:
            // replacing the binary underneath a daemon that would not stop is
            // exactly the state this is meant to prevent.
            //
            // A probe that could not tell whether the daemon is running is
            // fatal here for the same reason a stop that failed is: both
            // leave osm about to write over a binary that may still be
            // executing. Unknown is not "no".
            if plan.systemd == osm::install::Systemd::Manage {
                match unit_state("osm.service") {
                    // Nothing to stop, and `systemctl stop` on a unit that was
                    // never loaded fails — which is the ordinary state of a
                    // first install and must not be mistaken for "the daemon
                    // would not stop".
                    UnitState::Inactive => {}
                    UnitState::Unknown(why) => anyhow::bail!(
                        "could not find out whether osm.service is running: {why}; \
                         nothing was installed. An install that cannot tell used to carry \
                         on as though the daemon were stopped, which replaces the binary \
                         underneath a live one and leaves two versions of the engine on \
                         one database. Check the manager (`systemctl --user is-active \
                         osm.service`) and run this again; `--dry-run` shows the plan \
                         without asking it."
                    ),
                    UnitState::Active => {
                        // Queued and then *confirmed*. A `systemctl stop` that
                        // returned 0 says the manager took the job, which is
                        // not the same as the daemon having let go of the
                        // binary this is about to overwrite.
                        let outcome = stop_units(
                            &["stop", "--no-block", "osm.service"],
                            &["osm.service"],
                            &mut lines,
                        );
                        for line in &lines {
                            println!("{line}");
                        }
                        if let Err(why) = outcome {
                            anyhow::bail!(
                                "the running osm.service was not stopped ({why}), so nothing \
                                 was installed: replacing the binary underneath a daemon that \
                                 is still running leaves two versions of the engine on one \
                                 database"
                            );
                        }
                        lines.clear();
                    }
                }
            }

            lines.extend(osm::install::install(&plan, false)?);

            // After the binary is in place, never before: a hook set to a path
            // that does not exist yet fires at nothing.
            if plan.hooks {
                let tmux = resolve_tmux()?;
                // A machine with no tmux server is the ordinary state of one
                // that installs the engine before opening a terminal, and a
                // hook cannot be set on a server that is not there. This used
                // to be an error — raised *after* the binary and both units
                // were already on disk, so a completed install reported
                // failure. The daemon registers them when a server appears;
                // see `ensure_hooks`.
                if tmux.server_running() {
                    let n = osm::hooks::install(&tmux, &bin)?;
                    lines.push(format!("registered {n} tmux hook(s) at {bin}"));
                } else {
                    lines.push(
                        "no tmux server is running, so no tmux hooks were registered now; \
                         osm.service registers them when one starts"
                            .to_string(),
                    );
                }
            }

            // Which command stopped the install, and what it came back as.
            let mut refused: Option<(String, ActionOutcome)> = None;
            match plan.systemd {
                osm::install::Systemd::Manage => {
                    // In order, and no further than the first one that did not
                    // go through. The loop used to run every command whatever
                    // the one before it did: a `daemon-reload` that failed —
                    // or that was killed at its budget — was followed by
                    // `enable --now`, which starts the units from whatever
                    // definitions the manager still has cached. That is the
                    // stale-unit hazard the reload exists to close, reached by
                    // ignoring the reload's answer.
                    for argv in [vec!["daemon-reload"], ENABLE_UNITS.to_vec()] {
                        let step = systemctl_user(&argv, SYSTEMD_ACTION_BUDGET);
                        let outcome = step.outcome;
                        let blocked = step.blocked();
                        lines.push(step.line);
                        if blocked {
                            refused = Some((argv.join(" "), outcome));
                            break;
                        }
                    }
                    // Only when the manager took both commands, and worded so
                    // that it cannot be read as "the engine is running".
                    if refused.is_none() {
                        lines.push(START_QUEUED.to_string());
                    }
                }
                osm::install::Systemd::NotOnSearchPath => {
                    lines.push(plan.systemd_skipped(&ENABLE_UNITS))
                }
            }
            for line in lines {
                println!("{line}");
            }
            if let Some((command, outcome)) = refused {
                // "The engine is not running" used to be printed here whatever
                // had happened, including after an `enable --now` that was
                // killed at its budget — a command the manager may well have
                // taken, whose start job may well be running. Nothing had
                // asked the unit anything.
                match outcome {
                    ActionOutcome::Unknown => anyhow::bail!(
                        "the files are installed, and nothing came back from `systemctl --user \
                         {command}` inside its budget (see the line above): whether systemd \
                         took it is unknown, and so is whether the engine is running — \
                         anything it had already queued is still queued. `systemctl --user \
                         status osm.service osm-restore.service` says what actually happened."
                    ),
                    // It ran and refused, so the units are certainly not
                    // enabled — and whether a daemon is running is still a
                    // question for the unit, not for this command's exit
                    // status. An earlier one may have been running all along;
                    // this install only stops the daemon it found active.
                    _ => {
                        let engine = match unit_state("osm.service") {
                            UnitState::Inactive => "the engine is not running".to_string(),
                            UnitState::Active => {
                                "osm.service is running — whatever was already there, not the \
                                 units this install would have enabled"
                                    .to_string()
                            }
                            UnitState::Unknown(why) => format!(
                                "whether the engine is running could not be established: {why}"
                            ),
                        };
                        anyhow::bail!(
                            "the files are installed, but systemd would not run `systemctl \
                             --user {command}` (see the FAILED line above), so the units are \
                             not enabled; {engine}"
                        )
                    }
                }
            }
        }
        Command::Uninstall {
            prefix,
            remove_database,
            dry_run,
        } => {
            let prefix = match prefix {
                Some(p) => p,
                None => osm::install::default_prefix()?,
            };
            let probe = live_unit_path(dry_run);
            let plan = osm::install::plan_with_unit_path(&prefix, probe.to_plan_input("removed")?)?;
            let mut lines = Vec::new();
            // Things that did not happen. Every one of them makes this a
            // partial uninstall, and a partial uninstall must not exit 0: the
            // user is entitled to know the engine is still partly installed.
            let mut problems: Vec<String> = Vec::new();

            if dry_run {
                lines.push(match plan.systemd {
                    osm::install::Systemd::Manage => {
                        "would stop and disable osm.service and osm-restore.service, and \
                         wait for both to be inactive before removing anything"
                            .to_string()
                    }
                    osm::install::Systemd::NotOnSearchPath => plan.systemd_skipped(&DISABLE_UNITS),
                });
                lines.push("would remove the tmux hooks".to_string());
            } else {
                // Both before the files go. A unit stopped after its binary is
                // deleted cannot run its `ExecStop`, which is the shutdown
                // capture, and a hook left set after the binary is gone fires
                // at nothing on every tmux event.
                //
                // A stop that fails is therefore fatal *here*, while
                // everything is still on disk. It used to become a line of
                // text, after which the uninstall removed both units and the
                // binary and exited 0 — leaving a daemon running against a
                // deleted binary and a user who had been told the engine was
                // gone.
                match plan.systemd {
                    osm::install::Systemd::Manage => {
                        let step = systemctl_user(&["daemon-reload"], SYSTEMD_ACTION_BUDGET);
                        let reload = step.outcome;
                        let reloaded = !step.blocked();
                        lines.push(step.line);
                        // Queued, then confirmed against the units' own state:
                        // `--no-block` returns when the job is accepted, and
                        // what follows this deletes the binary those units
                        // run. An uninstall that removes it while osm.service
                        // is still active loses the `ExecStop` shutdown
                        // capture and leaves a daemon running against a file
                        // that is no longer there.
                        let outcome = if reloaded {
                            stop_units(&DISABLE_UNITS, &MANAGED_UNITS, &mut lines)
                        } else {
                            Err(match reload {
                                ActionOutcome::Unknown => "nothing came back from `systemctl \
                                     --user daemon-reload` inside its budget, so whether \
                                     systemd reloaded its unit files is unknown"
                                    .to_string(),
                                _ => "systemd did not reload its unit files".to_string(),
                            })
                        };
                        if let Err(why) = outcome {
                            for line in &lines {
                                println!("{line}");
                            }
                            anyhow::bail!(
                                "systemd did not stop the units ({why}), so nothing was \
                                 removed; the engine is still installed and nothing here has \
                                 said it stopped"
                            );
                        }
                    }
                    osm::install::Systemd::NotOnSearchPath => {
                        lines.push(plan.systemd_skipped(&DISABLE_UNITS))
                    }
                }
                // An uninstall must still remove the files when tmux is
                // unusable — but it must not pretend the hooks are gone, and
                // it must not report success.
                let hooks = resolve_tmux().and_then(|tmux| osm::hooks::uninstall(&tmux));
                match hooks {
                    Ok(n) => lines.push(format!("removed {n} tmux hook(s)")),
                    Err(e) => {
                        lines.push(format!("tmux hooks NOT removed: {e:#}"));
                        problems.push("the tmux hooks are still set".to_string());
                    }
                }
            }

            lines.extend(osm::install::uninstall(&plan, !remove_database, dry_run)?);
            if !dry_run {
                let restore_lock = osm::paths::state_dir()?.join("restore.lock");
                match osm::install::uninstall_database(
                    &osm::paths::db_path()?,
                    &restore_lock,
                    remove_database,
                ) {
                    Ok(removed) => lines.extend(removed),
                    Err(e) => {
                        lines.push(format!("the snapshot database was NOT removed: {e:#}"));
                        problems.push("the snapshot database is still there".to_string());
                    }
                }
            }

            for line in lines {
                println!("{line}");
            }
            if !problems.is_empty() {
                anyhow::bail!("uninstall did not finish: {}", problems.join("; "));
            }
        }
        Command::InstallHooks => {
            let tmux = resolve_tmux()?;
            let bin = std::env::current_exe()?;
            let n = osm::hooks::install(&tmux, bin.to_str().unwrap_or("osm"))?;
            println!("{}", serde_json::json!({ "installed": n }));
        }
        Command::UninstallHooks => {
            let tmux = resolve_tmux()?;
            let n = osm::hooks::uninstall(&tmux)?;
            println!("{}", serde_json::json!({ "removed": n }));
        }
        Command::Daemon => {
            let tmux = resolve_tmux()?;
            let config_path = osm::paths::config_path()?;
            // Timings may fall back to defaults — a broken config must not
            // stop the safety-net capture loop from running at all. Retention
            // does *not* fall back; see snapshot_with_configured_retention.
            let cfg = osm::config::load(&config_path).unwrap_or_else(|e| {
                eprintln!(
                    "osm: {}: {e:#}; running with default timings",
                    config_path.display()
                );
                osm::config::Config::default()
            });
            let state_dir = osm::paths::state_dir()?;
            let lock_path = state_dir.join("restore.lock");
            let last_capture_path = state_dir.join("last-capture");
            let health_path = osm::health::path(&state_dir);
            let interval = std::time::Duration::from_secs(cfg.capture.fallback_interval_secs);
            // The hooks are re-registered from here, not only by `install`,
            // because `install` can only reach the server that was running
            // when it ran. `unwrap_or("osm")` matches `install-hooks`: a path
            // that is not UTF-8 cannot go into a tmux command, and the bare
            // name at least resolves through PATH.
            let hook_bin = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("osm"));
            let hook_bin = hook_bin.to_str().unwrap_or("osm").to_string();
            let mut hooked: Option<String> = None;
            // Captures keep the cadence the user configured; the hook check
            // runs on its own, much shorter one. Deadline-based rather than
            // sleep-based so the shorter tick cannot drag the capture
            // interval around.
            let mut next_capture = std::time::Instant::now() + interval;
            loop {
                ensure_hooks(&tmux, &hook_bin, &mut hooked);
                let until_capture =
                    next_capture.saturating_duration_since(std::time::Instant::now());
                if !until_capture.is_zero() {
                    std::thread::sleep(until_capture.min(HOOK_CHECK_INTERVAL));
                    continue;
                }
                next_capture = std::time::Instant::now() + interval;
                let now = osm::boot::now_epoch();
                // debounced=false: the timer always captures regardless of
                // the debounce window, but still routes through
                // snapshot_maybe_debounced so it records last-capture —
                // otherwise a hook firing shortly after a timer tick would
                // never see that a capture just happened.
                let result = (|| -> Result<osm::capture::CaptureOutcome> {
                    let mut conn = osm::db::open(&osm::paths::db_path()?)?;
                    osm::capture::snapshot_maybe_debounced(
                        &mut conn,
                        &tmux,
                        Some(&osm::hypr::Live::new()),
                        "timer",
                        &lock_path,
                        &last_capture_path,
                        cfg.capture.debounce_max_latency_secs,
                        now,
                        false,
                    )
                })();
                // Every capture result used to be discarded (`let _ = …`), so
                // a daemon whose captures had failed for a week looked exactly
                // like one that was working.
                match result {
                    Ok(osm::capture::CaptureOutcome::Captured(_)) => {
                        let _ = osm::health::record_success(&health_path, now);
                    }
                    Ok(_) => {}
                    Err(e) => {
                        let count =
                            osm::health::record_failure(&health_path, now, &format!("{e:#}"))
                                .unwrap_or(0);
                        eprintln!("osm: capture failed ({count} in a row): {e:#}");
                        if count >= MAX_CONSECUTIVE_CAPTURE_FAILURES {
                            eprintln!(
                                "osm: giving up after {count} consecutive capture failures; \
                                 snapshots are going stale (see `osm status --json`)"
                            );
                            std::process::exit(1);
                        }
                    }
                }
            }
        }
        Command::Agents { json: _ } => {
            let tmux = resolve_tmux()?;
            let cfg = agent_config();
            let adapters = osm::agent::adapters(&cfg.agents.enabled);
            // Probing every pane on this server is the whole inventory: a
            // conversation is "live" because a pane is holding its
            // transcript open, which is a fact about the running processes,
            // not about anything osm recorded earlier.
            let probes: Vec<osm::agent::detect::PaneProbe> = tmux
                .list_panes()?
                .into_iter()
                .map(|p| osm::agent::detect::PaneProbe {
                    pane_id: p.id,
                    pane_pid: p.pid,
                    cwd: p.cwd,
                    foreground_cmd: p.cmd,
                })
                .collect();
            let inventory = osm::agent::inventory(
                &probes,
                &adapters,
                osm::agent::title::Policy::of(&cfg.privacy),
            )?;
            println!(
                "{}",
                serde_json::to_string(&osm::ipc::AgentsJson::from_inventory(&inventory))?
            );
        }
        Command::Resume { native_id } => {
            let tmux = resolve_tmux()?;
            let cfg = agent_config();
            let adapters = osm::agent::adapters(&cfg.agents.enabled);

            // The pane this command was typed in. Never inferred: with no
            // TMUX_PANE there is no "current pane", and choosing one would
            // mean sending a conversation into a pane someone is working in
            // — the single worst thing this feature can do.
            let pane = match std::env::var("TMUX_PANE") {
                Ok(p) if !p.is_empty() => p,
                _ => anyhow::bail!(
                    "TMUX_PANE is not set, so there is no current pane to resume into; \
                     run this from inside the tmux pane you want the conversation in"
                ),
            };

            // Which adapter owns this conversation is decided by asking each
            // one what it has on disk — never by the id's shape, which two
            // agents can share (both Claude and Codex name conversations
            // with a UUID).
            let mut owner = None;
            for adapter in &adapters {
                if adapter
                    .discover()?
                    .iter()
                    .any(|session| session.native_id == native_id)
                {
                    owner = Some(adapter);
                    break;
                }
            }
            let Some(adapter) = owner else {
                anyhow::bail!(
                    "no conversation {native_id:?} in any enabled agent ({}); \
                     `osm agents --json` lists the ids that exist",
                    cfg.agents.enabled.join(", ")
                );
            };

            // The incarnation this resume is bound to, read once and then
            // re-checked inside the same tmux operation as the delivery. A
            // server that dies between the preflight and the send is replaced
            // on the same socket and reissues the same `%N`, so without this
            // the conversation would be typed into whatever pane the
            // replacement has given that id to.
            let server = match tmux.running_server_incarnation()? {
                Some(server) => server,
                None => anyhow::bail!(
                    "there is no tmux server on this socket, so there is no pane to \
                     resume {native_id:?} into"
                ),
            };
            let live_pane_ids: Vec<String> = tmux.list_panes()?.into_iter().map(|p| p.id).collect();
            // Every precondition failure is reported as itself, not collapsed
            // into a generic error: "the pane is busy" and "this conversation
            // is already running elsewhere" call for different things from
            // whoever asked. `resume_into` holds one lock on the conversation
            // across the whole of it, so two of these started at the same
            // moment cannot both get past the exclusivity check.
            let outcome = osm::agent::resume::resume_into(
                &tmux,
                &pane,
                adapter.as_ref(),
                &native_id,
                &live_pane_ids,
                osm::agent::resume::DEFAULT_TIMEOUT,
                &server,
            );
            let resumed = matches!(outcome, osm::agent::resume::Outcome::Resumed);
            println!(
                "{}",
                serde_json::to_string(&osm::ipc::ResumeJson::new(
                    adapter.kind(),
                    &native_id,
                    &pane,
                    &outcome
                ))?
            );
            // The JSON is written first and always. The status exists for
            // the caller that does not parse it — a keybinding or a menu
            // entry — which must still be able to see that the conversation
            // is not in the pane.
            if !resumed {
                std::process::exit(1);
            }
        }
    }
    Ok(())
}
