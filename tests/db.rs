use std::os::unix::fs::PermissionsExt;

#[test]
fn open_creates_schema_with_pragmas_and_permissions() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("nested").join("state.db");

    let conn = osm::db::open(&path).expect("open db");

    let fk: i64 = conn
        .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
        .unwrap();
    assert_eq!(fk, 1, "foreign_keys must be ON");

    let mode: String = conn
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode.to_lowercase(), "wal");

    let tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    for expected in [
        "agent_sessions",
        "meta",
        "pane_rows",
        "restore_attempts",
        "restore_objects",
        "session_rows",
        "session_window_links",
        "snapshots",
        "terminal_windows",
        "window_rows",
    ] {
        assert!(tables.contains(&expected.to_string()), "missing {expected}");
    }

    let dir_mode = std::fs::metadata(path.parent().unwrap())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(dir_mode, 0o700);

    let file_mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(file_mode, 0o600);
}

#[test]
fn open_is_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("state.db");
    drop(osm::db::open(&path).unwrap());
    let conn = osm::db::open(&path).expect("second open");
    let v: u32 = conn
        .query_row(
            "SELECT value FROM meta WHERE key='schema_version'",
            [],
            |r| r.get::<_, String>(0).map(|s| s.parse().unwrap()),
        )
        .unwrap();
    assert_eq!(v, osm::db::SCHEMA_VERSION);
}

#[test]
fn foreign_keys_cascade_from_snapshots() {
    let tmp = tempfile::tempdir().unwrap();
    let conn = osm::db::open(&tmp.path().join("state.db")).unwrap();
    conn.execute(
        "INSERT INTO snapshots (id, taken_at, boot_id, reason, state)
         VALUES (1, 100, 'boot-a', 'manual', 'complete')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO session_rows (row_id, snapshot_id, tmux_session_id, name)
         VALUES (1, 1, '$0', 'dev')",
        [],
    )
    .unwrap();
    conn.execute("DELETE FROM snapshots WHERE id = 1", [])
        .unwrap();
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM session_rows", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 0, "session rows must cascade");
}

/// Building a database that says it is version `version`, holding one
/// snapshot row, without going through `osm::db::open` (which would stamp the
/// current version on it).
fn write_legacy_db(path: &std::path::Path, version: Option<u32>) {
    write_legacy_db_with(path, version, 1)
}

/// The same, with `rows` snapshot rows in it, so a test can tell the user's
/// database apart from an empty one that merely has the same name.
fn write_legacy_db_with(path: &std::path::Path, version: Option<u32>, rows: i64) {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch(
        "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
         CREATE TABLE snapshots (
           id INTEGER PRIMARY KEY, taken_at INTEGER NOT NULL,
           boot_id TEXT NOT NULL, reason TEXT NOT NULL, state TEXT NOT NULL);",
    )
    .unwrap();
    for id in 1..=rows {
        conn.execute(
            "INSERT INTO snapshots (id, taken_at, boot_id, reason, state)
             VALUES (?1, 100, 'boot-a', 'manual', 'complete')",
            [id],
        )
        .unwrap();
    }
    if let Some(v) = version {
        conn.execute(
            "INSERT INTO meta (key, value) VALUES ('schema_version', ?1)",
            [v.to_string()],
        )
        .unwrap();
    }
}

/// Every file in `dir` that is a preserved *database* — not one of its WAL
/// sidecars, and not the schema lock.
fn backup_databases(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut found: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
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

fn snapshot_count(path: &std::path::Path) -> i64 {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.query_row("SELECT COUNT(*) FROM snapshots", [], |r| r.get(0))
        .unwrap()
}

/// The regression: `open` used to `DROP` every table whenever the recorded
/// schema version was not this build's. It runs on *every* subcommand, so a
/// user who upgraded `osm` and merely typed `osm status` lost their only
/// snapshot of the pre-reboot state — before a restore ever ran.
#[test]
fn a_v1_database_is_preserved_not_destroyed() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("state.db");
    write_legacy_db(&path, Some(1));

    let conn = osm::db::open(&path).expect("open must succeed against an old database");

    // The new database really is this build's, and empty.
    let fresh: i64 = conn
        .query_row("SELECT COUNT(*) FROM snapshots", [], |r| r.get(0))
        .unwrap();
    assert_eq!(fresh, 0);

    let preserved = osm::db::preserved(&conn).expect("the preservation must be recorded");
    assert_eq!(preserved.schema_version, Some(1));

    let backup = std::path::PathBuf::from(&preserved.path);
    assert!(backup.exists(), "{} must exist", backup.display());
    assert_eq!(
        backup.file_name().unwrap().to_string_lossy(),
        "state.db.v1.bak",
        "the backup must sit beside the database under an obvious name"
    );
    assert_eq!(
        snapshot_count(&backup),
        1,
        "the old database's rows must still be there"
    );
}

#[test]
fn a_database_from_a_newer_build_is_preserved_too() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("state.db");
    write_legacy_db(&path, Some(osm::db::SCHEMA_VERSION + 5));

    let conn = osm::db::open(&path).unwrap();
    let preserved = osm::db::preserved(&conn).expect("a downgrade must preserve too");
    assert_eq!(preserved.schema_version, Some(osm::db::SCHEMA_VERSION + 5));
    assert_eq!(snapshot_count(std::path::Path::new(&preserved.path)), 1);
}

#[test]
fn a_database_with_no_recorded_version_is_preserved() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("state.db");
    write_legacy_db(&path, None);

    let conn = osm::db::open(&path).unwrap();
    let preserved = osm::db::preserved(&conn).expect("an unversioned database must be preserved");
    assert_eq!(preserved.schema_version, None);
    assert!(preserved.path.ends_with("state.db.unversioned.bak"));
    assert_eq!(snapshot_count(std::path::Path::new(&preserved.path)), 1);
}

/// Two upgrades, two databases worth keeping. Overwriting the first backup
/// with the second would reintroduce the data loss one release later.
#[test]
fn a_second_preservation_does_not_overwrite_the_first_backup() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("state.db");

    write_legacy_db(&path, Some(1));
    let first = osm::db::preserved(&osm::db::open(&path).unwrap())
        .unwrap()
        .path;
    std::fs::remove_file(&path).unwrap();
    write_legacy_db(&path, Some(1));
    let second = osm::db::preserved(&osm::db::open(&path).unwrap())
        .unwrap()
        .path;

    assert_ne!(
        first, second,
        "the second backup must not clobber the first"
    );
    assert_eq!(snapshot_count(std::path::Path::new(&first)), 1);
    assert_eq!(snapshot_count(std::path::Path::new(&second)), 1);
}

/// An ordinary database opens in place and reports nothing preserved.
#[test]
fn a_current_database_is_not_moved_aside() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("state.db");
    {
        let conn = osm::db::open(&path).unwrap();
        conn.execute(
            "INSERT INTO snapshots (id, taken_at, boot_id, reason, state)
             VALUES (1, 100, 'boot-a', 'manual', 'complete')",
            [],
        )
        .unwrap();
    }
    let conn = osm::db::open(&path).unwrap();
    assert!(osm::db::preserved(&conn).is_none());
    let kept: i64 = conn
        .query_row("SELECT COUNT(*) FROM snapshots", [], |r| r.get(0))
        .unwrap();
    assert_eq!(kept, 1, "reopening must never disturb existing snapshots");
}

/// `open` runs in every `osm` process and tmux fires hooks in parallel, so
/// several processes create the database at once. Creating the tables and
/// stamping the version in two steps let a second opener see `snapshots`
/// without a `schema_version`, call that an incompatible database, and move
/// the one the first opener was still building aside.
#[test]
fn concurrent_opens_do_not_preserve_each_other() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("state.db");

    let preserved: Vec<Option<String>> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let path = path.clone();
                s.spawn(move || {
                    let conn = osm::db::open(&path).expect("open");
                    osm::db::preserved(&conn).map(|p| p.path)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    assert!(
        preserved.iter().all(Option::is_none),
        "a concurrent opener moved the database aside: {preserved:?}"
    );
    let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".bak"))
        .collect();
    assert!(leftovers.is_empty(), "stray backups: {leftovers:?}");
}

/// The tables an osm database had before schema 6, in the shapes the
/// migration has to work on: `session_rows` with no `unresolved` column,
/// `restore_attempts` with the narrow `CHECK`, `restore_objects` holding a
/// foreign key into it, and `window_rows` with no `auto_named`. Everything else
/// `open` creates itself with `IF NOT EXISTS`, so only what the migration
/// touches is spelled out.
const PRE_V6_TABLES: &str = "
    CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
    CREATE TABLE window_rows (
      row_id         INTEGER PRIMARY KEY,
      snapshot_id    INTEGER NOT NULL REFERENCES snapshots(id) ON DELETE CASCADE,
      tmux_window_id TEXT    NOT NULL,
      name           TEXT    NOT NULL,
      layout         TEXT    NOT NULL,
      active_pane_id TEXT,
      zoomed         INTEGER NOT NULL DEFAULT 0 CHECK (zoomed IN (0,1)),
      UNIQUE (snapshot_id, tmux_window_id));
    CREATE TABLE session_rows (
      row_id           INTEGER PRIMARY KEY,
      snapshot_id      INTEGER NOT NULL REFERENCES snapshots(id) ON DELETE CASCADE,
      tmux_session_id  TEXT    NOT NULL,
      name             TEXT    NOT NULL,
      active_window_id TEXT,
      UNIQUE (snapshot_id, tmux_session_id));
    CREATE TABLE restore_attempts (
      id          INTEGER PRIMARY KEY,
      snapshot_id INTEGER NOT NULL REFERENCES snapshots(id) ON DELETE CASCADE,
      started_at  INTEGER NOT NULL,
      finished_at INTEGER,
      state       TEXT    NOT NULL
                  CHECK (state IN ('running','succeeded','partial','failed')));
    CREATE TABLE restore_objects (
      row_id     INTEGER PRIMARY KEY,
      attempt_id INTEGER NOT NULL REFERENCES restore_attempts(id) ON DELETE CASCADE,
      kind       TEXT    NOT NULL,
      ref        TEXT    NOT NULL,
      state      TEXT    NOT NULL,
      detail     TEXT,
      UNIQUE (attempt_id, kind, ref));
";

/// Version 4 differs from 5 by one added column with a default, and 5 from 6
/// by two more plus a table rebuild, so both are migrated in place: the user
/// keeps their snapshots *and* their file.
#[test]
fn a_v4_database_is_migrated_in_place() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("state.db");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(&format!(
            "CREATE TABLE snapshots (
               id INTEGER PRIMARY KEY, taken_at INTEGER NOT NULL,
               boot_id TEXT NOT NULL, reason TEXT NOT NULL,
               state TEXT NOT NULL
                     CHECK (state IN ('building','complete','restore_in_progress',
                                      'restored','failed')));
             {PRE_V6_TABLES}
             INSERT INTO snapshots (id, taken_at, boot_id, reason, state)
               VALUES (1, 100, 'boot-a', 'manual', 'complete');
             INSERT INTO session_rows (row_id, snapshot_id, tmux_session_id, name)
               VALUES (1, 1, '$0', 'dev');
             INSERT INTO restore_attempts (id, snapshot_id, started_at, state)
               VALUES (1, 1, 100, 'partial');
             INSERT INTO restore_objects (attempt_id, kind, ref, state)
               VALUES (1, 'session', 'dev', 'failed');
             INSERT INTO meta (key, value) VALUES ('schema_version', '4');"
        ))
        .unwrap();
    }

    let conn = osm::db::open(&path).expect("a v4 database must open");
    assert!(
        osm::db::preserved(&conn).is_none(),
        "a migratable database must not be moved aside"
    );
    let kept: i64 = conn
        .query_row("SELECT COUNT(*) FROM snapshots", [], |r| r.get(0))
        .unwrap();
    assert_eq!(kept, 1, "the migration must keep the existing snapshots");
    let unresolved: i64 = conn
        .query_row("SELECT unresolved FROM snapshots WHERE id=1", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(unresolved, 0, "existing rows default to resolved");
    let attempts: i64 = conn
        .query_row("SELECT COUNT(*) FROM restore_attempts", [], |r| r.get(0))
        .unwrap();
    assert_eq!(attempts, 1, "the rebuilt attempts table must keep its rows");
    let objects: i64 = conn
        .query_row("SELECT COUNT(*) FROM restore_objects", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        objects, 1,
        "dropping restore_attempts to widen its CHECK must not cascade away \
         the per-object rows that point at it"
    );
    let version: u32 = conn
        .query_row(
            "SELECT value FROM meta WHERE key='schema_version'",
            [],
            |r| r.get::<_, String>(0).map(|v| v.parse().unwrap()),
        )
        .unwrap();
    assert_eq!(version, osm::db::SCHEMA_VERSION);
}

/// A v5 database recorded its carry-forward debt as one flag on the whole
/// snapshot. Migrating it must push that debt down onto every session row of
/// the snapshots that carried it, and not quietly discharge it: v5 said "there
/// is something outstanding in here" without saying what, so the only safe
/// reading is that all of it is.
#[test]
fn a_v5_database_keeps_its_carry_debt_through_the_migration() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("state.db");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(&format!(
            "CREATE TABLE snapshots (
               id INTEGER PRIMARY KEY, taken_at INTEGER NOT NULL,
               boot_id TEXT NOT NULL, reason TEXT NOT NULL,
               state TEXT NOT NULL
                     CHECK (state IN ('building','complete','restore_in_progress',
                                      'restored','failed')),
               unresolved INTEGER NOT NULL DEFAULT 0 CHECK (unresolved IN (0,1)));
             {PRE_V6_TABLES}
             INSERT INTO snapshots (id, taken_at, boot_id, reason, state, unresolved)
               VALUES (1, 100, 'boot-a', 'manual', 'complete', 1),
                      (2, 200, 'boot-a', 'manual', 'complete', 0);
             INSERT INTO session_rows (row_id, snapshot_id, tmux_session_id, name)
               VALUES (1, 1, '$0', 'alpha'), (2, 1, '$1', 'beta'),
                      (3, 2, '$0', 'gamma');
             INSERT INTO meta (key, value) VALUES ('schema_version', '5');"
        ))
        .unwrap();
    }

    let conn = osm::db::open(&path).expect("a v5 database must open");
    assert!(osm::db::preserved(&conn).is_none());

    let owed: Vec<String> = conn
        .prepare("SELECT name FROM session_rows WHERE unresolved = 1 ORDER BY name")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        owed,
        vec!["alpha".to_string(), "beta".to_string()],
        "every session of an unresolved v5 snapshot must still be owed"
    );

    // The widened CHECK is really there.
    conn.execute(
        "INSERT INTO restore_attempts (snapshot_id, started_at, state)
         VALUES (1, 300, 'unsecured')",
        [],
    )
    .expect("the migrated attempts table must accept the new state");
}
/// The bug the empty-database test above cannot reach, because there is
/// nothing to preserve in it.
///
/// Thirty-two processes opening one **populated, older** database at once used
/// to run classify → checkpoint → rename → create with no mutual exclusion, and
/// produced ten preservation operations: the user's snapshot landed in
/// `state.db.v3.bak` and `.bak.1`…`.bak.9` were *empty* databases, each caught
/// mid-creation by another opener and moved aside as if it were data.
/// `osm status --json` then pointed the user at the last of those empty files
/// and called it their preserved database.
#[test]
fn concurrent_opens_of_a_populated_old_database_preserve_it_exactly_once() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("state.db");
    write_legacy_db_with(&path, Some(3), 7);

    let reported: Vec<Option<String>> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..32)
            .map(|_| {
                let path = path.clone();
                s.spawn(move || {
                    let conn = osm::db::open(&path).expect("open");
                    osm::db::preserved(&conn).map(|p| p.path)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let backups = backup_databases(tmp.path());
    assert_eq!(
        backups.len(),
        1,
        "one incompatible database must be preserved once, got {backups:?}"
    );
    assert_eq!(
        snapshot_count(&backups[0]),
        7,
        "the preserved database must be the user's, not an empty one caught \
         mid-creation: {}",
        backups[0].display()
    );

    let distinct: std::collections::BTreeSet<String> = reported.into_iter().flatten().collect();
    assert_eq!(
        distinct.len(),
        1,
        "every opener must be told about the same preserved database, got {distinct:?}"
    );
    assert_eq!(
        distinct.iter().next().unwrap(),
        &backups[0].display().to_string(),
        "the reported path must be the file that actually holds the snapshots"
    );
}

/// A `-wal` beside an incompatible database holds committed transactions that
/// are not in the database file. It has to travel with it, or the fresh
/// database taking that name inherits another file's journal — and the
/// failure to move it used to be discarded with `let _ =`.
#[test]
fn a_wal_sidecar_travels_with_the_database_it_belongs_to() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("state.db");
    write_legacy_db_with(&path, Some(3), 1);

    // A second connection, kept open in WAL mode, so the sidecar is really on
    // disk at the moment `open` decides to move the database aside.
    let keeper = rusqlite::Connection::open(&path).unwrap();
    keeper.pragma_update(None, "journal_mode", "WAL").unwrap();
    keeper
        .execute(
            "INSERT INTO snapshots (id, taken_at, boot_id, reason, state)
             VALUES (2, 200, 'boot-a', 'manual', 'complete')",
            [],
        )
        .unwrap();
    assert!(
        tmp.path().join("state.db-wal").exists(),
        "the fixture must actually leave a -wal behind"
    );

    let conn = osm::db::open(&path).expect("open must preserve rather than fail");
    drop(keeper);

    let preserved = osm::db::preserved(&conn).expect("the preservation must be recorded");
    let backup = std::path::PathBuf::from(&preserved.path);
    assert_eq!(
        snapshot_count(&backup),
        2,
        "both committed rows must be readable from the preserved database, \
         whether they came through the checkpoint or through the -wal"
    );
}

/// The preservation has to reserve the database name **and both sidecar
/// names** as one unit.
///
/// `state.db.v3.bak` free and `state.db.v3.bak-wal` occupied was enough to
/// split a database from its journal: the check looked only at the main name,
/// so the database moved successfully and the `-wal` move then failed — leaving
/// the two under different basenames, with a fresh `state.db` free to be
/// created beside the orphan and the preservation recorded nowhere.
#[test]
fn an_occupied_sidecar_name_never_splits_a_database_from_its_journal() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("state.db");
    write_legacy_db_with(&path, Some(3), 1);

    // A second connection, kept open in WAL mode with a read transaction, so
    // the sidecar is on disk *and* holds the second row: the checkpoint below
    // cannot fold it into the database file while this reader is inside one.
    let keeper = rusqlite::Connection::open(&path).unwrap();
    keeper.pragma_update(None, "journal_mode", "WAL").unwrap();
    keeper
        .execute(
            "INSERT INTO snapshots (id, taken_at, boot_id, reason, state)
             VALUES (2, 200, 'boot-a', 'manual', 'complete')",
            [],
        )
        .unwrap();
    keeper.execute_batch("BEGIN").unwrap();
    keeper
        .query_row("SELECT COUNT(*) FROM snapshots", [], |r| r.get::<_, i64>(0))
        .unwrap();
    assert!(
        tmp.path().join("state.db-wal").exists(),
        "the fixture must actually leave a -wal behind"
    );

    // An earlier preservation left its journal under the name this one would
    // otherwise choose. Only the sidecar name is taken; the database name
    // itself is free, which is exactly what the check used to miss.
    let decoy = tmp.path().join("state.db.v3.bak-wal");
    let decoy_bytes = b"another database's journal".to_vec();
    std::fs::write(&decoy, &decoy_bytes).unwrap();

    let conn = osm::db::open(&path).expect("open must preserve rather than fail");

    let preserved = osm::db::preserved(&conn).expect("the preservation must be recorded");
    let backup = std::path::PathBuf::from(&preserved.path);
    // Before the keeper is closed, so nothing has had a chance to fold the
    // journal into the database file: this is what says the row below really
    // did travel in the sidecar rather than in the main file.
    let moved_wal = std::path::PathBuf::from(format!("{}-wal", backup.display()));
    assert!(
        std::fs::metadata(&moved_wal).map(|m| m.len()).unwrap_or(0) > 0,
        "{} must hold the committed transaction the database file does not",
        moved_wal.display()
    );
    drop(keeper);

    assert_ne!(
        backup,
        tmp.path().join("state.db.v3.bak"),
        "a base name whose sidecar is occupied is not free"
    );
    assert!(
        !tmp.path().join("state.db.v3.bak").exists(),
        "nothing may be written next to another database's journal"
    );
    assert_eq!(
        std::fs::read(&decoy).unwrap(),
        decoy_bytes,
        "the occupied sidecar belongs to an earlier preservation and must be \
         left exactly as it was"
    );
    assert_eq!(
        snapshot_count(&backup),
        2,
        "every committed row must still be readable from the preserved \
         database, including the one that was only in its -wal"
    );
}

/// A v6 database is what a user upgrading to this build actually has: the
/// per-session carry debt and the window map, but no record of *which* tmux
/// server any of it belonged to.
///
/// The migration adds that record as two nullable columns, so every existing
/// row reads as "no identity" — which is what makes an old window map stop
/// being believed rather than start being believed against the wrong server.
#[test]
fn a_v6_database_gains_the_server_columns_without_claiming_an_identity() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("state.db");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE snapshots (
               id INTEGER PRIMARY KEY, taken_at INTEGER NOT NULL,
               boot_id TEXT NOT NULL, reason TEXT NOT NULL,
               state TEXT NOT NULL
                     CHECK (state IN ('building','complete','restore_in_progress',
                                      'restored','failed')),
               unresolved INTEGER NOT NULL DEFAULT 0 CHECK (unresolved IN (0,1)));
             CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE session_rows (
               row_id           INTEGER PRIMARY KEY,
               snapshot_id      INTEGER NOT NULL REFERENCES snapshots(id) ON DELETE CASCADE,
               tmux_session_id  TEXT    NOT NULL,
               name             TEXT    NOT NULL,
               active_window_id TEXT,
               unresolved INTEGER NOT NULL DEFAULT 0 CHECK (unresolved IN (0,1)));
             CREATE TABLE restore_attempts (
               id          INTEGER PRIMARY KEY,
               snapshot_id INTEGER NOT NULL REFERENCES snapshots(id) ON DELETE CASCADE,
               started_at  INTEGER NOT NULL,
               finished_at INTEGER,
               state       TEXT    NOT NULL
                           CHECK (state IN ('running','succeeded','partial','failed',
                                            'unsecured')));
             CREATE TABLE restore_window_map (
               row_id             INTEGER PRIMARY KEY,
               attempt_id         INTEGER NOT NULL
                                  REFERENCES restore_attempts(id) ON DELETE CASCADE,
               captured_window_id TEXT    NOT NULL,
               live_window_id     TEXT    NOT NULL,
               UNIQUE (attempt_id, captured_window_id));
             CREATE TABLE window_rows (
               row_id         INTEGER PRIMARY KEY,
               snapshot_id    INTEGER NOT NULL REFERENCES snapshots(id) ON DELETE CASCADE,
               tmux_window_id TEXT    NOT NULL,
               name           TEXT    NOT NULL,
               layout         TEXT    NOT NULL,
               active_pane_id TEXT,
               zoomed         INTEGER NOT NULL DEFAULT 0 CHECK (zoomed IN (0,1)),
               UNIQUE (snapshot_id, tmux_window_id));
             INSERT INTO snapshots (id, taken_at, boot_id, reason, state, unresolved)
               VALUES (1, 100, 'boot-a', 'manual', 'complete', 1);
             INSERT INTO session_rows (row_id, snapshot_id, tmux_session_id, name, unresolved)
               VALUES (1, 1, '$0', 'beta', 1);
             INSERT INTO restore_attempts (id, snapshot_id, started_at, state)
               VALUES (1, 1, 100, 'partial');
             INSERT INTO restore_window_map (attempt_id, captured_window_id, live_window_id)
               VALUES (1, '@1', '@1');
             INSERT INTO meta (key, value) VALUES ('schema_version', '6');",
        )
        .unwrap();
    }

    let conn = osm::db::open(&path).expect("a v6 database must open");
    assert!(
        osm::db::preserved(&conn).is_none(),
        "a migratable database must not be moved aside"
    );
    let owed: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM session_rows WHERE unresolved = 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(owed, 1, "the migration must not discharge carry debt");
    let mappings: i64 = conn
        .query_row("SELECT COUNT(*) FROM restore_window_map", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mappings, 1, "the window map must survive the migration");

    let identities: (Option<String>, Option<String>) = conn
        .query_row(
            "SELECT (SELECT server FROM snapshots WHERE id = 1),
                    (SELECT destination_server FROM restore_attempts WHERE id = 1)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        identities,
        (None, None),
        "a row written before the columns existed must claim no server"
    );

    let version: u32 = conn
        .query_row(
            "SELECT value FROM meta WHERE key='schema_version'",
            [],
            |r| r.get::<_, String>(0).map(|v| v.parse().unwrap()),
        )
        .unwrap();
    assert_eq!(version, osm::db::SCHEMA_VERSION);
}

/// A v8 database is what this build's predecessor wrote: window rows with a
/// name, and no record of whether the *user* chose it or tmux was still
/// deriving it.
///
/// The migration adds that record as one nullable column, and every existing
/// row keeps NULL — "this row cannot say". Equivalence reads that as "the name
/// is not an identity" and skips it, which is the only safe reading: those rows
/// were written by a build that could not tell a chosen name from a derived
/// one, so the `bash` in them may be either. Defaulting them to "the user chose
/// it" would leave every snapshot already on disk exposed to the failure the
/// column exists to prevent — a snapshot pinned out of retention forever
/// because tmux renamed a window after the capture.
#[test]
fn a_v8_database_gains_the_name_ownership_column_without_claiming_an_answer() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("state.db");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE snapshots (
               id INTEGER PRIMARY KEY, taken_at INTEGER NOT NULL,
               boot_id TEXT NOT NULL, reason TEXT NOT NULL,
               state TEXT NOT NULL
                     CHECK (state IN ('building','complete','restore_in_progress',
                                      'restored','failed')),
               unresolved INTEGER NOT NULL DEFAULT 0 CHECK (unresolved IN (0,1)),
               server TEXT);
             CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE window_rows (
               row_id         INTEGER PRIMARY KEY,
               snapshot_id    INTEGER NOT NULL REFERENCES snapshots(id) ON DELETE CASCADE,
               tmux_window_id TEXT    NOT NULL,
               name           TEXT    NOT NULL,
               layout         TEXT    NOT NULL,
               active_pane_id TEXT,
               zoomed         INTEGER NOT NULL DEFAULT 0 CHECK (zoomed IN (0,1)),
               UNIQUE (snapshot_id, tmux_window_id));
             INSERT INTO snapshots (id, taken_at, boot_id, reason, state, unresolved)
               VALUES (1, 100, 'boot-a', 'manual', 'complete', 1);
             INSERT INTO window_rows (row_id, snapshot_id, tmux_window_id, name, layout)
               VALUES (1, 1, '@0', 'bash', 'abcd,80x24,0,0,0');
             INSERT INTO meta (key, value) VALUES ('schema_version', '8');",
        )
        .unwrap();
    }

    let conn = osm::db::open(&path).expect("a v8 database must open");
    assert!(
        osm::db::preserved(&conn).is_none(),
        "a migratable database must not be moved aside"
    );
    let (name, auto): (String, Option<i64>) = conn
        .query_row(
            "SELECT name, auto_named FROM window_rows WHERE row_id = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(name, "bash", "the migration must keep the captured name");
    assert_eq!(
        auto, None,
        "a row written before the column existed must not claim to know who \
         named the window"
    );

    let version: u32 = conn
        .query_row(
            "SELECT value FROM meta WHERE key='schema_version'",
            [],
            |r| r.get::<_, String>(0).map(|v| v.parse().unwrap()),
        )
        .unwrap();
    assert_eq!(version, osm::db::SCHEMA_VERSION);
}

/// v10 recorded a `summary` column on `agent_sessions` that nothing ever
/// wrote — the design's "opt-in only, off by default" paraphrase of a
/// conversation, which was never built. v11 replaces it with what is actually
/// recorded: a one-line `title` and the `title_source` that says whether the
/// agent wrote it or it came from the user's first prompt.
///
/// The rename is deliberate rather than a reuse. `summary` promised a
/// paraphrase of a conversation's contents; a title is not that, and a column
/// whose name says otherwise is how the next reader concludes osm stores
/// transcript summaries.
#[test]
fn a_v10_database_gains_the_conversation_title_columns() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("state.db");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE snapshots (
               id INTEGER PRIMARY KEY, taken_at INTEGER NOT NULL,
               boot_id TEXT NOT NULL, reason TEXT NOT NULL,
               state TEXT NOT NULL
                     CHECK (state IN ('building','complete','restore_in_progress',
                                      'restored','failed')),
               unresolved INTEGER NOT NULL DEFAULT 0 CHECK (unresolved IN (0,1)),
               server TEXT);
             CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE agent_sessions (
               row_id      INTEGER PRIMARY KEY,
               kind        TEXT    NOT NULL,
               native_id   TEXT    NOT NULL,
               project_dir TEXT,
               store_path  TEXT,
               last_active INTEGER,
               size_bytes  INTEGER,
               summary     TEXT,
               alive       INTEGER NOT NULL DEFAULT 0 CHECK (alive IN (0,1)),
               UNIQUE (kind, native_id));
             INSERT INTO snapshots (id, taken_at, boot_id, reason, state)
               VALUES (1, 100, 'boot-a', 'manual', 'complete');
             INSERT INTO agent_sessions (kind, native_id, project_dir)
               VALUES ('claude', 'c1', '/home/u/app');
             INSERT INTO meta (key, value) VALUES ('schema_version', '10');",
        )
        .unwrap();
    }

    let conn = osm::db::open(&path).expect("a v10 database must open");
    assert!(
        osm::db::preserved(&conn).is_none(),
        "a migratable database must not be moved aside: the snapshots in it \
         are the user's only record of where their sessions were"
    );

    let (project, title, source): (String, Option<String>, Option<String>) = conn
        .query_row(
            "SELECT project_dir, title, title_source FROM agent_sessions WHERE native_id = 'c1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .expect("the row survives the migration with the new columns");
    assert_eq!(project, "/home/u/app", "the migration keeps what was there");
    assert_eq!(
        title, None,
        "a row written before titles existed has none, and must not claim one"
    );
    assert_eq!(source, None);

    let version: u32 = conn
        .query_row(
            "SELECT value FROM meta WHERE key='schema_version'",
            [],
            |r| r.get::<_, String>(0).map(|v| v.parse().unwrap()),
        )
        .unwrap();
    assert_eq!(version, osm::db::SCHEMA_VERSION);
}

/// A preserved database whose most recent transactions are still in its
/// `-wal` is counted **with** them.
///
/// `osm status` reads the preserved file to say what is in it, and reads it
/// with `immutable=1` so that a panel polling every five seconds does not
/// create SQLite's shared-memory index beside a backup it has no other
/// business with. `immutable=1` also ignores a `-wal` — so a file whose rows
/// are all in one would be reported as holding nothing, which is the same
/// untruth as the claim this replaced, pointing the other way. Preservation
/// checkpoints before it moves a database, but the one that could not
/// (see `preserve_aside`) carries its `-wal` along with it.
#[test]
fn a_preserved_database_holding_its_rows_in_a_wal_is_counted_with_them() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db.v1.bak");
    // Held open for the whole test: the last connection to close checkpoints
    // the WAL back into the file, which is exactly the state this is not
    // about.
    let held = rusqlite::Connection::open(&path).unwrap();
    held.pragma_update(None, "journal_mode", "WAL").unwrap();
    held.execute_batch(
        "CREATE TABLE snapshots (
           id INTEGER PRIMARY KEY, taken_at INTEGER NOT NULL,
           boot_id TEXT NOT NULL, reason TEXT NOT NULL, state TEXT NOT NULL);
         INSERT INTO snapshots VALUES (1, 100, 'boot-a', 'manual', 'complete');
         INSERT INTO snapshots VALUES (2, 200, 'boot-a', 'manual', 'complete');",
    )
    .unwrap();

    let wal = dir.path().join("state.db.v1.bak-wal");
    assert!(
        std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0) > 0,
        "the fixture checkpointed itself, so this proves nothing"
    );
    assert_eq!(
        osm::db::preserved_contents(&path),
        osm::db::PreservedContents::Snapshots(2),
        "the rows in the -wal were not counted"
    );
}

/// A v11 database gains `snapshots.placement_state`, and every row already in
/// it is called `known`.
///
/// Not `unknown`, which would be the cautious-looking answer and is the wrong
/// one. Under v11 a capture that could not read placement was **refused**, so
/// no v11 row can be an unknown-placement snapshot: every one of them either
/// carries the placement the compositor reported or was taken with the
/// compositor deliberately not asked. Stamping them `unknown` would hand
/// retention a fleet of rows to protect and would make a restore of any of
/// them go looking for placement somewhere else — inventing a doubt the old
/// build never had.
#[test]
fn a_v11_database_gains_the_placement_state_column_as_known() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("state.db");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE snapshots (
               id INTEGER PRIMARY KEY, taken_at INTEGER NOT NULL,
               boot_id TEXT NOT NULL, reason TEXT NOT NULL,
               state TEXT NOT NULL
                     CHECK (state IN ('building','complete','restore_in_progress',
                                      'restored','failed')),
               unresolved INTEGER NOT NULL DEFAULT 0 CHECK (unresolved IN (0,1)),
               server TEXT);
             CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO snapshots (id, taken_at, boot_id, reason, state)
               VALUES (1, 100, 'boot-a', 'manual', 'complete');
             INSERT INTO meta (key, value) VALUES ('schema_version', '11');",
        )
        .unwrap();
    }

    let conn = osm::db::open(&path).expect("a v11 database must open");
    assert!(
        osm::db::preserved(&conn).is_none(),
        "a migratable database must not be moved aside"
    );

    let state: String = conn
        .query_row(
            "SELECT placement_state FROM snapshots WHERE id = 1",
            [],
            |r| r.get(0),
        )
        .expect("the column exists after the migration");
    assert_eq!(
        state, "known",
        "a row written by a build that refused to record unknown placement \
         cannot be an unknown-placement row"
    );

    let version: u32 = conn
        .query_row(
            "SELECT value FROM meta WHERE key='schema_version'",
            [],
            |r| r.get::<_, String>(0).map(|v| v.parse().unwrap()),
        )
        .unwrap();
    assert_eq!(version, osm::db::SCHEMA_VERSION);
}

/// A v12 database is rebuilt so its snapshot ids are `AUTOINCREMENT`, and
/// **every snapshot keeps the id it had**.
///
/// The id is not an internal detail: every `snapshot_id` in the database is
/// one, so renumbering would detach every session, window and placement from
/// the snapshot it belongs to. The rebuild copies the ids across explicitly,
/// which also seeds `sqlite_sequence` with the highest of them — so the next
/// capture continues the user's history rather than restarting it at 1 on top
/// of rows that are still there.
#[test]
fn a_v12_database_keeps_every_snapshot_id_and_stops_reusing_them() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("state.db");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE snapshots (
               id INTEGER PRIMARY KEY, taken_at INTEGER NOT NULL,
               boot_id TEXT NOT NULL, reason TEXT NOT NULL,
               state TEXT NOT NULL
                     CHECK (state IN ('building','complete','restore_in_progress',
                                      'restored','failed')),
               unresolved INTEGER NOT NULL DEFAULT 0 CHECK (unresolved IN (0,1)),
               server TEXT,
               placement_state TEXT NOT NULL DEFAULT 'known'
                               CHECK (placement_state IN ('known','unknown','disabled')));
             CREATE TABLE session_rows (
               row_id INTEGER PRIMARY KEY,
               snapshot_id INTEGER NOT NULL REFERENCES snapshots(id) ON DELETE CASCADE,
               tmux_session_id TEXT NOT NULL, name TEXT NOT NULL,
               active_window_id TEXT,
               unresolved INTEGER NOT NULL DEFAULT 0 CHECK (unresolved IN (0,1)));
             CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO snapshots (id, taken_at, boot_id, reason, state, placement_state)
               VALUES (3706, 100, 'boot-a', 'timer', 'complete', 'known'),
                      (3716, 200, 'boot-a', 'timer', 'complete', 'unknown');
             INSERT INTO session_rows (row_id, snapshot_id, tmux_session_id, name)
               VALUES (1, 3706, '$0', 'alpha'), (2, 3716, '$0', 'alpha');
             INSERT INTO meta (key, value) VALUES ('schema_version', '12');",
        )
        .unwrap();
    }

    let conn = osm::db::open(&path).expect("a v12 database must open");
    assert!(
        osm::db::preserved(&conn).is_none(),
        "a migratable database must not be moved aside"
    );

    let ids: Vec<i64> = conn
        .prepare("SELECT id FROM snapshots ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(ids, vec![3706, 3716], "every snapshot keeps its own id");

    // The rest of each row travelled with it, including the column the
    // previous migration added.
    let placement: String = conn
        .query_row(
            "SELECT placement_state FROM snapshots WHERE id = 3716",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(placement, "unknown");

    // And the children still name the snapshots they always named.
    let attached: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM session_rows s JOIN snapshots n ON n.id = s.snapshot_id",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(attached, 2, "the rebuild must not detach the child rows");

    // The highest id on record is gone, and the id it had must not come back.
    conn.execute("DELETE FROM snapshots WHERE id = 3716", [])
        .unwrap();
    conn.execute(
        "INSERT INTO snapshots (taken_at, boot_id, reason, state)
         VALUES (300, 'boot-a', 'timer', 'complete')",
        [],
    )
    .unwrap();
    let next: i64 = conn
        .query_row("SELECT MAX(id) FROM snapshots", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        next, 3717,
        "the migrated sequence must continue past every id the database has \
         ever handed out, not restart from what is left"
    );

    let version: u32 = conn
        .query_row(
            "SELECT value FROM meta WHERE key='schema_version'",
            [],
            |r| r.get::<_, String>(0).map(|v| v.parse().unwrap()),
        )
        .unwrap();
    assert_eq!(version, osm::db::SCHEMA_VERSION);
}

/// Deleting a snapshot takes its rows with it **on a connection that
/// enforces no foreign keys**.
///
/// This is the defect that cost the maintainer 83 minutes, one level below
/// the id reuse that made it fatal. `ON DELETE CASCADE` is not a property of
/// the database: it is a property of whoever is connected to it.
/// `PRAGMA foreign_keys` defaults **off** in stock SQLite — the `sqlite3`
/// shell, any tool built on the system library, a future osm path that
/// switches it off for a table rebuild and does not switch it back — so a
/// snapshot deleted through any of them leaves every child row behind, and
/// `PRAGMA foreign_key_check` is the only thing that ever notices. His
/// database held 198 such rows: 72 `window_rows`, 72 `session_rows`, 54
/// `terminal_windows`, claiming nine snapshots that were not there.
///
/// So the relationship is written into the schema as well, where it belongs
/// and where no connection can opt out of it. Every table below is reached,
/// including the ones two levels down: a `pane_rows` row belongs to a window
/// that belongs to a snapshot, and with foreign keys off nothing else would
/// have taken it.
#[test]
fn a_snapshot_delete_takes_its_rows_with_it_without_foreign_keys() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("state.db");
    {
        let conn = osm::db::open(&path).unwrap();
        conn.execute_batch(
            "INSERT INTO snapshots (id, taken_at, boot_id, reason, state)
               VALUES (1, 100, 'boot-a', 'manual', 'complete'),
                      (2, 200, 'boot-a', 'manual', 'complete');

             INSERT INTO session_rows (row_id, snapshot_id, tmux_session_id, name)
               VALUES (10, 1, '$0', 'doomed'), (11, 2, '$0', 'kept');
             INSERT INTO window_rows (row_id, snapshot_id, tmux_window_id, name, layout)
               VALUES (20, 1, '@0', 'w', 'l'), (21, 2, '@0', 'w', 'l');
             INSERT INTO session_window_links (row_id, session_row_id, window_row_id, idx)
               VALUES (30, 10, 20, 0), (31, 11, 21, 0);
             INSERT INTO pane_rows (row_id, window_row_id, tmux_pane_id, idx, cwd,
                                    restore_policy)
               VALUES (40, 20, '%0', 0, '/tmp', 'shell'),
                      (41, 21, '%0', 0, '/tmp', 'shell');
             INSERT INTO terminal_windows (row_id, snapshot_id, hypr_address, window_class,
                                           terminal_kind, session_row_id, session_name,
                                           workspace_kind, workspace_ref, monitor_connector)
               VALUES (50, 1, '0xa', 'ghostty', 'ghostty', 10, 'doomed', 'id', '1', 'DP-1'),
                      (51, 2, '0xb', 'ghostty', 'ghostty', 11, 'kept', 'id', '1', 'DP-1');
             INSERT INTO restore_attempts (id, snapshot_id, started_at, state)
               VALUES (60, 1, 100, 'failed'), (61, 2, 100, 'failed');
             INSERT INTO restore_objects (row_id, attempt_id, kind, ref, state)
               VALUES (70, 60, 'session', 'doomed', 'done'),
                      (71, 61, 'session', 'kept', 'done');
             INSERT INTO restore_window_map (row_id, attempt_id, captured_window_id,
                                             live_window_id)
               VALUES (80, 60, '@0', '@7'), (81, 61, '@0', '@7');",
        )
        .unwrap();
    }

    // A connection like any other tool's: no pragma, nothing enforced.
    let raw = rusqlite::Connection::open(&path).unwrap();
    raw.pragma_update(None, "foreign_keys", "OFF").unwrap();
    let fk: i64 = raw
        .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
        .unwrap();
    assert_eq!(fk, 0, "this connection must enforce no foreign keys");
    raw.execute("DELETE FROM snapshots WHERE id = 1", [])
        .unwrap();

    for (table, column, id) in [
        ("session_rows", "row_id", 10),
        ("window_rows", "row_id", 20),
        ("session_window_links", "row_id", 30),
        ("pane_rows", "row_id", 40),
        ("terminal_windows", "row_id", 50),
        ("restore_attempts", "id", 60),
        ("restore_objects", "row_id", 70),
        ("restore_window_map", "row_id", 80),
    ] {
        let left: i64 = raw
            .query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE {column} = ?1"),
                [id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            left, 0,
            "{table} still holds a row belonging to the snapshot that was deleted"
        );
    }

    // And the snapshot that was not deleted keeps everything it had.
    for (table, column, id) in [
        ("session_rows", "row_id", 11),
        ("window_rows", "row_id", 21),
        ("session_window_links", "row_id", 31),
        ("pane_rows", "row_id", 41),
        ("terminal_windows", "row_id", 51),
        ("restore_attempts", "id", 61),
        ("restore_objects", "row_id", 71),
        ("restore_window_map", "row_id", 81),
    ] {
        let left: i64 = raw
            .query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE {column} = ?1"),
                [id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(left, 1, "{table} lost a row belonging to a live snapshot");
    }

    let violations: i64 = raw
        .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(violations, 0, "the delete left the database inconsistent");
}
