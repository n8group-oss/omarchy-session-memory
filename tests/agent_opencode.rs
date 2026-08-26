use osm::agent::{AgentAdapter, AgentKind};
use std::sync::Mutex;

/// Guards every stub write and every `discover()` spawn in this file
/// against a confirmed Linux race, not a hypothetical one: `fork()`
/// duplicates the *whole process's* file descriptor table, not just the
/// calling thread's. `cargo test` runs this file's tests on multiple
/// threads of one process by default. If thread A calls `discover()`
/// (which forks to exec a stub binary) at the exact moment thread B's
/// `std::fs::write()` of a *different*, freshly-created stub script still
/// has that file open for writing, thread A's forked child inherits a
/// duplicate of thread B's write-mode fd and holds it open until its own
/// exec (or exit) closes it. If thread B then tries to exec *its own*
/// stub while that window is still open, the kernel refuses with
/// `ETXTBSY` ("Text file busy") — even though thread B itself never held
/// the file open at exec time. Reproduced directly:
/// `DIAG spawn failed: ... err=Some(Os { code: 26, kind:
/// ExecutableFileBusy, message: "Text file busy" })`, which surfaces here
/// as `discover()` swallowing the spawn error into an empty Vec.
/// `fork()` is normally fast enough that the window is sub-microsecond,
/// but it slows down considerably under memory pressure (copying a
/// multi-gigabyte address space), which is exactly when this suite's
/// tests actually run — hence "intermittently flaky under full-suite
/// parallel load, passes in isolation".
///
/// A single process-wide lock around every write-then-close and every
/// spawn removes the race outright: a write's file descriptor is always
/// closed before the lock is released, so no fork() the lock permits to
/// proceed can ever observe that file still open. This costs no
/// parallelism outside this one file, and it addresses the actual
/// mechanism rather than a retry or `--test-threads=1` masking it.
static SPAWN_LOCK: Mutex<()> = Mutex::new(());

fn spawn_lock() -> std::sync::MutexGuard<'static, ()> {
    SPAWN_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A stub `opencode` that prints a known session list.
fn stub(dir: &std::path::Path, body: &str) -> String {
    stub_exiting(dir, body, 0)
}

/// A stub `opencode` that prints `body` and exits `code`.
fn stub_exiting(dir: &std::path::Path, body: &str, code: i32) -> String {
    let _guard = spawn_lock();
    let p = dir.join(format!("opencode-stub-{code}-{}", body.len()));
    std::fs::write(
        &p,
        format!(
            "#!/bin/sh\nif [ \"$1\" = session ]; then cat <<'EOF'\n{body}\nEOF\nfi\nexit {code}\n"
        ),
    )
    .unwrap();
    let mut perm = std::fs::metadata(&p).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perm, 0o755);
    std::fs::set_permissions(&p, perm).unwrap();
    p.to_str().unwrap().to_string()
    // `_guard` drops here, strictly after the file is written, chmod'd,
    // and closed — never while a write to it could still be in flight.
}

/// `discover()` forks to exec the adapter's binary; see [`SPAWN_LOCK`].
fn discover(bin: &str) -> anyhow::Result<Vec<osm::agent::AgentSession>> {
    let _guard = spawn_lock();
    osm::agent::opencode::OpenCode::with_binary(bin).discover()
}

#[test]
fn discovers_sessions_from_the_public_cli() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = stub(
        tmp.path(),
        r#"[{"id":"ses_abc","directory":"/home/u/app","time":{"updated":1787601392000}}]"#,
    );
    let found = discover(&bin).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].kind, AgentKind::OpenCode);
    assert_eq!(found[0].native_id, "ses_abc");
    assert_eq!(found[0].project_dir.as_deref(), Some("/home/u/app"));
}

#[test]
fn tolerates_a_banner_printed_before_the_json() {
    // Version managers print an activation line before command output; a
    // naive parser breaks on it. This actually happened in this project.
    let tmp = tempfile::tempdir().unwrap();
    let bin = stub(
        tmp.path(),
        "mise ~/.config/mise/config.toml tools: opencode@1.18.18\n[{\"id\":\"ses_x\"}]",
    );
    let found = discover(&bin).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].native_id, "ses_x");
}

#[test]
fn a_missing_binary_is_no_sessions_rather_than_an_error() {
    let found = discover("/nonexistent/opencode").unwrap();
    assert!(found.is_empty());
}

#[test]
fn resume_argv_uses_the_session_flag() {
    let argv = osm::agent::opencode::OpenCode::new().resume_argv("ses_abc");
    assert!(argv.iter().any(|a| a == "--session"), "{argv:?}");
    assert_eq!(argv.last().unwrap(), "ses_abc");
}

// --- "installed but unusable" is not "no conversations" ---------------------
//
// All three cases below used to produce an empty list, indistinguishable from
// a machine with no OpenCode conversations. A caller then reported "nothing to
// resume" for a machine whose conversations osm simply could not read.

#[test]
fn a_binary_that_exits_non_zero_is_a_failure_not_an_empty_inventory() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = stub_exiting(tmp.path(), "", 3);
    let err = discover(&bin).expect_err("a non-zero exit is a failure to look");
    let text = format!("{err:#}");
    assert!(text.contains("exited"), "{text}");
}

#[test]
fn output_that_is_not_json_is_a_failure_not_an_empty_inventory() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = stub(tmp.path(), "error: unknown flag --format");
    let err = discover(&bin).expect_err("unparsable output is a failure to look");
    assert!(format!("{err:#}").contains("invalid JSON"), "{err:#}");
}

#[test]
fn json_that_is_not_the_documented_array_is_a_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = stub(tmp.path(), r#"{"sessions":[]}"#);
    let err = discover(&bin).expect_err("an object is not the array of sessions");
    assert!(format!("{err:#}").contains("array of sessions"), "{err:#}");
}

// --- automatic capture and resume are unsupported, and say so ---------------

/// The finding this replaces a hole with: OpenCode owns no transcript, so
/// binding could score it at most 0.4 against a 0.75 threshold and *no pane
/// could ever be bound to an OpenCode conversation*. Its whole capture path
/// was unreachable, and nothing anywhere said so — the suite exercised
/// discovery and argv and never binding, which is exactly why it passed.
#[test]
fn opencode_declares_that_osm_will_not_capture_or_resume_it_automatically() {
    let a = osm::agent::opencode::OpenCode::new();
    let reason = a
        .auto_unsupported_reason()
        .expect("unsupported, stated rather than left to be inferred from a threshold");
    assert!(
        reason.contains("osm agents") || reason.contains("opencode --session"),
        "the reason tells the user what still works by hand: {reason}"
    );
}

/// And the consequence, at the level a caller sees: it is not offered to
/// `detect::prepare` at all, so nothing it could return can bind a pane.
#[test]
fn an_unsupported_adapter_is_never_prepared_for_binding() {
    let adapters = osm::agent::adapters(&["opencode".to_string(), "claude".to_string()]);
    let prepared = osm::agent::detect::prepare(&adapters).unwrap();
    let kinds: Vec<AgentKind> = prepared.iter().map(|p| p.adapter.kind()).collect();
    assert_eq!(
        kinds,
        vec![AgentKind::Claude],
        "an adapter osm cannot bind must not be given the chance to"
    );
}
