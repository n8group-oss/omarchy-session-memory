use crate::boot;
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension};

/// The snapshot `osm restore` would rebuild from right now: the newest
/// `complete` snapshot that was *not* taken during the current boot.
///
/// The `ORDER BY` here is load-bearing beyond selection — [`prune`] repeats it
/// verbatim to work out which row it must never delete. Change one and you
/// must change the other.
pub fn select_restore_source(conn: &Connection, current_boot: &str) -> Result<Option<i64>> {
    let id = conn
        .query_row(
            "SELECT id FROM snapshots
             WHERE state = 'complete' AND boot_id <> ?1
             ORDER BY taken_at DESC, id DESC
             LIMIT 1",
            [current_boot],
            |r| r.get(0),
        )
        .optional()?;
    Ok(id)
}

pub fn set_state(conn: &Connection, id: i64, state: &str) -> Result<()> {
    conn.execute(
        "UPDATE snapshots SET state = ?2 WHERE id = ?1",
        rusqlite::params![id, state],
    )?;
    Ok(())
}

/// Mark a snapshot as still holding sessions nothing has recovered.
///
/// Set after a restore that did not put everything back, cleared once a later
/// capture has carried the missing sessions into a newer snapshot (see
/// [`crate::capture::write_topology_in`]) or once a restore of it fully
/// succeeds. While it is set the snapshot is exempt from [`prune`], because a
/// newer snapshot is not yet a superset of it and deleting it would lose the
/// difference.
pub fn set_unresolved(conn: &Connection, id: i64, unresolved: bool) -> Result<()> {
    conn.execute(
        "UPDATE snapshots SET unresolved = ?2 WHERE id = ?1",
        rusqlite::params![id, unresolved as i64],
    )?;
    conn.execute(
        "UPDATE session_rows SET unresolved = ?2 WHERE snapshot_id = ?1",
        rusqlite::params![id, unresolved as i64],
    )?;
    Ok(())
}

/// Discharge the debt on exactly the sessions named, leaving the rest alone.
///
/// This is what a restore calls for the sessions it *verifiably* delivered.
/// The ones it did not stay outstanding, which is the whole point: a restore
/// that put back four sessions out of nine used to clear one flag covering all
/// nine.
pub fn resolve_sessions(conn: &Connection, snapshot_id: i64, names: &[&str]) -> Result<()> {
    for name in names {
        conn.execute(
            "UPDATE session_rows SET unresolved = 0 WHERE snapshot_id = ?1 AND name = ?2",
            rusqlite::params![snapshot_id, name],
        )?;
    }
    Ok(())
}

/// Recompute `snapshots.unresolved` from the session rows it covers, and
/// return it.
///
/// The snapshot-level column is a cache of `EXISTS(unresolved session row)`,
/// kept only so [`prune`] can filter on one indexed column instead of a
/// correlated subquery. Every writer of a session row's debt must call this so
/// the two cannot disagree — and they must not, because retention reads the
/// cache and data loss is what happens when it is wrong.
pub fn refresh_unresolved(conn: &Connection, snapshot_id: i64) -> Result<bool> {
    let outstanding: i64 = conn.query_row(
        "SELECT COUNT(*) FROM session_rows WHERE snapshot_id = ?1 AND unresolved = 1",
        [snapshot_id],
        |r| r.get(0),
    )?;
    conn.execute(
        "UPDATE snapshots SET unresolved = ?2 WHERE id = ?1",
        rusqlite::params![snapshot_id, i64::from(outstanding > 0)],
    )?;
    Ok(outstanding > 0)
}

/// Repairs the database state left behind by a restore whose process died
/// (SIGKILL, power loss, Ctrl-C) between marking a snapshot
/// `restore_in_progress` and writing the attempt's terminal state.
///
/// # The caller must already hold the restore lock
///
/// The lock is the liveness oracle: a restore holds it for its whole run, so
/// if we hold it, no restore is running, so every `running` attempt and every
/// `restore_in_progress` snapshot belongs to a process that is gone. Calling
/// this without the lock would happily "reclaim" a restore that is very much
/// alive and hand its snapshot to a second restorer.
///
/// Returns the number of snapshots handed back to `complete`.
pub fn reclaim_orphaned_restores(conn: &mut Connection) -> Result<usize> {
    let now = boot::now_epoch();
    let tx = conn.transaction()?;

    // The two statements that matter for reachability. `restore_in_progress`
    // is invisible to select_restore_source, so without this the newest
    // pre-reboot snapshot is wedged forever and the user is told there is
    // nothing to restore while their state sits intact in the database.
    tx.execute(
        "UPDATE restore_attempts SET state = 'failed', finished_at = ?1
         WHERE state = 'running'",
        [now],
    )?;
    // `unresolved` as well as `complete`: a restore that was killed mid-run
    // put back some unknown part of the snapshot, so until a capture has
    // carried the rest forward this row is the only record of the difference
    // and must not be pruned or superseded.
    let snapshots = tx.execute(
        "UPDATE snapshots SET state = 'complete', unresolved = 1
         WHERE state = 'restore_in_progress'",
        [],
    )?;
    // The same statement one level down. A restore marks every session row
    // outstanding when it starts, so this is normally already true; it is
    // repeated here so a database that reached `restore_in_progress` by some
    // other route (a hand edit, a future code path) cannot leave the
    // per-session debt saying less than the snapshot-level flag does.
    tx.execute(
        "UPDATE session_rows SET unresolved = 1
         WHERE snapshot_id IN (SELECT id FROM snapshots WHERE unresolved = 1)",
        [],
    )?;

    // Cosmetic bookkeeping, deliberately best-effort: per-object rows left
    // `pending` under an attempt we just failed describe work that never
    // finished. This must never be able to abort the reachability repair
    // above, which is the whole point of the function.
    if let Err(e) = tx.execute(
        "UPDATE restore_objects
         SET state = 'failed',
             detail = COALESCE(detail, 'restore interrupted; reclaimed on next run')
         WHERE state = 'pending'",
        [],
    ) {
        eprintln!("osm: reclaim: could not clear pending restore objects: {e}");
    }

    tx.commit()?;
    Ok(snapshots)
}

/// Deletes old snapshots, keeping the newest `keep` — with three exemptions,
/// all of which exist to protect one invariant: *the newest pre-reboot
/// snapshot survives until it has been restored.*
///
/// 1. **The row a restore would currently select.** Retention by recency
///    alone is a data-loss bug, not just a tuning choice: one previous-boot
///    snapshot plus `keep` newer current-boot snapshots is enough to delete
///    the user's entire pre-reboot layout, and the daemon alone reaches
///    `keep` snapshots within the hour. So the single row
///    [`select_restore_source`] would pick is exempt outright.
///
///    This is the *exemption* strategy rather than per-`boot_id` partitioned
///    retention: it is one extra `NOT IN`-style clause that reuses the
///    selector's own query verbatim (so the two can never disagree about
///    which row is protected), and it bounds the database at `keep + 1` rows
///    instead of `keep` rows *per boot generation* — with a window-function
///    partition, a machine that reboots often but never restores would grow
///    without limit. The cost is that only the single newest previous-boot
///    snapshot is protected; older un-restored generations still age out,
///    which is exactly the intended retention behaviour.
///
/// 2. **Snapshots with a `running` restore attempt.**
///
/// 3. **Snapshots marked `unresolved`.** A restore of one of these did not
///    put everything back, so no newer snapshot is a superset of it yet.
///    The marker is cleared by the first capture that carries the missing
///    sessions forward, after which ordinary retention applies again.
///
/// 4. **Snapshots in `restore_in_progress`.** These used to be dead weight
///    worth reclaiming disk from; now that [`reclaim_orphaned_restores`]
///    hands them back to `complete`, they are live restorable state and
///    deleting one destroys exactly what an interrupted restore was about to
///    rebuild.
///
/// 5. **The newest snapshot that still knows where the windows were.** The
///    same rule as (1), one field over. A capture whose placement could not
///    be read now records the tmux topology anyway and marks its placement
///    *unknown* (see [`crate::desktop::Placements`]) — which is a strict
///    improvement, except that a run of them is still a run of snapshots, and
///    under recency alone twenty of them evict the last row that records the
///    user's workspace and monitor layout. Nothing else on the machine holds
///    it: the compositor is asked afresh every capture and remembers nothing,
///    so once that row is gone the layout is gone. The maintainer's original
///    mapping was lost exactly this way, by a different route.
///
///    A `known` snapshot with no windows in it is not this row — it has
///    nothing to protect — and neither is an `unknown` one, which is the
///    whole point. Like (1) it is a *single* row rather than "every snapshot
///    with placement": an older placed snapshot has been superseded by a
///    newer one, and keeping them all would let a machine whose compositor
///    stays broken grow without limit. The bound goes from `keep + 1` rows to
///    `keep + 2`.
///
///    It is deliberately not conditioned on "and nothing newer answered".
///    That refinement would release the row once a later capture reported a
///    genuinely empty desktop, and it would cost a correlated subquery to
///    express. One extra row on a machine that has really stopped having
///    terminal windows is the cheaper mistake, and it is the one that errs
///    towards keeping the user's data.
///
/// 6. **The newest placed snapshot of a *previous* boot**, which is not
///    always the same row as (5). (1) protects the snapshot a restore would
///    rebuild from; when that snapshot's placement is unknown, the restore
///    takes its layout from an earlier snapshot of the same boot instead —
///    and (5) does not protect that one, because the new boot's first capture
///    answers its compositor perfectly well and takes the exemption over. The
///    previous boot's layout is then an ordinary old row, and a machine at
///    the retention limit deletes it in the seconds between login and the
///    boot restore.
///
///    Same single-row shape as (1), same reasoning, and it moves the bound to
///    `keep + 3`. Together (1) and (6) are the whole guarantee: the restore
///    can rebuild the topology *and* put the windows back.
///
/// `current_boot` must be the caller's real boot id — passing something else
/// silently changes which row is protected.
pub fn prune(conn: &Connection, keep: usize, current_boot: &str) -> Result<usize> {
    let deleted = conn.execute(
        "DELETE FROM snapshots
         WHERE id NOT IN (
           SELECT id FROM snapshots ORDER BY taken_at DESC, id DESC LIMIT ?1
         )
         AND id NOT IN (
           SELECT snapshot_id FROM restore_attempts
           WHERE state IN ('running')
         )
         AND state <> 'restore_in_progress'
         AND unresolved = 0
         -- `IS NOT` (not `<>`): when there is no restorable snapshot the
         -- subquery yields NULL, and `id <> NULL` is NULL, which would make
         -- the whole WHERE never match and prune nothing at all.
         AND id IS NOT (
           SELECT id FROM snapshots
           WHERE state = 'complete' AND boot_id <> ?2
           ORDER BY taken_at DESC, id DESC
           LIMIT 1
         )
         -- The last snapshot that knows where the windows were. `IS NOT` for
         -- the same reason as above: on a database that has never recorded a
         -- placement the subquery is NULL, and `<>` would make the whole
         -- WHERE never match.
         --
         -- Whether it holds any `terminal_windows` rows is deliberately not
         -- asked. This clause exists to keep alive the row
         -- `desktop::placement_for_restore` will read, and that function stops
         -- at the newest earlier `known` snapshot, empty or not: a compositor
         -- that was asked and answered that there are none has said where
         -- the windows are. Requiring rows here pinned the layout such an
         -- answer had superseded, let the answer itself age out, and so
         -- handed the next carry-forward a desktop the user had cleared.
         AND id IS NOT (
           SELECT s.id FROM snapshots s
           WHERE s.state <> 'building'
             AND s.placement_state = 'known'
           ORDER BY s.taken_at DESC, s.id DESC
           LIMIT 1
         )
         -- And the same row restricted to a previous boot: the layout the
         -- snapshot a restore would select is about to borrow.
         AND id IS NOT (
           SELECT s.id FROM snapshots s
           WHERE s.state <> 'building'
             AND s.boot_id <> ?2
             AND s.placement_state = 'known'
           ORDER BY s.taken_at DESC, s.id DESC
           LIMIT 1
         )",
        rusqlite::params![keep as i64, current_boot],
    )?;
    Ok(deleted)
}
