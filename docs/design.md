# Omarchy Session Memory — Design

**Date:** 2026-08-24
**Status:** Approved design, ready for implementation planning
**Repo (new, public):** `n8group-oss/omarchy-session-memory`
**Plugin id:** `io.github.n8group-oss.sessionmemory`
**License:** MIT

## Purpose

Restore a developer's full working state after a reboot: Hyprland workspaces
populated with terminal windows, each terminal attached to the tmux session it
had before, each pane back in its working directory, and — the headline feature
— AI coding-agent conversations (Claude Code and Codex) resumed in the panes
where they were running. OpenCode is discovered and listed but never captured
or resumed automatically; see "Agent resume" for why.

The project is distributed as an Omarchy marketplace plugin: a Quickshell bar
widget and menu that surface session state, backed by a standalone Rust engine
that owns capture, storage, and restore.

### Why this exists

The current implementation lives inside a private repo (`ops/ai-runtime/`) as
roughly 2600 lines of bash across `ai-agent-session-index`, `ai-agent-recover`,
`ai-desktop-capture`, `ai-desktop-restore`, `ai-sessions`, and
`ai-tmux-env-sync`. It works, but it is machine-specific (Ghostty only, one
user's paths), hard to maintain at that size, and depends on a chain of
loosely-coupled scripts communicating through marker files. This design keeps
the hard-won operational knowledge — the Hyprland dispatch quirks, boot-race
guards, agent session discovery — and rebuilds it as a maintainable, publicly
installable product.

The existing bash is reference material, not a migration source. Nothing is
copied verbatim.

### Non-goals for v1

- Restoring pane scrollback.
- Resurrecting arbitrary foreground processes (only shells and agent resumes).
- Multiple terminal clients attached to one tmux session.
- Non-tmux terminal windows.
- Custom tmux sockets.
- Replacing Herdr or tmux-resurrect for users who already rely on them; this
  engine is standalone and does not integrate with either.

## Architecture

Four surfaces, with clear ownership boundaries:

| Surface | Contents | Installed by |
|---|---|---|
| Marketplace plugin | `manifest.json`, `BarWidget.qml`, `Menu.qml` | `omarchy plugin add` |
| Engine binary | `osm` | `install.sh` / AUR package |
| systemd user units | `osm.service`, `osm-restore.service` | engine install |
| tmux hooks | namespaced `set-hook` entries | `osm install-hooks` |

The QML plugin contains no logic. It invokes `osm` through an argv array and
renders the JSON it returns. All state, all decisions, and all system
interaction live in the Rust binary.

### Repository layout

```
omarchy-session-memory/
├── manifest.json
├── BarWidget.qml
├── Menu.qml
├── preview.png
├── README.md
├── LICENSE
├── engine/
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs        # CLI dispatch
│       ├── db.rs          # schema, migrations, queries
│       ├── capture.rs     # snapshot transaction
│       ├── restore.rs     # boot restore orchestration
│       ├── tmux.rs        # tmux invocation + parsing
│       ├── hypr.rs        # hyprctl invocation + Lua dispatch
│       ├── agents/
│       │   ├── mod.rs     # AgentAdapter trait, registry
│       │   ├── claude.rs
│       │   ├── codex.rs
│       │   └── opencode.rs
│       └── ipc.rs         # status/sessions JSON for QML
├── systemd/
│   ├── osm.service
│   └── osm-restore.service
└── install.sh
```

### manifest.json

```json
{
  "schemaVersion": 1,
  "id": "io.github.n8group-oss.sessionmemory",
  "name": "Session Memory",
  "version": "0.1.0",
  "author": "n8group-oss",
  "license": "MIT",
  "description": "Restores tmux sessions, workspace placement, and AI agent conversations after reboot.",
  "kinds": ["bar-widget"],
  "entryPoints": { "barWidget": "BarWidget.qml" },
  "barWidget": {
    "displayName": "Session Memory",
    "category": "System",
    "allowMultiple": false,
    "defaultSection": "right"
  }
}
```

`Menu.qml` is loaded internally by `BarWidget.qml` through a `Loader`, keeping
the same `moduleName`. It is not declared as a second manifest kind.

### CLI surface

```
osm snapshot [--debounced] [--reason <str>]
osm restore [--dry-run]
osm status --json
osm sessions --json
osm resume <agent-session-id>
osm focus <tmux-session>
osm kill <tmux-session>
osm daemon
osm install-hooks | uninstall-hooks
osm uninstall
```

`status --json` includes `protocol_version`. The QML widget refuses to render a
session list when the major version does not match what it was built against,
and shows an explicit upgrade-required state instead.

## Data model

SQLite at `$XDG_STATE_HOME/osm/state.db` (default `~/.local/state/osm/state.db`),
WAL mode, directory `0700`, file `0600`.

Snapshots are generational: each capture writes a complete new generation, and
restore reads one pinned generation. Rows are snapshot-scoped surrogate keys
carrying the native identity alongside, so the same tmux session appears once
per snapshot without key collisions.

```sql
CREATE TABLE snapshots (
  id         INTEGER PRIMARY KEY,
  taken_at   INTEGER NOT NULL,          -- unix epoch
  boot_id    TEXT    NOT NULL,          -- /proc/sys/kernel/random/boot_id
  reason     TEXT    NOT NULL,          -- hook name, timer, shutdown, manual
  state      TEXT    NOT NULL           -- building|complete|restore_in_progress
                                        -- |restored|failed
             CHECK (state IN ('building','complete','restore_in_progress',
                              'restored','failed')),
  -- Set when a restore of this snapshot starts, cleared by a verified
  -- success or by the capture that carries its unrecovered sessions
  -- forward. See "Carrying unrecovered sessions forward".
  unresolved INTEGER NOT NULL DEFAULT 0 CHECK (unresolved IN (0,1))
);

CREATE TABLE session_rows (
  row_id           INTEGER PRIMARY KEY,
  snapshot_id      INTEGER NOT NULL REFERENCES snapshots(id) ON DELETE CASCADE,
  tmux_session_id  TEXT    NOT NULL,    -- native $N
  name             TEXT    NOT NULL,
  active_window_id TEXT,                -- native @N
  UNIQUE (snapshot_id, tmux_session_id)
);

-- A window belongs to as many sessions as it is linked into (`link-window`),
-- so windows are keyed by snapshot, never by session, and the membership
-- lives in session_window_links. Keying them by session made every capture
-- fail (duplicate pane inserts) while any link existed on the server.
CREATE TABLE window_rows (
  row_id          INTEGER PRIMARY KEY,
  snapshot_id     INTEGER NOT NULL REFERENCES snapshots(id) ON DELETE CASCADE,
  tmux_window_id  TEXT    NOT NULL,     -- native @N
  name            TEXT    NOT NULL,
  layout          TEXT    NOT NULL,     -- tmux window_layout string
  active_pane_id  TEXT,                 -- native %N
  zoomed          INTEGER NOT NULL DEFAULT 0 CHECK (zoomed IN (0,1)),
  UNIQUE (snapshot_id, tmux_window_id)
);

CREATE TABLE session_window_links (
  row_id          INTEGER PRIMARY KEY,
  session_row_id  INTEGER NOT NULL REFERENCES session_rows(row_id) ON DELETE CASCADE,
  window_row_id   INTEGER NOT NULL REFERENCES window_rows(row_id) ON DELETE CASCADE,
  idx             INTEGER NOT NULL,     -- the window's index *in this session*
  active          INTEGER NOT NULL DEFAULT 0 CHECK (active IN (0,1)),
  UNIQUE (session_row_id, window_row_id),
  UNIQUE (session_row_id, idx)
);

CREATE TABLE pane_rows (
  row_id            INTEGER PRIMARY KEY,
  window_row_id     INTEGER NOT NULL REFERENCES window_rows(row_id) ON DELETE CASCADE,
  tmux_pane_id      TEXT    NOT NULL,   -- native %N
  idx               INTEGER NOT NULL,
  cwd               TEXT    NOT NULL,
  title             TEXT,
  foreground_cmd    TEXT,               -- observation only, never replayed
  dead              INTEGER NOT NULL DEFAULT 0 CHECK (dead IN (0,1)),
  restore_policy    TEXT    NOT NULL    -- shell|agent_resume|none
                    CHECK (restore_policy IN ('shell','agent_resume','none')),
  restore_argv      TEXT,               -- JSON array, only for agent_resume
  agent_kind        TEXT,               -- claude|codex|opencode
  agent_session_id  TEXT,
  agent_confidence  REAL,               -- 0.0-1.0, NULL when unbound
  UNIQUE (window_row_id, tmux_pane_id)
);

CREATE TABLE terminal_windows (
  row_id           INTEGER PRIMARY KEY,
  snapshot_id      INTEGER NOT NULL REFERENCES snapshots(id) ON DELETE CASCADE,
  hypr_address     TEXT    NOT NULL,
  window_class     TEXT    NOT NULL,
  terminal_kind    TEXT    NOT NULL,    -- ghostty|alacritty|kitty|foot|other
  session_row_id   INTEGER REFERENCES session_rows(row_id) ON DELETE SET NULL,
  workspace_kind   TEXT    NOT NULL,    -- numbered|named|special
  workspace_ref    TEXT    NOT NULL,
  monitor_connector TEXT   NOT NULL,    -- DP-1
  monitor_desc     TEXT,                -- make/model/serial, durable across replug
  monitor_scale    REAL,
  monitor_transform INTEGER,
  floating         INTEGER NOT NULL DEFAULT 0 CHECK (floating IN (0,1)),
  rel_x            REAL,                -- geometry relative to monitor, 0.0-1.0
  rel_y            REAL,
  rel_w            REAL,
  rel_h            REAL,
  UNIQUE (snapshot_id, hypr_address)
);

-- Which conversations a restore has put a *pane* back for without (yet)
-- putting the conversation into it. The one reason a capture may carry an
-- agent binding forward instead of recording what it detected: a capture
-- landing in the reboot window sees bare shells everywhere.
--
-- Recorded per object by the restore that incurred it, with the boot it
-- belongs to and the moment it was taken on — never inferred from a ratio. The
-- rule this replaced compared the *set* of bound conversations between two
-- captures and threw the new map away when more than half had gone, which
-- reads a user quitting conversation A and starting B in the same pane as 100%
-- loss: the correct binding was discarded and A carried forward for ever.
CREATE TABLE agent_resume_debt (
  row_id       INTEGER PRIMARY KEY,
  boot_id      TEXT    NOT NULL,
  kind         TEXT    NOT NULL,
  native_id    TEXT    NOT NULL,
  session_name TEXT    NOT NULL,        -- the place, in the identity a restore preserves
  window_idx   INTEGER NOT NULL,
  pane_idx     INTEGER NOT NULL,
  recorded_at  INTEGER NOT NULL,
  UNIQUE (boot_id, kind, native_id, session_name, window_idx, pane_idx)
);

CREATE TABLE agent_sessions (
  row_id        INTEGER PRIMARY KEY,
  kind          TEXT    NOT NULL,       -- claude|codex|opencode
  native_id     TEXT    NOT NULL,
  project_dir   TEXT,
  store_path    TEXT,                   -- jsonl path where applicable
  last_active   INTEGER,
  size_bytes    INTEGER,
  title         TEXT,                   -- one line; NULL when none could be derived
  title_source  TEXT,                   -- agent|first_prompt; NULL exactly when title is
  UNIQUE (kind, native_id)
);

CREATE TABLE restore_attempts (
  id           INTEGER PRIMARY KEY,
  snapshot_id  INTEGER NOT NULL REFERENCES snapshots(id) ON DELETE CASCADE,
  started_at   INTEGER NOT NULL,
  finished_at  INTEGER,
  state        TEXT    NOT NULL         -- running|succeeded|partial|failed
               CHECK (state IN ('running','succeeded','partial','failed'))
);

CREATE TABLE restore_objects (
  row_id      INTEGER PRIMARY KEY,
  attempt_id  INTEGER NOT NULL REFERENCES restore_attempts(id) ON DELETE CASCADE,
  kind        TEXT    NOT NULL,         -- session|window|pane|terminal|agent
  ref         TEXT    NOT NULL,         -- natural key within kind
  state       TEXT    NOT NULL,         -- pending|done|adopted|skipped|failed
  detail      TEXT,
  UNIQUE (attempt_id, kind, ref)
);

CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
```

`PRAGMA foreign_keys = ON` on every connection. Snapshot retention keeps the
newest 20 complete generations plus any generation referenced by a
non-terminal restore attempt; deletion cascades.

### Conversation titles: the one thing read out of a transcript

A conversation carries a **title** — one short line saying what it is about —
in `agent_sessions.title`, with `agent_sessions.title_source` saying where
that line came from. It is written only for conversations a capture bound to a
pane, which bounds the table by the panes that have ever run an agent rather
than by the thousands of conversations on the machine, and it is derived
outside every write transaction, from the agent stores a capture already reads
before it takes the database's write lock.

There are two sources and they are not the same claim:

* **`agent`** — the agent's own name for the conversation. Claude Code writes
  `{"type":"ai-title","aiTitle":…}` records into its transcript and revises
  them as it goes; the last one is the current name. A title the agent wrote
  about itself is not transcript content.

* **`first_prompt`** — **one truncated line of the user's first message**.
  This *is* transcript content, and it relaxes the original rule ("no
  transcript content is ever displayed or stored — titles and ids only"). The
  relaxation is deliberate and it is bounded:

  * one line, whitespace collapsed, at most 120 characters, with an ellipsis
    when it was cut, and stripped of everything that would make the panel draw
    something other than what osm recorded: the control characters (C0, C1,
    the ANSI escapes), and the Unicode format characters that reverse or
    conceal text — the bidi overrides and isolates (U+202A–U+202E,
    U+2066–U+2069), the zero-width and invisible ones (U+200B–U+200F,
    U+2060–U+2064, U+FEFF, U+00AD), and the tag characters at U+E0000. The
    emoji variation selectors (U+FE00–U+FE0F) are kept: they choose a
    presentation for the visible character before them and can neither hide
    nor reorder anything;
  * from the user's *opening* message only — never a reply, a tool result, an
    attachment, or anything the agent writes as a user-shaped record (Claude's
    `isMeta` records and slash commands, the preamble Codex injects as a
    role-`user` message);
  * never from a conversation osm cannot attribute: a record naming a
    different conversation is refused, not borrowed;
  * off entirely with `privacy.prompt_titles = false`, which leaves Claude
    conversations named by whatever their agent called them and every Codex
    conversation untitled. Switching it off is a **revocation**, not merely a
    rule for the next derivation: `osm status --json` stops reporting every
    prompt-derived title the moment the key reads `false`, and the next
    capture clears the stored `title`/`title_source` of every conversation
    whose title came from a prompt — not only the ones a pane is still
    running. Titles an agent wrote about its own conversation are untouched;
    they were never what this key governed.

  Why relax it at all, measured rather than assumed: of the 2497
  conversations on the machine this was built for, 2386 are Codex's and carry
  no title of their own, and 37 of the 111 Claude ones have no `ai-title`
  either. Titles-only would have meant 2423 blank rows out of 2497, and a
  blank goal reads as a session with no purpose. With the fallback, 2336 come
  back named.

Where neither source yields anything the answer is **no title** — rendered
*untitled*, never fabricated and never blank. Nothing else from a transcript
is read, stored or shown.

Every read is bounded and stops at its answer: a 256 KiB suffix for Claude's
title, a 128 KiB prefix for its first prompt, a 512 KiB prefix for a Codex
rollout, each sized from measurements over a real 14 GB store and stated on
the constant in `src/agent/title.rs`. A title beyond the bound is *no title*.

### A session's goal

`osm status --json` gives each session a `goal`: **the title of the most
recently active conversation in the session that has one**, carrying the kind
and id of the conversation it came from so it can never be read against
another. Never a summary stitched out of several — osm does not write
sentences nobody said — and never the busiest or the biggest conversation's,
because after a reboot what is wanted back is what was being done last. Every
conversation the session held is reported beside it, newest first, with its
own title and the window and pane it was in, so the choice can be checked
rather than trusted.

## Configuration

TOML at `$XDG_CONFIG_HOME/osm/config.toml`, all keys optional:

```toml
[restore]
auto = true                  # restore at login; false = menu-triggered only
terminal = "auto"            # auto | ghostty | alacritty | kitty | foot
readiness_timeout_secs = 30

[agents]
auto_resume = true
auto_resume_max_age_mins = 30
enabled = ["claude", "codex", "opencode"]

[capture]
debounce_max_latency_secs = 5
fallback_interval_secs = 120
keep_snapshots = 20

[privacy]
prompt_titles = true         # false: only titles an agent wrote itself, so
                             # every Codex conversation reads "untitled".
                             # Switching it off also revokes the prompt-derived
                             # titles already on record.
```

Missing file means defaults. Invalid values are reported through
`osm status --json` and surfaced by the widget rather than failing silently.

A config that fails to load falls back to the defaults for everything except
`privacy.prompt_titles`, which is forced **off** (`Config::strict_fallback`).
The defaults exist so that a typo does not quietly stop osm tracking panes;
they must not be allowed to overrule something the user wrote down. A file
saying `prompt_titles = false` with an unrelated misspelling in it used to
fall all the way back to reading first prompts again — an explicit refusal
reversed by a mistake that had nothing to do with it. Nobody consents by
accident.

## Capture

### Triggers

Namespaced tmux hooks, installed idempotently and append-only so existing user
hooks survive. Each hook index is tagged with an `osm:` marker in its command
so `uninstall-hooks` can remove exactly its own entries.

Hooked events: `session-created`, `session-renamed`, `session-closed`,
`window-linked`, `window-unlinked`, `window-renamed`, `after-split-window`,
`after-kill-pane`, `after-select-pane`, `after-select-window`,
`after-resize-pane`, `client-attached`, `client-detached`.

Each fires `osm snapshot --debounced`, which is a cheap message to the daemon,
not a capture. The daemon coalesces bursts with a maximum latency of 5 seconds,
so a rapid sequence of splits produces one snapshot but never delays capture
indefinitely. A 120-second fallback timer covers state changes no hook reports.
A shutdown-ordered systemd unit takes a final snapshot before the session dies.

Snapshots taken by hooks are suppressed while a restore holds the lock; the
capture path never runs re-entrantly.

### Snapshot transaction

1. Insert `snapshots` row with `state='building'` and the current `boot_id`.
2. Query tmux in three calls (`list-sessions`, `list-windows -a`,
   `list-panes -a`) with explicit `-F` format strings that yield native ids,
   cwd, active and zoom flags, pane title, and window layout. Records are
   framed by printable ASCII tokens — tmux ≤ 3.6 rewrites non-printable bytes
   in format output — and every field is escaped by tmux itself before it is
   framed, so a window name, path, title or command containing a token or a
   newline is data rather than an outage.

   **osm requires tmux 3.7 or newer and refuses to start below it.** The
   framing above survives an older tmux; the *values* do not. tmux ≤ 3.6
   rewrites every non-ASCII byte and every newline in format output to `_`
   inside the server, before osm receives the field, so a pane in
   `/home/u/żółć` is persisted as `/home/u/______` and restored into that path.
   The damage is done upstream of every escape this engine applies, and it is
   silent. Debian bookworm (3.3a), Ubuntu (3.4) and Debian trixie (3.5a) are
   therefore all excluded; refusing loudly at startup is the only honest
   answer, and it is checked once per process from `tmux -V`.

   The version is only half of it. tmux decides whether a command client gets
   raw UTF-8 or the sanitised form from *that client's* `LC_ALL`/`LC_CTYPE`/
   `LANG`, so even 3.7c writes `/tmp/____` for `/tmp/żółć` when osm runs
   without a locale in its environment — the ordinary state of a systemd user
   unit and of a tmux hook. Every tmux invocation therefore passes `-u`, which
   takes the decision away from an environment variable nobody sets on
   purpose.
3. Query `hyprctl -j clients` and `hyprctl -j monitors`. Validate both parse as
   JSON arrays before use. Map each terminal window's pid to a tmux client and
   session by walking the process tree.
4. For each pane, run agent detection (below) and record a binding only above
   the confidence threshold.
5. Set `state='complete'`, prune old generations, commit.

A snapshot that fails at any step leaves its row in `building` and is ignored by
every reader; the next successful capture prunes it.

### Carrying unrecovered sessions forward

Restore selects its source by recency, so an incompletely restored snapshot
would be superseded by the very capture that recorded the incomplete result,
and then deleted by retention — losing the sessions the restore never put
back, with no error anywhere.

The debt is tracked **per session**, on `session_rows.unresolved`, and it is
created only by a restore. Every session of a snapshot is marked outstanding
when a restore of it *starts* (a restore killed mid-run never reaches any
reporting code), and each one the restore verifiably delivered — created or
adopted, with no degradation reported against it — is discharged when it
finishes. `snapshots.unresolved` is a cache of "any session row is still
outstanding", kept only so retention can filter on one column; while it is set,
retention may not delete the snapshot.

A whole-snapshot flag could not express this, and three separate failures came
of trying:

- **A partially restored session resolved its own source.** Treating "a live
  session holds this name" as recovery let a restore that failed after four of
  nine panes bless the truncated session it had just left behind. Resolution
  now requires full topology equivalence (`equiv::difference`, the same
  judgement adoption makes). A live session that holds the name but not the
  topology resolves nothing and is not carried either — a snapshot holding two
  sessions with one name could never be restored — so the source keeps the debt
  and stays out of retention.
- **A mixed live/carried pair split a linked window.** Where two sessions share
  a window and the restore delivered only one of them, the carried one used to
  get a second, independent copy. A restore now records captured→live window
  identity in `restore_window_map`, so the carried session is *linked* into the
  live window instead. The identity is verified against the row before it is
  believed, and a bare id is trusted as same-server only within one boot.
- **Deleting or renaming a session brought it back.** Since debt is only ever
  created by a restore that failed to deliver a specific session, a session that
  was delivered and then closed simply stays closed, and a rename adds nothing
  to carry. The resolution path is the tombstone.

Every capture therefore writes a superset: the live topology, plus each session
an earlier snapshot is still owed and this server has not demonstrably got
back. Carried rows keep their contents verbatim; only the tmux ids are
rewritten under a `carried:<snapshot>:` prefix, so they cannot collide with a
live `$0`/`@1`/`%2` and so one carried window keeps a single identity across
generations. The new snapshot owes whatever it carried.

A verified restore publishes the topology it produced as this boot's snapshot
and retires its source in the **same** transaction, while still holding the
restore lock: retiring first would leave an interval with no `complete`
snapshot on the machine at all. If that publication fails, the attempt is
`unsecured` rather than `succeeded` — the sessions are back, but nothing
durable records them — and `osm restore` exits non-zero so systemd restarts
it instead of recording a clean success.

### Schema versions

`db::open` never destroys a database it cannot read. A version it can migrate
is migrated in place; anything else — an older shape, an unversioned file, a
database from a newer build — is checkpointed and moved aside as
`state.db.v<N>.bak`, never overwriting an earlier backup, and a fresh database
takes its name. `osm status --json` reports it under `database.preserved`, and
says what is *in* it: the file is opened read-only and its snapshots counted,
so a backup holding work reads differently from one holding none, and one that
could not be read at all is reported as unknown rather than as either. The
notice used to assert that the snapshots in it were intact without ever
opening it — which on a preserved file holding nothing told its owner, every
time the panel polled, that he had lost work he never had. The count is
reported as a count and nothing more: it says how many rows are in one table,
not that anything is behind them, not that the newest is not a half-written
`building` row, and not that the file is internally consistent. A backup whose
`snapshots` table reads perfectly and holds nothing else answers the query and
holds nothing anyone could restore, so the notice says *contains N snapshot
records* rather than claiming they are intact. A file holding
nothing says it can be deleted; osm does not delete it, because it is the
user's file. The read uses `immutable=1` unless a non-empty `-wal` sits beside
the backup, so counting it neither creates SQLite's sidecars in the user's
state directory nor misses rows that are only in a WAL.

The whole of `open` runs under an exclusive lock beside the database, and the
classification is re-formed under that lock rather than trusted from before it.
Both halves are load-bearing: with the sequence unlocked, thirty-two concurrent
openers on one older database produced ten preservation operations, nine of them
moving aside *empty* databases caught mid-creation and reporting one of those as
the user's preserved data. Narrowing the lock to just the preservation branch is
not enough either — SQLite manages a WAL database's `-wal`/`-shm` by path, so a
process merely opening the file while another moves it aside fails with
`SQLITE_IOERR_DELETE`. Backups are created with `link(2)`, which fails
atomically on an existing destination where `rename(2)` would overwrite it, and
a sidecar that cannot travel with its database is a hard error.

### Agent detection

Detection is adapter-specific and confidence-scored rather than a single
heuristic. Signals: the pane's foreground process and its descendants, the
process argv (an explicit `--resume <id>` or `--session <id>` is decisive),
open file descriptors pointing at a known transcript path, the process cwd
matched against the session's project directory, and recent write time on
candidate transcripts.

A binding is stored only when signals agree above threshold. Ambiguous panes
are stored unbound, and the menu offers manual selection rather than the engine
guessing the newest file. This matters because the binding recorded at the last
snapshot is the only thing that survives reboot — after a restart there is no
`/proc` to re-derive it from.

Discovery only ever finds conversations **inside** the configured store. An
entry must be a regular file by `lstat` — a symlink named after a UUID is not
a transcript — and must resolve to somewhere under the resolved store root, so
a project directory that is itself a symlink out of the store contributes
nothing even though the files behind it are ordinary. The root is resolved
once, which is what keeps `~/.claude -> /data/claude` working: an agent home
that is itself a symlink is a normal arrangement, and the whole store is then
reached through one. Every read of a conversation file repeats the regular-file
check on the descriptor it actually obtained, because discovery is a check made
at one moment and the file is opened later, by name — and what is read becomes
a persisted title.

## Restore

`osm-restore.service` is `PartOf=graphical-session.target` and is started after
the compositor has imported the Wayland environment into the systemd user
manager. Reaching `graphical-session.target` alone does not prove Hyprland is
callable, so the engine probes rather than trusts.

```
 1. Acquire flock; exit quietly if another restore holds it.
 2. Readiness probe, bounded at 30s: $WAYLAND_DISPLAY socket exists and
    `hyprctl -j monitors` returns a non-empty array. Abort cleanly on timeout.
 3. Select source snapshot: newest state='complete' with boot_id != current.
    Pin it, set state='restore_in_progress', open a restore_attempts row.
    Never select by MAX(taken_at) alone.
 4. tmux: start the server if absent; apply `set-environment -g` for the
    allow-list (WAYLAND_DISPLAY, HYPRLAND_INSTANCE_SIGNATURE, XDG_RUNTIME_DIR,
    XDG_SESSION_TYPE, XDG_CURRENT_DESKTOP) BEFORE creating any pane.
 5. Reconcile sessions: adopt a live session whose name matches, else create it.
    A name conflict with a session the engine did not create is skipped and
    reported, never clobbered.
 6. Rebuild windows and panes from layout strings; set cwd only when the
    directory still exists; restore active window, active pane, and zoom.
 7. Apply per-pane restore policy: `shell` does nothing, `none` does nothing,
    `agent_resume` is queued for step 9.
 8. Spawn terminals: detect the terminal (config override, else Omarchy default,
    else first available of ghostty/alacritty/kitty/foot) and launch with class
    `osm-restore-<attempt-id>-<session>`. Wait for the window to map, then place
    it: workspace by recorded kind and reference, monitor by descriptor with
    connector then primary as fallbacks, floating geometry rebuilt from the
    stored relative values and clamped to the monitor's usable bounds.
 9. Agent resume pass.
10. Mark the attempt succeeded, partial, or failed; set snapshot state to
    restored or failed; release the lock, which unblocks capture.
```

Every step writes `restore_objects` rows, so an interrupted restore that is
restarted adopts what already exists instead of duplicating it. Ghost cleanup
removes only windows whose class carries an attempt id that the engine owns and
that is no longer live; a class-prefix match alone is never sufficient to kill
a window.

## Agent resume

```rust
trait AgentAdapter {
    fn kind(&self) -> AgentKind;
    fn discover(&self) -> Result<Vec<AgentSession>>;
    /// This agent's transcripts keyed by (device, inode) — the only thing an
    /// open file descriptor is ever matched against.
    fn transcript_index(&self) -> Result<TranscriptIndex>;
    fn resume_argv(&self, id: &str) -> Vec<String>;
    /// Active / Inactive / **Unknown**. Unknown is not Inactive.
    fn is_active_elsewhere(&self, id: &str) -> Result<Liveness>;
    /// Why osm will not capture or resume this agent by itself, or None.
    fn auto_unsupported_reason(&self) -> Option<&'static str>;
}
```

| Adapter | Discovery | Resume | Automatic |
|---|---|---|---|
| claude | scan `~/.claude/projects/*/*.jsonl` for id, cwd, mtime, size | `claude --resume <id>` | yes |
| codex | scan `$CODEX_HOME/sessions/**/rollout-*.jsonl` | `codex resume <uuid>` | yes |
| opencode | `opencode session list --format json` | `opencode --session <id>` | **no** |

OpenCode is driven exclusively through its public CLI. Its on-disk store has
changed shape across versions and is not a stable interface.

That confinement has a consequence the first draft of this design did not
follow through: everything osm does automatically rests on two questions it
must answer from the machine — *which conversation is this pane running*, and
*does anything else have this conversation open* — and the OpenCode CLI answers
neither. Left implicit, it produced a capture path that could never bind
anything (no transcript means at most 0.4 against a 0.75 threshold, so the
threshold was never reachable) and a liveness check that returned a bare
`false`, which callers read as "verified nobody has it". So the answer is
declared once, in `auto_unsupported_reason`, and every consequence follows from
it: no binding, no auto-resume, `osm resume` refuses with `unsupported`, and
`osm status --json` lists the kind under `agents.unsupported`. Discovery still
runs, so the conversations remain listed for a human to open by hand.

**Absent is not failed.** No agent home is no conversations and legitimate; a
home that exists and cannot be read, or an `opencode` that exits non-zero or
prints something other than the array of sessions it documents, is osm being
unable to look. The first is an empty inventory, the second an error — a
per-pane `Failed` during restore (so the run is `partial` and the snapshot
stays retryable), a `problems` entry in `osm agents --json`, and a carry-forward
cause during capture (which still records the topology: losing tmux snapshots
over an unreadable `~/.claude` would be the larger loss).

Any subprocess output parsed as JSON must tolerate leading non-JSON lines.
Version managers such as `mise` print an activation banner ahead of command
output, which corrupts naive parsers. The engine resolves absolute binary paths
where possible and strips content before the first `{` or `[` otherwise.

### Binding: identity, never a proxy for it

A pane is bound to a conversation only when both halves of the evidence come
from **the same process lineage**: a descendant of the pane running under the
adapter's binary name (read from `/proc/<pid>/cmdline`, the same source tmux's
`#{pane_current_command}` uses), or a descendant of that, holding open a file
that **is** one of the transcripts discovery found — matched by device and
inode, not by its path.

Both qualifications are load-bearing. Pooling every descendant's descriptors
let a background job tailing another conversation's transcript bind the pane to
*that* conversation; matching by name let any `.jsonl` with a UUID stem count,
anywhere on the filesystem. File identity also fixes the opposite error: a
symlinked or bind-mounted agent home resolves to the same file and is
recognised.

### Resume preconditions

All must hold before a resume is sent, and the whole sequence runs under an
exclusive lock keyed by `(agent kind, native id)` — taken **before** the
liveness check and released only after the identity is confirmed. Without it,
two `osm resume <same-id>` runs both finish their `/proc` scan before either
agent opens the transcript, both pass, and both send: the exact double attach
the check exists to prevent.

- The target pane exists (exact membership in the live pane list, never `-t`
  resolution) and its foreground process is an idle shell.
- The pane is not in copy mode and its input is not disabled.
- The adapter reports the session is not active elsewhere — and *reports* it.
  `Unknown` is `unsupported`, not a licence to proceed.

The engine then sends the resume argv, bound to the tmux server incarnation the
work was verified against — the check and the send are one tmux operation, so a
server replaced in between cannot receive the input — and waits with a bounded
timeout. Success is **not** a process with the right name appearing: the pane is
re-bound afterwards by the rules above and must hold exactly `(kind, id)`, and
go on holding it. An agent that starts, rejects the id and exits is `failed`.
Outcomes: `resumed`, `active_elsewhere`, `pane_busy`, `pane_missing`,
`unsupported`, `failed`. A failed resume is surfaced in the menu and never
retried by sending a second command into the same pane.

### Which pane a conversation goes back into

Never re-derived from the snapshot. The restore records a captured-pane →
live-pane map as it makes it — creation order for a window it built, the
already-validated layout-cell order for one it adopted — and a resume is
addressed only through that map, only for sessions the run verifiably
delivered, and only on the one server incarnation verified for the whole run.
Navigating by captured session name and window index instead sent a
conversation into an unrelated live session that merely held the same name;
pairing an adopted window's panes by `%N` order sent it into the wrong pane of
the right window.

### Auto-resume policy

Sessions whose last activity is under 30 minutes old are resumed automatically
during the restore pass. Older sessions appear in the menu as one-click
resumable. Both the threshold and the auto-resume behaviour are configurable.

## User interface

### BarWidget.qml

Icon plus live session count. Colour encodes state: normal, restoring, degraded
(restore finished with failures), and engine-unavailable. It polls
`osm status --json` on a 5-second interval and backs off sharply when the engine
is missing or the protocol version mismatches.

The widget invokes `osm` through an argv array. It never constructs a shell
command string, because session names and project paths are attacker-influenced
input in the general case.

### Menu.qml

Sessions grouped by workspace. Each row shows the session name, project
basename, an agent badge, age, and agent state. Row actions: focus, resume
agent, kill (behind a confirmation). A second group lists resumable agent
sessions from the `agent_sessions` inventory that are not currently running.
The footer offers restore now, snapshot now, and settings.

When the engine is absent or incompatible, the menu renders installation or
upgrade instructions instead of a session list.

## Distribution and lifecycle

Installing the marketplace plugin copies QML and `manifest.json` only. It cannot
install a binary, enable systemd units, or configure tmux — so the plugin must
handle the engine-missing state gracefully rather than appear installed and be
silently broken.

The engine is distributed as a GitHub release binary with per-architecture
SHA-256 checksums, with the digest pinned inside the matching `install.sh`
release. An AUR package that owns the binary and the user units is the intended
path once the project stabilises; at that point the marketplace plugin declares
the package as a dependency in its README. The QML never downloads or executes
an installer itself.

`osm uninstall` is the single teardown path: it stops and disables both units,
removes them, removes the engine's tmux hooks while preserving the user's own,
runs `systemctl --user daemon-reload`, removes the binary, and prompts whether
to keep or delete the database.

Unsupported architectures are refused by the installer rather than falling back
to an unverified build.

## Testing

- **Unit (Rust):** tmux format-string parsing, layout round-trip, monitor
  geometry normalisation and clamping, snapshot pruning and cascade,
  restore-source selection across boot ids, JSON parsing with banner prefixes.
- **Adapter contract tests:** each `AgentAdapter` against recorded fixtures for
  discovery and detection, including ambiguous cases that must stay unbound.
- **Integration (headless tmux):** capture a synthetic multi-session layout,
  restore into a clean server, assert topology equality; restart mid-restore and
  assert no duplication; run restore twice and assert idempotency; place a
  conflicting pre-existing session and assert it is skipped, not clobbered.
- **Hyprland-dependent tests:** gated behind an env flag, skipped in CI, run
  manually against a live compositor for placement and ghost cleanup.
- **QML:** the widget is thin enough to verify by driving `osm status --json`
  fixtures, including the protocol-mismatch and engine-missing states.

## Risks

| Risk | Mitigation |
|---|---|
| Marketplace install leaves a non-functional widget | Explicit engine-missing UI state with bootstrap instructions |
| Login-time capture overwrites the restore source | Restore pins a previous-boot snapshot before capture is permitted |
| Restore duplicates sessions after a crash | Per-object attempt state and adoption of live matches |
| Agent double-resume corrupts a conversation | Three-way precondition check plus adapter active-elsewhere probe |
| Agent binding lost before reboot is unrecoverable | Confidence scoring plus manual selection in the menu |
| Monitor layout changed while powered off | Monitor descriptor matching with connector and primary fallbacks, geometry clamped |
| Release binary compromise executes in the user session | Pinned digests, signed release metadata, no QML-initiated download |
