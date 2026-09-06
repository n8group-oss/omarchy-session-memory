//! The one line under "NEWEST SNAPSHOT" that says how capture is going.
//!
//! # Why this file exists
//!
//! The maintainer's panel read:
//!
//! > captures are fresh · the window placement for this capture could not be
//! > read; refusing to record a snapshot that would claim there is none
//!
//! Both halves came from the same `capture` block, and the second half was
//! 1.7 hours old. `last_success_at` was 14 seconds ago and
//! `consecutive_failures` was 0 — around a hundred captures had succeeded
//! since that error — but the line appended `last_error` unconditionally, so a
//! resolved failure was drawn beside "captures are fresh" as though it were
//! happening now. The engine never claimed it was current: `record_success`
//! in `src/health.rs` clears the failure streak and deliberately leaves
//! `last_error`/`last_error_at` in place, because an operator may still want
//! to know what went wrong. Deciding whether that record is history or a
//! condition is this panel's job, and it was not doing it.
//!
//! Presenting a resolved error as a current one is the same defect class this
//! plugin exists to avoid, pointed at the user's confidence rather than at
//! their data: it is alarming, it is contradictory, and it is not true.
//!
//! # What this actually verifies
//!
//! The `Text` that draws the line is **lifted verbatim out of `Menu.qml`,
//! instantiated by Qt, and read back** — its `text` and its colour — against a
//! `capture` block with the maintainer's own numbers in it. Nothing here
//! reimplements the rule; a Rust copy of the wording would only ever agree
//! with itself.
//!
//! The block is found by its `visible:` line rather than by an `id`, so this
//! measurement runs against the *broken* version of the file as well as the
//! fixed one. That is the only way to know it can fail.
//!
//! Four orderings are checked, because each is a different sentence and three
//! of them are ways of getting it wrong:
//!
//! * an error older than the newest success — history, and must read as
//!   history;
//! * an error newer than the newest success — a live problem, and must stay
//!   prominent;
//! * an error with no successful capture behind it at all — also live;
//! * an error with no time against it — neither claim can be made, and the
//!   line has to say so instead of picking one.
//!
//! # What it cannot verify without Qt
//!
//! It needs a QML runtime (`/usr/lib/qt6/bin/qml`, or `qml6`/`qml` on `PATH`),
//! which a machine running the Omarchy shell has and this project's CI
//! containers do not. Without one it says what it skipped and returns; the
//! source-level rule in `tests/manifest.rs` — that the line compares the two
//! timestamps at all — runs everywhere.

mod common;

use serde::Deserialize;
use serde_json::{json, Value};
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

/// A named function of the menu, or the empty string when it does not exist.
///
/// Tolerating absence is deliberate. The harness below has to build against a
/// `Menu.qml` that has not been fixed yet — one whose line is written inline
/// and calls none of these — because a check that cannot be run against the
/// broken file is a check nobody has seen fail.
fn qml_function_opt(src: &str, name: &str) -> String {
    match src.find(&format!("function {name}(")) {
        Some(at) => balanced(src, at),
        None => String::new(),
    }
}

/// The object declaration that contains `marker`, verbatim.
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
    let decl = balanced(src, name_start);
    assert!(
        decl.starts_with("Text"),
        "the capture line is no longer a Text: {}",
        &decl[..decl.len().min(60)]
    );
    decl
}

/// The line as the panel draws it: the sentence, and whether it is painted in
/// the alarm colour.
#[derive(Debug, Deserialize)]
struct Line {
    text: String,
    urgent: bool,
    visible: bool,
}

/// The `visible:` line of the capture Text — the one thing that is in the file
/// both before and after the fix, and so the only usable handle on the block.
const MARKER: &str = "root.status.capture !== undefined";

/// Instantiate that Text with `capture` as the engine's report, let Qt bind
/// it, and read back what it says.
fn draw(qml: &Path, capture: &Value) -> Line {
    let src = menu_source();
    let block = enclosing_block(&src, MARKER);
    // Everything the binding may reach for, in either version of the file.
    let functions = [
        "elide",
        "ago",
        "captureTime",
        "captureErrorAge",
        "captureErrorIsCurrent",
        "captureErrorNote",
        "captureLine",
        "captureIsAlarming",
    ]
    .iter()
    .map(|n| qml_function_opt(&src, n))
    .collect::<Vec<_>>()
    .join("\n  ");

    let status = json!({ "capture": capture });
    let harness = format!(
        r##"import QtQuick
import qs.Commons

Item {{
  id: root
  width: 420
  height: 200

  property color foreground: "#eeeeee"
  property color dim: "#999999"
  property color urgent: "#ff5555"
  property string fontFamily: "monospace"
  property var status: {status}
  {functions}

  Column {{
    id: column
    width: root.width

    {block}
  }}

  Timer {{
    interval: 120
    running: true
    repeat: false
    onTriggered: {{
      var item = column.children[0]
      console.warn("BEGIN" + JSON.stringify({{
        text: String(item.text),
        urgent: String(item.color) === String(root.urgent),
        visible: item.visible
      }}) + "END")
      Qt.exit(0)
    }}
  }}
}}
"##,
        status = serde_json::to_string(&status).unwrap(),
    );

    let tmp = tempfile::tempdir().unwrap();
    write_stub_commons(tmp.path());
    let file = tmp.path().join("captureline.qml");
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
        .unwrap_or_else(|| panic!("the QML runtime printed no line: {text}"));
    let end = text.find("END").expect("a terminated result");
    serde_json::from_str::<Line>(&text[begin + 5..end]).expect("the harness printed a line")
}

/// `Style` with plain numbers behind it. The real `qs.Commons` imports
/// `Quickshell`, whose plugin refuses to load outside the `quickshell` binary,
/// and none of the numbers matter to a sentence.
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

/// An arbitrary "now", so the fixtures below can be written as ages.
const NOW: i64 = 1_788_000_000;

/// The error the maintainer's panel was showing, word for word.
const ERROR: &str = "the window placement for this capture could not be read; \
                     refusing to record a snapshot that would claim there is none";

/// His numbers: the error 6154 seconds old, the newest success 14 seconds old,
/// no failing streak.
const ERROR_AGE: i64 = 6154;
const FRESH_AGE: i64 = 14;

fn capture(last_success_at: Option<i64>, last_error_at: Option<i64>, failures: u32) -> Value {
    let age = last_success_at.map(|at| NOW - at);
    json!({
        "last_success_at": last_success_at,
        "age_secs": age,
        "stale": age.is_none_or(|a| a > 900),
        "stale_after_secs": 900,
        "last_error": last_error_at.map(|_| ERROR),
        "last_error_at": last_error_at,
        "consecutive_failures": failures,
    })
}

/// The maintainer's panel, exactly: a fresh capture and an error from before
/// it.
///
/// This is the defect. The line must not read as though the placement were
/// unreadable right now — around a hundred captures have succeeded since — and
/// it must not be painted in the alarm colour either.
#[test]
fn an_error_older_than_the_newest_success_reads_as_history() {
    let Some(qml) = qml_runtime() else {
        eprintln!(
            "SKIPPED: no QML runtime found (looked at $OSM_QML, \
             /usr/lib/qt6/bin/qml, qml6 and qml on PATH). The source-level \
             rule in tests/manifest.rs still ran."
        );
        return;
    };
    let line = draw(
        &qml,
        &capture(Some(NOW - FRESH_AGE), Some(NOW - ERROR_AGE), 0),
    );
    assert!(line.visible, "the capture line was not drawn at all");

    // The pre-fix line, spelled out so that a run that reproduces the
    // maintainer's screen says so instead of just differing.
    assert!(
        !line.text.split(" · ").any(|part| part == ERROR),
        "the panel appended a 1.7-hour-old error as a bare clause beside \
         \"captures are fresh\", which is the maintainer's screen: {:?}",
        line.text
    );
    assert_eq!(
        line.text,
        format!(
            "captures are fresh · past error, 2h ago, with a successful capture since: {ERROR}"
        ),
        "a resolved error has to be marked as past and dated, never rendered \
         as the current state"
    );
    assert!(
        !line.urgent,
        "a resolved error painted the line in the alarm colour: {:?}",
        line.text
    );
}

/// The other ordering, which is a real problem and must stay one.
///
/// The newest capture attempt failed and nothing has succeeded since. Softened
/// into "past error" this would be the same defect pointed the other way — a
/// user told that a live failure is history.
#[test]
fn an_error_newer_than_the_last_success_stays_a_current_problem() {
    let Some(qml) = qml_runtime() else {
        eprintln!("SKIPPED: no QML runtime found; see tests/manifest.rs.");
        return;
    };
    let line = draw(
        &qml,
        &capture(Some(NOW - ERROR_AGE), Some(NOW - FRESH_AGE), 3),
    );
    assert_eq!(
        line.text,
        format!(
            "captures are stale · 3 failing in a row · current error, 14s ago, \
             newer than the last success: {ERROR}"
        ),
        "an error newer than the newest success is happening now and has to \
         read that way"
    );
    assert!(
        line.urgent,
        "a live capture failure was not painted in the alarm colour: {:?}",
        line.text
    );
}

/// No capture has ever succeeded, so there is nothing the error could be older
/// than.
///
/// `last_success_at` is null here, and a comparison against null is the easy
/// way to turn this case into "past error" — which would tell a user whose
/// engine has never once captured that the failure is behind them.
#[test]
fn an_error_with_no_success_behind_it_is_never_called_past() {
    let Some(qml) = qml_runtime() else {
        eprintln!("SKIPPED: no QML runtime found; see tests/manifest.rs.");
        return;
    };
    let line = draw(&qml, &capture(None, Some(NOW - FRESH_AGE), 1));
    assert_eq!(
        line.text,
        format!(
            "captures are stale · 1 failing in a row · current error, and no \
             capture has ever succeeded: {ERROR}"
        ),
        "with no successful capture on record the error cannot be history"
    );
    assert!(line.urgent, "{:?}", line.text);
}

/// An error with no time against it: neither claim is available.
///
/// `last_error_at` is nullable in the contract (`BarWidget.qml`'s
/// `captureFields` says so). Assuming it is old would hide a live failure;
/// assuming it is new would alarm over a resolved one. The line says what it
/// knows, which is that it does not know.
#[test]
fn an_undated_error_claims_neither_past_nor_present() {
    let Some(qml) = qml_runtime() else {
        eprintln!("SKIPPED: no QML runtime found; see tests/manifest.rs.");
        return;
    };
    let mut c = capture(Some(NOW - FRESH_AGE), Some(NOW - ERROR_AGE), 0);
    c["last_error_at"] = Value::Null;
    let line = draw(&qml, &c);
    assert_eq!(
        line.text,
        format!(
            "captures are fresh · error of unknown time, so whether it is \
             current is not known: {ERROR}"
        ),
        "an undated error must be reported as undated"
    );
    assert!(
        !line.urgent,
        "an error that may well be resolved raised the alarm colour: {:?}",
        line.text
    );
}

/// Nothing has ever gone wrong: the line is one clause and no more.
#[test]
fn a_clean_capture_record_says_only_that() {
    let Some(qml) = qml_runtime() else {
        eprintln!("SKIPPED: no QML runtime found; see tests/manifest.rs.");
        return;
    };
    let line = draw(&qml, &capture(Some(NOW - FRESH_AGE), None, 0));
    assert_eq!(line.text, "captures are fresh");
    assert!(!line.urgent, "{:?}", line.text);
}

/// A capture that failed in the same second as the last success.
///
/// `last_success_at` and `last_error_at` are epoch **seconds**. A capture
/// succeeds, the next one is triggered by the tmux hook it fires, and that one
/// fails — all inside one second, which on this machine is an ordinary
/// sequence rather than a contrived one. `consecutive_failures` is then 1 and
/// the two stamps are equal, so `last_error_at > last_success_at` is false and
/// the line called a failure that is happening right now "past error … with a
/// successful capture since".
///
/// The failure streak is the authority. `record_success` in `src/health.rs`
/// clears it, so a non-zero streak means every capture since the last success
/// has failed and the recorded error is the current condition — whatever the
/// second-resolution stamps can and cannot order.
///
/// The sentence must also not claim more than it knows. With equal stamps the
/// error is not "newer than the last success", so that clause is not the one
/// to write.
#[test]
fn a_failure_in_the_same_second_as_the_last_success_is_still_current() {
    let Some(qml) = qml_runtime() else {
        eprintln!("SKIPPED: no QML runtime found; see tests/manifest.rs.");
        return;
    };
    let at = NOW - FRESH_AGE;
    let line = draw(&qml, &capture(Some(at), Some(at), 1));

    assert!(
        !line.text.contains("past error"),
        "a capture is failing right now — one failure since the last success — \
         and the line called it history because the two stamps landed in the \
         same second: {:?}",
        line.text
    );
    assert!(
        !line.text.contains("with a successful capture since"),
        "nothing has succeeded since this error: {:?}",
        line.text
    );
    assert_eq!(
        line.text,
        format!(
            "captures are fresh · 1 failing in a row · current error, 14s ago, \
             with captures failing since the last success: {ERROR}"
        ),
        "the streak says the error is current; the stamps cannot say it is \
         newer than the success, so the sentence must not either"
    );
    assert!(
        line.urgent,
        "a capture failing right now was not painted in the alarm colour: {:?}",
        line.text
    );
}
