use super::*;

/// Columns added or removed by one grow/shrink sidebar key press.
pub(super) const SIDEBAR_WIDTH_KEY_STEP: u16 = 2;

impl ClientShellState {
    /// Validated (min, max) sidebar width; falls back to (18, 36) when the configured
    /// bounds are rejected.
    pub(super) fn sidebar_width_bounds(&self) -> (u16, u16) {
        crate::config::validated_sidebar_bounds(
            self.config.sidebar_min_width,
            self.config.sidebar_max_width,
        )
        .unwrap_or((18, 36))
    }

    /// Clamp `width` into bounds, store it, mark it manual and request a repaint. A pane
    /// surface invalidation and resize are requested only while the docked sidebar is
    /// expanded, since a collapsed layout does not depend on the stored width.
    /// Returns true when the stored width changed. Does not persist.
    pub(super) fn set_sidebar_width(&mut self, width: u16, outcome: &mut ClientShellInput) -> bool {
        let (min, max) = self.sidebar_width_bounds();
        let width = width.clamp(min, max);
        if self.sidebar_width == width {
            return false;
        }
        self.sidebar_width = width;
        self.sidebar_width_manual = true;
        outcome.repaint = true;
        if !self.sidebar_layout_collapsed() {
            self.invalidate_pane_surface();
            outcome.resize = true;
        }
        true
    }

    /// Add `delta` columns (negative shrinks), clamp, apply through `set_sidebar_width`, and
    /// persist chrome preferences when the width changed.
    pub(super) fn step_sidebar_width(&mut self, delta: i16, outcome: &mut ClientShellInput) {
        let target =
            (i32::from(self.sidebar_width) + i32::from(delta)).clamp(0, i32::from(u16::MAX));
        let width = u16::try_from(target).unwrap_or(self.sidebar_width);
        if self.set_sidebar_width(width, outcome) {
            self.persist_chrome_preferences(outcome);
        }
    }
}
