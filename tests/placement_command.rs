//! Placement, driven the way a user drives it: `osm snapshot`, then
//! `osm restore`, through the installed binary.
//!
//! # Why this suite exists at all
//!
//! Every helper in the placement path had a passing unit test while the
//! feature did nothing in production: `collect_with_desktop` had no caller, so
//! no real capture ever recorded a placement, and `run_restore` never called
//! `place_windows`, so a placement that somehow existed was never applied.
//! Forty-nine green tests said otherwise because every one of them called the
//! orphaned helpers directly. Plan 3 shipped the identical failure and it was
//! caught only by a test that went through the real command. So this one does
//! too, and asserts on what the outside world can see: the rows in the
//! database, the JSON the command prints, and the calls the compositor and
//! the terminal actually received.
//!
//! # Nothing here touches the machine it runs on
//!
//! * tmux is addressed only through `-L <socket>` carrying this process's id,
//!   built by `Tmux` itself — including the long-lived control-mode client,
//!   which goes through `Tmux::spawn` rather than a hand-built command line.
//!   The binary is built `--no-default-features`, where the socket-less
//!   constructor does not exist at all.
//! * `hyprctl` is a stub script on this test's `PATH`. It reads fixtures from
//!   a temporary directory and appends every dispatch to a log. It cannot
//!   reach a compositor: it is a shell script that echoes.
//! * The terminal is a stub script too. It records its argv, announces its
//!   own pid so the placement pass can find "its window", and sleeps. It
//!   never runs the attach command it was handed — a test that execs a real
//!   terminal puts a window on the developer's desktop, which is exactly what
//!   happened once.

mod common;

use osm::tmux::Tmux;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

struct Env {
    dir: tempfile::TempDir,
    socket: String,
}

/// Everything this suite starts is torn down from [`Drop`], not from lines at
/// the end of a test: an assertion that fires early would otherwise leave a
/// live tmux server, its socket, and a sleeping stub terminal behind — in the
/// same directory and on the same machine as the developer's own.
impl Drop for Env {
    fn drop(&mut self) {
        common::shutdown(&self.tmux());
        self.kill_stub_terminals();
    }
}

impl Env {
    fn new(label: &str) -> Self {
        let env = Env {
            dir: tempfile::tempdir().unwrap(),
            socket: format!("osm-place-{}-{}", label, std::process::id()),
        };
        std::fs::create_dir_all(env.desktop()).unwrap();
        std::fs::write(
            env.desktop().join("monitors.json"),
            r#"[{"id":0,"name":"DP-1","description":"Dell Inc. AW3423DWF 8CM42S3",
                "make":"Dell","model":"AW3423DWF","serial":"8CM42S3",
                "x":0,"y":0,"width":3440,"height":1440,"scale":1.0,
                "transform":0,"focused":true}]"#,
        )
        .unwrap();
        // The workspace `DP-1` is showing right now. A window that maps
        // without being told otherwise appears here, and so does one that is
        // moved to the monitor by name — see the dispatch half of the stub.
        std::fs::write(env.desktop().join("active_ws"), "2").unwrap();
        env.write_stubs();
        env
    }

    /// Where the compositor and terminal stubs keep their state.
    fn desktop(&self) -> PathBuf {
        self.dir.path().join("desktop")
    }

    fn bin(&self) -> PathBuf {
        self.dir.path().join("bin")
    }

    fn state_dir(&self) -> PathBuf {
        self.dir.path().join("state/osm")
    }

    fn db(&self) -> PathBuf {
        self.state_dir().join("state.db")
    }

    fn write_config(&self, body: &str) {
        let dir = self.dir.path().join("config/osm");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.toml"), body).unwrap();
    }

    /// The pids the stub compositor reports windows for, one per line.
    ///
    /// A window is invented for each, on `DP-1` and on workspace 3 unless
    /// something has moved it since. That is the whole of what placement
    /// reads from a window, and building them from pids is what lets the
    /// same stub serve both halves of the test: during capture the pid is a
    /// tmux client's, during restore it is the terminal the restore itself
    /// started.
    fn set_window_pids(&self, pids: &[u32]) {
        let body: String = pids
            .iter()
            .map(|p| format!("{p}\n"))
            .collect::<Vec<_>>()
            .concat();
        std::fs::write(self.desktop().join("pids"), body).unwrap();
    }

    /// The workspace the stub compositor currently has the window for `pid`
    /// on — the desktop's own answer, not a dispatch osm believes it made.
    ///
    /// The distinction is the whole subject of this file's placement tests:
    /// `hyprctl dispatch` answers `ok` for a call it accepted, which is not
    /// the same claim as "the window is there".
    fn window_workspace(&self, pid: u32) -> String {
        std::fs::read_to_string(self.desktop().join(format!("ws.{pid}")))
            .unwrap_or_else(|_| "3".to_string())
            .trim()
            .to_string()
    }

    /// The pid of the terminal the restore's `ghostty` stub ran as, i.e. the
    /// window osm placed.
    fn restored_terminal_pid(&self) -> u32 {
        read_lines(&self.desktop().join("terminals"))
            .last()
            .and_then(|l| l.trim().parse().ok())
            .expect("the restore started a stub terminal")
    }

    /// Replace the compositor stub with one that fails every call, the way
    /// `hyprctl` behaves when Hyprland has died or was never there.
    fn break_the_compositor(&self) {
        write_executable(
            &self.bin().join("hyprctl"),
            "#!/bin/sh\necho 'could not connect to the Hyprland socket' >&2\nexit 1\n",
        );
    }

    /// Every snapshot id in the database, oldest first.
    fn snapshot_ids(&self) -> Vec<i64> {
        let conn = osm::db::open(&self.db()).unwrap();
        let mut stmt = conn
            .prepare("SELECT id FROM snapshots ORDER BY id")
            .unwrap();
        let ids = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<Vec<i64>, _>>()
            .unwrap();
        ids
    }

    fn dispatches(&self) -> Vec<String> {
        read_lines(&self.desktop().join("dispatch.log"))
    }

    fn spawned_argv(&self) -> Vec<String> {
        read_lines(&self.desktop().join("spawn.log"))
    }

    fn write_stubs(&self) {
        std::fs::create_dir_all(self.bin()).unwrap();

        // `hyprctl`. Answers `-j clients` from the pid list, `-j monitors`
        // from a fixture, and records every dispatch, acknowledging it with
        // the `ok` a real Hyprland 0.56 prints.
        //
        // # It moves the window, because a stub that only says `ok` proves
        // # nothing
        //
        // The earlier version of this stub logged each dispatch, answered
        // `ok`, and went on reporting every window on workspace 3 whatever it
        // had been asked to do. Every test in this file passed against it
        // while, on the maintainer's real desktop, both restored terminals
        // came back on the active workspace instead of the two they were
        // captured on. A compositor fake that cannot disagree with osm is not
        // a test of placement.
        //
        // So this one keeps each window's workspace and applies the two
        // dispatches placement makes, with the semantics Hyprland 0.56.2
        // really has (verified by hand against the live compositor):
        //
        //   * `window.move({window=…, workspace='N'})` puts the window on
        //     workspace N.
        //   * `window.move({window=…, monitor='M'})` puts the window on
        //     **M's active workspace** — it does not keep the workspace it
        //     was on. That is the whole bug: osm dispatched the workspace
        //     move and then the monitor move, and the second undid the first.
        write_executable(
            &self.bin().join("hyprctl"),
            r#"#!/bin/bash
# Stand-in for hyprctl. See tests/placement_command.rs.
dir="$OSM_TEST_HYPR_DIR"
ws_of() {  # the workspace window $1 is on; 3 unless something moved it
  if [ -s "$dir/ws.$1" ]; then cat "$dir/ws.$1"; else echo 3; fi
}
if [ "$1" = "-j" ] && [ "$2" = "clients" ]; then
  # A test may need a window to be gone by the time the *publication* reads
  # the desktop back. `vanish_after_dispatch` holds the pids to drop. It is
  # acted on once, after the reads placement itself makes: `vanish_skip`
  # counts those down, and holds 1 because placement re-reads the desktop
  # once to confirm the window really moved before it reports `placed`.
  # Inert for every other test, none of which writes either file.
  if [ -s "$dir/vanish_after_dispatch" ] && [ -s "$dir/dispatch.log" ]; then
    skip=0
    [ -s "$dir/vanish_skip" ] && skip=$(cat "$dir/vanish_skip")
    if [ "$skip" -gt 0 ]; then
      echo $((skip - 1)) > "$dir/vanish_skip"
    else
      grep -vxF -f "$dir/vanish_after_dispatch" "$dir/pids" > "$dir/pids.next" || true
      mv "$dir/pids.next" "$dir/pids"
      mv "$dir/vanish_after_dispatch" "$dir/vanished"
    fi
  fi
  out="["
  first=1
  if [ -s "$dir/pids" ]; then
    while read -r pid; do
      [ -n "$pid" ] || continue
      [ "$first" -eq 1 ] || out="$out,"
      first=0
      ws="$(ws_of "$pid")"
      out="$out{\"address\":\"0x$pid\",\"pid\":$pid,\"class\":\"com.mitchellh.ghostty\",\"title\":\"omarchy:lies\",\"workspace\":{\"id\":$ws,\"name\":\"$ws\"},\"monitor\":0,\"at\":[344,144],\"size\":[1720,720],\"floating\":false}"
    done < "$dir/pids"
  fi
  echo "$out]"
  exit 0
fi
if [ "$1" = "-j" ] && [ "$2" = "monitors" ]; then
  cat "$dir/monitors.json"
  exit 0
fi
if [ "$1" = "dispatch" ]; then
  shift
  printf '%s\n' "$*" >> "$dir/dispatch.log"
  arg="$*"
  pid="$(printf '%s' "$arg" | sed -n "s/.*address:0x\([0-9][0-9]*\).*/\1/p")"
  if [ -n "$pid" ]; then
    want="$(printf '%s' "$arg" | sed -n "s/.*workspace='\([^']*\)'.*/\1/p")"
    if [ -n "$want" ]; then
      printf '%s' "$want" > "$dir/ws.$pid"
    elif printf '%s' "$arg" | grep -q "monitor='"; then
      # Hyprland 0.56.2: sending a window to a monitor lands it on that
      # monitor's *active* workspace, wherever it was before.
      printf '%s' "$(cat "$dir/active_ws")" > "$dir/ws.$pid"
    fi
  fi
  echo ok
  exit 0
fi
echo "unexpected hyprctl invocation: $*" >&2
exit 1
"#,
        );

        // The terminal. Records what it was asked to run, announces itself as
        // a window, attaches to the session it was pointed at, and waits.
        //
        // It attaches **in control mode**, and that is the only liberty it
        // takes with the command line it was handed: a test has no tty, so a
        // plain `tmux attach-session` would fail with "not a terminal" and
        // this stub would prove nothing about the attach. Everything else —
        // the `-L` socket, the session name — is used exactly as osm wrote
        // it, and the full argv is logged for the assertions that check it.
        //
        // The attach is not decoration. A window maps and accepts dispatches
        // before the shell inside it has attached, so a stub that only
        // announced a window let `Placed` be reported for a terminal that
        // never held the session — and the snapshot published afterwards then
        // recorded no window for it at all.
        write_executable(
            &self.bin().join("ghostty"),
            r#"#!/bin/bash
# Stand-in for a terminal. See tests/placement_command.rs.
dir="$OSM_TEST_HYPR_DIR"
printf '%s\n' "$*" >> "$dir/spawn.log"
echo "$$" >> "$dir/terminals"
# A terminal maps on whatever workspace is showing, never on the one it is
# eventually meant to live on. Announcing the window already on its target
# workspace is what let a placement that never moved anything look correct.
cat "$dir/active_ws" > "$dir/ws.$$"
echo "$$" >> "$dir/pids"
attach="${@: -1}"
# A control-mode client reads its commands from stdin, and osm gives a
# terminal it spawns /dev/null: the client would see EOF and detach the
# instant it attached. So it gets a fifo this script holds the write end of —
# which also means that when this script is killed, the client detaches and
# exits with it, leaving nothing running on the machine the test ran on.
fifo="$dir/attach.$$"
mkfifo "$fifo"
bash -c "${attach/ attach-session/ -C attach-session}" <"$fifo" >/dev/null 2>&1 &
exec 3>"$fifo"
# Deliberately not `exec`: the process that stays alive keeps this script's
# path in its command line, which is how cleanup proves a pid is one of ours
# before it signals it.
sleep 30
"#,
        );
    }

    fn osm(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_osm"));
        let path = format!(
            "{}:{}",
            self.bin().display(),
            std::env::var("PATH").unwrap_or_default()
        );
        cmd.env("PATH", path)
            .env("HOME", self.dir.path())
            .env("XDG_STATE_HOME", self.dir.path().join("state"))
            .env("XDG_CONFIG_HOME", self.dir.path().join("config"))
            .env("OSM_TEST_HYPR_DIR", self.desktop())
            .arg("--socket")
            .arg(&self.socket);
        cmd
    }

    fn tmux(&self) -> Tmux {
        Tmux::with_socket(&self.socket)
    }

    /// Kills the stub terminals this test started, and nothing else.
    ///
    /// A recorded pid number is not on its own a licence to signal: by the
    /// time cleanup runs the process may be long gone and the number reused
    /// by something the developer is using. So each pid is checked against
    /// its own `/proc/<pid>/cmdline`, which for a stub still running names a
    /// script inside this test's own temporary directory and can name
    /// nothing else — the directory, rather than one script in it, because
    /// a test may start more than one kind of stub terminal.
    fn kill_stub_terminals(&self) {
        let mine = self.bin().display().to_string();
        for line in read_lines(&self.desktop().join("terminals")) {
            let Ok(pid) = line.trim().parse::<u32>() else {
                continue;
            };
            let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
                continue;
            };
            if !String::from_utf8_lossy(&cmdline).contains(&mine) {
                continue;
            }
            let _ = Command::new("bash")
                .arg("-c")
                .arg(format!("kill {pid} 2>/dev/null || true"))
                .status();
        }
    }

    fn run(&self, args: &[&str]) -> (Value, std::process::Output) {
        let out = self.osm().args(args).output().unwrap();
        let v: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
            panic!(
                "`osm {}` did not print JSON ({e}): stdout={:?} stderr={:?}",
                args.join(" "),
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )
        });
        (v, out)
    }
}

fn read_lines(p: &Path) -> Vec<String> {
    std::fs::read_to_string(p)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

fn write_executable(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    let mut perm = std::fs::metadata(path).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perm, 0o755);
    std::fs::set_permissions(path, perm).unwrap();
}

/// A control-mode tmux client attached to `session`, so the server really has
/// a client with a pid for placement to walk up from.
///
/// Control mode is what makes this possible without a terminal: `tmux -C
/// attach` needs no tty, and appears in `list-clients` exactly like any other
/// client. The pid it runs under is a child of this test process, so the
/// stub compositor can report a "window" owning it and the ancestor walk
/// succeeds the way it does for a real terminal.
struct Client(std::process::Child);

impl Client {
    fn attach(t: &Tmux, session: &str) -> Client {
        Client(
            t.spawn(&["-C", "attach-session", "-t", session])
                .expect("a control-mode tmux client"),
        )
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Waits until the server reports `n` attached clients, and returns them.
fn wait_for_clients(t: &Tmux, n: usize) -> Vec<osm::desktop::ClientPid> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if let Ok(raw) = t.run(&["list-clients", "-F", "#{client_pid} #{client_session}"]) {
            if let Ok(cs) = osm::desktop::parse_clients_output(&raw) {
                if cs.len() == n {
                    return cs;
                }
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "tmux never reported {n} attached client(s)"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

#[test]
fn snapshot_records_placement_and_restore_puts_the_window_back() {
    let env = Env::new("roundtrip");
    // A short readiness budget: the stub terminal announces itself at once,
    // and nothing here should ever wait out a real 30s boot allowance.
    env.write_config(
        "[restore]\nreadiness_timeout_secs = 10\nterminal = \"ghostty\"\n\
         [agents]\nauto_resume = false\n",
    );

    let t = env.tmux();
    t.run(&["new-session", "-d", "-s", "dev", "-n", "code", "-c", "/tmp"])
        .unwrap();
    let client = Client::attach(&t, "dev");
    let clients = wait_for_clients(&t, 1);
    assert_eq!(clients[0].session, "dev");

    // The compositor sees one window, and the tmux client attached to `dev`
    // is inside it.
    env.set_window_pids(&[clients[0].pid]);

    // ---- capture, through the command ------------------------------------
    let (snap_json, out) = env.run(&["snapshot", "--reason", "test"]);
    assert!(
        out.status.success(),
        "{:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    let snap = snap_json["snapshot_id"].as_i64().expect("a snapshot id");

    let conn = osm::db::open(&env.db()).unwrap();
    let recorded = osm::desktop::placements_of(&conn, snap).unwrap();
    assert_eq!(
        recorded.len(),
        1,
        "an ordinary `osm snapshot` recorded no placement at all: {recorded:?}"
    );
    let p = &recorded[0];
    assert_eq!(p.session, "dev");
    assert_eq!(p.workspace_kind, "numbered");
    assert_eq!(p.workspace_ref, "3");
    assert_eq!(p.monitor_connector, "DP-1");
    assert_eq!(
        p.monitor_desc.as_deref(),
        Some("Dell Inc. AW3423DWF 8CM42S3")
    );
    assert_eq!(p.terminal_kind, "ghostty");
    drop(conn);

    // ---- a reboot --------------------------------------------------------
    let conn = osm::db::open(&env.db()).unwrap();
    conn.execute(
        "UPDATE snapshots SET boot_id='boot-previous' WHERE id=?1",
        [snap],
    )
    .unwrap();
    drop(conn);
    drop(client);
    common::shutdown(&t);
    // The stub terminal from the capture half is not ours to leave running
    // once its window is gone from the fixture.
    env.kill_stub_terminals();
    // The compositor is empty again: the terminal that held `dev` died with
    // the session it was showing.
    env.set_window_pids(&[]);
    let _ = std::fs::remove_file(env.desktop().join("dispatch.log"));

    // ---- restore, through the command ------------------------------------
    let (restore_json, out) = env.run(&["restore"]);
    assert_eq!(
        restore_json["state"],
        "succeeded",
        "{restore_json}\nstderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let windows = restore_json["windows"].as_array().expect("windows[]");
    assert_eq!(
        windows.len(),
        1,
        "the restore reported no window work at all: {restore_json}"
    );
    assert_eq!(windows[0]["session"], "dev");
    assert_eq!(
        windows[0]["outcome"],
        "placed",
        "{restore_json}\nstderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The terminal was started, on *this* restore's tmux server.
    let spawned = env.spawned_argv();
    assert_eq!(spawned.len(), 1, "{spawned:?}");
    assert!(
        spawned[0].contains(&format!(
            "tmux -u -L '{}' attach-session -t 'dev'",
            env.socket
        )),
        "the terminal was not pointed at this restore's server: {spawned:?}"
    );

    // And it really is on the workspace the capture recorded — asked of the
    // desktop, not of the dispatch log.
    //
    // The log is what this used to assert on, and it is exactly the wrong
    // question: `hyprctl dispatch` answers `ok` for a call it accepted, and
    // on the maintainer's machine both restored terminals came back on the
    // active workspace with every dispatch acknowledged. "osm asked" and
    // "the window is there" are different claims, and only the second one is
    // what `placed` may mean.
    let dispatched = env.dispatches();
    assert!(
        dispatched.iter().any(|d| d.contains("workspace='3'")),
        "the window was never moved to its workspace: {dispatched:?}"
    );
    let terminal = env.restored_terminal_pid();
    assert_eq!(
        env.window_workspace(terminal),
        "3",
        "the window osm reported as placed is not on the workspace it was \
         captured on; dispatches: {dispatched:?}"
    );

    // ---- and the next reboot has something to place ----------------------
    //
    // The half that made the whole feature undo itself once per boot. A
    // successful restore publishes a replacement snapshot and retires the
    // source; the publication wrote the tmux half only, so the machine was
    // left with a `complete` snapshot holding no `terminal_windows` at all
    // and the only record of where this window belonged had just been
    // retired. Asserting the restore said `placed` cannot see that — only the
    // rows the command left in the database can.
    let conn = osm::db::open(&env.db()).unwrap();
    let published: i64 = conn
        .query_row(
            "SELECT id FROM snapshots WHERE reason = 'post_restore' ORDER BY id DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .expect("the restore published a snapshot for this boot");
    assert_ne!(published, snap, "the published snapshot is a new one");
    let after = osm::desktop::placements_of(&conn, published).unwrap();
    assert_eq!(
        after.len(),
        1,
        "the snapshot this restore published records no terminal window, so the \
         next reboot has no placement to restore: {after:?}"
    );
    assert_eq!(after[0].session, "dev");
    assert_eq!(after[0].workspace_ref, "3");
    assert_eq!(after[0].monitor_connector, "DP-1");

    // And the source really was retired on the strength of it.
    let source_state: String = conn
        .query_row("SELECT state FROM snapshots WHERE id = ?1", [snap], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(source_state, "restored");
    drop(conn);
}

#[test]
fn a_terminal_that_never_attaches_is_not_a_placed_window() {
    // `Placed` used to mean "a window osm started appeared and took its
    // dispatches", which a terminal that never runs `tmux attach-session` —
    // or whose attach fails — satisfies exactly as well as one that did. The
    // publication then found no terminal window for the session, wrote a
    // snapshot saying so, and retired the source that knew where the window
    // belonged, all while reporting a success.
    //
    // The stub here is the one this suite used to use everywhere: it maps a
    // window and never attaches.
    let env = Env::new("noattach");
    env.write_config(
        "[restore]\nreadiness_timeout_secs = 2\nterminal = \"ghostty\"\n\
         [agents]\nauto_resume = false\n",
    );

    let t = env.tmux();
    t.run(&["new-session", "-d", "-s", "dev", "-n", "code", "-c", "/tmp"])
        .unwrap();
    let client = Client::attach(&t, "dev");
    let clients = wait_for_clients(&t, 1);
    env.set_window_pids(&[clients[0].pid]);

    let (snap_json, out) = env.run(&["snapshot", "--reason", "test"]);
    assert!(
        out.status.success(),
        "{:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    let snap = snap_json["snapshot_id"].as_i64().unwrap();

    let conn = osm::db::open(&env.db()).unwrap();
    conn.execute(
        "UPDATE snapshots SET boot_id='boot-previous' WHERE id=?1",
        [snap],
    )
    .unwrap();
    drop(conn);
    drop(client);
    common::shutdown(&t);
    env.kill_stub_terminals();
    env.set_window_pids(&[]);

    // A terminal that opens a window and holds no session in it.
    write_executable(
        &env.bin().join("ghostty"),
        r#"#!/bin/bash
dir="$OSM_TEST_HYPR_DIR"
printf '%s\n' "$*" >> "$dir/spawn.log"
echo "$$" >> "$dir/terminals"
echo "$$" >> "$dir/pids"
sleep 10
"#,
    );

    let (restore_json, out) = env.run(&["restore"]);
    assert_eq!(
        restore_json["windows"][0]["outcome"],
        "never_attached",
        "a terminal holding no session was reported as a placed window: \
         {restore_json}\nstderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        restore_json["state"],
        "partial",
        "{restore_json}\nstderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(restore_json["retryable"], true);
    assert_ne!(out.status.code(), Some(0));

    // The point of all of it: the record of where that window was is still
    // selectable, rather than retired against work nobody did.
    let conn = osm::db::open(&env.db()).unwrap();
    let state: String = conn
        .query_row("SELECT state FROM snapshots WHERE id = ?1", [snap], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        state, "complete",
        "the snapshot holding this window's placement was retired anyway"
    );
    assert_eq!(
        osm::desktop::placements_of(&conn, snap).unwrap().len(),
        1,
        "and its placement row must still be there"
    );
    drop(conn);

    common::shutdown(&env.tmux());
}

#[test]
fn a_restore_whose_window_never_appears_keeps_the_snapshot_restorable() {
    // The other half of the contract: placement that did not happen must not
    // be reported as a success, and must not retire the only record of where
    // the window was. The stub terminal here announces no window at all.
    let env = Env::new("nowindow");
    env.write_config(
        "[restore]\nreadiness_timeout_secs = 1\nterminal = \"ghostty\"\n\
         [agents]\nauto_resume = false\n",
    );
    write_executable(
        &env.bin().join("ghostty"),
        "#!/bin/bash\n         printf '%s\\n' \"$*\" >> \"$OSM_TEST_HYPR_DIR/spawn.log\"\n         echo \"$$\" >> \"$OSM_TEST_HYPR_DIR/terminals\"\n         sleep 5\n",
    );

    let t = env.tmux();
    t.run(&["new-session", "-d", "-s", "dev", "-n", "code", "-c", "/tmp"])
        .unwrap();
    let client = Client::attach(&t, "dev");
    let clients = wait_for_clients(&t, 1);
    env.set_window_pids(&[clients[0].pid]);

    let (snap_json, _) = env.run(&["snapshot", "--reason", "test"]);
    let snap = snap_json["snapshot_id"].as_i64().unwrap();

    let conn = osm::db::open(&env.db()).unwrap();
    conn.execute(
        "UPDATE snapshots SET boot_id='boot-previous' WHERE id=?1",
        [snap],
    )
    .unwrap();
    drop(conn);
    drop(client);
    common::shutdown(&t);
    env.set_window_pids(&[]);

    let (restore_json, out) = env.run(&["restore"]);
    assert_eq!(
        restore_json["state"],
        "partial",
        "a window that never came back was reported as a complete restore: \
         {restore_json}\nstderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(restore_json["windows"][0]["outcome"], "never_mapped");
    assert_eq!(
        restore_json["retryable"], true,
        "the snapshot that records where the window was must stay selectable"
    );
    assert_ne!(
        out.status.code(),
        Some(0),
        "a partial restore exits non-zero"
    );

    common::shutdown(&env.tmux());
}

/// A window that is not the one this restore placed cannot stand in for it.
///
/// The user already has a terminal of their own attached to `dev` — opened by
/// hand, on a workspace of their choosing, none of osm's business. osm spawns
/// its own terminal, moves it, sees a client of *that* terminal attached to
/// the session, and reports `placed`: every word of it true at the moment it
/// was said. The terminal then exits before the publication reads the desktop
/// back, so the snapshot published afterwards records the *user's* window for
/// `dev` and nothing of osm's.
///
/// A check that asks only whether the session has some window in the
/// published snapshot finds one, calls the restore clean, and retires the
/// source — the only record of where the window osm failed to deliver
/// belonged. The claim was about one window, so the check has to be about
/// that window.
#[test]
fn a_window_this_restore_did_not_place_does_not_stand_in_for_one_it_did() {
    let env = Env::new("competing");
    env.write_config(
        "[restore]\nreadiness_timeout_secs = 10\nterminal = \"ghostty\"\n\
         [agents]\nauto_resume = false\n",
    );
    // The user's terminal has to reach the same server the restore builds on,
    // and nothing else: `-L <socket>` is the whole of its addressing, handed
    // to it in a file rather than guessed.
    std::fs::write(env.desktop().join("socket"), &env.socket).unwrap();

    let t = env.tmux();
    t.run(&["new-session", "-d", "-s", "dev", "-n", "code", "-c", "/tmp"])
        .unwrap();
    let client = Client::attach(&t, "dev");
    let clients = wait_for_clients(&t, 1);
    env.set_window_pids(&[clients[0].pid]);

    let (snap_json, out) = env.run(&["snapshot", "--reason", "test"]);
    assert!(
        out.status.success(),
        "{:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    let snap = snap_json["snapshot_id"].as_i64().expect("a snapshot id");

    // ---- a reboot --------------------------------------------------------
    let conn = osm::db::open(&env.db()).unwrap();
    conn.execute(
        "UPDATE snapshots SET boot_id='boot-previous' WHERE id=?1",
        [snap],
    )
    .unwrap();
    drop(conn);
    drop(client);
    common::shutdown(&t);
    env.kill_stub_terminals();
    env.set_window_pids(&[]);

    // The user's own terminal for `dev`. It waits for the session to exist,
    // attaches to it on the restore's own server, announces its window only
    // once the server really holds its client, and stays.
    //
    // Started by this test, and deliberately not by the stub terminal osm
    // spawns: a child of that terminal is in its process tree, and the
    // placement pass would then be right to claim this window as the one it
    // started — which is the opposite of the situation under test.
    write_executable(
        &env.bin().join("competing"),
        r#"#!/bin/bash
# The user's own terminal. See tests/placement_command.rs.
dir="$OSM_TEST_HYPR_DIR"
sock="$(cat "$dir/socket")"
until tmux -u -L "$sock" has-session -t dev 2>/dev/null; do sleep 0.05; done
fifo="$dir/competing.fifo"
mkfifo "$fifo"
tmux -u -L "$sock" -C attach-session -t dev <"$fifo" >/dev/null 2>&1 &
exec 3>"$fifo"
until tmux -u -L "$sock" list-clients -F '#{client_session}' 2>/dev/null \
    | grep -qx dev; do
  sleep 0.05
done
echo "$$" >> "$dir/terminals"
echo "$$" >> "$dir/pids"
echo "$$" > "$dir/competing.pid"
touch "$dir/competing.ready"
sleep 60
"#,
    );

    // osm's terminal. It really does attach — so `placed` is the honest
    // report — and then goes away before the publication reads the desktop
    // back, which is the whole scenario.
    write_executable(
        &env.bin().join("ghostty"),
        r#"#!/bin/bash
# osm's terminal, which does its job and then exits. See
# tests/placement_command.rs.
dir="$OSM_TEST_HYPR_DIR"
printf '%s\n' "$*" >> "$dir/spawn.log"
echo "$$" >> "$dir/terminals"
# The user's terminal is already coming up, started by the test itself, and
# stays: the desktop the publication reads therefore always holds a window
# for this session -- just not this one.
until [ -f "$dir/competing.ready" ]; do sleep 0.05; done
attach="${@: -1}"
fifo="$dir/attach.$$"
mkfifo "$fifo"
bash -c "${attach/ attach-session/ -C attach-session}" <"$fifo" >/dev/null 2>&1 &
exec 3>"$fifo"
# Ask to be dropped from the desktop on the publication's read of it, and
# announce the window only afterwards, so the request can never arrive later
# than the read it is meant for. `vanish_skip` steps over the one read
# placement makes for itself after dispatching -- the confirmation that the
# window really moved -- so the window is still there for that and gone for
# the publication, which is the scenario.
echo "$$" > "$dir/vanish_after_dispatch"
echo 1 > "$dir/vanish_skip"
cat "$dir/active_ws" > "$dir/ws.$$"
echo "$$" >> "$dir/pids"
until [ -f "$dir/vanished" ]; do sleep 0.05; done
"#,
    );

    // The user opens their terminal. It blocks until the restore has created
    // `dev`, so it is attached and on the desktop before osm's own terminal
    // announces itself -- and it is this process's child, so nothing in it
    // can be mistaken for a window this restore started. Killed by `Env`'s
    // teardown along with every other stub terminal.
    let mut competing = Command::new(env.bin().join("competing"))
        .env("OSM_TEST_HYPR_DIR", env.desktop())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("the user's terminal");

    // ---- restore, through the command ------------------------------------
    let (restore_json, out) = env.run(&["restore"]);
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let _ = competing.kill();
    let _ = competing.wait();

    // The placement pass itself is not wrong: when it looked, its own
    // terminal was mapped, moved and attached.
    assert_eq!(
        restore_json["windows"][0]["outcome"], "placed",
        "{restore_json}\nstderr: {stderr}"
    );

    // What the publication wrote down, though, is the user's window — and it
    // is the publication the source is retired against.
    let conn = osm::db::open(&env.db()).unwrap();
    let published: i64 = conn
        .query_row(
            "SELECT id FROM snapshots WHERE reason = 'post_restore' ORDER BY id DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .expect("the restore published a snapshot for this boot");
    let competing_pid = std::fs::read_to_string(env.desktop().join("competing.pid"))
        .expect("the user's terminal never attached, so this test proves nothing");
    let held: Vec<String> = osm::desktop::placements_of(&conn, published)
        .unwrap()
        .into_iter()
        .filter(|p| p.session == "dev")
        .map(|p| p.address)
        .collect();
    assert_eq!(
        held,
        vec![format!("0x{}", competing_pid.trim())],
        "the published snapshot has to hold the user's window for `dev` and no \
         other, or this test is not exercising the finding at all"
    );
    drop(conn);

    // So the window this restore placed is not in the snapshot it published,
    // and the run must say so.
    assert_eq!(
        restore_json["state"], "partial",
        "a restore whose own window is absent from the snapshot it published \
         reported success, because a window the user opened happened to hold \
         the same session: {restore_json}\nstderr: {stderr}"
    );
    assert_eq!(restore_json["retryable"], true);
    assert_ne!(
        out.status.code(),
        Some(0),
        "a restore that did not finish its work exits non-zero"
    );

    // The point of all of it: the record of where osm's window belonged is
    // still selectable, rather than retired against a window osm never placed.
    let conn = osm::db::open(&env.db()).unwrap();
    let state: String = conn
        .query_row("SELECT state FROM snapshots WHERE id = ?1", [snap], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        state, "complete",
        "the snapshot holding this window's placement was retired anyway"
    );
    assert_eq!(
        osm::desktop::placements_of(&conn, snap).unwrap().len(),
        1,
        "and its placement row must still be there"
    );
    drop(conn);

    common::shutdown(&env.tmux());
}

/// Whether the stub terminal `pid` names is still running.
///
/// Proven by that process's own `/proc/<pid>/cmdline`, which for a stub of
/// this suite names a script inside this test's temporary directory and can
/// name nothing else. A bare number would be an answer about whatever has
/// since been given that pid.
fn stub_is_running(env: &Env, pid: u32) -> bool {
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .map(|c| String::from_utf8_lossy(&c).contains(&env.bin().display().to_string()))
        .unwrap_or(false)
}

/// The pids of the stub terminals started so far, oldest first.
fn stub_terminal_pids(env: &Env) -> Vec<u32> {
    read_lines(&env.desktop().join("terminals"))
        .iter()
        .filter_map(|l| l.trim().parse().ok())
        .collect()
}

/// A terminal that never attaches is taken back, not left on the desktop.
///
/// `never_attached` keeps the source snapshot retryable, which is right — and
/// every later restore spawns a terminal unconditionally, which is also
/// right, because a window osm has stopped tracking cannot be adopted by a
/// process-ancestry check that only its own attempt could satisfy. Leaving
/// the first one running therefore means the next attempt puts a second empty
/// terminal on the user's desktop, and the one after that a third. On the
/// maintainer's own machine that is one orphan window per retry.
#[test]
fn a_terminal_that_never_attaches_is_taken_back() {
    let env = Env::new("orphan");
    env.write_config(
        "[restore]\nreadiness_timeout_secs = 2\nterminal = \"ghostty\"\n\
         [agents]\nauto_resume = false\n",
    );

    let t = env.tmux();
    t.run(&["new-session", "-d", "-s", "dev", "-n", "code", "-c", "/tmp"])
        .unwrap();
    let client = Client::attach(&t, "dev");
    let clients = wait_for_clients(&t, 1);
    env.set_window_pids(&[clients[0].pid]);

    let (snap_json, out) = env.run(&["snapshot", "--reason", "test"]);
    assert!(
        out.status.success(),
        "{:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    let snap = snap_json["snapshot_id"].as_i64().unwrap();

    let conn = osm::db::open(&env.db()).unwrap();
    conn.execute(
        "UPDATE snapshots SET boot_id='boot-previous' WHERE id=?1",
        [snap],
    )
    .unwrap();
    drop(conn);
    drop(client);
    common::shutdown(&t);
    env.kill_stub_terminals();
    env.set_window_pids(&[]);
    let _ = std::fs::remove_file(env.desktop().join("terminals"));

    // A terminal that maps a window, holds no session, and would happily sit
    // there for the rest of the day.
    write_executable(
        &env.bin().join("ghostty"),
        r#"#!/bin/bash
dir="$OSM_TEST_HYPR_DIR"
printf '%s\n' "$*" >> "$dir/spawn.log"
echo "$$" >> "$dir/terminals"
echo "$$" >> "$dir/pids"
sleep 60
"#,
    );

    let (restore_json, out) = env.run(&["restore"]);
    assert_eq!(
        restore_json["windows"][0]["outcome"],
        "never_attached",
        "{restore_json}\nstderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let started = stub_terminal_pids(&env);
    assert_eq!(started.len(), 1, "{started:?}");
    assert!(
        !stub_is_running(&env, started[0]),
        "the terminal osm started never attached to anything and was left \
         running on the user's desktop, untracked: the next retry spawns \
         another beside it"
    );

    // Taking the terminal back does not make the work done: the source is
    // still the only record of where that window belonged.
    let conn = osm::db::open(&env.db()).unwrap();
    let state: String = conn
        .query_row("SELECT state FROM snapshots WHERE id = ?1", [snap], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(state, "complete");
    drop(conn);

    common::shutdown(&env.tmux());
}

/// A terminal of ours that is attached to *another* session is not taken back.
///
/// `Attach::No` used to mean "no client of ours is attached to the session we
/// asked about", and the kill was authorised on it. A `client-attached` hook
/// that switches the spawned client — `switch-client`, a session picker, a
/// wrapper that lands the user somewhere else — leaves a terminal of ours
/// alive and showing the user a session, and the combined
/// session-and-ownership predicate saw none of it: at the timeout osm killed
/// the terminal the user was looking at, and the shell and agents inside it.
///
/// The placement is still unfinished — the session it was spawned for has no
/// window — so the restore stays retryable. What it must not do is end a
/// process the user can see.
#[test]
fn a_terminal_whose_client_holds_another_session_is_not_taken_back() {
    let env = Env::new("switched");
    env.write_config(
        "[restore]\nreadiness_timeout_secs = 2\nterminal = \"ghostty\"\n\
         [agents]\nauto_resume = false\n",
    );
    // The stub reaches this test's own server and no other: the name is
    // handed to it in a file rather than guessed.
    std::fs::write(env.desktop().join("socket"), &env.socket).unwrap();

    let t = env.tmux();
    t.run(&["new-session", "-d", "-s", "dev", "-n", "code", "-c", "/tmp"])
        .unwrap();
    let client = Client::attach(&t, "dev");
    let clients = wait_for_clients(&t, 1);
    env.set_window_pids(&[clients[0].pid]);

    let (snap_json, out) = env.run(&["snapshot", "--reason", "test"]);
    assert!(
        out.status.success(),
        "{:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    let snap = snap_json["snapshot_id"].as_i64().unwrap();

    // ---- a reboot --------------------------------------------------------
    let conn = osm::db::open(&env.db()).unwrap();
    conn.execute(
        "UPDATE snapshots SET boot_id='boot-previous' WHERE id=?1",
        [snap],
    )
    .unwrap();
    drop(conn);
    drop(client);
    common::shutdown(&t);
    env.kill_stub_terminals();
    env.set_window_pids(&[]);
    let _ = std::fs::remove_file(env.desktop().join("terminals"));

    // osm's terminal, whose client ends up on a different session. It really
    // is attached, and it really is ours — its client is its own child, so
    // the ancestry walk finds this very process — but not to `dev`. The
    // window is announced only once the server confirms the client, so the
    // attach check never looks at a moment before the switch happened.
    write_executable(
        &env.bin().join("ghostty"),
        r#"#!/bin/bash
# osm's terminal, landed on another session. See tests/placement_command.rs.
dir="$OSM_TEST_HYPR_DIR"
printf '%s\n' "$*" >> "$dir/spawn.log"
echo "$$" >> "$dir/terminals"
sock="$(cat "$dir/socket")"
tmux -u -L "$sock" new-session -d -s other -c /tmp 2>/dev/null
fifo="$dir/attach.$$"
mkfifo "$fifo"
tmux -u -L "$sock" -C attach-session -t other <"$fifo" >/dev/null 2>&1 &
exec 3>"$fifo"
until tmux -u -L "$sock" list-clients -F '#{client_session}' 2>/dev/null \
    | grep -qx other; do
  sleep 0.05
done
echo "$$" >> "$dir/pids"
sleep 60
"#,
    );

    let (restore_json, out) = env.run(&["restore"]);
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert_eq!(
        restore_json["windows"][0]["outcome"], "never_attached",
        "{restore_json}\nstderr: {stderr}"
    );

    let started = stub_terminal_pids(&env);
    assert_eq!(started.len(), 1, "{started:?}");
    assert!(
        stub_is_running(&env, started[0]),
        "osm killed a terminal of its own that was attached to another \
         session — the window the user was looking at, and everything \
         running inside it"
    );

    common::shutdown(&env.tmux());
}

/// The attach is given its own interval, counted from when the window mapped.
///
/// Mapping and attaching used to share one deadline, so a terminal that was
/// slow to map — the ordinary case on a cold boot, which is when this runs —
/// was left whatever remained of the budget to attach in, sometimes nothing
/// at all. The restore then reported `never_attached` for a terminal that was
/// perfectly healthy and about to come up.
#[test]
fn the_attach_is_given_its_own_interval_after_the_window_maps() {
    let env = Env::new("slowmap");
    env.write_config(
        "[restore]\nreadiness_timeout_secs = 5\nterminal = \"ghostty\"\n\
         [agents]\nauto_resume = false\n",
    );

    let t = env.tmux();
    t.run(&["new-session", "-d", "-s", "dev", "-n", "code", "-c", "/tmp"])
        .unwrap();
    let client = Client::attach(&t, "dev");
    let clients = wait_for_clients(&t, 1);
    env.set_window_pids(&[clients[0].pid]);

    let (snap_json, out) = env.run(&["snapshot", "--reason", "test"]);
    assert!(
        out.status.success(),
        "{:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    let snap = snap_json["snapshot_id"].as_i64().unwrap();

    let conn = osm::db::open(&env.db()).unwrap();
    conn.execute(
        "UPDATE snapshots SET boot_id='boot-previous' WHERE id=?1",
        [snap],
    )
    .unwrap();
    drop(conn);
    drop(client);
    common::shutdown(&t);
    env.kill_stub_terminals();
    env.set_window_pids(&[]);

    // Three seconds to map, three more before its shell reaches the attach.
    // Neither exceeds the five-second budget; together they exceed it, which
    // is the whole difference between one deadline and two.
    write_executable(
        &env.bin().join("ghostty"),
        r#"#!/bin/bash
dir="$OSM_TEST_HYPR_DIR"
printf '%s\n' "$*" >> "$dir/spawn.log"
echo "$$" >> "$dir/terminals"
sleep 3
echo "$$" >> "$dir/pids"
sleep 3
attach="${@: -1}"
fifo="$dir/attach.$$"
mkfifo "$fifo"
bash -c "${attach/ attach-session/ -C attach-session}" <"$fifo" >/dev/null 2>&1 &
exec 3>"$fifo"
sleep 30
"#,
    );

    let (restore_json, out) = env.run(&["restore"]);
    assert_eq!(
        restore_json["windows"][0]["outcome"],
        "placed",
        "a terminal that mapped inside the budget and attached inside the \
         budget was reported as one that never attached, because the two \
         shared one deadline: {restore_json}\nstderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    common::shutdown(&env.tmux());
}

/// Everything below is about the capture half alone: what `osm snapshot`
/// does when the compositor cannot be read.

#[test]
fn a_capture_that_cannot_read_placement_fails_and_keeps_the_last_good_layout() {
    // The whole harm in one run. `desktop::placements` returns `None` for a
    // compositor that has stopped answering, a `list-clients` that failed, a
    // reply that did not parse, or an identity that moved — and the capture
    // used to commit a `complete` snapshot with no placement at all, report
    // success, and prune the snapshot that still held the layout out of
    // retention. `keep_snapshots = 1` is not contrived: it is the same
    // deletion every retention setting performs, brought within one capture.
    let env = Env::new("blind");
    env.write_config(
        "[restore]\nterminal = \"ghostty\"\n\
         [capture]\nkeep_snapshots = 1\n\
         [agents]\nauto_resume = false\n",
    );

    let t = env.tmux();
    t.run(&["new-session", "-d", "-s", "dev", "-n", "code", "-c", "/tmp"])
        .unwrap();
    let _client = Client::attach(&t, "dev");
    let clients = wait_for_clients(&t, 1);
    env.set_window_pids(&[clients[0].pid]);

    let (good, out) = env.run(&["snapshot", "--reason", "test"]);
    assert!(
        out.status.success(),
        "{:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    let good_id = good["snapshot_id"].as_i64().expect("a snapshot id");
    {
        let conn = osm::db::open(&env.db()).unwrap();
        assert_eq!(
            osm::desktop::placements_of(&conn, good_id).unwrap().len(),
            1,
            "the good capture recorded no placement to lose"
        );
    }

    // Hyprland dies. tmux is untouched: the sessions are all still there, so
    // nothing about this is "the machine is idle".
    env.break_the_compositor();

    let out = env
        .osm()
        .args(["snapshot", "--reason", "test"])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "a capture that could not read placement reported success: stdout={:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(
        env.snapshot_ids(),
        vec![good_id],
        "the capture that could not see the desktop wrote a snapshot, \
         and retention deleted the one that still held the layout"
    );
    let conn = osm::db::open(&env.db()).unwrap();
    let still = osm::desktop::placements_of(&conn, good_id).unwrap();
    assert_eq!(still.len(), 1, "the last good layout is gone");
    assert_eq!(still[0].session, "dev");
    drop(conn);

    common::shutdown(&env.tmux());
}

#[test]
fn a_machine_with_no_compositor_captures_tmux_happily_when_placement_is_off() {
    // The control for the test above, and the reason the strictness is safe:
    // a headless box says so once, in the config, and every capture after
    // that is an ordinary success with no placement in it. Nothing here can
    // reach a compositor — `hyprctl` fails every call — and the capture must
    // neither wait for one nor mind.
    let env = Env::new("headless");
    env.write_config("[restore]\nplace_windows = false\n[agents]\nauto_resume = false\n");
    env.break_the_compositor();

    let t = env.tmux();
    t.run(&["new-session", "-d", "-s", "dev", "-n", "code", "-c", "/tmp"])
        .unwrap();

    let (v, out) = env.run(&["snapshot", "--reason", "test"]);
    assert!(
        out.status.success(),
        "a headless capture failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let id = v["snapshot_id"].as_i64().expect("a snapshot id");

    let conn = osm::db::open(&env.db()).unwrap();
    assert!(
        osm::desktop::placements_of(&conn, id).unwrap().is_empty(),
        "placement is off; nothing may be recorded"
    );
    let sessions: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM session_rows WHERE snapshot_id = ?1",
            [id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(sessions, 1, "the tmux half must still be captured in full");
    drop(conn);

    common::shutdown(&env.tmux());
}

#[test]
fn a_healthy_capture_still_prunes_to_the_configured_retention() {
    // The second control: a machine whose compositor answers must go on
    // capturing and pruning exactly as before. If the new strictness ever
    // starts refusing healthy captures, this is what says so.
    let env = Env::new("healthy");
    env.write_config(
        "[restore]\nterminal = \"ghostty\"\n\
         [capture]\nkeep_snapshots = 1\ndebounce_max_latency_secs = 1\n\
         [agents]\nauto_resume = false\n",
    );

    let t = env.tmux();
    t.run(&["new-session", "-d", "-s", "dev", "-n", "code", "-c", "/tmp"])
        .unwrap();
    let _client = Client::attach(&t, "dev");
    let clients = wait_for_clients(&t, 1);
    env.set_window_pids(&[clients[0].pid]);

    let (first, out) = env.run(&["snapshot", "--reason", "test"]);
    assert!(out.status.success());
    let first_id = first["snapshot_id"].as_i64().unwrap();

    let (second, out) = env.run(&["snapshot", "--reason", "test"]);
    assert!(
        out.status.success(),
        "a second healthy capture failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let second_id = second["snapshot_id"].as_i64().unwrap();
    assert_ne!(first_id, second_id);

    assert_eq!(
        env.snapshot_ids(),
        vec![second_id],
        "retention must still prune on a healthy machine"
    );
    let conn = osm::db::open(&env.db()).unwrap();
    let ps = osm::desktop::placements_of(&conn, second_id).unwrap();
    assert_eq!(ps.len(), 1, "and the survivor must carry its placement");
    assert_eq!(ps[0].session, "dev");
    drop(conn);

    common::shutdown(&env.tmux());
}
