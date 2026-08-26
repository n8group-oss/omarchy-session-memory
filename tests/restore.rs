mod common;

use osm::{capture, db, model, restore, tmux::Tmux};

struct Server(Tmux);

impl Server {
    fn start(label: &str) -> Self {
        let sock = format!("osm-res-{}-{}", label, std::process::id());
        Server(Tmux::with_socket(&sock))
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

fn topology(t: &Tmux) -> Vec<(String, String, usize)> {
    let sessions = t.list_sessions().unwrap();
    let windows = t.list_windows().unwrap();
    let panes = t.list_panes().unwrap();
    let mut out: Vec<(String, String, usize)> = windows
        .iter()
        .map(|w| {
            let sname = sessions
                .iter()
                .find(|s| s.id == w.session_id)
                .unwrap()
                .name
                .clone();
            let count = panes.iter().filter(|p| p.window_id == w.id).count();
            (sname, w.name.clone(), count)
        })
        .collect();
    out.sort();
    out
}

#[test]
fn restores_sessions_windows_and_pane_counts() {
    let src = Server::start("src");
    let t = src.t();
    // Name the window at creation instead of renaming "alpha:1": window
    // indices start at 0 with tmux's stock configuration and at 1 with this
    // developer's, so targeting a literal index only worked on one machine.
    t.run(&[
        "new-session",
        "-d",
        "-s",
        "alpha",
        "-n",
        "main",
        "-c",
        "/tmp",
    ])
    .unwrap();
    t.run(&["split-window", "-t", "alpha:main", "-c", "/tmp"])
        .unwrap();
    t.run(&["new-window", "-t", "alpha", "-n", "logs", "-c", "/tmp"])
        .unwrap();
    // Named, like `alpha` above, and for a related reason: a window whose
    // name tmux owns is called `tmux` for the first few milliseconds of its
    // life — the pty's foreground process group is tmux itself until the
    // shell has exec'd — so this topology is read three times (here, at
    // capture, and after the restore) and any two of them can disagree.
    // Naming it turns automatic-rename off and removes the race; what this
    // test is about is that sessions, windows and pane counts round-trip.
    t.run(&[
        "new-session",
        "-d",
        "-s",
        "beta",
        "-n",
        "shell",
        "-c",
        "/tmp",
    ])
    .unwrap();

    let expected = topology(t);

    let tmp = tempfile::tempdir().unwrap();
    let mut conn = db::open(&tmp.path().join("state.db")).unwrap();
    let id = capture::snapshot(&mut conn, t, "test").unwrap();
    let tree = model::load(&conn, id).unwrap();

    let dst = Server::start("dst");
    let out = restore::restore_tree(dst.t(), &tree).unwrap();
    assert_eq!(out.created.len(), 2);
    assert!(out.adopted.is_empty());
    assert!(out.skipped.is_empty());

    assert_eq!(topology(dst.t()), expected);
}

#[test]
fn restore_is_idempotent_and_adopts_existing_sessions() {
    let src = Server::start("idem-src");
    src.t()
        .run(&[
            "new-session",
            "-d",
            "-s",
            "alpha",
            "-n",
            "code",
            "-c",
            "/tmp",
        ])
        .unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let mut conn = db::open(&tmp.path().join("state.db")).unwrap();
    let id = capture::snapshot(&mut conn, src.t(), "test").unwrap();
    let tree = model::load(&conn, id).unwrap();

    let dst = Server::start("idem-dst");
    restore::restore_tree(dst.t(), &tree).unwrap();
    let after_first = topology(dst.t());

    let second = restore::restore_tree(dst.t(), &tree).unwrap();
    assert_eq!(second.adopted, vec!["alpha".to_string()]);
    assert!(second.created.is_empty());
    assert_eq!(topology(dst.t()), after_first, "no duplication on rerun");
}

/// The captured directory still exists, so the pane must come back in it.
///
/// The predecessor of this test asserted only that *a session exists* after
/// restore, which would have passed with restore using `/`, using `$HOME`, or
/// omitting `-c` altogether — i.e. it could not tell a working directory
/// restore from no directory restore at all.
#[test]
fn restores_a_pane_into_its_captured_directory() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_str().unwrap().to_string();

    let src = Server::start("cwd-ok-src");
    src.t()
        .run(&[
            "new-session",
            "-d",
            "-s",
            "alpha",
            "-n",
            "code",
            "-c",
            &path,
        ])
        .unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let mut conn = db::open(&tmp.path().join("state.db")).unwrap();
    let id = capture::snapshot(&mut conn, src.t(), "test").unwrap();
    let tree = model::load(&conn, id).unwrap();

    let dst = Server::start("cwd-ok-dst");
    let out = restore::restore_tree(dst.t(), &tree).unwrap();
    assert!(
        out.degraded.is_empty(),
        "an existing directory is not a degraded restore: {:?}",
        out.degraded
    );

    let panes = dst.t().list_panes().unwrap();
    assert_eq!(panes.len(), 1);
    assert_eq!(
        panes[0].cwd, path,
        "the restored pane must be in the directory it was captured in"
    );
}

/// A directory that vanished between capture and restore — an unmounted
/// volume, an encrypted home that is not open yet, a network mount that is
/// still coming up at graphical-session start.
///
/// The pane still has to be created somewhere (`new-window -c <gone>` fails
/// outright and would take the session with it), but the substitution must be
/// *reported*, not passed off as a complete restore.
#[test]
fn a_missing_captured_directory_is_reported_as_degraded_not_as_success() {
    let gone = tempfile::tempdir().unwrap();
    let gone_path = gone.path().to_str().unwrap().to_string();

    let src = Server::start("cwd-src");
    src.t()
        .run(&[
            "new-session",
            "-d",
            "-s",
            "alpha",
            "-n",
            "code",
            "-c",
            &gone_path,
        ])
        .unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let mut conn = db::open(&tmp.path().join("state.db")).unwrap();
    let id = capture::snapshot(&mut conn, src.t(), "test").unwrap();
    let tree = model::load(&conn, id).unwrap();
    drop(gone); // directory no longer exists

    let dst = Server::start("cwd-dst");
    let out = restore::restore_tree(dst.t(), &tree).expect("must not fail on missing cwd");
    assert_eq!(dst.t().list_sessions().unwrap().len(), 1);

    assert_eq!(
        out.degraded.len(),
        1,
        "the substituted directory must be recorded: {out:?}"
    );
    assert_eq!(out.degraded[0].session, "alpha");
    assert_eq!(out.degraded[0].captured, gone_path);

    let expected = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
    assert_eq!(
        out.degraded[0].used, expected,
        "the report must name where the pane actually went"
    );

    let panes = dst.t().list_panes().unwrap();
    assert_eq!(panes.len(), 1);
    assert_eq!(
        panes[0].cwd, expected,
        "the pane's real destination directory, not merely that a session exists"
    );
    assert_ne!(
        panes[0].cwd, gone_path,
        "the captured directory is gone, so the pane cannot be there"
    );
}

#[test]
fn restores_active_window_selection() {
    let src = Server::start("act-src");
    let t = src.t();
    t.run(&[
        "new-session",
        "-d",
        "-s",
        "alpha",
        "-n",
        "code",
        "-c",
        "/tmp",
    ])
    .unwrap();
    t.run(&["new-window", "-t", "alpha", "-n", "second", "-c", "/tmp"])
        .unwrap();
    t.run(&["new-window", "-t", "alpha", "-n", "third", "-c", "/tmp"])
        .unwrap();
    t.run(&["select-window", "-t", "alpha:second"]).unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let mut conn = db::open(&tmp.path().join("state.db")).unwrap();
    let id = capture::snapshot(&mut conn, t, "test").unwrap();
    let tree = model::load(&conn, id).unwrap();

    let dst = Server::start("act-dst");
    restore::restore_tree(dst.t(), &tree).unwrap();

    let active: Vec<String> = dst
        .t()
        .list_windows()
        .unwrap()
        .into_iter()
        .filter(|w| w.active)
        .map(|w| w.name)
        .collect();
    assert_eq!(active, vec!["second".to_string()]);
}

#[test]
fn restores_active_pane_within_window() {
    // Give each of the three panes a distinct, identifiable cwd so we can
    // tell them apart after restore without relying on tmux pane_index,
    // which select-layout may renumber by geometry (Ruling 2).
    let dir0 = tempfile::tempdir().unwrap();
    let dir1 = tempfile::tempdir().unwrap();
    let dir2 = tempfile::tempdir().unwrap();

    let src = Server::start("pane-src");
    let t = src.t();
    t.run(&[
        "new-session",
        "-d",
        "-s",
        "alpha",
        "-n",
        "main",
        "-c",
        dir0.path().to_str().unwrap(),
    ])
    .unwrap();
    // Creation order: dir0 (initial pane), then dir1, then dir2.
    t.run(&[
        "split-window",
        "-t",
        "alpha:main",
        "-c",
        dir1.path().to_str().unwrap(),
    ])
    .unwrap();
    t.run(&[
        "split-window",
        "-t",
        "alpha:main",
        "-c",
        dir2.path().to_str().unwrap(),
    ])
    .unwrap();

    // Make the second-created pane (dir1) the active one, not the first.
    let panes_before = t.list_panes().unwrap();
    let target_pane_id = panes_before
        .iter()
        .find(|p| p.cwd == dir1.path().to_str().unwrap())
        .unwrap()
        .id
        .clone();
    t.run(&["select-pane", "-t", &target_pane_id]).unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let mut conn = db::open(&tmp.path().join("state.db")).unwrap();
    let id = capture::snapshot(&mut conn, t, "test").unwrap();
    let tree = model::load(&conn, id).unwrap();

    let dst = Server::start("pane-dst");
    restore::restore_tree(dst.t(), &tree).unwrap();

    let dst_panes = dst.t().list_panes().unwrap();
    let active_pane = dst_panes
        .iter()
        .find(|p| p.active)
        .expect("restored window must have an active pane");

    assert_eq!(
        active_pane.cwd,
        dir1.path().to_str().unwrap(),
        "active pane should be the second-created pane, identified by its cwd"
    );
}

/// Parses the `WxH` field out of a tmux layout string
/// (`checksum,WxH,x,y{...}`), mirroring the format tmux itself emits for
/// `#{window_layout}`.
fn layout_wh(layout: &str) -> (u32, u32) {
    let wxh = layout.split(',').nth(1).expect("layout has a WxH field");
    let (w, h) = wxh.split_once('x').expect("WxH field has an x separator");
    (w.parse().unwrap(), h.parse().unwrap())
}

/// Live-validation regression test: a detached tmux session with no
/// attached client defaults to 80x24. A window with 5+ panes captured at a
/// larger size cannot be rebuilt inside that default — `split-window` fails
/// with "no space for a new pane" partway through, and the captured layout
/// then fails to apply because the pane count no longer matches it. The
/// destination session must be created at (at least) the captured size, not
/// left at tmux's 80x24 default.
///
/// This test only discriminates because the source session is explicitly
/// sized *larger* than 80x24 (so 5 panes fit there) and the destination
/// server is a stock `Server::start`, which — absent the fix — creates
/// sessions with no `-x`/`-y` at all and thus falls back to 80x24.
#[test]
fn restores_five_pane_window_at_captured_size_not_default_80x24() {
    let src = Server::start("wide-src");
    let t = src.t();
    // Larger than 80x24 so a 5th pane actually fits on the source.
    t.run(&[
        "new-session",
        "-d",
        "-s",
        "alpha",
        "-n",
        "main",
        "-x",
        "300",
        "-y",
        "80",
        "-c",
        "/tmp",
    ])
    .unwrap();
    for _ in 0..4 {
        t.run(&["split-window", "-t", "alpha:main", "-c", "/tmp"])
            .unwrap();
    }

    let src_windows = t.list_windows().unwrap();
    assert_eq!(src_windows.len(), 1);
    let src_panes = t.list_panes().unwrap();
    assert_eq!(
        src_panes.len(),
        5,
        "source setup must actually produce 5 panes"
    );
    let expected_wh = layout_wh(&src_windows[0].layout);

    let tmp = tempfile::tempdir().unwrap();
    let mut conn = db::open(&tmp.path().join("state.db")).unwrap();
    let id = capture::snapshot(&mut conn, t, "test").unwrap();
    let tree = model::load(&conn, id).unwrap();

    // A fresh destination server with no client ever attached: this is
    // exactly the boot-time, no-attached-client case where tmux would
    // otherwise default new sessions to 80x24.
    let dst = Server::start("wide-dst");
    let out = restore::restore_tree(dst.t(), &tree).unwrap();
    assert!(
        out.failed.is_empty(),
        "restore must not fail: {:?}",
        out.failed
    );

    let dst_panes = dst.t().list_panes().unwrap();
    assert_eq!(
        dst_panes.len(),
        5,
        "all 5 panes must survive restore, not just the 4 that fit in 80x24"
    );

    let dst_windows = dst.t().list_windows().unwrap();
    assert_eq!(dst_windows.len(), 1);
    let actual_wh = layout_wh(&dst_windows[0].layout);
    assert_eq!(
        actual_wh, expected_wh,
        "restored layout dimensions must match the captured layout, not tmux's 80x24 default"
    );
}

/// A window's `automatic-rename` state survives the restore, in both
/// directions — and that is a decision about what a restored window should be
/// called, not an implementation detail.
///
/// A name tmux derived is a fact about what was in the window's foreground at
/// the instant of capture, and nothing is in that foreground any more after a
/// reboot. Replaying it through `new-window -n` would do two wrong things at
/// once: name the window after a process that is gone, and — because `-n`
/// turns `automatic-rename` off — freeze that stale name permanently on a
/// window the user had always let tmux name. So an auto-named window is
/// recreated without `-n` and tmux derives a fresh name, arriving back at what
/// the user actually saw.
///
/// A name the *user* chose is the opposite: it is the whole point of session
/// memory, and it is applied exactly as captured.
#[test]
fn a_restored_window_keeps_who_owned_its_name() {
    let src = Server::start("autoname-src");
    // `code` is named by the user; the second window is left to tmux.
    src.t()
        .run(&["new-session", "-d", "-s", "dev", "-n", "code", "-c", "/tmp"])
        .unwrap();
    src.t()
        .run(&["new-window", "-d", "-t", "dev", "-c", "/tmp"])
        .unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let mut conn = db::open(&tmp.path().join("state.db")).unwrap();
    let snap = capture::snapshot(&mut conn, src.t(), "test").unwrap();
    let mut tree = model::load(&conn, snap).unwrap();
    drop(src);

    // The captured flags, which the restore is about to act on.
    let session = &mut tree.sessions[0];
    session.windows.sort_by_key(|w| w.idx);
    // Only the flags: what tmux has derived for the second window by now is
    // the very thing that is not stable, so asserting it would be asserting a
    // race.
    assert_eq!(
        session
            .windows
            .iter()
            .map(|w| (w.name.clone(), w.auto_named))
            .collect::<Vec<_>>()
            .iter()
            .map(|(n, a)| (n.as_str(), *a))
            .collect::<Vec<_>>()[0],
        ("code", Some(false)),
        "the user named the first window"
    );
    assert_eq!(
        session.windows[1].auto_named,
        Some(true),
        "and tmux named the second"
    );
    // A stale derived name, exactly as a capture inside the rename gap would
    // have recorded it. If the restore replayed it, it would stick forever.
    session.windows[1].name = "tmux".to_string();

    let dst = Server::start("autoname-dst");
    restore::restore_tree(dst.t(), &tree).unwrap();

    // Read after the derived name has settled, not immediately. A pane's
    // foreground process group is tmux itself until the shell has finished
    // exec'ing, so a window tmux owns the name of is *called* `tmux` for its
    // first few milliseconds — 60 observations out of 60 on 3.7c. Asserting
    // straight after `restore_tree` is asserting that race, and it loses on a
    // machine fast enough to get there first (it failed exactly this way in
    // CI). The wait is on the observable condition and bounded; if the name
    // never settles the assertions below still run and still fail.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut live: Vec<(u32, String, bool)> = loop {
        let mut live: Vec<(u32, String, bool)> = dst
            .t()
            .list_windows()
            .unwrap()
            .into_iter()
            .map(|w| (w.idx, w.name, w.auto_named))
            .collect();
        live.sort();
        let settled = live.iter().all(|(_, name, auto)| !*auto || name != "tmux");
        if settled || std::time::Instant::now() >= deadline {
            break live;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    live.sort();
    assert_eq!(
        live.iter()
            .map(|(_, n, a)| (n.as_str(), *a))
            .collect::<Vec<_>>()[0],
        ("code", false),
        "the name the user chose comes back, still owned by the user"
    );
    let (_, derived, auto) = &live[1];
    assert!(
        *auto,
        "a window tmux named must come back still named by tmux, not frozen \
         on the name it happened to have at capture"
    );
    assert_ne!(
        derived, "tmux",
        "…and so must not be wearing the transient name the snapshot held"
    );
}
