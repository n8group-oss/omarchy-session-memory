//! What the menu does with 2334 conversations.
//!
//! # Why this file exists
//!
//! `osm agents --json` answers with every resumable conversation on the
//! machine, and it is right to: it is an inventory, its other readers want
//! all of it, and the scan is cheap because it reads metadata rather than
//! transcripts. On the maintainer's machine that answer is **2334
//! conversations**, going back seven months, out of a 14 GB store.
//!
//! The menu bound a `Repeater` straight to it — no order, no cap, no label.
//! 2334 delegates, each a row of two labels and a button, built while the
//! popup opens; and, long before that cost matters, a list of 2334 rows in a
//! bar popup is not a list anyone can use. Worse, the order was the engine's
//! own, which is not recency: the conversation the user rebooted away from is
//! somewhere in the middle.
//!
//! Capping it introduces the defect this project keeps having to close from
//! the other side — a menu that draws twenty of 2334 and says nothing has
//! told the user that the other 2314 do not exist. So the cap is only safe
//! with the count beside it, and both are checked here.
//!
//! # What this actually verifies
//!
//! The ordering and the label are **extracted from `Menu.qml` and executed by
//! Qt's own QML engine**, not reimplemented in Rust — a Rust copy would only
//! ever test the copy. The dataset handed to them has the shape of the real
//! one: thousands of rows, a spread of `last_active` values, and rows whose
//! `last_active` is missing entirely.
//!
//! # What it cannot verify without Qt
//!
//! It needs a QML runtime (`/usr/lib/qt6/bin/qml`, or `qml6`/`qml` on
//! `PATH`), which a machine running the Omarchy shell has and this project's
//! CI containers do not. Where it is missing these tests say what they
//! skipped and return; the source-level rules in `tests/manifest.rs` — that
//! the `Repeater` is not bound to the uncapped list, that a cap is declared
//! and is a sane size, and that the total is stated — run everywhere.

mod common;

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// The engine's answer on the maintainer's machine, by the numbers they
/// measured: 2334 conversations, 15 of them active today.
const TOTAL: usize = 2334;

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

/// A brace-balanced region of the QML source starting at `from`.
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

fn qml_function(src: &str, name: &str) -> String {
    let at = src
        .find(&format!("function {name}("))
        .unwrap_or_else(|| panic!("Menu.qml defines {name}"));
    balanced(src, at)
}

/// The cap the menu declares, read out of the source rather than written
/// here: the number is the menu's decision to make, and a test that pinned a
/// literal would have to be edited to change it.
fn declared_cap(src: &str) -> usize {
    let decl = "readonly property int resumableCap:";
    let at = src
        .find(decl)
        .expect("Menu.qml declares a cap on the resumable list");
    src[at + decl.len()..]
        .trim_start()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .expect("resumableCap is a plain integer")
}

/// The functions the harness lifts out of the menu. All pure: they touch
/// nothing but their arguments and each other, which is what makes them
/// runnable inside a bare `QtObject`.
///
/// The filtering functions are here because the drawn list goes through them
/// even when nobody has typed anything: what this file checks is the menu with
/// an empty filter box, which is the state it opens in and the state most of
/// its life is spent in. `tests/qml_filter.rs` checks the other one.
const FUNCTIONS: [&str; 8] = [
    "countText",
    "lastActive",
    "newestFirst",
    "normalizedQuery",
    "containsQuery",
    "conversationMatches",
    "conversationsMatching",
    "resumableNote",
];

/// Run the menu's own ordering and labelling over `rows`, and report what a
/// user would see.
///
/// Returns the `native_id`s of the rows the `Repeater` would be given, in
/// order, and the sentence printed above them.
fn render(qml: &Path, rows: &[Value]) -> (Vec<String>, String) {
    let src = menu_source();
    let functions = FUNCTIONS
        .iter()
        .map(|n| qml_function(&src, n))
        .collect::<Vec<_>>()
        .join("\n");
    let cap = declared_cap(&src);

    let harness = format!(
        "import QtQml\n\
         QtObject {{\n\
         id: root\n\
         readonly property int resumableCap: {cap}\n\
         {functions}\n\
         Component.onCompleted: {{\n\
           var rows = {rows}\n\
           var matching = root.conversationsMatching(rows, \"\")\n\
           var shown = root.newestFirst(matching, root.resumableCap)\n\
           var ids = []\n\
           for (var i = 0; i < shown.length; i++) ids.push(String(shown[i].native_id))\n\
           var note = root.resumableNote(rows.length, matching.length, shown.length, \"\")\n\
           console.warn(\"BEGIN\" + JSON.stringify([ids, note]) + \"END\")\n\
           Qt.exit(0)\n\
         }}\n\
         }}\n",
        cap = cap,
        functions = functions,
        rows = serde_json::to_string(rows).unwrap(),
    );

    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("resumable.qml");
    std::fs::write(&file, harness).unwrap();
    let out = common::run_bounded(
        std::process::Command::new(qml)
            .env("QT_FORCE_STDERR_LOGGING", "1")
            .env("QT_QPA_PLATFORM", "offscreen")
            .arg(&file),
        std::time::Duration::from_secs(60),
    );
    let text =
        String::from_utf8_lossy(&out.stderr).into_owned() + &String::from_utf8_lossy(&out.stdout);
    let begin = text
        .find("BEGIN")
        .unwrap_or_else(|| panic!("the QML runtime printed no result: {text}"));
    let end = text.find("END").expect("a terminated result");
    serde_json::from_str::<(Vec<String>, String)>(&text[begin + 5..end])
        .expect("the harness printed a JSON pair")
}

/// A store the size of the maintainer's, oldest first — which is *not* the
/// order the menu must draw, so an implementation that simply takes the first
/// `cap` rows cannot pass.
///
/// The last five rows carry no `last_active` at all. That field is optional,
/// and an unknown age must not be allowed to displace a known recent
/// conversation off a capped list.
fn a_real_sized_store() -> Vec<Value> {
    let mut rows: Vec<Value> = (0..TOTAL - 5)
        .map(|i| {
            json!({
                "kind": "codex",
                "native_id": format!("c{i:05}"),
                "project_dir": "/home/u/p",
                // Ascending: row 0 is the oldest, row TOTAL-6 the newest.
                "last_active": 1_700_000_000i64 + i as i64,
            })
        })
        .collect();
    for i in 0..5 {
        rows.push(json!({
            "kind": "codex",
            "native_id": format!("undated{i}"),
            "project_dir": "/home/u/p",
        }));
    }
    rows
}

#[test]
fn the_menu_draws_the_newest_conversations_and_no_more_than_its_cap() {
    let Some(qml) = qml_runtime() else {
        eprintln!(
            "SKIPPED: no QML runtime found (looked at $OSM_QML, \
             /usr/lib/qt6/bin/qml, qml6 and qml on PATH). The source-level \
             rules in tests/manifest.rs still ran."
        );
        return;
    };
    let cap = declared_cap(&menu_source());
    let rows = a_real_sized_store();
    let (ids, _note) = render(&qml, &rows);

    assert_eq!(
        ids.len(),
        cap,
        "the menu drew {} of {TOTAL} conversations; its cap is {cap}",
        ids.len()
    );

    // The newest `cap` dated rows, newest first. `c02328` is the last dated
    // row in the fixture.
    let want: Vec<String> = (0..cap).map(|k| format!("c{:05}", TOTAL - 6 - k)).collect();
    assert_eq!(
        ids, want,
        "the menu drew the wrong conversations, or drew them in the wrong \
         order: the list a user opens after a reboot must start with what \
         they were last working on"
    );
    assert!(
        !ids.iter().any(|id| id.starts_with("undated")),
        "a conversation with no last_active displaced one whose age is \
         known: {ids:?}"
    );
}

#[test]
fn the_menu_says_how_many_conversations_there_really_are() {
    let Some(qml) = qml_runtime() else {
        eprintln!("SKIPPED: no QML runtime found; see tests/manifest.rs.");
        return;
    };
    let cap = declared_cap(&menu_source());
    let (_ids, note) = render(&qml, &a_real_sized_store());

    assert!(
        note.contains(&TOTAL.to_string()),
        "the menu drew {cap} of {TOTAL} conversations without saying how many \
         there are — a list that is quietly not the list is how a user \
         concludes their unfinished work is gone: {note:?}"
    );
    assert!(
        note.contains(&cap.to_string()),
        "the menu capped the list without saying it had: {note:?}"
    );
}

#[test]
fn a_list_that_fits_is_not_labelled_as_cut_short() {
    let Some(qml) = qml_runtime() else {
        eprintln!("SKIPPED: no QML runtime found; see tests/manifest.rs.");
        return;
    };
    let rows: Vec<Value> = (0..3)
        .map(|i| {
            json!({
                "kind": "claude",
                "native_id": format!("s{i}"),
                "last_active": 1_700_000_000i64 + i as i64,
            })
        })
        .collect();
    let (ids, note) = render(&qml, &rows);

    assert_eq!(ids, vec!["s2", "s1", "s0"], "newest first, always");
    assert!(
        note.contains('3') && !note.contains("showing"),
        "three conversations, all three drawn, and the menu still claimed it \
         was showing a subset: {note:?}"
    );
}

#[test]
fn an_empty_list_still_says_so() {
    let Some(qml) = qml_runtime() else {
        eprintln!("SKIPPED: no QML runtime found; see tests/manifest.rs.");
        return;
    };
    let (ids, note) = render(&qml, &[]);
    assert!(ids.is_empty());
    assert!(
        note.to_lowercase().contains("nothing"),
        "an empty list must say it is empty rather than print a bare zero: \
         {note:?}"
    );
}
