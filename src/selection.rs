//! Text selection and clipboard support.
//!
//! Selection lifecycle:
//!
//!   MouseDown in pane → Anchor recorded (no visual yet)
//!   MouseDrag         → Selection becomes active, cells highlighted
//!   MouseUp           → Text extracted, copied via OSC 52, highlight stays
//!   Next click / key  → Selection cleared
//!
//! Coordinates are stored relative to the pane's inner area (the region
//! where terminal content is rendered, excluding borders).

use ratatui::layout::Rect;
use std::io::Write;

use crate::layout::PaneId;

/// Current phase of a selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Mouse is down but hasn't moved yet. If released without
    /// moving, this was just a click — no selection created.
    Anchored,
    /// Mouse has moved from the anchor point. Cells are being highlighted.
    Dragging,
    /// Mouse released after dragging. Selection is visible and text
    /// has been copied to clipboard. Cleared on next interaction.
    Done,
}

/// A text selection within a terminal pane.
#[derive(Debug, Clone)]
pub struct Selection {
    /// Which pane the selection belongs to.
    pub pane_id: PaneId,
    /// Anchor position in pane-relative coordinates (row, col).
    anchor: (u16, u16),
    /// Current/final position in pane-relative coordinates (row, col).
    cursor: (u16, u16),
    /// Selection phase.
    phase: Phase,
    /// The inner rect of the pane (for clamping during drag).
    /// This is the content area, excluding borders.
    pane_inner: Rect,
}

impl Selection {
    /// Start a potential selection. This records the anchor but doesn't
    /// make anything visible yet — the user might just be clicking.
    pub fn anchor(pane_id: PaneId, row: u16, col: u16, pane_inner: Rect) -> Self {
        Self {
            pane_id,
            anchor: (row, col),
            cursor: (row, col),
            phase: Phase::Anchored,
            pane_inner,
        }
    }

    /// Extend the selection as the mouse drags. Activates highlighting
    /// once the cursor moves to a different cell than the anchor.
    /// Screen coordinates are clamped to the pane boundary.
    pub fn drag(&mut self, screen_col: u16, screen_row: u16) {
        let (row, col) = self.clamp_to_pane(screen_col, screen_row);
        self.cursor = (row, col);
        if self.cursor != self.anchor {
            self.phase = Phase::Dragging;
        }
    }

    /// Finalize the selection. Returns the selected range if the user
    /// actually dragged (not just clicked). Returns None for plain clicks.
    pub fn finish(&mut self) -> bool {
        if self.phase == Phase::Dragging {
            self.phase = Phase::Done;
            true
        } else {
            false
        }
    }

    /// Whether this selection should be rendered (highlight visible).
    pub fn is_visible(&self) -> bool {
        self.phase == Phase::Dragging || self.phase == Phase::Done
    }

    /// Whether the user just clicked without dragging (not a selection).
    pub fn was_just_click(&self) -> bool {
        self.phase == Phase::Anchored
    }

    /// Returns (start, end) in reading order (top-left to bottom-right).
    fn ordered(&self) -> ((u16, u16), (u16, u16)) {
        let (ar, ac) = self.anchor;
        let (cr, cc) = self.cursor;
        if ar < cr || (ar == cr && ac <= cc) {
            ((ar, ac), (cr, cc))
        } else {
            ((cr, cc), (ar, ac))
        }
    }

    pub(crate) fn ordered_cells(&self) -> ((u16, u16), (u16, u16)) {
        self.ordered()
    }

    /// Check whether a pane-relative cell (row, col) is inside the selection.
    pub fn contains(&self, row: u16, col: u16) -> bool {
        if !self.is_visible() {
            return false;
        }
        let ((sr, sc), (er, ec)) = self.ordered();
        if row < sr || row > er {
            return false;
        }
        if sr == er {
            // Single-line: from sc to ec (inclusive)
            col >= sc && col <= ec
        } else if row == sr {
            // First line: from sc to end of line
            col >= sc
        } else if row == er {
            // Last line: from start to ec
            col <= ec
        } else {
            // Middle rows: fully selected
            true
        }
    }

    /// Clamp screen coordinates to the pane's inner area and convert
    /// to pane-relative coordinates.
    fn clamp_to_pane(&self, screen_col: u16, screen_row: u16) -> (u16, u16) {
        let r = &self.pane_inner;
        let clamped_col = screen_col.clamp(r.x, r.x + r.width.saturating_sub(1));
        let clamped_row = screen_row.clamp(r.y, r.y + r.height.saturating_sub(1));
        (clamped_row - r.y, clamped_col - r.x)
    }
}

/// Write text to the system clipboard via OSC 52.
///
/// OSC 52 format: `ESC ] 52 ; c ; <base64> BEL`
///
/// The terminal emulator (Ghostty, kitty, etc.) intercepts this
/// and sets the system clipboard. Ghostty has `clipboard-write = allow`
/// by default.
pub fn write_osc52(text: &str) {
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    // Use ST (ESC \) terminator — more widely supported than BEL in some terminals
    let sequence = format!("\x1b]52;c;{encoded}\x1b\\");
    let _ = std::io::stdout().write_all(sequence.as_bytes());
    let _ = std::io::stdout().flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_sel(sr: u16, sc: u16, er: u16, ec: u16) -> Selection {
        let mut sel = Selection::anchor(PaneId::from_raw(0), sr, sc, Rect::new(0, 0, 80, 24));
        sel.cursor = (er, ec);
        sel.phase = Phase::Dragging;
        sel
    }

    #[test]
    fn ordering_forward() {
        let sel = make_sel(2, 5, 4, 10);
        assert_eq!(sel.ordered(), ((2, 5), (4, 10)));
    }

    #[test]
    fn ordering_backward() {
        let sel = make_sel(4, 10, 2, 5);
        assert_eq!(sel.ordered(), ((2, 5), (4, 10)));
    }

    #[test]
    fn single_line_contains() {
        let sel = make_sel(2, 5, 2, 15);
        assert!(!sel.contains(2, 4));
        assert!(sel.contains(2, 5));
        assert!(sel.contains(2, 10));
        assert!(sel.contains(2, 15)); // inclusive
        assert!(!sel.contains(2, 16));
        assert!(!sel.contains(1, 10));
        assert!(!sel.contains(3, 10));
    }

    #[test]
    fn multi_line_contains() {
        let sel = make_sel(2, 5, 4, 10);
        // Row 2: from col 5 to end
        assert!(!sel.contains(2, 4));
        assert!(sel.contains(2, 5));
        assert!(sel.contains(2, 79));

        // Row 3: fully selected
        assert!(sel.contains(3, 0));
        assert!(sel.contains(3, 79));

        // Row 4: from start to col 10
        assert!(sel.contains(4, 0));
        assert!(sel.contains(4, 10));
        assert!(!sel.contains(4, 11));
    }

    #[test]
    fn anchored_not_visible() {
        let sel = Selection::anchor(PaneId::from_raw(0), 5, 10, Rect::new(0, 0, 80, 24));
        assert!(!sel.is_visible());
        assert!(!sel.contains(5, 10));
    }

    #[test]
    fn click_without_drag() {
        let mut sel = Selection::anchor(PaneId::from_raw(0), 5, 10, Rect::new(0, 0, 80, 24));
        assert!(sel.was_just_click());
        let copied = sel.finish();
        assert!(!copied);
    }

    #[test]
    fn drag_then_finish() {
        let mut sel = Selection::anchor(PaneId::from_raw(0), 5, 10, Rect::new(10, 5, 80, 24));
        sel.drag(20, 7); // screen coords → clamped to pane
        assert!(sel.is_visible());
        assert!(!sel.was_just_click());
        let copied = sel.finish();
        assert!(copied);
    }

    #[test]
    fn clamp_to_pane_bounds() {
        let sel = Selection::anchor(PaneId::from_raw(0), 0, 0, Rect::new(10, 5, 80, 24));
        // Drag way outside pane bounds
        let (row, col) = sel.clamp_to_pane(200, 100);
        assert_eq!(row, 23); // clamped to height - 1
        assert_eq!(col, 79); // clamped to width - 1

        // Drag left of pane
        let (row, col) = sel.clamp_to_pane(0, 0);
        assert_eq!(row, 0);
        assert_eq!(col, 0);
    }
}
