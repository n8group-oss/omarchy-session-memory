//! Framing and escaping of `tmux … -F` output.
//!
//! Both separators are printable ASCII, because tmux ≤ 3.6 rewrites every
//! non-printable byte in format output to `_`. Printable means a user can
//! type it, so nothing is left to chance: tmux escapes every field before it
//! is framed and the parser decodes it back. A window called
//! `work<|osm:f|>prod` or a directory holding a newline is data, not an
//! outage.

use osm::tmux;
use osm::tmux::{encode_field, FIELD_SEP as SEP, REC_SEP as REC};

/// tmux <= 3.6 sanitises format output before printing it: every byte that
/// is not printable ASCII (or a space) is replaced with `_`. Modelled here
/// so the separator's survival is checked on every machine, not only on a
/// box that happens to run an old tmux.
///
/// Verified against the real thing — the same format string on the same
/// server produced `$0\x1falpha` on tmux 3.7c and `$0_alpha` on tmux 3.3a.
fn sanitize_like_old_tmux(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_graphic() || b == b' ' || b == b'\n' {
                b as char
            } else {
                '_'
            }
        })
        .collect()
}

/// One record exactly as tmux emits it: the record separator, the escaped
/// fields joined by the field separator, and tmux's own trailing newline.
fn record(fields: &[&str]) -> String {
    let escaped: Vec<String> = fields.iter().map(|f| encode_field(f)).collect();
    format!("{REC}{}\n", escaped.join(SEP))
}

#[test]
fn separator_survives_the_output_sanitisation_of_old_tmux() {
    let input = format!(
        "{}{}",
        record(&["$0", "alpha"]),
        record(&["$1", "my notes"])
    );
    let sanitized = sanitize_like_old_tmux(&input);
    let out = tmux::parse_sessions(&sanitized)
        .expect("a separator mangled by old tmux collapses every record into one field");
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].id, "$0");
    assert_eq!(out[0].name, "alpha");
    assert_eq!(out[1].name, "my notes");
}

#[test]
fn separators_are_printable_ascii_and_multi_character() {
    for sep in [SEP, REC] {
        assert!(
            sep.bytes().all(|b| b.is_ascii_graphic()),
            "a non-printable separator is rewritten to _ by tmux <= 3.6"
        );
        assert!(sep.len() > 1, "a single character is far too easy to hit");
        assert!(
            !sep.contains('#'),
            "# would be parsed by tmux as a format expansion"
        );
    }
    assert_ne!(SEP, REC);
}

#[test]
fn parses_session_lines() {
    let input = format!("{}{}", record(&["$0", "dev"]), record(&["$3", "notes"]));
    let out = tmux::parse_sessions(&input).unwrap();
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].id, "$0");
    assert_eq!(out[0].name, "dev");
    assert_eq!(out[1].id, "$3");
}

#[test]
fn parses_window_lines_with_flags() {
    // Field 5 is `automatic-rename`, between the zoom flag and the name: `1`
    // when tmux owns the window's name, `0` when the user set it. It is what
    // tells a name that is *identity* from one tmux is merely deriving from
    // the foreground command.
    let input = format!(
        "{}{}",
        record(&["$0", "@1", "1", "1", "0", "1", "main", "c2b4,80x24,0,0,0"]),
        record(&["$0", "@2", "2", "0", "1", "0", "logs", "c2b4,80x24,0,0,1"]),
    );
    let out = tmux::parse_windows(&input).unwrap();
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].session_id, "$0");
    assert_eq!(out[0].id, "@1");
    assert_eq!(out[0].idx, 1);
    assert_eq!(out[0].name, "main");
    assert_eq!(out[0].layout, "c2b4,80x24,0,0,0");
    assert!(out[0].active);
    assert!(!out[0].zoomed);
    assert!(out[0].auto_named, "tmux owns @1's name");
    assert!(!out[1].active);
    assert!(out[1].zoomed);
    assert_eq!(out[1].name, "logs");
    assert!(!out[1].auto_named, "the user set @2's name");
}

#[test]
fn parses_pane_lines_preserving_paths_with_spaces() {
    let input = record(&[
        "@1",
        "%0",
        "0",
        "1",
        "0",
        "4242",
        "/home/u/my projects/app",
        "claude",
        "node",
    ]);
    let out = tmux::parse_panes(&input).unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].window_id, "@1");
    assert_eq!(out[0].pid, 4242);
    assert_eq!(out[0].id, "%0");
    assert!(out[0].active);
    assert!(!out[0].dead);
    assert_eq!(out[0].cwd, "/home/u/my projects/app");
    assert_eq!(out[0].title, "claude");
    assert_eq!(out[0].cmd, "node");
}

#[test]
fn empty_output_yields_empty_vec() {
    assert!(tmux::parse_sessions("").unwrap().is_empty());
    assert!(tmux::parse_windows("\n").unwrap().is_empty());
}

#[test]
fn malformed_record_is_an_error_not_a_panic() {
    let err = tmux::parse_panes(&record(&["not-enough-fields"])).unwrap_err();
    assert!(err.to_string().contains("expected 9 fields"));
}

/// Output that never went through the escaping substitutions — an ancient
/// tmux with no `s///` format modifier — must fail loudly rather than be
/// parsed as if it had.
#[test]
fn unexpanded_format_output_is_an_error() {
    let err = tmux::parse_sessions("$0<|osm:f|>alpha\n").unwrap_err();
    assert!(
        err.to_string()
            .contains("before the first record separator"),
        "got {err}"
    );
}

/// The regression this framing exists for. Every one of these used to make
/// the capture that saw it — and every capture after it — fail its
/// field-count check.
#[test]
fn free_text_containing_a_separator_round_trips() {
    for value in [
        "work<|osm:f|>prod",
        "work<|osm:r|>prod",
        "<|osm",
        "a<|osm:f|>b<|osm:r|>c",
        "~",
        "~~",
        "~L",
        "<|~osm",
        "<|~~osm",
        "~L:f|>",
        "trailing~",
    ] {
        let input = record(&["$0", value]);
        let out = tmux::parse_sessions(&input)
            .unwrap_or_else(|e| panic!("value {value:?} must parse, got {e}"));
        assert_eq!(out.len(), 1, "value {value:?}");
        assert_eq!(out[0].name, value, "value {value:?} must survive verbatim");
    }
}

/// Newlines are data, not framing. A directory name may hold one (POSIX
/// allows any byte but `/` and NUL) and tmux ≥ 3.7 reports it verbatim.
#[test]
fn free_text_containing_a_newline_round_trips() {
    for value in ["/tmp/a\nb", "\nleading", "trailing\n", "a\n\nb"] {
        let input = record(&["@1", "%0", "0", "1", "0", "1", value, "title", "sh"]);
        let out = tmux::parse_panes(&input)
            .unwrap_or_else(|e| panic!("cwd {value:?} must parse, got {e}"));
        assert_eq!(out.len(), 1, "cwd {value:?}");
        assert_eq!(out[0].cwd, value, "cwd {value:?} must survive verbatim");
    }
}

/// Two records where the first one's last field ends in a newline: the
/// framing must strip tmux's own terminator and nothing else.
#[test]
fn a_value_ending_in_a_newline_does_not_eat_the_next_record() {
    let input = format!(
        "{}{}",
        record(&["$0", "ends-in\n"]),
        record(&["$1", "second"])
    );
    let out = tmux::parse_sessions(&input).unwrap();
    assert_eq!(out.len(), 2, "{out:?}");
    assert_eq!(out[0].name, "ends-in\n");
    assert_eq!(out[1].name, "second");
}

#[test]
fn encoded_fields_never_contain_a_separator() {
    for value in [
        "work<|osm:f|>prod",
        "work<|osm:r|>prod",
        "<|osm",
        "~L<|osm:f|>",
    ] {
        let encoded = encode_field(value);
        assert!(
            !encoded.contains(SEP) && !encoded.contains(REC) && !encoded.contains("<|osm"),
            "encoding {value:?} left a live separator in {encoded:?}"
        );
    }
}
