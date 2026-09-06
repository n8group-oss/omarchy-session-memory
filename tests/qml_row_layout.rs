//! Where the menu's rows put their children, measured rather than reasoned.
//!
//! # Why this file exists
//!
//! The maintainer opened the panel and saw the "Copy resume" button cut in
//! half. The cause is in the source: the conversation row was a plain `Row`
//! with three children — a label at its natural width, a second label at
//! `Math.max(Style.space(40), width * 0.4)`, and a `Button`. A `Row` is a
//! positioner, not a layout: it places each child at that child's own width,
//! left to right, and never once looks at how much room the parent has. Give
//! it a 36-character conversation id and a 62-character project path and the
//! button starts past the right edge of the panel, where the enclosing
//! `Flickable` clips it. The session row had the same shape and the same bug.
//!
//! # What this actually verifies
//!
//! The delegate is **lifted verbatim out of `Menu.qml`, instantiated by Qt,
//! laid out, and then measured**: every drawn item's right edge must land
//! inside the row, and the button must still be at least its own implicit
//! width — a button squeezed to 30 px is as unusable as one that is clipped.
//! It runs at four panel widths, because a layout that only holds at the width
//! the author happened to test is the same defect deferred.
//!
//! A row is no longer a single layout. A session row is an identity line (its
//! name and counts), a goal line saying what the session is about, and — when
//! it is open — the conversations behind that goal; a conversation row is an
//! identity line above its project path. So the measurement walks *through*
//! the containers and reports every item that actually draws something, mapped
//! back into the row's own coordinates. That is strictly more than it used to
//! check: an item nested two layouts deep can overflow the panel exactly as
//! well as a direct child, and before this walk nothing would have seen it.
//!
//! # What it cannot verify
//!
//! It needs a QML runtime, and it cannot use the real `qs.Ui` / `qs.Commons`
//! modules: those import `Quickshell`, whose plugin refuses to load outside
//! the `quickshell` binary ("module \"Quickshell\" plugin
//! \"quickshell-coreplugin\" not found"). So `Style` and `Button` are stood in
//! for by the stubs written below — a button that is a rectangle sized to its
//! label. That means the *numbers* here are not the shell's numbers; what is
//! being checked is the geometry rule the delegate's own markup produces, which
//! is where the defect lived. The source-level rules in `tests/manifest.rs`
//! pin the structure itself and run everywhere, including CI containers with
//! no Qt.

mod common;

use serde::Deserialize;
use std::path::{Path, PathBuf};

fn qml_runtime() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("OSM_QML") {
        let p = PathBuf::from(explicit);
        return p.exists().then_some(p);
    }
    let fixed = PathBuf::from("/usr/lib/qt6/bin/qml");
    if fixed.exists() {
        return Some(fixed);
    }
    for name in ["qml6", "qml"] {
        for dir in std::env::var("PATH").unwrap_or_default().split(':') {
            let p = Path::new(dir).join(name);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

fn menu_source() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Menu.qml");
    std::fs::read_to_string(path).expect("Menu.qml exists")
}

/// The object declaration that contains `marker`, as `(type name, whole
/// declaration)`.
///
/// Used to lift a delegate out of the menu without copying it: the text that
/// gets instantiated below is the text that ships.
fn enclosing_block(src: &str, marker: &str) -> (String, String) {
    let at = src
        .find(marker)
        .unwrap_or_else(|| panic!("Menu.qml contains `{marker}`"));
    block_around(src, at)
}

/// The declaration that *contains* the one holding `marker` — the row a
/// button sits in, given something written on the button.
///
/// Naming the row by a child rather than by its own `id` is deliberate: it
/// makes this measurement work against the broken version of the file too,
/// which is the only way to know the measurement can fail.
fn enclosing_parent_block(src: &str, marker: &str) -> (String, String) {
    let at = src
        .find(marker)
        .unwrap_or_else(|| panic!("Menu.qml contains `{marker}`"));
    let (_, inner) = block_around(src, at);
    let inner_at = src.find(&inner).expect("the inner block came from src");
    block_around(src, inner_at)
}

fn block_around(src: &str, at: usize) -> (String, String) {
    let open = src[..at]
        .rfind('{')
        .expect("the offset is inside some object");
    let name_end = src[..open].trim_end().len();
    let name_start = src[..name_end]
        .rfind(|c: char| !(c.is_alphanumeric() || c == '_' || c == '.'))
        .map(|i| i + 1)
        .unwrap_or(0);
    let name = src[name_start..name_end].to_string();

    let bytes = src.as_bytes();
    let mut depth = 0usize;
    for i in open..bytes.len() {
        match bytes[i] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return (name, src[name_start..=i].to_string());
                }
            }
            _ => {}
        }
    }
    panic!("the block at {at} is not closed");
}

/// A one-line property declaration, verbatim.
///
/// The rows' threshold is written in terms of `root.identityFloor`; without
/// the real declaration in the harness the binding compares against
/// `undefined`, every row stacks, and these measurements quietly stop being
/// about the rule under test.
/// Absent from the file, absent from the harness — deliberately, and not
/// because it is optional. `tests/manifest.rs` requires the declaration. What
/// this allows is measuring a `Menu.qml` that does *not* have it, which is the
/// only way to know these measurements can fail at all.
fn qml_property(src: &str, name: &str) -> String {
    let decl = format!("readonly property int {name}:");
    let Some(at) = src.find(&decl) else {
        return String::new();
    };
    let end = src[at..].find('\n').map(|i| at + i).unwrap_or(src.len());
    src[at..end].to_string()
}

fn qml_function(src: &str, name: &str) -> String {
    let at = src
        .find(&format!("function {name}("))
        .unwrap_or_else(|| panic!("Menu.qml defines {name}"));
    let open = src[at..].find('{').expect("a function body") + at;
    let bytes = src.as_bytes();
    let mut depth = 0usize;
    for i in open..bytes.len() {
        match bytes[i] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return src[at..=i].to_string();
                }
            }
            _ => {}
        }
    }
    panic!("{name} is not closed");
}

/// One child of the row as Qt laid it out.
#[derive(Debug, Deserialize)]
struct Child {
    name: String,
    x: f64,
    /// Where the item sits vertically *within the row*, and how tall it is:
    /// together they are how two fields drawn side by side are told from two
    /// drawn one above the other.
    ///
    /// Position alone is not enough. A caption beside a body label is a
    /// smaller box on the same line, and a layout centres it — so the two have
    /// different `y` while being as side by side as anything ever is.
    y: f64,
    height: f64,
    width: f64,
    #[serde(rename = "implicitWidth")]
    implicit_width: f64,
    /// `Some` for a `Text` (whether it elided), `None` for anything else.
    truncated: Option<bool>,
    /// What the label says, whole, whether or not all of it is drawn.
    text: Option<String>,
    /// The width of [`SAMPLE`] in *this* child's own font, measured by Qt.
    ///
    /// The yardstick for "still identifiable": a field narrower than this
    /// cannot show enough of a session name or a project path to tell two of
    /// them apart, whatever the font turns out to be.
    sample: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct Row {
    #[serde(rename = "rowWidth")]
    row_width: f64,
    children: Vec<Child>,
}

impl Row {
    fn overflowing(&self) -> Vec<&Child> {
        self.children
            .iter()
            .filter(|c| c.x + c.width > self.row_width + 0.5)
            .collect()
    }

    fn describe(&self) -> String {
        let mut s = format!("row is {:.0}px wide;", self.row_width);
        for c in &self.children {
            s += &format!(
                " {} at x={:.0} y-row w={:.0} (right edge {:.0}, wants {:.0}){};",
                c.name,
                c.x,
                c.width,
                c.x + c.width,
                c.implicit_width,
                match &c.text {
                    Some(t) => format!(" holding {t:?}"),
                    None => String::new(),
                }
            );
        }
        s
    }
}

/// The stub shell modules, written into `dir` as `qs.Commons` and `qs.Ui`.
///
/// The real ones cannot be loaded here (see the file comment), so these
/// provide the same names with plain arithmetic behind them.
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
    readonly property int controlGap: 6
    readonly property int controlPaddingX: 10
    readonly property int controlPaddingY: 6
  }
  readonly property int cornerRadius: 4
}
"#,
    )
    .unwrap();

    std::fs::write(ui.join("qmldir"), "module qs.Ui\nButton 1.0 Button.qml\n").unwrap();
    // A button sized by its label, which is all this measurement needs from
    // one: the defect is about whether the row leaves room for that size.
    std::fs::write(
        ui.join("Button.qml"),
        r#"import QtQuick
Rectangle {
  id: button
  property string text: ""
  property color foreground: "white"
  property string fontFamily: "monospace"
  property bool bordered: false
  signal clicked()
  color: "transparent"
  implicitWidth: label.implicitWidth + 20
  implicitHeight: label.implicitHeight + 12
  Text {
    id: label
    anchors.centerIn: parent
    text: button.text
    color: button.foreground
    font.family: button.fontFamily
    font.pixelSize: 13
  }
}
"#,
    )
    .unwrap();
}

/// Instantiate the block holding `marker` at `width`, let Qt lay it out, and
/// report where every child landed.
///
/// `row_json` is the `modelData` for a `Repeater` delegate; a block that is
/// not a delegate (the footer's button row) passes `None` and is instantiated
/// directly.
fn lay_out(qml: &Path, marker: &str, row_json: Option<&str>, width: u32) -> Row {
    let src = menu_source();
    // A delegate is named by its own id; a plain row is named by one of its
    // children, so that the lookup survives the row being retyped.
    let (_kind, block) = match row_json {
        Some(_) => enclosing_block(&src, marker),
        None => enclosing_parent_block(&src, marker),
    };
    let helpers = [
        "shortId",
        "placementText",
        "goalText",
        "titleText",
        "paneText",
    ]
    .iter()
    .map(|n| qml_function(&src, n))
    .collect::<Vec<_>>()
    .join("\n  ");
    let (body, accessor) = match row_json {
        Some(json) => (
            format!(
                "Repeater {{\n      id: rows\n      model: [ {json} ]\n\n      {block}\n    }}"
            ),
            "rows.itemAt(0)",
        ),
        None => (block, "column.children[0]"),
    };

    let sample = SAMPLE;
    let expanded = SESSION_NAME;
    let floor = qml_property(&src, "identityFloor");
    let harness = format!(
        r##"import QtQuick
import QtQuick.Layouts
import qs.Commons
import qs.Ui

Item {{
  id: root
  width: {width}
  height: 300

  property color foreground: "#eeeeee"
  property color dim: "#999999"
  property color urgent: "#ff5555"
  property string fontFamily: "monospace"
  property string engineState: "ready"
  {floor}
  property bool restoreArmed: true
  // The session under test is the open one, so its conversations are built,
  // laid out and measured like everything else rather than left to a reader's
  // confidence that a hidden thing would have fitted.
  property string expandedSession: "{expanded}"
  function toggleSession(name) {{ }}

  // Every item the row actually draws, in the row's own coordinates.
  //
  // Descends through the containers — a Column of layouts is still one row to
  // the user — and stops at the things that draw: a Text, or a control like
  // the Button stub, whose own label is its business and not this walk's.
  function collect(row, item, out) {{
    for (var i = 0; i < item.children.length; i++) {{
      var c = item.children[i]
      var name = String(c).replace(/\(.*$/, "")
      // A Repeater draws nothing and has no size; its delegates are children
      // of the item the Repeater is in, and are reached that way.
      if (name.indexOf("QQuickRepeater") === 0) continue
      if (c.visible === false) continue
      var container = name.indexOf("Layout") >= 0
        || name.indexOf("QQuickColumn") === 0
        || name.indexOf("QQuickRow") === 0
        || name.indexOf("QQuickFlow") === 0
      if (container) {{ root.collect(row, c, out); continue }}
      var at = c.mapToItem(row, 0, 0)
      out.push({{
        name: name,
        x: at.x,
        y: at.y,
        height: c.height,
        width: c.width,
        implicitWidth: c.implicitWidth,
        truncated: (c.truncated === undefined ? null : c.truncated),
        text: (c.text === undefined ? null : String(c.text)),
        sample: (c.truncated === undefined ? null : root.sampleWidth(c.font))
      }})
    }}
  }}
  // The width of `{sample}` in `font`, measured by Qt in that very font
  // rather than guessed from a pixel size.
  function sampleWidth(font) {{
    var m = Qt.createQmlObject(
      'import QtQuick; TextMetrics {{ text: "{sample}" }}', root)
    m.font = font
    var w = m.advanceWidth
    m.destroy()
    return w
  }}
  function copyResumeCommand(entry) {{ }}
  function snapshotNow() {{ }}
  function restoreNow() {{ }}
  function refreshAll() {{ }}
  {helpers}

  QtObject {{
    id: actionProcess
    property bool running: false
  }}

  Column {{
    id: column
    width: root.width

    {body}
  }}

  Timer {{
    interval: 250
    running: true
    repeat: false
    onTriggered: {{
      var item = {accessor}
      var kids = []
      root.collect(item, item, kids)
      console.warn("BEGIN" + JSON.stringify({{ rowWidth: item.width, children: kids }}) + "END")
      Qt.exit(0)
    }}
  }}
}}
"##
    );

    let tmp = tempfile::tempdir().unwrap();
    write_stub_modules(tmp.path());
    let file = tmp.path().join("row.qml");
    std::fs::write(&file, harness).unwrap();

    let out = common::run_bounded(
        std::process::Command::new(qml)
            .env("QT_FORCE_STDERR_LOGGING", "1")
            .env("QT_QPA_PLATFORM", "offscreen")
            .arg("-I")
            .arg(tmp.path())
            .arg("-I")
            .arg("/usr/lib/qt6/qml")
            .arg(&file),
        std::time::Duration::from_secs(60),
    );
    let text =
        String::from_utf8_lossy(&out.stderr).into_owned() + &String::from_utf8_lossy(&out.stdout);
    let begin = text
        .find("BEGIN")
        .unwrap_or_else(|| panic!("the QML runtime printed no geometry: {text}"));
    let end = text.find("END").expect("a terminated result");
    serde_json::from_str::<Row>(&text[begin + 5..end]).expect("the harness printed a row")
}

/// A conversation as long as the ones on the maintainer's machine: a full
/// UUID and a real project path.
const CONVERSATION: &str = r#"{
  "kind": "codex",
  "native_id": "0198f2ac-1c4e-7a3b-9f11-2b6d8c4e5a17",
  "project_dir": "/home/user/projects/n8group-oss/omarchy-session-memory/docs/plans",
  "title": "Read-only design review, twenty-seventh round, no edits, no git writes",
  "title_source": "first_prompt"
}"#;

/// The title the conversation row draws, whole.
const CONVERSATION_TITLE: &str =
    "Read-only design review, twenty-seventh round, no edits, no git writes";

/// The session name the fixture below carries, and the row the harness opens.
const SESSION_NAME: &str = "omarchy-session-memory-plan4";

/// A session with a long name and a long monitor, which is what pushed the
/// second label off the session row.
const SESSION: &str = r#"{
  "name": "omarchy-session-memory-plan4",
  "windows": 6,
  "panes": 18,
  "agents": 4,
  "workspace": 3,
  "monitor": "Dell-Inc.-DELL-U4025QW-HXXXXXX",
  "goal": {
    "title": "Tmux memory management plugin",
    "source": "agent",
    "kind": "claude",
    "native_id": "099d4f76-19b5-47fc-8a7f-fffb219cf6eb"
  },
  "conversations": [
    {
      "kind": "claude",
      "native_id": "099d4f76-19b5-47fc-8a7f-fffb219cf6eb",
      "title": "Tmux memory management plugin",
      "title_source": "agent",
      "window_idx": 0,
      "pane_idx": 1,
      "last_active": 1788500000
    },
    {
      "kind": "codex",
      "native_id": "0198f2ac-1c4e-7a3b-9f11-2b6d8c4e5a17",
      "title": null,
      "title_source": null,
      "window_idx": 2,
      "pane_idx": 0,
      "last_active": null
    }
  ]
}"#;

/// The goal that session row draws.
const SESSION_GOAL: &str = "Tmux memory management plugin";

/// The widths a bar popup actually gets on a screen too small for the one it
/// asks for. 180 is narrower than anything the previous wave measured, and it
/// is where a single line stops being able to hold an identity, a set of
/// counts and a button at once.
///
/// These stay where they are as the declared width grows. `fittedContentWidth`
/// clamps the request to the screen, so asking for more is what a *large*
/// display gets and a small one is left exactly here — which makes these four
/// the checks that protect the small display, and a wider default must not
/// cost them anything.
const WIDTHS: [u32; 4] = [420, 320, 240, 180];

/// The width `Menu.qml` asks the panel for, read from the file.
///
/// Read rather than written down, because a number repeated in two places is
/// one that drifts: the previous version of this file said 420 in a comment
/// and in an assertion, and both would have gone on passing against a menu
/// that had been widened or narrowed underneath them.
fn declared_panel_width(src: &str) -> u32 {
    let marker = "fittedContentWidth(Style.space(";
    let at = src
        .find(marker)
        .expect("Menu.qml asks the panel for a content width");
    src[at + marker.len()..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .expect("the requested width is a plain integer")
}

/// The least a field has to be able to show to be worth drawing: eight
/// characters.
///
/// Eight is not a design token, it is a claim about reading. `dev` and
/// `dev-old` differ in the first seven; `…/osm-plan4` and `…/osm-plan4b`
/// differ in the last ten. A row that shows fewer than eight characters of the
/// only field that identifies it is a row the user cannot tell from the one
/// above it, and the identical counts beside it make that worse rather than
/// better. Measured in each field's own font by Qt, so it means the same thing
/// whatever font the shell is set to.
const SAMPLE: &str = "mmmmmmmm";

#[test]
fn the_copy_resume_button_is_inside_the_panel_at_every_width() {
    let Some(qml) = qml_runtime() else {
        eprintln!(
            "SKIPPED: no QML runtime found (looked at $OSM_QML, \
             /usr/lib/qt6/bin/qml, qml6 and qml on PATH). The structural \
             rules in tests/manifest.rs still ran."
        );
        return;
    };
    for width in WIDTHS {
        let row = lay_out(&qml, "id: conversationDelegate", Some(CONVERSATION), width);
        assert!(
            row.overflowing().is_empty(),
            "at a {width}px panel the conversation row put a child past its \
             own right edge, where the Flickable clips it — this is the \
             maintainer's \"some part of button is truncated\": {}",
            row.describe()
        );
        let button = row
            .children
            .iter()
            .find(|c| c.text.as_deref() == Some("Copy resume"))
            .unwrap_or_else(|| {
                panic!(
                    "the conversation row has no \"Copy resume\" button: {}",
                    row.describe()
                )
            });
        assert!(
            button.truncated.is_none(),
            "the item saying \"Copy resume\" is a Text, not the button: {}",
            row.describe()
        );
        assert!(
            button.width + 0.5 >= button.implicit_width,
            "at a {width}px panel the button was squeezed to {:.0}px when it \
             needs {:.0}px, which cuts its label just as surely as clipping \
             it: {}",
            button.width,
            button.implicit_width,
            row.describe()
        );
    }
}

#[test]
fn a_long_project_path_elides_instead_of_pushing_the_button_out() {
    let Some(qml) = qml_runtime() else {
        eprintln!("SKIPPED: no QML runtime found; see tests/manifest.rs.");
        return;
    };
    let row = lay_out(&qml, "id: conversationDelegate", Some(CONVERSATION), 420);
    assert!(
        row.children.iter().any(|c| c.truncated == Some(true)),
        "a 72-character project path was drawn at its full width instead of \
         eliding, so something else in the row had to give: {}",
        row.describe()
    );
}

/// The footer had the same shape with no elastic child at all: three buttons
/// at their natural widths, and "Restore now" grows into "Confirm restore"
/// the moment it is armed. A third button half off the edge is an action the
/// user cannot reach.
#[test]
fn the_footer_buttons_are_all_reachable_at_every_width() {
    let Some(qml) = qml_runtime() else {
        eprintln!("SKIPPED: no QML runtime found; see tests/manifest.rs.");
        return;
    };
    for width in WIDTHS {
        let row = lay_out(&qml, "text: \"Snapshot now\"", None, width);
        assert_eq!(
            row.children.len(),
            3,
            "the footer should hold three buttons: {}",
            row.describe()
        );
        assert!(
            row.overflowing().is_empty(),
            "at a {width}px panel a footer button ran off the edge instead of \
             wrapping onto the next line: {}",
            row.describe()
        );
        for button in &row.children {
            assert!(
                button.width + 0.5 >= button.implicit_width,
                "at a {width}px panel a footer button was squeezed below its \
                 own label: {}",
                row.describe()
            );
        }
    }
}

#[test]
fn the_session_row_keeps_its_counts_inside_the_panel() {
    let Some(qml) = qml_runtime() else {
        eprintln!("SKIPPED: no QML runtime found; see tests/manifest.rs.");
        return;
    };
    for width in WIDTHS {
        let row = lay_out(&qml, "id: sessionDelegate", Some(SESSION), width);
        assert!(
            row.overflowing().is_empty(),
            "at a {width}px panel the session row put a child past its own \
             right edge: the window/pane/agent counts and the monitor are \
             the whole content of that label, and half of it is no fact at \
             all: {}",
            row.describe()
        );
    }
}

impl Row {
    /// Whether the two items saying `a` and `b` were drawn on the same line.
    ///
    /// A row is several lines now, so "is this row on one line" is no longer a
    /// question with an answer. What is still a question — and the one the
    /// stacking rule is about — is whether a *line* held its fields side by
    /// side or gave up and put them one under the other.
    fn share_a_line(&self, a: &str, b: &str) -> bool {
        let (a, b) = (self.carrying(a), self.carrying(b));
        a.y < b.y + b.height && b.y < a.y + a.height
    }

    /// The child that carries `identity`, whole — the label whose text is the
    /// one thing telling this row from the one above it.
    ///
    /// Found by what it says rather than by its position, so that rearranging
    /// the row cannot quietly move the assertion onto a different label.
    fn carrying(&self, identity: &str) -> &Child {
        self.children
            .iter()
            .find(|c| c.text.as_deref() == Some(identity))
            .unwrap_or_else(|| {
                panic!(
                    "no child of this row says {identity:?}: {}",
                    self.describe()
                )
            })
    }
}

/// A row that has shrunk to counts and a monitor, with nothing saying *whose*
/// counts they are, is worse than one that overflows.
///
/// Both of the menu's rows have an elastic identity field — the session name,
/// the conversation's project path — declared with `Layout.preferredWidth: 0`
/// and no minimum. Qt's layout contract lets a fill item shrink to its
/// declared minimum, and a stretch factor is not a floor: it decides who gets
/// *spare* room, and there is none to give. So under width pressure the
/// identity is the first thing to reach zero, and what is left on screen is
/// several rows of identical metadata that the user cannot tell apart. The
/// previous wave's tests would have passed through all of it: nothing
/// overflowed.
#[test]
fn the_session_name_is_still_readable_at_every_width() {
    let Some(qml) = qml_runtime() else {
        eprintln!("SKIPPED: no QML runtime found; see tests/manifest.rs.");
        return;
    };
    for width in WIDTHS {
        let row = lay_out(&qml, "id: sessionDelegate", Some(SESSION), width);
        let name = row.carrying("omarchy-session-memory-plan4");
        let floor = name.sample.expect("the session name is a Text");
        assert!(
            name.width > 0.0,
            "at a {width}px panel the session name was given no width at all: \
             the row is counts and a monitor and nothing that says whose they \
             are: {}",
            row.describe()
        );
        assert!(
            name.width + 0.5 >= floor,
            "at a {width}px panel the session name was drawn {:.0}px wide, \
             less than the {floor:.0}px eight of its own characters need: two \
             sessions whose names differ after the first few are the same row \
             on screen. The metadata beside it must give first: {}",
            name.width,
            row.describe()
        );
    }
}

/// The same for a conversation, where the identity is the project path.
///
/// `kind` and a twelve-character id are bounded and so keep their natural
/// width; the path is the elastic one and the only field that says which
/// project a conversation belongs to. `codex 0198f2ac-1c4e` beside a blank is
/// not something a user can choose from — with 2448 of them, choosing is the
/// entire purpose of the list.
#[test]
fn the_project_path_is_still_readable_at_every_width() {
    let Some(qml) = qml_runtime() else {
        eprintln!("SKIPPED: no QML runtime found; see tests/manifest.rs.");
        return;
    };
    for width in WIDTHS {
        let row = lay_out(&qml, "id: conversationDelegate", Some(CONVERSATION), width);
        let path =
            row.carrying("/home/user/projects/n8group-oss/omarchy-session-memory/docs/plans");
        let floor = path.sample.expect("the project path is a Text");
        assert!(
            path.width > 0.0,
            "at a {width}px panel the conversation row lost its project path \
             entirely, leaving a kind, an id fragment and a button: {}",
            row.describe()
        );
        assert!(
            path.width + 0.5 >= floor,
            "at a {width}px panel the project path was drawn {:.0}px wide, \
             less than the {floor:.0}px eight of its own characters need — and \
             it elides from the left, so what little is drawn is the tail that \
             identifies it: {}",
            path.width,
            row.describe()
        );
    }
}

/// Each row's identity line stays on one line at the width the menu actually
/// asks for, and stacks only when it must.
///
/// Two-line rows everywhere would be a different panel from the one the
/// maintainer looks at every day, and "it never overflows" is satisfied just
/// as well by stacking everything as by laying anything out. So the threshold
/// itself is measured: at the width `Menu.qml` asks for the name sits beside
/// its counts and the conversation's id beside its title, and at 180px both
/// have stacked and given the identity the full width.
#[test]
fn the_rows_stack_only_where_one_line_cannot_hold_them() {
    let Some(qml) = qml_runtime() else {
        eprintln!("SKIPPED: no QML runtime found; see tests/manifest.rs.");
        return;
    };
    for (marker, model, identity, beside) in [
        (
            "id: sessionDelegate",
            SESSION,
            SESSION_NAME,
            "6w \u{b7} 18p \u{b7} 4a \u{b7} Dell-Inc.-DELL-U4025QW-HXXXXXX",
        ),
        (
            "id: conversationDelegate",
            CONVERSATION,
            CONVERSATION_TITLE,
            "codex 0198f2ac-1c4\u{2026}",
        ),
    ] {
        let asks_for = declared_panel_width(&menu_source());
        let wide = lay_out(&qml, marker, Some(model), asks_for);
        assert!(
            wide.share_a_line(identity, beside),
            "at the {asks_for}px the menu asks for, this row's identity line \
             stacked when it did not have to. Lines that always stack pass \
             every no-overflow check and are still not the panel that was \
             there before: {}",
            wide.describe()
        );

        let narrow = lay_out(&qml, marker, Some(model), 180);
        assert!(
            !narrow.share_a_line(identity, beside),
            "at a 180px panel this line was still holding both fields, which \
             it cannot do without cutting one below the point of being \
             readable: {}",
            narrow.describe()
        );
        let field = narrow.carrying(identity);
        assert!(
            field.width + 0.5 >= narrow.row_width,
            "a stacked line gives the identity the whole width; this one drew \
             it {:.0}px wide in a {:.0}px row: {}",
            field.width,
            narrow.row_width,
            narrow.describe()
        );
    }
}

/// The session's goal is drawn, and drawn wide enough to read.
///
/// This is the line the whole change exists for. A row that says
/// `6w · 18p · 4a · DP-3` and nothing else is what the maintainer had, and a
/// goal squeezed to an ellipsis beside a "Detail" button would be the same
/// row with an extra widget on it.
#[test]
fn the_session_goal_is_drawn_and_readable_at_every_width() {
    let Some(qml) = qml_runtime() else {
        eprintln!("SKIPPED: no QML runtime found; see tests/manifest.rs.");
        return;
    };
    for width in WIDTHS {
        let row = lay_out(&qml, "id: sessionDelegate", Some(SESSION), width);
        let goal = row.carrying(SESSION_GOAL);
        let floor = goal.sample.expect("the goal is a Text");
        assert!(
            goal.width + 0.5 >= floor,
            "at a {width}px panel the session's goal was drawn {:.0}px wide, \
             less than the {floor:.0}px eight of its own characters need — so \
             the one line that says what this session is for is unreadable: {}",
            goal.width,
            row.describe()
        );
        assert!(
            !row.share_a_line(SESSION_GOAL, SESSION_NAME),
            "the goal must sit under the name, not beside it: a title and a \
             session name competing for one line is how both end up elided: {}",
            row.describe()
        );
    }
}

/// Opening a session row draws the conversations behind its goal, inside the
/// panel, each still saying which conversation it is.
#[test]
fn an_opened_session_shows_its_conversations_without_overflowing() {
    let Some(qml) = qml_runtime() else {
        eprintln!("SKIPPED: no QML runtime found; see tests/manifest.rs.");
        return;
    };
    for width in WIDTHS {
        let row = lay_out(&qml, "id: sessionDelegate", Some(SESSION), width);
        assert!(
            row.overflowing().is_empty(),
            "at a {width}px panel an opened session row put something past its \
             own right edge, where the Flickable clips it: {}",
            row.describe()
        );
        // The conversation with no title of its own: it is listed, and it is
        // listed as untitled rather than as a blank line.
        let untitled = row.carrying("untitled");
        let floor = untitled.sample.expect("the conversation title is a Text");
        assert!(
            untitled.width + 0.5 >= floor,
            "at a {width}px panel a conversation in the detail was drawn \
             {:.0}px wide: {}",
            untitled.width,
            row.describe()
        );
        let button = row
            .children
            .iter()
            .find(|c| c.text.as_deref() == Some("Hide"))
            .unwrap_or_else(|| panic!("an open row offers a way to close it: {}", row.describe()));
        assert!(
            button.width + 0.5 >= button.implicit_width,
            "at a {width}px panel the detail button was squeezed to {:.0}px \
             when it needs {:.0}px: {}",
            button.width,
            button.implicit_width,
            row.describe()
        );
    }
}

/// The panel is as wide as the row it now draws, not as wide as it was before
/// that row grew.
///
/// A session row used to be a name and its counts. It is now a name, its
/// counts, a line saying what the session is *about*, and a button that opens
/// the conversations behind it — and the panel was still asking for the width
/// it asked for when there were two fields on it. On the maintainer's screen
/// that is about 390 drawn pixels, and measured offscreen at the old 420 the
/// name was drawn 108px of the 218px it wants and the goal 107px of 226: both
/// cut roughly in half, on every row, with the identical-looking counts beside
/// them intact.
///
/// So the requested width is measured against the row rather than chosen: the
/// two fields that say which session this is and what it is for are drawn
/// whole for the maintainer's own fixture — a 28-character session name and a
/// 29-character goal — and the conversation row's project path is drawn whole
/// beside its button.
///
/// This costs nothing on a small display. `KeyboardPanel.fittedContentWidth`
/// returns `min(requested, screen - margins)`, so the request is a ceiling and
/// never a floor; a narrow screen gets exactly what it got before, which is
/// what [`WIDTHS`] above goes on measuring.
///
/// The numbers are this harness's, not the shell's — `Style` is stubbed and
/// the font is whatever `monospace` resolves to here — so what is pinned is
/// the relationship between the declared width and the row's own markup, not
/// a promise about pixels on the maintainer's monitor.
#[test]
fn the_panel_asks_for_a_width_that_fits_the_row_it_draws() {
    let Some(qml) = qml_runtime() else {
        eprintln!("SKIPPED: no QML runtime found; see tests/manifest.rs.");
        return;
    };
    let width = declared_panel_width(&menu_source());

    let row = lay_out(&qml, "id: sessionDelegate", Some(SESSION), width);
    for whole in [SESSION_NAME, SESSION_GOAL] {
        let field = row.carrying(whole);
        assert_eq!(
            field.truncated,
            Some(false),
            "at the {width}px the menu asks for, {whole:?} was drawn {:.0}px \
             of the {:.0}px it needs. This is the line that says which session \
             the row is and what it is for, cut so that the counts beside it — \
             which read much the same on every row — could be whole: {}",
            field.width,
            field.implicit_width,
            row.describe()
        );
    }

    let conversation = lay_out(&qml, "id: conversationDelegate", Some(CONVERSATION), width);
    let path = conversation
        .carrying("/home/user/projects/n8group-oss/omarchy-session-memory/docs/plans");
    assert_eq!(
        path.truncated,
        Some(false),
        "at the {width}px the menu asks for, the project path was drawn \
         {:.0}px of the {:.0}px it needs — and it is the field that says which \
         of 2448 conversations this row is: {}",
        path.width,
        path.implicit_width,
        conversation.describe()
    );

    // And the widening did not come at the cost of the things the narrow
    // widths protect: nothing runs off the edge, and the button is whole.
    assert!(
        row.overflowing().is_empty(),
        "at {width}px the session row put a child past its own right edge: {}",
        row.describe()
    );
    let button = row
        .children
        .iter()
        .find(|c| c.text.as_deref() == Some("Hide") || c.text.as_deref() == Some("Detail"))
        .unwrap_or_else(|| panic!("the session row has no detail button: {}", row.describe()));
    assert!(
        button.width + 0.5 >= button.implicit_width,
        "at {width}px the detail button was squeezed to {:.0}px when it needs \
         {:.0}px: {}",
        button.width,
        button.implicit_width,
        row.describe()
    );
}
