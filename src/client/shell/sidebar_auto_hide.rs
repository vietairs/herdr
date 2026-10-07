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
            && !self.sidebar_drawer_blocked()
            && (self.sidebar_hover_reveal || self.mode == ClientShellMode::Navigate)
    }

    /// True when an open overlay or popup hides the drawer. Menus are exempt so a drawer that
    /// was already showing stays drawn beneath a menu opened from its rows or launcher; a menu
    /// never arms a new reveal (see `update_sidebar_auto_reveal`).
    fn sidebar_drawer_blocked(&self) -> bool {
        self.popup_terminal_id.is_some() || (self.overlay.is_some() && !self.sidebar_menu_open())
    }

    fn sidebar_menu_open(&self) -> bool {
        matches!(
            self.overlay,
            Some(ClientShellOverlay::ContextMenu(_) | ClientShellOverlay::GlobalMenu(_))
        )
    }

    /// Drops the hover reveal. Returns true when the drawer state changed.
    pub(super) fn clear_sidebar_hover_reveal(&mut self) -> bool {
        std::mem::take(&mut self.sidebar_hover_reveal)
    }

    /// Drops a hover reveal that an overlay or popup has hidden, so closing that overlay does
    /// not bring the drawer back with the pointer somewhere else.
    pub(super) fn clear_blocked_sidebar_hover_reveal(&mut self) -> bool {
        self.sidebar_drawer_blocked() && self.clear_sidebar_hover_reveal()
    }

    /// What the sidebar looks like to the user: collapsed in the docked layout and not
    /// currently covered by the drawer.
    pub(super) fn sidebar_presented_collapsed(&self) -> bool {
        self.sidebar_layout_collapsed() && !self.sidebar_overlay_visible()
    }

    /// Layout used to render chrome while the drawer is visible: the expanded sidebar for the
    /// current width, drawn over the docked layout's unchanged tab bar and pane surface.
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
            sidebar: expanded.sidebar,
            ..docked
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
    /// pointer leaves the drawer. A drag never closes it, and neither does any event while a
    /// sidebar gesture or a menu opened from the drawer is in flight. No overlay may arm a new
    /// reveal, so a pane or tab menu near the left edge never opens the drawer beneath it.
    /// Only repaints: hover never changes the docked layout, so it never resizes the panes.
    pub(super) fn update_sidebar_auto_reveal(
        &mut self,
        mouse: crossterm::event::MouseEvent,
        outcome: &mut ClientShellInput,
    ) {
        use crossterm::event::MouseEventKind;

        if !self.config.sidebar_auto_hide
            || self.sidebar_auto_hide_pinned
            || self.mobile_layout_active()
            || self.sidebar_drawer_blocked()
        {
            outcome.repaint |= self.clear_sidebar_hover_reveal();
            return;
        }
        if self.sidebar_hover_reveal {
            let overlay = self.hits.sidebar_overlay;
            let may_close = !overlay.is_empty()
                && !self.sidebar_menu_open()
                && match mouse.kind {
                    MouseEventKind::Drag(_) => false,
                    MouseEventKind::Up(_) => true,
                    _ => self.chrome_drag.is_none() && self.workspace_press.is_none(),
                };
            if may_close && mouse.column >= overlay.right() {
                self.sidebar_hover_reveal = false;
                outcome.repaint = true;
            }
        } else if mouse.kind == MouseEventKind::Moved
            && self.overlay.is_none()
            && mouse.column < self.sidebar_reveal_trigger_width()
        {
            self.sidebar_hover_reveal = true;
            outcome.repaint = true;
        }
    }

    /// A click outside the drawer that closes a menu opened from it closes the drawer too,
    /// as moving the pointer away would.
    pub(super) fn close_sidebar_drawer_after_menu_click(&mut self, column: u16) {
        let overlay = self.hits.sidebar_overlay;
        if !overlay.is_empty() && column >= overlay.right() {
            self.sidebar_hover_reveal = false;
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

    /// The drawer's collapse button hides the drawer; unlike the toggle keybind it never docks
    /// the sidebar, so the panes keep their size.
    pub(super) fn hide_sidebar_drawer(&mut self, outcome: &mut ClientShellInput) {
        self.sidebar_hover_reveal = false;
        if self.mode == ClientShellMode::Navigate {
            self.mode = self.copy_or_terminal_mode();
            self.navigate_workspace_id = None;
        }
        outcome.repaint = true;
    }

    /// Shared by the toggle keybind and the docked toggle button. With auto-hide on this pins
    /// or unpins the docked sidebar for the session without touching the saved preference.
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

/// Drops or trims tab bar hits that the drawer covers, so a click on the drawer never
/// reaches a tab drawn beneath it.
pub(super) fn clip_tab_hits_under_sidebar_overlay(hits: &mut ShellHitMap) {
    if hits.sidebar_overlay.is_empty() {
        return;
    }
    let edge = hits.sidebar_overlay.right();
    hits.tabs.retain_mut(|(rect, _)| {
        if rect.x < edge {
            rect.width = rect.right().saturating_sub(edge);
            rect.x = edge;
        }
        rect.width > 0
    });
    for button in [
        &mut hits.new_tab,
        &mut hits.tab_scroll_left,
        &mut hits.tab_scroll_right,
    ] {
        if button.x < edge {
            *button = Rect::default();
        }
    }
}
