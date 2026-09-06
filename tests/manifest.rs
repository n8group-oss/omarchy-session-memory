//! The marketplace plugin's manifest, checked against what Omarchy's
//! `PluginRegistry.validateManifest` actually requires — and against the two
//! rules the QML itself has to keep.
//!
//! The QML cannot be unit-tested here: rendering it needs a running Quattro
//! shell. What *is* checkable is that the manifest is well-formed, that every
//! entry point names a file that exists, and that neither QML file ever hands
//! a session name or a conversation id to a shell.

use serde_json::Value;

fn manifest() -> Value {
    let raw = std::fs::read_to_string("manifest.json").expect("manifest.json exists");
    serde_json::from_str(&raw).expect("manifest is valid JSON")
}

#[test]
fn the_manifest_matches_what_omarchy_requires() {
    let m = manifest();

    assert_eq!(m["schemaVersion"], 1);
    assert_eq!(m["id"], "io.github.n8group-oss.sessionmemory");
    assert_eq!(m["license"], "MIT");
    assert!(
        m["version"].as_str().unwrap().split('.').count() == 3,
        "semver"
    );

    // Every declared kind needs an entry point, and every entry point must
    // name a file that exists — a plugin whose QML is missing installs and
    // then fails at runtime, which the marketplace validator catches but the
    // author should catch first.
    let kinds = m["kinds"].as_array().expect("kinds is an array");
    assert!(!kinds.is_empty());
    for kind in kinds {
        let key = match kind.as_str().unwrap() {
            "bar-widget" => "barWidget",
            other => other,
        };
        let file = m["entryPoints"][key]
            .as_str()
            .unwrap_or_else(|| panic!("no entry point for kind {kind}"));
        assert!(
            std::path::Path::new(file).exists(),
            "entry point {file} does not exist"
        );
    }
}

/// The registry's own gate, restated here so a manifest that this repository
/// is happy with is one the shell will actually load.
///
/// Taken from `PluginRegistry.validateManifest`: five required fields, an id
/// that cannot escape its own directory, a non-empty `kinds`, entry points
/// that are relative paths, and — when present — a `defaultSection` the bar
/// recognises. A manifest that fails any of these is dropped with a
/// `console.warn` the user never sees.
#[test]
fn the_manifest_passes_the_registrys_own_checks() {
    let m = manifest();

    for field in ["id", "name", "version", "kinds", "entryPoints"] {
        assert!(!m[field].is_null(), "missing required field {field}");
    }

    let id = m["id"].as_str().unwrap();
    assert!(
        !id.is_empty() && !id.contains('/') && !id.contains("..") && !id.starts_with('/'),
        "the registry rejects this id: {id}"
    );

    let entry_points = m["entryPoints"]
        .as_object()
        .expect("entryPoints is an object");
    assert!(!entry_points.is_empty());
    for (key, value) in entry_points {
        let path = value
            .as_str()
            .unwrap_or_else(|| panic!("entry point {key} is not a string"));
        assert!(
            !path.is_empty() && !path.starts_with('/') && !path.contains(".."),
            "unsafe entry point {key}={path}"
        );
    }

    if let Some(section) = m["barWidget"]["defaultSection"].as_str() {
        assert!(
            ["left", "center", "right"].contains(&section),
            "the registry rejects defaultSection={section}"
        );
    }
}

/// `Menu.qml` is loaded by a `Loader` inside the widget, deliberately.
///
/// Declaring it as a second kind would make the shell mount it in its own
/// right, so the menu would exist twice: once as the widget's popup and once
/// as a panel of its own, each polling the engine on its own timer.
#[test]
fn the_menu_is_loaded_internally_and_is_not_a_second_kind() {
    let m = manifest();
    let entry_points = m["entryPoints"].as_object().unwrap();
    assert!(
        entry_points
            .values()
            .all(|v| v.as_str() != Some("Menu.qml")),
        "Menu.qml must not be an entry point: {entry_points:?}"
    );
    let widget = std::fs::read_to_string("BarWidget.qml").expect("BarWidget.qml exists");
    assert!(
        widget.contains("Menu.qml"),
        "BarWidget.qml must load Menu.qml itself"
    );
}

#[test]
fn the_manifest_version_matches_the_crate() {
    assert_eq!(
        manifest()["version"].as_str().unwrap(),
        env!("CARGO_PKG_VERSION"),
        "the plugin and the engine version must not drift"
    );
}

#[test]
fn the_qml_never_builds_a_shell_string() {
    // Session names and conversation ids reach these files. Invoking a shell
    // with them interpolated would be a command injection with the user's
    // own data as the payload.
    // `bar.run` and `Util.execDetached` are the same hazard wearing the
    // shell's clothes: both end in `bash -lc <string>`, so a session name
    // with a `$(…)` in it would be executed. `Util.execArgv` and
    // `Quickshell.execDetached([…])` are the argv-preserving forms.
    //
    // Comment lines are skipped so the files may name the hazard they avoid.
    for f in ["BarWidget.qml", "Menu.qml"] {
        let src = std::fs::read_to_string(f).unwrap_or_else(|_| panic!("{f} exists"));
        for (n, line) in src.lines().enumerate() {
            if line.trim_start().starts_with("//") {
                continue;
            }
            for bad in [
                "sh -c",
                "bash -c",
                "/bin/sh",
                "\"-lc\"",
                "bar.run(",
                "Util.execDetached(",
            ] {
                assert!(
                    !line.contains(bad),
                    "{f}:{}: hands a string to a shell ({bad}); use an argv array",
                    n + 1
                );
            }
        }
    }
}

/// Every command the QML runs is an argv array literal, never a built string.
///
/// The denylist above only catches the shells this project thought of.
/// `Process.command` and `Quickshell.execDetached` both accept a string on
/// some paths, and a string is what gets word-split — so the invariant worth
/// enforcing is the positive one: the right-hand side of every command is a
/// list.
#[test]
fn every_command_the_qml_runs_is_an_array() {
    for f in ["BarWidget.qml", "Menu.qml"] {
        let src = std::fs::read_to_string(f).unwrap_or_else(|_| panic!("{f} exists"));
        for (n, line) in src.lines().enumerate() {
            if line.trim_start().starts_with("//") {
                continue;
            }
            let rhs = line
                .split_once(".command =")
                .or_else(|| line.split_once("command:"))
                .or_else(|| line.split_once("execDetached("))
                .map(|(_, rhs)| rhs.trim());
            let Some(rhs) = rhs else { continue };
            assert!(
                rhs.starts_with('[') || rhs.starts_with("root.argv") || rhs.is_empty(),
                "{f}:{}: command is not an argv array: {}",
                n + 1,
                line.trim()
            );
        }
    }
}

// ---------------------------------------------------------------------------
// What the QML source has to be true of.
//
// Rendering these files needs a running Quattro shell, so behaviour is not
// testable here. Their *source* is: the checks below read the two QML files as
// text and hold them to the invariants that would otherwise only be visible to
// someone reading them carefully — that no probe can hang forever, that no
// partially-shaped response is published as a status, that the protocol is
// checked before anything is rendered from it, and that nothing the user's
// data flows through is assembled by string interpolation.

/// The body of a QML `function name(...)` — from its opening brace to the
/// matching close.
fn qml_function(src: &str, name: &str) -> String {
    let at = src
        .find(&format!("function {name}("))
        .unwrap_or_else(|| panic!("no function {name} in the source"));
    let open = src[at..].find('{').expect("a function body") + at;
    let bytes = src.as_bytes();
    let mut depth = 0usize;
    for i in open..bytes.len() {
        match bytes[i] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return src[open..=i].to_string();
                }
            }
            _ => {}
        }
    }
    panic!("function {name} is not closed");
}

/// Source with `//` comment lines removed, so a check cannot be satisfied by
/// prose describing the thing it is looking for.
fn without_comments(src: &str) -> String {
    src.lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn qml(file: &str) -> String {
    std::fs::read_to_string(file).unwrap_or_else(|_| panic!("{file} exists"))
}

/// Every `Process` in the plugin is bounded by a watchdog that terminates it.
///
/// `osm status`, `osm agents` and `osm restore` are three processes the shell
/// starts and then waits on. A probe that never answers — a binary that hangs
/// on a lock, an NFS home that stops responding — leaves the widget rendering
/// whatever it last knew, indefinitely and with no sign that it is old. That
/// is the plugin's own failure mode wearing a healthy icon.
///
/// Each `Process { id: X }` must therefore have a `Timer { id: XWatchdog }`
/// that signals it, must arm that timer when it starts the process, and must
/// disarm it when the process exits.
#[test]
fn every_process_is_bounded_by_a_watchdog() {
    for f in ["BarWidget.qml", "Menu.qml"] {
        let src = without_comments(&qml(f));
        let ids = process_ids(&src);
        assert!(!ids.is_empty(), "{f}: no Process blocks found to check");
        for id in ids {
            let watchdog = format!("{id}Watchdog");
            assert!(
                src.contains(&format!("Timer {{\n    id: {watchdog}"))
                    || src.contains(&format!("id: {watchdog}")),
                "{f}: {id} has no {watchdog} timer, so it can hang forever"
            );
            assert!(
                src.contains(&format!("{id}.signal(")),
                "{f}: {watchdog} never signals {id}; a timer that only fires is not a timeout"
            );
            assert!(
                src.contains(&format!("{watchdog}.restart()")),
                "{f}: {watchdog} is never armed, so {id} runs unbounded"
            );
            assert!(
                src.contains(&format!("{watchdog}.stop()")),
                "{f}: {watchdog} is never disarmed after {id} exits"
            );
        }
    }
}

/// Every `Process { id: … }` in one file.
fn process_ids(src: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = src;
    while let Some(at) = rest.find("Process {") {
        rest = &rest[at + "Process {".len()..];
        let id_at = rest.find("id:").expect("a Process without an id");
        let after = &rest[id_at + 3..];
        let id: String = after
            .trim_start()
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        assert!(!id.is_empty(), "a Process has an unreadable id");
        out.push(id);
    }
    out
}

/// A status response is published only after its whole shape is checked.
///
/// `{"protocol_version":1,"ready":true}` is a valid JSON object with a
/// protocol this plugin speaks, and it is not a status report: it has no
/// `snapshot`, no `sessions`, no `capture`. Published, it renders as "no
/// snapshot has been recorded" and an empty session list — a machine with
/// nothing to restore, which is a claim the engine never made and the exact
/// defect class this plugin exists to avoid.
#[test]
fn a_status_is_published_only_after_its_whole_shape_is_checked() {
    let src = qml("BarWidget.qml");
    let body = qml_function(&src, "applyProbe");

    let publish = body
        .find("status = parsed")
        .expect("applyProbe must publish the parsed response somewhere");
    for gate in [
        "readableFault(",
        "protocolFault(",
        "shapeFault(",
        "\"incompatible\"",
        "\"unreadable\"",
    ] {
        let at = body
            .find(gate)
            .unwrap_or_else(|| panic!("applyProbe does not check {gate}"));
        assert!(
            at < publish,
            "applyProbe reaches `{gate}` only after publishing the response"
        );
    }

    // Each field of the contract is named, so a response missing one is
    // rejected rather than rendered as an absence.
    let shape = without_comments(&src);
    for field in [
        "engine_version",
        "ready",
        "capture",
        "database",
        "tmux",
        "agents",
        "sessions",
        "snapshot",
    ] {
        assert!(
            shape.contains(&format!("{field}:")) || shape.contains(&format!("\"{field}\"")),
            "the shape check does not require {field}"
        );
    }
}

/// The engine's protocol must be *this* protocol, in both responses.
///
/// `protocol_version !== undefined` accepts protocol 2, whose fields this
/// plugin has never seen; the session and conversation lists it would render
/// from one are guesses.
#[test]
fn both_responses_require_the_exact_protocol() {
    let widget = without_comments(&qml("BarWidget.qml"));
    let menu = without_comments(&qml("Menu.qml"));

    // An exact comparison, in either direction — not a `>=`, not a truthiness
    // test, not a check that the field merely exists.
    assert!(
        ["=== supportedProtocol", "!== supportedProtocol"]
            .iter()
            .any(|c| widget.contains(c)),
        "the status probe does not compare the protocol to supportedProtocol exactly"
    );
    assert!(
        menu.contains("supportedProtocol"),
        "the agents probe does not compare the protocol to supportedProtocol"
    );
    for (f, src) in [("BarWidget.qml", &widget), ("Menu.qml", &menu)] {
        assert!(
            !src.contains("protocol_version !== undefined"),
            "{f}: still accepts any protocol that merely has a protocol_version"
        );
    }
}

/// A failed or timed-out probe drops what the previous one said.
///
/// A conversation list, a session list and a snapshot id all describe a moment
/// that has passed. Left on screen beside an error, they read as current.
#[test]
fn a_failed_probe_clears_what_the_last_one_published() {
    let widget = qml_function(&qml("BarWidget.qml"), "applyProbe");
    assert!(
        widget.contains("status = null"),
        "applyProbe leaves the previous status on screen when a probe fails"
    );
    let menu = without_comments(&qml("Menu.qml"));
    assert!(
        menu.contains("root.agents = null"),
        "Menu.qml leaves the previous conversation list on screen when its probe fails"
    );
}

/// Nothing the user's data flows into is assembled by interpolation.
///
/// The clipboard is the one place in this plugin where a string leaves QML's
/// argv discipline: the menu tells the user to paste it into a shell, so a
/// conversation id of `x; touch /tmp/pwn #` becomes a command they run
/// themselves. Every operand of a clipboard assignment must therefore be
/// either a literal or a call to the quoting function.
#[test]
fn clipboard_text_is_never_built_from_unquoted_data() {
    for f in ["BarWidget.qml", "Menu.qml"] {
        let src = without_comments(&qml(f));
        for (n, line) in src.lines().enumerate() {
            let Some((_, rhs)) = line.split_once("clipboardText =") else {
                continue;
            };
            for operand in concat_operands(rhs) {
                let operand = operand.trim();
                let literal = operand.starts_with('"') && operand.ends_with('"');
                let quoted =
                    operand.starts_with("root.shellQuote(") || operand.starts_with("shellQuote(");
                assert!(
                    literal || quoted,
                    "{f}:{}: clipboard text is built from `{operand}`, which is neither a \
                     literal nor a shell-quoted value",
                    n + 1
                );
            }
        }
    }
}

/// One concatenation split at its top-level `+`, with string literals kept
/// whole so a `+` inside one is not a separator.
fn concat_operands(expr: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    for c in expr.trim().chars() {
        if in_string {
            current.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                current.push(c);
            }
            '(' | '[' => {
                depth += 1;
                current.push(c);
            }
            ')' | ']' => {
                depth -= 1;
                current.push(c);
            }
            '+' if depth == 0 => {
                out.push(std::mem::take(&mut current));
            }
            _ => current.push(c),
        }
    }
    if !current.trim().is_empty() {
        out.push(current);
    }
    out.into_iter().filter(|o| !o.trim().is_empty()).collect()
}

/// The resume command ends its options before the id.
///
/// A conversation id beginning with `-` would otherwise be read by the shell's
/// `osm` as a flag, and by clap as an unknown option.
#[test]
fn the_resume_command_ends_options_before_the_id() {
    let src = without_comments(&qml("Menu.qml"));
    assert!(
        src.contains("\"osm resume -- \""),
        "the copied resume command does not end its options before the id"
    );
}

/// The menu draws a bounded, ordered list, and says what it is bounding.
///
/// `model: root.resumableAgents` was every resumable conversation on the
/// machine — 2334 of them on the maintainer's, in whatever order the engine
/// produced. Capping it is only half a fix: a menu that silently draws twenty
/// of 2334 tells the user the other 2314 do not exist, which is this
/// project's oldest defect wearing a different hat.
///
/// The behaviour is checked properly in `tests/qml_resumable_list.rs`, which
/// runs the menu's own functions under Qt. This is the part that still runs
/// where Qt is not installed.
#[test]
fn the_menu_caps_orders_and_labels_the_resumable_list() {
    let src = without_comments(&qml("Menu.qml"));
    assert!(
        !src.contains("model: root.resumableAgents"),
        "Menu.qml renders a delegate per resumable conversation, with no cap"
    );
    assert!(
        src.contains("model: root.shownResumableAgents"),
        "Menu.qml's conversation list is not drawn from the capped, ordered \
         property"
    );
    assert!(
        src.contains("root.conversationsMatching(root.resumableAgents, root.filterText)"),
        "the filtered list is not drawn from the full one"
    );
    assert!(
        src.contains("root.newestFirst(root.matchingResumableAgents, root.resumableCap)"),
        "the drawn list is not the newest conversations from the matching \
         ones — the cap has to come after the filter, or a search can never \
         reach anything older than the newest 25"
    );

    let decl = "readonly property int resumableCap:";
    let at = src
        .find(decl)
        .expect("Menu.qml declares a cap on the resumable list");
    let cap: usize = src[at + decl.len()..]
        .trim_start()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .expect("resumableCap is a plain integer");
    assert!(
        (1..=50).contains(&cap),
        "resumableCap is {cap}: a cap of zero draws nothing, and one this \
         large is the unbounded list again"
    );

    assert!(
        src.contains("root.resumableNote(root.resumableAgents.length"),
        "the menu does not state how many conversations exist, so a capped \
         list reads as the whole of them"
    );
}

/// A recorded capture error is never rendered without asking when it was.
///
/// This is the maintainer's "captures are fresh · the window placement for
/// this capture could not be read …": one clause saying everything is fine and
/// the next one describing a failure, from the same block, 1.7 hours apart.
/// `record_success` in `src/health.rs` clears the failure streak and leaves
/// `last_error`/`last_error_at` alone on purpose, so the record outlives the
/// condition by design and the panel is the only thing that can tell them
/// apart. It could not: it appended `last_error` unconditionally.
///
/// The rule is that every read of `last_error` happens somewhere that also
/// reads both timestamps — there is no other way to know which of the two
/// sentences to write. The wording itself, and the four orderings it has to
/// distinguish, are measured under Qt in `tests/qml_capture_line.rs`; this is
/// the half that runs where Qt is not installed.
#[test]
fn a_recorded_capture_error_is_dated_before_it_is_shown() {
    let src = without_comments(&qml("Menu.qml"));
    let note = qml_function(&src, "captureErrorNote");
    for field in ["last_error", "last_error_at", "last_success_at"] {
        assert!(
            note.contains(field),
            "captureErrorNote never looks at {field}, so it cannot tell a \
             failure that is happening from one that was resolved hours ago"
        );
    }

    // And the stamps are not the only authority, because they cannot always
    // be one. They are epoch *seconds*: a capture that succeeds and then fails
    // inside the same second leaves `last_error_at === last_success_at`, so
    // `at > ok` is false while a capture is failing right now. The failure
    // streak is what `record_success` clears, so a non-zero one settles it.
    let current = qml_function(&src, "captureErrorIsCurrent");
    assert!(
        current.contains("consecutive_failures"),
        "captureErrorIsCurrent decides on timestamps alone, so a failure in \
         the same second as the last success is drawn as a past error with a \
         successful capture since — see \
         tests/qml_capture_line.rs::a_failure_in_the_same_second_as_the_last_success_is_still_current"
    );

    // And the drawing site does not reach for the error text behind its back.
    // Reading `last_error` in a binding is how the defect was written in the
    // first place: a `text:` block that pushes clauses onto an array has
    // nowhere to put the comparison, and nothing to remind an author it is
    // missing.
    let mut rest = src.clone();
    for name in [
        "captureErrorNote",
        "captureErrorAge",
        "captureErrorIsCurrent",
    ] {
        let body = qml_function(&src, name);
        rest = rest.replace(&body, "");
    }
    assert!(
        !rest.contains("last_error"),
        "Menu.qml reads last_error outside the functions that date it, where \
         nothing compares it with last_success_at"
    );
}

/// A list that runs past the bottom of the panel says so, and answers the
/// wheel.
///
/// The maintainer's panel is about 390px wide and holds 8 sessions, 20 live
/// conversations and a count of 2448 resumable ones; the content is taller
/// than the card, so it ends mid-row at the bottom edge. His report was that
/// he could not scroll.
///
/// `ScrollBar.AsNeeded` is the reason it looks that way. It does not mean
/// "when there is more below": in every Qt Quick Controls style the bar is
/// drawn at zero opacity until the view is `active` — moving, hovered or
/// pressed — so a panel at rest carries no mark at all that the list
/// continues. The affordance appears only to someone who already guessed.
///
/// So the policy follows the overflow, and the wheel is handled explicitly
/// rather than left to flick physics, which throw the list a slightly
/// different distance each time and keep coasting after the notch.
///
/// Measured behaviour — the bar's opacity before anything is touched, and what
/// a real wheel event does to `contentY` — is in `tests/qml_menu_scroll.rs`,
/// which drives the lifted `Flickable` under `qmltestrunner`. This is the half
/// that runs where Qt is not installed.
#[test]
fn the_menus_list_shows_that_it_scrolls_and_answers_the_wheel() {
    let src = without_comments(&qml("Menu.qml"));
    let list = blocks(&src, "Flickable {")
        .into_iter()
        .find(|b| b.contains("id: menuFlick"))
        .expect("Menu.qml draws its list in a Flickable called menuFlick");

    assert!(
        list.contains("WheelHandler"),
        "the menu's list has no wheel handler, so scrolling it with the wheel \
         is whatever Flickable's flick physics happen to do with the event"
    );
    assert!(
        list.contains("interactive: contentHeight > height"),
        "the list is interactive regardless of whether there is anything below \
         the fold"
    );
    assert!(
        list.contains("ScrollBar.AlwaysOn"),
        "the scrollbar is never shown outright, so a list that overflows looks \
         truncated rather than scrollable"
    );
    assert!(
        list.contains("menuFlick.contentHeight > menuFlick.height"),
        "the scrollbar's policy does not follow the overflow: pinned on, it \
         marks more content below a list that has none; pinned to AsNeeded, it \
         is invisible until the user is already scrolling"
    );
}

/// The panel's requested width is a ceiling the screen can lower, never a
/// floor.
///
/// `KeyboardPanel.fittedContentWidth(w)` returns `min(w, screen - margins)`,
/// so asking for a wider panel is safe on a display that cannot give one:
/// a 1366px laptop gets its screen width and nothing more. Setting
/// `contentWidth` to a bare number instead would put a panel wider than the
/// display on it, with the right-hand side of every row off the edge — the
/// clipped-button defect again, arriving from the other direction and on
/// exactly the machines least able to absorb it.
///
/// What the width has to be *big enough* for is measured in
/// `tests/qml_row_layout.rs`, which lays the rows out at the declared width.
#[test]
fn the_panels_width_is_clamped_to_the_screen() {
    let src = without_comments(&qml("Menu.qml"));
    assert!(
        src.contains("contentWidth: menuPanel.fittedContentWidth("),
        "the menu sets its own width without asking the panel what the screen \
         can give, so a wide default becomes a panel that hangs off a small \
         display"
    );
}

/// Every balanced `{ … }` block in `src` that begins with `opener`.
fn blocks(src: &str, opener: &str) -> Vec<String> {
    let bytes = src.as_bytes();
    let mut out = Vec::new();
    let mut from = 0usize;
    while let Some(rel) = src[from..].find(opener) {
        let start = from + rel;
        let open = start + opener.len() - 1;
        let mut depth = 0usize;
        for i in open..bytes.len() {
            match bytes[i] {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        out.push(src[start..=i].to_string());
                        break;
                    }
                }
                _ => {}
            }
        }
        from = start + opener.len();
    }
    out
}

/// No row in the menu places its children without looking at the panel.
///
/// This is the maintainer's "some part of button is truncated". `Row` is a
/// positioner: it puts each child at that child's own width and never once
/// asks how wide the parent is, so a long conversation id beside a long
/// project path pushes the "Copy resume" button past the right edge of the
/// popup, where the `Flickable` clips it. The same shape, and the same bug,
/// was in the session row.
///
/// The second half of the rule is about the fraction. `width: Math.max(…,
/// parentRow.width * 0.4)` looks like it respects the parent and does not: it
/// claims 40% for one child while its siblings still take whatever they want,
/// so the total is 40% *plus* two natural widths and overflows exactly as
/// before.
///
/// Measured behaviour is in `tests/qml_row_layout.rs`, which lays these rows
/// out under Qt and checks where every child landed. This is the part that
/// still runs where Qt is not installed.
#[test]
fn no_row_in_the_menu_places_children_without_looking_at_the_panel() {
    let src = without_comments(&qml("Menu.qml"));
    for (n, line) in src.lines().enumerate() {
        assert!(
            line.trim() != "Row {",
            "Menu.qml:{}: a plain Row lays its children out at their own \
             widths and lets them run off the panel; use a RowLayout (for a \
             row that has something elastic in it) or a Flow (for a row of \
             buttons)",
            n + 1
        );
    }
    assert!(
        !src.contains(".width * 0."),
        "Menu.qml sizes a child as a fraction of its own row. A fraction is \
         not a budget: the siblings still take their natural widths on top of \
         it."
    );
}

/// In every row of the menu, the labels can shrink and the buttons cannot —
/// and the field that says which row it is can shrink least of all.
///
/// A QtQuick layout only ever resizes items that declare `Layout.fillWidth`,
/// so a label without it is fixed at its implicit width and a row of such
/// labels overflows just like a `Row` would. And a shrinkable button is only
/// a slower version of the reported defect — a control cut down to a third of
/// its label is one the user can neither read nor reliably hit — so each one
/// declares its own `implicitWidth` as a floor and does not fill.
///
/// Filling with no minimum is the other half of the same defect: Qt shrinks a
/// fill item to its declared minimum, and a stretch factor is not a floor —
/// it only decides who gets *spare* room, of which there is none under
/// pressure. Measured offscreen at a 320px panel, the session name was drawn
/// 8px wide: the ellipsis, beside 304px of counts that read much the same on
/// every row. So the identity declares a real minimum, and each row is a
/// `GridLayout` that drops to one column rather than squeeze past it.
///
/// Measured behaviour is in `tests/qml_row_layout.rs`.
#[test]
fn the_menus_rows_let_labels_give_and_never_the_buttons() {
    let src = without_comments(&qml("Menu.qml"));
    assert!(
        src.contains("readonly property int identityFloor:"),
        "the menu declares no floor for the fields that identify a row, so \
         every one of them may be shrunk to nothing"
    );
    let rows = blocks(&src, "GridLayout {");
    assert!(
        rows.len() >= 2,
        "expected the session row and the conversation row to be GridLayouts, \
         which is what lets a row that cannot hold one line drop to one \
         column; found {}",
        rows.len()
    );

    for row in &rows {
        let head = row.lines().take(3).collect::<Vec<_>>().join(" ");
        assert_eq!(
            row.matches("Layout.preferredWidth: 0").count(),
            1,
            "a row needs exactly one elastic child — the one holding \
             unbounded user data — asking for no width of its own and \
             absorbing the leftover: {head}"
        );
        assert_eq!(
            row.matches("Layout.horizontalStretchFactor: 1").count(),
            1,
            "the elastic child must be the only one that grows, or a short \
             label spreads out while the path it sits beside stays elided: \
             {head}"
        );
        assert!(
            row.contains("Layout.minimumWidth: Math.min(root.identityFloor,"),
            "the row's elastic child declares no floor, so the layout may \
             shrink it away and leave a row of metadata identical to the one \
             above it: {head}"
        );
        let columns = row
            .lines()
            .find(|l| l.trim_start().starts_with("columns:"))
            .unwrap_or_else(|| {
                panic!(
                    "a row with a floor it cannot always honour on one line \
                     must be able to stack: {head}"
                )
            });
        assert!(
            columns.contains("? ") || row.contains("root.identityFloor ?"),
            "the column count is fixed, so the row squeezes rather than \
             stacking when its floor stops fitting: {}",
            columns.trim()
        );
        assert!(
            row.contains("root.identityFloor ? "),
            "the row does not decide how to lay itself out from the same \
             floor the identity is given, so the two can disagree: {head}"
        );

        for label in blocks(row, "Text {") {
            assert!(
                label.contains("Layout.fillWidth: true"),
                "a label that does not fill cannot be shrunk by the layout, \
                 so it overflows the panel instead: {head}"
            );
            assert!(
                label.contains("elide:"),
                "a label that shrinks without eliding paints over its \
                 neighbour rather than admitting it was cut: {head}"
            );
        }

        for button in blocks(row, "Button {") {
            assert!(
                !button.contains("Layout.fillWidth: true"),
                "a button that fills is a button the layout may shrink: {head}"
            );
            let floor = button
                .lines()
                .find(|l| l.trim_start().starts_with("Layout.minimumWidth:"))
                .unwrap_or_else(|| {
                    panic!(
                        "a button in a row must reserve its own width, or a \
                         long path squeezes it: {head}"
                    )
                });
            assert!(
                floor.trim_end().ends_with(".implicitWidth"),
                "a button's floor must be its own implicit width, not a \
                 number someone guessed: {}",
                floor.trim()
            );
        }
    }
}

/// A row that is nothing but buttons wraps rather than clipping the last one.
///
/// The footer holds three, and "Restore now" becomes the wider "Confirm
/// restore" the moment it is armed — so whether they fit is not a question
/// that can be settled by looking at the file. A `Flow` puts the overflow on
/// the next line, where it is still reachable.
#[test]
fn the_menus_button_rows_wrap_instead_of_running_off_the_edge() {
    let src = without_comments(&qml("Menu.qml"));
    for flow in blocks(&src, "Flow {") {
        assert!(
            flow.contains("width: parent.width"),
            "a Flow with no width never wraps, because it never knows it has \
             run out of room"
        );
    }
    let footer = blocks(&src, "Flow {")
        .into_iter()
        .find(|b| b.contains("id: footerActions"))
        .expect("the footer's action buttons are in a Flow");
    assert_eq!(
        footer.matches("Button {").count(),
        3,
        "the footer's three buttons must all be in the wrapping row"
    );
}

/// The menu can be searched, and the search is the shell's own text field.
///
/// The maintainer has 2447 resumable conversations and 7 recorded sessions,
/// and the list draws the newest 25: without a filter there is no route at all
/// to a conversation older than that. Their words: "There should be also
/// option to search or filter session."
///
/// `qs.Ui.TextField` rather than an input built here, so the box carries the
/// same focus ring, selection colour and hover cursor as every other Omarchy
/// panel.
///
/// The behaviour is checked in `tests/qml_filter.rs`, which runs the menu's
/// own matching and wording under Qt. This is the part that still runs where
/// Qt is not installed.
#[test]
fn the_menu_can_be_filtered_from_a_box_that_belongs_to_the_shell() {
    let src = without_comments(&qml("Menu.qml"));
    assert!(
        src.contains("import qs.Ui"),
        "the filter box must come from the shell's own kit"
    );
    let field = blocks(&src, "TextField {")
        .into_iter()
        .next()
        .expect("Menu.qml has a search box");
    assert!(
        field.contains("placeholderText:"),
        "a box with no placeholder is a box nobody knows the purpose of"
    );
    assert!(
        field.contains("root.filterText = "),
        "the search box does not drive the property the lists filter on"
    );

    // Typing must not re-run the engine. Every row the filter matches against
    // is already in hand; a probe per keystroke would walk /proc and every
    // agent's transcript store on this machine, 2447 conversations' worth,
    // between one letter and the next.
    for forbidden in ["refresh", "Process", ".running = true", "root.argv"] {
        assert!(
            !field.contains(forbidden),
            "the search box mentions `{forbidden}`: typing into a filter must \
             not start work, it must only narrow what is already here"
        );
    }
}

/// While the box has focus, the box gets the keys.
///
/// `PanelKeyCatcher` sets `Keys.priority: Keys.BeforeItem`, so without this
/// the panel takes every key first and typing `restore` into the filter would
/// refresh the status and fire a snapshot on the way past. `blocked` is the
/// kit's own escape hatch for exactly this.
#[test]
fn typing_into_the_filter_does_not_drive_the_panel() {
    let src = without_comments(&qml("Menu.qml"));
    let catcher = blocks(&src, "PanelKeyCatcher {")
        .into_iter()
        .next()
        .expect("Menu.qml has a key catcher");
    assert!(
        catcher.contains("blocked: filterField.activeFocus"),
        "the panel's key handler is not suspended while the filter box has \
         focus, so letters typed into it also run the menu's shortcuts"
    );
}

/// And there is a key that gets the box the focus in the first place.
///
/// A panel summoned from the keyboard focuses `keyCatcher`; Tab switches
/// panel and Escape closes. Without a key that hands the focus on, a keyboard
/// user could not search at all — and trying to type a query ran the
/// shortcuts instead, because `restore` starts with `r` and carries an `s`.
///
/// The key is `/`, the conventional one, and the panel says so on screen: a
/// key nobody is told about is a key nobody has. Real focus and real key
/// delivery are asserted in `tests/qml_focus.rs`, which drives the shell's own
/// `PanelKeyCatcher` with synthetic key events. This is the part that still
/// runs where Qt is not installed.
#[test]
fn the_filter_box_can_be_reached_from_the_keyboard() {
    let src = without_comments(&qml("Menu.qml"));
    let catcher = blocks(&src, "PanelKeyCatcher {")
        .into_iter()
        .next()
        .expect("Menu.qml has a key catcher");
    assert!(
        catcher.contains("filterField.forceActiveFocus()"),
        "no key in the panel's handler moves the focus into the filter box, \
         so the box is unreachable from the keyboard"
    );
    assert!(
        catcher.contains("t === \"/\""),
        "the key that focuses the box is not `/`; if it is deliberately \
         something else, say so here and on screen"
    );
    assert!(
        src.contains("Press / to type here"),
        "the panel does not tell anyone which key reaches the box"
    );

    // The shortcuts that were already there keep working, and Escape stays an
    // exit: the box may be reachable, never inescapable.
    for shortcut in ["t === \"r\"", "t === \"s\""] {
        assert!(
            catcher.contains(shortcut),
            "the panel lost its `{shortcut}` shortcut to the filter key"
        );
    }
    let field = blocks(&src, "TextField {")
        .into_iter()
        .next()
        .expect("Menu.qml has a search box");
    assert!(
        field.contains("keyCatcher.forceActiveFocus()"),
        "Escape in the box never gives the keys back to the panel, so a \
         focused box is a panel that cannot be closed from the keyboard"
    );
}

/// Both lists are filtered, and both say what the filter did.
///
/// A filter that narrows a list without saying so is the defect this plugin
/// keeps closing: a user reads a short list, believes it is the list, and
/// concludes the rest is gone. The conversation line has three numbers to
/// reconcile — how many exist, how many match, how many are drawn — and the
/// session line two, because that list has no cap.
#[test]
fn a_filtered_list_says_what_the_filter_did() {
    let src = without_comments(&qml("Menu.qml"));

    assert!(
        src.contains("root.sessionsMatching(root.sessions, root.filterText)"),
        "the session list is not filtered"
    );
    assert!(
        src.contains("root.matchingSessions"),
        "the workspace groups are not built from the sessions that matched"
    );
    assert!(
        src.contains("root.sessionsNote(root.sessions.length"),
        "a filtered session list does not say how many of how many matched"
    );

    let note = qml_function(&qml("Menu.qml"), "resumableNote");
    for number in ["total", "matching", "shown"] {
        assert!(
            note.contains(number),
            "the conversation count line does not reconcile `{number}`; a \
             user must always be able to tell \"there are none\" from \"none \
             match what you typed\" from \"there are more than are shown\""
        );
    }
    assert!(
        src.contains("root.resumableNote(root.resumableAgents.length,")
            && src.contains("root.matchingResumableAgents.length,")
            && src.contains("root.shownResumableAgents.length,")
            && src.contains("root.filterText)"),
        "the count line is not given all three numbers and the query"
    );

    // Opening the popup starts with an empty box: a filter left over from
    // last time is a list that is short for a reason nobody remembers.
    assert!(
        src.contains("filterText = \"\""),
        "the filter is not cleared when the menu opens"
    );
}

/// Nothing a transcript says is drawn as markup.
///
/// A `Text` with no `textFormat` is `Text.AutoText`: Qt inspects the string
/// and renders it as HTML when it decides the string looks like HTML. Four of
/// the menu's labels carry text osm read out of a conversation store — the
/// three that draw a title (the agent's own `ai-title`, or one line of the
/// user's first prompt) and the one that draws a conversation's project
/// directory, which comes from the transcript's own `cwd` record.
///
/// Two consequences, the second serious: a title of `<b>work</b>` is drawn as
/// a bold `work`, which is not what osm recorded; and a title of `<img
/// src="…">` makes the panel **fetch that resource when it opens**. Measured
/// under Qt 6.11.2 in `tests/qml_title_markup.rs`, which needs a QML runtime
/// and therefore skips in CI. This is the half that runs everywhere, and it is
/// what stops the declaration being deleted on a machine with no Qt.
#[test]
fn every_label_drawn_from_a_transcript_is_plain_text() {
    let src = without_comments(&qml("Menu.qml"));
    for id in [
        "sessionGoal",
        "conversationDetailTitle",
        "conversationTitle",
        "conversationPath",
    ] {
        let marker = format!("id: {id}\n");
        let at = src
            .find(&marker)
            .unwrap_or_else(|| panic!("Menu.qml still declares `{id}`"));
        // The `Text` it is the id of: back up to the brace that opened the
        // object, then take the balanced block from the type name.
        let open = src[..at].rfind('{').expect("the id is inside an object");
        let name_end = src[..open].trim_end().len();
        let name_start = src[..name_end]
            .rfind(|c: char| !(c.is_alphanumeric() || c == '_' || c == '.'))
            .map(|i| i + 1)
            .unwrap_or(0);
        let block = blocks(&src[name_start..], "Text {")
            .into_iter()
            .next()
            .unwrap_or_else(|| panic!("{id} is no longer a Text"));
        assert!(
            block.contains(&marker),
            "the block lifted for {id} is not the one that declares it"
        );
        assert!(
            block.contains("textFormat: Text.PlainText"),
            "{id} draws text osm read out of a transcript and does not declare \
             textFormat: Text.PlainText, so Qt renders it as markup whenever it \
             decides the string looks like HTML — and an <img src=…> title makes \
             the panel load that resource when it opens"
        );
    }
}
