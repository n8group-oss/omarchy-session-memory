use anyhow::Result;
use clap::{Parser, Subcommand};

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

/// The configuration the agent subcommands read, falling back to the
/// defaults when it cannot be loaded.
///
/// A broken config must not make `osm agents` and `osm resume` unusable:
/// the field they need is which adapters are enabled, and the default set
/// is the right answer for a user who never wrote a config at all. The
/// problem is still said out loud on stderr, and `osm status --json`
/// reports it as a real problem — the same treatment `daemon` gives its
/// timings.
fn agent_config() -> osm::config::Config {
    match osm::paths::config_path().and_then(|p| osm::config::load(&p)) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("osm: {e:#}; using the default agent configuration");
            osm::config::Config::default()
        }
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

            let db_path = osm::paths::db_path()?;
            let database = match osm::db::open(&db_path) {
                Ok(conn) => {
                    let counts: Result<(i64, Option<i64>)> = (|| {
                        Ok(conn.query_row(
                            "SELECT COUNT(*), MAX(taken_at) FROM snapshots",
                            [],
                            |r| Ok((r.get(0)?, r.get(1)?)),
                        )?)
                    })();
                    let (snapshots, newest) = counts.unwrap_or((0, None));
                    let preserved = osm::db::preserved(&conn);
                    if let Some(p) = &preserved {
                        notices.push(format!(
                            "database: an incompatible schema was preserved at {}; \
                             its snapshots are intact but osm no longer reads them",
                            p.path
                        ));
                    }
                    osm::ipc::DatabaseStatus {
                        path: db_path.display().to_string(),
                        reachable: true,
                        error: None,
                        snapshots: Some(snapshots),
                        newest_snapshot_at: newest,
                        preserved: preserved.map(|p| osm::ipc::PreservedDatabase {
                            path: p.path,
                            schema_version: p.schema_version,
                            preserved_at: p.preserved_at,
                        }),
                    }
                }
                Err(e) => {
                    problems.push(format!("database: {e:#}"));
                    osm::ipc::DatabaseStatus {
                        path: db_path.display().to_string(),
                        reachable: false,
                        error: Some(format!("{e:#}")),
                        snapshots: None,
                        newest_snapshot_at: None,
                        preserved: None,
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
                osm::ipc::StatusReport::new(ready, message, capture, database, tmux, agents);
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
            loop {
                std::thread::sleep(interval);
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
            let inventory = osm::agent::inventory(&probes, &adapters)?;
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
