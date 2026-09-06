//! Finding one conversation among 2447.
//!
//! # Why this file exists
//!
//! The menu draws the newest 25 conversations and says how many there are.
//! That is honest and it is not usable: the maintainer has 2447 resumable
//! conversations and 7 recorded sessions, and the one they want after a reboot
//! is very often not in the newest 25 — it is the thing they were working on
//! last Thursday. Their words: *"There should be also option to search or
//! filter session."*
//!
//! A filter is what makes the cap survivable. It is also the place where this
//! plugin's one rule is easiest to break, because the count line now has three
//! numbers to reconcile — how many exist, how many match, how many are drawn —
//! and a line that reports any two of them lets the user believe something
//! false. "There are none", "none match what you typed" and "there are more
//! than are shown" have to stay three different sentences.
//!
//! # What this actually verifies
//!
//! The matching, the ordering and the wording are **extracted from `Menu.qml`
//! and executed by Qt's own QML engine**. A Rust reimplementation would only
//! ever test itself.
//!
//! # What it cannot verify without Qt
//!
//! It needs a QML runtime (`/usr/lib/qt6/bin/qml`, or `qml6`/`qml` on `PATH`),
//! which a machine running the Omarchy shell has and this project's CI
//! containers do not. Where it is missing these tests say what they skipped;
//! the source-level rules in `tests/manifest.rs` — that the drawn list comes
//! from the filtered one, that the search box is the shell's own component,
//! and that typing into it starts no process — run everywhere.

mod common;

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// The engine's answer on the maintainer's machine: 2334 codex conversations
/// and 113 claude ones.
const CODEX: usize = 2334;
const CLAUDE: usize = 113;

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

fn qml_function(src: &str, name: &str) -> String {
    let at = src
        .find(&format!("function {name}("))
        .unwrap_or_else(|| panic!("Menu.qml defines {name}"));
    balanced(src, at)
}

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

/// The menu's own filtering, ordering and labelling. All pure — they touch
/// nothing but their arguments and each other.
const FUNCTIONS: [&str; 10] = [
    "countText",
    "lastActive",
    "newestFirst",
    "normalizedQuery",
    "containsQuery",
    "conversationMatches",
    "sessionMatches",
    "conversationsMatching",
    "sessionsMatching",
    "resumableNote",
];

/// Evaluate `script` with the menu's functions in scope as `root.*`, and
/// return whatever it assigns to `answer`.
fn evaluate(qml: &Path, script: &str) -> Value {
    let src = menu_source();
    let mut names: Vec<&str> = FUNCTIONS.to_vec();
    names.push("sessionsNote");
    let functions = names
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
           var answer = null\n\
           {script}\n\
           console.warn(\"BEGIN\" + JSON.stringify(answer) + \"END\")\n\
           Qt.exit(0)\n\
         }}\n\
         }}\n"
    );

    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("filter.qml");
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
    serde_json::from_str::<Value>(&text[begin + 5..end]).expect("the harness printed JSON")
}

/// What the conversation list looks like for `query`: the ids drawn, in
/// order, and the sentence printed above them.
fn conversations(qml: &Path, rows: &[Value], query: &str) -> (Vec<String>, String) {
    let script = format!(
        "var rows = {rows}\n\
         var query = {query}\n\
         var matching = root.conversationsMatching(rows, query)\n\
         var shown = root.newestFirst(matching, root.resumableCap)\n\
         var ids = []\n\
         for (var i = 0; i < shown.length; i++) ids.push(String(shown[i].native_id))\n\
         answer = [ids, root.resumableNote(rows.length, matching.length, shown.length, query)]\n",
        rows = serde_json::to_string(rows).unwrap(),
        query = serde_json::to_string(query).unwrap(),
    );
    let value = evaluate(qml, &script);
    serde_json::from_value(value).expect("a pair of ids and a note")
}

/// The same for the session list, which has no cap: the names drawn and the
/// sentence beside them.
fn sessions(qml: &Path, rows: &[Value], query: &str) -> (Vec<String>, String) {
    let script = format!(
        "var rows = {rows}\n\
         var query = {query}\n\
         var matching = root.sessionsMatching(rows, query)\n\
         var names = []\n\
         for (var i = 0; i < matching.length; i++) names.push(String(matching[i].name))\n\
         answer = [names, root.sessionsNote(rows.length, matching.length, query)]\n",
        rows = serde_json::to_string(rows).unwrap(),
        query = serde_json::to_string(query).unwrap(),
    );
    let value = evaluate(qml, &script);
    serde_json::from_value(value).expect("a pair of names and a note")
}

/// A store the shape of the maintainer's: 2334 codex conversations under one
/// project and 113 claude ones under another, oldest first, plus a single old
/// conversation in a project of its own — the one they are trying to find.
fn a_real_sized_store() -> Vec<Value> {
    let mut rows: Vec<Value> = (0..CODEX)
        .map(|i| {
            json!({
                "kind": "codex",
                "native_id": format!("c{i:05}"),
                "project_dir": "/home/user/projects/process",
                "last_active": 1_700_000_000i64 + i as i64,
            })
        })
        .collect();
    for i in 0..CLAUDE {
        rows.push(json!({
            "kind": "claude",
            "native_id": format!("k{i:05}"),
            "project_dir": "/home/user/projects/n8group-oss/osm-plan4",
            "last_active": 1_700_000_000i64 + i as i64,
        }));
    }
    // Older than everything above, and therefore nowhere near the newest 25.
    rows.push(json!({
        "kind": "claude",
        "native_id": "0198f2ac-1c4e-7a3b-9f11-2b6d8c4e5a17",
        "project_dir": "/home/user/projects/widgets",
        "last_active": 1_600_000_000i64,
    }));
    // The one the search is for when the user remembers *what* they were
    // doing and not where: an old conversation in a project full of them,
    // told apart from its 2334 neighbours by nothing but its title.
    rows.push(json!({
        "kind": "claude",
        "native_id": "5c0a17ed-4c6e-4f27-9d3a-6b1f0c2e77aa",
        "project_dir": "/home/user/projects/n8group-oss/osm-placefix",
        "title": "Stream Deck lock-notify bug",
        "title_source": "agent",
        "last_active": 1_600_000_001i64,
    }));
    rows
}

fn the_maintainers_sessions() -> Vec<Value> {
    [
        "dev",
        "process",
        "osm-plan4",
        "notes",
        "widgets",
        "ops",
        "scratch",
    ]
    .iter()
    .map(|name| {
        json!({ "name": name, "windows": 3, "panes": 9, "agents": 2,
                        "goal": null, "conversations": [] })
    })
    .collect()
}

/// The same seven sessions, as the engine answers now: each with the goal of
/// its newest conversation and the conversations behind it.
///
/// Two of them are what the search below is for. `dev` and `scratch` are names
/// that say nothing — on the maintainer's machine every session is called
/// something like that — and what tells them apart is that one is about the
/// Stream Deck and the other about a restore race.
fn the_maintainers_sessions_with_goals() -> Vec<Value> {
    vec![
        json!({
            "name": "dev", "windows": 3, "panes": 9, "agents": 2,
            "goal": {
                "title": "Stream Deck lock-notify bug",
                "source": "agent",
                "kind": "claude",
                "native_id": "5c0a17ed-4c6e-4f27-9d3a-6b1f0c2e77aa",
            },
            "conversations": [
                { "kind": "claude", "native_id": "5c0a17ed-4c6e-4f27-9d3a-6b1f0c2e77aa",
                  "title": "Stream Deck lock-notify bug", "title_source": "agent",
                  "window_idx": 0, "pane_idx": 0, "last_active": 1_700_000_000i64 },
                { "kind": "codex", "native_id": "c00000",
                  "title": "Certify the one-way sync in proj-alpha", "title_source": "first_prompt",
                  "window_idx": 1, "pane_idx": 0, "last_active": 1_699_000_000i64 },
            ],
        }),
        json!({
            "name": "scratch", "windows": 1, "panes": 2, "agents": 1,
            "goal": {
                "title": "Fix the restore race in osm",
                "source": "first_prompt",
                "kind": "codex",
                "native_id": "c00001",
            },
            "conversations": [
                { "kind": "codex", "native_id": "c00001",
                  "title": "Fix the restore race in osm", "title_source": "first_prompt",
                  "window_idx": 0, "pane_idx": 1, "last_active": 1_700_000_500i64 },
            ],
        }),
        json!({
            "name": "notes", "windows": 1, "panes": 1, "agents": 0,
            "goal": null, "conversations": [],
        }),
    ]
}

/// The whole point of the box: it reaches past the cap.
///
/// The cap draws the newest 25. A conversation from seven months ago is not
/// in them and never will be, so without a filter the only way to a specific
/// old conversation is to not use this menu.
#[test]
fn a_search_reaches_a_conversation_far_older_than_the_cap() {
    let Some(qml) = qml_runtime() else {
        eprintln!(
            "SKIPPED: no QML runtime found (looked at $OSM_QML, \
             /usr/lib/qt6/bin/qml, qml6 and qml on PATH). The source-level \
             rules in tests/manifest.rs still ran."
        );
        return;
    };
    let rows = a_real_sized_store();
    let (unfiltered, _) = conversations(&qml, &rows, "");
    assert!(
        !unfiltered.contains(&"0198f2ac-1c4e-7a3b-9f11-2b6d8c4e5a17".to_string()),
        "the fixture is wrong: the conversation being searched for is already \
         in the newest {}, so finding it proves nothing",
        unfiltered.len()
    );

    let (found, note) = conversations(&qml, &rows, "widgets");
    assert_eq!(
        found,
        vec!["0198f2ac-1c4e-7a3b-9f11-2b6d8c4e5a17"],
        "a search for a project directory did not reach the one conversation \
         in it: {note}"
    );
}

/// The three numbers, and the three sentences that keep them apart.
#[test]
fn the_count_line_tells_the_three_states_apart() {
    let Some(qml) = qml_runtime() else {
        eprintln!("SKIPPED: no QML runtime found; see tests/manifest.rs.");
        return;
    };
    let rows = a_real_sized_store();
    let total = rows.len();
    let cap = declared_cap(&menu_source());

    // More matches than the cap draws: every number has to be there, or the
    // user reads 25 and believes that is all there is.
    let (ids, capped) = conversations(&qml, &rows, "process");
    assert_eq!(ids.len(), cap);
    for number in [CODEX.to_string(), total.to_string(), cap.to_string()] {
        assert!(
            capped.contains(&number),
            "the line above a filtered, capped list must say how many exist \
             ({total}), how many match ({CODEX}) and how many are drawn \
             ({cap}); it said {capped:?}"
        );
    }

    // Fewer matches than the cap: nothing is being withheld, so nothing may
    // claim to be.
    let (ids, uncapped) = conversations(&qml, &rows, "widgets");
    assert_eq!(ids.len(), 1);
    assert!(
        !uncapped.contains("showing"),
        "one match, one row drawn, and the line still said it was showing a \
         subset: {uncapped:?}"
    );
    assert!(
        uncapped.contains(&total.to_string()),
        "a filtered list must still say how many conversations exist, or the \
         filter looks like it emptied the machine: {uncapped:?}"
    );

    // No matches at all: different from an empty store, and it must read
    // differently.
    let (ids, none) = conversations(&qml, &rows, "no-such-project");
    assert!(ids.is_empty());
    assert!(
        none.contains(&total.to_string()),
        "\"nothing matched\" and \"there is nothing\" must not be the same \
         sentence: {none:?}"
    );
    let (_, empty_store) = conversations(&qml, &[], "no-such-project");
    assert_ne!(
        none, empty_store,
        "a filter that matched nothing said exactly what a machine with no \
         conversations at all says"
    );
    assert!(
        empty_store.to_lowercase().contains("nothing"),
        "an empty store must still say it is empty: {empty_store:?}"
    );
}

/// With the box empty, every word is the one that was there before.
///
/// The filter is an addition. A user who never types in it must not see the
/// menu's wording change under them.
#[test]
fn an_empty_box_leaves_the_existing_wording_alone() {
    let Some(qml) = qml_runtime() else {
        eprintln!("SKIPPED: no QML runtime found; see tests/manifest.rs.");
        return;
    };
    let cap = declared_cap(&menu_source());
    let rows = a_real_sized_store();
    let total = rows.len();

    let (_, note) = conversations(&qml, &rows, "");
    assert_eq!(
        note,
        format!(
            "{total} conversations waiting to be resumed — showing the {cap} \
             most recently active."
        )
    );

    let three: Vec<Value> = (0..3)
        .map(|i| json!({ "kind": "claude", "native_id": format!("s{i}"), "last_active": 1_700_000_000i64 + i as i64 }))
        .collect();
    let (_, note) = conversations(&qml, &three, "");
    assert_eq!(note, "3 conversations waiting to be resumed.");

    let (_, note) = conversations(&qml, &[], "");
    assert_eq!(note, "Nothing waiting to be resumed.");

    // Whitespace is not a query. A stray space must not turn the wording into
    // the filtered form and report "0 of 2448 match".
    let (_, note) = conversations(&qml, &rows, "   ");
    assert_eq!(
        note,
        format!(
            "{total} conversations waiting to be resumed — showing the {cap} \
             most recently active."
        )
    );
}

/// The filter matches what the row shows: the path, the kind, and the id.
#[test]
fn the_filter_matches_what_the_row_puts_on_screen() {
    let Some(qml) = qml_runtime() else {
        eprintln!("SKIPPED: no QML runtime found; see tests/manifest.rs.");
        return;
    };
    let rows = a_real_sized_store();

    let (by_kind, _) = conversations(&qml, &rows, "claude");
    assert!(
        by_kind
            .iter()
            .all(|id| id.starts_with('k') || id.contains('-')),
        "filtering by kind returned a codex conversation: {by_kind:?}"
    );
    assert!(!by_kind.is_empty(), "filtering by kind returned nothing");

    let (by_id, _) = conversations(&qml, &rows, "0198f2ac");
    assert_eq!(
        by_id,
        vec!["0198f2ac-1c4e-7a3b-9f11-2b6d8c4e5a17"],
        "a search for the visible head of an id did not find it"
    );

    let (upper, _) = conversations(&qml, &rows, "WIDGETS");
    assert_eq!(
        upper,
        vec!["0198f2ac-1c4e-7a3b-9f11-2b6d8c4e5a17"],
        "the filter is case sensitive, so a user has to know how their own \
         directories are spelled"
    );
}

/// A substring, and nothing cleverer.
///
/// Nobody typing into a one-line box in a bar popup means `.` as "any
/// character". A filter that quietly is a pattern language hides rows for
/// reasons the user cannot see.
#[test]
fn the_filter_is_a_plain_substring_not_a_pattern() {
    let Some(qml) = qml_runtime() else {
        eprintln!("SKIPPED: no QML runtime found; see tests/manifest.rs.");
        return;
    };
    let rows = vec![json!({
        "kind": "codex",
        "native_id": "abc",
        "project_dir": "/home/u/abc",
        "last_active": 1_700_000_000i64,
    })];
    for pattern in ["a.c", "a*c", "^abc$", "ab|xy"] {
        let (found, _) = conversations(&qml, &rows, pattern);
        assert!(
            found.is_empty(),
            "{pattern:?} was treated as a pattern rather than as the \
             characters the user typed: {found:?}"
        );
    }
    let (found, _) = conversations(&qml, &rows, "abc");
    assert_eq!(found, vec!["abc"]);
}

/// Filtering selects; it does not reorder.
#[test]
fn filtering_leaves_the_order_alone() {
    let Some(qml) = qml_runtime() else {
        eprintln!("SKIPPED: no QML runtime found; see tests/manifest.rs.");
        return;
    };
    let rows = a_real_sized_store();
    let cap = declared_cap(&menu_source());
    let (matched, _) = conversations(&qml, &rows, "osm-plan4");

    let want: Vec<String> = (0..cap)
        .map(|k| format!("k{:05}", CLAUDE - 1 - k))
        .collect();
    assert_eq!(
        matched, want,
        "the matches must be the newest ones, newest first — the same rule \
         the unfiltered list follows"
    );
}

/// The session list filters too, and says how many of how many matched.
#[test]
fn the_session_list_filters_and_counts() {
    let Some(qml) = qml_runtime() else {
        eprintln!("SKIPPED: no QML runtime found; see tests/manifest.rs.");
        return;
    };
    let rows = the_maintainers_sessions();

    let (all, note) = sessions(&qml, &rows, "");
    assert_eq!(all.len(), 7, "an empty box hides no session");
    assert_eq!(
        note, "",
        "with an empty box the session list says exactly what it said before, \
         which is nothing extra: {note:?}"
    );

    let (matched, note) = sessions(&qml, &rows, "PRO");
    assert_eq!(
        matched,
        vec!["process"],
        "session names match by substring, \
         case-insensitively"
    );
    assert!(
        note.contains('1') && note.contains('7'),
        "a filtered session list must say how many of how many matched, or \
         six recorded sessions look like they were never recorded: {note:?}"
    );

    let (none, note) = sessions(&qml, &rows, "zzz");
    assert!(none.is_empty());
    assert!(
        note.contains('7'),
        "\"no session matches\" must still say how many sessions there are: \
         {note:?}"
    );
}

/// The search reaches a conversation by what it is *about*.
///
/// This is the reason the filter had to grow: a project directory narrows
/// 2495 conversations to the hundreds in one project, and after that the only
/// thing that tells them apart is the title. Searching for it must reach past
/// the cap exactly as searching for a path does.
#[test]
fn a_search_matches_what_a_conversation_is_about() {
    let Some(qml) = qml_runtime() else {
        eprintln!(
            "SKIPPED: no QML runtime found (looked at $OSM_QML, \
             /usr/lib/qt6/bin/qml, qml6 and qml on PATH). The source-level \
             rules in tests/manifest.rs still ran."
        );
        return;
    };
    let rows = a_real_sized_store();
    let (unfiltered, _) = conversations(&qml, &rows, "");
    assert!(
        !unfiltered.contains(&"5c0a17ed-4c6e-4f27-9d3a-6b1f0c2e77aa".to_string()),
        "the fixture is wrong: the conversation being searched for is already \
         in the newest {}, so finding it proves nothing",
        unfiltered.len()
    );

    let (found, note) = conversations(&qml, &rows, "lock-notify");
    assert_eq!(
        found,
        vec!["5c0a17ed-4c6e-4f27-9d3a-6b1f0c2e77aa"],
        "a search for words that appear only in a conversation's title did not \
         find it: {note}"
    );

    // And case-insensitively, like every other field the box matches.
    let (upper, _) = conversations(&qml, &rows, "STREAM DECK");
    assert_eq!(upper, vec!["5c0a17ed-4c6e-4f27-9d3a-6b1f0c2e77aa"]);
}

/// A session is found by what it is about, too.
///
/// `dev`, `scratch`, `ops`: the names say nothing, and on a machine with eight
/// of them the goal is the only thing that does. The goal and the
/// conversations behind it are both on screen — the second behind the row's
/// own detail button — so both are matched.
#[test]
fn a_session_matches_on_its_goal_and_on_the_conversations_behind_it() {
    let Some(qml) = qml_runtime() else {
        eprintln!("SKIPPED: no QML runtime found; see tests/manifest.rs.");
        return;
    };
    let rows = the_maintainers_sessions_with_goals();

    let (by_goal, note) = sessions(&qml, &rows, "lock-notify");
    assert_eq!(
        by_goal,
        vec!["dev"],
        "a session whose goal says \"Stream Deck lock-notify bug\" was not \
         found by searching for it: {note}"
    );

    let (by_conversation, _) = sessions(&qml, &rows, "proj-alpha");
    assert_eq!(
        by_conversation,
        vec!["dev"],
        "the conversations behind a session's goal are on screen when the row \
         is opened, so a search must reach them"
    );

    let (by_name, _) = sessions(&qml, &rows, "scratch");
    assert_eq!(by_name, vec!["scratch"], "matching by name still works");

    let (none, note) = sessions(&qml, &rows, "no-such-thing");
    assert!(none.is_empty());
    assert!(
        note.contains('3'),
        "\"no session matches\" must still say how many sessions there are: \
         {note}"
    );
}
