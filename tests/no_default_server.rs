//! Enforcement: no test may address the **default** tmux server.
//!
//! Reaching the default server once destroyed a live 7-session, 24-pane
//! development environment. Two things stop it now:
//!
//! 1. **Structurally.** `Tmux`'s socket field is private and its socket-less
//!    constructor, `Tmux::default_server()`, exists only under the
//!    `default-server` feature. CI runs the suite with
//!    `--no-default-features`, where that constructor is not compiled at all,
//!    so a test cannot name the default server even if it tries.
//! 2. **By this test**, which runs *with* the tests rather than after them
//!    (the CI grep it replaces ran after the test step, i.e. after any
//!    destruction had already happened) and which knows about this project's
//!    own API — the previous grep only looked for `Command::new("tmux")` and
//!    waved `Tmux::default()` straight through.
//!
//! What neither catches: a test that shells out to tmux through an indirection
//! this file cannot see (a variable holding the string "tmux", a helper script,
//! a `sh -c` line), or a socket name that collides with a real user socket.

use std::fs;
use std::path::{Path, PathBuf};

fn rust_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(rust_files(&path));
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
    out.sort();
    out
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// This file is full of the very strings it hunts for.
fn is_this_file(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|n| n == "no_default_server.rs")
}

#[test]
fn no_test_can_reach_the_default_tmux_server() {
    // Built as one string so the guard does not trip over its own needles.
    let banned_in_tests: [(&str, &str); 4] = [
        (
            "Tmux::default_server",
            "constructs a Tmux aimed at the user's real tmux server",
        ),
        (
            "Tmux::default(",
            "the old socket-less constructor; use Tmux::with_socket",
        ),
        (
            "Tmux { socket",
            "a struct literal would bypass with_socket (the field is private, so this cannot compile — flagged anyway)",
        ),
        (
            "Command::new(\"tmux\")",
            "spawns tmux directly; go through Tmux::with_socket so -L is always passed",
        ),
    ];

    let mut violations: Vec<String> = Vec::new();

    for path in rust_files(&repo_root().join("tests")) {
        if is_this_file(&path) {
            continue;
        }
        let text = fs::read_to_string(&path).unwrap();
        for (line_no, line) in text.lines().enumerate() {
            for (needle, why) in banned_in_tests {
                if line.contains(needle) {
                    violations.push(format!(
                        "{}:{}: {needle} — {why}",
                        path.display(),
                        line_no + 1
                    ));
                }
            }
        }
    }

    // TMUX_TMPDIR does not isolate tmux on every version (it is ignored by
    // the tmux on this developer's machine), so it must not be used as an
    // isolation mechanism anywhere in the project.
    for dir in ["tests", "src"] {
        for path in rust_files(&repo_root().join(dir)) {
            if is_this_file(&path) {
                continue;
            }
            let text = fs::read_to_string(&path).unwrap();
            for (line_no, line) in text.lines().enumerate() {
                if line.contains("TMUX_TMPDIR") {
                    violations.push(format!(
                        "{}:{}: TMUX_TMPDIR — does not isolate tmux on all versions; use -L",
                        path.display(),
                        line_no + 1
                    ));
                }
            }
        }
    }

    assert!(
        violations.is_empty(),
        "these tests could reach the developer's real tmux server:\n{}",
        violations.join("\n")
    );
}

/// Every `Tmux` a test can build carries a socket — there is no other public
/// constructor.
#[test]
fn the_only_public_constructor_takes_a_socket() {
    let t = osm::tmux::Tmux::with_socket("osm-guard-socket");
    assert_eq!(t.socket(), Some("osm-guard-socket"));
}

/// Every spawn of the built `osm` must be given a private state directory and
/// a private tmux server — checked at the spawn, not somewhere in the file.
///
/// `osm` resolves its database from `XDG_STATE_HOME` and its tmux server from
/// `--socket`, so a spawn without both reads and writes the developer's own
/// `~/.local/state/osm/state.db` and talks to the tmux server holding their
/// live work. Two suites did exactly that: `cli.rs` and `agent_cli.rs`.
///
/// The first version of this check asked only whether the *file* contained the
/// strings `CARGO_BIN_EXE_osm`, `XDG_STATE_HOME` and `--socket`. That passes
/// for a file with one isolated helper and one bare `Command::new` beside it,
/// and it passes for a file whose only mention of either control is a comment.
/// Both were demonstrated against it before it was replaced.
///
/// What is checked now is the command chain each spawn actually belongs to:
/// the statement naming the binary, plus — when that statement binds a name —
/// every other statement in the same block that uses that name. Comments and
/// the *contents* of string literals are blanked before the search for
/// statement boundaries, so a comment cannot satisfy the rule and a brace
/// inside a string cannot move a boundary.
///
/// Order counts as well as membership: a control applied after the statement
/// that spawns the process is applied to a process that has already run, and
/// the scan stops there. `cmd.status(); cmd.env("XDG_STATE_HOME", …)` used to
/// pass.
///
/// What it still cannot see: isolation applied through a function that takes
/// the `Command` by reference (`fn isolate(cmd: &mut Command)`), which would
/// be reported as a violation here. Nothing in this repository does that, and
/// the remedy if something needs to is `tests/common/mod.rs`, whose `Env::osm`
/// carries both controls in one chain.
#[test]
fn every_spawn_of_the_binary_isolates_its_state() {
    let mut violations = Vec::new();

    for path in rust_files(&repo_root().join("tests")) {
        if is_this_file(&path) {
            continue;
        }
        let text = fs::read_to_string(&path).unwrap();
        for v in unisolated_spawns(&text) {
            violations.push(format!(
                "{}:{}: spawns osm without {} in its command chain",
                path.display(),
                v.line,
                v.missing.join(" or ")
            ));
        }
    }

    assert!(
        violations.is_empty(),
        "these spawns run osm against the developer's live state:\n{}",
        violations.join("\n")
    );
}

/// One spawn that is missing at least one isolation control.
#[derive(Debug, PartialEq, Eq)]
struct Unisolated {
    line: usize,
    missing: Vec<&'static str>,
}

/// The two controls, and nothing else, that make a spawned `osm` harmless.
///
/// Searched for as bare text rather than as `"--socket"` with its quotes,
/// because one call site builds a wrapper script and passes them through a
/// shell line rather than through `Command::arg`.
const ISOLATION: [&str; 2] = ["XDG_STATE_HOME", "--socket"];

/// The name Cargo gives the built binary. Split so this file, which is exempt
/// from the scan, does not read as a spawn site to a future reader.
fn binary_needle() -> String {
    format!("CARGO_BIN_EXE{}osm", "_")
}

/// Every spawn in `src` whose command chain does not carry both controls.
fn unisolated_spawns(src: &str) -> Vec<Unisolated> {
    let masked = mask(src);
    let needle = binary_needle();
    let mut out = Vec::new();

    for at in find_all(&masked.code, needle.as_bytes()) {
        let chain = command_chain(&masked, at);
        let missing: Vec<&'static str> = ISOLATION
            .iter()
            .copied()
            .filter(|c| !contains(&chain, c.as_bytes()))
            .collect();
        if !missing.is_empty() {
            out.push(Unisolated {
                line: line_of(src, at),
                missing,
            });
        }
    }
    out
}

/// The calls that turn a `Command` into a running process.
///
/// Everything applied to the builder after one of these has already been
/// evaluated is applied to a process that is gone.
const CONSUMING: [&str; 3] = [".spawn(", ".status(", ".output("];

/// The text of the command chain the spawn at `at` belongs to.
///
/// The statement holding the spawn, and — when it binds a name — the other
/// statements in the same block that mention that name, because that is where
/// `cmd.env(…)` and `cmd.arg("--socket")` live when the chain is written over
/// several statements.
///
/// # Only the ones that run first
///
/// Gathering every statement that mentions the binding, wherever it sat,
/// accepted this:
///
/// ```text
/// let mut cmd = Command::new(env!("CARGO_BIN_EXE_osm"));
/// cmd.status();
/// cmd.env("XDG_STATE_HOME", state).args(["--socket", &socket]);
/// ```
///
/// which compiles, runs osm against the developer's own state directory and
/// their live tmux server, and then sets two variables on a `Command` whose
/// process has already exited. Both controls are present, on the right
/// binding, in the right block, and neither reached the process — so the scan
/// stops at the statement that consumes the command. Controls *within* that
/// statement still count: `cmd.args([…]).output()` is one expression,
/// evaluated left to right.
///
/// A command that is never consumed in its own block — the `fn osm(&self) ->
/// Command` helpers in `tests/status.rs` and `tests/install_replace.rs` —
/// escapes to a caller this scan cannot see, so every statement touching it
/// counts, as before.
fn command_chain(masked: &Masked, at: usize) -> Vec<u8> {
    let (start, end) = statement_bounds(&masked.skeleton, at);
    let mut chain = masked.code[start..end].to_vec();

    if let Some(name) = binding_name(&masked.code[start..end]) {
        let (block_start, block_end) = enclosing_block(&masked.skeleton, start);
        let all = statements(&masked.skeleton, block_start, block_end);
        // Where the builder stops being a builder: the first statement, at or
        // after the one that binds it, that both names it and consumes it.
        let consumed_at = all
            .iter()
            .filter(|(s, _)| *s >= start)
            .find(|(s, e)| {
                let stmt = &masked.code[*s..*e];
                mentions(stmt, name.as_bytes())
                    && CONSUMING.iter().any(|c| contains(stmt, c.as_bytes()))
            })
            .map(|(_, e)| *e);
        for (s, e) in all {
            if (s, e) == (start, end) || !mentions(&masked.code[s..e], name.as_bytes()) {
                continue;
            }
            if consumed_at.is_some_and(|stop| s >= stop) {
                continue;
            }
            chain.push(b'\n');
            chain.extend_from_slice(&masked.code[s..e]);
        }
    }
    chain
}

/// Two copies of the source, each the same length as the original so every
/// offset means the same thing in all three.
///
/// * `code` has comments blanked. It is what the content checks read, so a
///   comment naming an isolation control cannot satisfy one.
/// * `skeleton` has comments *and* the contents of string, byte-string and
///   character literals blanked. It is what the structural scan reads, so a
///   brace, a parenthesis or a semicolon inside a string cannot move a
///   statement boundary.
struct Masked {
    code: Vec<u8>,
    skeleton: Vec<u8>,
}

fn mask(src: &str) -> Masked {
    let b = src.as_bytes();
    let mut code = b.to_vec();
    let mut skeleton = b.to_vec();

    // Newlines survive every blanking so line numbers keep counting.
    let mut blank = |from: usize, to: usize, both: bool| {
        for i in from..to.min(b.len()) {
            if b[i] != b'\n' {
                skeleton[i] = b' ';
                if both {
                    code[i] = b' ';
                }
            }
        }
    };

    let mut i = 0;
    while i < b.len() {
        // Line comment.
        if b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'/' {
            let end = b[i..]
                .iter()
                .position(|c| *c == b'\n')
                .map_or(b.len(), |p| i + p);
            blank(i, end, true);
            i = end;
            continue;
        }
        // Block comment, which nests in Rust.
        if b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'*' {
            let mut depth = 1usize;
            let mut j = i + 2;
            while j < b.len() && depth > 0 {
                if b[j] == b'/' && j + 1 < b.len() && b[j + 1] == b'*' {
                    depth += 1;
                    j += 2;
                } else if b[j] == b'*' && j + 1 < b.len() && b[j + 1] == b'/' {
                    depth -= 1;
                    j += 2;
                } else {
                    j += 1;
                }
            }
            blank(i, j, true);
            i = j;
            continue;
        }
        // Raw string: r"…", r#"…"#, br##"…"##.
        if let Some((body, end)) = raw_string(b, i) {
            blank(body.0, body.1, false);
            i = end;
            continue;
        }
        // Ordinary string or byte string.
        if b[i] == b'"' {
            let mut j = i + 1;
            while j < b.len() {
                if b[j] == b'\\' {
                    j += 2;
                    continue;
                }
                if b[j] == b'"' {
                    break;
                }
                j += 1;
            }
            blank(i + 1, j.min(b.len()), false);
            i = (j + 1).min(b.len());
            continue;
        }
        // A character literal, but not a lifetime.
        if b[i] == b'\'' {
            if i + 1 < b.len() && b[i + 1] == b'\\' {
                let mut j = i + 2;
                while j < b.len() && b[j] != b'\'' {
                    j += 1;
                }
                blank(i + 1, j, false);
                i = (j + 1).min(b.len());
                continue;
            }
            if i + 2 < b.len() && b[i + 2] == b'\'' {
                blank(i + 1, i + 2, false);
                i += 3;
                continue;
            }
        }
        i += 1;
    }

    Masked { code, skeleton }
}

/// `(body range, position after the closing delimiter)` for a raw string
/// starting at `i`, or `None` if one does not start there.
fn raw_string(b: &[u8], i: usize) -> Option<((usize, usize), usize)> {
    let mut j = i;
    if b[j] == b'b' {
        j += 1;
    }
    if j >= b.len() || b[j] != b'r' {
        return None;
    }
    // `r` has to start a token: `for` and `str` are not raw strings.
    if i > 0 && (b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_') {
        return None;
    }
    j += 1;
    let hashes = b[j..].iter().take_while(|c| **c == b'#').count();
    j += hashes;
    if j >= b.len() || b[j] != b'"' {
        return None;
    }
    let body_start = j + 1;
    let mut k = body_start;
    let close: Vec<u8> = std::iter::once(b'"')
        .chain(std::iter::repeat_n(b'#', hashes))
        .collect();
    while k < b.len() {
        if b[k] == b'"' && b[k..].starts_with(&close) {
            return Some(((body_start, k), k + close.len()));
        }
        k += 1;
    }
    Some(((body_start, b.len()), b.len()))
}

/// The statement containing `at`: from just after the previous statement to
/// just after its own terminator.
fn statement_bounds(skel: &[u8], at: usize) -> (usize, usize) {
    let mut depth: i32 = 0;
    let mut closed_block: Option<usize> = None;
    let mut i = at;
    let start = loop {
        if i == 0 {
            break 0;
        }
        i -= 1;
        match skel[i] {
            b')' | b']' => depth += 1,
            b'}' => {
                if depth == 0 {
                    closed_block = Some(i);
                }
                depth += 1;
            }
            b'(' | b'[' => {
                // Stepping out of a call the spawn sits inside — `env!(…)`,
                // `format!(…)` — is not a statement boundary; the statement
                // continues to the left of it.
                depth = (depth - 1).max(0);
            }
            b'{' => {
                depth -= 1;
                if depth < 0 {
                    break i + 1;
                }
                // The previous statement ended in a block of its own; that
                // block's closing brace is this statement's left edge.
                if depth == 0 {
                    if let Some(close) = closed_block.take() {
                        break close + 1;
                    }
                }
            }
            b';' if depth == 0 => break i + 1,
            _ => {}
        }
    };

    let mut depth: i32 = 0;
    let mut j = start;
    let end = loop {
        if j >= skel.len() {
            break skel.len();
        }
        match skel[j] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => {
                depth -= 1;
                if depth < 0 {
                    break j;
                }
            }
            b';' if depth == 0 => break j + 1,
            _ => {}
        }
        j += 1;
    };
    (start, end)
}

/// The innermost `{ … }` containing `pos`, as the range between the braces.
fn enclosing_block(skel: &[u8], pos: usize) -> (usize, usize) {
    let mut depth: i32 = 0;
    let mut i = pos;
    let start = loop {
        if i == 0 {
            break 0;
        }
        i -= 1;
        match skel[i] {
            b'}' => depth += 1,
            b'{' => {
                if depth == 0 {
                    break i + 1;
                }
                depth -= 1;
            }
            _ => {}
        }
    };

    let mut depth: i32 = 0;
    let mut j = start;
    let end = loop {
        if j >= skel.len() {
            break skel.len();
        }
        match skel[j] {
            b'{' => depth += 1,
            b'}' => {
                if depth == 0 {
                    break j;
                }
                depth -= 1;
            }
            _ => {}
        }
        j += 1;
    };
    (start, end)
}

/// The top-level statements of one block.
fn statements(skel: &[u8], start: usize, end: usize) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut depth: i32 = 0;
    let mut from = start;
    let mut i = start;
    while i < end {
        match skel[i] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b';' if depth == 0 => {
                out.push((from, i + 1));
                from = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    if from < end {
        out.push((from, end));
    }
    out
}

/// The name a `let` statement binds, if it is one.
fn binding_name(stmt: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(stmt);
    let rest = text.trim_start().strip_prefix("let ")?;
    let rest = rest.trim_start();
    let rest = rest.strip_prefix("mut ").unwrap_or(rest).trim_start();
    let name: String = rest
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    (!name.is_empty()).then_some(name)
}

fn find_all(hay: &[u8], needle: &[u8]) -> Vec<usize> {
    let mut out = Vec::new();
    if needle.is_empty() || hay.len() < needle.len() {
        return out;
    }
    for i in 0..=hay.len() - needle.len() {
        if &hay[i..i + needle.len()] == needle {
            out.push(i);
        }
    }
    out
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    !find_all(hay, needle).is_empty()
}

/// `needle` as a whole identifier, so `cmd` does not match `cmdline`.
fn mentions(hay: &[u8], needle: &[u8]) -> bool {
    find_all(hay, needle).into_iter().any(|i| {
        let before_ok = i == 0 || !(hay[i - 1].is_ascii_alphanumeric() || hay[i - 1] == b'_');
        let after = i + needle.len();
        let after_ok =
            after >= hay.len() || !(hay[after].is_ascii_alphanumeric() || hay[after] == b'_');
        before_ok && after_ok
    })
}

fn line_of(src: &str, at: usize) -> usize {
    src.as_bytes()[..at].iter().filter(|c| **c == b'\n').count() + 1
}

/// The scanner, held to the evasions that defeated the one it replaces.
///
/// A guard nobody has attacked is a guard nobody has tested, and the previous
/// version passed its own suite while a bare spawn sat two lines below an
/// isolated helper. These are that spawn, and the others it would also have
/// waved through, written out.
mod scanner {
    use super::{unisolated_spawns, Unisolated};

    /// Written at run time so this file, which the scan skips, still does not
    /// contain a literal spawn site for a future reader to imitate.
    fn src(body: &str) -> String {
        body.replace("BIN", &super::binary_needle())
    }

    fn missing(body: &str) -> Vec<Unisolated> {
        unisolated_spawns(&src(body))
    }

    #[test]
    fn a_chain_carrying_both_controls_is_accepted() {
        assert_eq!(
            missing(
                r#"
                fn osm() -> Output {
                    Command::new(env!("BIN"))
                        .env("XDG_STATE_HOME", dir.join("state"))
                        .args(["--socket", &socket, "status"])
                        .output()
                        .unwrap()
                }
                "#
            ),
            vec![]
        );
    }

    #[test]
    fn a_chain_split_over_statements_is_accepted() {
        assert_eq!(
            missing(
                r#"
                fn osm() -> Command {
                    let mut cmd = Command::new(env!("BIN"));
                    cmd.env("XDG_STATE_HOME", dir.join("state"));
                    if hard_links { cmd.env_remove("X"); } else { cmd.env("X", "1"); }
                    let out = cmd
                        .args(["--socket", &socket, "status"])
                        .output()
                        .unwrap();
                    out
                }
                "#
            ),
            vec![]
        );
    }

    /// The evasion the previous scanner passed: one isolated helper, and a
    /// bare spawn in the same file.
    #[test]
    fn a_bare_spawn_beside_an_isolated_helper_is_caught() {
        let found = missing(
            r#"
            fn osm(&self) -> Command {
                let mut cmd = Command::new(env!("BIN"));
                cmd.env("XDG_STATE_HOME", self.state()).arg("--socket").arg(&self.socket);
                cmd
            }

            fn quick() -> Output {
                Command::new(env!("BIN")).arg("status").output().unwrap()
            }
            "#,
        );
        assert_eq!(found.len(), 1, "the bare spawn was not caught: {found:?}");
        assert_eq!(found[0].missing, vec!["XDG_STATE_HOME", "--socket"]);
    }

    /// The other evasion: the controls are named, but only in prose.
    #[test]
    fn controls_that_appear_only_in_comments_do_not_count() {
        let found = missing(
            r#"
            // This helper sets XDG_STATE_HOME and passes --socket.
            fn osm() -> Output {
                /* XDG_STATE_HOME, --socket */
                Command::new(env!("BIN")).arg("status").output().unwrap()
            }
            "#,
        );
        assert_eq!(found.len(), 1, "a comment satisfied the check: {found:?}");
        assert_eq!(found[0].missing, vec!["XDG_STATE_HOME", "--socket"]);
    }

    /// Half-isolated is a violation, and the report says which half.
    #[test]
    fn a_socket_without_a_state_directory_is_caught() {
        let found = missing(
            r#"
            fn osm() -> Output {
                Command::new(env!("BIN"))
                    .args(["--socket", &socket, "snapshot"])
                    .output()
                    .unwrap()
            }
            "#,
        );
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].missing, vec!["XDG_STATE_HOME"]);
    }

    #[test]
    fn a_state_directory_without_a_socket_is_caught() {
        let found = missing(
            r#"
            fn osm() -> Output {
                Command::new(env!("BIN"))
                    .env("XDG_STATE_HOME", state)
                    .arg("snapshot")
                    .output()
                    .unwrap()
            }
            "#,
        );
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].missing, vec!["--socket"]);
    }

    /// A neighbouring statement's isolation is not this spawn's isolation.
    #[test]
    fn a_sibling_statements_controls_are_not_borrowed() {
        let found = missing(
            r#"
            fn go() {
                let isolated = Command::new(env!("BIN"))
                    .env("XDG_STATE_HOME", state)
                    .args(["--socket", &socket, "status"])
                    .output()
                    .unwrap();
                let bare = Command::new(env!("BIN")).arg("status").output().unwrap();
            }
            "#,
        );
        assert_eq!(found.len(), 1, "expected only the bare spawn: {found:?}");
        assert_eq!(found[0].missing, vec!["XDG_STATE_HOME", "--socket"]);
    }

    /// The controls reach a wrapper script through a shell line rather than
    /// through `Command`, which is how `tests/hooks.rs` spawns osm.
    #[test]
    fn controls_written_into_a_wrapper_script_count() {
        assert_eq!(
            missing(
                r##"
                fn wrapper() {
                    let body = format!(
                        "#!/bin/sh\nXDG_STATE_HOME=\"{state}\" exec \"{bin}\" --socket \"{socket}\" snapshot\n",
                        state = state.display(),
                        bin = env!("BIN"),
                        socket = socket,
                    );
                }
                "##
            ),
            vec![]
        );
    }

    /// A brace inside a string literal must not be read as a block boundary.
    #[test]
    fn braces_inside_string_literals_do_not_move_a_boundary() {
        assert_eq!(
            missing(
                r##"
                fn osm() -> Output {
                    let script = r#"case $1 in x) echo "{" ;; esac"#;
                    let mut cmd = Command::new(env!("BIN"));
                    cmd.env("XDG_STATE_HOME", state).arg("--socket").arg(&socket);
                    cmd.output().unwrap()
                }
                "##
            ),
            vec![]
        );
    }

    /// The evasion this scanner could not see: every control is present, on
    /// the right binding, in the right block — and applied *after* the
    /// process has already been spawned.
    ///
    /// `cmd.status()` runs osm there and then, against the developer's state
    /// directory and their tmux server. What the next line does to `cmd` is
    /// of no interest to a process that has already exited. The scan gathered
    /// every statement mentioning the binding regardless of where it sat, so
    /// this compiled, ran unisolated, and passed the guard written to stop
    /// exactly that.
    #[test]
    fn controls_applied_after_the_spawn_do_not_count() {
        let found = missing(
            r#"
            fn go() {
                let mut cmd = Command::new(env!("BIN"));
                cmd.status();
                cmd.env("XDG_STATE_HOME", state).args(["--socket", &socket]);
            }
            "#,
        );
        assert_eq!(
            found.len(),
            1,
            "isolation applied after the spawn satisfied the check: {found:?}"
        );
        assert_eq!(found[0].missing, vec!["XDG_STATE_HOME", "--socket"]);
    }

    /// The same, for the other two ways a `Command` is consumed.
    #[test]
    fn every_consuming_call_ends_the_chain() {
        for consume in ["output()", "spawn()"] {
            let found = missing(&format!(
                r#"
                fn go() {{
                    let mut cmd = Command::new(env!("BIN"));
                    let handle = cmd.{consume};
                    cmd.env("XDG_STATE_HOME", state).args(["--socket", &socket]);
                }}
                "#
            ));
            assert_eq!(
                found.len(),
                1,
                "`{consume}` did not end the chain: {found:?}"
            );
            assert_eq!(found[0].missing, vec!["XDG_STATE_HOME", "--socket"]);
        }
    }

    /// Half before, half after: the half that came too late does not count,
    /// and the report says which one is missing.
    #[test]
    fn a_control_added_after_the_spawn_is_the_one_reported_missing() {
        let found = missing(
            r#"
            fn go() {
                let mut cmd = Command::new(env!("BIN"));
                cmd.env("XDG_STATE_HOME", state);
                cmd.output().unwrap();
                cmd.args(["--socket", &socket]);
            }
            "#,
        );
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].missing, vec!["--socket"]);
    }

    /// The controls in the consuming statement itself are applied before the
    /// call that consumes it — that is one expression, evaluated left to
    /// right — so they count.
    #[test]
    fn controls_in_the_consuming_statement_itself_count() {
        assert_eq!(
            missing(
                r#"
                fn go() {
                    let mut cmd = Command::new(env!("BIN"));
                    cmd.env("XDG_STATE_HOME", state);
                    cmd.args(["--socket", &socket, "status"]).output().unwrap();
                }
                "#
            ),
            vec![]
        );
    }

    /// A `Command` that is *returned* is consumed somewhere this scan cannot
    /// see, so every statement that touches it still counts. This is how
    /// `tests/status.rs` and `tests/install_replace.rs` build theirs.
    #[test]
    fn a_command_returned_rather_than_consumed_is_read_whole() {
        assert_eq!(
            missing(
                r#"
                fn osm(&self) -> Command {
                    let mut cmd = Command::new(env!("BIN"));
                    cmd.env("XDG_STATE_HOME", self.state());
                    cmd.arg("--socket").arg(&self.socket);
                    cmd
                }
                "#
            ),
            vec![]
        );
    }

    /// The line reported is the spawn's own, so the message points at the
    /// call rather than at the file.
    #[test]
    fn the_reported_line_is_the_spawn_site() {
        let found = missing("\nfn go() {\n    Command::new(env!(\"BIN\")).output().unwrap();\n}\n");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].line, 3);
    }
}
