//! Reaching the search box from the keyboard, with real key events.
//!
//! # Why this file exists
//!
//! The panel can be summoned from the keyboard, and `KeyboardPanel` gives the
//! keys to `PanelKeyCatcher`. Nothing ever handed them on to the filter box.
//! Tab switched panels, Escape closed the popup, and the only letters the
//! catcher knew were `r` and `s` — so a keyboard user could not search at all,
//! and *trying* to ran the shortcuts instead: typing `restore` refreshed the
//! status on the `r` and fired a snapshot on the `s`, on a machine with 2448
//! resumable conversations where the filter is the only way to reach any but
//! the newest 25.
//!
//! # What this actually verifies
//!
//! Real focus and real key delivery. The panel's key wiring and the filter
//! box are **lifted verbatim out of `Menu.qml`**, wrapped around the shell's
//! **own, unmodified `PanelKeyCatcher.qml`** — it imports nothing but QtQuick,
//! so unlike the rest of `qs.Ui` it loads outside the `quickshell` binary —
//! and driven by `QtTest`'s synthetic key events through
//! `/usr/lib/qt6/bin/qmltestrunner`. What is asserted is which item holds
//! `activeFocus` and where each keystroke landed, not that some handler
//! exists.
//!
//! # What it cannot verify
//!
//! `TextField` is stood in for: the shell's own imports `qs.Commons`, which
//! imports Quickshell. The stand-in is Qt Quick Controls' `TextField`, which
//! is the real one's base class, so focus and text entry behave as they do in
//! the shell while the colours and the focus ring do not exist here. Nor does
//! this cover `KeyboardPanel` itself — that it focuses `focusTarget` on open
//! is the kit's contract and is asserted at the source level in
//! `tests/manifest.rs`, which also runs where Qt is not installed.

mod common;

use std::path::{Path, PathBuf};

/// `qmltestrunner`, which runs a `TestCase` and reports per function.
fn test_runner() -> Option<PathBuf> {
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

/// The shell's own key catcher, as installed. Nothing is copied into this
/// repository: the component under the panel has to be the one the user's
/// shell actually runs, or the contract being relied on ("`Keys.priority:
/// Keys.BeforeItem`, short-circuited by `blocked`") is only assumed.
fn shell_key_catcher() -> Option<PathBuf> {
    let base =
        std::env::var("OSM_SHELL").unwrap_or_else(|_| "/usr/share/omarchy/shell".to_string());
    let p = PathBuf::from(base).join("Ui/PanelKeyCatcher.qml");
    p.exists().then_some(p)
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

/// The object declaration containing `marker`, verbatim.
fn block_with(src: &str, marker: &str) -> String {
    let at = src
        .find(marker)
        .unwrap_or_else(|| panic!("Menu.qml contains `{marker}`"));
    let open = src[..at]
        .rfind('{')
        .expect("the marker is inside an object");
    let name_end = src[..open].trim_end().len();
    let name_start = src[..name_end]
        .rfind(|c: char| !(c.is_alphanumeric() || c == '_' || c == '.'))
        .map(|i| i + 1)
        .unwrap_or(0);
    balanced(src, name_start)
}

/// The `PanelKeyCatcher`'s own properties and handlers — everything it
/// declares before its first child object.
///
/// The child is the whole menu: a `Flickable` of shell components that cannot
/// be instantiated here. What is under test is the wiring, and the wiring is
/// this header, taken as it is written.
fn key_catcher_header(src: &str) -> String {
    let block = block_with(src, "id: keyCatcher");
    let mut out = Vec::new();
    for (n, line) in block.lines().enumerate() {
        let t = line.trim();
        // A nested object opens a line of its own: `Flickable {`. A handler or
        // a binding that opens a brace always carries a colon first.
        let opens_child = n > 0
            && t.ends_with('{')
            && !t.contains(':')
            && t.starts_with(|c: char| c.is_ascii_uppercase());
        if opens_child {
            break;
        }
        out.push(line);
    }
    out.join("\n")
}

/// Write the harness and the two components it needs into `dir`.
fn write_harness(dir: &Path, catcher: &Path) {
    let src = menu_source();
    std::fs::copy(catcher, dir.join("PanelKeyCatcher.qml"))
        .expect("the shell's catcher is readable");

    // Qt Quick Controls' TextField is what the shell's own derives from; the
    // properties the menu sets on it that the base class does not have are
    // declared here so the lifted block instantiates unchanged.
    std::fs::write(
        dir.join("TextField.qml"),
        r#"import QtQuick
import QtQuick.Controls
TextField {
  property color foreground: "white"
}
"#,
    )
    .unwrap();

    let harness = format!(
        r##"import QtQuick
import QtTest

Item {{
  id: root
  width: 420
  height: 320

  property string filterText: ""
  property int refreshes: 0
  property int snapshots: 0
  property int closes: 0
  property int panelSwitches: 0
  property color foreground: "#eeeeee"
  property string fontFamily: "monospace"
  function refreshAll() {{ root.refreshes++ }}
  function snapshotNow() {{ root.snapshots++ }}
  function close() {{ root.closes++ }}
  function switchPanel(direction) {{ root.panelSwitches++ }}

  {catcher}

    Column {{
      anchors.fill: parent

      {field}
    }}
  }}

  TestCase {{
    name: "menuFilterFocus"
    when: windowShown

    function init() {{
      filterField.text = ""
      root.filterText = ""
      root.refreshes = 0
      root.snapshots = 0
      root.closes = 0
      root.panelSwitches = 0
      keyCatcher.forceActiveFocus()
      verify(!filterField.activeFocus, "each case starts with the panel holding the keys")
    }}

    // The whole finding: from the panel's own focus, there was no key at all
    // that reached the box.
    function test_1_a_documented_key_moves_focus_into_the_box() {{
      keyClick(Qt.Key_Slash)
      verify(filterField.activeFocus,
             "`/` left the focus on the panel: with 2448 conversations behind "
             + "a 25-row cap, a filter that cannot be reached from the keyboard "
             + "is a filter a keyboard user does not have")
      compare(filterField.text, "",
              "the key that opens the box was also typed into it")
    }}

    // And the shortcuts do not fire on the way in. `r` and `s` are two of the
    // seven letters of "restore".
    function test_2_typing_a_query_does_not_run_the_panels_shortcuts() {{
      keyClick(Qt.Key_Slash)
      keyClick("r"); keyClick("e"); keyClick("s"); keyClick("t")
      keyClick("o"); keyClick("r"); keyClick("e")
      compare(filterField.text, "restore", "the query did not reach the box")
      compare(root.filterText, "restore", "the box did not drive the filter")
      compare(root.refreshes, 0, "typing the query refreshed the status")
      compare(root.snapshots, 0, "typing the query fired a snapshot")
      compare(root.closes, 0, "typing the query closed the panel")
    }}

    // Escape stays an exit, in two steps: the query first, then the box.
    function test_3_escape_clears_then_hands_the_keys_back() {{
      keyClick(Qt.Key_Slash)
      keyClick("d"); keyClick("e"); keyClick("v")
      compare(filterField.text, "dev")
      keyClick(Qt.Key_Escape)
      compare(filterField.text, "", "the first Escape did not clear the query")
      verify(filterField.activeFocus, "the first Escape also gave up the box")
      compare(root.closes, 0, "the first Escape closed the panel as well")
      keyClick(Qt.Key_Escape)
      verify(!filterField.activeFocus, "the second Escape did not leave the box")
      verify(keyCatcher.activeFocus, "the keys did not go back to the panel")
      keyClick(Qt.Key_Escape)
      compare(root.closes, 1, "the panel could no longer be closed from the keyboard")
    }}

    // The shortcuts still work when the box does not have the keys — which is
    // the state the panel opens in.
    function test_4_the_shortcuts_still_work_outside_the_box() {{
      keyClick("r")
      keyClick("s")
      compare(root.refreshes, 1, "`r` no longer refreshes")
      compare(root.snapshots, 1, "`s` no longer snapshots")
      compare(filterField.text, "", "a shortcut key was typed into the box")
      keyClick(Qt.Key_Tab)
      compare(root.panelSwitches, 1, "Tab no longer switches panel")
    }}
  }}
}}
"##,
        catcher = key_catcher_header(&src),
        field = block_with(&src, "id: filterField"),
    );
    std::fs::write(dir.join("tst_menufocus.qml"), harness).unwrap();
}

/// The shell modules the filter box's *container* names, stubbed.
///
/// The field on its own needs none of these, which is why the first harness
/// has none — and is also why that harness could not see this defect: what it
/// left out is the very thing that makes the box disappear.
fn write_stub_modules(dir: &Path) {
    let commons = dir.join("qs/Commons");
    let ui = dir.join("qs/Ui");
    std::fs::create_dir_all(&commons).unwrap();
    std::fs::create_dir_all(&ui).unwrap();

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

    std::fs::write(
        ui.join("qmldir"),
        "module qs.Ui\nPanelSectionHeader 1.0 PanelSectionHeader.qml\n",
    )
    .unwrap();
    std::fs::write(
        ui.join("PanelSectionHeader.qml"),
        r#"import QtQuick
Text {
  property color foreground: "white"
  property string fontFamily: "monospace"
  color: foreground
  font.family: fontFamily
}
"#,
    )
    .unwrap();
}

/// The same wiring, with the box's **containing column** instead of the box.
///
/// That column carries `visible: root.engineState === "ready"`, and its
/// visibility is the whole subject here. Qt keeps a child's `activeFocus`
/// when its parent stops being visible, so a `/` pressed while the engine is
/// missing or incompatible — or readiness lost *while typing*, which the
/// status timer can do at any moment — left an item nobody can see holding
/// the keys, with `PanelKeyCatcher` switched off behind it by `blocked`. The
/// panel then answered no key at all: not `r`, not Tab, not Escape. It could
/// only be closed with the mouse.
fn write_visibility_harness(dir: &Path, catcher: &Path) {
    let src = menu_source();
    std::fs::copy(catcher, dir.join("PanelKeyCatcher.qml"))
        .expect("the shell's catcher is readable");
    std::fs::write(
        dir.join("TextField.qml"),
        r#"import QtQuick
import QtQuick.Controls
TextField {
  property color foreground: "white"
}
"#,
    )
    .unwrap();
    write_stub_modules(dir);

    let harness = format!(
        r##"import QtQuick
import QtTest
import qs.Commons
import qs.Ui

Item {{
  id: root
  width: 420
  height: 320

  // The one property the lifted column is bound to. Every value but "ready"
  // takes the box off the screen; "missing" and "incompatible" are two the
  // engine really reports.
  property string engineState: "ready"
  property string filterText: ""
  property int refreshes: 0
  property int snapshots: 0
  property int closes: 0
  property int panelSwitches: 0
  property color foreground: "#eeeeee"
  property color dim: "#999999"
  property string fontFamily: "monospace"
  function refreshAll() {{ root.refreshes++ }}
  function snapshotNow() {{ root.snapshots++ }}
  function close() {{ root.closes++ }}
  function switchPanel(direction) {{ root.panelSwitches++ }}

  {catcher}

    Column {{
      anchors.fill: parent

      {box}
    }}
  }}

  TestCase {{
    name: "menuFilterVisibility"
    when: windowShown

    function init() {{
      root.engineState = "ready"
      filterField.text = ""
      root.filterText = ""
      root.refreshes = 0
      root.snapshots = 0
      root.closes = 0
      root.panelSwitches = 0
      keyCatcher.forceActiveFocus()
      verify(!filterField.activeFocus, "each case starts with the panel holding the keys")
    }}

    // `/` while the engine is not ready.
    function test_1_the_key_does_not_focus_a_box_that_is_not_on_screen() {{
      root.engineState = "missing"
      verify(!filterField.visible,
             "premise: with no engine there is nothing to filter, so the box is not shown")
      keyClick(Qt.Key_Slash)
      verify(!filterField.activeFocus,
             "`/` focused a box nobody can see; `blocked` then switches the "
             + "panel's key catcher off behind it")
      verify(keyCatcher.activeFocus, "the panel gave the keys away to an invisible item")
      verify(!keyCatcher.blocked, "the panel is blocked by a box that is not on screen")

      // The consequence, stated as the user meets it: every key still works.
      keyClick("r")
      compare(root.refreshes, 1, "the panel answers no key at all")
      keyClick(Qt.Key_Tab)
      compare(root.panelSwitches, 1, "Tab no longer switches panel")
      keyClick(Qt.Key_Escape)
      compare(root.closes, 1, "the panel can no longer be closed from the keyboard")
    }}

    // Readiness lost *while typing*, which is the ordinary way in: the status
    // probe reruns on a timer and the engine can stop being ready at any
    // moment.
    function test_2_losing_readiness_while_typing_hands_the_keys_back() {{
      keyClick(Qt.Key_Slash)
      verify(filterField.activeFocus, "premise: `/` reaches the box while the engine is ready")
      keyClick("d"); keyClick("e"); keyClick("v")
      compare(filterField.text, "dev")

      root.engineState = "incompatible"
      verify(!filterField.visible, "premise: an incompatible engine takes the box off the screen")
      verify(!filterField.activeFocus,
             "the box kept the keys after it left the screen, and there is no "
             + "key that can take them back from it")
      verify(keyCatcher.activeFocus, "the keys did not go back to the panel")
      verify(!keyCatcher.blocked, "the panel is still blocked by the box that vanished")

      keyClick("r")
      compare(root.refreshes, 1, "the panel answers no key at all")
      keyClick(Qt.Key_Escape)
      compare(root.closes, 1, "the panel can only be closed with the mouse")
    }}
  }}
}}
"##,
        catcher = key_catcher_header(&src),
        box = block_with(&src, "id: filterBox"),
    );
    std::fs::write(dir.join("tst_menuvisibility.qml"), harness).unwrap();
}

/// Run one harness file under `qmltestrunner`, or say why it was skipped.
fn run_harness(file: &str, write: impl FnOnce(&Path, &Path), what: &str) {
    let Some(runner) = test_runner() else {
        eprintln!(
            "SKIPPED: no qmltestrunner found (looked at $OSM_QMLTESTRUNNER, \
             /usr/lib/qt6/bin/qmltestrunner and PATH). The structural rules in \
             tests/manifest.rs still ran."
        );
        return;
    };
    let Some(catcher) = shell_key_catcher() else {
        eprintln!(
            "SKIPPED: the Omarchy shell's PanelKeyCatcher.qml is not installed \
             (looked under $OSM_SHELL, default /usr/share/omarchy/shell). This \
             test drives the real component rather than a copy of it."
        );
        return;
    };

    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), &catcher);

    let out = common::run_bounded(
        std::process::Command::new(&runner)
            .current_dir(tmp.path())
            .env("QT_FORCE_STDERR_LOGGING", "1")
            .env("QT_QPA_PLATFORM", "offscreen")
            .arg("-input")
            .arg(file)
            .arg("-import")
            .arg(tmp.path())
            .arg("-import")
            .arg("/usr/lib/qt6/qml"),
        std::time::Duration::from_secs(60),
    );
    let text =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{what}:\n{text}");
    assert!(
        text.contains("0 failed"),
        "qmltestrunner reported no result at all:\n{text}"
    );
}

/// `/` may not strand the panel on a box that is not on screen.
#[test]
fn the_filter_box_never_holds_the_keys_while_it_is_invisible() {
    run_harness(
        "tst_menuvisibility.qml",
        write_visibility_harness,
        "the menu's key wiring with the filter box's own visibility, driven by \
         real key events through the shell's own PanelKeyCatcher",
    );
}

#[test]
fn the_filter_box_can_be_reached_and_left_with_the_keyboard() {
    let Some(runner) = test_runner() else {
        eprintln!(
            "SKIPPED: no qmltestrunner found (looked at $OSM_QMLTESTRUNNER, \
             /usr/lib/qt6/bin/qmltestrunner and PATH). The structural rules in \
             tests/manifest.rs still ran."
        );
        return;
    };
    let Some(catcher) = shell_key_catcher() else {
        eprintln!(
            "SKIPPED: the Omarchy shell's PanelKeyCatcher.qml is not installed \
             (looked under $OSM_SHELL, default /usr/share/omarchy/shell). This \
             test drives the real component rather than a copy of it."
        );
        return;
    };

    let tmp = tempfile::tempdir().unwrap();
    write_harness(tmp.path(), &catcher);

    let out = common::run_bounded(
        std::process::Command::new(&runner)
            .current_dir(tmp.path())
            .env("QT_FORCE_STDERR_LOGGING", "1")
            .env("QT_QPA_PLATFORM", "offscreen")
            .arg("-input")
            .arg("tst_menufocus.qml")
            .arg("-import")
            .arg("/usr/lib/qt6/qml"),
        std::time::Duration::from_secs(60),
    );
    let text =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "the menu's key wiring, driven by real key events through the shell's \
         own PanelKeyCatcher:\n{text}"
    );
    assert!(
        text.contains("0 failed"),
        "qmltestrunner reported no result at all:\n{text}"
    );
}
