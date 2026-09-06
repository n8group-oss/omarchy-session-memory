//! A title is text. The panel must draw it as text.
//!
//! # Why this file exists
//!
//! A conversation's title comes out of a transcript osm did not write — the
//! agent's own `ai-title`, or one line of the user's first prompt. It reached
//! three `Text` items in `Menu.qml` with no `textFormat` set, and a `Text`
//! with no `textFormat` is `Text.AutoText`: Qt looks at the string, decides
//! for itself whether it is HTML, and renders it as markup when it thinks so.
//!
//! Two things follow, and the second is the serious one.
//!
//! * A title of `<b>work</b>` is drawn as a bold **work**. The panel shows the
//!   user something other than what is in their transcript, which is the same
//!   defect class as every other one this plugin is built around: the row says
//!   something that is not what osm recorded.
//! * A title of `<img src="…">` makes Qt **fetch that resource when the panel
//!   opens**. Measured here, on this machine, with Qt 6.11.2: the runtime
//!   prints `QML Text: Cannot open: file:///…` for a missing path, which is
//!   the sound of a load having been attempted. A path is a file read; the
//!   same markup with an `http://` source is a request. Nothing about a title
//!   should reach outside the process, and a title osm derived from somebody's
//!   first prompt is exactly the string an attacker who can write into a
//!   transcript store controls.
//!
//! `project_dir` is here for the same reason, and it is not a fourth title: it
//! is read out of the transcript's own `cwd` record (see
//! `agent::project_dir_from_transcript`), so it is transcript content drawn in
//! the panel, and it had no `textFormat` either.
//!
//! # What this actually verifies
//!
//! Each `Text` is **lifted verbatim out of `Menu.qml`**, given a markup-shaped
//! title, instantiated by Qt and then read back three ways:
//!
//! * its resolved `textFormat` is `Text.PlainText`;
//! * its `implicitWidth` matches that of a control `Text` holding the same
//!   string in the same font with `textFormat: Text.PlainText` — i.e. the
//!   markup really was drawn as characters, not interpreted. On the unfixed
//!   file `<b>work</b>` measured 31.19px against the control's 85.77px;
//! * the runtime printed no resource-load complaint, which is what proves the
//!   `<img>` title was never fetched.
//!
//! # What it cannot verify without Qt
//!
//! It needs a QML runtime (`/usr/lib/qt6/bin/qml`, or `qml6`/`qml` on `PATH`),
//! which the machine running the Omarchy shell has and this project's CI
//! containers do not. **These tests skip in CI.** The source-level rule in
//! `tests/manifest.rs` — that each of these `Text` items declares
//! `textFormat: Text.PlainText` — runs everywhere, and is what keeps the fix
//! from being deleted on a machine with no Qt.

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

/// The `Text` declaration whose `id` is `id`, verbatim.
///
/// Found by the id, which is in the file both before and after the fix — so
/// this measurement runs against the broken version too, which is the only way
/// to know it can fail.
fn text_block(src: &str, id: &str) -> String {
    let marker = format!("id: {id}\n");
    let at = src
        .find(&marker)
        .unwrap_or_else(|| panic!("Menu.qml declares `{marker}`"));
    let open = src[..at].rfind('{').expect("the id is inside some object");
    let name_end = src[..open].trim_end().len();
    let name_start = src[..name_end]
        .rfind(|c: char| !(c.is_alphanumeric() || c == '_' || c == '.'))
        .map(|i| i + 1)
        .unwrap_or(0);
    let decl = balanced(src, name_start);
    assert!(
        decl.starts_with("Text"),
        "{id} is no longer a Text: {}",
        &decl[..decl.len().min(60)]
    );
    decl
}

fn qml_function(src: &str, name: &str) -> String {
    let at = src
        .find(&format!("function {name}("))
        .unwrap_or_else(|| panic!("Menu.qml defines {name}"));
    balanced(src, at)
}

/// The `identityFloor` declaration, or nothing when the file has none.
fn identity_floor(src: &str) -> String {
    let decl = "readonly property int identityFloor:";
    match src.find(decl) {
        Some(at) => {
            let end = src[at..].find('\n').map(|i| at + i).unwrap_or(src.len());
            src[at..end].to_string()
        }
        None => String::new(),
    }
}

/// What Qt made of the item, once it had laid it out.
#[derive(Debug, Deserialize)]
struct Drawn {
    /// The resolved `textFormat`, as the enum's own number.
    format: i64,
    /// The same string in the same font, drawn as plain text — the yardstick.
    #[serde(rename = "controlWidth")]
    control_width: f64,
    #[serde(rename = "implicitWidth")]
    implicit_width: f64,
    text: String,
}

/// The enum values Qt reports, so a failure can name what it found rather than
/// print a bare integer. Confirmed against Qt 6.11.2 by the harness itself,
/// which prints them.
fn format_name(value: i64) -> &'static str {
    match value {
        0 => "Text.PlainText",
        1 => "Text.RichText",
        2 => "Text.AutoText",
        3 => "Text.MarkdownText",
        4 => "Text.StyledText",
        _ => "an unknown text format",
    }
}

/// One of the panel's transcript-derived labels, and what it takes to build it
/// on its own.
struct Renderer {
    /// The `id` of the `Text` in `Menu.qml`.
    id: &'static str,
    /// The type and id of the container the block is written inside. It has to
    /// be a real one: the block's own bindings name it (`sessionGoalLine.width`),
    /// and its `Layout.*` attached properties only mean anything inside a
    /// layout.
    container: (&'static str, &'static str),
    /// A second object the block names, when the container is not also the
    /// delegate holding `modelData`.
    delegate: Option<&'static str>,
}

const RENDERERS: [Renderer; 4] = [
    Renderer {
        id: "sessionGoal",
        container: ("GridLayout", "sessionGoalLine"),
        delegate: Some("sessionDelegate"),
    },
    Renderer {
        id: "conversationDetailTitle",
        container: ("GridLayout", "conversationDetailDelegate"),
        delegate: None,
    },
    Renderer {
        id: "conversationTitle",
        container: ("GridLayout", "conversationIdentity"),
        delegate: Some("conversationDelegate"),
    },
    Renderer {
        id: "conversationPath",
        container: ("Column", "conversationDelegate"),
        delegate: None,
    },
];

/// `Style` with plain numbers behind it. The real `qs.Commons` imports
/// `Quickshell`, whose plugin refuses to load outside the `quickshell` binary.
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

/// Instantiate `renderer`'s `Text` with `title` in it, and report what Qt drew
/// — plus everything the runtime said while drawing it.
fn draw(qml: &Path, renderer: &Renderer, title: &str) -> (Drawn, String) {
    let src = menu_source();
    let block = text_block(&src, renderer.id);
    let helpers = ["goalText", "titleText", "shortId", "paneText"]
        .iter()
        .map(|n| qml_function(&src, n))
        .collect::<Vec<_>>()
        .join("\n  ");
    let floor = identity_floor(&src);

    // One conversation row, whose title *and* project path are the hostile
    // string: whichever label is under test, its own field carries it.
    let model = serde_json::json!({
        "kind": "claude",
        "native_id": "0cfebf91-81c0-43d5-af63-c9fe7e844d01",
        "title": title,
        "title_source": "first_prompt",
        "window_idx": 1,
        "pane_idx": 1,
        "project_dir": title,
        "name": "dev",
        "windows": 1,
        "panes": 1,
        "agents": 1,
        "goal": { "title": title, "source": "first_prompt", "kind": "claude",
                  "native_id": "0cfebf91-81c0-43d5-af63-c9fe7e844d01" },
        "conversations": [],
    });
    let goal = serde_json::to_string(&model["goal"]).unwrap();
    // Parenthesised: a QML binding whose body starts with `{` is parsed as a
    // block, not as an object literal.
    let model = format!("({})", serde_json::to_string(&model).unwrap());
    let goal = format!("({goal})");
    let (container_type, container_id) = renderer.container;
    let extra = match renderer.delegate {
        Some(id) => format!(
            "QtObject {{ id: {id}; property var modelData: {model}; \
             property var goal: {goal}; property var conversations: [] }}"
        ),
        None => String::new(),
    };

    let harness = format!(
        r##"import QtQuick
import QtQuick.Layouts
import qs.Commons

Item {{
  id: root
  width: 420
  height: 300

  property color foreground: "#eeeeee"
  property color dim: "#999999"
  property color urgent: "#ff5555"
  property string fontFamily: "monospace"
  {floor}
  {helpers}

  {extra}

  {container_type} {{
    id: {container_id}
    width: root.width
    property var modelData: {model}
    property var goal: {goal}
    property var conversations: []

    {block}
  }}

  // The yardstick: the same string, in the same font, drawn as characters and
  // nothing else. Built from the subject's own font rather than from a guess,
  // so the comparison cannot be wrong about the typeface.
  Text {{
    id: control
    visible: false
    textFormat: Text.PlainText
    font: {id}.font
    text: String({id}.text)
  }}

  Timer {{
    interval: 200
    running: true
    repeat: false
    onTriggered: {{
      console.warn("BEGIN" + JSON.stringify({{
        format: {id}.textFormat,
        implicitWidth: {id}.implicitWidth,
        controlWidth: control.implicitWidth,
        text: String({id}.text)
      }}) + "END")
      Qt.exit(0)
    }}
  }}
}}
"##,
        id = renderer.id,
    );

    let tmp = tempfile::tempdir().unwrap();
    write_stub_commons(tmp.path());
    let file = tmp.path().join("titlemarkup.qml");
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
    let said =
        String::from_utf8_lossy(&out.stderr).into_owned() + &String::from_utf8_lossy(&out.stdout);
    let begin = said.find("BEGIN").unwrap_or_else(|| {
        panic!(
            "the QML runtime printed nothing usable for {}: {said}",
            renderer.id
        )
    });
    let end = said.find("END").expect("a terminated result");
    let drawn = serde_json::from_str::<Drawn>(&said[begin + 5..end])
        .unwrap_or_else(|e| panic!("the harness printed a result for {}: {e}", renderer.id));
    (drawn, said)
}

/// The finding's own example.
const MARKUP: &str = "<b>work</b>";

/// A title that makes Qt go and get something. The path does not exist, which
/// is what makes the attempt audible: Qt says so.
const IMAGE: &str = "<img src=\"file:///tmp/osm-title-must-never-be-fetched.png\">";

fn skip() -> bool {
    if qml_runtime().is_none() {
        eprintln!(
            "SKIPPED: no QML runtime found (looked at $OSM_QML, \
             /usr/lib/qt6/bin/qml, qml6 and qml on PATH). CI has none, so this \
             file proves nothing there; the source-level rule in \
             tests/manifest.rs still ran."
        );
        return true;
    }
    false
}

/// A title that looks like HTML is drawn as the characters it is.
#[test]
fn markup_in_a_title_is_drawn_and_never_interpreted() {
    if skip() {
        return;
    }
    let qml = qml_runtime().unwrap();
    for renderer in &RENDERERS {
        let (drawn, _) = draw(&qml, renderer, MARKUP);
        assert_eq!(
            drawn.format,
            0,
            "{} draws transcript-derived text as {} — with no textFormat a Text \
             is Text.AutoText, and Qt decides for itself that a title like {:?} \
             is HTML",
            renderer.id,
            format_name(drawn.format),
            MARKUP,
        );
        assert_eq!(
            drawn.text, MARKUP,
            "{} was handed something other than the title under test",
            renderer.id
        );
        assert!(
            (drawn.implicit_width - drawn.control_width).abs() < 0.5,
            "{} drew {:?} in {:.2}px where the same string as plain text needs \
             {:.2}px, so the tags were interpreted rather than shown",
            renderer.id,
            MARKUP,
            drawn.implicit_width,
            drawn.control_width,
        );
    }
}

/// A title that asks for a resource does not get one.
///
/// This is the half that reaches outside the process. `Cannot open` is Qt
/// reporting a load it *tried*; a title that is plain text never causes one.
#[test]
fn an_image_title_makes_the_panel_fetch_nothing() {
    if skip() {
        return;
    }
    let qml = qml_runtime().unwrap();
    for renderer in &RENDERERS {
        let (drawn, said) = draw(&qml, renderer, IMAGE);
        assert!(
            !said.contains("Cannot open"),
            "opening the panel made Qt go and load the resource named in a \
             conversation's own {} — the runtime said: {}",
            renderer.id,
            said.lines()
                .find(|l| l.contains("Cannot open"))
                .unwrap_or_default(),
        );
        assert_eq!(
            drawn.format,
            0,
            "{} is {}, so an <img> title is markup to it",
            renderer.id,
            format_name(drawn.format),
        );
        assert!(
            (drawn.implicit_width - drawn.control_width).abs() < 0.5,
            "{} drew an <img …> title in {:.2}px against {:.2}px for the same \
             characters: it rendered an image box, not the text",
            renderer.id,
            drawn.implicit_width,
            drawn.control_width,
        );
    }
}
