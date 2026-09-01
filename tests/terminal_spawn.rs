//! Spawning a marked terminal attached to a session.

use osm::terminal::{self, Kind};

#[test]
fn an_explicit_configuration_wins_over_detection() {
    assert_eq!(terminal::detect("kitty"), Some(Kind::Kitty));
    assert_eq!(terminal::detect("foot"), Some(Kind::Foot));
    assert_eq!(terminal::detect("alacritty"), Some(Kind::Alacritty));
}

#[test]
fn an_unknown_terminal_name_is_rejected_rather_than_guessed() {
    assert_eq!(terminal::detect("xterm"), None);
    assert_eq!(terminal::detect(""), None);
    assert_eq!(terminal::detect("GHOSTTY"), None, "matching is exact");
}

#[test]
fn ghostty_is_spawned_as_its_own_instance() {
    // Without this flag the request is forwarded to a running Ghostty and no
    // new window appears at all — observed during a manual recovery, where
    // every spawn silently produced nothing.
    let argv = terminal::spawn_argv(Kind::Ghostty, "dev", "osm-restore-7", Some("osm-1"));
    assert!(
        argv.iter().any(|a| a == "--gtk-single-instance=false"),
        "{argv:?}"
    );
}

#[test]
fn every_terminal_carries_the_ownership_marker() {
    for k in [Kind::Ghostty, Kind::Alacritty, Kind::Kitty, Kind::Foot] {
        let argv = terminal::spawn_argv(k, "dev", "osm-restore-7", Some("osm-1"));
        assert!(
            argv.iter().any(|a| a.contains("osm-restore-7")),
            "{k:?} spawned without the marker that proves osm owns it: {argv:?}"
        );
    }
}

#[test]
fn the_session_name_reaches_tmux_as_one_argument_however_odd_it_is() {
    // A quote or a space in a session name must not split the command or
    // escape the quoting. Plan 1 shipped a hook that broke on exactly this.
    for name in ["my session", "it's mine", "a'b c'd", "plain"] {
        let argv = terminal::spawn_argv(Kind::Ghostty, name, "m", Some("osm-1"));
        let cmd = argv.last().unwrap();
        let prefix = "exec tmux -u -L 'osm-1' attach-session -t ";
        assert!(cmd.starts_with(prefix), "{name:?} -> {cmd:?}");
        // Round-trip it through a shell to prove the quoting survives.
        let out = std::process::Command::new("bash")
            .arg("-c")
            .arg(format!("printf '%s' {}", &cmd[prefix.len()..]))
            .output()
            .expect("bash");
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            name,
            "quoting mangled {name:?}"
        );
    }
}

#[test]
fn the_marker_is_unique_per_attempt() {
    assert_ne!(terminal::marker_for(7), terminal::marker_for(8));
    assert!(terminal::marker_for(7).contains('7'));
}

#[test]
fn the_terminal_is_pointed_at_the_restores_own_tmux_server() {
    // Without `-L` the terminal attaches to the *default* server. With a
    // session name that also exists there — `dev`, `main`, `mine` — it
    // attaches to somebody else's work; without one it exits immediately and
    // the restore reports a window that never mapped. Neither is acceptable,
    // and the first has already happened once on the maintainer's desktop.
    let argv = terminal::spawn_argv(Kind::Ghostty, "dev", "m", Some("osm-restore-1"));
    let cmd = argv.last().unwrap();
    assert!(
        cmd.contains("-L 'osm-restore-1'"),
        "the socket never reached the attach: {cmd:?}"
    );
    assert!(
        cmd.contains("-u "),
        "tmux mangles non-ASCII without -u: {cmd:?}"
    );
}

#[test]
fn a_socket_name_is_shell_quoted_like_the_session_name() {
    // `--socket` takes whatever the caller gives it, and the whole attach is
    // one `bash -lc` string.
    let argv = terminal::spawn_argv(Kind::Ghostty, "dev", "m", Some("odd name's"));
    let cmd = argv.last().unwrap();
    let out = std::process::Command::new("bash")
        .arg("-c")
        .arg(format!(
            "printf '%s' {}",
            &cmd[cmd.find("-L ").unwrap() + 3..cmd.find(" attach-session").unwrap()]
        ))
        .output()
        .expect("bash");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "odd name's");
}

#[test]
fn no_socket_means_the_default_server_and_says_so_by_omission() {
    // The only case where leaving `-L` out is the truth.
    let argv = terminal::spawn_argv(Kind::Ghostty, "dev", "m", None);
    let cmd = argv.last().unwrap();
    assert_eq!(cmd, "exec tmux -u attach-session -t 'dev'");
}

/// A directory holding `binaries`, each an executable file, and nothing else.
///
/// The whole `PATH` a `choose_on` test sees: no test here may read or write
/// this process's own `PATH`, which every other test in this binary is using
/// at the same time — and which on the developer's machine has their real
/// terminals on it.
fn path_with(binaries: &[&str]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for b in binaries {
        let p = dir.path().join(b);
        std::fs::write(&p, "#!/bin/sh\nexit 0\n").unwrap();
        let mut perm = std::fs::metadata(&p).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perm, 0o755);
        std::fs::set_permissions(&p, perm).unwrap();
    }
    dir
}

#[test]
fn auto_falls_back_when_the_captured_terminal_is_no_longer_installed() {
    // The session was captured in Ghostty; Ghostty has since been removed and
    // Kitty is what this machine has. `auto` means "whatever is here", and the
    // README promises exactly this fallback — but the captured class was taken
    // as an answer without ever asking whether the binary exists, so every
    // placement failed to spawn `ghostty` and no window came back at all.
    let dir = path_with(&["kitty"]);
    let path = std::ffi::OsString::from(dir.path());
    assert_eq!(
        terminal::choose_on("auto", "ghostty", Some(&path)),
        Some(Kind::Kitty),
        "`auto` chose a terminal that is not installed"
    );
}

#[test]
fn auto_still_prefers_the_captured_terminal_when_it_is_installed() {
    // The control. A machine where nothing has changed must go on opening the
    // terminal the session was captured in, not the first of the search order.
    let dir = path_with(&["ghostty", "kitty"]);
    let path = std::ffi::OsString::from(dir.path());
    assert_eq!(
        terminal::choose_on("auto", "kitty", Some(&path)),
        Some(Kind::Kitty),
        "`auto` stopped following the capture"
    );
}

#[test]
fn a_captured_terminal_that_cannot_be_executed_is_not_installed() {
    // A file of the right name with no execute bit is not a terminal, and
    // spawning it fails exactly as a missing one does.
    let dir = path_with(&["foot"]);
    std::fs::write(dir.path().join("ghostty"), "#!/bin/sh\n").unwrap();
    let path = std::ffi::OsString::from(dir.path());
    assert_eq!(
        terminal::choose_on("auto", "ghostty", Some(&path)),
        Some(Kind::Foot)
    );
}

#[test]
fn an_explicitly_configured_terminal_is_never_substituted() {
    // The counterpart to the fallback, and deliberate: a user who named a
    // terminal gets that terminal, or a spawn failure that says so — never
    // something they did not ask for, opened in place of it.
    let dir = path_with(&["kitty"]);
    let path = std::ffi::OsString::from(dir.path());
    assert_eq!(
        terminal::choose_on("ghostty", "ghostty", Some(&path)),
        Some(Kind::Ghostty),
        "an explicit restore.terminal was silently replaced"
    );
}

#[test]
fn auto_with_no_terminal_installed_at_all_names_none() {
    let dir = path_with(&[]);
    let path = std::ffi::OsString::from(dir.path());
    assert_eq!(terminal::choose_on("auto", "ghostty", Some(&path)), None);
}
