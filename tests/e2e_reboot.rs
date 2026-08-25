mod common;

use osm::layout::{self, LayoutNode};
use osm::{capture, db, restore, tmux::Tmux};

fn sock(label: &str) -> String {
    format!("osm-e2e-{}-{}", label, std::process::id())
}

struct Server(Tmux);

impl Drop for Server {
    fn drop(&mut self) {
        common::shutdown(&self.0);
    }
}

/// (session name, window index, window name, pane count) for every window,
/// sorted for stable comparison.
fn shape(t: &Tmux) -> Vec<(String, u32, String, usize)> {
    let sessions = t.list_sessions().unwrap();
    let windows = t.list_windows().unwrap();
    let panes = t.list_panes().unwrap();
    let mut out: Vec<(String, u32, String, usize)> = windows
        .iter()
        .map(|w| {
            let sname = sessions
                .iter()
                .find(|s| s.id == w.session_id)
                .unwrap()
                .name
                .clone();
            let count = panes.iter().filter(|p| p.window_id == w.id).count();
            (sname, w.idx, w.name.clone(), count)
        })
        .collect();
    out.sort();
    out
}

/// The exact pane count recorded in `shape()` for one (session, window),
/// asserted individually so a class of bug that silently caps a window's
/// pane count (while total counts across the layout happen to still add up)
/// cannot slip through unnoticed.
fn pane_count(shape: &[(String, u32, String, usize)], session: &str, window: &str) -> usize {
    shape
        .iter()
        .find(|(s, _, w, _)| s == session && w == window)
        .unwrap_or_else(|| panic!("no window {session}:{window} in shape"))
        .3
}

/// A tmux layout string's geometry, with the volatile parts normalized away:
/// the leading checksum (it varies with pane ids, which are themselves
/// reassigned fresh on every restore) and each leaf pane's trailing pane id.
/// Everything that actually describes the layout -- the nesting structure
/// (`{` horizontal splits vs `[` vertical splits) and every `WxH,x,y` triple
/// -- is preserved and must match exactly.
///
/// Comparing only the window's overall `WxH` (an earlier version of this test
/// did) does not discriminate: every window in a session inherits one
/// session-wide size from `session_dims()` regardless of whether
/// `select-layout` ever ran, so that comparison passes even with the
/// `select-layout` call deleted entirely. Comparing the full geometry tree
/// closes that gap.
///
/// This is `osm::layout::parse`, not a second parser written for the test.
/// The same grammar now decides whether a captured layout is safe to hand to
/// `select-layout` at all, so a bug in it is a restore bug, and this test
/// exercising the real thing is the point rather than an economy.
fn normalize_layout(layout: &str) -> LayoutNode {
    layout::parse(layout)
        .unwrap_or_else(|e| panic!("tmux emitted a layout osm rejects: {layout:?}: {e}"))
}

/// (session name, window name, normalized layout geometry tree) for every
/// window, sorted for stable comparison. Topology equality (matching
/// session/window/pane counts) can pass even while the captured layout is
/// being silently discarded during restore -- this compares the actual
/// nested split structure and per-pane dimensions tmux reports for each
/// window to catch that class of bug.
fn layouts(t: &Tmux) -> Vec<(String, String, LayoutNode)> {
    let sessions = t.list_sessions().unwrap();
    let windows = t.list_windows().unwrap();
    let mut out: Vec<(String, String, LayoutNode)> = windows
        .iter()
        .map(|w| {
            let sname = sessions
                .iter()
                .find(|s| s.id == w.session_id)
                .unwrap()
                .name
                .clone();
            (sname, w.name.clone(), normalize_layout(&w.layout))
        })
        .collect();
    out.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    out
}

#[test]
fn full_layout_survives_a_simulated_reboot() {
    let before = Server(Tmux::with_socket(&sock("before")));
    let t = &before.0;

    // A layout resembling real use: a multi-pane dev session and a side
    // session. dev:code gets 5 panes specifically -- a bug that silently
    // capped windows at 4 panes shipped undetected because every earlier
    // test used windows with 3 panes or fewer; it was caught only by manual
    // validation against a real 5-pane session.
    // -x/-y large enough that splitting dev:code into 5 panes below has
    // room; a detached session with no attached client otherwise defaults
    // to tmux's own 80x24, too small for a 5-way split.
    t.run(&[
        "new-session",
        "-d",
        "-s",
        "dev",
        "-n",
        "code",
        "-c",
        "/tmp",
        "-x",
        "200",
        "-y",
        "50",
    ])
    .unwrap();
    t.run(&["split-window", "-t", "dev:code", "-c", "/tmp"])
        .unwrap();
    t.run(&["split-window", "-t", "dev:code", "-c", "/tmp"])
        .unwrap();
    t.run(&["split-window", "-t", "dev:code", "-c", "/tmp"])
        .unwrap();
    t.run(&["split-window", "-t", "dev:code", "-c", "/tmp"])
        .unwrap();
    // Skew one pane's size away from tmux's default even split. Plain
    // `split-window` calls alone -- which is exactly what restore's
    // fill_window uses to recreate each pane -- would independently
    // reproduce an even split from the same starting window size with or
    // without select-layout ever running, so an even-split layout can't
    // tell the two apart. A deliberately uneven captured layout can only be
    // reproduced by applying the captured layout string.
    t.run(&["resize-pane", "-t", "dev:code.2", "-y", "5"])
        .unwrap();
    t.run(&["new-window", "-t", "dev", "-n", "test", "-c", "/tmp"])
        .unwrap();
    t.run(&["split-window", "-t", "dev:test", "-c", "/tmp"])
        .unwrap();
    t.run(&["resize-pane", "-t", "dev:test.1", "-y", "10"])
        .unwrap();
    t.run(&[
        "new-session",
        "-d",
        "-s",
        "notes",
        "-n",
        "scratch",
        "-c",
        "/tmp",
    ])
    .unwrap();
    t.run(&["select-window", "-t", "dev:test"]).unwrap();

    let expected = shape(t);
    let expected_layouts = layouts(t);
    assert_eq!(expected.len(), 3, "two dev windows plus one notes window");
    assert_eq!(
        pane_count(&expected, "dev", "code"),
        5,
        "dev:code has 5 panes"
    );
    assert_eq!(
        pane_count(&expected, "dev", "test"),
        2,
        "dev:test has 2 panes"
    );
    assert_eq!(
        pane_count(&expected, "notes", "scratch"),
        1,
        "notes:scratch has 1 pane"
    );

    let tmp = tempfile::tempdir().unwrap();
    let mut conn = db::open(&tmp.path().join("state.db")).unwrap();
    let snap = capture::snapshot(&mut conn, t, "e2e").unwrap();

    // Simulate a reboot: the snapshot belongs to a previous boot, and the
    // tmux server is gone.
    conn.execute(
        "UPDATE snapshots SET boot_id='boot-previous' WHERE id=?1",
        [snap],
    )
    .unwrap();
    drop(before);

    let after = Server(Tmux::with_socket(&sock("after")));
    let report = restore::run_restore(&mut conn, &after.0, false).unwrap();

    assert_eq!(report.state, "succeeded");
    assert_eq!(report.snapshot_id, Some(snap));

    let restored = shape(&after.0);
    assert_eq!(restored, expected, "layout topology must match pre-reboot");
    assert_eq!(
        pane_count(&restored, "dev", "code"),
        5,
        "dev:code still has 5 panes"
    );
    assert_eq!(
        pane_count(&restored, "dev", "test"),
        2,
        "dev:test still has 2 panes"
    );
    assert_eq!(
        pane_count(&restored, "notes", "scratch"),
        1,
        "notes:scratch still has 1 pane"
    );
    assert_eq!(
        layouts(&after.0),
        expected_layouts,
        "restored window layout geometry (nested split structure and per-pane WxH,x,y) must match pre-reboot"
    );

    let after_sessions = after.0.list_sessions().unwrap();
    let dev_session_id = after_sessions
        .iter()
        .find(|s| s.name == "dev")
        .unwrap()
        .id
        .clone();
    let active: Vec<String> = after
        .0
        .list_windows()
        .unwrap()
        .into_iter()
        .filter(|w| w.active && w.session_id == dev_session_id)
        .map(|w| w.name)
        .collect();
    assert_eq!(
        active,
        vec!["test".to_string()],
        "active window is restored"
    );
}

#[test]
fn restoring_twice_after_reboot_does_not_duplicate() {
    let before = Server(Tmux::with_socket(&sock("dup-before")));
    before
        .0
        .run(&["new-session", "-d", "-s", "dev", "-c", "/tmp"])
        .unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let mut conn = db::open(&tmp.path().join("state.db")).unwrap();
    let snap = capture::snapshot(&mut conn, &before.0, "e2e").unwrap();
    conn.execute(
        "UPDATE snapshots SET boot_id='boot-previous' WHERE id=?1",
        [snap],
    )
    .unwrap();
    drop(before);

    let after = Server(Tmux::with_socket(&sock("dup-after")));
    restore::run_restore(&mut conn, &after.0, false).unwrap();

    // Re-run the restore the only way production ever re-runs one: the first
    // attempt was killed after tmux had already created the sessions, leaving
    // the snapshot `restore_in_progress` with a `running` attempt. (This used
    // to hand-patch state='complete', a step production performs nowhere.)
    conn.execute(
        "UPDATE snapshots SET state='restore_in_progress' WHERE id=?1",
        [snap],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO restore_attempts (snapshot_id, started_at, state)
         VALUES (?1, 1, 'running')",
        [snap],
    )
    .unwrap();

    let second = restore::run_restore(&mut conn, &after.0, false).unwrap();
    assert_eq!(
        second.outcome.adopted,
        vec!["dev".to_string()],
        "the reclaimed snapshot must adopt the live session"
    );
    assert_eq!(after.0.list_sessions().unwrap().len(), 1);
}
