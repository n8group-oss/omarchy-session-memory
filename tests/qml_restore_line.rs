//! What the panel says when a restore finishes.
//!
//! # Why this file exists
//!
//! The engine has three ways of putting a window layout back, and one of them
//! is a *borrowed* layout: when the source snapshot's placement could not be
//! read at capture time, the restore carries the last layout the same boot
//! knew and reports `placement_carried` in `osm restore --json`, naming the
//! snapshot it took the layout from. That report exists because a restore
//! that used a layout it did not itself record has to say so.
//!
//! The panel ran `restore --json`, discarded its stdout, and rendered exit 0
//! as `Done.` So the one consumer this contract was written for was the one
//! consumer that could not see it, and a user whose terminals came back on a
//! layout from twenty minutes ago was told the same word as a user whose
//! layout was recorded and replayed exactly.
//!
//! # What this verifies
//!
//! The functions that build the sentence are **lifted verbatim out of
//! `Menu.qml`, instantiated by Qt, and called** with a real-shaped restore
//! report. Nothing here reimplements the wording.
//!
//! The `detail` in that report is not typed out by hand either: it is read
//! out of `src/restore.rs`, from the `format!` the engine actually emits, so
//! a change to the engine's sentence that the panel can no longer read fails
//! here rather than in front of the user.
//!
//! # What it cannot verify without Qt
//!
//! It needs a QML runtime (`/usr/lib/qt6/bin/qml`, or `qml6`/`qml` on
//! `PATH`), which a machine running the Omarchy shell has and this project's
//! CI containers do not. Without one it says what it skipped and returns; the
//! source-level rule below — that the handler consults the output at all —
//! runs everywhere.

use serde_json::json;
use std::path::{Path, PathBuf};

mod common;

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

fn source(file: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(file);
    std::fs::read_to_string(path).unwrap_or_else(|_| panic!("{file} exists"))
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

/// A named function of the menu, or the empty string when it does not exist.
///
/// Tolerating absence deliberately: the harness has to build against a
/// `Menu.qml` that has not been fixed yet, or nobody has seen this fail.
fn qml_function_opt(src: &str, name: &str) -> String {
    match src.find(&format!("function {name}(")) {
        Some(at) => balanced(src, at),
        None => String::new(),
    }
}

/// The engine's own `placement_carried` sentence, for a given source
/// snapshot, read out of `src/restore.rs`.
///
/// Read rather than typed, because the panel picks the snapshot id out of it:
/// a fixture written by hand would go on agreeing with a panel that no longer
/// agrees with the engine.
fn carried_detail(from: i64, taken_at: i64) -> String {
    let src = source("src/restore.rs");
    // Anchored on the half that only the *carried* sentence has. The
    // `Unavailable` sentence a few lines above it opens with the same words
    // and names no snapshot, and it comes first in the file.
    let marker = src
        .find("the layout comes from snapshot {from}")
        .expect("src/restore.rs still emits a carried-placement sentence");
    let at = src[..marker]
        .rfind('"')
        .expect("the sentence is inside a string literal");
    let rest = &src[at + 1..];
    let end = rest.find('"').expect("a closing quote");
    // Rust string continuation: a backslash at end of line eats the newline
    // and the indentation after it.
    let raw = &rest[..end];
    let mut out = String::new();
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            while chars.peek().is_some_and(|c| c.is_whitespace()) {
                chars.next();
            }
            continue;
        }
        out.push(c);
    }
    out.replace("{from}", &from.to_string())
        .replace("{taken_at}", &taken_at.to_string())
}

/// Call one of the menu's functions with `stdout` and read back what it
/// returns.
fn call_menu(qml: &Path, call: &str, stdout: &str) -> String {
    let src = source("Menu.qml");
    let functions = ["elide", "carriedNote", "carriedFrom", "restoreDone"]
        .iter()
        .map(|n| qml_function_opt(&src, n))
        .collect::<Vec<_>>()
        .join("\n  ");

    let harness = format!(
        r##"import QtQuick

Item {{
  id: root
  width: 400
  height: 100

  property string out: {out}
  {functions}

  Timer {{
    interval: 80
    running: true
    repeat: false
    onTriggered: {{
      var said = ""
      try {{
        said = String({call})
      }} catch (e) {{
        said = "THREW: " + e
      }}
      console.warn("BEGIN" + JSON.stringify({{ text: said }}) + "END")
      Qt.exit(0)
    }}
  }}
}}
"##,
        out = serde_json::to_string(stdout).unwrap(),
    );

    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("restoreline.qml");
    std::fs::write(&file, harness).unwrap();

    let outcome = common::run_bounded(
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
    let text = String::from_utf8_lossy(&outcome.stderr).into_owned()
        + &String::from_utf8_lossy(&outcome.stdout);
    let begin = text
        .find("BEGIN")
        .unwrap_or_else(|| panic!("the QML runtime printed no line: {text}"));
    let end = text.find("END").expect("a terminated result");
    let v: serde_json::Value =
        serde_json::from_str(&text[begin + 5..end]).expect("the harness printed a line");
    v["text"].as_str().unwrap().to_string()
}

/// A `succeeded` restore report that carried its layout from snapshot 41.
fn carried_report() -> String {
    json!({
        "protocol_version": 1,
        "state": "succeeded",
        "reason": "ok",
        "snapshot_id": 42,
        "attempt_id": 7,
        "created": ["dev"],
        "adopted": [],
        "skipped": [],
        "failed": [],
        "conflicts": [],
        "degraded": [],
        "skipped_layouts": [],
        "retryable": false,
        "agents_resumed": 0,
        "agents_failed": [],
        "windows": [
            { "session": "*", "outcome": "placement_carried",
              "detail": carried_detail(41, 1_788_000_000) },
            { "session": "dev", "outcome": "placed", "detail": null }
        ]
    })
    .to_string()
}

/// A restore that borrowed its layout does not report the same word as one
/// that replayed its own.
#[test]
fn a_successful_carried_restore_says_where_the_layout_came_from() {
    let Some(qml) = qml_runtime() else {
        eprintln!("skipped: no QML runtime; set OSM_QML to one to run this");
        return;
    };

    let said = call_menu(&qml, "root.restoreDone(root.out)", &carried_report());

    assert!(
        !said.starts_with("THREW:"),
        "the panel has no working restoreDone(): {said}"
    );
    assert_ne!(
        said.trim(),
        "Done.",
        "a restore that used a layout it did not record was reported as an \
         ordinary success"
    );
    assert!(
        said.contains("41"),
        "the panel does not name the snapshot the layout came from: {said}"
    );
}

/// And a restore that recorded its own layout is still just done.
///
/// The control. A panel that mentioned a borrowed layout on every run would
/// be telling the user something is unusual every time nothing is.
#[test]
fn an_ordinary_successful_restore_is_still_reported_plainly() {
    let Some(qml) = qml_runtime() else {
        eprintln!("skipped: no QML runtime; set OSM_QML to one to run this");
        return;
    };

    let plain = json!({
        "protocol_version": 1,
        "state": "succeeded",
        "reason": "ok",
        "snapshot_id": 42,
        "windows": [{ "session": "dev", "outcome": "placed", "detail": null }]
    })
    .to_string();

    let said = call_menu(&qml, "root.restoreDone(root.out)", &plain);
    assert_eq!(said.trim(), "Done.", "{said}");
}

/// Output that is not a restore report at all leaves the plain word alone.
///
/// `osm snapshot` shares this `Process`, and a restore that was killed by the
/// watchdog leaves whatever half-written bytes it had managed. Neither may
/// make the panel throw.
#[test]
fn output_that_is_not_a_report_is_reported_plainly() {
    let Some(qml) = qml_runtime() else {
        eprintln!("skipped: no QML runtime; set OSM_QML to one to run this");
        return;
    };

    for out in ["", "snapshot 12 recorded", "{\"state\":", "null"] {
        let said = call_menu(&qml, "root.restoreDone(root.out)", out);
        assert_eq!(said.trim(), "Done.", "on {out:?}: {said}");
    }
}

/// The handler has to actually read the output.
///
/// The rule that runs without Qt, and the one that was broken: `restore
/// --json` was started, its `StdioCollector` filled, and every exit-0 run
/// rendered `Done.` with the report unread.
#[test]
fn the_action_handler_reads_what_the_restore_printed() {
    let src = source("Menu.qml");
    let handler = {
        let at = src
            .find("id: actionProcess")
            .expect("Menu.qml has an actionProcess");
        let onexit = src[at..]
            .find("onExited:")
            .expect("actionProcess has an onExited")
            + at;
        balanced(&src, onexit)
    };

    assert!(
        handler.contains("restoreDone"),
        "the successful-restore branch does not consult the report the engine \
         printed:\n{handler}"
    );
    assert!(
        src.contains("actionOut"),
        "Menu.qml no longer collects the action's stdout at all"
    );
}
