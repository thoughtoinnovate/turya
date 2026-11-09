//! Mouse selection over the transcript.
//!
//! Captured mouse modes bundle wheel events with clicks and drags — that is
//! the xterm protocol, not a choice — so selection cannot be left to the
//! terminal while the wheel stays usable. This implements what tmux and
//! crush do instead: app-level selection. Click sets an anchor, drag moves
//! the head, release copies the covered text to the system clipboard via
//! OSC 52. All coordinates are in the transcript's own space: absolute
//! wrapped rows (the same axis `row_offsets` and the viewport use) plus the
//! column inside the chat pane.

/// A mouse selection in absolute wrapped-row coordinates: the anchor (the
/// fixed end, at the press) and the head (the moving end, at release).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MouseSelection {
    pub(crate) anchor: (usize, usize),
    pub(crate) head: (usize, usize),
}

impl MouseSelection {
    pub(crate) fn from(cell: (usize, usize)) -> Self {
        Self {
            anchor: cell,
            head: cell,
        }
    }

    /// Endpoints ordered low-to-high: rows first, columns second. No matter
    /// which way the user dragged, the copy always runs start → end.
    pub(crate) fn ordered(&self) -> ((usize, usize), (usize, usize)) {
        if (self.anchor.0, self.anchor.1) <= (self.head.0, self.head.1) {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    /// The wrapped rows this selection covers, inclusive.
    pub(crate) fn rows(&self) -> (usize, usize) {
        (
            self.anchor.0.min(self.head.0),
            self.anchor.0.max(self.head.0),
        )
    }
}

/// Where the chat pane rendered its cells the last frame. Mouse events
/// carry screen coordinates; they are only meaningful through the frame
/// that produced them, so render publishes this and the mouse handlers
/// map through it — never through stale guesses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ChatGeometry {
    /// X of the pane's inner cell area (inside the border).
    pub(crate) x: u16,
    /// Y of the pane's inner cell area.
    pub(crate) y: u16,
    pub(crate) width: u16,
    pub(crate) height: u16,
    /// Absolute wrapped row shown at the top of the pane.
    pub(crate) off: usize,
}

impl ChatGeometry {
    /// The zero geometry: no render has mapped any coordinates yet, so
    /// every screen position is outside the chat pane.
    pub(crate) const EMPTY: Self = Self {
        x: 0,
        y: 0,
        width: 0,
        height: 0,
        off: 0,
    };
}

/// Encode text as an OSC 52 clipboard set: the one escape any terminal
/// may honour without the app ever owning the OS clipboard. BEL-terminated
/// so tmux and plain terminals parse it alike. Payload is capped because
/// terminals bound the sequence length.
pub(crate) fn osc52(text: &str) -> String {
    use base64::Engine as _;

    const MAX_TEXT_CHARS: usize = 100_000;
    let capped: String = text.chars().take(MAX_TEXT_CHARS).collect();
    format!(
        "\x1b]52;c;{}\x07",
        base64::engine::general_purpose::STANDARD.encode(capped.as_bytes())
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordering_survives_the_drag_direction() {
        let up = MouseSelection::from((40, 10));
        let mut sel = up.clone();
        sel.head = (5, 3);
        assert_eq!(sel.rows(), (5, 40));
        let (start, end) = sel.ordered();
        assert_eq!(start, (5, 3), "dragging up: head becomes the start");
        assert_eq!(end, (40, 10));
        assert!(sel.rows().0 <= 5 && 41 > sel.rows().1, "rows only");
    }

    #[test]
    fn osc52_wraps_the_payload_in_escape_codes() {
        let seq = osc52("hi");
        assert!(seq.starts_with("\x1b]52;c;"), "{seq:?}");
        assert!(seq.ends_with('\x07'), "{seq:?}");
        // "hi" in standard base64
        assert!(seq.contains("aGk="), "{seq:?}");
    }

    #[test]
    fn osc52_caps_pathological_payloads() {
        let big: String = std::iter::repeat_n('x', 500_000).collect();
        let seq = osc52(&big);
        // 100_000 chars, base64 ≈ 133k chars, comfortably inside typical
        // terminal limits and far under the 500k offered.
        let body = seq
            .trim_start_matches("\x1b]52;c;")
            .trim_end_matches('\x07');
        assert!(body.len() < 160_000, "payload must be capped");
    }
}
