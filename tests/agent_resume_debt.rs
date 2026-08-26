//! What a restore writes down when it owes a conversation, and what makes
//! that statement stop being true.
//!
//! These are the properties `capture` relies on to decide whether a binding it
//! cannot see may still be carried. They are asserted against the database
//! rather than against a predicate, so a rule that "passes" while recording
//! nothing cannot pass here.
//!
//! No tmux and no process-global environment, so these run in parallel with
//! everything else.

use osm::agent::AgentKind;
use osm::db;
use rusqlite::Connection;
use std::collections::HashSet;

const A: &str = "0cfebf91-81c0-43d5-af63-c9fe7e844f01";
const B: &str = "0cfebf91-81c0-43d5-af63-c9fe7e844f02";

/// A snapshot with one window in one session, whose panes are given by
/// `(pane_idx, Option<(kind, native_id)>)`.
fn snapshot_with(conn: &Connection, panes: &[(u32, Option<(&str, &str)>)]) -> i64 {
    conn.execute(
        "INSERT INTO snapshots (taken_at, boot_id, reason, state)
         VALUES (1, 'boot-x', 'test', 'complete')",
        [],
    )
    .unwrap();
    let snapshot_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO session_rows (snapshot_id, tmux_session_id, name) VALUES (?1, '$0', 'dev')",
        [snapshot_id],
    )
    .unwrap();
    let session_row = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO window_rows (snapshot_id, tmux_window_id, name, layout)
         VALUES (?1, '@0', 'code', '')",
        [snapshot_id],
    )
    .unwrap();
    let window_row = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO session_window_links (session_row_id, window_row_id, idx, active)
         VALUES (?1, ?2, 0, 1)",
        [session_row, window_row],
    )
    .unwrap();
    for (idx, binding) in panes {
        let (kind, native_id) = match binding {
            Some((k, id)) => (Some(*k), Some(*id)),
            None => (None, None),
        };
        conn.execute(
            "INSERT INTO pane_rows
               (window_row_id, tmux_pane_id, idx, cwd, restore_policy, agent_kind, agent_session_id)
             VALUES (?1, ?2, ?3, '/tmp', ?4, ?5, ?6)",
            rusqlite::params![
                window_row,
                format!("%{idx}"),
                idx,
                if kind.is_some() {
                    "agent_resume"
                } else {
                    "shell"
                },
                kind,
                native_id
            ],
        )
        .unwrap();
    }
    snapshot_id
}

/// The two sets a restore that verifiably put this whole fixture back would
/// hand `record`: the session it delivered, and every captured pane id in it.
///
/// Spelled out rather than defaulted to "everything", because that is the
/// point of the parameter — a restore that delivered less says less.
struct Delivered {
    sessions: HashSet<&'static str>,
    pane_ids: Vec<String>,
}

impl Delivered {
    fn everything() -> Self {
        Self {
            sessions: ["dev"].into_iter().collect(),
            pane_ids: (0..8).map(|i| format!("%{i}")).collect(),
        }
    }
    fn panes(&self) -> HashSet<&str> {
        self.pane_ids.iter().map(String::as_str).collect()
    }
}

fn open() -> (tempfile::TempDir, Connection) {
    let tmp = tempfile::tempdir().unwrap();
    let conn = db::open(&tmp.path().join("state.db")).unwrap();
    (tmp, conn)
}

#[test]
fn only_bound_panes_are_recorded_as_owed() {
    let (_tmp, conn) = open();
    let all = Delivered::everything();
    let snap = snapshot_with(
        &conn,
        &[(0, None), (1, Some(("claude", A))), (2, Some(("codex", B)))],
    );
    assert_eq!(
        osm::debt::record(&conn, snap, "boot-now", 1_000, &all.sessions, &all.panes()).unwrap(),
        2
    );
    let owed = osm::debt::pending(&conn, "boot-now", 1_000).unwrap();
    assert_eq!(owed.len(), 2, "{owed:?}");
    assert!(owed.contains(&(
        ("dev".to_string(), 0, 1),
        (AgentKind::Claude, A.to_string())
    )));
    assert!(owed.contains(&(("dev".to_string(), 0, 2), (AgentKind::Codex, B.to_string()))));
}

/// A native id is only unique within the agent that issued it, so the kind is
/// part of the identity. Were it not, discharging one would discharge both.
#[test]
fn the_agent_kind_is_part_of_the_identity_a_debt_is_keyed_by() {
    let (_tmp, conn) = open();
    let all = Delivered::everything();
    let snap = snapshot_with(&conn, &[(0, Some(("claude", A))), (1, Some(("codex", A)))]);
    osm::debt::record(&conn, snap, "boot-now", 1_000, &all.sessions, &all.panes()).unwrap();
    osm::debt::discharge(&conn, &[(AgentKind::Claude, A.to_string())]).unwrap();
    let owed = osm::debt::pending(&conn, "boot-now", 1_000).unwrap();
    assert_eq!(
        owed,
        [(("dev".to_string(), 0, 1), (AgentKind::Codex, A.to_string()))]
            .into_iter()
            .collect(),
        "only the Claude conversation named {A} was discharged"
    );
}

/// A conversation observed running owes nobody anything, whichever pane it
/// turned up in.
#[test]
fn a_conversation_that_came_back_is_no_longer_owed() {
    let (_tmp, conn) = open();
    let all = Delivered::everything();
    let snap = snapshot_with(&conn, &[(0, Some(("claude", A)))]);
    osm::debt::record(&conn, snap, "boot-now", 1_000, &all.sessions, &all.panes()).unwrap();
    osm::debt::discharge(&conn, &[(AgentKind::Claude, A.to_string())]).unwrap();
    assert!(osm::debt::pending(&conn, "boot-now", 1_000)
        .unwrap()
        .is_empty());
}

/// Debt is a statement about the panes of one boot's server. Nothing owed
/// before a reboot survives one — and it is deleted, not merely filtered, so
/// the table cannot accumulate a row per conversation per boot for ever.
#[test]
fn debt_does_not_survive_a_reboot() {
    let (_tmp, conn) = open();
    let all = Delivered::everything();
    let snap = snapshot_with(&conn, &[(0, Some(("claude", A)))]);
    osm::debt::record(&conn, snap, "boot-one", 1_000, &all.sessions, &all.panes()).unwrap();
    assert!(osm::debt::pending(&conn, "boot-two", 1_000)
        .unwrap()
        .is_empty());
    let rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM agent_resume_debt", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        rows, 0,
        "the previous boot's rows are gone, not just hidden"
    );
}

/// The window is what keeps "a restore has not put this back yet" from
/// becoming "this conversation is bound to that pane for ever".
#[test]
fn debt_expires_and_is_deleted_when_it_does() {
    let (_tmp, conn) = open();
    let all = Delivered::everything();
    let snap = snapshot_with(&conn, &[(0, Some(("claude", A)))]);
    let recorded_at = 1_000_000;
    osm::debt::record(
        &conn,
        snap,
        "boot-now",
        recorded_at,
        &all.sessions,
        &all.panes(),
    )
    .unwrap();

    let inside = recorded_at + osm::debt::WINDOW_SECS;
    assert_eq!(
        osm::debt::pending(&conn, "boot-now", inside).unwrap().len(),
        1,
        "still owed at the edge of the window"
    );
    let outside = recorded_at + osm::debt::WINDOW_SECS + 1;
    assert!(osm::debt::pending(&conn, "boot-now", outside)
        .unwrap()
        .is_empty());
    let rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM agent_resume_debt", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 0);
}

/// A second restore attempt for the same snapshot is a fresh statement about
/// the same panes. Dating it from the first attempt would let the window run
/// out while the retry is still working.
#[test]
fn re_recording_refreshes_the_window_rather_than_duplicating_the_row() {
    let (_tmp, conn) = open();
    let all = Delivered::everything();
    let snap = snapshot_with(&conn, &[(0, Some(("claude", A)))]);
    osm::debt::record(&conn, snap, "boot-now", 1_000, &all.sessions, &all.panes()).unwrap();
    osm::debt::record(
        &conn,
        snap,
        "boot-now",
        1_000 + osm::debt::WINDOW_SECS,
        &all.sessions,
        &all.panes(),
    )
    .unwrap();
    let rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM agent_resume_debt", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 1, "one pane, one debt");
    assert_eq!(
        osm::debt::pending(&conn, "boot-now", 1_000 + osm::debt::WINDOW_SECS + 1)
            .unwrap()
            .len(),
        1,
        "the second recording carries the window forward"
    );
}

/// A debt is a promise about one pane, and `pending` has to say which. Keyed
/// by the conversation alone, one pane's permission authorised carrying that
/// conversation onto every pane the previous snapshot could be mapped onto.
#[test]
fn a_debt_says_which_pane_it_is_owed_at() {
    let (_tmp, conn) = open();
    let all = Delivered::everything();
    let snap = snapshot_with(&conn, &[(3, Some(("claude", A)))]);
    osm::debt::record(&conn, snap, "boot-now", 1_000, &all.sessions, &all.panes()).unwrap();
    assert_eq!(
        osm::debt::pending(&conn, "boot-now", 1_000).unwrap(),
        [(
            ("dev".to_string(), 0, 3),
            (AgentKind::Claude, A.to_string())
        )]
        .into_iter()
        .collect(),
        "the session, window index and pane index the debt was taken for are \
         the whole of what makes it about one pane"
    );
}

/// A session the restore conflicted on had nothing put back into it, so
/// nothing in it is owed — however faithfully the snapshot describes it.
#[test]
fn a_session_this_restore_did_not_deliver_is_not_owed() {
    let (_tmp, conn) = open();
    let all = Delivered::everything();
    let snap = snapshot_with(&conn, &[(0, Some(("claude", A)))]);
    let delivered: HashSet<&str> = HashSet::new();
    assert_eq!(
        osm::debt::record(&conn, snap, "boot-now", 1_000, &delivered, &all.panes()).unwrap(),
        0
    );
    assert!(osm::debt::pending(&conn, "boot-now", 1_000)
        .unwrap()
        .is_empty());
}

/// Same again for the pane map: a captured pane this attempt has no record of
/// putting back is not one it may promise anything about.
#[test]
fn a_pane_this_restore_did_not_put_back_is_not_owed() {
    let (_tmp, conn) = open();
    let all = Delivered::everything();
    let snap = snapshot_with(&conn, &[(0, Some(("claude", A))), (1, Some(("codex", B)))]);
    let built: HashSet<&str> = ["%1"].into_iter().collect();
    assert_eq!(
        osm::debt::record(&conn, snap, "boot-now", 1_000, &all.sessions, &built).unwrap(),
        1
    );
    assert_eq!(
        osm::debt::pending(&conn, "boot-now", 1_000).unwrap(),
        [(("dev".to_string(), 0, 1), (AgentKind::Codex, B.to_string()))]
            .into_iter()
            .collect(),
        "only the pane the restore recorded putting back"
    );
}

/// Recording prunes as well as reading it does.
///
/// Debt used to be pruned only where it was read, so a machine that restored
/// partially several times without a capture in between kept every previous
/// attempt's rows: another boot-scoped set per restore, for ever, in a table
/// nothing else ever deleted from.
#[test]
fn recording_a_debt_prunes_the_rows_that_are_no_longer_about_this_boot() {
    let (_tmp, conn) = open();
    let all = Delivered::everything();
    let snap = snapshot_with(&conn, &[(0, Some(("claude", A)))]);
    osm::debt::record(&conn, snap, "boot-one", 1_000, &all.sessions, &all.panes()).unwrap();
    // A reboot, and a restore that never gets as far as a capture.
    osm::debt::record(&conn, snap, "boot-two", 2_000, &all.sessions, &all.panes()).unwrap();
    let rows: Vec<String> = conn
        .prepare("SELECT boot_id FROM agent_resume_debt")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        rows,
        vec!["boot-two".to_string()],
        "the previous boot's row is deleted by the recording itself, without \
         waiting for a capture to come and read the table"
    );
}
