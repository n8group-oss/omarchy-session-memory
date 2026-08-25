use osm::{db, snapshots};
use rusqlite::Connection;

fn insert(conn: &Connection, id: i64, at: i64, boot: &str, state: &str) {
    conn.execute(
        "INSERT INTO snapshots (id, taken_at, boot_id, reason, state)
         VALUES (?1, ?2, ?3, 'test', ?4)",
        rusqlite::params![id, at, boot, state],
    )
    .unwrap();
}

fn fresh() -> (tempfile::TempDir, Connection) {
    let tmp = tempfile::tempdir().unwrap();
    let conn = db::open(&tmp.path().join("state.db")).unwrap();
    (tmp, conn)
}

#[test]
fn selects_newest_complete_snapshot_from_a_previous_boot() {
    let (_t, conn) = fresh();
    insert(&conn, 1, 100, "boot-old", "complete");
    insert(&conn, 2, 200, "boot-old", "complete");
    insert(&conn, 3, 300, "boot-now", "complete"); // this boot: must be ignored

    let picked = snapshots::select_restore_source(&conn, "boot-now").unwrap();
    assert_eq!(picked, Some(2));
}

#[test]
fn ignores_incomplete_snapshots() {
    let (_t, conn) = fresh();
    insert(&conn, 1, 100, "boot-old", "complete");
    insert(&conn, 2, 200, "boot-old", "building");
    insert(&conn, 3, 250, "boot-old", "failed");

    let picked = snapshots::select_restore_source(&conn, "boot-now").unwrap();
    assert_eq!(picked, Some(1));
}

#[test]
fn returns_none_when_only_this_boot_has_snapshots() {
    let (_t, conn) = fresh();
    insert(&conn, 1, 100, "boot-now", "complete");
    assert_eq!(
        snapshots::select_restore_source(&conn, "boot-now").unwrap(),
        None
    );
}

#[test]
fn prune_keeps_newest_n_complete_snapshots() {
    let (_t, conn) = fresh();
    for i in 1..=25 {
        insert(&conn, i, i * 10, "boot-old", "complete");
    }
    // Every row is from "boot-old", so relative to boot-old itself none of
    // them is a restore source and plain recency retention applies.
    let deleted = snapshots::prune(&conn, 20, "boot-old").unwrap();
    assert_eq!(deleted, 5);
    let remaining: i64 = conn
        .query_row("SELECT COUNT(*) FROM snapshots", [], |r| r.get(0))
        .unwrap();
    assert_eq!(remaining, 20);
    let oldest: i64 = conn
        .query_row("SELECT MIN(id) FROM snapshots", [], |r| r.get(0))
        .unwrap();
    assert_eq!(oldest, 6, "the five oldest are the ones removed");
}

#[test]
fn prune_never_deletes_a_snapshot_with_a_live_restore_attempt() {
    let (_t, conn) = fresh();
    for i in 1..=25 {
        insert(&conn, i, i * 10, "boot-old", "complete");
    }
    conn.execute(
        "INSERT INTO restore_attempts (snapshot_id, started_at, state)
         VALUES (1, 5, 'running')",
        [],
    )
    .unwrap();

    snapshots::prune(&conn, 20, "boot-old").unwrap();
    let kept: i64 = conn
        .query_row("SELECT COUNT(*) FROM snapshots WHERE id=1", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(kept, 1, "snapshot referenced by a running attempt survives");
}

/// The whole-branch review's Critical 1, reproduced exactly: one previous-boot
/// snapshot buried under a boot's worth of current-boot ones. Retention by
/// recency alone deletes the user's entire pre-reboot layout, and `osm
/// restore` then reports nothing to restore.
///
/// Both pre-existing prune tests used a single boot id, which is precisely why
/// this was invisible to them.
#[test]
fn prune_never_deletes_the_snapshot_a_restore_would_select() {
    let (_t, conn) = fresh();
    insert(&conn, 1, 10, "boot-previous", "complete");
    for i in 2..=26 {
        insert(&conn, i, i * 10, "boot-now", "complete");
    }
    assert_eq!(
        snapshots::select_restore_source(&conn, "boot-now").unwrap(),
        Some(1),
        "precondition: snapshot 1 is what a restore would rebuild from"
    );

    snapshots::prune(&conn, 20, "boot-now").unwrap();

    assert_eq!(
        snapshots::select_restore_source(&conn, "boot-now").unwrap(),
        Some(1),
        "retention must never delete the snapshot a restore would select"
    );
}

/// The exemption protects the row a restore would *currently* pick, not every
/// old row forever: older previous-boot generations still age out normally.
#[test]
fn prune_exempts_only_the_current_restore_source_not_every_old_boot() {
    let (_t, conn) = fresh();
    insert(&conn, 1, 10, "boot-two-ago", "complete");
    insert(&conn, 2, 20, "boot-previous", "complete");
    for i in 3..=27 {
        insert(&conn, i, i * 10, "boot-now", "complete");
    }

    snapshots::prune(&conn, 20, "boot-now").unwrap();

    let survives = |id: i64| -> bool {
        conn.query_row("SELECT COUNT(*) FROM snapshots WHERE id=?1", [id], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap()
            == 1
    };
    assert!(survives(2), "the newest previous-boot snapshot is exempt");
    assert!(
        !survives(1),
        "an older, superseded generation is still reclaimable"
    );
}

/// Deferred-minor 6, promoted to load-bearing by the reclaim fix: a
/// `restore_in_progress` snapshot is no longer dead weight, it is state an
/// interrupted restore is about to rebuild from.
#[test]
fn prune_never_deletes_a_snapshot_in_restore_in_progress() {
    let (_t, conn) = fresh();
    insert(&conn, 1, 10, "boot-previous", "restore_in_progress");
    // A newer previous-boot 'complete' row takes the restore-source exemption,
    // so row 1 can only survive on its own state.
    insert(&conn, 2, 20, "boot-previous", "complete");
    for i in 3..=27 {
        insert(&conn, i, i * 10, "boot-now", "complete");
    }

    snapshots::prune(&conn, 20, "boot-now").unwrap();

    let kept: i64 = conn
        .query_row("SELECT COUNT(*) FROM snapshots WHERE id=1", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(kept, 1, "an interrupted restore's snapshot is reclaimable");
}

/// With nothing restorable, the exempting subquery yields NULL. `id <> NULL`
/// is NULL in SQL, which would make the WHERE clause match nothing and turn
/// retention off entirely — hence `IS NOT` in the query.
#[test]
fn prune_still_prunes_when_there_is_no_restore_source_at_all() {
    let (_t, conn) = fresh();
    for i in 1..=25 {
        insert(&conn, i, i * 10, "boot-now", "complete");
    }
    assert_eq!(
        snapshots::select_restore_source(&conn, "boot-now").unwrap(),
        None
    );

    let deleted = snapshots::prune(&conn, 20, "boot-now").unwrap();
    assert_eq!(deleted, 5);
}

#[test]
fn reclaim_returns_a_wedged_snapshot_to_complete_and_fails_its_attempt() {
    let (_t, mut conn) = fresh();
    insert(&conn, 1, 10, "boot-previous", "restore_in_progress");
    conn.execute(
        "INSERT INTO restore_attempts (snapshot_id, started_at, state)
         VALUES (1, 5, 'running')",
        [],
    )
    .unwrap();
    assert_eq!(
        snapshots::select_restore_source(&conn, "boot-now").unwrap(),
        None,
        "precondition: a wedged snapshot is unreachable"
    );

    let n = snapshots::reclaim_orphaned_restores(&mut conn).unwrap();
    assert_eq!(n, 1);
    assert_eq!(
        snapshots::select_restore_source(&conn, "boot-now").unwrap(),
        Some(1)
    );

    let attempt: (String, Option<i64>) = conn
        .query_row(
            "SELECT state, finished_at FROM restore_attempts WHERE snapshot_id=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(attempt.0, "failed");
    assert!(
        attempt.1.is_some(),
        "a terminal attempt records finished_at"
    );
}

#[test]
fn reclaim_leaves_healthy_snapshots_alone() {
    let (_t, mut conn) = fresh();
    insert(&conn, 1, 10, "boot-previous", "complete");
    insert(&conn, 2, 20, "boot-previous", "restored");
    insert(&conn, 3, 30, "boot-previous", "failed");

    assert_eq!(snapshots::reclaim_orphaned_restores(&mut conn).unwrap(), 0);
    let states: Vec<String> = conn
        .prepare("SELECT state FROM snapshots ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(states, vec!["complete", "restored", "failed"]);
}
