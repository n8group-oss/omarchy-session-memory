//! A corrupted layout string must cost a comparison its precision, never the
//! process.
//!
//! [`osm::equiv::window_shape`] puts a window's panes into the order the
//! window's own layout describes, which is what lets two servers be compared
//! cell by cell. The layout is not a trusted input: a snapshot row can be
//! corrupted or hand-edited, so it can name one pane id twice, or name an id
//! no pane has, while still parsing as a perfectly well-formed tmux layout.
//!
//! A duplicate id used to panic — the second cell claiming pane `0` unwrapped
//! a slot the first had already emptied. That panic landed in the two code
//! paths that exist to protect the user's state: every adoption, and every
//! discharge of carry debt. Both then abort, so the snapshot is neither
//! adopted nor carried and the process dies mid-run.
//!
//! No tmux server is started here: this is the pure shape layer.

use osm::equiv::{self, RawPane};

/// Two panes side by side, `w` and `h` chosen so the string is a real layout.
/// `first` and `second` are the pane ids its two leaves name.
fn layout(first: u32, second: u32) -> String {
    format!("abcd,200x50,0,0{{100x50,0,0,{first},99x50,101,0,{second}}}")
}

fn panes() -> Vec<RawPane> {
    vec![
        RawPane {
            id: "%0".to_string(),
            idx: 0,
            cwd: "/first".to_string(),
            active: true,
        },
        RawPane {
            id: "%1".to_string(),
            idx: 1,
            cwd: "/second".to_string(),
            active: false,
        },
    ]
}

fn ordered(layout: &str) -> Vec<(String, String)> {
    let shape = equiv::window_shape(
        "@0".to_string(),
        0,
        "code".to_string(),
        layout.to_string(),
        false,
        panes(),
    );
    shape
        .panes
        .into_iter()
        .map(|p| (p.id, p.cwd))
        .collect::<Vec<_>>()
}

fn by_index() -> Vec<(String, String)> {
    vec![
        ("%0".to_string(), "/first".to_string()),
        ("%1".to_string(), "/second".to_string()),
    ]
}

/// The control, and it is load-bearing: without it the two checks below would
/// pass against an implementation that never reorders anything at all.
#[test]
fn a_sound_layout_still_puts_the_panes_in_cell_order() {
    let l = layout(1, 0);
    assert!(
        osm::layout::parse_with_pane_ids(&l).is_ok(),
        "the fixture must be a layout tmux itself would accept"
    );
    assert_eq!(
        ordered(&l),
        vec![
            ("%1".to_string(), "/second".to_string()),
            ("%0".to_string(), "/first".to_string()),
        ],
        "cell order, not index order, is what the layout describes"
    );
}

/// Both cells name pane `0`. The layout parses, so the panic was reachable
/// from any snapshot row, and the pane count matches, so nothing earlier
/// rejected it.
#[test]
fn a_duplicate_layout_leaf_id_falls_back_instead_of_panicking() {
    let l = layout(0, 0);
    assert!(
        osm::layout::parse_with_pane_ids(&l).is_ok(),
        "the fixture must parse, or this test proves nothing"
    );
    assert_eq!(
        ordered(&l),
        by_index(),
        "a layout that does not identify the panes cannot order them either; \
         the comparison falls back to index order"
    );
}

/// A cell naming a pane that is not in the window. Same fallback, and the
/// case that already worked — kept so the permutation check cannot regress
/// into accepting it.
#[test]
fn a_layout_leaf_naming_no_pane_falls_back_too() {
    let l = layout(0, 5);
    assert!(osm::layout::parse_with_pane_ids(&l).is_ok());
    assert_eq!(ordered(&l), by_index());
}
