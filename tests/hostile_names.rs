//! User-controlled text must never break the capture framing.
//!
//! The field separator is printable ASCII by necessity (tmux ≤ 3.6 rewrites
//! non-printable bytes in format output to `_`), which means a user can type
//! it: `tmux rename-window 'work<|osm:f|>prod'`, a directory called
//! `/tmp/a<|osm:f|>b`, a pane title, the name of a binary on `$PATH`. Every
//! one of those used to make the capture that saw it — and every capture
//! afterwards — fail with `expected N fields, got M`, silently, because the
//! tmux hooks discard output.
//!
//! Newlines and non-ASCII text are the same bug one level up. A directory name
//! may contain either (POSIX allows any byte but `/` and NUL). tmux ≥ 3.7
//! reports both verbatim; tmux ≤ 3.6 rewrote them to `_` inside the server,
//! before osm ever saw them, which is why osm now refuses to run there at all
//! (see `tests/tmux_version.rs`). So every assertion below is **verbatim**:
//! accepting "the original or what an old tmux made of it" was codifying the
//! data loss as correct behaviour. tmux itself refuses a newline in a window
//! name and the shell overwrites a pane title, so those two vectors are
//! covered against synthetic tmux output in `tests/tmux_parse.rs` instead.

mod common;

use osm::tmux::{Tmux, FIELD_SEP, REC_SEP};
use osm::{capture, db};
use std::path::Path;

struct Server(Tmux);

impl Server {
    fn start(label: &str) -> Self {
        Server(Tmux::with_socket(&format!(
            "osm-hostile-{}-{}",
            label,
            std::process::id()
        )))
    }
    fn t(&self) -> &Tmux {
        &self.0
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        common::shutdown(&self.0);
    }
}

fn capture_into_db(t: &Tmux) -> (tempfile::TempDir, rusqlite::Connection, i64) {
    let tmp = tempfile::tempdir().unwrap();
    let mut conn = db::open(&tmp.path().join("state.db")).unwrap();
    let id = capture::snapshot(&mut conn, t, "test")
        .expect("a capture must survive user text that contains the framing tokens");
    (tmp, conn, id)
}

fn one_string(conn: &rusqlite::Connection, sql: &str, id: i64) -> String {
    conn.query_row(sql, [id], |r| r.get(0)).unwrap()
}

#[test]
fn a_window_name_containing_the_separator_is_captured_verbatim() {
    let name = format!("work{FIELD_SEP}prod{REC_SEP}");
    let src = Server::start("winname");
    let t = src.t();
    t.run(&["new-session", "-d", "-s", "dev", "-n", &name, "-c", "/tmp"])
        .unwrap();

    let (_tmp, conn, id) = capture_into_db(t);
    let got = one_string(
        &conn,
        "SELECT name FROM window_rows WHERE snapshot_id = ?1",
        id,
    );
    assert_eq!(got, name, "the window name must round-trip through capture");
}

#[test]
fn a_session_name_containing_the_separator_is_captured_verbatim() {
    // tmux rewrites `.` and `:` in session names, so this vector uses the
    // record separator's shape without its colon.
    let name = "dev<|osm|>work";
    let src = Server::start("sessname");
    let t = src.t();
    t.run(&["new-session", "-d", "-s", name, "-c", "/tmp"])
        .unwrap();

    let (_tmp, conn, id) = capture_into_db(t);
    let got = one_string(
        &conn,
        "SELECT name FROM session_rows WHERE snapshot_id = ?1",
        id,
    );
    assert_eq!(got, name);
}

#[test]
fn a_working_directory_containing_the_separator_is_captured_verbatim() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join(format!("a{FIELD_SEP}b{REC_SEP}c"));
    std::fs::create_dir(&dir).unwrap();
    let dir = dir.to_str().unwrap().to_string();

    let src = Server::start("cwd");
    let t = src.t();
    t.run(&["new-session", "-d", "-s", "dev", "-c", &dir])
        .unwrap();

    let (_tmp, conn, id) = capture_into_db(t);
    let got = one_string(
        &conn,
        "SELECT p.cwd FROM pane_rows p
         JOIN window_rows w ON w.row_id = p.window_row_id
         WHERE w.snapshot_id = ?1",
        id,
    );
    assert_eq!(got, dir);
}

#[test]
fn a_working_directory_containing_a_newline_is_captured() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("nl\ndir");
    std::fs::create_dir(&dir).unwrap();
    let dir = dir.to_str().unwrap().to_string();

    let src = Server::start("cwdnl");
    let t = src.t();
    t.run(&["new-session", "-d", "-s", "dev", "-c", &dir])
        .unwrap();

    let (_tmp, conn, id) = capture_into_db(t);
    let got = one_string(
        &conn,
        "SELECT p.cwd FROM pane_rows p
         JOIN window_rows w ON w.row_id = p.window_row_id
         WHERE w.snapshot_id = ?1",
        id,
    );
    assert_eq!(
        got, dir,
        "a directory name holding a newline must round-trip verbatim"
    );
}

/// The vector the tmux floor exists for. On tmux ≤ 3.6 every byte of every
/// non-ASCII character arrives as `_`, so this directory was persisted — and
/// restored — as `.../______`: a path that is not the user's. There is no
/// "either" here any more; osm refuses to run on those versions instead.
#[test]
fn a_working_directory_containing_non_ascii_is_captured_verbatim() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("żółć");
    std::fs::create_dir(&dir).unwrap();
    let dir = dir.to_str().unwrap().to_string();

    let src = Server::start("cwdutf8");
    let t = src.t();
    t.run(&["new-session", "-d", "-s", "dev", "-c", &dir])
        .unwrap();

    let (_tmp, conn, id) = capture_into_db(t);
    let got = one_string(
        &conn,
        "SELECT p.cwd FROM pane_rows p
         JOIN window_rows w ON w.row_id = p.window_row_id
         WHERE w.snapshot_id = ?1",
        id,
    );
    assert_eq!(
        got, dir,
        "a non-ASCII working directory must round-trip verbatim"
    );
}

/// Same vector in a window name, which the user types directly.
#[test]
fn a_window_name_containing_non_ascii_is_captured_verbatim() {
    let name = "żółć-café-日本語";
    let src = Server::start("winutf8");
    let t = src.t();
    t.run(&["new-session", "-d", "-s", "dev", "-n", name, "-c", "/tmp"])
        .unwrap();

    let (_tmp, conn, id) = capture_into_db(t);
    let got = one_string(
        &conn,
        "SELECT name FROM window_rows WHERE snapshot_id = ?1",
        id,
    );
    assert_eq!(
        got, name,
        "a non-ASCII window name must round-trip verbatim"
    );
}

#[test]
fn a_pane_title_containing_the_separator_is_captured_verbatim() {
    let title = format!("ti{FIELD_SEP}tle{REC_SEP}");
    let src = Server::start("title");
    let t = src.t();
    // `sleep`, not a shell: an interactive shell rewrites the pane title with
    // its own escape sequence the moment it draws a prompt, which would
    // overwrite the fixture before the capture ever ran.
    t.run(&["new-session", "-d", "-s", "dev", "-c", "/tmp", "sleep 60"])
        .unwrap();
    let pane = t
        .run(&["list-panes", "-t", "dev", "-F", "#{pane_id}"])
        .unwrap()
        .trim()
        .to_string();
    t.run(&["select-pane", "-t", &pane, "-T", &title]).unwrap();

    let (_tmp, conn, id) = capture_into_db(t);
    let got = one_string(
        &conn,
        "SELECT p.title FROM pane_rows p
         JOIN window_rows w ON w.row_id = p.window_row_id
         WHERE w.snapshot_id = ?1",
        id,
    );
    assert_eq!(got, title);
}

/// `pane_current_command` is the running process's name, so the vector is a
/// binary whose *filename* holds the separator — nothing exotic, just a
/// script or a symlink someone created.
#[test]
fn a_command_name_containing_the_separator_is_captured_verbatim() {
    let root = tempfile::tempdir().unwrap();
    let cmd_name = format!("sl{FIELD_SEP}p");
    let cmd = root.path().join(&cmd_name);
    // A copy of a real binary, not a `#!/bin/sh` script: the kernel reports
    // the *interpreter* as a script's command name, so a script called
    // `sl<|osm:f|>p` shows up as plain `sh` and would test nothing.
    let sleep = ["/bin/sleep", "/usr/bin/sleep"]
        .into_iter()
        .find(|p| Path::new(p).exists())
        .expect("a sleep binary");
    std::fs::copy(sleep, &cmd).unwrap();
    let mut perms = std::fs::metadata(&cmd).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    std::fs::set_permissions(&cmd, perms).unwrap();

    // tmux hands a shell-command to `sh -c`, and the separator contains `<`
    // and `|`, which are shell metacharacters. Quoting them through tmux's
    // parser *and* the shell's is a fixture detail with nothing to do with
    // the bug, so the token-named script is launched by a wrapper whose own
    // path is boring.
    let runner = root.path().join("run.sh");
    std::fs::write(&runner, format!("#!/bin/sh\nexec '{}' 60\n", cmd.display())).unwrap();
    let mut perms = std::fs::metadata(&runner).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    std::fs::set_permissions(&runner, perms).unwrap();

    let src = Server::start("cmd");
    let t = src.t();
    t.run(&["new-session", "-d", "-s", "dev", "-c", "/tmp"])
        .unwrap();
    t.run(&[
        "split-window",
        "-d",
        "-t",
        "dev",
        "-c",
        "/tmp",
        runner.to_str().unwrap(),
    ])
    .unwrap();
    wait_for_command(t, &cmd_name);

    let (_tmp, conn, id) = capture_into_db(t);
    let commands: Vec<String> = conn
        .prepare(
            "SELECT p.foreground_cmd FROM pane_rows p
             JOIN window_rows w ON w.row_id = p.window_row_id
             WHERE w.snapshot_id = ?1",
        )
        .unwrap()
        .query_map([id], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(
        commands.iter().any(|c| c == &cmd_name),
        "the running command name must round-trip through capture, got {commands:?}"
    );
}

/// The script needs a moment to become the pane's foreground command.
fn wait_for_command(t: &Tmux, name: &str) {
    for _ in 0..50 {
        if t.list_panes()
            .map(|panes| panes.iter().any(|p| p.cmd == name))
            .unwrap_or(false)
        {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(40));
    }
    panic!("pane never reported {name:?} as its current command");
}

/// A guard on the guard: the fixtures above are only meaningful if the paths
/// they build really do contain the separator.
#[test]
fn the_fixtures_actually_contain_the_separator() {
    assert!(FIELD_SEP.contains('|') && REC_SEP.contains('|'));
    assert!(!Path::new(FIELD_SEP).is_absolute());
}
