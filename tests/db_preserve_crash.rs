//! A preservation that a crash interrupts must never cost the user their
//! snapshots — and must never look like a fresh state directory.
//!
//! Moving a database and its two sidecars aside is several file operations,
//! and no filesystem makes several of them one. The failure that matters is
//! the one the process never gets to observe: `state.db` gone, the backup
//! holding the database, the `-wal` still under the old name — after which
//! `open` saw no `state.db`, created one, and the engine ran on an empty
//! database while every snapshot the user had sat in a backup nothing had
//! recorded. The record was written *after* the moves, into the new database,
//! which is the one moment it cannot help.
//!
//! So this crashes a real `osm` process at each individual link, rename and
//! unlink in turn — `OSM_PRESERVE_CRASH_AFTER` exits outright, no unwinding,
//! no `Drop` — and then opens the database properly and demands the data.
//!
//! No tmux server is involved; `--socket` is passed all the same, so nothing
//! here can reach the developer's real one.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The v3 database a user upgrading from an older osm actually has, with one
/// row in the file itself.
fn write_legacy_db(path: &Path) {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch(
        "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
         CREATE TABLE snapshots (
           id INTEGER PRIMARY KEY, taken_at INTEGER NOT NULL,
           boot_id TEXT NOT NULL, reason TEXT NOT NULL, state TEXT NOT NULL);
         INSERT INTO snapshots (id, taken_at, boot_id, reason, state)
           VALUES (1, 100, 'boot-a', 'manual', 'complete');
         INSERT INTO meta (key, value) VALUES ('schema_version', '3');",
    )
    .unwrap();
}

/// Every preserved *database* in `dir` — not its sidecars, not the lock, not
/// the manifest.
fn backup_databases(dir: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            let name = p
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            name.contains(".bak") && !name.ends_with("-wal") && !name.ends_with("-shm")
        })
        .collect();
    found.sort();
    found
}

fn snapshot_count(path: &Path) -> i64 {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.query_row("SELECT COUNT(*) FROM snapshots", [], |r| r.get(0))
        .unwrap()
}

/// A v3 database whose second row is only in its `-wal`, with a reader
/// holding that `-wal` open.
///
/// The reader is deliberately **leaked**: SQLite deletes a `-wal` when the
/// last connection to it closes, and by then this test has moved that file
/// somewhere else on purpose. Leaking it means nothing in this process ever
/// runs SQLite's close path over a database that is no longer where it was.
fn stage_v3_with_a_live_wal(path: &Path) -> rusqlite::Connection {
    write_legacy_db(path);
    let keeper = rusqlite::Connection::open(path).unwrap();
    keeper.pragma_update(None, "journal_mode", "WAL").unwrap();
    keeper
        .execute(
            "INSERT INTO snapshots (id, taken_at, boot_id, reason, state)
             VALUES (2, 200, 'boot-a', 'manual', 'complete')",
            [],
        )
        .unwrap();
    // Inside a read transaction, so the preserving process cannot checkpoint
    // the row into the database file: it has to move the -wal.
    keeper.execute_batch("BEGIN").unwrap();
    keeper
        .query_row("SELECT COUNT(*) FROM snapshots", [], |r| r.get::<_, i64>(0))
        .unwrap();
    for suffix in ["-wal", "-shm"] {
        let side = PathBuf::from(format!("{}{suffix}", path.display()));
        assert!(
            side.exists(),
            "the fixture must actually leave {} behind",
            side.display()
        );
    }
    keeper
}

/// Whether the filesystem the fixtures land on supports hard links.
///
/// The link-then-unlink mode has twice as many crash points as the rename
/// one, so a run on a filesystem without hard links would see the rename
/// profile for both modes and the difference this test is about would
/// silently disappear.
fn hard_links_work() -> bool {
    let tmp = tempfile::tempdir().unwrap();
    let from = tmp.path().join("a");
    std::fs::write(&from, b"x").unwrap();
    std::fs::hard_link(&from, tmp.path().join("b")).is_ok()
}

/// Crash a real `osm` at the n-th file operation of a preservation and then
/// open the database properly. Returns whether the child really did crash
/// there.
///
/// `hard_links` picks the mode: `true` is the link-then-unlink pair every
/// filesystem osm actually runs on takes, `false` is the rename-per-file
/// fallback for the ones with no hard links, which no CI filesystem would
/// ever select on its own.
fn crash_at(n: u32, hard_links: bool) -> bool {
    let mode = if hard_links { "hard links" } else { "renames" };
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("state/osm");
    std::fs::create_dir_all(&state).unwrap();
    let path = state.join("state.db");
    let keeper = stage_v3_with_a_live_wal(&path);

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_osm"));
    cmd.env("XDG_STATE_HOME", tmp.path().join("state"))
        .env("XDG_CONFIG_HOME", tmp.path().join("config"))
        .env("OSM_PRESERVE_CRASH_AFTER", n.to_string());
    if hard_links {
        cmd.env_remove("OSM_PRESERVE_NO_HARDLINK");
    } else {
        cmd.env("OSM_PRESERVE_NO_HARDLINK", "1");
    }
    let out = cmd
        .args([
            "--socket",
            &format!("osm-preserve-crash-{}", std::process::id()),
            "status",
            "--json",
        ])
        .output()
        .unwrap();
    let crashed = out.status.code() == Some(osm::db::PRESERVE_CRASH_EXIT);
    // The reader has done its job — the child could not fold the second row
    // into the database file, so it had to move the `-wal`. Ending the
    // transaction now spares the open below the same wait; the connection
    // itself is still leaked, for the reason above.
    let _ = keeper.execute_batch("COMMIT");
    std::mem::forget(keeper);

    // The whole point: whatever the directory looks like now, the next open
    // must find the user's database rather than start an empty one.
    let conn = osm::db::open(&path)
        .unwrap_or_else(|e| panic!("{mode}: crash after operation {n}: open refused: {e:#}"));

    let preserved = osm::db::preserved(&conn).unwrap_or_else(|| {
        panic!(
            "{mode}: crash after operation {n}: the engine started a fresh database and \
             said nothing about the one it moved aside; directory now holds {:?}",
            std::fs::read_dir(&state)
                .unwrap()
                .flatten()
                .map(|e| e.file_name())
                .collect::<Vec<_>>()
        )
    });
    let backup = PathBuf::from(&preserved.path);
    assert_eq!(
        snapshot_count(&backup),
        2,
        "{mode}: crash after operation {n}: both committed rows must be readable from \
         {}, including the one that was only in its -wal",
        backup.display()
    );
    assert_eq!(
        backup_databases(&state),
        vec![backup.clone()],
        "{mode}: crash after operation {n}: exactly one basename may hold the preserved \
         database"
    );
    assert_eq!(
        snapshot_count(&path),
        0,
        "{mode}: crash after operation {n}: the database taking the old name is the \
         fresh one"
    );
    assert!(
        !state.join("state.db.preserving.json").exists(),
        "{mode}: crash after operation {n}: a reconciled preservation must clear its \
         manifest"
    );
    crashed
}

/// Every operation of both modes, in turn.
///
/// The preservation moves three files — the database and its two sidecars —
/// and does it in one of two ways. With hard links there are **six** points at
/// which the process can die: three links, then three unlinks. Without them
/// there are **three**: one rename per file. Going past the end of either is a
/// preservation that simply finished, which must hold up too, and every one of
/// the points in between is checked by `crash_at` itself.
///
/// The exact sets are asserted, not a lower bound on how many fired. The
/// previous version demanded only that "at least two" injection points fire —
/// which every CI filesystem satisfies with hard links alone, so `rename_all`
/// was never once exercised and four of the six hard-link ticks could have
/// been deleted without turning the test red.
#[test]
fn a_crash_at_any_point_of_a_preservation_still_finds_the_database() {
    assert!(
        hard_links_work(),
        "this test distinguishes the hard-link mode from the rename one, and the \
         filesystem the fixtures land on has no hard links to distinguish it with"
    );

    let fired =
        |hard_links: bool| -> Vec<u32> { (1..=8).filter(|n| crash_at(*n, hard_links)).collect() };

    assert_eq!(
        fired(true),
        vec![1, 2, 3, 4, 5, 6],
        "with hard links the preservation is three links then three unlinks, and the \
         process must be killable at each of them and nowhere after"
    );
    assert_eq!(
        fired(false),
        vec![1, 2, 3],
        "without hard links it is one rename per file, and a crash between two of them \
         leaves the set split — which is the mode whose reconciliation matters most"
    );
}
