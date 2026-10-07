use super::*;

impl ClientShellState {
    /// Docked (pane-geometry) collapse. With auto-hide on the docked layout stays collapsed
    /// unless the user pinned the sidebar open; otherwise it follows the stored collapse.
    /// Must not call `layout()` or `mobile_layout_active()`, which both depend on it.
    pub(super) fn sidebar_layout_collapsed(&self) -> bool {
        if self.config.sidebar_auto_hide {
            !self.sidebar_auto_hide_pinned
        } else {
            self.sidebar_collapsed
        }
    }

    /// True when the drawer is drawn over the panes this frame.
    pub(super) fn sidebar_overlay_visible(&self) -> bool {
        self.config.sidebar_auto_hide
            && !self.sidebar_auto_hide_pinned
            && !self.mobile_layout_active()
            && self.overlay.is_none()
            && self.popup_terminal_id.is_none()
            && (self.sidebar_hover_reveal || self.mode == ClientShellMode::Navigate)
    }

    /// What the sidebar looks like to the user: collapsed in the docked layout and not
    /// currently covered by the drawer.
    pub(super) fn sidebar_presented_collapsed(&self) -> bool {
        self.sidebar_layout_collapsed() && !self.sidebar_overlay_visible()
    }

    /// Layout used to render chrome while the drawer is visible: the expanded layout for the
    /// current width, drawn over the docked layout's unchanged pane surface.
    pub(super) fn sidebar_overlay_layout(
        &self,
        cols: u16,
        rows: u16,
        docked: ClientShellLayout,
    ) -> ClientShellLayout {
        let expanded = self.config.layout(
            cols,
            rows,
            false,
            self.focused_tab_count(),
            self.sidebar_width,
        );
        ClientShellLayout {
            pane_surface: docked.pane_surface,
            ..expanded
        }
    }

    /// Columns that open the drawer when the pointer moves onto them: the collapsed strip in
    /// compact mode, the left edge column in hidden mode.
    pub(super) fn sidebar_reveal_trigger_width(&self) -> u16 {
        match self.config.sidebar_collapsed_mode {
            crate::config::SidebarCollapsedModeConfig::Compact => 4,
            crate::config::SidebarCollapsedModeConfig::Hidden => 1,
        }
    }

    /// Opens the drawer when the pointer moves onto the trigger columns and closes it once the
    /// pointer leaves the drawer (unless its width is being dragged). Only repaints: hover
    /// never changes the docked layout, so it never resizes the panes.
    pub(super) fn update_sidebar_auto_reveal(
        &mut self,
        mouse: crossterm::event::MouseEvent,
        outcome: &mut ClientShellInput,
    ) {
        if !self.config.sidebar_auto_hide
            || self.sidebar_auto_hide_pinned
            || self.mobile_layout_active()
        {
            if self.sidebar_hover_reveal {
                self.sidebar_hover_reveal = false;
                outcome.repaint = true;
            }
            return;
        }
        let overlay = self.hits.sidebar_overlay;
        let dragging_width = matches!(self.chrome_drag, Some(ClientChromeDrag::SidebarWidth));
        if self.sidebar_hover_reveal {
            if !overlay.is_empty() && mouse.column >= overlay.right() && !dragging_width {
                self.sidebar_hover_reveal = false;
                outcome.repaint = true;
            }
        } else if mouse.kind == crossterm::event::MouseEventKind::Moved
            && mouse.column < self.sidebar_reveal_trigger_width()
        {
            self.sidebar_hover_reveal = true;
            outcome.repaint = true;
        }
    }

    /// True when this event lands on the drawer and no pane gesture is in flight, so pane hit
    /// rects beneath the drawer must not see it.
    pub(super) fn sidebar_overlay_masks_panes(&self, mouse: crossterm::event::MouseEvent) -> bool {
        let overlay = self.hits.sidebar_overlay;
        !overlay.is_empty()
            && super::contains(overlay, (mouse.column, mouse.row))
            && self.pane_mouse_gesture.is_none()
            && !self
                .selection
                .as_ref()
                .is_some_and(|selection| selection.is_in_progress())
            && !matches!(
                self.chrome_drag,
                Some(ClientChromeDrag::PaneSplit { .. } | ClientChromeDrag::PaneScrollbar { .. })
            )
    }

    /// Shared by the toggle keybind and the toggle button. With auto-hide on this pins or
    /// unpins the docked sidebar for the session without touching the saved preference.
    pub(super) fn toggle_sidebar(&mut self, outcome: &mut ClientShellInput) {
        if self.config.sidebar_auto_hide {
            self.sidebar_auto_hide_pinned = !self.sidebar_auto_hide_pinned;
            self.sidebar_hover_reveal = false;
            self.invalidate_pane_surface();
            outcome.repaint = true;
            outcome.resize = true;
            return;
        }
        self.sidebar_collapsed = !self.sidebar_collapsed;
        self.sidebar_collapsed_manual = true;
        self.reveal_navigation_workspace = true;
        self.invalidate_pane_surface();
        outcome.repaint = true;
        outcome.resize = true;
        self.persist_chrome_preferences(outcome);
    }
}
