//! The one string this plugin hands to a shell.
//!
//! Everything the QML runs goes out as an argv array, where a conversation id
//! is one argument whatever is in it. The clipboard is where that discipline
//! ends: `Menu.qml` builds a command line, puts it on the clipboard, and tells
//! the user to paste it into a terminal. An id of `x; touch /tmp/pwn #` was
//! copied verbatim, so the user ran it against themselves — a delayed
//! injection with the plugin as the courier. Conversation ids are file names
//! in an agent's transcript store; they are not this plugin's to trust.
//!
//! # What this actually verifies
//!
//! The quoting function is **extracted from `Menu.qml` and executed by Qt's
//! own QML engine**, not reimplemented here — a Rust copy of the algorithm
//! would only ever test the copy. Each result is then run through `/bin/sh`
//! against a stub `osm` that records its argv, so the assertion is about what
//! a shell does with the string rather than about how the string looks.
//!
//! The same harness runs the *unquoted* form the plugin used to copy, and
//! requires the payload to fire. A shell-injection test where the payload
//! cannot fire proves nothing, and this one says so out loud.
//!
//! # What it cannot verify without Qt
//!
//! The execution half needs a QML runtime (`/usr/lib/qt6/bin/qml`, or `qml6`
//! or `qml` on `PATH`), which is present on a machine running the Omarchy
//! shell and absent from this project's CI containers. Where it is missing,
//! [`the_quoting_is_the_reviewed_posix_form`] still runs and still fails on
//! any change to the algorithm — it pins the function's text to the reviewed
//! four lines. That is a weaker check, deliberately narrow, and it is the
//! reason this file does not silently pass when Qt is not installed.

mod common;

use std::path::{Path, PathBuf};

/// The reviewed implementation, character for character once whitespace is
/// normalised.
///
/// Pinning source text is a blunt instrument and is the right one here: this
/// is four lines whose exact form is the whole security property, and any edit
/// to them has to be looked at by a person. `'` is closed, escaped and
/// reopened; nothing else needs an escape inside single quotes, which is why
/// there is no table of characters to strip and no regular expression.
const REVIEWED: &str =
    r#"function shellQuote(value) { return "'" + String(value).split("'").join("'\\''") + "'" }"#;

/// Ids chosen because each one breaks a different naive attempt at this.
const HOSTILE: &[(&str, &str)] = &[
    ("a semicolon and a comment", "x; touch pwn #"),
    ("a command substitution", "$(touch pwn)"),
    ("a backtick substitution", "`touch pwn`"),
    ("a pipeline", "x | touch pwn"),
    ("an and-list", "x && touch pwn"),
    ("a single quote", "it's-a-session"),
    ("a double quote", "say \"hello\""),
    ("a leading dash", "--force"),
    ("a short leading dash", "-x"),
    ("a variable", "$HOME"),
    ("a glob", "*"),
    ("a newline", "one\ntouch pwn"),
    ("a quote that closes and reopens", "a'; touch pwn; '"),
    ("an ordinary id", "0198f2ac-1c4e-7a3b-9f11-2b6d8c4e5a17"),
];

fn menu_source() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Menu.qml");
    std::fs::read_to_string(path).expect("Menu.qml exists")
}

/// `function shellQuote(…) { … }`, verbatim, out of `Menu.qml`.
fn extract_shell_quote(src: &str) -> String {
    let at = src
        .find("function shellQuote(")
        .expect("Menu.qml defines shellQuote");
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
    panic!("shellQuote is not closed");
}

fn squeeze(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The algorithm is the reviewed one, unchanged.
///
/// This is the half that runs everywhere, including where no QML runtime is
/// installed. It cannot tell you the algorithm is correct; it can tell you
/// nobody has altered the one that was checked.
#[test]
fn the_quoting_is_the_reviewed_posix_form() {
    let found = squeeze(&extract_shell_quote(&menu_source()));
    assert_eq!(
        found,
        squeeze(REVIEWED),
        "shellQuote has been edited. It is the only thing standing between a \
         conversation id and the user's shell — read the change, then update \
         REVIEWED here deliberately."
    );
}

/// A QML runtime, if this machine has one.
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

/// The expression `copyResumeCommand` assigns to the clipboard, verbatim.
///
/// Taken from the source rather than written out here, so this test covers the
/// construction the plugin actually performs. A `shellQuote` that is correct
/// and a `copyResumeCommand` that forgets to call it is exactly the regression
/// worth catching.
fn extract_clipboard_expression(src: &str) -> String {
    let at = src
        .find("function copyResumeCommand(")
        .expect("Menu.qml defines copyResumeCommand");
    let body = &src[at..];
    let end = body.find("\n  }").expect("copyResumeCommand is closed");
    let line = body[..end]
        .lines()
        .find(|l| l.contains("clipboardText ="))
        .expect("copyResumeCommand assigns the clipboard");
    line.split_once("clipboardText =")
        .expect("an assignment")
        .1
        .trim()
        .to_string()
}

/// Run `Menu.qml`'s own clipboard construction over every hostile id, in Qt's
/// engine.
///
/// Returns the command line the menu would put on the clipboard for each.
fn clipboard_lines(qml: &Path, dir: &Path) -> Vec<String> {
    let src = menu_source();
    let function = extract_shell_quote(&src);
    let expression = extract_clipboard_expression(&src);
    let ids: Vec<String> = HOSTILE
        .iter()
        .map(|(_, id)| serde_json::to_string(id).unwrap())
        .collect();
    let harness = format!(
        "import QtQml\n\
         QtObject {{\n\
         id: root\n\
         {function}\n\
         Component.onCompleted: {{\n\
           var ids = [{}]\n\
           var out = []\n\
           for (var i = 0; i < ids.length; i++) {{\n\
             var entry = {{ \"native_id\": ids[i] }}\n\
             out.push({expression})\n\
           }}\n\
           console.warn(\"BEGIN\" + JSON.stringify(out) + \"END\")\n\
           Qt.exit(0)\n\
         }}\n\
         }}\n",
        ids.join(", ")
    );
    let file = dir.join("quote.qml");
    std::fs::write(&file, harness).unwrap();

    let out = common::run_bounded(
        std::process::Command::new(qml)
            // Qt hands qDebug/qWarning to the journal when stderr is not a
            // terminal, which is exactly what a test harness is.
            .env("QT_FORCE_STDERR_LOGGING", "1")
            .env("QT_QPA_PLATFORM", "offscreen")
            .arg(&file),
        std::time::Duration::from_secs(30),
    );
    let text =
        String::from_utf8_lossy(&out.stderr).into_owned() + &String::from_utf8_lossy(&out.stdout);
    let begin = text
        .find("BEGIN")
        .unwrap_or_else(|| panic!("the QML runtime printed no result: {text}"));
    let end = text.find("END").expect("a terminated result");
    let json = &text[begin + "BEGIN".len()..end];
    serde_json::from_str::<Vec<String>>(json).expect("the harness printed a JSON array")
}

/// A stub `osm` that records its arguments, NUL-separated, and nothing else.
///
/// NUL rather than newline because one of the ids below contains a newline —
/// which is a legal shell word inside single quotes, and a separator that
/// could not tell "one argument with a newline in it" from "two arguments"
/// would report the correct behaviour as a failure.
fn stub_osm(dir: &Path) -> PathBuf {
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let path = bin.join("osm");
    std::fs::write(
        &path,
        "#!/bin/sh\n\
         : > argv\n\
         for a in \"$@\"; do printf '%s\\0' \"$a\" >> argv; done\n",
    )
    .unwrap();
    let mut perm = std::fs::metadata(&path).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perm, 0o755);
    std::fs::set_permissions(&path, perm).unwrap();
    bin
}

/// Run one clipboard line the way a user would: pasted into a shell.
///
/// Contained on purpose — the working directory is this test's own temporary
/// one, and the only payload any hostile id here carries is `touch pwn`,
/// which lands there and is what the assertions look for. `/usr/bin` and
/// `/bin` stay on the `PATH` so that payload *can* run; a shell-injection
/// test whose payload could not fire would pass for the wrong reason.
fn paste_into_a_shell(line: &str, dir: &Path, bin: &Path) -> Vec<String> {
    let _ = std::fs::remove_file(dir.join("argv"));
    let _ = std::fs::remove_file(dir.join("pwn"));
    let status = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(line)
        .current_dir(dir)
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("HOME", dir)
        .status()
        .expect("run a shell");
    let _ = status;
    let Ok(raw) = std::fs::read(dir.join("argv")) else {
        return Vec::new();
    };
    String::from_utf8_lossy(&raw)
        .split('\0')
        .filter(|a| !a.is_empty())
        .map(str::to_string)
        .collect()
}

/// The command the menu copies survives a shell with the id intact, and does
/// nothing else.
#[test]
fn the_copied_resume_command_survives_a_shell() {
    let Some(qml) = qml_runtime() else {
        // Not a silent pass: say which half did not run, and why.
        eprintln!(
            "SKIPPED the executable half: no QML runtime found \
             (looked at $OSM_QML, /usr/lib/qt6/bin/qml, qml6 and qml on PATH). \
             the_quoting_is_the_reviewed_posix_form still pins the algorithm."
        );
        return;
    };

    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let bin = stub_osm(dir);
    let lines = clipboard_lines(&qml, dir);
    assert_eq!(lines.len(), HOSTILE.len());

    for ((what, id), line) in HOSTILE.iter().zip(&lines) {
        let argv = paste_into_a_shell(line, dir, &bin);
        assert_eq!(
            argv,
            vec!["resume".to_string(), "--".to_string(), (*id).to_string()],
            "{what}: `{line}` did not reach osm as one intact argument"
        );
        assert!(
            !dir.join("pwn").exists(),
            "{what}: `{line}` executed the payload in the id"
        );
    }
}

/// The harness can fail.
///
/// The same shell, the same stub, the same payloads — with the command line
/// the plugin used to copy, which pasted the id in raw. If this stops
/// detecting the injection then the test above is measuring nothing.
#[test]
fn the_unquoted_form_this_replaced_is_still_an_injection() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let bin = stub_osm(dir);

    let payload = "x; touch pwn #";
    let argv = paste_into_a_shell(&format!("osm resume {payload}"), dir, &bin);
    assert!(
        dir.join("pwn").exists(),
        "the old form did not fire its payload, so this harness cannot detect \
         an injection at all: osm saw {argv:?}"
    );

    // And the same id, quoted the way Menu.qml now does it, does not.
    let quoted = "osm resume -- 'x; touch pwn #'";
    let argv = paste_into_a_shell(quoted, dir, &bin);
    assert_eq!(argv, vec!["resume", "--", payload]);
    assert!(!dir.join("pwn").exists());
}
