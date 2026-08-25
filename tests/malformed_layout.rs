//! A captured layout tmux will not accept must cost that window its geometry
//! and nothing else.
//!
//! The layout string is the only part of a snapshot that osm replays into tmux
//! verbatim, and it is not a trusted input: a database can be corrupted or
//! hand-edited, and a layout written by a newer tmux can be rejected by the
//! older one a machine boots into. Handing it over unchecked used to be a
//! `?` — the session's restore aborted at that window, so the windows after it
//! were never built and the session was reported `failed` — and on some tmux
//! builds a bad layout takes the *server* down, which would destroy every
//! session this same restore had already rebuilt and every live session it had
//! adopted.
//!
//! So every test here ends by asserting the destination server is still alive.
//! That assertion is the point of the file; the rest is what "still alive"
//! has to mean in detail.
//!
//! Every tmux server started here is private, addressed by a `-L` socket name
//! carrying this process's id.

mod common;

use osm::{capture, db, restore, tmux::Tmux};

fn sock(label: &str) -> String {
    format!("osm-badlayout-{}-{}", label, std::process::id())
}

struct Server(Tmux);

impl Drop for Server {
    fn drop(&mut self) {
        common::shutdown(&self.0);
    }
}

/// Two sessions: `dev` with a three-pane `code` window plus a `logs` window,
/// and `notes`. Captured, attributed to a previous boot, with `code`'s layout
/// then overwritten by `layout`.
fn snapshot_with_layout(
    src: &Server,
    layout: &str,
) -> (tempfile::TempDir, rusqlite::Connection, i64) {
    let t = &src.0;
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
    t.run(&["new-window", "-d", "-t", "dev", "-n", "logs", "-c", "/tmp"])
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

    let tmp = tempfile::tempdir().unwrap();
    let mut conn = db::open(&tmp.path().join("state.db")).unwrap();
    let snap = capture::snapshot(&mut conn, t, "test").unwrap();
    conn.execute(
        "UPDATE snapshots SET boot_id='boot-previous' WHERE id=?1",
        [snap],
    )
    .unwrap();
    // The corruption: exactly one window's layout column, everything else
    // captured normally.
    let changed = conn
        .execute(
            "UPDATE window_rows SET layout=?1 WHERE name='code'",
            [layout],
        )
        .unwrap();
    assert_eq!(changed, 1, "exactly one window's layout is corrupted");
    (tmp, conn, snap)
}

fn windows(t: &Tmux) -> Vec<(String, String, usize)> {
    let sessions = t.list_sessions().expect("the tmux server must still be up");
    let panes = t.list_panes().unwrap();
    let mut out: Vec<(String, String, usize)> = t
        .list_windows()
        .unwrap()
        .iter()
        .map(|w| {
            let session = sessions
                .iter()
                .find(|s| s.id == w.session_id)
                .unwrap()
                .name
                .clone();
            let count = panes.iter().filter(|p| p.window_id == w.id).count();
            (session, w.name.clone(), count)
        })
        .collect();
    out.sort();
    out
}

fn snapshot_state(conn: &rusqlite::Connection, snap: i64) -> String {
    conn.query_row("SELECT state FROM snapshots WHERE id=?1", [snap], |r| {
        r.get(0)
    })
    .unwrap()
}

/// The captured string is not a layout at all — here a truncated one, the
/// shape a half-written or clipped database row takes.
#[test]
fn an_unparsable_layout_is_skipped_and_the_server_survives() {
    let src = Server(Tmux::with_socket(&sock("unparsable-src")));
    let (_tmp, mut conn, snap) =
        snapshot_with_layout(&src, "1e0d,200x50,0,0{100x50,0,0,0,99x50,101,0");
    drop(src);

    let dst = Server(Tmux::with_socket(&sock("unparsable-dst")));
    let report = restore::run_restore(&mut conn, &dst.0, false).unwrap();

    // The whole reason this file exists.
    assert!(
        dst.0.server_running(),
        "a layout osm cannot validate must never reach tmux: the destination \
         server is gone, and with it every session this restore had already \
         rebuilt"
    );

    // The panes are back, and so is every window after the broken one.
    assert_eq!(
        windows(&dst.0),
        vec![
            ("dev".to_string(), "code".to_string(), 3),
            ("dev".to_string(), "logs".to_string(), 1),
            ("notes".to_string(), "scratch".to_string(), 1),
        ],
        "only the geometry of one window may be lost, not its panes, not the \
         windows after it, and not the other sessions"
    );

    assert!(
        report.outcome.failed.is_empty(),
        "a bad layout is not a failed session: {:?}",
        report.outcome.failed
    );
    assert_eq!(
        report.outcome.created,
        vec!["dev".to_string(), "notes".to_string()]
    );

    assert_eq!(
        report.outcome.skipped_layouts.len(),
        1,
        "the window must be reported as degraded: {:?}",
        report.outcome.skipped_layouts
    );
    let skipped = &report.outcome.skipped_layouts[0];
    assert_eq!(skipped.session, "dev");
    assert_eq!(skipped.window, "code");
    assert!(
        skipped.reason.contains("parse"),
        "the report must say the layout did not parse, got {:?}",
        skipped.reason
    );

    // Degraded is not success: the snapshot holds the only copy of that
    // layout, so it must stay selectable for a later run.
    assert_eq!(report.state, "partial", "{report:?}");
    assert!(report.retryable, "{report:?}");
    assert_eq!(snapshot_state(&conn, snap), "complete");
}

/// A layout that is structurally fine but that tmux itself rejects — here a
/// single-pane layout for a window that has three panes, which is what a
/// layout captured from a different window or a different tmux looks like.
/// osm cannot know this in advance; what it must do is not abort.
#[test]
fn a_layout_tmux_refuses_is_not_fatal_and_the_server_survives() {
    let src = Server(Tmux::with_socket(&sock("refused-src")));
    let (_tmp, mut conn, snap) = snapshot_with_layout(&src, "1e0d,200x50,0,0,0");
    drop(src);

    let dst = Server(Tmux::with_socket(&sock("refused-dst")));
    let report = restore::run_restore(&mut conn, &dst.0, false).unwrap();

    assert!(
        dst.0.server_running(),
        "a layout tmux refuses must not take the server with it"
    );
    assert_eq!(
        windows(&dst.0),
        vec![
            ("dev".to_string(), "code".to_string(), 3),
            ("dev".to_string(), "logs".to_string(), 1),
            ("notes".to_string(), "scratch".to_string(), 1),
        ],
        "a refused layout must not cost the window its panes or the session \
         its later windows"
    );
    assert!(
        report.outcome.failed.is_empty(),
        "tmux refusing one layout is not a failed session: {:?}",
        report.outcome.failed
    );
    assert_eq!(
        report.outcome.skipped_layouts.len(),
        1,
        "the refusal must be reported: {:?}",
        report.outcome.skipped_layouts
    );
    assert!(
        report.outcome.skipped_layouts[0].reason.contains("refused"),
        "the report must carry tmux's own complaint, got {:?}",
        report.outcome.skipped_layouts[0].reason
    );
    assert_eq!(report.state, "partial", "{report:?}");
    assert!(report.retryable, "{report:?}");
    assert_eq!(snapshot_state(&conn, snap), "complete");
}
