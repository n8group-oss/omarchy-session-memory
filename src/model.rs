use anyhow::Result;
use rusqlite::Connection;

/// (row_id, tmux_window_id, link idx, name, layout, active_pane_id, zoomed)
type WindowRow = (i64, String, u32, String, String, Option<String>, i64);

#[derive(Debug, Clone)]
pub struct PanePlan {
    pub tmux_pane_id: String,
    pub idx: u32,
    pub cwd: String,
    pub restore_policy: String,
}

/// One window as it appears **in one session**. The same window (same
/// `tmux_window_id`) may appear in several [`SessionPlan`]s when it is
/// linked; `idx` is then per-session while everything else is shared.
#[derive(Debug, Clone)]
pub struct WindowPlan {
    pub tmux_window_id: String,
    pub idx: u32,
    pub name: String,
    pub layout: String,
    pub active_pane_id: Option<String>,
    pub zoomed: bool,
    pub panes: Vec<PanePlan>,
}

#[derive(Debug, Clone)]
pub struct SessionPlan {
    pub name: String,
    pub active_window_id: Option<String>,
    pub windows: Vec<WindowPlan>,
}

#[derive(Debug, Clone)]
pub struct SnapshotTree {
    pub snapshot_id: i64,
    pub sessions: Vec<SessionPlan>,
}

/// Every window linked into one session, in that session's own index order.
fn windows_of(conn: &Connection, session_row_id: i64) -> Result<Vec<WindowPlan>> {
    // The same window row can be returned for several sessions (a linked
    // window), each time with that session's own index. `restore` recognises
    // the repeat by `tmux_window_id` and issues `link-window` instead of
    // building a second copy.
    let mut w_stmt = conn.prepare(
        "SELECT w.row_id, w.tmux_window_id, l.idx, w.name, w.layout,
                w.active_pane_id, w.zoomed
         FROM session_window_links l
         JOIN window_rows w ON w.row_id = l.window_row_id
         WHERE l.session_row_id = ?1
         ORDER BY l.idx",
    )?;
    let w_rows: Vec<WindowRow> = w_stmt
        .query_map([session_row_id], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
            ))
        })?
        .collect::<Result<_, _>>()?;

    let mut windows = Vec::new();
    for (window_row_id, tmux_window_id, idx, wname, layout, active_pane_id, zoomed) in w_rows {
        let mut p_stmt = conn.prepare(
            "SELECT tmux_pane_id, idx, cwd, restore_policy
             FROM pane_rows WHERE window_row_id = ?1 ORDER BY idx",
        )?;
        let panes: Vec<PanePlan> = p_stmt
            .query_map([window_row_id], |r| {
                Ok(PanePlan {
                    tmux_pane_id: r.get(0)?,
                    idx: r.get(1)?,
                    cwd: r.get(2)?,
                    restore_policy: r.get(3)?,
                })
            })?
            .collect::<Result<_, _>>()?;

        windows.push(WindowPlan {
            tmux_window_id,
            idx,
            name: wname,
            layout,
            active_pane_id,
            zoomed: zoomed == 1,
            panes,
        });
    }
    Ok(windows)
}

/// One session of a snapshot, addressed by its `session_rows.row_id`.
///
/// Split out of [`load`] because a capture needs exactly one session's plan:
/// deciding whether an outstanding session has since been recovered means
/// comparing *that* session against the live server, and loading the whole
/// tree to reach it would also make the lookup go by name — which is precisely
/// the shortcut that let a truncated live session pass as the captured one.
pub fn load_session(conn: &Connection, session_row_id: i64) -> Result<SessionPlan> {
    let (name, active_window_id): (String, Option<String>) = conn.query_row(
        "SELECT name, active_window_id FROM session_rows WHERE row_id = ?1",
        [session_row_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok(SessionPlan {
        name,
        active_window_id,
        windows: windows_of(conn, session_row_id)?,
    })
}

pub fn load(conn: &Connection, snapshot_id: i64) -> Result<SnapshotTree> {
    let mut s_stmt =
        conn.prepare("SELECT row_id FROM session_rows WHERE snapshot_id = ?1 ORDER BY row_id")?;
    let s_rows: Vec<i64> = s_stmt
        .query_map([snapshot_id], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    drop(s_stmt);

    let mut sessions = Vec::new();
    for session_row_id in s_rows {
        sessions.push(load_session(conn, session_row_id)?);
    }

    Ok(SnapshotTree {
        snapshot_id,
        sessions,
    })
}
