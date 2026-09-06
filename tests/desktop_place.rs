//! Resolving a monitor and placing a window on its workspace.

use osm::desktop::{self, Placement};
use osm::hypr::{Client, Monitor};

fn mon(id: i64, name: &str, desc: &str, focused: bool) -> Monitor {
    Monitor {
        id,
        name: name.into(),
        description: desc.into(),
        make: String::new(),
        model: String::new(),
        serial: String::new(),
        x: 0,
        y: 0,
        width: 1000,
        height: 500,
        scale: 1.0,
        transform: 0,
        focused,
    }
}

fn place(connector: &str, desc: Option<&str>) -> Placement {
    Placement {
        session: "dev".into(),
        address: "0x1".into(),
        class: "c".into(),
        terminal_kind: "ghostty".into(),
        workspace_kind: "numbered".into(),
        workspace_ref: "3".into(),
        monitor_connector: connector.into(),
        monitor_desc: desc.map(str::to_string),
        monitor_scale: Some(1.0),
        monitor_transform: Some(0),
        floating: false,
        rel: None,
    }
}

fn win(monitor_index: i64, at: (i32, i32), size: (i32, i32)) -> Client {
    Client {
        address: "0x1".into(),
        pid: 1,
        class: "com.mitchellh.ghostty".into(),
        title: "t".into(),
        workspace_id: 3,
        workspace_name: "3".into(),
        monitor_index,
        at,
        size,
        floating: true,
    }
}

/// A window that has just mapped: at `address`, on workspace `ws` of the
/// monitor with index `monitor_index`, and not yet where it belongs.
///
/// `place_lua` takes the window rather than its address because the two
/// dispatches it can emit contradict each other — sending a window to a
/// monitor puts it on that monitor's *active* workspace — so which of them
/// is right depends on where the window already is.
fn mapped(address: &str, ws: &str, monitor_index: i64) -> Client {
    Client {
        address: address.into(),
        pid: 1,
        class: "com.mitchellh.ghostty".into(),
        title: "t".into(),
        workspace_id: ws.parse().unwrap_or(0),
        workspace_name: ws.into(),
        monitor_index,
        at: (0, 0),
        size: (100, 100),
        floating: false,
    }
}

#[test]
fn geometry_is_recorded_as_a_fraction_of_its_monitor() {
    let g =
        desktop::relative_geometry(&win(0, (100, 50), (500, 250)), &[mon(0, "DP-1", "d", true)])
            .unwrap();
    assert!((g.0 - 0.1).abs() < 1e-6, "x {g:?}");
    assert!((g.1 - 0.1).abs() < 1e-6, "y {g:?}");
    assert!((g.2 - 0.5).abs() < 1e-6, "w {g:?}");
    assert!((g.3 - 0.5).abs() < 1e-6, "h {g:?}");
}

#[test]
fn geometry_on_an_unknown_monitor_is_absent_not_zero() {
    // Zeroes would restore the window into the corner at no size. Absent
    // means "put it on the workspace and let the compositor decide".
    assert!(
        desktop::relative_geometry(&win(9, (0, 0), (10, 10)), &[mon(0, "DP-1", "d", true)])
            .is_none()
    );
}

#[test]
fn a_degenerate_monitor_yields_no_geometry_rather_than_dividing_by_zero() {
    let mut m = mon(0, "DP-1", "d", true);
    m.width = 0;
    assert!(desktop::relative_geometry(&win(0, (0, 0), (10, 10)), &[m]).is_none());
}

#[test]
fn workspace_kind_distinguishes_special_from_numbered_and_named() {
    assert_eq!(
        desktop::workspace_kind_of(3, "3"),
        ("numbered", "3".to_string())
    );
    assert_eq!(
        desktop::workspace_kind_of(-99, "special:magic"),
        ("special", "special:magic".to_string())
    );
    assert_eq!(
        desktop::workspace_kind_of(7, "code"),
        ("named", "code".to_string())
    );
}

#[test]
fn a_monitor_is_found_by_description_even_when_its_connector_changed() {
    // Replugging a cable renames DP-1 to DP-2. The panel is the same panel,
    // and its description is what says so.
    let ms = vec![
        mon(0, "DP-2", "Dell U2720Q ABC123", false),
        mon(1, "eDP-1", "built-in", true),
    ];
    let m = desktop::resolve_monitor(&place("DP-1", Some("Dell U2720Q ABC123")), &ms).unwrap();
    assert_eq!(m.name, "DP-2");
}

#[test]
fn it_falls_back_to_the_connector_then_to_the_focused_monitor() {
    let ms = vec![
        mon(0, "DP-1", "something else", false),
        mon(1, "eDP-1", "built-in", true),
    ];
    assert_eq!(
        desktop::resolve_monitor(&place("DP-1", None), &ms)
            .unwrap()
            .name,
        "DP-1"
    );
    // Neither description nor connector matches: fall back, never nowhere.
    assert_eq!(
        desktop::resolve_monitor(&place("HDMI-9", Some("gone")), &ms)
            .unwrap()
            .name,
        "eDP-1"
    );
}

#[test]
fn geometry_is_clamped_inside_the_monitor() {
    let m = mon(0, "DP-1", "d", true);
    let (x, y, w, h) = desktop::clamp_to_usable((0.9, 0.9, 0.5, 0.5), &m).unwrap();
    assert!(x + w <= m.x + m.width, "x{x} w{w} exceeds {}", m.width);
    assert!(y + h <= m.y + m.height, "y{y} h{h} exceeds {}", m.height);
    assert!(
        w > 0 && h > 0,
        "a clamped window must still be visible: {w}x{h}"
    );
}

#[test]
fn a_tiled_window_is_moved_to_its_workspace_and_not_resized() {
    let lua = desktop::place_lua(
        &mapped("0xA", "2", 0),
        &place("DP-1", None),
        &[mon(0, "DP-1", "d", true)],
    );
    let all = lua.join(" ");
    assert!(all.contains("workspace='3'"), "{lua:?}");
    assert!(
        !all.contains("resize"),
        "a tiled window must not be resized: {lua:?}"
    );
    assert!(!all.contains("float"), "{lua:?}");
}

#[test]
fn a_floating_window_gets_its_geometry_after_being_moved() {
    let mut p = place("DP-1", None);
    p.floating = true;
    p.rel = Some((0.1, 0.1, 0.5, 0.5));
    let lua = desktop::place_lua(&mapped("0xA", "2", 0), &p, &[mon(0, "DP-1", "d", true)]);
    let move_i = lua.iter().position(|s| s.contains("workspace=")).unwrap();
    let size_i = lua.iter().position(|s| s.contains("resize")).unwrap();
    assert!(
        move_i < size_i,
        "geometry must be applied after the move: {lua:?}"
    );
    assert!(lua.iter().any(|s| s.contains("float")), "{lua:?}");
}

#[test]
fn a_special_workspace_is_addressed_as_special_not_as_a_number() {
    let mut p = place("DP-1", None);
    p.workspace_kind = "special".into();
    p.workspace_ref = "special:magic".into();
    let lua =
        desktop::place_lua(&mapped("0xA", "2", 0), &p, &[mon(0, "DP-1", "d", true)]).join(" ");
    assert!(lua.contains("special:magic"), "{lua}");
}

#[test]
fn every_dispatch_addresses_the_window_by_address() {
    // Addressing by anything else (title, class, "activewindow") would move
    // whatever happens to be focused, which may be the user's own window.
    let mut p = place("DP-1", None);
    p.floating = true;
    p.rel = Some((0.1, 0.1, 0.5, 0.5));
    for d in desktop::place_lua(&mapped("0xABC", "2", 0), &p, &[mon(0, "DP-1", "d", true)]) {
        assert!(d.contains("window='address:0xABC'"), "{d}");
    }
}

// ---------------------------------------------------------------------------
// Logical vs physical coordinates.
//
// `hyprctl monitors` reports the panel's *pixel* mode and its `scale`
// separately, while a client's `at`/`size` and a monitor's `x`/`y` are
// logical. Dividing one by the other is not a rounding error: it is wrong by
// exactly the scale factor, invisibly, and only shows up when the scale
// changes between capture and restore.
// ---------------------------------------------------------------------------

/// A monitor with an explicit pixel size, scale, transform and origin.
fn panel(w: i32, h: i32, scale: f32, transform: i32, x: i32, y: i32) -> Monitor {
    let mut m = mon(0, "DP-1", "d", true);
    m.width = w;
    m.height = h;
    m.scale = scale;
    m.transform = transform;
    m.x = x;
    m.y = y;
    m
}

#[test]
fn a_scaled_monitor_measures_windows_in_logical_pixels() {
    // A 3840x2160 panel at scale 2 is 1920x1080 to every window on it. A
    // 960x540 window therefore fills a quarter of it, not a sixteenth.
    let m = panel(3840, 2160, 2.0, 0, 0, 0);
    let g = desktop::relative_geometry(&win(0, (0, 0), (960, 540)), &[m]).unwrap();
    assert!((g.2 - 0.5).abs() < 1e-6, "width fraction {g:?}");
    assert!((g.3 - 0.5).abs() < 1e-6, "height fraction {g:?}");
}

#[test]
fn a_window_survives_a_change_of_scale_at_its_intended_size() {
    // Captured on a 3840x2160 panel at scale 2 — 1920x1080 logical — and
    // restored after the user set scale 1 on a 1920x1080 panel. The window
    // occupied half the logical width before and must occupy half of it
    // after. Measuring the capture in pixels made it a quarter.
    let captured = panel(3840, 2160, 2.0, 0, 0, 0);
    let rel = desktop::relative_geometry(&win(0, (0, 0), (960, 540)), &[captured]).unwrap();

    let restored = panel(1920, 1080, 1.0, 0, 0, 0);
    let (_, _, w, h) = desktop::clamp_to_usable(rel, &restored).unwrap();
    assert_eq!((w, h), (960, 540), "the window changed size with the scale");
}

#[test]
fn a_rotated_monitor_swaps_its_axes() {
    // transform 1 is 90 degrees: a 3840x2160 panel presents a 2160x3840 area.
    // Measuring against the unrotated pixel size put a portrait window at
    // more than 100% of its monitor's height.
    let m = panel(3840, 2160, 1.0, 1, 0, 0);
    let g = desktop::relative_geometry(&win(0, (0, 0), (1080, 3840)), std::slice::from_ref(&m))
        .unwrap();
    assert!((g.2 - 0.5).abs() < 1e-6, "width fraction {g:?}");
    assert!((g.3 - 1.0).abs() < 1e-6, "height fraction {g:?}");

    let (_, _, w, h) = desktop::clamp_to_usable(g, &m).unwrap();
    assert_eq!((w, h), (1080, 3840), "clamped against landscape bounds");
}

#[test]
fn a_monitor_left_of_the_origin_keeps_its_offset() {
    // A second display placed to the left has a negative x. Geometry is
    // relative to the monitor, so the same window is at the same fraction of
    // it wherever the monitor is, and comes back at the monitor's own origin.
    let m = panel(1920, 1080, 1.0, 0, -1920, -200);
    let g = desktop::relative_geometry(
        &win(0, (-1920 + 192, -200 + 108), (960, 540)),
        std::slice::from_ref(&m),
    )
    .unwrap();
    assert!((g.0 - 0.1).abs() < 1e-6, "x fraction {g:?}");
    assert!((g.1 - 0.1).abs() < 1e-6, "y fraction {g:?}");

    let (x, y, _, _) = desktop::clamp_to_usable(g, &m).unwrap();
    assert_eq!((x, y), (-1920 + 192, -200 + 108));
}

#[test]
fn a_monitor_with_no_usable_scale_yields_no_geometry() {
    // Substituting 1.0 for a scale that makes no sense produces geometry that
    // is wrong by exactly the factor nobody can see.
    let m = panel(1920, 1080, 0.0, 0, 0, 0);
    assert!(
        desktop::relative_geometry(&win(0, (0, 0), (10, 10)), std::slice::from_ref(&m)).is_none()
    );
    assert!(desktop::clamp_to_usable((0.1, 0.1, 0.5, 0.5), &m).is_none());
}

// ---------------------------------------------------------------------------
// The monitor dispatch, and the Lua the dispatches are written in.
// ---------------------------------------------------------------------------

#[test]
fn a_window_on_the_wrong_monitor_is_sent_to_it_before_its_workspace_move() {
    // The window mapped on `eDP-1` and belongs on workspace 3 of `DP-1`.
    // Both dispatches are needed, and the order is the whole of it: sending a
    // window to a monitor puts it on that monitor's *active* workspace, so
    // the monitor move has to come first and the workspace move has to be the
    // last word. Emitted the other way round — which is how this shipped —
    // the second call threw away the first, and every restored terminal came
    // back on whatever workspace was active.
    let ms = vec![
        mon(0, "DP-1", "d", false),
        mon(1, "eDP-1", "built-in", true),
    ];
    let lua = desktop::place_lua(&mapped("0xA", "2", 1), &place("DP-1", None), &ms);
    let monitor_i = lua
        .iter()
        .position(|d| d.contains("monitor='DP-1'"))
        .unwrap_or_else(|| panic!("the window is never sent to its monitor: {lua:?}"));
    let workspace_i = lua
        .iter()
        .position(|d| d.contains("workspace='3'"))
        .unwrap_or_else(|| panic!("the window is never sent to its workspace: {lua:?}"));
    assert!(
        monitor_i < workspace_i,
        "the monitor move comes after the workspace move and undoes it: {lua:?}"
    );
}

#[test]
fn a_window_already_on_its_monitor_is_not_sent_to_it_again() {
    // The dispatch that can only do harm. `DP-1` is where the window already
    // is, so asking for it again changes no monitor — and costs the window
    // the workspace it was just moved to, because that is what a move to a
    // monitor does. Sending it "just in case" is what broke placement on the
    // maintainer's single-monitor desktop.
    let lua = desktop::place_lua(
        &mapped("0xA", "2", 0),
        &place("DP-1", None),
        &[mon(0, "DP-1", "d", true)],
    );
    assert!(
        !lua.iter().any(|d| d.contains("monitor=")),
        "the window was sent to the monitor it is already on: {lua:?}"
    );
    assert!(
        lua.iter().any(|d| d.contains("workspace='3'")),
        "it still has to be moved to its workspace: {lua:?}"
    );
}

#[test]
fn a_window_already_where_it_belongs_is_not_dispatched_at() {
    // Confirmation re-reads the desktop and asks again for whatever is still
    // wrong. When nothing is, there is nothing to ask for — and a loop that
    // kept dispatching anyway would move a settled window on every pass.
    let lua = desktop::place_lua(
        &mapped("0xA", "3", 0),
        &place("DP-1", None),
        &[mon(0, "DP-1", "d", true)],
    );
    assert!(lua.is_empty(), "{lua:?}");
}

#[test]
fn a_window_only_on_the_wrong_monitor_is_left_on_its_workspace() {
    // It is on workspace 3, which is what it was captured on, and workspace 3
    // currently lives on `eDP-1` rather than `DP-1`. The one dispatch that
    // would put it on `DP-1` would take it off workspace 3 to do it — so
    // nothing is emitted, and `placement_gap` reports the shortfall instead.
    // Trading the workspace for the monitor and calling the result `placed`
    // is the failure this file exists to prevent.
    let ms = vec![
        mon(0, "DP-1", "d", false),
        mon(1, "eDP-1", "built-in", true),
    ];
    let w = mapped("0xA", "3", 1);
    assert!(
        desktop::place_lua(&w, &place("DP-1", None), &ms).is_empty(),
        "the window was taken off its workspace to satisfy the monitor"
    );
    let gap = desktop::placement_gap(&w, &place("DP-1", None), &ms)
        .expect("a window on the wrong monitor is not placed");
    assert!(gap.contains("DP-1"), "{gap}");
    assert!(!gap.contains("workspace"), "the workspace is right: {gap}");
}

#[test]
fn the_gap_names_the_workspace_the_window_is_actually_on() {
    // What `misplaced` reports to whoever reads the restore's JSON. "It did
    // not work" is not actionable; "it is on 2, not 9" is.
    let ms = vec![mon(0, "DP-1", "d", true)];
    let gap = desktop::placement_gap(&mapped("0xA", "2", 0), &place("DP-1", None), &ms).unwrap();
    assert!(gap.contains("\"2\"") && gap.contains("\"3\""), "{gap}");
    assert!(
        desktop::placement_gap(&mapped("0xA", "3", 0), &place("DP-1", None), &ms).is_none(),
        "a window that is exactly where it belongs has no gap"
    );
}

#[test]
fn a_compositor_that_lists_no_monitor_cannot_confirm_a_placement() {
    // This test used to assert the opposite — that a compositor listing no
    // monitor is *owed* no monitor, so a window on its recorded workspace is
    // `placed`. That is the unsafe reading, and it is reachable: readiness
    // proves a monitor exists, and a later `hyprctl -j monitors` that comes
    // back as a valid, empty array then makes `resolve_monitor` return
    // nothing. The window is confirmed on its workspace alone, the claim
    // carries no connector, the publication skips the monitor check it cannot
    // make — and a restore that put the user's terminal on the wrong panel
    // reports `succeeded` and retires the snapshot that knew the right one.
    //
    // An empty monitor list is not "no monitor is owed". It is "this
    // compositor cannot be asked where the window is", which is a shortfall,
    // not a placement. `spawn_and_place` refuses to get this far at all
    // (see `tests/desktop_confirm.rs`); the gap is reported here as well, so
    // no caller can confirm a placement against a desktop it cannot read.
    let w = mapped("0xA", "3", -1);
    let gap = desktop::placement_gap(&w, &place("DP-1", None), &[])
        .expect("a monitor that cannot be resolved is not a confirmed placement");
    assert!(
        gap.contains("DP-1"),
        "the gap must name the monitor that could not be confirmed: {gap}"
    );
    // Still nothing to dispatch: there is no monitor to send it to, and
    // inventing one would move the user's window somewhere nobody recorded.
    assert!(desktop::place_lua(&w, &place("DP-1", None), &[]).is_empty());
}

#[test]
fn a_workspace_name_with_an_apostrophe_produces_a_valid_literal() {
    // `Bob's` is an ordinary named workspace. Pasted between quotes untouched
    // it ends the Lua string early and the dispatch is rejected outright.
    let mut p = place("DP-1", None);
    p.workspace_kind = "named".into();
    p.workspace_ref = "Bob's".into();
    let lua = desktop::place_lua(&mapped("0xA", "2", 0), &p, &[mon(0, "DP-1", "d", true)]);
    let ws = lua
        .iter()
        .find(|d| d.contains("workspace="))
        .expect("a workspace dispatch");
    assert!(ws.contains(r"workspace='Bob\'s'"), "{ws}");
    assert_eq!(
        ws.matches("workspace=").count(),
        1,
        "the literal must not be closed early: {ws}"
    );
}

/// The decoded content of the `workspace='…'` literal in `dispatch`, and
/// everything after its closing quote.
///
/// A Lua lexer, in miniature: it walks the literal honouring `\\`, `\'` and
/// the numeric `\ddd` escapes, so it finds the quote that *actually* ends the
/// string rather than the first one it sees. Asserting on substrings cannot
/// tell an escaped apostrophe from one that closed the literal, which is the
/// whole question.
fn workspace_literal(dispatch: &str) -> (String, String) {
    let start = dispatch.find("workspace='").expect("a workspace literal") + "workspace='".len();
    let mut decoded = String::new();
    let rest: Vec<char> = dispatch[start..].chars().collect();
    let mut i = 0;
    while i < rest.len() {
        match rest[i] {
            '\'' => {
                let after: String = rest[i + 1..].iter().collect();
                return (decoded, after);
            }
            '\\' => {
                let next = rest[i + 1];
                if next.is_ascii_digit() {
                    let digits: String = rest[i + 1..i + 4].iter().collect();
                    decoded.push(char::from_u32(digits.parse().unwrap()).unwrap());
                    i += 4;
                } else {
                    decoded.push(next);
                    i += 2;
                }
            }
            c => {
                decoded.push(c);
                i += 1;
            }
        }
    }
    panic!("the literal is never closed: {dispatch}");
}

#[test]
fn a_workspace_name_cannot_inject_lua() {
    // A name a keybind can legitimately create is still the user's text, and
    // this process dispatches as the user. Closing the literal and appending
    // a call must be impossible, not merely unlikely.
    let name = r"x', y=1}) hl.dsp.window.close({window='address:0xB";
    let mut p = place("DP-1", None);
    p.workspace_kind = "named".into();
    p.workspace_ref = name.into();
    let lua = desktop::place_lua(&mapped("0xA", "2", 0), &p, &[mon(0, "DP-1", "d", true)]);
    let ws = lua.iter().find(|d| d.contains("workspace=")).unwrap();

    let (decoded, after) = workspace_literal(ws);
    assert_eq!(decoded, name, "the name did not survive intact: {ws}");
    assert_eq!(
        after, ", follow=false})",
        "the literal ended early and the rest of the name became code: {ws}"
    );
}

#[test]
fn a_backslash_in_a_name_is_escaped_rather_than_continued() {
    let mut p = place("DP-1", None);
    p.workspace_kind = "named".into();
    p.workspace_ref = r"back\slash".into();
    let lua = desktop::place_lua(&mapped("0xA", "2", 0), &p, &[mon(0, "DP-1", "d", true)]);
    let ws = lua.iter().find(|d| d.contains("workspace=")).unwrap();
    assert!(ws.contains(r"workspace='back\\slash'"), "{ws}");
}

#[test]
fn lua_literals_escape_quotes_backslashes_and_control_characters() {
    assert_eq!(desktop::lua_str("plain"), "'plain'");
    assert_eq!(desktop::lua_str("it's"), r"'it\'s'");
    assert_eq!(desktop::lua_str(r"a\b"), r"'a\\b'");
    // Three digits always: `\9` followed by a literal `5` would otherwise lex
    // as the single escape `\95`.
    assert_eq!(desktop::lua_str("a\tb"), r"'a\009b'");
    assert_eq!(desktop::lua_str("a\n5"), r"'a\0105'");
}
