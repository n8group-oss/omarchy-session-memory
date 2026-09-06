//! Whether the menu's list can be scrolled, and whether it looks like it can.
//!
//! # Why this file exists
//!
//! The maintainer's panel is about 390 px wide and 620 px tall on a 3440 px
//! monitor, and it holds 8 tmux sessions, 20 live conversations and a count of
//! 2448 resumable ones. The content is taller than the panel, so the list ends
//! mid-row at the bottom edge — and his report was that he *cannot scroll*.
//!
//! The `Flickable` had `interactive: contentHeight > height` and a
//! `ScrollBar` whose policy was `AsNeeded`. `AsNeeded` is not "when it is
//! needed": in every Qt Quick Controls style the bar is drawn at zero opacity
//! until the view is `active` — moving, hovered or pressed — so the panel
//! offers *no* mark that there is more below. A list that is cut off with no
//! affordance is one a user has no reason to try to scroll, and no way to know
//! how far down they are if they do.
//!
//! # What this actually verifies
//!
//! The `Flickable` is **lifted verbatim out of `Menu.qml`** — its own
//! properties, its scrollbar and any handler declared on it — instantiated
//! under `qmltestrunner`, and then driven with real wheel events through
//! `TestCase.mouseWheel`. Its children are replaced by a stand-in of a chosen
//! height, so the same block can be measured both overflowing and fitting.
//!
//! It is lifted by `id: menuFlick`, which is in the file before and after the
//! fix, so these measurements run against the broken version too. That is the
//! only way to know they can fail.
//!
//! # What it cannot verify
//!
//! * **The real content.** `qs.Ui` imports `Quickshell`, whose plugin refuses
//!   to load outside the `quickshell` binary, so the filter box and the row
//!   buttons cannot be the real ones. They are stood in for by what those
//!   components are made of, read from their source: `qs/Ui/TextField.qml` is
//!   a `QtQuick.Controls` `TextField`, and `qs/Ui/Button.qml` is a rectangle
//!   with a hover-enabled `MouseArea` and a `HoverHandler`. That is enough to
//!   answer the question this was written for — whether anything between the
//!   cursor and the `Flickable` swallows the wheel — but the numbers are not
//!   the shell's numbers.
//!
//! * **The drawn scrollbar.** Opacity is read from the control's
//!   `contentItem`, which belongs to whichever Qt Quick Controls style is
//!   loaded; here that is the default one. `policy` is asserted alongside it
//!   precisely because that half does not depend on the style.
//!
//! * **Timestamps.** `mouseWheel` stamps every synthetic event with the same
//!   time, and `Flickable`'s built-in wheel handling derives a velocity from
//!   the gap between events — so a burst of notches sent back to back is not
//!   what a real mouse produces. The burst is measured anyway, because what is
//!   asserted of it is that each notch moves the list by the same fixed
//!   amount, which is a property of scrolling by a step rather than by flick
//!   physics, and is true of a real mouse as well.
//!
//! It needs `qmltestrunner`, which a machine running the Omarchy shell has and
//! this project's CI containers do not. Without it these tests say what they
//! skipped; the source-level rules in `tests/manifest.rs` run everywhere.

mod common;

use serde::Deserialize;
use std::path::{Path, PathBuf};

/// `ScrollBar.AlwaysOn`, as Qt numbers it.
const ALWAYS_ON: i32 = 2;

fn qmltest_runtime() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("OSM_QMLTESTRUNNER") {
        let p = PathBuf::from(explicit);
        return p.exists().then_some(p);
    }
    let fixed = PathBuf::from("/usr/lib/qt6/bin/qmltestrunner");
    if fixed.exists() {
        return Some(fixed);
    }
    for dir in std::env::var("PATH").unwrap_or_default().split(':') {
        let p = Path::new(dir).join("qmltestrunner");
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

fn menu_source() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Menu.qml");
    std::fs::read_to_string(path).expect("Menu.qml exists")
}

fn balanced(src: &str, from: usize) -> String {
    let open = src[from..].find('{').expect("an opening brace") + from;
    let bytes = src.as_bytes();
    let mut depth = 0usize;
    for i in open..bytes.len() {
        match bytes[i] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return src[from..=i].to_string();
                }
            }
            _ => {}
        }
    }
    panic!("unbalanced braces from {from}");
}

/// The declaration that contains `marker`, verbatim, with its type name.
fn enclosing_block(src: &str, marker: &str) -> String {
    let at = src
        .find(marker)
        .unwrap_or_else(|| panic!("Menu.qml contains `{marker}`"));
    let open = src[..at]
        .rfind('{')
        .expect("the marker is inside some object");
    let name_end = src[..open].trim_end().len();
    let name_start = src[..name_end]
        .rfind(|c: char| !(c.is_alphanumeric() || c == '_' || c == '.'))
        .map(|i| i + 1)
        .unwrap_or(0);
    balanced(src, name_start)
}

/// The menu's `Flickable`, with the list it holds swapped for a stand-in
/// `implicitHeight` px tall.
///
/// Everything the `Flickable` itself declares — how it decides it is
/// interactive, its bounds behaviour, its scrollbar, any handler on it — comes
/// through untouched. Only the children are replaced, because the real ones
/// need `Quickshell`.
fn flickable_with_content(src: &str) -> String {
    let block = enclosing_block(src, "id: menuFlick");
    assert!(
        block.starts_with("Flickable"),
        "the menu's list is no longer a Flickable: {}",
        &block[..block.len().min(60)]
    );
    let column_at = block
        .find("id: column")
        .expect("the Flickable holds a Column called `column`");
    let start = block[..column_at]
        .rfind("Column")
        .expect("that id belongs to a Column");
    let column = balanced(&block, start);
    block.replace(&column, STAND_IN)
}

/// What replaces the list: an item of the same name and the same one property
/// the `Flickable` reads off it, holding the two things the brief suspects of
/// eating wheel events.
const STAND_IN: &str = r#"Item {
          id: column
          width: menuFlick.width
          implicitHeight: root.contentTall
          height: implicitHeight

          Column {
            width: parent.width
            TextField { id: filterStandIn; width: parent.width; height: 40 }
            Rectangle {
              id: buttonStandIn
              width: parent.width
              height: 40
              color: "grey"
              MouseArea {
                anchors.fill: parent
                hoverEnabled: true
                cursorShape: Qt.PointingHandCursor
                acceptedButtons: Qt.LeftButton | Qt.RightButton
              }
              HoverHandler { }
            }
          }
        }"#;

/// Everything one run measures.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Scroll {
    /// Whether the view will respond to a drag at all.
    interactive: bool,
    /// The scrollbar's `policy`, as Qt numbers it.
    policy: i32,
    /// How opaque the drawn bar is *before anything has touched the panel* —
    /// which is the whole question: an affordance that appears once you are
    /// already scrolling is not an affordance.
    bar_opacity: f64,
    max_y: f64,
    /// Where one notch of the wheel leaves the list, and where it still is
    /// 300 ms later. Two different numbers mean it is coasting.
    one_notch: f64,
    one_notch_settled: f64,
    /// Three notches from the top.
    three_notches: f64,
    /// One notch delivered with the cursor over the filter box, and over a
    /// button.
    over_text_field: f64,
    over_button: f64,
    /// One notch down from ten pixels short of the end, and one notch up from
    /// five pixels short of the start.
    bottom_clamped: f64,
    top_clamped: f64,
}

/// The panel's real size on the maintainer's machine, near enough: about
/// 390 px of content across, and a card the screen keeps under ~620.
const PANEL_W: u32 = 390;
const PANEL_H: u32 = 300;

fn measure(runner: &Path, content_height: u32) -> Scroll {
    let src = menu_source();
    let flickable = flickable_with_content(&src);

    let harness = format!(
        r##"import QtQuick
import QtQuick.Controls
import QtTest
import qs.Commons

Item {{
  id: root
  width: {PANEL_W}
  height: {PANEL_H}
  property real contentTall: {content_height}

  {flickable}

  TestCase {{
    name: "menuflick"
    when: windowShown

    function notch(item, x, y, delta) {{
      mouseWheel(item, x, y, 0, delta)
    }}

    function test_scroll() {{
      // Long enough for the style's fade-out to finish: it holds the bar at
      // full opacity for 450ms before animating it away, so a shorter wait
      // reads a bar that is on its way out as one that is on.
      wait(900)
      var bar = menuFlick.ScrollBar.vertical
      var out = {{}}
      // Read before anything has been touched. Every later probe moves the
      // view, and a moving view makes the bar `active`.
      out.interactive = menuFlick.interactive
      out.policy = bar.policy
      out.barOpacity = bar.contentItem ? bar.contentItem.opacity : -1
      out.maxY = Math.max(0, menuFlick.contentHeight - menuFlick.height)

      menuFlick.contentY = 0; wait(250)
      notch(menuFlick, 200, 150, -120); wait(300)
      out.oneNotch = menuFlick.contentY
      wait(300)
      out.oneNotchSettled = menuFlick.contentY

      menuFlick.contentY = 0; wait(300)
      notch(menuFlick, 200, 150, -120)
      notch(menuFlick, 200, 150, -120)
      notch(menuFlick, 200, 150, -120)
      wait(350)
      out.threeNotches = menuFlick.contentY

      menuFlick.contentY = 0; wait(300)
      notch(filterStandIn, 20, 20, -120); wait(300)
      out.overTextField = menuFlick.contentY

      menuFlick.contentY = 0; wait(300)
      notch(buttonStandIn, 20, 20, -120); wait(300)
      out.overButton = menuFlick.contentY

      menuFlick.contentY = Math.max(0, out.maxY - 10); wait(300)
      notch(menuFlick, 200, 150, -120); wait(300)
      out.bottomClamped = menuFlick.contentY

      menuFlick.contentY = Math.min(5, out.maxY); wait(300)
      notch(menuFlick, 200, 150, 120); wait(300)
      out.topClamped = menuFlick.contentY

      console.warn("BEGIN" + JSON.stringify(out) + "END")
    }}
  }}
}}
"##
    );

    let tmp = tempfile::tempdir().unwrap();
    write_stub_commons(tmp.path());
    // qmltestrunner picks test files up by name.
    let file = tmp.path().join("tst_menuflick.qml");
    std::fs::write(&file, harness).unwrap();

    let out = common::run_bounded(
        std::process::Command::new(runner)
            .env("QT_FORCE_STDERR_LOGGING", "1")
            .env("QT_QPA_PLATFORM", "offscreen")
            .arg("-import")
            .arg(tmp.path())
            .arg("-input")
            .arg(&file),
        std::time::Duration::from_secs(120),
    );
    let text =
        String::from_utf8_lossy(&out.stderr).into_owned() + &String::from_utf8_lossy(&out.stdout);
    assert!(
        !text.contains("FAIL!"),
        "the QML harness itself failed: {text}"
    );
    let begin = text
        .find("BEGIN")
        .unwrap_or_else(|| panic!("the QML runtime printed no measurement: {text}"));
    let end = text[begin..]
        .find("END")
        .map(|i| begin + i)
        .expect("a terminated result");
    serde_json::from_str::<Scroll>(&text[begin + 5..end])
        .expect("the harness printed a measurement")
}

/// `Style` with plain numbers behind it; the real `qs.Commons` needs
/// `Quickshell`.
fn write_stub_commons(dir: &Path) {
    let commons = dir.join("qs/Commons");
    std::fs::create_dir_all(&commons).unwrap();
    std::fs::write(
        commons.join("qmldir"),
        "module qs.Commons\nsingleton Style 1.0 Style.qml\n",
    )
    .unwrap();
    std::fs::write(
        commons.join("Style.qml"),
        r#"pragma Singleton
import QtQml
QtObject {
  function space(n) { return n }
  readonly property QtObject font: QtObject {
    readonly property int body: 14
    readonly property int bodySmall: 13
    readonly property int caption: 11
  }
  readonly property QtObject spacing: QtObject {
    readonly property int labelGap: 4
  }
}
"#,
    )
    .unwrap();
}

fn skip() -> bool {
    if qmltest_runtime().is_none() {
        eprintln!(
            "SKIPPED: no qmltestrunner found (looked at $OSM_QMLTESTRUNNER, \
             /usr/lib/qt6/bin/qmltestrunner and PATH). The source-level rules \
             in tests/manifest.rs still ran."
        );
        return true;
    }
    false
}

/// The defect: a list that runs past the bottom of the panel and says nothing.
///
/// `AsNeeded` draws the bar at zero opacity until the view is already moving,
/// so the only way to discover that the panel scrolls is to have guessed it
/// first. The maintainer did not, and reported that he could not scroll.
#[test]
fn an_overflowing_list_shows_its_scrollbar_before_anything_touches_it() {
    if skip() {
        return;
    }
    let runner = qmltest_runtime().unwrap();
    let s = measure(&runner, 2000);

    assert!(
        s.interactive,
        "content {}px tall in a {PANEL_H}px panel and the view is not \
         interactive at all",
        2000
    );
    assert!(
        s.bar_opacity > 0.0,
        "the scrollbar was fully transparent while the list overflowed by \
         {}px, with nothing having been touched: the panel looks truncated \
         rather than scrollable",
        s.max_y
    );
    assert_eq!(
        s.policy, ALWAYS_ON,
        "the scrollbar's policy is {} (AsNeeded is 0), so it is drawn only \
         once the view is already moving — there is nothing on screen to tell \
         the user the list continues below the edge",
        s.policy
    );
}

/// And a panel that fits shows no bar.
///
/// The other half of the rule, and the reason the policy is bound to the
/// overflow rather than pinned to `AlwaysOn`: a scrollbar beside a list with
/// nothing below it is a mark that says there is more when there is not.
#[test]
fn a_list_that_fits_shows_no_scrollbar_at_all() {
    if skip() {
        return;
    }
    let runner = qmltest_runtime().unwrap();
    let s = measure(&runner, 100);

    assert!(
        !s.interactive,
        "a 100px list in a {PANEL_H}px panel was still draggable"
    );
    assert_eq!(
        s.bar_opacity, 0.0,
        "a list with nothing below the fold was given a visible scrollbar \
         (policy {})",
        s.policy
    );
}

/// One notch of the wheel moves the list by one fixed step, and then stops.
///
/// Left to `Flickable`'s own wheel handling the list is thrown rather than
/// stepped: the same notch lands somewhere slightly different each time and
/// keeps coasting after the event, and a run of notches does not add up to a
/// run of steps. A menu is a list to read, not a surface to fling, and a step
/// per notch is also the only wheel behaviour that can be measured at all.
#[test]
fn one_notch_of_the_wheel_moves_the_list_by_one_fixed_step() {
    if skip() {
        return;
    }
    let runner = qmltest_runtime().unwrap();
    let s = measure(&runner, 2000);

    assert!(
        s.one_notch > 0.0,
        "a wheel notch over the list moved it nowhere"
    );
    assert_eq!(
        s.one_notch, s.one_notch_settled,
        "the list was still coasting 300ms after the wheel stopped: it went \
         to {} and then drifted to {}",
        s.one_notch, s.one_notch_settled
    );
    assert_eq!(
        s.three_notches,
        s.one_notch * 3.0,
        "three notches moved the list {}px when one moves {}px: the wheel is \
         driving flick physics rather than scrolling by a step",
        s.three_notches,
        s.one_notch
    );
}

/// Nothing between the cursor and the list eats the wheel.
///
/// The filter box and the row buttons sit inside the `Flickable` and cover
/// most of it, so if either consumed wheel events the panel would refuse to
/// scroll exactly where the user's pointer usually is. Neither does — a
/// `TextField` and a `MouseArea` both let it through — and this is here to
/// keep it that way, not because it was ever found broken.
#[test]
fn neither_the_filter_box_nor_a_button_swallows_the_wheel() {
    if skip() {
        return;
    }
    let runner = qmltest_runtime().unwrap();
    let s = measure(&runner, 2000);

    assert!(
        s.over_text_field > 0.0,
        "a wheel notch with the cursor over the filter box moved the list \
         nowhere"
    );
    assert!(
        s.over_button > 0.0,
        "a wheel notch with the cursor over a button moved the list nowhere"
    );
}

/// The wheel stops at both ends instead of running past them.
#[test]
fn the_wheel_stops_at_the_ends_of_the_list() {
    if skip() {
        return;
    }
    let runner = qmltest_runtime().unwrap();
    let s = measure(&runner, 2000);

    assert_eq!(
        s.bottom_clamped, s.max_y,
        "a notch ten pixels from the end left the list at {} when the end is \
         {}",
        s.bottom_clamped, s.max_y
    );
    assert_eq!(
        s.top_clamped, 0.0,
        "a notch five pixels from the top left the list at {}",
        s.top_clamped
    );
}
