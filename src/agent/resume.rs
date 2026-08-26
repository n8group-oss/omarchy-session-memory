//! Resume preconditions and delivery into a pane.
//!
//! Two rules carried over from the user's bash tooling, learned from real
//! recovery incidents rather than derived in the abstract (see the
//! prior-art study in the plan): never treat tmux target resolution as an
//! existence test, and refusing to resume is always cheaper than a wrong
//! one. `-t` matching is fuzzy twice over — an unprefixed session name
//! matches by *prefix*, and a missing pane index resolves to a *different*
//! pane — so a plausible-looking `display-message` can succeed against a
//! pane that no longer exists. Every check here is instead exact membership
//! in a list the caller supplies, or an unambiguous `%N` pane id.

use super::{AgentAdapter, Liveness};
use crate::agent::detect::{self, PaneProbe};
use crate::tmux::Tmux;
use std::path::PathBuf;

/// What happened, or would happen, when osm tried to put a conversation
/// back into a pane.
///
/// Every precondition failure gets its own variant rather than a bool or a
/// generic error, so a caller — and `osm restore --json` — can tell
/// "nothing to do" (`ActiveElsewhere`, `Unsupported`) apart from "this pane
/// needs a human" (`PaneBusy`, `PaneMissing`) apart from "this actually
/// broke" (`Failed`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Every precondition held. In this module that is the whole meaning —
    /// [`preflight`] never actually sends anything into the pane; delivery
    /// and confirming it took is [`deliver`] (Task 6).
    Resumed,
    /// Some other live process already owns this conversation (its
    /// transcript is held open elsewhere). Sending a second client into it
    /// would race the first, so leaving this pane as a shell is correct,
    /// not a failure.
    ActiveElsewhere,
    /// The pane exists but is not available: something other than an idle
    /// shell is running in its foreground, or it is in copy mode, or its
    /// input is disabled.
    PaneBusy,
    /// `pane_id` is not among the panes the caller listed as live right
    /// now. Never produced by target-resolution success/failure — see the
    /// module docs.
    PaneMissing,
    /// The adapter cannot resume at all (reserved for callers above this
    /// module; nothing in [`preflight`] or [`deliver`] returns it, since
    /// every adapter Tasks 1-4 register does support resume).
    Unsupported,
    /// Whether some other live process already owns this conversation could
    /// not be established — [`super::Liveness::Unknown`].
    ///
    /// Deliberately **not** [`Outcome::Unsupported`]. The two were the same
    /// variant once, and they are opposites in the only way that matters to a
    /// restore: `Unsupported` means there was never anything osm could have
    /// done here, so a run that reports it is still a full success; this means
    /// osm was capable, refused to attempt it because the conversation might
    /// be alive elsewhere, and therefore left a pane as a bare shell that
    /// should be holding a conversation. Reporting that as a success retires
    /// the snapshot which is the only record of what belonged in that pane,
    /// having sent nothing. It is work not done, so it is degraded and the
    /// source stays retryable — the answer may well be different next time,
    /// which is the whole point of retrying.
    OwnershipUnknown,
    /// A check itself failed — a tmux call errored, or (in [`deliver`])
    /// delivery could not be confirmed. Carries a human-readable reason.
    Failed(String),
}

impl Outcome {
    /// A stable, machine-readable name for JSON output and logs — never the
    /// `Debug` form, which is not a contract.
    pub fn as_str(&self) -> &'static str {
        match self {
            Outcome::Resumed => "resumed",
            Outcome::ActiveElsewhere => "active_elsewhere",
            Outcome::PaneBusy => "pane_busy",
            Outcome::PaneMissing => "pane_missing",
            Outcome::Unsupported => "unsupported",
            Outcome::OwnershipUnknown => "ownership_unknown",
            Outcome::Failed(_) => "failed",
        }
    }
}

/// Foreground commands treated as "nothing running here" — an idle
/// interactive shell it is safe to hand a resume command to.
const IDLE_SHELLS: [&str; 4] = ["bash", "zsh", "fish", "sh"];

/// The four preconditions that must all hold before a conversation may be
/// sent into `pane_id`, checked in order and returning the first failure:
///
/// 1. `pane_id` is exact membership in `live_pane_ids` — never a tmux
///    target-resolution existence test (see module docs).
/// 2. the pane's foreground command is an idle shell.
/// 3. the pane is not in copy mode and its input is not disabled.
/// 4. `adapter.is_active_elsewhere(id)` says the conversation is not open
///    anywhere else — and *says* so, rather than being unable to tell, which
///    is [`Outcome::OwnershipUnknown`] and not a licence to proceed.
///
/// Returns `Resumed` only as a placeholder meaning "every precondition
/// holds right now" — it does not send anything into the pane. Actual
/// delivery, and confirming it took, is [`deliver`] (Task 6).
pub fn preflight(
    tmux: &Tmux,
    pane_id: &str,
    adapter: &dyn AgentAdapter,
    id: &str,
    live_pane_ids: &[String],
) -> Outcome {
    // Precondition 1: exact membership, not target resolution. A caller
    // that passes a stale or fabricated pane_id is refused here regardless
    // of whether some *other* pane would happen to answer for it.
    if !live_pane_ids.iter().any(|p| p == pane_id) {
        return Outcome::PaneMissing;
    }

    let panes = match tmux.list_panes() {
        Ok(panes) => panes,
        Err(e) => return Outcome::Failed(format!("list-panes: {e}")),
    };
    // This lookup is not a second existence test — membership in
    // `live_pane_ids` above already established that — it is only how the
    // pane's *current* foreground command is read, by the same exact id.
    let Some(pane) = panes.iter().find(|p| p.id == pane_id) else {
        return Outcome::PaneMissing;
    };

    // Precondition 2.
    if !IDLE_SHELLS.contains(&pane.cmd.as_str()) {
        return Outcome::PaneBusy;
    }

    // Precondition 3.
    match pane_mode_status(tmux, pane_id) {
        Ok((in_copy_mode, input_disabled)) => {
            if in_copy_mode || input_disabled {
                return Outcome::PaneBusy;
            }
        }
        Err(e) => return Outcome::Failed(e),
    }

    // Precondition 4.
    match adapter.is_active_elsewhere(id) {
        Ok(Liveness::Active) => return Outcome::ActiveElsewhere,
        Ok(Liveness::Inactive) => {}
        // Fail closed. "Nobody has this open" and "osm cannot tell whether
        // anybody has this open" differ by exactly one live conversation being
        // handed a second client.
        //
        // Its own outcome, not `Unsupported`: nothing was sent, so the pane is
        // a shell where a conversation belongs, and a caller that treats this
        // as "nothing osm could ever have done" reports a success over it.
        Ok(Liveness::Unknown) => return Outcome::OwnershipUnknown,
        Err(e) => return Outcome::Failed(format!("is_active_elsewhere: {e:#}")),
    }

    Outcome::Resumed
}

/// `(pane_in_mode, pane_input_off)` for `pane_id`.
///
/// Targets `pane_id` directly (a `%N` form) rather than a
/// `session:window.pane` coordinate. Unlike a session name — which tmux
/// matches by *prefix* when unprefixed with `=` — or a pane *index*, which
/// silently resolves onto a different pane when the requested one is gone,
/// a pane id is a unique handle tmux either has or does not have; there is
/// no fuzzy resolution here to avoid. The existence test for `pane_id`
/// itself already happened in [`preflight`] before this is ever called.
fn pane_mode_status(tmux: &Tmux, pane_id: &str) -> Result<(bool, bool), String> {
    let raw = tmux
        .run(&[
            "display-message",
            "-p",
            "-t",
            pane_id,
            "#{pane_in_mode}\t#{pane_input_off}",
        ])
        .map_err(|e| format!("display-message: {e}"))?;
    let line = raw.trim_end_matches('\n');
    let mut fields = line.splitn(2, '\t');
    match (fields.next(), fields.next()) {
        (Some(mode), Some(input_off)) => Ok((mode == "1", input_off == "1")),
        _ => Err(format!("unexpected display-message output: {line:?}")),
    }
}

/// How long to wait for a resumed agent to show up as the pane's foreground
/// command before giving up on it.
///
/// Generous on purpose: a real agent restores a conversation from disk and
/// re-establishes a network session before it draws anything, and the cost
/// of waiting too long is a slower restore, while the cost of waiting too
/// little is reporting a resume as failed that was merely slow — which
/// makes an otherwise clean restore `partial`.
pub const DEFAULT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// How often [`deliver`] re-checks the pane's foreground command while
/// waiting for `expect_cmd` to show up.
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// Wait for a pane the caller has just created to settle to its shell.
///
/// # Why this is needed, and why it is not a sleep in disguise
///
/// tmux reports a pane's foreground command as the name of its pty's
/// foreground process group, and for a moment after a pane is created that
/// group is **tmux itself** rather than the shell it is about to exec. Probed
/// on tmux 3.7c, a `list-panes` issued immediately after `new-session` reports
/// `tmux` 60 times out of 60.
///
/// [`preflight`] reads that same field to decide whether a pane is an idle
/// shell, and a restore preflights the panes it has only just built. So a
/// restore could refuse its own fresh pane as [`Outcome::PaneBusy`], report
/// itself `partial`, and leave the conversation unresumed — for no reason
/// except that it asked too early. It is load-dependent, which is the worst
/// kind: fine on a developer's machine, intermittent on a busy one, and
/// exactly when a boot restore runs.
///
/// This waits for the *observable* condition instead of guessing a duration:
/// it returns as soon as the pane reports an idle shell, and `false` if the
/// budget runs out — at which point the pane really is running something, and
/// `preflight` refuses it, correctly. A pane that is genuinely busy is
/// therefore still refused; only the transient is waited out.
pub fn wait_until_idle(tmux: &Tmux, pane_id: &str, timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match tmux.list_panes() {
            Ok(panes) => match panes.iter().find(|p| p.id == pane_id) {
                // Gone: not something waiting can fix, and `preflight` reports
                // it as the missing pane it is.
                None => return false,
                Some(pane) if IDLE_SHELLS.contains(&pane.cmd.as_str()) => return true,
                Some(_) => {}
            },
            Err(_) => return false,
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// How long [`wait_until_idle`] gives a freshly built pane. Generous relative
/// to the transient it exists for (milliseconds) and short relative to a
/// resume's own budget, so a pane that is genuinely busy costs a restore very
/// little before it is reported as such.
pub const SETTLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Where the inter-process lock for one conversation lives.
///
/// Under the state directory, named from `(kind, native id)` — the same
/// identity a binding is keyed by — so two `osm` processes reaching for the
/// *same* conversation contend and two reaching for different ones do not.
///
/// The id comes from an agent's own store and is interpolated into a
/// filename, so it is percent-encoded down to `[A-Za-z0-9._-]`: a conversation
/// called `../../.ssh/authorized_keys` must name a file inside this directory
/// or nothing at all. The encoding is deterministic and injective, which is
/// what matters — the same conversation must always reach the same lock, while
/// two different ones colliding would only over-serialise, which is safe in
/// the direction that counts.
pub fn conversation_lock_path(kind: super::AgentKind, id: &str) -> Result<PathBuf, String> {
    let dir = crate::paths::state_dir()
        .map_err(|e| format!("locate the state directory for the resume lock: {e:#}"))?
        .join("locks");
    Ok(dir.join(format!(
        "resume-{}-{}.lock",
        kind.as_str(),
        encode_for_filename(id)
    )))
}

/// `[A-Za-z0-9._-]` kept, everything else percent-encoded; long ids are cut
/// and given a deterministic suffix so the result always fits a filename.
fn encode_for_filename(id: &str) -> String {
    const MAX: usize = 180;
    let mut out = String::with_capacity(id.len());
    for b in id.bytes() {
        if b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'_' {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    if out.len() > MAX {
        // FNV-1a, so the truncated form is still unique to this id without
        // reaching for a hashing dependency.
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for b in id.bytes() {
            hash ^= b as u64;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        out.truncate(MAX);
        out.push_str(&format!("-{hash:016x}"));
    }
    out
}

/// Check every precondition and deliver, **holding one lock on the
/// conversation for the whole of it**.
///
/// # The race this closes
///
/// `preflight`'s exclusivity check answers "is any process holding this
/// conversation's transcript open" by scanning `/proc`. Two `osm resume
/// <same-id>` runs started at the same moment both finish that scan before
/// either agent has opened the file, so both see nothing, both pass, and both
/// send — the exact double attach the check exists to prevent. The window is
/// the whole of an agent's startup, which is not small.
///
/// Nothing observable distinguishes the two runs, so nothing about the check
/// itself can fix it. They have to be serialised, and the only identity that
/// serialises the right pairs is the conversation: `(kind, native id)`. The
/// lock is taken **before** the liveness check and held through delivery and
/// identity confirmation, so by the time the second run can look, the first
/// has either put the conversation in a pane — where the scan now finds it —
/// or failed and left nothing.
///
/// A run that cannot take the lock reports [`Outcome::ActiveElsewhere`],
/// which is what it is: another process has this conversation right now.
/// Waiting instead would only turn one refusal into a slower one.
///
/// A state directory that cannot be located or created is
/// [`Outcome::Failed`], never a resume that proceeds unlocked: an unlockable
/// machine is one where this race is live, and the safe answer there is to do
/// nothing.
#[allow(clippy::too_many_arguments)]
pub fn resume_into(
    tmux: &Tmux,
    pane_id: &str,
    adapter: &dyn AgentAdapter,
    id: &str,
    live_pane_ids: &[String],
    timeout: std::time::Duration,
    server: &str,
) -> Outcome {
    let path = match conversation_lock_path(adapter.kind(), id) {
        Ok(path) => path,
        Err(why) => return Outcome::Failed(why),
    };
    let guard = match crate::lock::SingleInstance::acquire(&path) {
        Ok(Some(guard)) => guard,
        // Another osm process holds this conversation right now.
        Ok(None) => return Outcome::ActiveElsewhere,
        Err(e) => {
            return Outcome::Failed(format!(
                "could not take the resume lock at {} ({e:#}); refusing to resume \
                 {id} unserialised",
                path.display()
            ))
        }
    };

    let outcome = match preflight(tmux, pane_id, adapter, id, live_pane_ids) {
        Outcome::Resumed => deliver(tmux, pane_id, adapter, id, timeout, server),
        refused => refused,
    };
    // Explicit, so it is impossible to read this function and think the lock
    // is released before the identity has been confirmed.
    drop(guard);
    outcome
}

/// Put `id` back into `pane_id` and **prove** it is there before reporting
/// success.
///
/// Two steps, and the second one is the point:
///
/// 1. [`deliver_command`] sends the adapter's resume argv, bound to the
///    incarnation `server` names, and waits for the pane's foreground command
///    to become the agent's;
/// 2. the pane is then re-bound, with the same scoring, lineage and
///    file-identity rules `capture` uses, and the binding must be exactly
///    `(adapter.kind(), id)` — twice, a moment apart.
///
/// # Why step 1 is not evidence
///
/// It says a process called `claude` appeared. `claude --resume <id>` with an
/// id the agent rejects *also* makes a process called `claude` appear, for as
/// long as it takes to print the error and exit — and the first poll, a
/// hundred milliseconds later, can easily catch it. The restore then reports
/// the conversation resumed, retires the snapshot that was the only record of
/// it, and leaves the user a shell. That is the exact failure this project
/// cannot reintroduce, and "a process with the right name exists" cannot
/// distinguish it from success.
///
/// # Why twice
///
/// A conversation the agent is about to reject is briefly indistinguishable
/// from one it has accepted: the transcript is opened before it is validated.
/// Requiring the binding to still hold after [`IDENTITY_SETTLE`] is what makes
/// the difference observable without guessing how long a real agent takes to
/// start — the deadline is the caller's timeout either way.
///
/// An adapter osm does not resume automatically
/// ([`AgentAdapter::auto_unsupported_reason`]) gets [`Outcome::Unsupported`]
/// and nothing is sent: there would be no way to tell a resume that worked
/// from one that did not, and reporting success on that basis is what retires
/// snapshots over empty shells.
pub fn deliver(
    tmux: &Tmux,
    pane_id: &str,
    adapter: &dyn AgentAdapter,
    id: &str,
    timeout: std::time::Duration,
    server: &str,
) -> Outcome {
    if adapter.auto_unsupported_reason().is_some() {
        return Outcome::Unsupported;
    }
    let deadline = std::time::Instant::now() + timeout;
    let started = deliver_command(
        tmux,
        pane_id,
        &adapter.resume_argv(id),
        adapter.kind().as_str(),
        timeout,
        server,
    );
    if started != Outcome::Resumed {
        return started;
    }
    confirm_identity(tmux, pane_id, adapter, id, deadline)
}

/// How long the pane must go on holding the conversation before a resume is
/// called confirmed.
const IDENTITY_SETTLE: std::time::Duration = std::time::Duration::from_millis(400);

/// Poll until `pane_id` is bound to exactly `(adapter.kind(), id)` and stays
/// bound to it for [`IDENTITY_SETTLE`], or `deadline` passes.
///
/// Reports what it *did* find rather than only that it did not find what it
/// wanted: a pane bound to a different conversation is a different problem
/// from a pane bound to none, and the operator needs to be able to tell them
/// apart.
fn confirm_identity(
    tmux: &Tmux,
    pane_id: &str,
    adapter: &dyn AgentAdapter,
    id: &str,
    deadline: std::time::Instant,
) -> Outcome {
    let mut settled_since: Option<std::time::Instant> = None;
    let mut last = String::from("nothing");
    loop {
        match bound_now(tmux, pane_id, adapter) {
            Err(e) => return Outcome::Failed(e),
            Ok(Some(binding)) if binding.kind == adapter.kind() && binding.native_id == id => {
                let since = *settled_since.get_or_insert_with(std::time::Instant::now);
                if since.elapsed() >= IDENTITY_SETTLE {
                    return Outcome::Resumed;
                }
            }
            Ok(other) => {
                // Any wobble restarts the clock: a binding that comes and goes
                // is an agent that started and stopped.
                settled_since = None;
                last = match other {
                    Some(b) => format!("{}:{}", b.kind.as_str(), b.native_id),
                    None => "nothing".to_string(),
                };
            }
        }
        if std::time::Instant::now() >= deadline {
            return Outcome::Failed(format!(
                "pane {pane_id} was handed {}:{id} but never held it long enough to \
                 be sure it took (it is running {last}); the conversation is not \
                 confirmed to be back",
                adapter.kind().as_str()
            ));
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// The conversation `pane_id` is running under `adapter` right now, judged by
/// exactly the rules capture uses.
fn bound_now(
    tmux: &Tmux,
    pane_id: &str,
    adapter: &dyn AgentAdapter,
) -> Result<Option<super::Binding>, String> {
    let panes = tmux.list_panes().map_err(|e| format!("list-panes: {e}"))?;
    let Some(pane) = panes.iter().find(|p| p.id == pane_id) else {
        return Err(format!("pane {pane_id} is gone"));
    };
    let probe = PaneProbe {
        pane_id: pane.id.clone(),
        pane_pid: pane.pid,
        cwd: pane.cwd.clone(),
        foreground_cmd: pane.cmd.clone(),
    };
    detect::bound_conversation(&probe, adapter).map_err(|e| format!("{e:#}"))
}

/// Send `argv` into `pane_id`, on the server incarnation `server` names and
/// no other, and confirm the *command* started.
///
/// The mechanism only. It proves that a process of the expected name is the
/// pane's foreground command; it proves nothing about **which** conversation
/// that process is running, which is why every caller resuming a real
/// conversation goes through [`deliver`] instead.
///
/// # The delivery is bound to one server incarnation
///
/// A resume is a statement about a pane, and a pane id is issued by one tmux
/// server. If that server dies and a replacement takes the socket, the
/// replacement hands out `%0`, `%1`, … from zero again, so the id this
/// function was given now names a pane belonging to whatever the user has
/// opened since — and the conversation would be typed into it. Checking the
/// identity first and sending afterwards does not close that: the gap between
/// the two is exactly where the replacement happens. So the check and the
/// send are one tmux operation — see [`Tmux::run_if_incarnation`] — and the
/// payload travels as a *buffer* rather than as `send-keys` arguments
/// precisely so that the guarded command carries nothing that needs quoting
/// inside tmux's own parser.
///
/// Three checks, not one:
///
/// 1. the identity, read before anything is written, so an obviously moved
///    server costs nothing and reports itself plainly;
/// 2. the guarded paste, which cannot run on any other incarnation;
/// 3. confirmation, *polled* rather than sampled once, that the pane's
///    foreground command becomes `expect_cmd` before `timeout` elapses. A
///    freshly created pane briefly reports `tmux` itself as its foreground
///    command before settling to the shell, and a real agent can take a
///    moment to launch, so a single `list-panes` right after delivery would
///    misreport both a slow start and that transient as failures (or, worse,
///    race a stale reading into a false success).
///
/// A timeout without `expect_cmd` ever showing up is [`Outcome::Failed`],
/// never [`Outcome::Resumed`] — see the module-level note: a resume
/// reported as success when the agent never started is the one failure
/// mode this project cannot reintroduce, because it retires the only
/// snapshot that knew the conversation while leaving the user a bare shell.
/// A guarded paste that never ran because the server moved lands here too,
/// and says so.
pub fn deliver_command(
    tmux: &Tmux,
    pane_id: &str,
    argv: &[String],
    expect_cmd: &str,
    timeout: std::time::Duration,
    server: &str,
) -> Outcome {
    // `pane_id` reaches `osm resume` from `$TMUX_PANE`, which is a user's
    // environment variable, and it is about to be interpolated into a tmux
    // command string. Nothing but a `%N` may go there.
    if !is_pane_id(pane_id) {
        return Outcome::Failed(format!(
            "{pane_id:?} is not a tmux pane id, so nothing was delivered anywhere"
        ));
    }
    if let Some(why) = server_moved(tmux, server) {
        return Outcome::Failed(why);
    }

    let cmdline = shell_join(argv);
    // Unique per delivery: a restore resumes several conversations in one
    // run, and two of them must not be able to reach for the same buffer.
    let buffer = format!(
        "osm-resume-{}-{}",
        std::process::id(),
        DELIVERY_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    // The trailing newline is the Enter: `paste-buffer` rewrites a line feed
    // as a carriage return on its way into the pane.
    if let Err(e) = tmux.run(&["set-buffer", "-b", &buffer, &format!("{cmdline}\n")]) {
        return Outcome::Failed(format!("set-buffer for {pane_id}: {e}"));
    }
    let guarded =
        tmux.run_if_incarnation(server, &format!("paste-buffer -d -b {buffer} -t {pane_id}"));
    // `-d` already removed it if the paste ran; this is for when it did not.
    let _ = tmux.run(&["delete-buffer", "-b", &buffer]);
    if let Err(e) = guarded {
        return Outcome::Failed(format!("guarded paste into {pane_id}: {e:#}"));
    }

    let deadline = std::time::Instant::now() + timeout;
    loop {
        match tmux.list_panes() {
            Ok(panes) => {
                if let Some(pane) = panes.iter().find(|p| p.id == pane_id) {
                    if pane.cmd == expect_cmd {
                        return Outcome::Resumed;
                    }
                }
            }
            Err(e) => return Outcome::Failed(format!("list-panes: {e}")),
        }
        if std::time::Instant::now() >= deadline {
            // The likeliest reason for nothing having happened at all is that
            // the guard refused, so say which it was rather than blaming the
            // agent for a start it was never asked to make.
            if let Some(why) = server_moved(tmux, server) {
                return Outcome::Failed(why);
            }
            return Outcome::Failed(format!(
                "timed out after {timeout:?} waiting for pane {pane_id} to run {expect_cmd:?}"
            ));
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Distinguishes the deliveries one process makes, so their tmux buffers
/// cannot collide.
static DELIVERY_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// `%` followed by at least one digit and nothing else.
fn is_pane_id(s: &str) -> bool {
    let Some(rest) = s.strip_prefix('%') else {
        return false;
    };
    !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit())
}

/// Why the server on this socket is no longer the incarnation `server`
/// names, or `None` when it still is.
fn server_moved(tmux: &Tmux, server: &str) -> Option<String> {
    match tmux.running_server_incarnation() {
        Ok(Some(now)) if now == server => None,
        Ok(Some(now)) => Some(format!(
            "the tmux server this resume was verified against ({server}) has been \
             replaced by {now}; nothing was delivered"
        )),
        Ok(None) => Some(format!(
            "the tmux server this resume was verified against ({server}) is gone; \
             nothing was delivered"
        )),
        Err(e) => Some(format!(
            "the tmux server would not report a usable identity ({e:#}), so this \
             resume cannot be shown to be going to the right server; nothing was \
             delivered"
        )),
    }
}

/// Join `argv` into one shell command line, quoting every element.
///
/// A path in the user's install can contain a space or an apostrophe —
/// Plan 1 shipped a hook-quoting bug of exactly this kind — so every
/// element is quoted, never just the ones that look like they need it.
fn shell_join(argv: &[String]) -> String {
    argv.iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ")
}

/// POSIX single-quote `s` for a shell command line: wrap in `'...'` and
/// replace every embedded `'` with `'\''` (close the quote, an escaped
/// literal quote, reopen it). Safe for any byte a POSIX path can contain,
/// including spaces, apostrophes and newlines.
fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}
