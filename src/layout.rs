//! Strict parsing — and therefore validation — of tmux layout strings.
//!
//! # Why this is a library module and not a test helper
//!
//! A captured layout is the one piece of a snapshot that osm hands back to
//! tmux *verbatim*: `select-layout -t <window> <string>`. Everything else it
//! replays is built from typed fields (a name, an index, a directory), but the
//! layout is opaque text copied out of `#{window_layout}` and copied back in.
//! If that text is wrong, whatever tmux does with it happens to the user's
//! whole server.
//!
//! What tmux does with it is **version-dependent**. On tmux 3.7c a bad layout
//! is a clean per-command error (`invalid layout: …`, exit 1, server
//! untouched). Older tmux is where the risk lives: Debian bookworm ships 3.3a
//! and Ubuntu ships 3.4, so most of the people this engine is for are running
//! the versions with the least defensive layout code, and a restore that takes
//! the server down destroys every session it had *already* rebuilt plus every
//! live session it had adopted. The database is not a trusted input either: it
//! can be corrupted, hand-edited, or written by a newer tmux whose layout
//! dialect an older one rejects.
//!
//! So the string is validated here, in osm, before tmux ever sees it. A layout
//! that does not parse is skipped — the window still gets all of its panes,
//! it just does not get its captured geometry — and the restore reports itself
//! degraded rather than complete (see [`crate::restore::SkippedLayout`]).
//!
//! # What "valid" means here
//!
//! The grammar tmux emits (`layout-custom.c`):
//!
//! ```text
//! layout  := checksum "," node
//! node    := WxH "," x "," y ( "{" node ("," node)* "}"
//!                            | "[" node ("," node)* "]"
//!                            | "," pane-id )
//! ```
//!
//! `{}` is a horizontal split (children side by side), `[]` a vertical one.
//! This module checks exactly that: a checksum, well-formed `WxH,x,y` triples
//! throughout, balanced and correctly paired brackets, and **no trailing
//! input**. It deliberately does not re-check what only tmux can know — that
//! the cell count matches the window's pane count, or that the children tile
//! their parent exactly. Those tmux reports as ordinary command errors, which
//! the caller also treats as non-fatal.
//!
//! The checksum is checked for *shape* (a short run of hex digits), not
//! recomputed. Recomputing it would reject every layout tmux itself accepts
//! after a manual edit and buys nothing: the checksum guards against
//! truncation in transit, and this parser catches truncation directly.

use std::fmt;

/// Nesting depth this parser will accept.
///
/// The parser recurses once per nested split, so an attacker-supplied or
/// corrupted string of ten thousand `{`s would otherwise overflow osm's own
/// stack — the exact class of failure this module exists to prevent, just
/// moved from tmux's process into ours. Real layouts nest once per *change of
/// split direction*, so even a pathological hand-built window stays in the low
/// tens.
const MAX_DEPTH: usize = 256;

/// Longest checksum prefix accepted. tmux writes `%04x` of a `u_short`, i.e.
/// always exactly four hex digits; the extra headroom costs nothing and means
/// a future tmux that widens the field does not make every layout unrestorable.
const MAX_CHECKSUM_LEN: usize = 8;

/// One tmux layout node with the volatile parts dropped: the leading checksum
/// and each leaf's trailing pane id are both reassigned on every restore, so a
/// perfectly restored window never reproduces the captured string byte for
/// byte. What is left — the nesting of horizontal (`{`) and vertical (`[`)
/// splits and every `WxH,x,y` — is the geometry the layout actually describes,
/// which is what may be compared between two servers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayoutNode {
    Pane {
        w: u32,
        h: u32,
        x: u32,
        y: u32,
    },
    Split {
        /// `b'{'` for a horizontal split, `b'['` for a vertical one.
        bracket: u8,
        w: u32,
        h: u32,
        x: u32,
        y: u32,
        children: Vec<LayoutNode>,
    },
}

impl LayoutNode {
    /// How many panes this subtree holds.
    pub fn panes(&self) -> usize {
        match self {
            LayoutNode::Pane { .. } => 1,
            LayoutNode::Split { children, .. } => children.iter().map(LayoutNode::panes).sum(),
        }
    }

    /// The node's own `WxH`.
    pub fn size(&self) -> (u32, u32) {
        match *self {
            LayoutNode::Pane { w, h, .. } => (w, h),
            LayoutNode::Split { w, h, .. } => (w, h),
        }
    }
}

/// Why a string is not a tmux layout. Carries the byte offset so a corrupted
/// row can actually be found rather than merely described.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutError {
    pub at: usize,
    pub what: String,
}

impl fmt::Display for LayoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} at byte {}", self.what, self.at)
    }
}

impl std::error::Error for LayoutError {}

struct Cursor<'a> {
    s: &'a [u8],
    i: usize,
    /// Byte offset of `s` within the original layout string, so error
    /// positions point into what the caller passed rather than into the
    /// checksum-stripped remainder.
    base: usize,
}

impl Cursor<'_> {
    fn err(&self, what: impl Into<String>) -> LayoutError {
        LayoutError {
            at: self.base + self.i,
            what: what.into(),
        }
    }

    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn eat(&mut self, want: u8) -> Result<(), LayoutError> {
        if self.peek() == Some(want) {
            self.i += 1;
            Ok(())
        } else {
            Err(self.err(format!(
                "expected {:?}, found {}",
                want as char,
                match self.peek() {
                    Some(b) => format!("{:?}", b as char),
                    None => "end of string".to_string(),
                }
            )))
        }
    }

    fn number(&mut self) -> Result<u32, LayoutError> {
        let start = self.i;
        while matches!(self.peek(), Some(b) if b.is_ascii_digit()) {
            self.i += 1;
        }
        if start == self.i {
            return Err(self.err("expected a number"));
        }
        // Only ASCII digits were consumed, so the slice is valid UTF-8 and the
        // only way `parse` fails is an overflowing u32 — which is a rejection,
        // not a panic.
        std::str::from_utf8(&self.s[start..self.i])
            .map_err(|_| self.err("number is not valid UTF-8"))?
            .parse()
            .map_err(|_| LayoutError {
                at: self.base + start,
                what: "number does not fit in 32 bits".to_string(),
            })
    }

    fn node(&mut self, depth: usize, ids: &mut Vec<u32>) -> Result<LayoutNode, LayoutError> {
        if depth > MAX_DEPTH {
            return Err(self.err(format!("layout nests deeper than {MAX_DEPTH} levels")));
        }
        let w = self.number()?;
        self.eat(b'x')?;
        let h = self.number()?;
        self.eat(b',')?;
        let x = self.number()?;
        self.eat(b',')?;
        let y = self.number()?;
        match self.peek() {
            Some(open @ (b'{' | b'[')) => {
                self.i += 1;
                let close = if open == b'{' { b'}' } else { b']' };
                let mut children = vec![self.node(depth + 1, ids)?];
                while self.peek() == Some(b',') {
                    self.i += 1;
                    children.push(self.node(depth + 1, ids)?);
                }
                // Mispaired brackets (`{…]`) land here, which is the point:
                // this is what makes nesting *balanced* and not merely
                // counted.
                self.eat(close)?;
                Ok(LayoutNode::Split {
                    bracket: open,
                    w,
                    h,
                    x,
                    y,
                    children,
                })
            }
            _ => {
                // Leaf: the trailing ",<pane id>" is kept, in order, by
                // [`parse_with_pane_ids`]. It is deliberately *not* part of
                // [`LayoutNode`] — tmux assigns fresh pane ids on every
                // restore, so two servers holding the same window produce
                // different ids and geometry comparison has to ignore them.
                // What the ids are good for is the other half of the job:
                // saying *which* pane occupies each cell, which is the only
                // way to tell a window from the same window with two of its
                // panes swapped.
                self.eat(b',')?;
                ids.push(self.number()?);
                Ok(LayoutNode::Pane { w, h, x, y })
            }
        }
    }
}

/// Parses a full tmux layout string (`checksum,WxH,x,y…`) into its geometry.
///
/// The whole string must be consumed: a layout with anything after the root
/// node is rejected rather than silently truncated, because "the prefix
/// parsed" is exactly the reasoning that would hand a corrupted tail to tmux.
pub fn parse(layout: &str) -> Result<LayoutNode, LayoutError> {
    parse_with_pane_ids(layout).map(|(node, _)| node)
}

/// [`parse`], plus the pane id of every leaf **in layout order**.
///
/// Layout order is cell order: tmux writes the leaves depth-first, which is
/// also the order `select-layout` assigns cells and therefore the order in
/// which it renumbers pane indices. So the *n*th id here is the pane occupying
/// the *n*th cell, on either server, whatever each of them happens to call its
/// panes — which is what makes two windows comparable pane by pane instead of
/// as an unordered bag of values.
pub fn parse_with_pane_ids(layout: &str) -> Result<(LayoutNode, Vec<u32>), LayoutError> {
    let Some((checksum, rest)) = layout.split_once(',') else {
        return Err(LayoutError {
            at: 0,
            what: "layout has no checksum prefix".to_string(),
        });
    };
    if checksum.is_empty()
        || checksum.len() > MAX_CHECKSUM_LEN
        || !checksum.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(LayoutError {
            at: 0,
            what: format!("{checksum:?} is not a hexadecimal layout checksum"),
        });
    }
    let base = checksum.len() + 1;
    let mut cursor = Cursor {
        s: rest.as_bytes(),
        i: 0,
        base,
    };
    let mut ids = Vec::new();
    let node = cursor.node(0, &mut ids)?;
    if cursor.i != cursor.s.len() {
        return Err(cursor.err("trailing input after the layout"));
    }
    Ok((node, ids))
}

/// Whether `layout` is safe to hand to `tmux select-layout`.
pub fn is_valid(layout: &str) -> bool {
    parse(layout).is_ok()
}
