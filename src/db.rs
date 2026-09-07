use anyhow::{Context, Result};
use rusqlite::Connection;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// Bumped to 2 by the linked-window fix, which replaced the
/// session-owns-window schema with a session↔window link relation, and to 3
/// by verified adoption, which added the `conflicted` restore-object state
/// (a live session that holds the captured name but not the captured
/// topology), and to 4 by the `degraded` state, written for a `pane` whose
/// captured directory no longer exists and for a `window` whose captured
/// layout could not be applied, and to 5 by carry-forward, which added
/// `snapshots.unresolved` — the marker that says a snapshot still holds
/// sessions nothing has recovered — and to 6 by per-session carry debt, which
/// moved that marker down onto `session_rows.unresolved`, added
/// `restore_window_map` (the captured→live window identity a restore
/// establishes) and widened `restore_attempts.state` with `unsecured`, and to
/// 7 by server identity, which added `snapshots.server` and
/// `restore_attempts.destination_server` — the tmux server *incarnation* a
/// topology was read from and a restore's window ids belong to, without which
/// a window map outlived the server that gave it meaning, and to 9 by
/// `window_rows.auto_named` — whether tmux owned a window's name at capture
/// time, without which a name tmux was still deriving was compared as if the
/// user had chosen it (see [`crate::equiv`]).
///
/// # What happens to a database written under another version
///
/// Never a `DROP`. [`open`] is called by *every* subcommand, `status`
/// included, so dropping on mismatch meant a user who upgraded `osm` and
/// merely asked it how it was doing lost the only snapshot of their
/// pre-reboot state — before a restore ever ran, and in both directions,
/// since the test was `!=`.
///
/// | on disk | what [`open`] does |
/// |---------|--------------------|
/// | 1, 2, 3 | preserved beside itself as `state.db.v<N>.bak`; a fresh database is created |
/// | 4       | migrated in place (one `ALTER TABLE`, adding `snapshots.unresolved`) |
/// | 5       | migrated in place (adds `session_rows.unresolved` and `restore_window_map`, rebuilds `restore_attempts` for its wider `CHECK`) |
/// | 6       | migrated in place (two added columns, both nullable: the server identities) |
/// | 7       | migrated in place (adds `agent_resume_debt`) |
/// | 8       | migrated in place (one added column, nullable: `window_rows.auto_named`) |
/// | 9       | migrated in place (adds `terminal_windows.session_name`, backfilled from the link) |
/// | 10      | migrated in place (replaces the never-written `agent_sessions.summary` with `title` and `title_source`, and drops its `alive`) |
/// | 11      | migrated in place (one added column with a default: `snapshots.placement_state`) |
/// | 12      | migrated in place (`snapshots` is rebuilt so its id is `AUTOINCREMENT`) |
/// | 13      | opened in place |
/// | unversioned (osm tables, no `schema_version`) | preserved as `state.db.unversioned.bak` |
/// | newer than this build | preserved as `state.db.v<N>.bak` |
///
/// 11 is migrated by adding one column with a default, and the default is
/// `'known'` rather than the cautious-looking `'unknown'`. Under 11 a capture
/// whose placement could not be read was *refused*, so no row on disk can be
/// an unknown-placement snapshot: every one of them either carries what the
/// compositor reported or was taken with the compositor deliberately not
/// asked. Stamping them `unknown` would hand retention a whole database of
/// rows to hold back and would send every restore of one looking elsewhere
/// for placement it already has — a doubt the build that wrote them never
/// had.
///
/// 10 is migrated by renaming a column nothing ever wrote. `summary` promised
/// an opt-in paraphrase of a conversation's contents, which was never built;
/// what is recorded now is a *title* — one line the agent wrote about itself,
/// or one truncated line of the user's first prompt, with `title_source`
/// saying which. Keeping the old name would tell the next reader that osm
/// stores summaries. `alive` goes with it: whether a conversation is open
/// right now is a fact about this moment, and this table is a registry keyed
/// by conversation with no moment in it, so the column could only ever be
/// stale. Every row keeps everything else it had, and gains no title it did
/// not have.
///
/// 8 is migrated too, and the added column is nullable on purpose: a row
/// written before it existed cannot say whether the user chose the window's
/// name, and NULL is read as "not an identity" — the same direction as an
/// auto-renamed window. Defaulting those rows to "the user named it" would
/// leave every snapshot already on disk exposed to the bug the column exists
/// to fix, with no way for the user to clear it.
///
/// 1, 2 and 3 are deliberately not migrated: v1 keyed windows by
/// session, so its rows cannot be lifted into the link relation without
/// inventing information, and there is no released osm whose users would be
/// carrying such a database. 4, 5 and 6 are migrated, because they can be: 4
/// adds one column with a default, 5 adds two more plus a table rebuild whose
/// rows copy across unchanged, and 6 adds two nullable columns. A row that
/// predates the server identity keeps `NULL`, which is read as "no identity"
/// and never as a match — so an old database loses a linked window's link
/// rather than linking the wrong window. What matters is that the file is still there
/// afterwards. [`preserved`] reports the situation and `osm status --json`
/// prints it, so a preserved database is visible rather than silent.
pub const SCHEMA_VERSION: u32 = 13;

const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS snapshots (
  -- AUTOINCREMENT, and it is the whole of what stops one class of outage.
  --
  -- A plain `INTEGER PRIMARY KEY` is a rowid, and SQLite allocates one as
  -- `max(id) + 1`: delete the highest snapshot and the next capture is handed
  -- its id back. Every table below is keyed by this id and `window_rows` is
  -- `UNIQUE (snapshot_id, tmux_window_id)`, so a single row that outlived its
  -- snapshot turns that reuse into a permanent wedge — the reissued id meets
  -- the stale row, the insert fails, and the *next* capture is handed the
  -- same id again, for ever. That is the maintainer's 83-minute outage: 198
  -- rows claiming snapshots 3717…3725, a highest surviving snapshot of 3716,
  -- and 41 systemd restarts that could not have helped.
  --
  -- The cost is one `sqlite_sequence` row and an id that is never reissued
  -- even when the table is emptied, which is exactly the property wanted:
  -- with it, no stale row anywhere can collide with a new snapshot, whatever
  -- put it there. It is not a substitute for the rows not being there — see
  -- the delete trigger below and `db::repair` — it is what makes their
  -- presence survivable rather than fatal.
  id       INTEGER PRIMARY KEY AUTOINCREMENT,
  taken_at INTEGER NOT NULL,
  boot_id  TEXT    NOT NULL,
  reason   TEXT    NOT NULL,
  state    TEXT    NOT NULL
           CHECK (state IN ('building','complete','restore_in_progress',
                            'restored','failed')),
  -- A cache of "some session row below is still outstanding", kept only so
  -- retention can filter on one column. `session_rows.unresolved` is the
  -- truth; `snapshots::refresh_unresolved` keeps the two in step, and every
  -- writer of the per-session debt must call it, because retention reads this
  -- one and data loss is what happens when it is wrong.
  --
  -- Recency alone used to decide which snapshot a restore rebuilds from, so
  -- an incompletely restored one was silently superseded by the very capture
  -- that recorded the incomplete result — and then pruned.
  unresolved INTEGER NOT NULL DEFAULT 0 CHECK (unresolved IN (0,1)),
  -- The tmux server *incarnation* this topology was read from, or NULL when
  -- it was not read from a server at all (a synthetic topology, or a row
  -- written before this column existed).
  --
  -- Load-bearing for window identity: unprefixed tmux ids in a snapshot only
  -- name windows on the server that produced them, and every server started
  -- on a socket hands out `@0`, `@1`, … from zero again. Comparing boot ids
  -- was not enough — a tmux restart *within* one boot leaves them equal.
  server TEXT,
  -- What this snapshot knows about where its sessions' terminal windows
  -- were. Three answers, and the third is the point:
  --
  --   'known'    the compositor and the tmux server both answered. The
  --              `terminal_windows` rows below are the whole of it, and
  --              *no rows* means there genuinely were no such windows.
  --   'unknown'  one of them could not be read, or they described different
  --              tmux servers. The absence of rows says nothing at all.
  --   'disabled' the compositor was never asked -- `restore.place_windows`
  --              is off, or this capture had no compositor to ask.
  --
  -- Without this column the three collapse into "there are no rows", and a
  -- snapshot recorded during a compositor hiccup is indistinguishable from a
  -- machine that genuinely has no windows. Retention would then prune the
  -- last snapshot that knew where the user's windows belonged in favour of
  -- one that never knew -- the same class of loss the `unresolved` column
  -- above exists to prevent, one level up.
  placement_state TEXT NOT NULL DEFAULT 'known'
                  CHECK (placement_state IN ('known','unknown','disabled'))
);

CREATE TABLE IF NOT EXISTS session_rows (
  row_id           INTEGER PRIMARY KEY,
  snapshot_id      INTEGER NOT NULL REFERENCES snapshots(id) ON DELETE CASCADE,
  tmux_session_id  TEXT    NOT NULL,
  name             TEXT    NOT NULL,
  active_window_id TEXT,
  -- The debt, per session. `snapshots.unresolved` above is the OR of these
  -- and exists only so retention can filter on one column.
  --
  -- A whole-snapshot flag could not express what actually has to be tracked:
  -- a restore delivers some sessions and not others, and the ones it did not
  -- deliver are the only ones a later capture may carry forward. Carrying the
  -- whole snapshot instead resurrected sessions the user had deliberately
  -- closed, and clearing the whole flag as soon as one live session happened
  -- to hold a matching *name* dropped the rest on the floor.
  unresolved INTEGER NOT NULL DEFAULT 0 CHECK (unresolved IN (0,1))
);

-- One row per *window*, not per (session, window): a window linked into
-- several sessions is one window with one set of panes, and tmux reports it
-- once per session it is linked into. Keying windows by session made every
-- capture fail (UNIQUE violation on pane_rows) for as long as any link
-- existed anywhere on the server.
CREATE TABLE IF NOT EXISTS window_rows (
  row_id         INTEGER PRIMARY KEY,
  snapshot_id    INTEGER NOT NULL REFERENCES snapshots(id) ON DELETE CASCADE,
  tmux_window_id TEXT    NOT NULL,
  name           TEXT    NOT NULL,
  layout         TEXT    NOT NULL,
  -- Whether tmux owned `name` when this row was written (`automatic-rename`
  -- on for the window), or NULL when the row predates this column.
  --
  -- Load-bearing for equivalence, not decoration: with the flag on, `name` is
  -- a value tmux derives from the foreground command and rewrites as it
  -- changes, so a capture that lands in the moment a fresh window is still
  -- called `tmux` records a name that never matches again. Comparing it then
  -- pinned the snapshot out of retention forever. See `crate::equiv`.
  auto_named     INTEGER CHECK (auto_named IN (0,1)),
  active_pane_id TEXT,
  zoomed         INTEGER NOT NULL DEFAULT 0 CHECK (zoomed IN (0,1)),
  UNIQUE (snapshot_id, tmux_window_id)
);

-- The many-to-many session↔window relation. Everything that is per-link
-- rather than per-window lives here: the window's index *within that
-- session* and whether it is that session's active window.
CREATE TABLE IF NOT EXISTS session_window_links (
  row_id         INTEGER PRIMARY KEY,
  session_row_id INTEGER NOT NULL REFERENCES session_rows(row_id) ON DELETE CASCADE,
  window_row_id  INTEGER NOT NULL REFERENCES window_rows(row_id) ON DELETE CASCADE,
  idx            INTEGER NOT NULL,
  active         INTEGER NOT NULL DEFAULT 0 CHECK (active IN (0,1)),
  UNIQUE (session_row_id, window_row_id),
  UNIQUE (session_row_id, idx)
);

CREATE TABLE IF NOT EXISTS pane_rows (
  row_id           INTEGER PRIMARY KEY,
  window_row_id    INTEGER NOT NULL REFERENCES window_rows(row_id) ON DELETE CASCADE,
  tmux_pane_id     TEXT    NOT NULL,
  idx              INTEGER NOT NULL,
  cwd              TEXT    NOT NULL,
  title            TEXT,
  foreground_cmd   TEXT,
  dead             INTEGER NOT NULL DEFAULT 0 CHECK (dead IN (0,1)),
  restore_policy   TEXT    NOT NULL
                   CHECK (restore_policy IN ('shell','agent_resume','none')),
  restore_argv     TEXT,
  agent_kind       TEXT,
  agent_session_id TEXT,
  agent_confidence REAL,
  UNIQUE (window_row_id, tmux_pane_id)
);

CREATE TABLE IF NOT EXISTS terminal_windows (
  row_id            INTEGER PRIMARY KEY,
  snapshot_id       INTEGER NOT NULL REFERENCES snapshots(id) ON DELETE CASCADE,
  hypr_address      TEXT    NOT NULL,
  window_class      TEXT    NOT NULL,
  terminal_kind     TEXT    NOT NULL,
  session_row_id    INTEGER REFERENCES session_rows(row_id) ON DELETE SET NULL,
  -- The session this window was attached to, by name.
  --
  -- Kept alongside the row link rather than derived from it: a placement
  -- whose session vanished between the tmux read and the compositor read is
  -- still worth recording, and reading the name through the join loses it
  -- exactly then -- leaving a row that cannot be restored, because there is
  -- no name to attach to.
  session_name      TEXT    NOT NULL DEFAULT '',
  workspace_kind    TEXT    NOT NULL,
  workspace_ref     TEXT    NOT NULL,
  monitor_connector TEXT    NOT NULL,
  monitor_desc      TEXT,
  monitor_scale     REAL,
  monitor_transform INTEGER,
  floating          INTEGER NOT NULL DEFAULT 0 CHECK (floating IN (0,1)),
  rel_x REAL, rel_y REAL, rel_w REAL, rel_h REAL,
  UNIQUE (snapshot_id, hypr_address)
);

-- What osm knows about a conversation, keyed by the conversation and not by
-- any one snapshot: the same conversation runs in a pane, is captured a
-- hundred times, and is still one conversation.
--
-- Written only for conversations a capture actually bound to a pane, which
-- bounds this table by the panes that have ever run an agent rather than by
-- the 2494 conversations on the machine.
CREATE TABLE IF NOT EXISTS agent_sessions (
  row_id       INTEGER PRIMARY KEY,
  kind         TEXT    NOT NULL,
  native_id    TEXT    NOT NULL,
  project_dir  TEXT,
  store_path   TEXT,
  last_active  INTEGER,
  size_bytes   INTEGER,
  -- One line saying what the conversation is about, NULL when osm could
  -- derive none. Never a summary of what was said: see `crate::agent::title`
  -- and the design's privacy note.
  title        TEXT,
  -- Where `title` came from: `agent` (the agent's own name for it) or
  -- `first_prompt` (one truncated line of the user's opening message). NULL
  -- exactly when `title` is. Stored rather than inferred, because a reader
  -- must be able to tell the agent's words from the user's without guessing.
  title_source TEXT,
  UNIQUE (kind, native_id)
);

CREATE TABLE IF NOT EXISTS restore_attempts (
  id          INTEGER PRIMARY KEY,
  snapshot_id INTEGER NOT NULL REFERENCES snapshots(id) ON DELETE CASCADE,
  started_at  INTEGER NOT NULL,
  finished_at INTEGER,
  state       TEXT    NOT NULL
              CHECK (state IN ('running','succeeded','partial','failed',
                               'unsecured')),
  -- The tmux server incarnation this attempt built on, and therefore the only
  -- server on which its `restore_window_map` rows below mean anything. NULL
  -- when it could not be established, which makes those rows unusable rather
  -- than universally usable. The mappings are children of exactly one attempt
  -- (FK, ON DELETE CASCADE), so this is their identity too and is deliberately
  -- not repeated on every row, where the two copies could disagree.
  destination_server TEXT
);

-- Which live window each captured window became, as established by one
-- restore.
--
-- A carried session and a live session can share a window: `alpha` and `beta`
-- are one linked window in the snapshot, the restore delivers `alpha` and not
-- `beta`, and the capture that carries `beta` forward has to link it to the
-- window `alpha` is *already holding* rather than copy a second one. Nothing
-- else on the machine knows that the live `@7` is the captured `@0`; only the
-- restore that made it does, so it writes it down.
CREATE TABLE IF NOT EXISTS restore_window_map (
  row_id             INTEGER PRIMARY KEY,
  attempt_id         INTEGER NOT NULL REFERENCES restore_attempts(id) ON DELETE CASCADE,
  captured_window_id TEXT    NOT NULL,
  live_window_id     TEXT    NOT NULL,
  UNIQUE (attempt_id, captured_window_id)
);

CREATE TABLE IF NOT EXISTS restore_objects (
  row_id     INTEGER PRIMARY KEY,
  attempt_id INTEGER NOT NULL REFERENCES restore_attempts(id) ON DELETE CASCADE,
  kind       TEXT    NOT NULL,
  ref        TEXT    NOT NULL,
  state      TEXT    NOT NULL
             CHECK (state IN ('pending','done','adopted','skipped',
                              'conflicted','degraded','failed')),
  detail     TEXT,
  UNIQUE (attempt_id, kind, ref)
);

-- Which conversations a restore has put a *pane* back for without (yet)
-- putting the conversation back into it.
--
-- The one reason a capture may carry an agent binding forward instead of
-- writing down what it detected. A capture landing between a restore building
-- its panes and the resumes completing sees bare shells everywhere, and
-- recording that verbatim erases the only record of which conversation
-- belonged where. The previous rule — "more than half the conversations are
-- gone, so distrust the whole map" — inferred that cause from a ratio, and
-- inference is what made it fire on a user who simply closed an agent: the
-- correct fresh binding was discarded and the closed conversation carried
-- forward for ever. Debt is therefore recorded per object, by the restore
-- that incurred it, with the boot it belongs to and the moment it was taken
-- on, exactly as Plan 1 concluded for session carry-forward.
--
-- `boot_id` scopes it: a debt is a statement about panes on this boot's
-- server, and nothing owed before a reboot survives one. `recorded_at` bounds
-- it: see `debt::WINDOW_SECS`.
CREATE TABLE IF NOT EXISTS agent_resume_debt (
  row_id       INTEGER PRIMARY KEY,
  boot_id      TEXT    NOT NULL,
  kind         TEXT    NOT NULL,
  native_id    TEXT    NOT NULL,
  -- Where the pane was, in the only identity that survives a restore: the
  -- session's name, the window's index in it, and the pane's index in the
  -- window. Kept so an operator can see what is owed and where, and so two
  -- panes owing the same conversation are two rows rather than one.
  session_name TEXT    NOT NULL,
  window_idx   INTEGER NOT NULL,
  pane_idx     INTEGER NOT NULL,
  recorded_at  INTEGER NOT NULL,
  UNIQUE (boot_id, kind, native_id, session_name, window_idx, pane_idx)
);

CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
"#;

/// The `ON DELETE` behaviour declared above, written a second time as
/// triggers — because the declarations alone are not enforced by this
/// database.
///
/// # Why this is not belt and braces
///
/// `PRAGMA foreign_keys` is **per connection** and stock SQLite defaults it
/// off: the `sqlite3` shell, anything linked against the system library, a
/// backup tool, and — inside this file — [`migrate`], which switches it off
/// for a table rebuild. Every one of those can delete a snapshot and leave
/// every row underneath it, and nothing but `PRAGMA foreign_key_check` will
/// ever say so. That is how the maintainer's database came to hold 198 rows
/// belonging to nine snapshots that were not there: 72 `window_rows`, 72
/// `session_rows`, 54 `terminal_windows`. Combined with a reused id it took
/// his capture down for 83 minutes, and it would never have recovered.
///
/// A trigger is not optional in the same way. It lives in the schema, it runs
/// for whoever is connected, and `PRAGMA recursive_triggers` does not govern
/// it: a `DELETE` inside a trigger body still fires the triggers of the table
/// it deletes from, which is what carries a snapshot's removal all the way
/// down to `pane_rows`. The foreign keys are kept as well — they are the
/// statement of intent, they still reject a child row written against a
/// snapshot that does not exist, and where they are enforced they simply do
/// the work first and leave the trigger nothing to find.
///
/// Deliberately **not** a numbered migration step. `CREATE TRIGGER IF NOT
/// EXISTS` needs the tables it names to exist, and a migration runs before
/// [`create_schema`] has made them; more to the point, a schema version is
/// what decides whether an older build moves a user's database aside, and
/// nothing here changes a single row or column. So these are created by every
/// [`create_schema`] instead, which every [`open`] calls whatever the version
/// on disk — which also means a database that loses them (a rebuild of
/// `snapshots` drops its triggers with it) has them back before anything else
/// touches the file.
const CASCADE_TRIGGERS_SQL: &str = r#"
-- `terminal_windows` first: deleting the session rows below would otherwise
-- spend a pass setting `session_row_id` to NULL on rows that are about to go.
CREATE TRIGGER IF NOT EXISTS snapshots_cascade_delete
AFTER DELETE ON snapshots
BEGIN
  DELETE FROM terminal_windows WHERE snapshot_id = OLD.id;
  DELETE FROM window_rows      WHERE snapshot_id = OLD.id;
  DELETE FROM session_rows     WHERE snapshot_id = OLD.id;
  DELETE FROM restore_attempts WHERE snapshot_id = OLD.id;
END;

-- The `ON DELETE SET NULL` here is deliberate and is mirrored, not upgraded
-- to a delete: a placement whose session row is gone is still a record of
-- where a window was.
CREATE TRIGGER IF NOT EXISTS session_rows_cascade_delete
AFTER DELETE ON session_rows
BEGIN
  DELETE FROM session_window_links WHERE session_row_id = OLD.row_id;
  UPDATE terminal_windows SET session_row_id = NULL
   WHERE session_row_id = OLD.row_id;
END;

CREATE TRIGGER IF NOT EXISTS window_rows_cascade_delete
AFTER DELETE ON window_rows
BEGIN
  DELETE FROM pane_rows            WHERE window_row_id = OLD.row_id;
  DELETE FROM session_window_links WHERE window_row_id  = OLD.row_id;
END;

CREATE TRIGGER IF NOT EXISTS restore_attempts_cascade_delete
AFTER DELETE ON restore_attempts
BEGIN
  DELETE FROM restore_objects    WHERE attempt_id = OLD.id;
  DELETE FROM restore_window_map WHERE attempt_id = OLD.id;
END;
"#;

/// What an existing database file says about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
enum OnDisk {
    /// No osm tables at all: a brand-new file, or none.
    Empty,
    /// A recorded `schema_version`.
    Version(u32),
    /// osm tables, but no version to compare against. Treated exactly like a
    /// version mismatch: something wrote this file and it was not this build.
    Unversioned,
}

fn table_exists(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
        [name],
        |_| Ok(()),
    )
    .is_ok()
}

fn on_disk_schema(conn: &Connection) -> OnDisk {
    let version = conn
        .query_row(
            "SELECT value FROM meta WHERE key='schema_version'",
            [],
            |r| r.get::<_, String>(0),
        )
        .ok()
        .and_then(|v| v.parse().ok());
    match version {
        Some(v) => OnDisk::Version(v),
        // A file with osm's own tables but no version is not a fresh
        // database: running `CREATE TABLE IF NOT EXISTS` over it would leave
        // whatever shape it has in place and then fail every query at
        // runtime, which is the outcome the old drop-on-mismatch code was
        // trying to avoid.
        None if table_exists(conn, "snapshots") => OnDisk::Unversioned,
        None => OnDisk::Empty,
    }
}

/// A database this build could not open, kept beside the new one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preserved {
    /// Where the incompatible database now lives.
    pub path: String,
    /// The `schema_version` it recorded, or `None` if it had none.
    pub schema_version: Option<u32>,
    /// When it was moved aside, in epoch seconds.
    pub preserved_at: Option<i64>,
}

/// The incompatible database [`open`] moved aside, if it ever did.
///
/// Reported by `osm status --json`: preserving a database silently would be
/// only marginally better than deleting one silently, since the user's
/// snapshots are no longer where anything looks for them.
pub fn preserved(conn: &Connection) -> Option<Preserved> {
    let path: String = conn
        .query_row(
            "SELECT value FROM meta WHERE key='preserved_db_path'",
            [],
            |r| r.get(0),
        )
        .ok()?;
    let read = |key: &str| -> Option<String> {
        conn.query_row("SELECT value FROM meta WHERE key=?1", [key], |r| r.get(0))
            .ok()
    };
    Some(Preserved {
        path,
        schema_version: read("preserved_db_version").and_then(|v| v.parse().ok()),
        preserved_at: read("preserved_db_at").and_then(|v| v.parse().ok()),
    })
}

/// What a preserved database turned out to hold.
///
/// Three answers rather than a claim. `osm status` used to tell its reader
/// that the snapshots in a preserved file "are intact" without ever opening
/// it, which on a machine whose backup held nothing frightened its owner
/// about data he had never lost. A file with snapshots in it and a file with
/// none are different facts; a file nothing could read is a third, and must
/// not be reported as either of the first two.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreservedContents {
    /// It opened, and holds this many snapshots. Zero is an answer: there is
    /// nothing in the file to lose.
    Snapshots(i64),
    /// Nothing is at the path any more — the ordinary end of the story, since
    /// a preserved file holding nothing is the user's to delete.
    Gone,
    /// It is there, and what it holds could not be established.
    Unreadable(String),
}

/// How long the count below may wait on a lock before it is called unknown.
///
/// Nothing writes to a preserved database — it is a backup no code opens for
/// writing — so this is only reached by a file some other process happens to
/// be holding. `osm status` is polled by the panel every few seconds and must
/// never be the thing that blocks it.
const PRESERVED_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);

/// Count the snapshots in the preserved database at `path`, read-only.
///
/// **A count, and only a count.** [`PreservedContents::Snapshots`] means this
/// many rows are in that one table, and nothing more was looked at: not
/// whether anything is behind them, not whether the newest is a `building` row
/// describing a capture that never finished, not whether the file is
/// internally consistent. Any caller that turns this number into a claim about
/// the state of the user's data is asserting something nobody checked — see
/// `preserved_notice` in `src/main.rs`, which used to call a positive count
/// *intact*.
///
/// Cheap and bounded on purpose: one open, one `COUNT(*)` over an index-free
/// table of at most a retention window's rows, on every `osm status`. The
/// connection is opened **read only**, without `SQLITE_OPEN_CREATE`: this is
/// the user's file, osm's business with it ended when it was moved aside, and
/// a missing one must read as missing rather than be conjured back as an
/// empty database.
pub fn preserved_contents(path: &Path) -> PreservedContents {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return PreservedContents::Gone,
        // Anything else — a permission error on the directory, say — is a
        // question that was not answered, not a file that is not there.
        Err(e) => return PreservedContents::Unreadable(e.to_string()),
        Ok(_) => {}
    }
    match count_preserved_snapshots(path) {
        Ok(n) => PreservedContents::Snapshots(n),
        Err(e) => PreservedContents::Unreadable(e.to_string()),
    }
}

/// Count the rows, touching the user's directory as little as the file
/// allows.
///
/// A plain read-only connection to a WAL database makes SQLite build the
/// shared-memory index it reads the WAL through, which means **creating**
/// `<backup>-shm` and `<backup>-wal` beside the user's backup — on a status
/// the panel runs every five seconds. `immutable=1` creates nothing, locks
/// nothing and reads only the file itself, but it also ignores a `-wal`, and
/// a preserved database whose checkpoint failed carries its most recent
/// transactions in exactly that file. Reporting those snapshots as absent is
/// the untruth this whole function exists to stop.
///
/// So: `immutable=1` when there is no WAL content to miss, which is the
/// ordinary case (preservation checkpoints the database before moving it),
/// and an ordinary read-only open when there is — where the sidecars are the
/// price of counting the file correctly.
fn count_preserved_snapshots(path: &Path) -> rusqlite::Result<i64> {
    let hot_wal = fs::metadata(sidecar(path, "-wal")).is_ok_and(|m| m.len() > 0);
    let conn = match immutable_uri(path).filter(|_| !hot_wal) {
        Some(uri) => Connection::open_with_flags(
            uri,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
                | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        ),
        None => Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ),
    }?;
    conn.busy_timeout(PRESERVED_READ_TIMEOUT)?;
    conn.query_row("SELECT COUNT(*) FROM snapshots", [], |r| r.get(0))
}

/// `path` as a SQLite `file:` URI opening it immutably, or `None` for a path
/// SQLite's URI parser cannot be handed safely.
///
/// Everything outside an unreserved set is percent-encoded, because `?`, `#`
/// and `%` are all legal in a filename and all mean something else in a URI:
/// a state directory called `what?` would otherwise open a database called
/// `what` with a query string. A path that is not UTF-8 has no URI form at
/// all, so it takes the ordinary open.
fn immutable_uri(path: &Path) -> Option<String> {
    let raw = path.to_str()?;
    let mut uri = String::from("file:");
    for b in raw.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'.' | b'_' | b'~' => {
                uri.push(b as char)
            }
            _ => uri.push_str(&format!("%{b:02X}")),
        }
    }
    uri.push_str("?immutable=1");
    Some(uri)
}

/// The lock **every** [`open`] is taken under.
///
/// # Why a lock, and why around the whole of `open`
///
/// `open` runs in every `osm` process and tmux fires hooks in parallel, so a
/// dozen processes routinely open one database at the same moment. The
/// classify → checkpoint → rename → create sequence used to run with no
/// mutual exclusion at all, and with thirty-two concurrent openers on one v3
/// database the result was ten preservation operations: the real snapshot
/// ended up in `state.db.v3.bak` while `.bak.1`…`.bak.9` were *empty*
/// databases, caught by a later opener mid-creation and moved aside as if
/// they were the user's data. `status --json` then reported the last of those
/// empty files as the preserved database "with intact snapshots".
///
/// A tighter interleaving was worse still: two processes could pick the same
/// free backup name before either renamed, and Unix `rename` replaces its
/// destination, so the second one silently destroyed the first one's backup.
///
/// Locking only the preservation branch is *not* enough, and that is worth
/// spelling out because it looks like it should be: SQLite manages a WAL
/// database's `-wal` and `-shm` **by path**, so a process merely opening the
/// database while another moves it aside creates sidecars for a file that is
/// no longer there and then fails with `SQLITE_IOERR_DELETE` ("File being
/// deleted does not exist"). The only sound boundary is the whole of `open`:
/// while one process is deciding what this file is, nobody else may have it
/// open at all.
///
/// The cost is one `flock` per open, uncontended in the ordinary case, and
/// [`crate::lock::SingleInstance::acquire_blocking`] backs off from 1 ms so a
/// burst of hooks does not pay a fixed poll interval each.
fn migration_lock_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".migrate.lock");
    path.with_file_name(name)
}

/// How long a process waits for whoever is migrating or preserving the
/// database. Generous: the work is one checkpoint, two renames and one
/// transaction, so anything slower than this is wedged rather than busy, and
/// giving up loudly beats proceeding without the lock.
const MIGRATION_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

fn migration_guard(path: &Path) -> Result<crate::lock::SingleInstance> {
    let lock = migration_lock_path(path);
    crate::lock::SingleInstance::acquire_blocking(&lock, MIGRATION_LOCK_WAIT)?.ok_or_else(|| {
        anyhow::anyhow!(
            "another osm process has held the schema lock at {} for more than {}s; \
             refusing to classify or preserve {} without it",
            lock.display(),
            MIGRATION_LOCK_WAIT.as_secs(),
            path.display()
        )
    })
}

/// The suffixes SQLite manages beside a database file, in the order they must
/// travel with it.
///
/// Both are part of the database's identity on disk, not decoration: a `-wal`
/// holds committed transactions that are not in the main file, and SQLite finds
/// either of them **by path**. A preserved database that lost its `-wal` is
/// missing its most recent transactions; a `-wal` left behind is inherited by
/// whatever takes the database's name next.
const SIDECARS: [&str; 2] = ["-wal", "-shm"];

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

fn occupied(to: &Path, from: &Path) -> anyhow::Error {
    anyhow::anyhow!(
        "refusing to overwrite the existing backup {} with {}",
        to.display(),
        from.display()
    )
}

/// A base name next to `path` where **neither the database nor either
/// sidecar** is already occupied.
///
/// Checking only the main name is not enough, and the difference is a split
/// database: with `state.db.v3.bak` free but `state.db.v3.bak-wal` taken, the
/// old code chose that base, moved the database successfully and then failed on
/// the `-wal` — leaving the database under one basename and its committed
/// journal under another, with a fresh `state.db` free to be created beside the
/// orphan.
///
/// Never overwrites: a second incompatible database is a second thing worth
/// keeping, and clobbering the first backup with it would reintroduce the data
/// loss one release later. Called only under [`migration_guard`], so the name
/// it hands back is still free when the caller uses it — and
/// [`preserve_aside`] refuses anyway if it is not.
fn free_backup_path(path: &Path, label: &str) -> PathBuf {
    let taken = |candidate: &Path| {
        candidate.exists() || SIDECARS.iter().any(|s| sidecar(candidate, s).exists())
    };
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{label}.bak"));
    let base = path.with_file_name(name);
    if !taken(&base) {
        return base;
    }
    for n in 1..1000 {
        let mut name = base.file_name().unwrap_or_default().to_os_string();
        name.push(format!(".{n}"));
        let candidate = base.with_file_name(name);
        if !taken(&candidate) {
            return candidate;
        }
    }
    base
}

/// One file that has to travel: where it is, where it is going, and which
/// sidecar it is (`None` for the database itself).
type Move = (PathBuf, PathBuf, Option<&'static str>);

/// The exit status a process takes when [`CrashAfter`] fires.
///
/// Only ever reachable with `OSM_PRESERVE_CRASH_AFTER` set; see there.
pub const PRESERVE_CRASH_EXIT: i32 = 97;

/// A deliberate crash, `n` file operations into a preservation.
///
/// Inert unless `OSM_PRESERVE_CRASH_AFTER` names a positive number, which
/// nothing but `tests/db_preserve_crash.rs` does: preservation is the one
/// sequence in this engine whose interruption is unrecoverable by
/// construction unless something durable records it, and the only honest way
/// to prove the record works is to interrupt a real one, at each individual
/// rename and unlink in turn, in a real process that really dies.
///
/// It kills the process outright — no unwinding, no `Drop`, nothing flushed —
/// which is exactly what a power cut does and rather more than a `SIGKILL`
/// leaves to chance.
struct CrashAfter(Option<u32>);

impl CrashAfter {
    fn from_env() -> Self {
        CrashAfter(
            std::env::var("OSM_PRESERVE_CRASH_AFTER")
                .ok()
                .and_then(|v| v.parse::<u32>().ok())
                .filter(|n| *n > 0),
        )
    }

    fn tick(&mut self) {
        if let Some(left) = self.0.as_mut() {
            *left -= 1;
            if *left == 0 {
                std::process::exit(PRESERVE_CRASH_EXIT);
            }
        }
    }
}

/// What a preservation intends to do, written down before it does any of it.
///
/// # Why this file exists
///
/// Moving a database and its sidecars aside is several file operations, and
/// no filesystem makes several of them one. Whatever happens in between —
/// a failure this process sees, or a power cut it does not — leaves the set
/// split across two basenames, and the previous [`open`] could not tell that
/// from a fresh state directory: `state.db` absent meant "create one", so the
/// engine started with an empty database while every snapshot the user had
/// sat in a backup nothing had recorded yet. The record was written *after*
/// the moves, into the new database, which is the one moment it cannot help.
///
/// So the intent is written first, `fsync`ed, and reconciled by every `open`
/// before anything else looks at the directory — see
/// [`reconcile_preservation`]. Until it is gone, no fresh database is created
/// and no classification is attempted.
#[derive(serde::Serialize, serde::Deserialize)]
struct PreserveManifest {
    /// The database being moved aside.
    source: String,
    /// The basename it is moving to.
    backup: String,
    /// The label the backup name was built from, for a human reading this.
    label: String,
    /// The `schema_version` the source recorded, if it had one.
    version: Option<u32>,
}

fn manifest_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".preserving.json");
    path.with_file_name(name)
}

/// `fsync` a directory, so a rename into it survives a power cut.
fn sync_dir(dir: &Path) -> Result<()> {
    fs::File::open(dir)
        .with_context(|| format!("open {} to sync it", dir.display()))?
        .sync_all()
        .with_context(|| format!("sync {}", dir.display()))
}

fn parent_of(path: &Path) -> &Path {
    path.parent().unwrap_or(Path::new("."))
}

/// Write the manifest so that it is either entirely there or not there at
/// all: full file, `fsync`, atomic rename, `fsync` of the directory.
fn write_manifest(path: &Path, manifest: &PreserveManifest) -> Result<()> {
    use std::io::Write;
    let final_path = manifest_path(path);
    let mut staging = final_path.clone().into_os_string();
    staging.push(".new");
    let staging = PathBuf::from(staging);
    let body = serde_json::to_vec(manifest)?;
    {
        let mut f =
            fs::File::create(&staging).with_context(|| format!("create {}", staging.display()))?;
        f.write_all(&body)?;
        f.sync_all()?;
    }
    fs::rename(&staging, &final_path).with_context(|| {
        format!(
            "put the preservation manifest in place at {}",
            final_path.display()
        )
    })?;
    sync_dir(parent_of(&final_path))
}

fn same_file(a: &Path, b: &Path) -> Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let (a, b) = (fs::metadata(a)?, fs::metadata(b)?);
    Ok(a.dev() == b.dev() && a.ino() == b.ino())
}

/// Get one file to where the reconciliation has decided it belongs.
///
/// Idempotent, which is what lets the reconciliation be re-run from any crash
/// point: a file already at its destination is either the same file reached
/// under two names — the hard-link phase's intermediate state, so the source
/// name is simply dropped — or a different file entirely, which is never
/// overwritten.
fn settle(from: &Path, to: &Path) -> Result<()> {
    if !from.exists() {
        return Ok(());
    }
    if to.exists() {
        if same_file(from, to)? {
            return fs::remove_file(from)
                .with_context(|| format!("drop the spare name {}", from.display()));
        }
        return Err(occupied(to, from));
    }
    fs::rename(from, to).with_context(|| format!("move {} to {}", from.display(), to.display()))
}

/// Finish, or undo, the preservation a manifest describes — and leave exactly
/// one complete basename behind.
///
/// Runs at the top of every [`open`], before a connection is opened and
/// therefore before a fresh database can be created. Returns the preserved
/// database when the preservation is now complete, so `open` can record it in
/// the fresh database it is about to create; `None` when the database never
/// left its own name and there is nothing to record.
///
/// The direction is decided by one fact: **where the database file is**.
/// `rename` is atomic, so it is under exactly one name; the hard-link phase
/// can have it under both, and then the two are one file and the source name
/// is spare. If it is at the backup, the move is finished — the sidecars
/// follow it, because a `-wal` left behind holds committed transactions the
/// database file does not and the fresh database taking that name would
/// inherit it. If it is not, everything that did move comes back.
///
/// Neither name holding a database is not something this can repair, and it
/// refuses rather than starting empty: the manifest stays, every `open` says
/// the same thing, and the operator has both names to look at.
fn reconcile_preservation(path: &Path) -> Result<Option<(String, Option<u32>)>> {
    let manifest = manifest_path(path);
    let body = match fs::read(&manifest) {
        Ok(body) => body,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(anyhow::anyhow!(
                "read the preservation manifest {}: {e}",
                manifest.display()
            ))
        }
    };
    let intent: PreserveManifest = serde_json::from_slice(&body).with_context(|| {
        format!(
            "parse the preservation manifest {}; it records where {} was being \
             moved to and must not be guessed at",
            manifest.display(),
            path.display()
        )
    })?;
    if Path::new(&intent.source) != path {
        anyhow::bail!(
            "the preservation manifest {} describes {}, not {}",
            manifest.display(),
            intent.source,
            path.display()
        );
    }
    let backup = PathBuf::from(&intent.backup);

    let preserved = if backup.exists() {
        for suffix in SIDECARS {
            settle(&sidecar(path, suffix), &sidecar(&backup, suffix))?;
        }
        settle(path, &backup)?;
        fs::set_permissions(&backup, fs::Permissions::from_mode(0o600)).with_context(|| {
            format!("restrict permissions on the preserved {}", backup.display())
        })?;
        Some((backup.display().to_string(), intent.version))
    } else {
        for suffix in SIDECARS {
            settle(&sidecar(&backup, suffix), &sidecar(path, suffix))?;
        }
        if !path.exists() {
            anyhow::bail!(
                "an interrupted preservation left no database at {} and none at {}; \
                 refusing to start a fresh one over the top of whatever happened here",
                path.display(),
                backup.display()
            );
        }
        None
    };

    fs::remove_file(&manifest)
        .with_context(|| format!("clear the preservation manifest {}", manifest.display()))?;
    sync_dir(parent_of(&manifest))?;
    Ok(preserved)
}

/// `rename`-based preservation, for a filesystem with no hard links.
///
/// Weaker than the link-then-unlink pair in [`preserve_aside`] — a crash
/// between two renames leaves the set split — but the manifest written before
/// any of this says so, and the next [`open`] finishes the job.
/// `rename(2)` replaces its destination silently, which is precisely how one
/// racing preservation destroyed another's backup, so the existence check is
/// not optional; the caller holds the migration lock, so nothing else can
/// create a destination in between.
fn rename_all(moves: &[Move], first_error: std::io::Error, crash: &mut CrashAfter) -> Result<()> {
    for (from, to, _) in moves {
        if to.exists() {
            return Err(occupied(to, from));
        }
    }
    for (from, to, _) in moves {
        fs::rename(from, to).map_err(|e| {
            anyhow::anyhow!(
                "preserve {} as {} (this filesystem has no hard links: \
                 {first_error}): {e}",
                from.display(),
                to.display()
            )
        })?;
        crash.tick();
    }
    Ok(())
}

/// Carry the original failure, and say so when the rollback failed too.
///
/// The rollback's result used to be discarded with `let _ =`, which is how a
/// half-undone preservation became a silent one.
fn with_undo(first: anyhow::Error, undo: Result<()>) -> anyhow::Error {
    match undo {
        Ok(()) => first,
        Err(e) => first.context(format!("and undoing it failed too: {e:#}")),
    }
}

/// Pretend this filesystem has no hard links, when
/// `OSM_PRESERVE_NO_HARDLINK` says so.
///
/// [`move_all`] has two modes and which one runs is decided by the
/// filesystem, not by osm: everywhere osm is actually used — ext4, btrfs, xfs,
/// tmpfs — `hard_link` succeeds, so [`rename_all`] and its weaker crash
/// profile are only ever reached on the filesystems that have neither
/// (an exFAT or FAT32 stick, some FUSE mounts). That is precisely the mode
/// whose interruption is least recoverable, and it cannot be exercised by
/// waiting for the right filesystem to turn up in CI.
///
/// So it is selectable, by the same kind of switch and with the same
/// visibility as [`CrashAfter`]: inert unless the variable is set, and set by
/// nothing but `tests/db_preserve_crash.rs`. The error handed to `rename_all`
/// is the one a real filesystem gives — the caller has to be reached the same
/// way, message and all.
fn hard_links_refused() -> Option<std::io::Error> {
    std::env::var_os("OSM_PRESERVE_NO_HARDLINK")
        .map(|_| std::io::Error::from(std::io::ErrorKind::Unsupported))
}

/// Link every destination, then unlink every source; or, on a filesystem
/// without hard links, rename them one at a time.
fn move_all(moves: &[Move], backup: &Path) -> Result<()> {
    let mut crash = CrashAfter::from_env();
    if let Some(e) = hard_links_refused() {
        return rename_all(moves, e, &mut crash);
    }
    let mut linked: Vec<PathBuf> = Vec::new();
    let unwind = |linked: &[PathBuf]| -> Result<()> {
        for done in linked {
            fs::remove_file(done)
                .with_context(|| format!("remove the spare link {}", done.display()))?;
        }
        Ok(())
    };
    let mut renamed = false;
    for (from, to, _) in moves {
        match fs::hard_link(from, to) {
            Ok(()) => {
                linked.push(to.clone());
                crash.tick();
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(with_undo(occupied(to, from), unwind(&linked)));
            }
            // Nothing has been linked yet, so this is the filesystem
            // answering "no hard links here" rather than one file failing:
            // redo the whole set with renames instead.
            Err(e) if linked.is_empty() => {
                rename_all(moves, e, &mut crash)?;
                renamed = true;
                break;
            }
            Err(e) => {
                let why = anyhow::anyhow!(
                    "preserve {} beside {}; it holds data the preserved database needs: {e}",
                    from.display(),
                    backup.display()
                );
                return Err(with_undo(why, unwind(&linked)));
            }
        }
    }

    if !renamed {
        // Every destination exists now, so removing a source cannot lose
        // anything: the file is already reachable under its new name.
        for (from, _, _) in moves {
            fs::remove_file(from)
                .with_context(|| format!("unlink {} after preserving it", from.display()))?;
            crash.tick();
        }
    }
    Ok(())
}

/// Move `path` **and its WAL sidecars** aside, together or not at all.
///
/// The connection must already be closed: renaming a database out from under
/// an open handle leaves the sidecars pointing at a file that is no longer
/// where SQLite thinks it is. The caller must hold [`migration_guard`].
///
/// Returns the backup path and which sidecars travelled with it.
///
/// # Why the intent is written down first
///
/// A sidecar that exists and cannot be moved is a hard error — a `-wal` left
/// behind holds committed transactions that are not in the database file, and
/// the fresh database about to take this name would inherit it. But *no*
/// ordering of these operations is atomic, and the failure that matters most
/// is the one this process never gets to observe. So a durable
/// [`PreserveManifest`] goes down before the first file moves, and
/// [`reconcile_preservation`] — this function's own error path, and every
/// later [`open`] — is the single authority on what the directory ends up
/// holding. It leaves exactly one complete basename, and until it has, no
/// fresh database is created.
fn preserve_aside(
    path: &Path,
    label: &str,
    version: Option<u32>,
) -> Result<(PathBuf, Vec<&'static str>)> {
    let backup = free_backup_path(path, label);
    // The database first: it is the file the caller is told about, and the
    // one whose failure must be reported.
    let mut moves: Vec<Move> = vec![(path.to_path_buf(), backup.clone(), None)];
    for suffix in SIDECARS {
        let from = sidecar(path, suffix);
        // Ordinarily gone already (the connection is closed and
        // checkpointed), but a crashed writer can leave them; they belong
        // with the database they describe, not with the fresh one about to
        // take its name.
        if from.exists() {
            moves.push((from, sidecar(&backup, suffix), Some(suffix)));
        }
    }

    write_manifest(
        path,
        &PreserveManifest {
            source: path.display().to_string(),
            backup: backup.display().to_string(),
            label: label.to_string(),
            version,
        },
    )?;

    let moved = move_all(&moves, &backup);
    let settled = reconcile_preservation(path)?;
    match (moved, settled) {
        // Preserved — whether every operation succeeded or the reconciliation
        // finished what was left.
        (_, Some(_)) => Ok((backup.clone(), present_sidecars(&backup))),
        (Err(e), None) => Err(e),
        (Ok(()), None) => Err(anyhow::anyhow!(
            "preserved {} as {} and then could not find it there",
            path.display(),
            backup.display()
        )),
    }
}

/// Which sidecars are sitting beside a preserved database.
fn present_sidecars(backup: &Path) -> Vec<&'static str> {
    SIDECARS
        .into_iter()
        .filter(|s| sidecar(backup, s).exists())
        .collect()
}

/// How long a writer waits for another writer before giving up.
///
/// Generous: an osm write is a handful of small inserts, so anything that
/// takes longer than this is wedged rather than busy.
const BUSY_TIMEOUT_MS: i64 = 5_000;

/// How many times to retry the WAL switch, and how long to pause between
/// tries. Together roughly the same budget as [`BUSY_TIMEOUT_MS`].
const WAL_SWITCH_ATTEMPTS: u32 = 250;
const WAL_SWITCH_DELAY: std::time::Duration = std::time::Duration::from_millis(20);

/// Put the database in WAL mode, waiting out a competing opener.
///
/// `PRAGMA journal_mode = WAL` needs an exclusive lock and — unlike ordinary
/// statements — SQLite does **not** run the busy handler for it, so the
/// `busy_timeout` above does not cover this one case. Two `osm` processes
/// opening the same fresh database at once (routine: tmux fires hooks in
/// parallel) therefore had one of them fail outright with "database is
/// locked". Retried here instead, and skipped entirely once the mode is
/// already WAL, which it is for every open after the first.
fn ensure_wal(conn: &Connection) -> Result<()> {
    let mut last: Option<rusqlite::Error> = None;
    for _ in 0..WAL_SWITCH_ATTEMPTS {
        let mode: String = conn.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
        if mode.eq_ignore_ascii_case("wal") {
            return Ok(());
        }
        match conn.pragma_update(None, "journal_mode", "WAL") {
            Ok(()) => return Ok(()),
            Err(e) => {
                last = Some(e);
                std::thread::sleep(WAL_SWITCH_DELAY);
            }
        }
    }
    Err(last.expect("at least one attempt")).context("switch the journal to WAL")
}

fn open_connection(path: &Path) -> Result<Connection> {
    let conn =
        Connection::open(path).with_context(|| format!("open database {}", path.display()))?;
    // First, before anything that can contend. Every tmux hook is its own
    // `osm` process and they fire in parallel, so two writers meeting is
    // routine, not exceptional; without a busy timeout SQLite returns
    // SQLITE_BUSY the instant it sees a competing writer and the loser dies
    // with "database is locked" — a failure the hooks would swallow, since
    // they discard output. Switching the journal to WAL needs an exclusive
    // lock of its own, so even that has to be able to wait.
    conn.pragma_update(None, "busy_timeout", BUSY_TIMEOUT_MS)?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    ensure_wal(&conn)?;
    Ok(conn)
}

fn set_meta(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![key, value],
    )?;
    Ok(())
}

/// Bring a database written under an earlier schema up to this one, in place.
///
/// Returns `false` when there is no path from `from` to [`SCHEMA_VERSION`],
/// leaving the database untouched for [`open`] to preserve instead.
///
/// One `IMMEDIATE` transaction, and the recorded version is re-read inside it:
/// two `osm` processes may reach this at the same moment, and the loser must
/// see the migration the winner already committed rather than apply it twice.
fn migrate(conn: &mut Connection, from: u32) -> Result<bool> {
    if from > SCHEMA_VERSION {
        return Ok(false);
    }
    // The 5 → 6 step rebuilds `restore_attempts` to widen its `CHECK`, which
    // means dropping a table another table has a foreign key to. Neither
    // pragma can be changed inside a transaction, so both are set here and put
    // back below — including on the error path, so a failed migration does not
    // leave the connection with foreign keys silently disabled.
    //
    // `legacy_alter_table` is ON for the rename: with it off, SQLite tries to
    // rewrite every *other* table's references while `restore_attempts` does
    // not momentarily exist, and fails. With it on, `restore_objects` keeps
    // naming `restore_attempts` throughout, which is exactly right.
    conn.pragma_update(None, "foreign_keys", "OFF")?;
    conn.pragma_update(None, "legacy_alter_table", "ON")?;
    let migrated = migrate_steps(conn, from);
    let _ = conn.pragma_update(None, "legacy_alter_table", "OFF");
    conn.pragma_update(None, "foreign_keys", "ON")?;
    let migrated = migrated?;
    if migrated {
        // The rebuild copied rows between tables with the enforcement off, so
        // this is the only thing that can say it copied them correctly.
        let violations: i64 =
            conn.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
                r.get(0)
            })?;
        if violations > 0 {
            anyhow::bail!(
                "the schema migration left {violations} dangling foreign-key row(s); \
                 refusing to hand back a database that is no longer self-consistent"
            );
        }
    }
    Ok(migrated)
}

fn migrate_steps(conn: &mut Connection, _from: u32) -> Result<bool> {
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let mut version = match on_disk_schema(&tx) {
        OnDisk::Version(v) => v,
        _ => return Ok(false),
    };
    while version < SCHEMA_VERSION {
        match version {
            // Carry-forward's marker. One added column with a default, so
            // every existing row is `unresolved = 0` — which is the truth:
            // nothing had been marked before this build existed.
            4 => {
                tx.execute_batch(
                    "ALTER TABLE snapshots ADD COLUMN unresolved INTEGER NOT NULL DEFAULT 0
                     CHECK (unresolved IN (0,1))",
                )?;
                version = 5;
            }
            // Per-session carry debt.
            //
            // The existing whole-snapshot flag is pushed down onto every
            // session row of the snapshots that carried it, which is the
            // conservative reading of what a v5 database was recording: it
            // said "something in here is still outstanding" without saying
            // what, so every session in it is treated as outstanding. The
            // alternative — defaulting them all to resolved — would silently
            // discharge debt that was never paid.
            5 => {
                tx.execute_batch(
                    "ALTER TABLE session_rows ADD COLUMN unresolved INTEGER NOT NULL DEFAULT 0
                     CHECK (unresolved IN (0,1));

                     UPDATE session_rows SET unresolved = 1
                     WHERE snapshot_id IN (SELECT id FROM snapshots WHERE unresolved = 1);

                     CREATE TABLE IF NOT EXISTS restore_window_map (
                       row_id             INTEGER PRIMARY KEY,
                       attempt_id         INTEGER NOT NULL
                                          REFERENCES restore_attempts(id) ON DELETE CASCADE,
                       captured_window_id TEXT    NOT NULL,
                       live_window_id     TEXT    NOT NULL,
                       UNIQUE (attempt_id, captured_window_id)
                     );

                     CREATE TABLE restore_attempts_v6 (
                       id          INTEGER PRIMARY KEY,
                       snapshot_id INTEGER NOT NULL REFERENCES snapshots(id) ON DELETE CASCADE,
                       started_at  INTEGER NOT NULL,
                       finished_at INTEGER,
                       state       TEXT    NOT NULL
                                   CHECK (state IN ('running','succeeded','partial','failed',
                                                    'unsecured'))
                     );
                     INSERT INTO restore_attempts_v6 (id, snapshot_id, started_at,
                                                      finished_at, state)
                       SELECT id, snapshot_id, started_at, finished_at, state
                       FROM restore_attempts;
                     DROP TABLE restore_attempts;
                     ALTER TABLE restore_attempts_v6 RENAME TO restore_attempts;",
                )?;
                version = 6;
            }
            // Server identity. Two nullable columns, so every existing row
            // reads as "no identity": a v6 window map stops being believed
            // rather than being believed against the wrong server, and a v6
            // snapshot's unprefixed tmux ids stop naming windows on whatever
            // server happens to be running now.
            6 => {
                tx.execute_batch(
                    "ALTER TABLE snapshots ADD COLUMN server TEXT;
                     ALTER TABLE restore_attempts ADD COLUMN destination_server TEXT;",
                )?;
                version = 7;
            }
            // Pending resume debt. A new, empty table: a database written
            // before this build recorded no debt, and an empty table is the
            // truthful reading of that — nothing is owed, so nothing is
            // carried, which is the conservative half. The alternative
            // (treating every existing binding as owed) would resurrect
            // conversations the user closed under the old build.
            7 => {
                tx.execute_batch(
                    "CREATE TABLE IF NOT EXISTS agent_resume_debt (
                       row_id       INTEGER PRIMARY KEY,
                       boot_id      TEXT    NOT NULL,
                       kind         TEXT    NOT NULL,
                       native_id    TEXT    NOT NULL,
                       session_name TEXT    NOT NULL,
                       window_idx   INTEGER NOT NULL,
                       pane_idx     INTEGER NOT NULL,
                       recorded_at  INTEGER NOT NULL,
                       UNIQUE (boot_id, kind, native_id, session_name,
                               window_idx, pane_idx)
                     );",
                )?;
                version = 8;
            }
            // Whether tmux owned a window's name. Nullable, and every existing
            // row keeps NULL — "this row cannot say". Equivalence reads that
            // as "the name is not an identity" and skips it, which is the only
            // safe reading: the rows were written by a build that could not
            // distinguish a name the user chose from one tmux was still
            // deriving, so a captured `tmux` or `bash` in them may be either.
            // Believing them would keep exactly the snapshots the user already
            // has pinned out of retention forever.
            8 => {
                tx.execute_batch(
                    "ALTER TABLE window_rows ADD COLUMN auto_named INTEGER
                     CHECK (auto_named IN (0,1))",
                )?;
                version = 9;
            }
            // A placement carries its session's name.
            //
            // Reading the name through the row link loses it for exactly the
            // rows that need it most: a placement whose session vanished
            // between the tmux read and the compositor read is still worth
            // keeping, and with no name there is nothing to attach to.
            //
            // Existing rows are backfilled from the link where it resolves; a
            // row whose link is already gone keeps the empty default, which
            // is honest — that name was never recorded and cannot be invented.
            9 => {
                // Only databases that already have the table need altering.
                // A database migrating up from further back may not have
                // reached it yet; the base schema creates it with the column
                // already present, so there is nothing to add.
                let has_table: bool = tx.query_row(
                    "SELECT COUNT(*) FROM sqlite_master
                      WHERE type='table' AND name='terminal_windows'",
                    [],
                    |r| r.get::<_, i64>(0),
                )? == 1;
                if has_table {
                    let has_column: bool = tx
                        .prepare("SELECT * FROM terminal_windows LIMIT 0")?
                        .column_names()
                        .contains(&"session_name");
                    if !has_column {
                        tx.execute_batch(
                            "ALTER TABLE terminal_windows
                               ADD COLUMN session_name TEXT NOT NULL DEFAULT '';

                             UPDATE terminal_windows
                                SET session_name = COALESCE(
                                      (SELECT name FROM session_rows
                                        WHERE session_rows.row_id
                                              = terminal_windows.session_row_id),
                                      '');",
                        )?;
                    }
                }
                version = 10;
            }
            // Conversation titles. `summary` is renamed rather than kept
            // beside them: nothing ever wrote it, and a column promising a
            // paraphrase of a transcript next to one holding a title is how a
            // reader concludes osm stores the former.
            10 => {
                let has_table: bool = tx.query_row(
                    "SELECT COUNT(*) FROM sqlite_master
                      WHERE type='table' AND name='agent_sessions'",
                    [],
                    |r| r.get::<_, i64>(0),
                )? == 1;
                if has_table {
                    let columns = tx
                        .prepare("SELECT * FROM agent_sessions LIMIT 0")?
                        .column_names()
                        .iter()
                        .map(|c| c.to_string())
                        .collect::<Vec<_>>();
                    if columns.iter().any(|c| c == "summary") {
                        tx.execute_batch(
                            "ALTER TABLE agent_sessions RENAME COLUMN summary TO title",
                        )?;
                    } else if !columns.iter().any(|c| c == "title") {
                        tx.execute_batch("ALTER TABLE agent_sessions ADD COLUMN title TEXT")?;
                    }
                    if !columns.iter().any(|c| c == "title_source") {
                        tx.execute_batch(
                            "ALTER TABLE agent_sessions ADD COLUMN title_source TEXT",
                        )?;
                    }
                    if columns.iter().any(|c| c == "alive") {
                        tx.execute_batch("ALTER TABLE agent_sessions DROP COLUMN alive")?;
                    }
                }
                version = 11;
            }
            // Unknown placement. One added column with a default; see the
            // module docs for why the default is `known`.
            11 => {
                tx.execute_batch(
                    "ALTER TABLE snapshots ADD COLUMN placement_state TEXT NOT NULL
                     DEFAULT 'known'
                     CHECK (placement_state IN ('known','unknown','disabled'))",
                )?;
                version = 12;
            }
            // Snapshot ids that are never reissued.
            //
            // `AUTOINCREMENT` cannot be added by `ALTER TABLE`, so the table
            // is rebuilt and every row copied across with the id it already
            // has: a snapshot keeps its identity, and everything that
            // references it — every `snapshot_id` in this database — keeps
            // pointing at the same row. The rebuild runs with foreign keys
            // off and `legacy_alter_table` on, exactly as the 5 → 6 rebuild
            // does and for the same two reasons: the children must not be
            // cascaded away by the `DROP`, and they must go on naming
            // `snapshots` across the rename.
            //
            // Inserting explicit ids into an `AUTOINCREMENT` table seeds
            // `sqlite_sequence` with the highest of them, so the first
            // snapshot written afterwards continues from where the database
            // left off rather than from 1. An empty table seeds nothing,
            // which is right: there is no id to avoid.
            12 => {
                tx.execute_batch(
                    "CREATE TABLE snapshots_v13 (
                       id       INTEGER PRIMARY KEY AUTOINCREMENT,
                       taken_at INTEGER NOT NULL,
                       boot_id  TEXT    NOT NULL,
                       reason   TEXT    NOT NULL,
                       state    TEXT    NOT NULL
                                CHECK (state IN ('building','complete',
                                                 'restore_in_progress',
                                                 'restored','failed')),
                       unresolved INTEGER NOT NULL DEFAULT 0
                                  CHECK (unresolved IN (0,1)),
                       server TEXT,
                       placement_state TEXT NOT NULL DEFAULT 'known'
                                       CHECK (placement_state IN ('known','unknown',
                                                                  'disabled'))
                     );
                     INSERT INTO snapshots_v13
                       (id, taken_at, boot_id, reason, state, unresolved, server,
                        placement_state)
                       SELECT id, taken_at, boot_id, reason, state, unresolved, server,
                              placement_state
                       FROM snapshots;
                     DROP TABLE snapshots;
                     ALTER TABLE snapshots_v13 RENAME TO snapshots;",
                )?;
                version = 13;
            }
            _ => return Ok(false),
        }
    }
    set_meta(&tx, "schema_version", &version.to_string())?;
    tx.commit()?;
    Ok(true)
}

/// Create this build's schema and stamp its version, atomically.
///
/// One `IMMEDIATE` transaction around both halves, and that is load-bearing:
/// `open` runs in every `osm` process, and tmux fires hooks in parallel. With
/// the two halves separate, a process that opened between them saw a database
/// that had `snapshots` but no `schema_version` — [`OnDisk::Unversioned`] —
/// and moved the database another process was in the middle of creating
/// aside. The tables are created by the transaction that also writes the
/// version, so no reader can ever observe one without the other.
fn create_schema(conn: &mut Connection, preserved: Option<(String, Option<u32>)>) -> Result<()> {
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    tx.execute_batch(SCHEMA_SQL)?;
    tx.execute_batch(CASCADE_TRIGGERS_SQL)?;
    set_meta(&tx, "schema_version", &SCHEMA_VERSION.to_string())?;
    if let Some((backup, version)) = preserved {
        set_meta(&tx, "preserved_db_path", &backup)?;
        set_meta(
            &tx,
            "preserved_db_at",
            &crate::boot::now_epoch().to_string(),
        )?;
        if let Some(v) = version {
            set_meta(&tx, "preserved_db_version", &v.to_string())?;
        }
    }
    tx.commit()?;
    Ok(())
}

pub fn open(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create state dir {}", parent.display()))?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    }

    // Nothing below — not even opening a connection — may run while another
    // process is classifying, migrating or preserving this file. See
    // [`migration_lock_path`] for why the boundary has to be this wide.
    let _guard = migration_guard(path)?;

    // Before a connection is opened, because opening one *creates*
    // `state.db`: a preservation that a crash interrupted has to be finished
    // or undone while the directory still shows what happened. Starting a
    // fresh database over the top of it is how the user's only snapshots
    // ended up in a backup nothing had recorded, with the engine reporting a
    // healthy empty database.
    let recovered = reconcile_preservation(path)?;
    if let Some((backup, _)) = &recovered {
        eprintln!(
            "osm: {}: finished a preservation an earlier run did not: it is now {} \
             (see `osm status --json`)",
            path.display(),
            backup
        );
    }
    let mut conn = open_connection(path)?;

    // Never a DROP. An incompatible database is moved aside intact and a
    // fresh one takes its place, so the worst an unexpected version can cost
    // is the *reachability* of the old snapshots, never the snapshots.
    let preserved = match on_disk_schema(&conn) {
        // Either a fresh database, or one somebody else already brought up to
        // date while we queued for the lock. This is the arm that stops
        // thirty-two openers producing thirty-two backups.
        OnDisk::Empty | OnDisk::Version(SCHEMA_VERSION) => None,
        // Migrated in place where the shape allows it, so the user keeps
        // their snapshots *and* their file.
        OnDisk::Version(v) if migrate(&mut conn, v)? => None,
        other => {
            let (label, version) = match other {
                OnDisk::Version(v) => (format!("v{v}"), Some(v)),
                _ => ("unversioned".to_string(), None),
            };
            // Fold the WAL back into the database file first, so what is
            // moved aside is complete on its own. If that fails the WAL still
            // holds committed data, so it has to travel with the file instead
            // — and if it did neither, this stops rather than preserving a
            // database that is missing its most recent transactions.
            let checkpoint = conn.pragma_update(None, "wal_checkpoint", "TRUNCATE");
            drop(conn);
            let (backup, moved) = preserve_aside(path, &label, version)?;
            if let Err(e) = checkpoint {
                if !moved.contains(&"-wal") {
                    anyhow::bail!(
                        "could not checkpoint {} before preserving it ({e}), and it has no \
                         -wal to preserve alongside; refusing to move aside a database that \
                         may be missing its most recent transactions",
                        path.display()
                    );
                }
                eprintln!(
                    "osm: {}: could not checkpoint before preserving it ({e}); \
                     its -wal was preserved alongside {} instead",
                    path.display(),
                    backup.display()
                );
            }
            eprintln!(
                "osm: {}: schema {label} is not this build's schema {SCHEMA_VERSION}; \
                 preserved as {} and starting a fresh database \
                 (see `osm status --json`)",
                path.display(),
                backup.display()
            );
            fs::set_permissions(&backup, fs::Permissions::from_mode(0o600)).with_context(|| {
                format!("restrict permissions on the preserved {}", backup.display())
            })?;
            conn = open_connection(path)?;
            Some((backup.display().to_string(), version))
        }
    };

    // A preservation this process finished on someone else's behalf still has
    // to be recorded: the database it moved aside is nowhere the engine looks
    // any more, and `osm status --json` is the only thing that says so.
    create_schema(&mut conn, preserved.or(recovered))?;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(conn)
}
