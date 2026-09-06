//! The widget's idea of a status report, checked against the engine's.
//!
//! `BarWidget.qml` refuses to publish a response that is not a complete
//! protocol-1 status report. That refusal is only safe if the shape it insists
//! on is the shape `osm status --json` actually produces: a validator that is
//! one field stricter than the engine turns every healthy machine into "the
//! engine did not return status JSON", which is the same defect as the one it
//! was written to fix, pointed the other way.
//!
//! Nothing else keeps the two in step. The contract lives in `src/ipc.rs` as
//! `StatusReport` and in `BarWidget.qml` as `requiredFields`, and the two are
//! written in different languages by different hands.
//!
//! So: run the real engine, take its real output, and run the widget's own
//! validator functions over it — extracted from the QML and executed by Qt's
//! engine, never reimplemented here. Then mutate that output the ways a
//! partial or newer engine would, and require each mutation to be rejected
//! with a message naming the field.
//!
//! # What this cannot do without Qt
//!
//! It needs a QML runtime (`/usr/lib/qt6/bin/qml`, or `qml6`/`qml` on `PATH`),
//! which a machine running the Omarchy shell has and this project's CI
//! containers do not. Without one it prints what it skipped and returns; the
//! source-level rules in `tests/manifest.rs` run everywhere.

mod common;

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

/// What `osm status --json` prints on this machine, right now.
///
/// The exit status is deliberately not asserted: `status` reports a refusal
/// *inside* its JSON — an unsupported or unreachable tmux is a status report,
/// not a missing engine — and the shape of that report is the subject here.
fn real_status() -> Value {
    let env = common::Env::new("qmlshape");
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_osm"))
        .env("XDG_STATE_HOME", env.dir.path().join("state"))
        .env("XDG_CONFIG_HOME", env.dir.path().join("config"))
        .args(["--socket", &env.socket, "status", "--json"])
        .output()
        .expect("run osm status --json");
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "osm status --json did not print JSON ({e}): {}",
            String::from_utf8_lossy(&out.stdout)
        )
    })
}

fn qml_source() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("BarWidget.qml");
    std::fs::read_to_string(path).expect("BarWidget.qml exists")
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
        .unwrap_or_else(|| panic!("BarWidget.qml defines {name}"));
    balanced(src, at)
}

/// Every contract declaration in the widget, verbatim.
///
/// Found by shape rather than by a list written here, so a nested contract
/// added to the widget is picked up by this test without anyone remembering
/// to add it — the failure mode this whole file exists to close is a
/// validator and a test that drifted apart.
fn contract_properties(src: &str) -> String {
    let mut out = Vec::new();
    let mut from = 0usize;
    while let Some(rel) = src[from..].find(CONTRACT_DECL) {
        let at = from + rel;
        let name_at = at + CONTRACT_DECL.len();
        let name: String = src[name_at..]
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if name.ends_with("Fields") {
            // The declaration is wrapped in parentheses, which `balanced`
            // stops inside; put the closing one back.
            out.push(format!("{})", balanced(src, at)));
        }
        from = name_at.max(at + 1);
    }
    assert!(
        out.iter().any(|d| d.contains("requiredFields")),
        "BarWidget.qml declares no requiredFields"
    );
    out.join("\n")
}

const CONTRACT_DECL: &str = "readonly property var ";

/// The validator functions the harness lifts out of the widget.
///
/// Listed rather than discovered because only these are pure: the rest of
/// `BarWidget.qml` reaches for `Process`, `Timer` and the panel's `Style`,
/// none of which exist in a bare `QtObject`. A name absent from the source is
/// skipped rather than fatal, so this list can describe the validators the
/// widget *should* have without the harness refusing to run against one that
/// does not have them yet.
const VALIDATORS: [&str; 7] = [
    "typeName",
    "readableFault",
    "protocolFault",
    "fieldsFault",
    "shapeFault",
    "sessionFault",
    "snapshotFault",
];

/// Run the widget's own validators over each named response.
///
/// Returns `""` for one the widget would publish, and the refusal it would
/// record for one it would not — the same three checks `applyProbe` makes, in
/// the same order.
fn verdicts(qml: &Path, cases: &[(&str, Value)]) -> Vec<(String, String)> {
    let src = qml_source();
    let functions = VALIDATORS
        .iter()
        .filter(|n| src.contains(&format!("function {n}(")))
        .map(|n| qml_function(&src, n))
        .collect::<Vec<_>>()
        .join("\n");

    let payload: Vec<Value> = cases
        .iter()
        .map(|(name, body)| json!({ "name": name, "body": body.to_string() }))
        .collect();

    let harness = format!(
        "import QtQml\n\
         QtObject {{\n\
         id: root\n\
         readonly property int supportedProtocol: 1\n\
         {fields}\n\
         {functions}\n\
         Component.onCompleted: {{\n\
           var cases = {payload}\n\
           var out = []\n\
           for (var i = 0; i < cases.length; i++) {{\n\
             var v = null\n\
             try {{ v = JSON.parse(cases[i].body) }} catch (e) {{ v = null }}\n\
             var fault = v === null ? \"the output was not JSON\" : root.readableFault(v)\n\
             if (fault === \"\") fault = root.protocolFault(v)\n\
             if (fault === \"\") fault = root.shapeFault(v)\n\
             out.push([cases[i].name, fault])\n\
           }}\n\
           console.warn(\"BEGIN\" + JSON.stringify(out) + \"END\")\n\
           Qt.exit(0)\n\
         }}\n\
         }}\n",
        fields = contract_properties(&src),
        functions = functions,
        payload = serde_json::to_string(&payload).unwrap(),
    );

    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("shape.qml");
    std::fs::write(&file, harness).unwrap();
    // The bound scales with the work. A flat 30s was enough for the
    // protocol-1 contract as it stood; adding `goal` and `conversations`
    // multiplied the mutants this generates, and the harness then took ~39s
    // and was killed one run in three — reported as "a terminated result",
    // which named the symptom and hid the cause. CI has no Qt runtime, so
    // this test skips there and the flake was invisible to it.
    // Generous on purpose. This bound exists only to stop a wedged QML
    // runtime hanging the suite forever; it is not a performance assertion,
    // and every time it has been tight it has cost a real signal — the run
    // is killed part-way and the missing terminator reads as a rejected
    // response. A machine also running a cargo build has taken >4x the
    // unloaded time, so the headroom is deliberate.
    let budget = std::time::Duration::from_secs(120)
        + std::time::Duration::from_millis(400) * u32::try_from(cases.len()).unwrap_or(u32::MAX);
    let out = common::run_bounded(
        std::process::Command::new(qml)
            .env("QT_FORCE_STDERR_LOGGING", "1")
            .env("QT_QPA_PLATFORM", "offscreen")
            .arg(&file),
        budget,
    );
    let text =
        String::from_utf8_lossy(&out.stderr).into_owned() + &String::from_utf8_lossy(&out.stdout);
    let begin = text
        .find("BEGIN")
        .unwrap_or_else(|| panic!("the QML runtime printed no result: {text}"));
    let end = text.find("END").unwrap_or_else(|| {
        panic!(
            "the QML harness did not finish within {budget:?} while checking {} cases: it was \
             killed part-way, so this is a bound that no longer fits the contract rather than \
             a widget that rejected something. Output so far: {text}",
            cases.len()
        )
    });
    serde_json::from_str::<Vec<(String, String)>>(&text[begin + 5..end])
        .expect("the harness printed a JSON array")
}

#[test]
fn the_widget_accepts_what_the_engine_prints_and_nothing_less() {
    let Some(qml) = qml_runtime() else {
        eprintln!(
            "SKIPPED: no QML runtime found (looked at $OSM_QML, \
             /usr/lib/qt6/bin/qml, qml6 and qml on PATH). The source-level \
             rules in tests/manifest.rs still ran."
        );
        return;
    };

    let real = real_status();
    let object = real.as_object().expect("status is a JSON object").clone();

    let mut cases: Vec<(&str, Value)> = vec![
        ("the engine's own output", real.clone()),
        // The response from the finding: valid JSON, a protocol this plugin
        // speaks, and not a status report.
        (
            "a response with only a protocol and ready",
            json!({"protocol_version": 1, "ready": true}),
        ),
        ("a newer protocol", {
            let mut v = object.clone();
            v.insert("protocol_version".into(), json!(2));
            Value::Object(v)
        }),
        ("an array", json!([1, 2, 3])),
        ("a bare number", json!(7)),
    ];

    // Every field of the contract, dropped one at a time. Each is a thing the
    // menu renders, and each absence would otherwise render as a fact.
    let contract = [
        "engine_version",
        "ready",
        "capture",
        "database",
        "tmux",
        "agents",
        "sessions",
        "snapshot",
    ];
    let dropped: Vec<(String, Value)> = contract
        .iter()
        .map(|field| {
            let mut v = object.clone();
            v.remove(*field);
            (format!("no {field}"), Value::Object(v))
        })
        .collect();
    for (name, body) in &dropped {
        cases.push((name.as_str(), body.clone()));
    }

    let results = verdicts(&qml, &cases);
    let verdict = |name: &str| -> String {
        results
            .iter()
            .find(|(n, _)| n == name)
            .unwrap_or_else(|| panic!("no verdict for {name}"))
            .1
            .clone()
    };

    assert_eq!(
        verdict("the engine's own output"),
        "",
        "the widget rejects what the engine actually prints — every healthy \
         machine would render as 'the engine did not return status JSON'"
    );

    for (name, _) in cases.iter().skip(1) {
        assert!(
            !verdict(name).is_empty(),
            "the widget accepts `{name}` as a complete status report"
        );
    }

    // The refusal has to name the field, or the user gets "something is wrong".
    for field in contract {
        let said = verdict(&format!("no {field}"));
        assert!(
            said.contains(field),
            "dropping {field} is refused with `{said}`, which does not name it"
        );
    }
}

// ---------------------------------------------------------------------------
// Every nested field, both ways it can go wrong.
//
// The test above drops *top-level* fields only, so it never asked what the
// widget does with `"capture": {}`. The answer was: publish it. `typeof {}`
// is `"object"`, the top-level check was satisfied, and `Menu.qml` then read
// the absent `capture.stale` as "captures are fresh" and the absent
// `database.snapshots` as a count — a machine whose captures had been failing
// for a week rendered as a healthy one with nothing recorded. Unknown shown
// as no, which is the defect class this plugin exists to not have.
//
// So: take a response the engine really printed, with every nested field
// actually filled in, and break one field at a time. Two mutants per field,
// because a validator can be blind to each independently — the field removed
// (a partial or older engine) and the field retyped (a newer one that changed
// a shape). Each must be refused, and the refusal must name the field.
// ---------------------------------------------------------------------------

/// The v3 database a user upgrading from an older osm has. Opening it makes
/// this build preserve it and report `database.preserved`, which is the only
/// way that sub-object is ever populated.
fn write_legacy_db(path: &std::path::Path) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch(
        "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
         CREATE TABLE snapshots (
           id INTEGER PRIMARY KEY, taken_at INTEGER NOT NULL,
           boot_id TEXT NOT NULL, reason TEXT NOT NULL, state TEXT NOT NULL);
         INSERT INTO snapshots (id, taken_at, boot_id, reason, state)
           VALUES (1, 100, 'boot-a', 'manual', 'complete');
         INSERT INTO meta (key, value) VALUES ('schema_version', '3');",
    )
    .unwrap();
}

/// A status report with the whole contract filled in, printed by the engine.
///
/// [`real_status`] describes an empty machine, where most of the contract is
/// `null` and `[]`: no snapshot, no sessions, no preserved database. A field
/// that is not in a response cannot be mutated, so a walk over that one would
/// silently check a third of the shape and report success. This gives the
/// engine the three things that populate the rest — a tmux server with
/// sessions in it, a snapshot taken of them, and a database from an older
/// schema to push aside.
///
/// The `Env` is returned with the value because dropping it kills the tmux
/// server and deletes the state directory.
///
/// `label` names this fixture's own tmux server. Two tests sharing one built
/// the same session on the same server and the second failed with `duplicate
/// session: dev` — a fixture collision, on every machine that has a QML
/// runtime to run these against.
fn populated_status(label: &str) -> (common::Env, Value) {
    let env = common::Env::new(label);
    write_legacy_db(&env.dir.path().join("state/osm/state.db"));

    env.tmux(&["new-session", "-d", "-s", "dev", "-c", "/tmp"]);
    env.tmux(&["split-window", "-t", "=dev:", "-c", "/tmp"]);
    env.tmux(&["new-session", "-d", "-s", "notes", "-c", "/tmp"]);
    env.osm(&["snapshot", "--reason", "test"]);

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_osm"))
        .env("XDG_STATE_HOME", env.dir.path().join("state"))
        .env("XDG_CONFIG_HOME", env.dir.path().join("config"))
        .args(["--socket", &env.socket, "status", "--json"])
        .output()
        .expect("run osm status --json");
    let value: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "osm status --json did not print JSON ({e}): {}",
            String::from_utf8_lossy(&out.stdout)
        )
    });
    (env, value)
}

/// One step into a JSON document.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Seg {
    Key(String),
    Index(usize),
}

fn render(path: &[Seg]) -> String {
    let mut out = String::new();
    for seg in path {
        match seg {
            Seg::Key(k) => {
                if !out.is_empty() {
                    out.push('.');
                }
                out.push_str(k);
            }
            Seg::Index(i) => out.push_str(&format!("[{i}]")),
        }
    }
    out
}

/// The last object key on `path` — the name a refusal has to mention.
fn leaf_key(path: &[Seg]) -> String {
    path.iter()
        .rev()
        .find_map(|s| match s {
            Seg::Key(k) => Some(k.clone()),
            Seg::Index(_) => None,
        })
        .expect("every path starts with a key")
}

/// Every position in `value` a mutation can be applied at.
fn positions(value: &Value, prefix: &[Seg], out: &mut Vec<Vec<Seg>>) {
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                let mut p = prefix.to_vec();
                p.push(Seg::Key(k.clone()));
                out.push(p.clone());
                positions(v, &p, out);
            }
        }
        Value::Array(items) => {
            for (i, v) in items.iter().enumerate() {
                let mut p = prefix.to_vec();
                p.push(Seg::Index(i));
                out.push(p.clone());
                positions(v, &p, out);
            }
        }
        _ => {}
    }
}

/// A value of a type the field at hand cannot legally have.
///
/// `null` is the awkward one: a field that is `null` in this response does not
/// say what it is when it is not, so the replacement has to be wrong for every
/// nullable type in the contract. `true` is — nothing in the protocol is
/// declared boolean-or-null. If that ever changes, this test fails loudly on
/// that field rather than quietly passing, which is the right way round.
fn wrong_type(current: &Value) -> Value {
    match current {
        Value::Null => json!(true),
        Value::Bool(_) => json!("not a boolean"),
        Value::Number(_) => json!("not a number"),
        Value::String(_) => json!(0),
        Value::Array(_) => json!("not an array"),
        Value::Object(_) => json!("not an object"),
    }
}

fn at_mut<'a>(root: &'a mut Value, path: &[Seg]) -> &'a mut Value {
    let mut cur = root;
    for seg in path {
        cur = match seg {
            Seg::Key(k) => cur.get_mut(k).expect("the path exists"),
            Seg::Index(i) => cur.get_mut(i).expect("the path exists"),
        };
    }
    cur
}

fn with_dropped(root: &Value, path: &[Seg]) -> Value {
    let mut copy = root.clone();
    let (last, parent_path) = path.split_last().expect("a non-empty path");
    let parent = at_mut(&mut copy, parent_path);
    match last {
        Seg::Key(k) => {
            parent
                .as_object_mut()
                .expect("a key is dropped from an object")
                .remove(k);
        }
        Seg::Index(_) => unreachable!("array elements are never dropped"),
    }
    copy
}

fn with_retyped(root: &Value, path: &[Seg]) -> Value {
    let mut copy = root.clone();
    let slot = at_mut(&mut copy, path);
    *slot = wrong_type(slot);
    copy
}

/// Top-level fields the engine may legitimately omit, which the widget must
/// therefore accept the absence of. `message` is `skip_serializing_if` in
/// `StatusReport`: a ready engine with nothing to say has no `message` key,
/// and refusing that response would black out every healthy machine.
const OPTIONAL: [&str; 1] = ["message"];

#[test]
fn every_nested_field_of_the_contract_is_checked() {
    let Some(qml) = qml_runtime() else {
        eprintln!(
            "SKIPPED: no QML runtime found (looked at $OSM_QML, \
             /usr/lib/qt6/bin/qml, qml6 and qml on PATH). The source-level \
             rules in tests/manifest.rs still ran."
        );
        return;
    };

    let (_env, real) = populated_status("qmlnested");

    // The fixture has to actually be populated, or this test checks nothing
    // and says it checked everything.
    assert!(real["snapshot"].is_object(), "no snapshot recorded: {real}");
    assert!(
        real["sessions"].as_array().is_some_and(|s| s.len() >= 2),
        "the fixture recorded no sessions: {real}"
    );
    assert!(
        real["database"]["preserved"].is_object(),
        "the legacy database was not preserved: {real}"
    );
    assert!(
        real["agents"]["unsupported"]
            .as_array()
            .is_some_and(|u| !u.is_empty()),
        "no unsupported agent is reported, so that row is never checked: {real}"
    );
    assert!(real["message"].is_string(), "no message to retype: {real}");

    let mut paths = Vec::new();
    positions(&real, &[], &mut paths);

    // A response with a nested object present but empty. This is the finding,
    // written out: valid JSON, the right protocol, `ready:true`, and every
    // nested field of it missing at once.
    let mut hollow = real.clone();
    for name in ["capture", "database", "tmux", "agents"] {
        hollow[name] = json!({});
    }

    let mut cases: Vec<(String, Value)> = vec![
        ("the engine's populated output".to_string(), real.clone()),
        ("hollow nested objects".to_string(), hollow),
    ];
    let mut expected_reject: Vec<String> = vec!["hollow nested objects".to_string()];
    let mut expected_accept: Vec<String> = vec!["the engine's populated output".to_string()];

    for path in &paths {
        let name = render(path);
        let retyped = format!("{name} retyped");
        cases.push((retyped.clone(), with_retyped(&real, path)));
        expected_reject.push(retyped);

        // An array element is not dropped: a shorter list of valid rows is a
        // shorter list, not a malformed response.
        if matches!(path.last(), Some(Seg::Index(_))) {
            continue;
        }
        let dropped = format!("{name} dropped");
        cases.push((dropped.clone(), with_dropped(&real, path)));
        if path.len() == 1 && OPTIONAL.contains(&leaf_key(path).as_str()) {
            expected_accept.push(dropped);
        } else {
            expected_reject.push(dropped);
        }
    }

    let borrowed: Vec<(&str, Value)> = cases.iter().map(|(n, v)| (n.as_str(), v.clone())).collect();
    let results = verdicts(&qml, &borrowed);
    let verdict = |name: &str| -> String {
        results
            .iter()
            .find(|(n, _)| n == name)
            .unwrap_or_else(|| panic!("no verdict for {name}"))
            .1
            .clone()
    };

    for name in &expected_accept {
        assert_eq!(
            verdict(name),
            "",
            "the widget refuses `{name}`, which the engine can really print — \
             a healthy machine would render as 'the engine did not return \
             status JSON'"
        );
    }

    let mut accepted: Vec<String> = Vec::new();
    for name in &expected_reject {
        if verdict(name).is_empty() {
            accepted.push(name.clone());
        }
    }

    // A refusal that does not name the field leaves the user with "something
    // is wrong", which is not a report anyone can act on.
    let mut unnamed: Vec<String> = Vec::new();
    for path in &paths {
        for name in [
            format!("{} retyped", render(path)),
            format!("{} dropped", render(path)),
        ] {
            if !expected_reject.contains(&name) {
                continue;
            }
            let said = verdict(&name);
            if !said.is_empty() && !said.contains(&leaf_key(path)) {
                unnamed.push(format!("{name} is refused with `{said}`"));
            }
        }
    }

    assert!(
        accepted.is_empty(),
        "the widget publishes these as complete status reports:\n  {}",
        accepted.join("\n  ")
    );
    assert!(
        unnamed.is_empty(),
        "these refusals do not name the field that is wrong:\n  {}",
        unnamed.join("\n  ")
    );
}

/// A session that actually holds a goal and conversations, held to the same
/// standard as everything else.
///
/// `every_nested_field_of_the_contract_is_checked` walks the engine's own
/// output, and a fixture with no agent pane in it has `"goal": null` and
/// `"conversations": []` — so the two shapes that carry the new claims are
/// never reached there. They are the ones that matter: a goal missing its
/// `native_id` is a title that can be drawn against the wrong conversation,
/// and a conversation missing `title_source` is a line whose provenance the
/// menu would have to guess.
#[test]
fn a_populated_session_goal_is_held_to_its_whole_shape() {
    let Some(qml) = qml_runtime() else {
        eprintln!("SKIPPED: no QML runtime found; see tests/manifest.rs.");
        return;
    };
    let (_env, real) = populated_status("qmlgoal");

    let goal = json!({
        "title": "Tmux memory management plugin",
        "source": "agent",
        "kind": "claude",
        "native_id": "099d4f76-19b5-47fc-8a7f-fffb219cf6eb",
    });
    let conversation = json!({
        "kind": "claude",
        "native_id": "099d4f76-19b5-47fc-8a7f-fffb219cf6eb",
        "title": "Tmux memory management plugin",
        "title_source": "agent",
        "window_idx": 0,
        "pane_idx": 1,
        "last_active": 1_788_500_000i64,
    });

    let with_session = |goal: Value, conversations: Value| {
        let mut v = real.clone();
        let mut row = real["sessions"][0].clone();
        row["goal"] = goal;
        row["conversations"] = conversations;
        v["sessions"] = json!([row]);
        v
    };

    let mut cases: Vec<(&str, Value)> = vec![(
        "a session with a goal and a conversation",
        with_session(goal.clone(), json!([conversation.clone()])),
    )];

    // Each field of the goal, dropped and retyped.
    let mut rejected: Vec<String> = Vec::new();
    let mut owned: Vec<(String, Value)> = Vec::new();
    for field in ["title", "source", "kind", "native_id"] {
        let mut dropped = goal.clone();
        dropped.as_object_mut().unwrap().remove(field);
        owned.push((
            format!("goal without {field}"),
            with_session(dropped, json!([conversation.clone()])),
        ));
        let mut retyped = goal.clone();
        retyped[field] = json!(7);
        owned.push((
            format!("goal whose {field} is a number"),
            with_session(retyped, json!([conversation.clone()])),
        ));
    }
    for field in [
        "kind",
        "native_id",
        "title",
        "title_source",
        "window_idx",
        "pane_idx",
        "last_active",
    ] {
        let mut dropped = conversation.clone();
        dropped.as_object_mut().unwrap().remove(field);
        owned.push((
            format!("conversation without {field}"),
            with_session(goal.clone(), json!([dropped])),
        ));
    }
    for (name, body) in &owned {
        cases.push((name.as_str(), body.clone()));
        rejected.push(name.clone());
    }

    let results = verdicts(&qml, &cases);
    let verdict = |name: &str| -> String {
        results
            .iter()
            .find(|(n, _)| n == name)
            .unwrap_or_else(|| panic!("no verdict for {name}"))
            .1
            .clone()
    };

    assert_eq!(
        verdict("a session with a goal and a conversation"),
        "",
        "the widget rejects a session row the engine really prints, so a \
         machine with an agent in a pane would render as a broken engine"
    );
    let accepted: Vec<&String> = rejected
        .iter()
        .filter(|name| verdict(name).is_empty())
        .collect();
    assert!(
        accepted.is_empty(),
        "the widget publishes these as complete status reports: {accepted:?}"
    );
}
