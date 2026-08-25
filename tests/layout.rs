//! The layout validator that stands between a stored layout string and
//! `tmux select-layout`.
//!
//! Everything a restore replays into tmux is built from typed fields except
//! the window layout, which is copied out of `#{window_layout}` and copied
//! back in verbatim. What tmux does with a bad one is version-dependent and
//! the blast radius of the bad cases is the whole server, so the string is
//! checked here before tmux ever sees it. These cases are pure string
//! parsing: no tmux server is involved, and none may be started.

use osm::layout::{self, LayoutNode};

/// Layouts as tmux really emits them.
#[test]
fn real_tmux_layouts_parse() {
    for layout in [
        // Single pane.
        "c5f8,80x24,0,0,0",
        // Two panes side by side (horizontal split).
        "d1b5,80x24,0,0{40x24,0,0,0,39x24,41,0,1}",
        // Two panes stacked (vertical split).
        "b3a1,80x24,0,0[80x12,0,0,0,80x11,0,13,1]",
        // A five-pane window with both split directions nested, the shape the
        // e2e reboot test builds.
        "9f2c,200x50,0,0{100x50,0,0[100x25,0,0,0,100x24,0,26,1],99x50,101,0[99x5,101,0,2,99x44,101,6,3]}",
        // Large pane ids, which tmux hands out after a long uptime.
        "0000,80x24,0,0,4294967295",
    ] {
        assert!(
            layout::is_valid(layout),
            "tmux emits this layout, so it must validate: {layout:?}"
        );
    }
}

#[test]
fn a_valid_layout_reports_its_size_and_pane_count() {
    let node = layout::parse("d1b5,80x24,0,0{40x24,0,0,0,39x24,41,0,1}").unwrap();
    assert_eq!(node.size(), (80, 24));
    assert_eq!(node.panes(), 2);
    assert!(matches!(node, LayoutNode::Split { bracket: b'{', .. }));
}

/// Every one of these would be handed straight to `select-layout` without the
/// validator. Each is a real way a stored layout goes wrong: truncation,
/// corruption, a hand-edited database row, a field that is not a number.
#[test]
fn malformed_layouts_are_rejected() {
    for (layout, why) in [
        ("", "empty string"),
        ("not-a-layout", "no comma at all"),
        ("80x24,0,0,0", "no checksum prefix"),
        ("zzzz,80x24,0,0,0", "checksum is not hexadecimal"),
        ("d1b5,80x24,0,0{40x24,0,0,0,39x24,41,0,1", "unclosed brace"),
        (
            "d1b5,80x24,0,0{40x24,0,0,0,39x24,41,0,1]",
            "mispaired brackets",
        ),
        ("d1b5,80x24,0,0{}", "empty split"),
        ("d1b5,80x24,0,0", "leaf with no pane id"),
        ("d1b5,80x,0,0,0", "missing height"),
        ("d1b5,80x24,0,0,0}", "trailing input"),
        ("d1b5,80x24,0,0,0,junk", "trailing input after the pane id"),
        ("d1b5,-80x24,0,0,0", "negative width"),
        ("d1b5,4294967296x24,0,0,0", "width overflows 32 bits"),
        (
            "d1b5,80x24,0,0{40x24,0,0,0,}",
            "missing sibling after a comma",
        ),
    ] {
        assert!(
            layout::parse(layout).is_err(),
            "{why} must be rejected, but {layout:?} validated"
        );
    }
}

/// The parser is recursive, so a corrupted row of nested braces must be
/// rejected on a depth counter rather than by overflowing osm's own stack —
/// which would be the same crash this module exists to keep out of tmux, just
/// relocated into this process.
#[test]
fn absurd_nesting_is_rejected_without_overflowing_the_stack() {
    let mut body = String::from("80x24,0,0,0");
    for _ in 0..50_000 {
        body = format!("80x24,0,0{{{body}}}");
    }
    let layout = format!("d1b5,{body}");
    assert!(
        layout::parse(&layout).is_err(),
        "50k levels of nesting must be rejected, not parsed"
    );
}

/// The parse error has to name a position, because the operator's next step
/// after "this snapshot has a bad layout" is finding it in the database.
#[test]
fn a_rejection_says_where() {
    let err = layout::parse("d1b5,80x24,0,junk,0").unwrap_err();
    assert!(err.at > 0, "error must carry a byte offset: {err:?}");
    let text = err.to_string();
    assert!(
        text.contains("byte"),
        "error text must locate the problem, got {text:?}"
    );
}
