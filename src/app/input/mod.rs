//! Input handling — translates crossterm key/mouse events into state mutations.

use bytes::Bytes;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
// Every `debug!` site in this module sits inside a `#[cfg(unix)]` remote-paste
// helper, so the import would be unused on non-Unix targets.
#[cfg(unix)]
use tracing::debug;
use tracing::warn;

use crate::app::PaneClickState;
use crate::input::TerminalKey;
#[cfg(test)]
use ratatui::layout::Direction;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScrollbarClickTarget {
    Thumb { grab_row_offset: u16 },
    Track { offset_from_bottom: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg(test)]
enum WheelRouting {
    HostScroll,
    MouseReport,
    AlternateScroll,
}

const WORKSPACE_DRAG_THRESHOLD: u16 = 1;
const TAB_DRAG_THRESHOLD: u16 = 1;

fn modified_url_click_modifier() -> KeyModifiers {
    KeyModifiers::CONTROL
}

#[cfg(test)]
#[test]
fn modified_url_click_modifier_matches_terminal_mouse_reporting() {
    assert_eq!(modified_url_click_modifier(), KeyModifiers::CONTROL);
}

mod clipboard;
mod copy_mode;
mod lease;
mod modal;
mod mouse;
mod navigate;
mod overlays;
mod selection;
mod settings;
mod sidebar;
mod terminal;

pub(crate) use self::{
    lease::{ConsumedInputLease, ForwardedInputLease, InputLeaseKey, InputLeaseTable, RepeatPlan},
    modal::{
        handle_global_menu_key, handle_keybind_help_key, handle_navigator_key,
        insert_keybind_help_query_text, insert_navigator_search_text, insert_rename_input_text,
        open_new_workspace_dialog,
    },
    navigate::{
        terminal_direct_indexed_navigation_action, terminal_direct_non_indexed_navigation_action,
    },
    settings::open_settings_at,
};
use self::{
    modal::{
        modal_action_from_key, ModalAction, ONBOARDING_WELCOME_ACTIONS, RELEASE_NOTES_ACTIONS,
    },
    mouse::MouseAction,
    settings::SettingsAction,
};
use super::state::{AppState, Mode};
use super::App;

// ---------------------------------------------------------------------------
// Key handling
// ---------------------------------------------------------------------------

impl App {
    pub(super) async fn handle_key(
        &mut self,
        key: TerminalKey,
    ) -> Option<super::TerminalInputTarget> {
        if self.state.popup_pane.is_some() {
            return self.handle_terminal_key(key).await;
        }
        let key_event = key.as_key_event();
        if modal_paste_target_active(&self.state) && is_modal_paste_shortcut(&key_event) {
            if let Some(text) = crate::platform::read_clipboard_text() {
                self.paste_into_active_text_input(&text);
            }
            return None;
        }

        // A `keys.remote_image_paste` press only belongs to this intercept
        // when the focused pane really is a pane of a live mounted remote
        // workspace whose peer can stage files. Anything else falls through
        // untouched into the mode dispatch below, so a local `ctrl+v` still
        // reaches the pane app that wants it (readline quoted-insert, vim
        // visual-block) and every non-terminal mode keeps its own keymap.
        #[cfg(unix)]
        if self.dispatch_remote_image_paste_key(&key) == RemoteImagePasteKeyDisposition::Consume {
            return None;
        }

        match self.state.mode {
            Mode::Terminal => return self.handle_terminal_key(key).await,
            Mode::Prefix => self.handle_prefix_key(key),
            Mode::Navigate => self.handle_navigate_key(key),
            Mode::Copy => self.handle_copy_mode_key(key),
            _ => match self.state.mode {
                Mode::Onboarding => self.handle_onboarding_key(key_event),
                Mode::ReleaseNotes => self.handle_release_notes_key(key_event),
                Mode::ProductAnnouncement => self.handle_product_announcement_key(key_event),
                Mode::Prefix | Mode::Navigate | Mode::Copy => unreachable!(),
                Mode::RenameWorkspace | Mode::RenameTab | Mode::RenamePane => {
                    self.handle_rename_key_via_api(key_event)
                }
                Mode::NewLinkedWorktree => self.handle_worktree_create_key(key_event),
                Mode::OpenExistingWorktree => self.handle_worktree_open_key(key_event),
                Mode::ConfirmRemoveWorktree => self.handle_worktree_remove_key(key_event),
                Mode::MountRemoteWorkspace => self.handle_remote_mount_key(key_event),
                Mode::Resize => self.handle_resize_key_via_api(key),
                Mode::ConfirmClose => self.handle_confirm_close_key_via_api(key_event),
                Mode::ContextMenu => {
                    self.handle_context_menu_key_via_api(key_event);
                }
                Mode::Settings => self.handle_settings_key(key_event),
                Mode::GlobalMenu => handle_global_menu_key(&mut self.state, key_event),
                Mode::KeybindHelp => handle_keybind_help_key(&mut self.state, key),
                Mode::Navigator => {
                    handle_navigator_key(&mut self.state, &self.terminal_runtimes, key_event)
                }
                Mode::Terminal => unreachable!(),
            },
        }
        None
    }

    pub(crate) fn handle_text_commit_headless(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if self.state.popup_pane.is_some() {
            if let Some(runtime) = self.popup_runtime() {
                let _ = runtime.try_send_bytes(Bytes::copy_from_slice(text.as_bytes()));
            } else {
                self.close_popup_pane();
            }
            return;
        }
        if self.state.mode != Mode::Terminal {
            self.paste_into_active_text_input(text);
            return;
        }

        self.state.clear_selection();
        self.selection_autoscroll_deadline = None;
        self.state.update_dismissed = true;
        if let Some(ws_idx) = self.state.active {
            if let Some(runtime) = self
                .state
                .focused_runtime_in_workspace(&self.terminal_runtimes, ws_idx)
            {
                let _ = runtime.try_send_bytes(Bytes::copy_from_slice(text.as_bytes()));
            }
        }
    }

    pub(super) async fn handle_text_commit(&mut self, text: String) {
        if text.is_empty() {
            return;
        }
        if self.state.popup_pane.is_some() {
            if let Some(runtime) = self.popup_runtime() {
                let _ = runtime.send_bytes(Bytes::from(text)).await;
            } else {
                self.close_popup_pane();
            }
            return;
        }
        if self.state.mode != Mode::Terminal {
            self.paste_into_active_text_input(&text);
            return;
        }

        self.state.clear_selection();
        self.selection_autoscroll_deadline = None;
        self.state.update_dismissed = true;
        if let Some(ws_idx) = self.state.active {
            if let Some(runtime) = self
                .state
                .focused_runtime_in_workspace(&self.terminal_runtimes, ws_idx)
            {
                let _ = runtime.send_bytes(Bytes::from(text)).await;
            }
        }
    }

    pub(super) async fn handle_paste(&mut self, text: String) {
        if self.state.popup_pane.is_some() {
            if let Some(runtime) = self.popup_runtime() {
                let _ = runtime.send_paste(text).await;
            } else {
                self.close_popup_pane();
            }
            return;
        }
        if self.state.mode != Mode::Terminal {
            self.paste_into_active_text_input(&text);
            return;
        }

        // A bracketed paste landing on a federated remote pane may actually be
        // a local clipboard image rather than text — either as a substituted
        // temp-file path or as an empty paste. The shared intercept decides;
        // anything it does not claim falls through to the ordinary forward
        // below untouched.
        #[cfg(unix)]
        {
            if self.dispatch_bracketed_paste_image(&text) == RemoteImagePasteKeyDisposition::Consume
            {
                return;
            }
        }

        if let Some(ws_idx) = self.state.active {
            if let Some(rt) = self
                .state
                .focused_runtime_in_workspace(&self.terminal_runtimes, ws_idx)
            {
                let _ = rt.send_paste(text).await;
            }
        }
    }

    pub(crate) fn paste_into_active_text_input(&mut self, text: &str) -> bool {
        match self.state.mode {
            Mode::RenameWorkspace | Mode::RenameTab | Mode::RenamePane => {
                insert_rename_input_text(&mut self.state, text);
                true
            }
            Mode::NewLinkedWorktree => {
                self.insert_worktree_create_text(text);
                true
            }
            Mode::MountRemoteWorkspace => {
                self.insert_remote_mount_text(text);
                true
            }
            Mode::OpenExistingWorktree => {
                if !self
                    .state
                    .worktree_open
                    .as_ref()
                    .is_some_and(|open| open.search_focused)
                {
                    return false;
                }
                self.insert_worktree_open_search_text(text);
                true
            }
            Mode::Navigator => {
                if !self.state.navigator.search_focused {
                    return false;
                }
                insert_navigator_search_text(&mut self.state, &self.terminal_runtimes, text);
                true
            }
            Mode::KeybindHelp => {
                if !self.state.keybind_help.search_focused {
                    return false;
                }
                insert_keybind_help_query_text(&mut self.state, text);
                true
            }
            Mode::Copy => {
                let Some(prompt) = self
                    .state
                    .copy_mode
                    .as_mut()
                    .and_then(|copy_mode| copy_mode.search.prompt.as_mut())
                else {
                    return false;
                };
                prompt
                    .query
                    .extend(text.chars().filter(|ch| !ch.is_control()));
                true
            }
            _ => false,
        }
    }

    pub(crate) fn handle_onboarding_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Right | KeyCode::Char('l') => self.open_settings_from_onboarding(),
            _ => {
                if let Some(ModalAction::Continue) =
                    modal_action_from_key(&key, ONBOARDING_WELCOME_ACTIONS)
                {
                    self.open_settings_from_onboarding();
                }
            }
        }
    }

    pub(crate) fn handle_release_notes_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.scroll_release_notes(-1),
            KeyCode::Down | KeyCode::Char('j') => self.scroll_release_notes(1),
            KeyCode::PageUp => self.scroll_release_notes(-8),
            KeyCode::PageDown => self.scroll_release_notes(8),
            KeyCode::Home => {
                if let Some(notes) = &mut self.state.release_notes {
                    notes.scroll = 0;
                }
            }
            KeyCode::End => {
                let max_scroll = self.state.release_notes_max_scroll();
                if let Some(notes) = &mut self.state.release_notes {
                    notes.scroll = max_scroll;
                }
            }
            _ => {
                if let Some(ModalAction::Close) = modal_action_from_key(&key, RELEASE_NOTES_ACTIONS)
                {
                    self.dismiss_release_notes();
                }
            }
        }
    }

    pub(crate) fn handle_product_announcement_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.scroll_product_announcement(-1),
            KeyCode::Down | KeyCode::Char('j') => self.scroll_product_announcement(1),
            KeyCode::PageUp => self.scroll_product_announcement(-8),
            KeyCode::PageDown => self.scroll_product_announcement(8),
            KeyCode::Home => {
                if let Some(announcement) = &mut self.state.product_announcement {
                    announcement.scroll = 0;
                }
            }
            KeyCode::End => {
                let max_scroll = self.state.product_announcement_max_scroll();
                if let Some(announcement) = &mut self.state.product_announcement {
                    announcement.scroll = max_scroll;
                }
            }
            _ => {
                if let Some(ModalAction::Close) = modal_action_from_key(&key, RELEASE_NOTES_ACTIONS)
                {
                    self.dismiss_product_announcement();
                }
            }
        }
    }

    pub(super) fn handle_mouse(&mut self, mouse: MouseEvent) {
        self.handle_mouse_from_input_source(super::LOCAL_INPUT_SOURCE, mouse);
    }

    pub(super) fn handle_mouse_from_input_source(
        &mut self,
        source_id: super::InputSourceId,
        mouse: MouseEvent,
    ) {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.pending_url_click_sources.remove(&source_id);
            }
            MouseEventKind::Drag(MouseButton::Left)
                if self.pending_url_click_sources.contains(&source_id) =>
            {
                return;
            }
            MouseEventKind::Up(MouseButton::Left)
                if self.pending_url_click_sources.remove(&source_id) =>
            {
                return;
            }
            _ => {}
        }

        if self.state.popup_pane.is_some() {
            self.handle_popup_mouse(mouse);
            return;
        }
        if self.handle_overlay_mouse(mouse) {
            return;
        }

        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            && self.state.on_sidebar_divider(mouse.column, mouse.row)
        {
            let now = std::time::Instant::now();
            let is_double_click = self
                .last_sidebar_divider_click
                .is_some_and(|last| now.duration_since(last) <= super::SIDEBAR_DOUBLE_CLICK_WINDOW);
            self.last_sidebar_divider_click = Some(now);

            if is_double_click {
                self.state.sidebar_width = self.state.default_sidebar_width;
                self.state.sidebar_width_source =
                    crate::app::state::SidebarWidthSource::ConfigDefault;
                self.state.sidebar_width_auto = false;
                self.state.mark_session_dirty();
                self.state.drag = None;
                return;
            }
        }

        if self.handle_modified_url_click(source_id, mouse) {
            return;
        }

        let handled_pane_double_click = self.handle_pane_double_click(mouse);
        if !handled_pane_double_click {
            self.focus_pane_before_mouse_press(mouse);
        }

        let previous_agent_panel_sort = self.state.agent_panel_sort;
        let previous_settings_section = self.state.settings.section;
        if !handled_pane_double_click {
            if let Some(action) = self.state.handle_mouse(&mut self.terminal_runtimes, mouse) {
                match action {
                    MouseAction::NewWorkspace => {
                        self.begin_tui_workspace_create("tui.mouse.workspace.create")
                    }
                    MouseAction::Settings(action) => match action {
                        SettingsAction::SaveTheme(name) => self.save_theme(&name),
                        SettingsAction::SaveSound(enabled) => self.save_sound(enabled),
                        SettingsAction::SaveToastDelivery(delivery) => {
                            self.save_toast_delivery(delivery)
                        }
                        SettingsAction::SaveAgentBorderLabels(enabled) => {
                            self.save_agent_border_labels(enabled)
                        }
                        SettingsAction::InstallRecommendedIntegrations => {
                            self.install_recommended_integrations()
                        }
                    },
                    MouseAction::FocusWorkspace { ws_idx } => {
                        self.focus_workspace_idx_via_api(ws_idx)
                    }
                    MouseAction::FocusTab { tab_idx } => self.focus_tab_idx_via_api(tab_idx),
                    MouseAction::FocusPane { ws_idx, pane_id } => {
                        self.focus_pane_internal_via_api(ws_idx, pane_id)
                    }
                    MouseAction::FocusToastTarget => self.focus_toast_target_via_api(),
                    MouseAction::MoveWorkspace {
                        source_ws_idx,
                        insert_idx,
                    } => self.move_workspace_via_api(source_ws_idx, insert_idx),
                    MouseAction::MoveWorkspaceBlock { params } => {
                        self.move_workspace_block_via_api(params)
                    }
                    MouseAction::MoveTab {
                        ws_idx,
                        source_tab_idx,
                        insert_idx,
                    } => self.move_tab_via_api(ws_idx, source_tab_idx, insert_idx),
                    MouseAction::SetSplitRatio { path, ratio } => {
                        self.set_split_ratio_via_api(path, ratio)
                    }
                    MouseAction::RenameModal(action) => {
                        self.apply_rename_mouse_action_via_api(action)
                    }
                    MouseAction::ConfirmCloseAccept => self.confirm_close_accept_via_api(),
                    MouseAction::ContextMenu { menu, idx } => {
                        self.apply_context_menu_action_via_api(menu, idx)
                    }
                }
            }
            if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
                && self
                    .state
                    .selection
                    .as_ref()
                    .is_none_or(crate::selection::Selection::is_in_progress)
            {
                self.selection_highlight_clear_deadline = None;
            }
        }
        if previous_settings_section != crate::app::state::SettingsSection::Integrations
            && self.state.settings.section == crate::app::state::SettingsSection::Integrations
        {
            self.refresh_integration_recommendations();
        }
        if self.state.agent_panel_sort != previous_agent_panel_sort {
            self.save_agent_panel_sort(self.state.agent_panel_sort);
        }

        self.dispatch_pending_clipboard_write();

        // Sync autoscroll deadline with state (mouse handler may have
        // set or cleared selection_autoscroll during handle_mouse).
        if self.state.selection_autoscroll.is_none() {
            self.selection_autoscroll_deadline = None;
        } else if self.selection_autoscroll_deadline.is_none() {
            self.selection_autoscroll_deadline =
                Some(std::time::Instant::now() + super::SELECTION_AUTOSCROLL_INTERVAL);
        }
    }

    fn handle_popup_mouse(&mut self, mouse: MouseEvent) {
        let Some((_outer, inner)) =
            crate::ui::popup_pane_rects(&self.state, self.state.view.terminal_area)
        else {
            return;
        };
        if mouse.column < inner.x
            || mouse.column >= inner.x.saturating_add(inner.width)
            || mouse.row < inner.y
            || mouse.row >= inner.y.saturating_add(inner.height)
        {
            return;
        }
        let Some(rt) = self.popup_runtime() else {
            self.close_popup_pane();
            return;
        };
        let column = mouse.column.saturating_sub(inner.x);
        let row = mouse.row.saturating_sub(inner.y);
        let bytes = match mouse.kind {
            MouseEventKind::ScrollUp
            | MouseEventKind::ScrollDown
            | MouseEventKind::ScrollLeft
            | MouseEventKind::ScrollRight => match rt.wheel_routing() {
                Some(crate::pane::WheelRouting::MouseReport) => {
                    rt.encode_mouse_wheel(mouse.kind, column, row, mouse.modifiers)
                }
                Some(crate::pane::WheelRouting::AlternateScroll) => {
                    rt.encode_alternate_scroll(mouse.kind)
                }
                Some(crate::pane::WheelRouting::HostScroll) | None => {
                    let lines_per_notch = self.state.mouse_scroll_lines;
                    match mouse.kind {
                        MouseEventKind::ScrollUp => rt.scroll_up(lines_per_notch),
                        MouseEventKind::ScrollDown => rt.scroll_down(lines_per_notch),
                        _ => {}
                    }
                    return;
                }
            },
            MouseEventKind::Down(_) | MouseEventKind::Up(_) | MouseEventKind::Drag(_) => {
                rt.encode_mouse_button(mouse.kind, column, row, mouse.modifiers)
            }
            MouseEventKind::Moved => {
                rt.encode_mouse_motion(mouse.kind, column, row, mouse.modifiers)
            }
        };
        let Some(bytes) = bytes else {
            return;
        };
        if !matches!(mouse.kind, MouseEventKind::Moved) {
            rt.scroll_reset();
        }
        if let Err(err) = rt.try_send_bytes(Bytes::from(bytes)) {
            warn!(err = %err, kind = ?mouse.kind, "failed to forward popup mouse event");
        }
    }

    fn focus_pane_before_mouse_press(&mut self, mouse: MouseEvent) {
        if !matches!(self.state.mode, Mode::Terminal | Mode::Resize)
            || !matches!(
                mouse.kind,
                MouseEventKind::Down(MouseButton::Left | MouseButton::Middle)
            )
        {
            return;
        }

        let Some(pane_id) = self
            .state
            .pane_at(mouse.column, mouse.row)
            .map(|info| info.id)
        else {
            return;
        };
        let Some(ws_idx) = self.state.active else {
            return;
        };

        // Focus through the runtime API before an application can consume its press.
        self.focus_pane_internal_via_api(ws_idx, pane_id);
    }

    fn handle_modified_url_click(
        &mut self,
        source_id: super::InputSourceId,
        mouse: MouseEvent,
    ) -> bool {
        if self.state.mode != Mode::Terminal
            || !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            || !mouse.modifiers.contains(modified_url_click_modifier())
        {
            return false;
        }

        let Some(info) = self.state.pane_at(mouse.column, mouse.row).cloned() else {
            return false;
        };
        let viewport_row = mouse.row.saturating_sub(info.inner_rect.y);
        let col = mouse.column.saturating_sub(info.inner_rect.x);
        let Some(url) =
            self.state
                .url_at_pane_cell(&self.terminal_runtimes, info.id, viewport_row, col)
        else {
            return false;
        };

        self.last_pane_click = None;
        self.pending_url_click_sources.insert(source_id);
        match self.invoke_plugin_link_handler_for_url(&url, info.id) {
            Ok(true) => return true,
            Ok(false) => {}
            Err(err) => {
                tracing::warn!(err = %err, url = %url, "failed to invoke plugin link handler");
            }
        }
        if let Err(err) = crate::platform::open_url(&url) {
            tracing::warn!(err = %err, url = %url, "failed to open pane URL");
        }
        true
    }

    fn handle_pane_double_click(&mut self, mouse: MouseEvent) -> bool {
        // Explicit double-click copy stays available even with copy_on_select
        // disabled: that setting only suppresses the implicit copy-on-drag
        // gesture (see mouse.rs `copy_selection` gating), not a deliberate
        // double-click.

        // A pane press stops being a double-click candidate once it becomes
        // a drag or completes as a real text selection.
        match mouse.kind {
            MouseEventKind::Drag(MouseButton::Left) => {
                self.last_pane_click = None;
                return false;
            }
            MouseEventKind::Up(MouseButton::Left)
                if self
                    .state
                    .selection
                    .as_ref()
                    .is_some_and(|selection| selection.is_visible()) =>
            {
                self.last_pane_click = None;
                return false;
            }
            _ => {}
        }

        // Only terminal-pane left-clicks can start this gesture; other clicks
        // should keep their existing mouse behavior and clear stale candidates.
        let Some(click) = self.pane_click_candidate(mouse) else {
            return false;
        };

        // Require the second click to land near the first click in the same pane
        // and within the double-click window so adjacent interactions do not select a word.
        if !self.take_pane_double_click(click) {
            return false;
        }

        self.select_double_clicked_word(click)
    }

    fn pane_click_candidate(&mut self, mouse: MouseEvent) -> Option<PaneClickState> {
        if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            return None;
        }

        if !mouse.modifiers.is_empty() {
            self.last_pane_click = None;
            return None;
        }

        if self.state.mode != Mode::Terminal {
            self.last_pane_click = None;
            return None;
        }

        let Some(info) = self.state.pane_at(mouse.column, mouse.row).cloned() else {
            self.last_pane_click = None;
            return None;
        };

        Some(PaneClickState {
            pane_id: info.id,
            viewport_row: mouse.row - info.inner_rect.y,
            col: mouse.column - info.inner_rect.x,
            at: std::time::Instant::now(),
        })
    }

    fn take_pane_double_click(&mut self, click: PaneClickState) -> bool {
        if !self
            .last_pane_click
            .is_some_and(|last| last.is_double_click_for(click))
        {
            self.last_pane_click = Some(click);
            return false;
        }

        self.last_pane_click = None;
        true
    }

    fn select_double_clicked_word(&mut self, click: PaneClickState) -> bool {
        let selected = self.state.select_word_at_pane_cell(
            &self.terminal_runtimes,
            click.pane_id,
            click.viewport_row,
            click.col,
        );
        if selected {
            self.selection_highlight_clear_deadline = self
                .state
                .copy_on_select
                .then(|| std::time::Instant::now() + super::PANE_COPY_HIGHLIGHT_DURATION);
        }
        selected
    }
}

// ---------------------------------------------------------------------------
// Clipboard image paste on a mounted remote workspace
// ---------------------------------------------------------------------------
//
// Pasting an image while a mounted remote workspace's pane is focused has to
// write the file on the *remote* host, because that is where the agent reading
// it runs. The App owns the local machine's clipboard directly, so the capture
// belongs here rather than in the thin `herdr --remote` bridge client, whose
// own `ClientMessage::ClipboardImage` path is unchanged and stays the trigger
// for that topology.
//
// Known limitation, accepted: if an App that is itself the far end of a
// `herdr --remote` bridge also mounts a remote workspace, this intercept reads
// the clipboard of the host the App runs on — not the user's machine — and
// consumes the key. There is no App-side predicate for "I am serving a bridge
// client" today, so no guard is invented for it. That is a mount-inside-bridge
// nesting, not the shipped topology.

/// Toast context for a mount whose peer never negotiated file staging.
#[cfg(unix)]
pub(crate) const TOAST_REMOTE_TOO_OLD: &str = "remote herdr is too old for image paste; update it";

/// Toast context for a press whose clipboard holds no pasteable image.
#[cfg(unix)]
const TOAST_NO_CLIPBOARD_IMAGE: &str = "clipboard has no image (png/jpg/gif/webp/bmp)";

/// Toast context for an image the wire refuses before it is ever sent.
#[cfg(unix)]
const TOAST_IMAGE_TOO_LARGE: &str = "image is over 16MB, herdr's remote paste limit";

/// Toast context for a clipboard owner that never answered the read.
#[cfg(unix)]
const TOAST_CLIPBOARD_READ_TIMED_OUT: &str = "the clipboard did not answer; try again";

/// How long a clipboard read may run before the press is abandoned.
///
/// Needed because the read talks to another process on the user's machine —
/// an X11 selection owner that has stopped servicing requests never answers at
/// all — and without a bound the press would simply never resolve, leaving the
/// user with no image and no explanation. Generous enough that a healthy
/// clipboard holding a large screenshot is never cut off.
#[cfg(unix)]
const CLIPBOARD_IMAGE_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Runs `read` off the caller's thread and reports the outcome as an event.
///
/// Returns as soon as the work is scheduled, which is the whole point: the
/// caller is the single App event loop, and everything it drives — rendering,
/// every pane's output, the API loop — stops for exactly as long as it stays
/// inside a synchronous clipboard read.
///
/// A read that outlives `timeout` is abandoned rather than cancelled. Blocking
/// work cannot be interrupted, so the thread runs to completion and its result
/// is discarded; what matters is that the user is told, and that a wedged
/// clipboard owner holds nothing but one pool thread.
#[cfg(unix)]
pub(crate) fn spawn_clipboard_image_capture<F>(
    events: tokio::sync::mpsc::Sender<crate::events::AppEvent>,
    workspace_id: String,
    target_pane_id: crate::layout::PaneId,
    timeout: std::time::Duration,
    read: F,
) where
    F: FnOnce() -> Option<crate::platform::ClipboardImage> + Send + 'static,
{
    use crate::events::ClipboardImageCapture;

    tokio::spawn(async move {
        let capture = match tokio::time::timeout(timeout, tokio::task::spawn_blocking(read)).await {
            Ok(Ok(Some(image))) => ClipboardImageCapture::Image(image),
            Ok(Ok(None)) => ClipboardImageCapture::NoImage,
            Ok(Err(err)) => {
                warn!(%err, "the clipboard image read did not complete");
                ClipboardImageCapture::NoImage
            }
            Err(_) => ClipboardImageCapture::ReadTimedOut,
        };
        let _ = events
            .send(crate::events::AppEvent::RemoteClipboardImageCaptured {
                workspace_id,
                target_pane_id,
                capture,
            })
            .await;
    });
}

/// What a `keys.remote_image_paste` press means for the currently focused
/// pane. Exactly three outcomes, and only two of them claim the key.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemoteImagePasteDecision {
    /// Not this intercept's key. The press is left completely alone.
    FallThrough,
    /// A mounted remote pane whose peer never agreed to file staging. The
    /// feature cannot work on this pane for the life of the mount, so the key
    /// is *not* claimed: it goes on to the pane app as usual, and the reason
    /// is reported once per pane so the press does not merely look ignored.
    /// Carries the pane so that report can be de-duplicated.
    Unsupported {
        target_pane_id: crate::layout::PaneId,
    },
    /// A mounted remote pane on a peer that can stage files.
    Capture {
        ws_idx: usize,
        target_pane_id: crate::layout::PaneId,
    },
}

/// The focused pane of the active workspace, when that workspace is a
/// federation mount, along with whether the mount's peer has agreed to
/// `FILE_STAGING`.
///
/// Shared by every intercept that needs to know "is this input headed at a
/// federated remote pane" — the raw `ctrl+v` keybinding and the
/// bracketed-paste image-path bridge both resolve the same target through
/// this one path, so the two never disagree about which pane, or which
/// mount, they are looking at.
#[cfg(unix)]
struct RemotePasteTarget {
    ws_idx: usize,
    target_pane_id: crate::layout::PaneId,
    supports_file_staging: bool,
}

#[cfg(unix)]
fn resolve_remote_paste_target(state: &AppState) -> Option<RemotePasteTarget> {
    use crate::remote::federation::protocol::Capability;

    if state.mode != Mode::Terminal {
        debug!(mode = ?state.mode, "remote paste target: not in terminal mode");
        return None;
    }
    let Some(ws_idx) = state.active else {
        debug!("remote paste target: no active workspace");
        return None;
    };
    let Some(workspace) = state.workspaces.get(ws_idx) else {
        debug!(
            ws_idx,
            "remote paste target: active workspace index out of range"
        );
        return None;
    };
    // A federated workspace carries the mount's host key in its space
    // membership; matching it against the live mirrors is what distinguishes
    // "this workspace came from a remote host" from "this workspace is local".
    let Some(space_key) = workspace.worktree_space().map(|space| space.key.as_str()) else {
        debug!(workspace_id = %workspace.id, "remote paste target: workspace has no space membership");
        return None;
    };
    let Some(mirror) = state
        .remote_mirrors
        .iter()
        .find(|(host_key, _)| format!("federation:{}", host_key.as_str()) == space_key)
        .map(|(_, mirror)| mirror)
    else {
        debug!(
            space_key,
            mirror_host_keys = ?state
                .remote_mirrors
                .keys()
                .map(|host_key| host_key.as_str().to_owned())
                .collect::<Vec<_>>(),
            "remote paste target: no mirror matches the workspace space key"
        );
        return None;
    };
    let Some(target_pane_id) = workspace.focused_pane_id() else {
        debug!(workspace_id = %workspace.id, "remote paste target: workspace has no focused pane");
        return None;
    };
    let supports_file_staging = mirror.supports(&Capability::new(Capability::FILE_STAGING));
    Some(RemotePasteTarget {
        ws_idx,
        target_pane_id,
        supports_file_staging,
    })
}

/// Decides what a key press means for the remote image-paste intercept.
///
/// Pure, and deliberately so: the branch that must never be taken by accident
/// is the one that consumes a key, and a pure function makes each condition
/// assertable without a clipboard, a mount, or a running event loop.
#[cfg(unix)]
pub(crate) fn remote_image_paste_decision(
    state: &AppState,
    key: &TerminalKey,
) -> RemoteImagePasteDecision {
    let Some(binding) = state.remote_image_paste_key else {
        return RemoteImagePasteDecision::FallThrough;
    };
    if !crate::config::terminal_key_matches_combo(key, binding) {
        return RemoteImagePasteDecision::FallThrough;
    }
    let Some(target) = resolve_remote_paste_target(state) else {
        return RemoteImagePasteDecision::FallThrough;
    };
    if !target.supports_file_staging {
        return RemoteImagePasteDecision::Unsupported {
            target_pane_id: target.target_pane_id,
        };
    }
    RemoteImagePasteDecision::Capture {
        ws_idx: target.ws_idx,
        target_pane_id: target.target_pane_id,
    }
}

/// What a bracketed-paste payload means for the remote image-path bridge:
/// does the pasted text have the exact shape of a local image file the
/// terminal substituted for a clipboard image (see `crate::image_path`),
/// landing on a federated remote pane?
///
/// Deliberately conservative in the same spirit as `remote_image_paste_decision`:
/// `FallThrough` covers every case where the paste should still reach the
/// remote PTY as ordinary text, including a local pane, a non-federated
/// workspace, or text that merely looks like a path. Unlike the keybinding
/// intercept, an unmatched shape is not itself suspicious — most pastes are
/// text — so only a *shape match* against a peer that lacks `FILE_STAGING`
/// raises `Unsupported`; anything that never matched the shape falls
/// through untouched.
#[cfg(unix)]
pub(crate) enum BracketedPasteImageDecision {
    FallThrough,
    Unsupported,
    Capture {
        ws_idx: usize,
        target_pane_id: crate::layout::PaneId,
        /// Canonicalized candidate for the OFF-LOOP read: the decision only
        /// shape-checks and gates on the drop location; the caller hands this
        /// path to `begin_remote_clipboard_image_capture` so the up-to-16MiB
        /// file read never runs on the App event loop, and the read itself
        /// re-proves temp-dir containment against the opened fd
        /// (`crate::image_path::read_verified_image_drop_file`).
        path: std::path::PathBuf,
        extension: &'static str,
    },
}

#[cfg(unix)]
pub(crate) fn bracketed_paste_image_decision(
    state: &AppState,
    text: &str,
) -> BracketedPasteImageDecision {
    let Some(target) = resolve_remote_paste_target(state) else {
        return BracketedPasteImageDecision::FallThrough;
    };
    let Some((path, extension)) = crate::image_path::local_image_path_from_text(text) else {
        debug!("bracketed paste: text does not have image-path shape");
        return BracketedPasteImageDecision::FallThrough;
    };
    // Ordinary paste content triggers this path, unlike the dedicated
    // keybinding, so the shape match alone is not enough evidence: the
    // candidate must also sit in a location only a clipboard-image drop
    // would use, never a path the user typed or pasted on purpose. This
    // on-loop check is advisory (cheap syscalls, decides interception vs
    // fall-through only); the authoritative containment proof is re-run
    // against the opened fd inside `read_verified_image_drop_file` during
    // the off-loop read, so a symlink swapped in after this point cannot
    // widen what gets bridged.
    let Some(canonical_path) = crate::image_path::recognized_image_drop_location(&path) else {
        debug!(path = %path.display(), temp_dir = %std::env::temp_dir().display(), "bracketed paste: image path is not in a recognized drop location");
        return BracketedPasteImageDecision::FallThrough;
    };
    if !target.supports_file_staging {
        return BracketedPasteImageDecision::Unsupported;
    }
    BracketedPasteImageDecision::Capture {
        ws_idx: target.ws_idx,
        target_pane_id: target.target_pane_id,
        path: canonical_path,
        extension,
    }
}

/// What a remote image-paste intercept decided about one piece of local input
/// — a key press or a bracketed paste payload — in the only terms its callers
/// care about: does that input still belong to the pane?
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemoteImagePasteKeyDisposition {
    /// Not claimed. Keep dispatching exactly as if the intercept did not exist.
    Forward,
    /// Claimed. The input must not reach the pane.
    Consume,
}

/// The clipboard reader the raw-key intercept hands to the off-loop capture.
///
/// A seam rather than a direct call to `crate::platform::read_clipboard_image`
/// so that "one physical press starts exactly one clipboard read" is
/// assertable: the real reader spawns `osascript` on macOS and a chain of
/// `wl-paste`/`xclip` on Linux, so a test that counts reads would otherwise
/// spawn one child process per counted read and read the developer's own
/// clipboard.
#[cfg(all(unix, not(test)))]
fn clipboard_image_reader() -> fn() -> Option<crate::platform::ClipboardImage> {
    crate::platform::read_clipboard_image
}

/// Test build: answers "no image" instantly, touching no OS clipboard. Every
/// started read still resolves into exactly one
/// `AppEvent::RemoteClipboardImageCaptured` on the App's own event channel, so
/// counting those events counts the reads a key path launched — per App, and
/// therefore isolated between tests running in the same process.
#[cfg(all(unix, test))]
fn clipboard_image_reader() -> fn() -> Option<crate::platform::ClipboardImage> {
    || None
}

#[cfg(unix)]
impl App {
    /// Runs the `keys.remote_image_paste` intercept for one key.
    ///
    /// Shared by every dispatcher that forwards keys to a pane — the legacy
    /// `handle_key` loop and the headless `prepare_terminal_key_forward` path
    /// the attached-client server actually runs — so the two can never
    /// disagree about which presses are claimed or how an unusable mount is
    /// reported.
    pub(crate) fn dispatch_remote_image_paste_key(
        &mut self,
        key: &TerminalKey,
    ) -> RemoteImagePasteKeyDisposition {
        match remote_image_paste_decision(&self.state, key) {
            // Not this intercept's key. Note the absence of any side effect
            // here: callers rely on a fall-through leaving the key completely
            // untouched, so a local ctrl+v still reaches the pane app that
            // wants it (readline quoted-insert, vim visual-block).
            RemoteImagePasteDecision::FallThrough => RemoteImagePasteKeyDisposition::Forward,
            RemoteImagePasteDecision::Unsupported { target_pane_id } => {
                // The peer cannot stage a file, and that cannot change while
                // the mount lives, so the feature will never work on this
                // pane. Claiming the key anyway would take ctrl+v away from
                // the pane app permanently in exchange for nothing, so the
                // key is delivered and the reason is reported instead.
                //
                // Reported once per pane: the key now reaches the pane app,
                // so pressing it is an ordinary thing to do, and repeating
                // the same unchanging notice on every press would be noise.
                if key.kind == crossterm::event::KeyEventKind::Press
                    && self
                        .remote_image_paste_unsupported_notices
                        .insert(target_pane_id)
                {
                    self.raise_clipboard_stage_toast(
                        crate::app::remote_clipboard_stage::TOAST_TITLE_FAILED,
                        TOAST_REMOTE_TOO_OLD,
                    );
                }
                RemoteImagePasteKeyDisposition::Forward
            }
            RemoteImagePasteDecision::Capture {
                ws_idx,
                target_pane_id,
            } => {
                // Only a press starts a read. Under the enhanced keyboard
                // protocol a held key also reports `Repeat`, and the lease
                // table reprocesses the repeats of a consumed press back
                // through this same path, so without this gate one held
                // ctrl+v would start a clipboard read, a blocking thread, a
                // staged remote file and a pasted path per repeat. The repeat
                // is still consumed rather than forwarded: the key belongs to
                // the intercept, and passing it on would send the remote PTY
                // the very `0x16` the press deliberately withheld.
                if key.kind != crossterm::event::KeyEventKind::Press {
                    return RemoteImagePasteKeyDisposition::Consume;
                }
                // Reading the clipboard is unbounded synchronous OS work — a
                // child `osascript` on macOS, a chain of `wl-paste`/`xclip`
                // spawns on Linux — and this is the hot terminal key path
                // shared by every pane and every client, so the read runs
                // off-loop and answers as an event.
                debug!(
                    ws_idx,
                    ?target_pane_id,
                    "intercepted remote image paste key before forwarding to pane"
                );
                self.begin_remote_clipboard_image_capture(
                    ws_idx,
                    target_pane_id,
                    clipboard_image_reader(),
                );
                RemoteImagePasteKeyDisposition::Consume
            }
        }
    }

    /// Runs the clipboard-image intercepts for one bracketed paste payload.
    ///
    /// Shared by every dispatcher that forwards pastes to a pane — the legacy
    /// `handle_paste` loop and the headless `RawInputEvent::Paste` arm the
    /// attached-client server actually runs — for the same reason the key
    /// intercept is shared: the two must not drift.
    ///
    /// Two shapes are recognized, and neither can be induced remotely. A
    /// bracketed paste only ever originates on the local terminal's stdin
    /// (`crate::raw_input`); a remote pane produces terminal *output*, which
    /// is parsed into screen cells and never re-enters input dispatch. So a
    /// hostile peer cannot make the local clipboard be read.
    pub(crate) fn dispatch_bracketed_paste_image(
        &mut self,
        text: &str,
    ) -> RemoteImagePasteKeyDisposition {
        if text.is_empty() {
            return self.dispatch_empty_bracketed_paste();
        }
        // A bracketed paste landing on a federated remote pane may be a local
        // screenshot: the terminal (iTerm2/Terminal.app/cmux "paste image as
        // path") substitutes a local temp-file path for the image and delivers
        // *that* as pasted text. Forwarding it verbatim sends the remote host
        // a path that does not exist there.
        match bracketed_paste_image_decision(&self.state, text) {
            BracketedPasteImageDecision::Unsupported => {
                self.raise_clipboard_stage_toast(
                    crate::app::remote_clipboard_stage::TOAST_TITLE_FAILED,
                    TOAST_REMOTE_TOO_OLD,
                );
                RemoteImagePasteKeyDisposition::Consume
            }
            BracketedPasteImageDecision::Capture {
                ws_idx,
                target_pane_id,
                path,
                extension,
            } => {
                // Off-loop like the ctrl+v clipboard read: the file can be
                // 16MiB, and a synchronous read here would stall rendering and
                // every pane for its duration.
                self.begin_remote_clipboard_image_capture(ws_idx, target_pane_id, move || {
                    crate::image_path::read_verified_image_drop_file(&path, extension)
                });
                RemoteImagePasteKeyDisposition::Consume
            }
            BracketedPasteImageDecision::FallThrough => RemoteImagePasteKeyDisposition::Forward,
        }
    }

    /// An *empty* bracketed paste (`ESC[200~ESC[201~`) on a federated remote
    /// pane starts the same clipboard-image capture the `ctrl+v` intercept
    /// starts.
    ///
    /// Measured on macOS: Warp answers Cmd+V with a screenshot on the
    /// clipboard by emitting exactly that empty bracket pair — the image has no
    /// text flavor, so the paste carries no payload. herdr already reads the
    /// same signal this way for its own `herdr --remote` client
    /// (`crate::client`); this extends it to federation mounts, where the
    /// capture and staging path already exists.
    ///
    /// An empty paste is also what a genuinely empty *text* clipboard produces,
    /// and the two are indistinguishable at this point. That costs nothing: the
    /// capture simply reports no image and the user gets the ordinary "no image
    /// on the clipboard" toast, which is the truth in both cases.
    fn dispatch_empty_bracketed_paste(&mut self) -> RemoteImagePasteKeyDisposition {
        // One setting governs the whole clipboard-image-paste feature.
        // `keys.remote_image_paste = ""` is documented as the way to turn it
        // off, and a user who set it did so to stop herdr reading the
        // clipboard, not merely to free one keystroke — so it must also stop
        // this trigger, which reads the same clipboard for the same purpose
        // and would otherwise leave the feature with no off switch at all.
        if self.state.remote_image_paste_key.is_none() {
            return RemoteImagePasteKeyDisposition::Forward;
        }
        // Not a federated pane: an empty paste is the pane app's business, and
        // reading the clipboard for a local pane would be a side effect the
        // user never asked for.
        let Some(target) = resolve_remote_paste_target(&self.state) else {
            return RemoteImagePasteKeyDisposition::Forward;
        };
        if !target.supports_file_staging {
            // Same contract as the key intercept: the peer will never be able
            // to stage a file, so the paste is delivered rather than
            // confiscated, and the reason is reported once per pane.
            if self
                .remote_image_paste_unsupported_notices
                .insert(target.target_pane_id)
            {
                self.raise_clipboard_stage_toast(
                    crate::app::remote_clipboard_stage::TOAST_TITLE_FAILED,
                    TOAST_REMOTE_TOO_OLD,
                );
            }
            return RemoteImagePasteKeyDisposition::Forward;
        }
        debug!(
            ws_idx = target.ws_idx,
            target_pane_id = ?target.target_pane_id,
            "intercepted an empty bracketed paste as a clipboard image paste"
        );
        // Off-loop for the same reason as the key path: the read spawns a
        // child process and this is the shared event loop.
        self.begin_remote_clipboard_image_capture(
            target.ws_idx,
            target.target_pane_id,
            clipboard_image_reader(),
        );
        RemoteImagePasteKeyDisposition::Consume
    }
}

/// Whether a claimed image paste reached the wire. The key is consumed either
/// way — the user asked for an image paste on a remote pane, and answering
/// with a raw `Ctrl-V` to the remote PTY would be worse than a toast.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ImagePasteOutcome {
    /// The request is on the wire; its answer arrives as an `AppEvent`.
    Staged,
    /// Refused locally, and the user was told why.
    Rejected,
}

#[cfg(unix)]
impl App {
    /// Drops the per-pane clipboard-image-paste bookkeeping of the given
    /// (closing) workspaces.
    ///
    /// Both sets are keyed by `PaneId` and neither is otherwise reachable once
    /// the pane is gone: the unsupported-mount notice would sit there for the
    /// life of the process, and an in-flight marker whose answer is dropped
    /// (the workspace lookup in `handle_remote_clipboard_image_captured`
    /// fails silently) would too. Run wherever the other per-pane federation
    /// indexes are purged, so the whole group behaves the same on both the
    /// locally-initiated close and the remote teardown.
    pub(crate) fn purge_remote_image_paste_pane_state_for_workspaces(
        &mut self,
        workspace_ids: &std::collections::HashSet<String>,
    ) {
        let closing_pane_ids: std::collections::HashSet<crate::layout::PaneId> = self
            .state
            .workspaces
            .iter()
            .filter(|ws| workspace_ids.contains(&ws.id))
            .flat_map(|ws| ws.tabs.iter().flat_map(|tab| tab.layout.pane_ids()))
            .collect();
        self.remote_image_paste_unsupported_notices
            .retain(|pane_id| !closing_pane_ids.contains(pane_id));
        self.remote_clipboard_image_reads_in_flight
            .retain(|pane_id| !closing_pane_ids.contains(pane_id));
    }

    /// Starts the off-loop clipboard read for a claimed image-paste press.
    ///
    /// Records the target as a stable workspace id rather than the index the
    /// decision produced: the read can outlive the workspace, and an index
    /// would then name whichever workspace took over the slot.
    ///
    /// At most one read runs per pane at a time. The trigger is user input, and
    /// input arrives in floods — a held `Cmd+V`, a terminal replaying a paste —
    /// while a read is an unbounded blocking child process. A second read
    /// started while the first is outstanding would ask the same clipboard the
    /// same question, so the flood is answered by the read already running
    /// rather than by one child per event. A distinct later trigger, once the
    /// pane has no read outstanding, is never dropped.
    pub(crate) fn begin_remote_clipboard_image_capture<F>(
        &mut self,
        ws_idx: usize,
        target_pane_id: crate::layout::PaneId,
        read: F,
    ) where
        F: FnOnce() -> Option<crate::platform::ClipboardImage> + Send + 'static,
    {
        let Some(workspace_id) = self.state.workspaces.get(ws_idx).map(|ws| ws.id.clone()) else {
            return;
        };
        if !self
            .remote_clipboard_image_reads_in_flight
            .insert(target_pane_id)
        {
            debug!(
                ?target_pane_id,
                "a clipboard image read is already running for this pane; not starting another"
            );
            return;
        }
        spawn_clipboard_image_capture(
            self.event_tx.clone(),
            workspace_id,
            target_pane_id,
            CLIPBOARD_IMAGE_READ_TIMEOUT,
            read,
        );
    }

    /// `AppEvent::RemoteClipboardImageCaptured` handler: the off-loop read
    /// finished, so the press can finally be answered.
    pub(crate) fn handle_remote_clipboard_image_captured(
        &mut self,
        workspace_id: String,
        target_pane_id: crate::layout::PaneId,
        capture: crate::events::ClipboardImageCapture,
    ) {
        use crate::events::ClipboardImageCapture;

        // The read this event answers is over, whatever it found, so the pane
        // may start another one. Released before any early return below: a
        // "no image" or timed-out answer must not leave the pane unable to try
        // again.
        self.remote_clipboard_image_reads_in_flight
            .remove(&target_pane_id);

        let image = match capture {
            ClipboardImageCapture::Image(image) => image,
            ClipboardImageCapture::NoImage => {
                self.raise_clipboard_stage_toast(
                    crate::app::remote_clipboard_stage::TOAST_TITLE_FAILED,
                    TOAST_NO_CLIPBOARD_IMAGE,
                );
                return;
            }
            ClipboardImageCapture::ReadTimedOut => {
                warn!("the clipboard owner did not answer an image read in time");
                self.raise_clipboard_stage_toast(
                    crate::app::remote_clipboard_stage::TOAST_TITLE_FAILED,
                    TOAST_CLIPBOARD_READ_TIMED_OUT,
                );
                return;
            }
        };
        // Silent when the workspace is gone: the user closed the thing they
        // pasted into while the read ran, so there is nothing to report and
        // nowhere sensible to report it.
        let Some(ws_idx) = self
            .state
            .workspaces
            .iter()
            .position(|ws| ws.id == workspace_id)
        else {
            warn!(
                %workspace_id,
                "dropping a clipboard image whose workspace closed during the read"
            );
            return;
        };
        self.handle_remote_image_paste(ws_idx, target_pane_id, image);
    }

    /// Everything the image-paste intercept does after the clipboard read.
    ///
    /// Split from `handle_key` so the success branch is drivable in a test:
    /// the OS clipboard cannot be made to hold a PNG in CI, but a
    /// `ClipboardImage` literal handed to this function exercises the same
    /// size check, the same staging call and the same failure reporting.
    pub(crate) fn handle_remote_image_paste(
        &mut self,
        ws_idx: usize,
        target_pane_id: crate::layout::PaneId,
        image: crate::platform::ClipboardImage,
    ) -> ImagePasteOutcome {
        // Checked here rather than left to the peer: an oversized frame is
        // dropped by the transport as a protocol violation, which surfaces to
        // the user as a closed connection and reads as "the mount died".
        if image.bytes.len() > crate::protocol::MAX_CLIPBOARD_IMAGE_PAYLOAD {
            warn!(
                bytes = image.bytes.len(),
                max = crate::protocol::MAX_CLIPBOARD_IMAGE_PAYLOAD,
                "refusing to stage a clipboard image larger than the paste limit"
            );
            self.raise_clipboard_stage_toast(
                crate::app::remote_clipboard_stage::TOAST_TITLE_FAILED,
                TOAST_IMAGE_TOO_LARGE,
            );
            return ImagePasteOutcome::Rejected;
        }
        // Every refusal inside `begin_remote_clipboard_stage` raises its own
        // toast, so a rejection here is already reported by the time it
        // returns; the outcome exists for the caller, not for the user.
        match self.begin_remote_clipboard_stage(ws_idx, target_pane_id, &image) {
            Ok(()) => ImagePasteOutcome::Staged,
            Err(_) => ImagePasteOutcome::Rejected,
        }
    }
}

pub(crate) fn is_modal_paste_shortcut(key: &KeyEvent) -> bool {
    if !matches!(key.code, KeyCode::Char('v' | 'V')) {
        return false;
    }

    #[cfg(target_os = "macos")]
    {
        key.modifiers.contains(KeyModifiers::SUPER) || key.modifiers.contains(KeyModifiers::CONTROL)
    }

    #[cfg(not(target_os = "macos"))]
    {
        key.modifiers.contains(KeyModifiers::CONTROL)
    }
}

pub(crate) fn modal_paste_target_active(state: &AppState) -> bool {
    match state.mode {
        Mode::RenameWorkspace
        | Mode::RenameTab
        | Mode::RenamePane
        | Mode::NewLinkedWorktree
        | Mode::MountRemoteWorkspace => true,
        Mode::OpenExistingWorktree => state
            .worktree_open
            .as_ref()
            .is_some_and(|open| open.search_focused),
        Mode::Navigator => state.navigator.search_focused,
        Mode::KeybindHelp => state.keybind_help.search_focused,
        Mode::Copy => state
            .copy_mode
            .as_ref()
            .is_some_and(|copy_mode| copy_mode.search.prompt.is_some()),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Mouse handling
// ---------------------------------------------------------------------------

// Note: split_pane needs runtime (event_tx for PTY spawn), so it lives on App
impl AppState {
    #[cfg(test)]
    pub(crate) fn split_pane(
        &mut self,
        terminal_runtimes: &mut crate::terminal::TerminalRuntimeRegistry,
        direction: Direction,
    ) {
        // Actual PTY spawning happens in Workspace::split_focused
        // which needs events channel — this is called from navigate_key
        // where we don't have async context, so the workspace handles it
        let (rows, cols) = self.estimate_pane_size();
        let new_rows = (rows / 2).max(4);
        let new_cols = (cols / 2).max(10);

        let follow_cwd = self
            .active
            .and_then(|i| self.workspaces.get(i))
            .and_then(|ws| {
                let tab = ws.active_tab()?;
                let terminal_id = tab.terminal_id(tab.layout.focused())?;
                super::creation::launch_cwd_for_terminal(
                    terminal_id,
                    &self.terminals,
                    terminal_runtimes,
                )
            });
        let cwd = Some(super::creation::resolve_new_terminal_cwd(
            &self.new_terminal_cwd,
            follow_cwd,
        ));

        let previous_focus = self.current_pane_focus_target();
        if let Some(ws_idx) = self.active {
            let Some(ws) = self.workspaces.get_mut(ws_idx) else {
                return;
            };
            if let Ok(new_pane) = ws.split_focused(
                direction,
                new_rows,
                new_cols,
                cwd,
                self.pane_scrollback_limit_bytes,
                self.host_terminal_theme,
                self.host_terminal_appearance,
                crate::pane::PaneShellConfig::new(&self.default_shell, self.shell_mode),
                Vec::new(),
            ) {
                let new_id = new_pane.pane_id;
                terminal_runtimes.insert(new_pane.terminal.id.clone(), new_pane.runtime);
                self.remove_alias_shadowed_by_new_pane(new_id);
                self.terminals
                    .insert(new_pane.terminal.id.clone(), new_pane.terminal);
                self.record_pane_focus_change(previous_focus, ws_idx, new_id);
                self.mark_session_dirty();
                self.mode = Mode::Terminal;
            }
        }
    }
}

#[cfg(test)]
fn state_with_workspaces(names: &[&str]) -> AppState {
    let mut state = AppState::test_new();
    state.workspaces = names
        .iter()
        .map(|name| crate::workspace::Workspace::test_new(name))
        .collect();
    if !state.workspaces.is_empty() {
        state.active = Some(0);
        state.selected = 0;
        state.mode = Mode::Navigate;
    }
    state
}

#[cfg(test)]
fn app_for_mouse_test() -> App {
    let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(
        &crate::config::Config::default(),
        true,
        None,
        api_rx,
        crate::api::EventHub::default(),
    );
    app.state.mode = Mode::Terminal;
    app.state.update_available = None;
    app.state.latest_release_notes_available = false;
    app.state.view.sidebar_rect = ratatui::layout::Rect::new(0, 0, 26, 20);
    app.state.view.terminal_area = ratatui::layout::Rect::new(26, 0, 80, 20);
    app
}

#[cfg(test)]
fn mouse(
    kind: crossterm::event::MouseEventKind,
    col: u16,
    row: u16,
) -> crossterm::event::MouseEvent {
    crossterm::event::MouseEvent {
        kind,
        column: col,
        row,
        modifiers: crossterm::event::KeyModifiers::empty(),
    }
}

#[cfg(test)]
fn numbered_lines_bytes(count: usize) -> Vec<u8> {
    (0..count)
        .map(|i| format!("{i:06}\r\n"))
        .collect::<String>()
        .into_bytes()
}

#[cfg(test)]
fn capture_snapshot(state: &AppState) -> crate::persist::SessionSnapshot {
    let terminal_runtimes = crate::terminal::TerminalRuntimeRegistry::new();
    crate::persist::capture(
        &state.workspaces,
        &state.terminals,
        &terminal_runtimes,
        state.active,
        state.selected,
        state.sidebar_width,
        state.sidebar_section_split,
        state.collapsed_space_keys.clone(),
    )
}

#[cfg(test)]
fn root_layout_ratio(snapshot: &crate::persist::SessionSnapshot) -> Option<f32> {
    match &snapshot.workspaces.first()?.tabs.first()?.layout {
        crate::persist::LayoutSnapshot::Split { ratio, .. } => Some(*ratio),
        crate::persist::LayoutSnapshot::Pane(_) => None,
    }
}

#[cfg(test)]
fn unique_temp_path(name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("herdr-{name}-{}-{nanos}", std::process::id()))
}

#[cfg(test)]
#[cfg(unix)]
fn wait_for_file(path: &std::path::Path) -> String {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        if let Ok(content) = std::fs::read_to_string(path) {
            if !content.is_empty() {
                return content;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    panic!("timed out waiting for {}", path.display());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_app() -> App {
        App::new(
            &crate::config::Config::default(),
            true,
            None,
            tokio::sync::mpsc::unbounded_channel().1,
            crate::api::EventHub::default(),
        )
    }

    #[tokio::test]
    async fn paste_routes_to_rename_modal_input() {
        let mut app = test_app();
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("test")];
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.mode = Mode::RenameTab;
        app.state.name_input = "2".into();
        app.state.name_input_replace_on_type = true;

        app.handle_paste("feature/logs".into()).await;

        assert_eq!(app.state.name_input, "feature/logs");
        assert!(!app.state.name_input_replace_on_type);
    }

    #[tokio::test]
    async fn paste_routes_to_keybind_help_query_only_when_searching() {
        let mut app = test_app();
        app.state.mode = Mode::KeybindHelp;
        app.handle_paste("ignored".into()).await;
        assert!(app.state.keybind_help.query.is_empty());

        app.state.keybind_help.search_focused = true;
        app.state.keybind_help.scroll = 3;
        app.handle_paste("work\nspace".into()).await;

        assert_eq!(app.state.keybind_help.query, "workspace");
        assert_eq!(app.state.keybind_help.scroll, 0);
    }

    #[tokio::test]
    async fn paste_routes_to_new_linked_worktree_input() {
        let mut app = test_app();
        app.state.mode = Mode::NewLinkedWorktree;
        app.state.name_input = "generated-branch".into();
        app.state.name_input_replace_on_type = true;
        app.state.worktree_create = Some(crate::app::state::WorktreeCreateState {
            source_workspace_id: "source".into(),
            source_checkout_path: "/repo/herdr".into(),
            source_existing_membership: None,
            source_repo_root: "/repo/herdr".into(),
            repo_key: "repo-key".into(),
            repo_name: "herdr".into(),
            branch: "generated-branch".into(),
            checkout_path: "/repo/herdr-generated-branch".into(),
            error: None,
            creating: false,
        });

        app.handle_paste("feature/linear-302".into()).await;

        assert_eq!(app.state.name_input, "feature/linear-302");
        assert_eq!(
            app.state
                .worktree_create
                .as_ref()
                .map(|create| create.branch.as_str()),
            Some("feature/linear-302")
        );
    }

    #[test]
    fn modal_paste_shortcut_matches_platform_primary_v() {
        #[cfg(target_os = "macos")]
        let modifiers = KeyModifiers::SUPER;
        #[cfg(not(target_os = "macos"))]
        let modifiers = KeyModifiers::CONTROL;

        assert!(is_modal_paste_shortcut(&KeyEvent::new(
            KeyCode::Char('v'),
            modifiers
        )));
        assert!(is_modal_paste_shortcut(&KeyEvent::new(
            KeyCode::Char('V'),
            modifiers | KeyModifiers::SHIFT
        )));
        assert!(!is_modal_paste_shortcut(&KeyEvent::new(
            KeyCode::Char('v'),
            KeyModifiers::ALT
        )));
    }

    #[test]
    fn modal_paste_target_is_active_only_for_text_inputs() {
        let mut state = AppState::test_new();

        state.mode = Mode::RenameTab;
        assert!(modal_paste_target_active(&state));

        state.mode = Mode::Navigator;
        state.navigator.search_focused = false;
        assert!(!modal_paste_target_active(&state));
        state.navigator.search_focused = true;
        assert!(modal_paste_target_active(&state));

        state.mode = Mode::KeybindHelp;
        state.keybind_help.search_focused = false;
        assert!(!modal_paste_target_active(&state));
        state.keybind_help.search_focused = true;
        assert!(modal_paste_target_active(&state));

        state.mode = Mode::ConfirmClose;
        assert!(!modal_paste_target_active(&state));
    }
}

#[cfg(all(test, unix))]
#[allow(clippy::unwrap_used)]
mod remote_image_paste_tests {
    use super::*;
    use crate::app::remote_clipboard_stage::{TOAST_TITLE_FAILED, TOAST_TITLE_SAVING};
    use crate::config::Config;
    use crate::events::AppEvent;
    use crate::input::TerminalKey;
    use crate::layout::PaneId;
    use crate::remote::federation::id::{HostKey, Mount, ServerInstanceId};
    use crate::remote::federation::protocol::{Capability, FederationMessage};
    use crate::workspace::Workspace;
    use bytes::Bytes;
    use std::collections::BTreeSet;
    use std::sync::Arc;
    use tokio::sync::mpsc;
    use tokio::sync::Notify;

    const HOST: &str = "remote-host";

    fn host_key() -> HostKey {
        HostKey::new(HOST, "s1")
    }

    fn ctrl_v() -> TerminalKey {
        TerminalKey::new(KeyCode::Char('v'), KeyModifiers::CONTROL)
    }

    fn test_app() -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![Workspace::test_new("space")];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.mode = Mode::Terminal;
        // The shipped default drops every notification, which would make
        // "the user was told" unobservable and every toast assertion vacuous.
        app.state.toast_config.delivery = crate::config::ToastDelivery::Herdr;
        app
    }

    /// A plain local pane backed by a real runtime, so a key that falls
    /// through is observable as bytes on the pane's own channel.
    fn attach_local_pane(app: &mut App) -> (PaneId, mpsc::Receiver<Bytes>) {
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let (runtime, rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        app.terminal_runtimes.insert(terminal_id, runtime);
        (pane_id, rx)
    }

    /// A pane of a mounted remote workspace. Its runtime is a real remote
    /// runtime, so raw key bytes and staging frames travel the same channel —
    /// one receiver therefore witnesses both "no PTY write" and "no wire send".
    fn attach_remote_mount(
        app: &mut App,
        staging: bool,
    ) -> (PaneId, mpsc::UnboundedReceiver<FederationMessage>) {
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        let (_output_tx, output_rx) = mpsc::channel::<Bytes>(4);
        let (clipboard_tx, _clipboard_rx) = mpsc::unbounded_channel();
        let (events_tx, _events_rx) = mpsc::channel::<AppEvent>(8);
        let runtime = crate::terminal::TerminalRuntime::spawn_remote(
            pane_id,
            24,
            80,
            1 << 16,
            crate::terminal_theme::TerminalTheme::default(),
            None,
            "term_1".to_string(),
            1,
            out_tx,
            output_rx,
            clipboard_tx,
            events_tx,
            Arc::new(Notify::new()),
            Arc::new(crate::render_signal::RenderSignal::new()),
        )
        .expect("a remote runtime needs no local PTY");
        app.terminal_runtimes.insert(terminal_id, runtime);

        let key = host_key();
        app.state.workspaces[0].worktree_space = Some(crate::workspace::WorktreeSpaceMembership {
            key: format!("federation:{}", key.as_str()),
            label: "remote".to_string(),
            repo_root: std::path::PathBuf::from("/"),
            checkout_path: std::path::PathBuf::from("/"),
            is_linked_worktree: false,
        });
        let mut mirror = crate::remote::federation::reducer::RemoteMirror::new(Mount {
            host_key: key.clone(),
            server_instance_id: ServerInstanceId("inst-1".to_string()),
            mount_generation: 1,
        });
        let mut caps = BTreeSet::new();
        if staging {
            caps.insert(Capability::new(Capability::FILE_STAGING));
        }
        mirror.set_agreed_capabilities(caps);
        mirror
            .set_connection_epoch(crate::remote::federation::client::MountConnectionEpoch::mint());
        app.state.remote_mirrors.insert(key, mirror);

        // Anything the runtime emitted while coming up is setup noise, not an
        // observation about the key under test.
        while out_rx.try_recv().is_ok() {}
        (pane_id, out_rx)
    }

    fn drain(rx: &mut mpsc::UnboundedReceiver<FederationMessage>) -> Vec<FederationMessage> {
        let mut seen = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            seen.push(msg);
        }
        seen
    }

    /// Waits long enough for the pane's input-forwarding task to run before
    /// concluding that nothing was sent. A remote runtime hands user input to
    /// a bounded queue that a spawned task drains onto the mount, so an
    /// immediate `try_recv` would report "empty" for a key that was in fact
    /// forwarded a moment later.
    async fn assert_no_frame(rx: &mut mpsc::UnboundedReceiver<FederationMessage>, why: &str) {
        match tokio::time::timeout(std::time::Duration::from_millis(250), rx.recv()).await {
            Err(_) => {}
            Ok(frame) => panic!("{why}, but the mount received {frame:?}"),
        }
    }

    fn stage_requests(messages: &[FederationMessage]) -> Vec<&str> {
        messages
            .iter()
            .filter_map(|msg| match msg {
                FederationMessage::ClipboardStageRequest(request) => {
                    Some(request.original_filename.as_str())
                }
                _ => None,
            })
            .collect()
    }

    fn png(len: usize) -> crate::platform::ClipboardImage {
        crate::platform::ClipboardImage {
            bytes: vec![7u8; len],
            extension: "png",
        }
    }

    #[tokio::test]
    async fn image_paste_decision_is_capture_for_a_focused_mounted_remote_pane() {
        let mut app = test_app();
        let (pane_id, _out_rx) = attach_remote_mount(&mut app, true);

        assert_eq!(
            remote_image_paste_decision(&app.state, &ctrl_v()),
            RemoteImagePasteDecision::Capture {
                ws_idx: 0,
                target_pane_id: pane_id,
            }
        );

        // A key that is not the binding is never this intercept's business,
        // on a remote pane or anywhere else.
        assert_eq!(
            remote_image_paste_decision(
                &app.state,
                &TerminalKey::new(KeyCode::Char('x'), KeyModifiers::CONTROL)
            ),
            RemoteImagePasteDecision::FallThrough
        );
        // Neither is any key at all once the binding is cleared.
        app.state.remote_image_paste_key = None;
        assert_eq!(
            remote_image_paste_decision(&app.state, &ctrl_v()),
            RemoteImagePasteDecision::FallThrough
        );
    }

    #[tokio::test]
    async fn image_paste_stages_and_consumes_the_key_for_a_supplied_clipboard_image() {
        let mut app = test_app();
        let (pane_id, mut out_rx) = attach_remote_mount(&mut app, true);

        assert_eq!(
            app.handle_remote_image_paste(0, pane_id, png(4)),
            ImagePasteOutcome::Staged
        );
        assert_eq!(
            app.pending_remote_clipboard_stages.len(),
            1,
            "a staged paste must be waiting for its answer"
        );
        // The filename is asserted on the wire request, which is the only
        // place it exists: the pending entry deliberately does not carry it.
        assert_eq!(stage_requests(&drain(&mut out_rx)), vec!["image.png"]);
    }

    #[tokio::test]
    async fn image_paste_decision_is_fall_through_for_a_local_pane() {
        let mut app = test_app();
        let (_pane_id, mut rx) = attach_local_pane(&mut app);

        assert_eq!(
            remote_image_paste_decision(&app.state, &ctrl_v()),
            RemoteImagePasteDecision::FallThrough
        );

        // The decision value alone would still pass against an intercept that
        // returned on `FallThrough`, so drive the real handler and require the
        // key to actually arrive at the pane.
        app.handle_key(ctrl_v()).await;
        let mut received = Vec::new();
        while let Ok(bytes) = rx.try_recv() {
            received.extend_from_slice(&bytes);
        }
        assert_eq!(
            received,
            vec![0x16],
            "ctrl+v must still reach a local pane app"
        );
        assert!(
            app.state.toast.is_none(),
            "a local ctrl+v must not raise an image-paste toast"
        );
        assert!(app.pending_remote_clipboard_stages.is_empty());
    }

    #[tokio::test]
    async fn fall_through_still_reaches_non_terminal_mode_handlers() {
        let mut app = test_app();
        // The focused workspace is a live mounted remote one, so the only
        // thing keeping this press out of the intercept is the mode check.
        let (_pane_id, _out_rx) = attach_remote_mount(&mut app, true);
        app.state.workspaces.push(Workspace::test_new("second"));
        app.state.ensure_test_terminals();
        app.state.mode = Mode::Navigate;
        // Bound to the key navigate mode itself uses, so an intercept that
        // returned on its default branch would disable the mode's keymap.
        app.state.remote_image_paste_key = Some((KeyCode::Down, KeyModifiers::empty()));

        assert_eq!(
            remote_image_paste_decision(
                &app.state,
                &TerminalKey::new(KeyCode::Down, KeyModifiers::empty())
            ),
            RemoteImagePasteDecision::FallThrough
        );

        app.handle_key(TerminalKey::new(KeyCode::Down, KeyModifiers::empty()))
            .await;
        assert_eq!(
            app.state.selected, 1,
            "navigate mode's own key must still move the selection"
        );
    }

    #[tokio::test]
    async fn image_paste_decision_is_unsupported_when_the_mount_lacks_the_staging_capability() {
        let mut app = test_app();
        let (pane_id, mut out_rx) = attach_remote_mount(&mut app, false);

        assert_eq!(
            remote_image_paste_decision(&app.state, &ctrl_v()),
            RemoteImagePasteDecision::Unsupported {
                target_pane_id: pane_id
            }
        );

        app.handle_key(ctrl_v()).await;
        assert_eq!(
            app.state.toast.as_ref().map(|toast| toast.title.as_str()),
            Some(TOAST_TITLE_FAILED),
            "an unusable mount must tell the user instead of doing nothing"
        );
        // The peer can never answer a stage request, so nothing may be staged
        // — but the key is not taken away from the pane app either: a feature
        // that can never run on this mount must not cost the user ctrl+v for
        // the life of the mount, so the plain `0x16` still reaches the pane.
        assert_eq!(
            recv_input_bytes(
                &mut out_rx,
                "a mount that cannot stage must still let ctrl+v reach the pane app",
            )
            .await,
            vec![0x16]
        );
        assert!(app.pending_remote_clipboard_stages.is_empty());

        // Same unchanging fact, so it is reported once and not on every
        // press: the key now reaches the pane app, which makes pressing it
        // repeatedly ordinary use rather than a repeated request.
        app.state.toast = None;
        app.handle_key(ctrl_v()).await;
        assert!(
            app.state.toast.is_none(),
            "the unusable-mount notice repeated on a later press: {:?}",
            app.state.toast
        );

        // Positive control on the identical fixture: with the capability
        // agreed, a stage frame really does reach this receiver, so "nothing
        // was sent" above is a fact about the gate and not about the fixture.
        let mut app = test_app();
        let (pane_id, mut out_rx) = attach_remote_mount(&mut app, true);
        assert_eq!(
            remote_image_paste_decision(&app.state, &ctrl_v()),
            RemoteImagePasteDecision::Capture {
                ws_idx: 0,
                target_pane_id: pane_id,
            }
        );
        assert_eq!(
            app.handle_remote_image_paste(0, pane_id, png(4)),
            ImagePasteOutcome::Staged
        );
        assert_eq!(stage_requests(&drain(&mut out_rx)), vec!["image.png"]);
    }

    #[tokio::test]
    async fn oversized_clipboard_image_is_rejected_before_any_wire_send() {
        let mut app = test_app();
        let (pane_id, mut out_rx) = attach_remote_mount(&mut app, true);

        let oversized = png(crate::protocol::MAX_CLIPBOARD_IMAGE_PAYLOAD + 1);
        assert_eq!(
            app.handle_remote_image_paste(0, pane_id, oversized),
            ImagePasteOutcome::Rejected
        );
        assert_eq!(
            app.state.toast.as_ref().map(|toast| toast.context.as_str()),
            Some(TOAST_IMAGE_TOO_LARGE)
        );
        assert_no_frame(&mut out_rx, "an oversized image must not reach the wire").await;
        assert!(app.pending_remote_clipboard_stages.is_empty());

        // The same fixture, one byte under the limit: it does send, so the
        // assertions above describe the size check and not a dead mount.
        assert_eq!(
            app.handle_remote_image_paste(
                0,
                pane_id,
                png(crate::protocol::MAX_CLIPBOARD_IMAGE_PAYLOAD)
            ),
            ImagePasteOutcome::Staged
        );
        assert_eq!(stage_requests(&drain(&mut out_rx)), vec!["image.png"]);
    }

    /// A file under the OS temp dir with a recognized image extension, the
    /// shape a terminal's "paste image as path" convention produces. Cleaned
    /// up on drop so a failing assertion still leaves nothing behind.
    struct TempDropFile {
        path: std::path::PathBuf,
    }

    impl TempDropFile {
        fn new(bytes: &[u8]) -> Self {
            let path = std::env::temp_dir().join(format!(
                "herdr-bracketed-paste-test-{}-{}.png",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::write(&path, bytes).unwrap();
            Self { path }
        }
    }

    impl Drop for TempDropFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    #[tokio::test]
    async fn bracketed_paste_of_a_temp_image_path_stages_on_a_remote_pane() {
        let mut app = test_app();
        let (pane_id, mut out_rx) = attach_remote_mount(&mut app, true);
        let file = TempDropFile::new(b"image-bytes");

        assert!(matches!(
            bracketed_paste_image_decision(&app.state, &file.path.display().to_string()),
            BracketedPasteImageDecision::Capture {
                ws_idx: 0,
                target_pane_id,
                ..
            } if target_pane_id == pane_id
        ));

        app.handle_paste(file.path.display().to_string()).await;

        // The file read runs off-loop; the paste is answered when its
        // capture event comes back, exactly like the ctrl+v clipboard read.
        let ev = tokio::time::timeout(std::time::Duration::from_secs(10), app.event_rx.recv())
            .await
            .expect("the off-loop file read must answer")
            .expect("the capture sender is still alive");
        app.handle_internal_event(ev);

        assert_eq!(
            app.pending_remote_clipboard_stages.len(),
            1,
            "the path must have been staged, not forwarded as text"
        );
        assert_eq!(stage_requests(&drain(&mut out_rx)), vec!["image.png"]);
    }

    #[tokio::test]
    async fn bracketed_paste_of_a_temp_image_path_on_a_local_pane_is_forwarded_unchanged() {
        let mut app = test_app();
        let (_pane_id, mut rx) = attach_local_pane(&mut app);
        let file = TempDropFile::new(b"image-bytes");
        let text = file.path.display().to_string();

        assert!(matches!(
            bracketed_paste_image_decision(&app.state, &text),
            BracketedPasteImageDecision::FallThrough
        ));

        app.handle_paste(text.clone()).await;

        let mut received = Vec::new();
        while let Ok(bytes) = rx.try_recv() {
            received.extend_from_slice(&bytes);
        }
        assert_eq!(
            received,
            text.into_bytes(),
            "a local pane must receive the pasted path as ordinary text"
        );
        assert!(app.pending_remote_clipboard_stages.is_empty());
    }

    #[tokio::test]
    async fn bracketed_paste_of_ordinary_text_on_a_remote_pane_is_forwarded_unchanged() {
        let mut app = test_app();
        let (_pane_id, mut out_rx) = attach_remote_mount(&mut app, true);

        assert!(matches!(
            bracketed_paste_image_decision(&app.state, "not a path, just some pasted text"),
            BracketedPasteImageDecision::FallThrough
        ));

        app.handle_paste("not a path, just some pasted text".to_string())
            .await;

        assert!(app.pending_remote_clipboard_stages.is_empty());
        // The remote runtime forwards ordinary pastes over the wire as a
        // terminal write, not a `ClipboardStageRequest`, so the only thing
        // this test needs to prove is that staging never happened.
        assert!(stage_requests(&drain(&mut out_rx)).is_empty());
    }

    #[tokio::test]
    async fn bracketed_paste_of_a_temp_image_path_is_unsupported_without_file_staging() {
        let mut app = test_app();
        let (_pane_id, mut out_rx) = attach_remote_mount(&mut app, false);
        let file = TempDropFile::new(b"image-bytes");

        assert!(matches!(
            bracketed_paste_image_decision(&app.state, &file.path.display().to_string()),
            BracketedPasteImageDecision::Unsupported
        ));

        app.handle_paste(file.path.display().to_string()).await;

        assert_eq!(
            app.state.toast.as_ref().map(|toast| toast.title.as_str()),
            Some(TOAST_TITLE_FAILED),
            "a mount that cannot stage files must still tell the user, not silently forward"
        );
        assert!(app.pending_remote_clipboard_stages.is_empty());
        assert_no_frame(
            &mut out_rx,
            "an unsupported mount must not receive the raw path either",
        )
        .await;
    }

    #[tokio::test]
    async fn bracketed_paste_of_a_path_outside_the_temp_dir_is_forwarded_unchanged() {
        // Same shape as the accepted case (existing file, recognized
        // extension, single line) but outside any recognized drop location —
        // e.g. a path under the repo or the user's home directory that they
        // pasted on purpose, not one the terminal substituted for a
        // clipboard image.
        let mut app = test_app();
        let (_pane_id, mut out_rx) = attach_remote_mount(&mut app, true);
        let home = std::env::var("HOME").expect("HOME must be set to run this test");
        let path = std::path::PathBuf::from(home).join(format!(
            "herdr-bracketed-paste-outside-test-{}-not-a-drop.png",
            std::process::id()
        ));
        std::fs::write(&path, b"bytes").unwrap();

        assert!(
            crate::image_path::recognized_image_drop_location(&path).is_none(),
            "fixture must actually sit outside the temp dir root to exercise the gate"
        );
        assert!(matches!(
            bracketed_paste_image_decision(&app.state, &path.display().to_string()),
            BracketedPasteImageDecision::FallThrough
        ));

        let _ = std::fs::remove_file(&path);
        assert!(stage_requests(&drain(&mut out_rx)).is_empty());
    }

    #[tokio::test]
    async fn clipboard_stage_failure_raises_a_toast_with_the_documented_copy() {
        use crate::app::remote_clipboard_stage::clipboard_stage_failure_context;
        use crate::remote::federation::protocol::ClipboardStageFailure;

        // Every variant, with its shipped words: a mapping change has to be
        // deliberate, and no variant may be left without an explanation.
        for (failure, context) in [
            (
                ClipboardStageFailure::InvalidFilename,
                "the remote host rejected the file name",
            ),
            (
                ClipboardStageFailure::UnsupportedExtension,
                "the remote host does not accept this image type",
            ),
            (
                ClipboardStageFailure::InvalidPayload,
                "the image data did not survive the trip",
            ),
            (
                ClipboardStageFailure::PayloadTooLarge,
                "the image is too large for the remote host",
            ),
            (
                ClipboardStageFailure::QuotaExceeded,
                "the remote host's paste storage is full",
            ),
            (
                ClipboardStageFailure::StagingUnavailable,
                "the remote host has no usable temp folder",
            ),
            (
                ClipboardStageFailure::Busy,
                "the remote host is busy; paste again in a moment",
            ),
            (
                ClipboardStageFailure::WriteFailed,
                "the remote host could not write the file",
            ),
        ] {
            assert_eq!(clipboard_stage_failure_context(failure), context);

            // And the words actually reach the user through the real handler.
            let mut app = test_app();
            let (pane_id, _out_rx) = attach_remote_mount(&mut app, true);
            app.handle_remote_image_paste(0, pane_id, png(4));
            let request_id = *app
                .pending_remote_clipboard_stages
                .keys()
                .next()
                .expect("the stage was accepted");
            let epoch = app.state.remote_mirrors[&host_key()].connection_epoch();
            app.state.toast = None;
            app.handle_internal_event(AppEvent::FederationClipboardStageFailed {
                request_id,
                failure,
                origin: host_key(),
                connection_epoch: epoch,
            });
            let toast = app
                .state
                .toast
                .as_ref()
                .unwrap_or_else(|| panic!("{failure:?} was not reported to the user"));
            assert_eq!(toast.title, TOAST_TITLE_FAILED);
            assert_eq!(toast.context, context);
        }
    }

    #[test]
    fn every_clipboard_stage_toast_string_fits_the_status_line() {
        use crate::app::remote_clipboard_stage::clipboard_stage_failure_context;
        use crate::remote::federation::protocol::ClipboardStageFailure;
        use unicode_width::UnicodeWidthStr;

        // The status line renders the title and the context as single
        // unwrapped lines, so anything longer is hard-clipped with no
        // ellipsis — a silently truncated explanation.
        const MAX_CELLS: usize = 60;
        let mut strings = vec![
            TOAST_TITLE_FAILED,
            TOAST_TITLE_SAVING,
            TOAST_REMOTE_TOO_OLD,
            TOAST_NO_CLIPBOARD_IMAGE,
            TOAST_IMAGE_TOO_LARGE,
        ];
        for failure in [
            ClipboardStageFailure::InvalidFilename,
            ClipboardStageFailure::UnsupportedExtension,
            ClipboardStageFailure::InvalidPayload,
            ClipboardStageFailure::PayloadTooLarge,
            ClipboardStageFailure::QuotaExceeded,
            ClipboardStageFailure::StagingUnavailable,
            ClipboardStageFailure::Busy,
            ClipboardStageFailure::WriteFailed,
        ] {
            strings.push(clipboard_stage_failure_context(failure));
        }
        for text in strings {
            assert!(!text.is_empty());
            assert!(
                UnicodeWidthStr::width(text) <= MAX_CELLS,
                "{text:?} is {} cells and would be clipped",
                UnicodeWidthStr::width(text)
            );
        }
    }

    #[tokio::test]
    async fn a_slow_stage_tells_the_user_it_is_still_working() {
        let mut app = test_app();
        let (pane_id, _out_rx) = attach_remote_mount(&mut app, true);
        app.handle_remote_image_paste(0, pane_id, png(4));
        let request_id = *app
            .pending_remote_clipboard_stages
            .keys()
            .next()
            .expect("the stage was accepted");
        app.state.toast = None;

        app.handle_internal_event(AppEvent::FederationClipboardStageStillRunning { request_id });

        assert_eq!(
            app.state.toast.as_ref().map(|toast| toast.title.as_str()),
            Some(TOAST_TITLE_SAVING)
        );
    }

    /// How long the fake clipboard owner stalls. Long enough that a caller
    /// which waited for it could not possibly be mistaken for one that did
    /// not, short enough that the test is not slow.
    const FAKE_CLIPBOARD_STALL: std::time::Duration = std::time::Duration::from_millis(300);

    #[tokio::test]
    async fn a_clipboard_read_runs_off_the_caller_and_answers_as_an_event() {
        use crate::events::ClipboardImageCapture;

        let mut app = test_app();
        let (pane_id, _out_rx) = attach_remote_mount(&mut app, true);
        let (tx, mut rx) = mpsc::channel(4);
        let started = std::time::Instant::now();
        spawn_clipboard_image_capture(
            tx,
            "ws-1".to_string(),
            pane_id,
            std::time::Duration::from_secs(30),
            || {
                std::thread::sleep(FAKE_CLIPBOARD_STALL);
                Some(png(4))
            },
        );
        // The caller here stands in for the single App event loop, which
        // drives rendering, every pane and the API loop. It has to come back
        // long before the clipboard owner does.
        let handed_back = started.elapsed();
        assert!(
            handed_back < FAKE_CLIPBOARD_STALL / 3,
            "the clipboard read blocked the caller for {handed_back:?}"
        );

        let ev = tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
            .await
            .expect("the read must answer")
            .expect("the sender is still alive");
        match ev {
            AppEvent::RemoteClipboardImageCaptured {
                workspace_id,
                target_pane_id,
                capture: ClipboardImageCapture::Image(image),
            } => {
                assert_eq!(workspace_id, "ws-1");
                assert_eq!(target_pane_id, pane_id);
                assert_eq!(image, png(4));
            }
            other => panic!("expected the captured image, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_clipboard_owner_that_never_answers_is_abandoned_and_reported() {
        use crate::events::ClipboardImageCapture;

        let mut app = test_app();
        let (pane_id, _out_rx) = attach_remote_mount(&mut app, true);
        let (tx, mut rx) = mpsc::channel(4);
        spawn_clipboard_image_capture(
            tx,
            "ws-1".to_string(),
            pane_id,
            std::time::Duration::from_millis(50),
            || {
                std::thread::sleep(std::time::Duration::from_secs(30));
                Some(png(4))
            },
        );

        let ev = tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
            .await
            .expect("an abandoned read must still resolve the press")
            .expect("the sender is still alive");
        assert!(
            matches!(
                ev,
                AppEvent::RemoteClipboardImageCaptured {
                    capture: ClipboardImageCapture::ReadTimedOut,
                    ..
                }
            ),
            "expected an abandoned read, got {ev:?}"
        );

        let workspace_id = app.state.workspaces[0].id.clone();
        app.handle_internal_event(AppEvent::RemoteClipboardImageCaptured {
            workspace_id,
            target_pane_id: pane_id,
            capture: ClipboardImageCapture::ReadTimedOut,
        });
        assert_eq!(
            app.state.toast.as_ref().map(|toast| toast.context.as_str()),
            Some(TOAST_CLIPBOARD_READ_TIMED_OUT)
        );
    }

    #[tokio::test]
    async fn an_image_paste_press_consumes_the_key_without_reading_the_clipboard_inline() {
        let mut app = test_app();
        let (_pane_id, mut out_rx) = attach_remote_mount(&mut app, true);
        assert!(matches!(
            remote_image_paste_decision(&app.state, &ctrl_v()),
            RemoteImagePasteDecision::Capture { .. }
        ));

        app.handle_key(ctrl_v()).await;

        // A press answered inline would have resolved by now, one way or the
        // other: either the clipboard held an image and a stage is pending, or
        // it did not and the user was told so. Neither may have happened yet.
        assert!(
            app.pending_remote_clipboard_stages.is_empty(),
            "the press was answered on the event loop"
        );
        assert!(
            app.state.toast.is_none(),
            "the press was answered on the event loop: {:?}",
            app.state.toast
        );
        assert_no_frame(&mut out_rx, "a consumed key must not reach the remote PTY").await;
    }

    #[tokio::test]
    async fn a_captured_clipboard_image_is_staged_for_the_workspace_that_asked() {
        use crate::events::ClipboardImageCapture;

        let mut app = test_app();
        let (pane_id, mut out_rx) = attach_remote_mount(&mut app, true);
        let workspace_id = app.state.workspaces[0].id.clone();

        app.handle_internal_event(AppEvent::RemoteClipboardImageCaptured {
            workspace_id: workspace_id.clone(),
            target_pane_id: pane_id,
            capture: ClipboardImageCapture::Image(png(4)),
        });
        assert_eq!(app.pending_remote_clipboard_stages.len(), 1);
        assert_eq!(stage_requests(&drain(&mut out_rx)), vec!["image.png"]);

        // An empty clipboard is reported, not silently dropped.
        app.state.toast = None;
        app.handle_internal_event(AppEvent::RemoteClipboardImageCaptured {
            workspace_id: workspace_id.clone(),
            target_pane_id: pane_id,
            capture: ClipboardImageCapture::NoImage,
        });
        assert_eq!(
            app.state.toast.as_ref().map(|toast| toast.context.as_str()),
            Some(TOAST_NO_CLIPBOARD_IMAGE)
        );

        // A workspace that closed while the read ran takes its press with it,
        // rather than the id resolving to whatever now sits in that slot.
        app.state.toast = None;
        app.pending_remote_clipboard_stages.clear();
        app.handle_internal_event(AppEvent::RemoteClipboardImageCaptured {
            workspace_id: "a workspace that no longer exists".to_string(),
            target_pane_id: pane_id,
            capture: ClipboardImageCapture::Image(png(4)),
        });
        assert!(app.pending_remote_clipboard_stages.is_empty());
        assert_no_frame(
            &mut out_rx,
            "a closed workspace's press must not reach the wire",
        )
        .await;
    }

    #[tokio::test]
    async fn a_stage_that_already_finished_never_raises_the_still_working_toast() {
        let mut app = test_app();
        let (pane_id, _out_rx) = attach_remote_mount(&mut app, true);
        app.handle_remote_image_paste(0, pane_id, png(4));
        let request_id = *app
            .pending_remote_clipboard_stages
            .keys()
            .next()
            .expect("the stage was accepted");
        // The answer beat the timer.
        app.pending_remote_clipboard_stages.remove(&request_id);
        app.state.toast = None;

        app.handle_internal_event(AppEvent::FederationClipboardStageStillRunning { request_id });

        assert!(
            app.state.toast.is_none(),
            "a resolved stage must not be announced as still running"
        );
    }

    // -----------------------------------------------------------------
    // The dispatcher every attached client actually goes through.
    //
    // `handle_key` only runs in the monolithic `--no-session` loop; a normal
    // session routes keys through `handle_terminal_key_headless_from`, so
    // these cases assert the intercept on that path rather than on the
    // decision function alone.
    // -----------------------------------------------------------------

    /// Raw terminal-input bytes the mount received, in arrival order.
    fn input_bytes(messages: &[FederationMessage]) -> Vec<u8> {
        messages
            .iter()
            .filter_map(|msg| match msg {
                FederationMessage::Terminal(
                    crate::remote::federation::protocol::TerminalChannelMessage::Input {
                        bytes,
                        ..
                    },
                ) => Some(bytes.clone()),
                _ => None,
            })
            .flatten()
            .collect()
    }

    /// Waits for the pane's input-forwarding task to put the key on the mount.
    /// A remote runtime hands user input to a bounded queue drained by a
    /// spawned task, so an immediate `try_recv` would report "empty" for a key
    /// that is forwarded a moment later.
    async fn recv_input_bytes(
        rx: &mut mpsc::UnboundedReceiver<FederationMessage>,
        why: &str,
    ) -> Vec<u8> {
        let mut seen = Vec::new();
        loop {
            match tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv()).await {
                Ok(Some(msg)) => {
                    seen.push(msg);
                    let bytes = input_bytes(&seen);
                    if !bytes.is_empty() {
                        return bytes;
                    }
                }
                Ok(None) | Err(_) => panic!("{why}, but the mount received {seen:?}"),
            }
        }
    }

    /// One raw input event, as the attached-client dispatcher sees it.
    fn raw(key: TerminalKey) -> crate::raw_input::RawInputEvent {
        crate::raw_input::RawInputEvent::Key(key)
    }

    /// How many clipboard reads a key path started.
    ///
    /// Counted by their answers: a started read always resolves into exactly
    /// one `AppEvent::RemoteClipboardImageCaptured` on this App's own event
    /// channel, so the count is per-App and unaffected by other tests running
    /// in the same process. In a test build the read itself is the instant
    /// no-op seam (`clipboard_image_reader`), so this settles immediately
    /// instead of spawning one `osascript`/`wl-paste` per counted read.
    async fn clipboard_reads_started(app: &mut App) -> usize {
        let mut started = 0;
        loop {
            match tokio::time::timeout(std::time::Duration::from_millis(250), app.event_rx.recv())
                .await
            {
                Ok(Some(AppEvent::RemoteClipboardImageCaptured { .. })) => started += 1,
                Ok(Some(_)) => continue,
                Ok(None) | Err(_) => return started,
            }
        }
    }

    #[tokio::test]
    async fn live_key_dispatch_intercepts_the_image_paste_key_on_a_mounted_remote_pane() {
        let mut app = test_app();
        let (pane_id, mut out_rx) = attach_remote_mount(&mut app, true);
        let workspace_id = app.state.workspaces[0].id.clone();

        assert!(
            app.handle_terminal_key_headless(ctrl_v()).is_none(),
            "the intercept must claim the key instead of forwarding it"
        );

        // The off-loop clipboard read is what proves the capture branch ran:
        // it answers as an event addressed at the pane the decision resolved,
        // whatever the OS clipboard happens to hold on this machine.
        match tokio::time::timeout(std::time::Duration::from_secs(10), app.event_rx.recv()).await {
            Ok(Some(AppEvent::RemoteClipboardImageCaptured {
                workspace_id: captured_workspace_id,
                target_pane_id,
                ..
            })) => {
                assert_eq!(captured_workspace_id, workspace_id);
                assert_eq!(target_pane_id, pane_id);
            }
            other => panic!("the capture branch must start a clipboard read, got {other:?}"),
        }
        assert_no_frame(
            &mut out_rx,
            "an intercepted image-paste key must not also reach the remote PTY",
        )
        .await;
    }

    #[tokio::test]
    async fn live_key_dispatch_forwards_the_image_paste_key_on_a_local_pane() {
        let mut app = test_app();
        let (_pane_id, mut rx) = attach_local_pane(&mut app);

        app.handle_terminal_key_headless(ctrl_v());

        let mut received = Vec::new();
        while let Ok(bytes) = rx.try_recv() {
            received.extend_from_slice(&bytes);
        }
        assert_eq!(
            received,
            vec![0x16],
            "ctrl+v must still reach a local pane app"
        );
        assert!(
            app.state.toast.is_none(),
            "a local ctrl+v must not raise an image-paste toast"
        );
        assert!(app.pending_remote_clipboard_stages.is_empty());
    }

    #[tokio::test]
    async fn live_key_dispatch_forwards_the_image_paste_key_when_the_binding_is_disabled() {
        let mut app = test_app();
        let (_pane_id, mut out_rx) = attach_remote_mount(&mut app, true);
        // An empty `keys.remote_image_paste` is how a user turns the raw-key
        // intercept off; the key then belongs to the pane app again, even on
        // a mount that could have staged the image.
        app.state.remote_image_paste_key = None;

        app.handle_terminal_key_headless(ctrl_v());

        assert_eq!(
            recv_input_bytes(
                &mut out_rx,
                "a disabled binding must leave ctrl+v to the remote PTY"
            )
            .await,
            vec![0x16]
        );
        assert!(app.state.toast.is_none());
        assert!(app.pending_remote_clipboard_stages.is_empty());
    }

    #[tokio::test]
    async fn live_key_dispatch_forwards_a_non_matching_key_on_a_mounted_remote_pane() {
        let mut app = test_app();
        let (_pane_id, mut out_rx) = attach_remote_mount(&mut app, true);

        app.handle_terminal_key_headless(TerminalKey::new(
            KeyCode::Char('x'),
            KeyModifiers::CONTROL,
        ));

        assert_eq!(
            recv_input_bytes(
                &mut out_rx,
                "a key that is not the binding must reach the remote PTY untouched"
            )
            .await,
            vec![0x18]
        );
        assert!(app.state.toast.is_none());
    }

    #[tokio::test]
    async fn live_key_dispatch_reports_a_mount_that_cannot_stage_but_still_delivers_the_key() {
        let mut app = test_app();
        let (_pane_id, mut out_rx) = attach_remote_mount(&mut app, false);

        app.handle_terminal_key_headless(ctrl_v());

        assert_eq!(
            app.state.toast.as_ref().map(|toast| toast.title.as_str()),
            Some(TOAST_TITLE_FAILED),
            "an unusable mount must tell the user instead of doing nothing"
        );
        // Nothing can be staged on this peer, but the key is not confiscated
        // for it: the feature can never work here, and consuming ctrl+v for
        // the life of the mount would cost the pane app a key it needs
        // (readline quoted-insert, vim visual-block) in exchange for nothing.
        assert_eq!(
            recv_input_bytes(
                &mut out_rx,
                "a mount that cannot stage must still let ctrl+v reach the pane app",
            )
            .await,
            vec![0x16]
        );
        assert!(app.pending_remote_clipboard_stages.is_empty());

        // Once, not per press. The notice is about a mount capability that
        // cannot change while the mount lives, and the key reaching the pane
        // app makes pressing it repeatedly ordinary use.
        app.state.toast = None;
        app.handle_terminal_key_headless(ctrl_v());
        assert!(
            app.state.toast.is_none(),
            "the unusable-mount notice repeated on a later press: {:?}",
            app.state.toast
        );
        assert_eq!(
            recv_input_bytes(&mut out_rx, "the second press must reach the pane app too").await,
            vec![0x16],
            "silencing the repeated notice must not also silence the key"
        );
    }

    #[tokio::test]
    async fn live_key_dispatch_starts_one_clipboard_read_for_one_physical_press() {
        let mut app = test_app();
        let (_pane_id, mut out_rx) = attach_remote_mount(&mut app, true);

        app.route_client_events(
            vec![
                raw(ctrl_v()),
                raw(ctrl_v().with_kind(crossterm::event::KeyEventKind::Release)),
            ],
            false,
        );

        assert_eq!(
            clipboard_reads_started(&mut app).await,
            1,
            "one press must start exactly one clipboard read"
        );
        assert_no_frame(
            &mut out_rx,
            "a claimed image-paste key must not also reach the remote PTY",
        )
        .await;
    }

    // Under the enhanced keyboard protocol a held key also reports `Repeat`,
    // and the lease table reprocesses the repeats of a *consumed* press back
    // through the same handler. Without a press gate, leaning on ctrl+v for a
    // second would start a clipboard read, a blocking thread, a staged remote
    // file and a pasted path per repeat.
    #[tokio::test]
    async fn live_key_dispatch_starts_no_further_clipboard_read_while_the_key_is_held() {
        let mut app = test_app();
        let (_pane_id, mut out_rx) = attach_remote_mount(&mut app, true);

        let mut events = vec![raw(ctrl_v())];
        events.extend(
            (0..5).map(|_| raw(ctrl_v().with_kind(crossterm::event::KeyEventKind::Repeat))),
        );
        events.push(raw(
            ctrl_v().with_kind(crossterm::event::KeyEventKind::Release)
        ));
        app.route_client_events(events, false);

        assert_eq!(
            clipboard_reads_started(&mut app).await,
            1,
            "the press starts one clipboard read and the five repeats start none"
        );
        // A repeat is consumed rather than passed on: forwarding it would
        // send the remote PTY the very `0x16` the press withheld.
        assert_no_frame(
            &mut out_rx,
            "a repeat of a claimed image-paste key must not reach the remote PTY",
        )
        .await;
    }

    // A host can also report a held key as one event with a repeat count,
    // which the lease table expands into `repeat_count - 1` reprocessed
    // repeats. Same defect, same guard, different arrival shape.
    #[tokio::test]
    async fn live_key_dispatch_starts_one_clipboard_read_for_a_pre_counted_repeat() {
        let mut app = test_app();
        let (_pane_id, mut out_rx) = attach_remote_mount(&mut app, true);

        app.route_client_events(vec![raw(ctrl_v().with_repeat_count(3))], false);

        assert_eq!(
            clipboard_reads_started(&mut app).await,
            1,
            "a press carrying a repeat count must still start one clipboard read"
        );
        assert_no_frame(
            &mut out_rx,
            "the expanded repeats must not reach the remote PTY either",
        )
        .await;
    }

    // -----------------------------------------------------------------
    // The empty bracketed paste, which is what Warp emits for Cmd+V when the
    // clipboard holds an image and therefore has no text flavor.
    //
    // Measured on macOS 2026-08-07 with a screenshot on the clipboard:
    //   Warp            -> ESC[200~ ESC[201~        (EMPTY_BRACKETED_PASTE)
    //   cmux            -> ESC[200~/var/...png ESC[201~
    //   Terminal.app    -> nothing at all
    // These fixtures are those exact byte strings, fed through the real
    // `raw_input` parser, so a change in either the parser or the intercept
    // shows up here rather than in a hand-written `Paste(...)` event.
    // -----------------------------------------------------------------

    /// Warp's Cmd+V with an image-only clipboard, byte for byte.
    const EMPTY_BRACKETED_PASTE: &[u8] = b"\x1b[200~\x1b[201~";

    #[test]
    fn the_measured_warp_cmd_v_bytes_parse_as_an_empty_paste() {
        // The whole bridge rests on this: if the parser ever stopped turning
        // the empty bracket pair into an empty `Paste`, every test below would
        // still pass while the feature was dead.
        let events = crate::raw_input::parse_raw_input_bytes_sync(EMPTY_BRACKETED_PASTE);
        assert!(
            matches!(events.as_slice(), [crate::raw_input::RawInputEvent::Paste(text)] if text.is_empty()),
            "the measured Warp bytes must arrive as one empty paste, got {events:?}"
        );
    }

    #[tokio::test]
    async fn empty_bracketed_paste_starts_one_clipboard_read_on_a_mounted_remote_pane() {
        let mut app = test_app();
        let (_pane_id, mut out_rx) = attach_remote_mount(&mut app, true);

        app.route_client_input(EMPTY_BRACKETED_PASTE.to_vec());

        assert_eq!(
            clipboard_reads_started(&mut app).await,
            1,
            "an empty bracketed paste on a staging-capable mount must start exactly one clipboard read"
        );
        assert_no_frame(
            &mut out_rx,
            "a claimed empty paste must not also reach the remote PTY",
        )
        .await;
        assert!(app.pending_remote_clipboard_stages.is_empty());
    }

    /// The legacy dispatcher (`herdr --remote`, `--no-session`) shares the same
    /// intercept, so it must reach the same conclusion from the same input.
    #[tokio::test]
    async fn empty_bracketed_paste_starts_one_clipboard_read_on_the_legacy_paste_path() {
        let mut app = test_app();
        let (_pane_id, mut out_rx) = attach_remote_mount(&mut app, true);

        app.handle_paste(String::new()).await;

        assert_eq!(
            clipboard_reads_started(&mut app).await,
            1,
            "the legacy paste path must claim the empty paste too"
        );
        assert_no_frame(
            &mut out_rx,
            "a claimed empty paste must not also reach the remote PTY",
        )
        .await;
    }

    /// An empty bracketed paste is *also* what a genuinely empty text clipboard
    /// produces, and the two are indistinguishable here. The outcome must be
    /// the ordinary "no image" notice, not an error and not a lost keystroke.
    #[tokio::test]
    async fn empty_bracketed_paste_with_no_clipboard_image_reports_it_and_stages_nothing() {
        let mut app = test_app();
        let (_pane_id, mut out_rx) = attach_remote_mount(&mut app, true);

        app.route_client_input(EMPTY_BRACKETED_PASTE.to_vec());

        // The test-build reader answers "no image" instantly, which is exactly
        // the empty-clipboard case; the answer is handled on the real path.
        let ev = tokio::time::timeout(std::time::Duration::from_secs(10), app.event_rx.recv())
            .await
            .expect("the off-loop clipboard read must answer")
            .expect("the capture sender is still alive");
        assert!(
            matches!(
                ev,
                AppEvent::RemoteClipboardImageCaptured {
                    capture: crate::events::ClipboardImageCapture::NoImage,
                    ..
                }
            ),
            "the empty-clipboard case must resolve as NoImage, got {ev:?}"
        );
        app.handle_internal_event(ev);

        assert_eq!(
            app.state.toast.as_ref().map(|toast| toast.context.as_str()),
            Some(TOAST_NO_CLIPBOARD_IMAGE),
            "an empty clipboard must be reported, not swallowed"
        );
        assert!(app.pending_remote_clipboard_stages.is_empty());
        assert_no_frame(&mut out_rx, "nothing may be staged for an empty clipboard").await;
    }

    #[tokio::test]
    async fn empty_bracketed_paste_on_an_ordinary_local_pane_is_forwarded_unchanged() {
        let mut app = test_app();
        let (_pane_id, mut rx) = attach_local_pane(&mut app);

        app.route_client_input(EMPTY_BRACKETED_PASTE.to_vec());

        // The pane's bracketed-paste mode is off in this fixture, so an empty
        // paste forwards an empty payload — the point is that a payload was
        // forwarded at all rather than claimed by the intercept.
        assert!(
            rx.try_recv().is_ok(),
            "an empty paste on a local pane must still reach the pane"
        );
        assert_eq!(
            clipboard_reads_started(&mut app).await,
            0,
            "a local pane must never trigger a clipboard read"
        );
        assert!(app.state.toast.is_none());
        assert!(app.pending_remote_clipboard_stages.is_empty());
    }

    /// Same contract as the key intercept on a peer that cannot stage: the
    /// paste is delivered rather than confiscated, and the unchanging reason is
    /// reported at most once per pane.
    #[tokio::test]
    async fn empty_bracketed_paste_reports_a_mount_that_cannot_stage_but_still_delivers_it() {
        let mut app = test_app();
        let (_pane_id, mut out_rx) = attach_remote_mount(&mut app, false);

        assert_eq!(
            app.dispatch_bracketed_paste_image(""),
            RemoteImagePasteKeyDisposition::Forward,
            "a mount that can never stage must not swallow the paste"
        );
        assert_eq!(
            app.state.toast.as_ref().map(|toast| toast.title.as_str()),
            Some(TOAST_TITLE_FAILED),
            "an unusable mount must tell the user instead of doing nothing"
        );

        app.state.toast = None;
        assert_eq!(
            app.dispatch_bracketed_paste_image(""),
            RemoteImagePasteKeyDisposition::Forward
        );
        assert!(
            app.state.toast.is_none(),
            "the unusable-mount notice repeated on a later paste: {:?}",
            app.state.toast
        );

        assert_eq!(
            clipboard_reads_started(&mut app).await,
            0,
            "a peer that cannot stage must not cause a clipboard read"
        );
        assert!(app.pending_remote_clipboard_stages.is_empty());
        assert_no_frame(&mut out_rx, "nothing may be staged on an unusable mount").await;
    }

    /// `keys.remote_image_paste = ""` is documented as the off switch for
    /// clipboard image paste, so it must silence this trigger too — otherwise
    /// a user who turned the feature off to stop herdr reading their clipboard
    /// still gets a clipboard read out of an ordinary Cmd+V.
    #[tokio::test]
    async fn empty_bracketed_paste_is_forwarded_when_the_binding_is_disabled() {
        let mut app = test_app();
        let (_pane_id, mut out_rx) = attach_remote_mount(&mut app, true);
        app.state.remote_image_paste_key = None;

        assert_eq!(
            app.dispatch_bracketed_paste_image(""),
            RemoteImagePasteKeyDisposition::Forward,
            "a disabled binding must leave the empty paste to the pane"
        );

        assert_eq!(
            clipboard_reads_started(&mut app).await,
            0,
            "a disabled binding must not read the clipboard"
        );
        assert!(app.state.toast.is_none());
        assert!(app.pending_remote_clipboard_stages.is_empty());
        assert_no_frame(&mut out_rx, "nothing may be staged with the feature off").await;
    }

    /// The press gate that bounds a held ctrl+v cannot help here: a bracketed
    /// paste carries no key kind and never enters the lease table. Without a
    /// per-pane in-flight guard a held Cmd+V, or a terminal replaying a paste,
    /// would start one `spawn_blocking` clipboard child per event.
    #[tokio::test]
    async fn a_flood_of_empty_bracketed_pastes_starts_one_clipboard_read() {
        let mut app = test_app();
        let (_pane_id, mut out_rx) = attach_remote_mount(&mut app, true);

        for _ in 0..8 {
            app.route_client_input(EMPTY_BRACKETED_PASTE.to_vec());
        }

        // Collected rather than merely counted: the one read that did start
        // has to be handed back to the App below, because handling its answer
        // is what releases the pane for the next paste.
        let mut captures = Vec::new();
        loop {
            match tokio::time::timeout(std::time::Duration::from_millis(250), app.event_rx.recv())
                .await
            {
                Ok(Some(ev @ AppEvent::RemoteClipboardImageCaptured { .. })) => captures.push(ev),
                Ok(Some(_)) => continue,
                Ok(None) | Err(_) => break,
            }
        }
        assert_eq!(
            captures.len(),
            1,
            "eight pastes arriving while one read is outstanding must start one read, not eight"
        );
        assert_no_frame(
            &mut out_rx,
            "a claimed empty paste must not reach the remote PTY, flood or not",
        )
        .await;

        // The guard bounds a flood; it must not cost the user a later paste
        // they actually meant. Once the read answers, the pane is free again.
        for ev in captures {
            app.handle_internal_event(ev);
        }
        app.route_client_input(EMPTY_BRACKETED_PASTE.to_vec());
        assert_eq!(
            clipboard_reads_started(&mut app).await,
            1,
            "a distinct paste with no read outstanding must still be answered"
        );
    }

    /// Both per-pane sets are keyed by `PaneId` and unreachable once the pane
    /// is gone, so a long-lived session would otherwise accumulate an entry
    /// for every pane the user ever tried the feature on.
    #[tokio::test]
    async fn remote_image_paste_pane_state_is_purged_when_the_workspace_closes() {
        let mut app = test_app();
        let (pane_id, _out_rx) = attach_remote_mount(&mut app, true);
        app.state
            .workspaces
            .push(crate::workspace::Workspace::test_new("survivor"));
        app.state.ensure_test_terminals();
        let survivor_pane_id = app.state.workspaces[1].tabs[0].root_pane;

        app.remote_image_paste_unsupported_notices.insert(pane_id);
        app.remote_image_paste_unsupported_notices
            .insert(survivor_pane_id);
        app.remote_clipboard_image_reads_in_flight.insert(pane_id);
        app.remote_clipboard_image_reads_in_flight
            .insert(survivor_pane_id);

        let closing: std::collections::HashSet<String> =
            [app.state.workspaces[0].id.clone()].into_iter().collect();
        app.purge_remote_image_paste_pane_state_for_workspaces(&closing);

        assert!(
            !app.remote_image_paste_unsupported_notices
                .contains(&pane_id),
            "the closing workspace's notice must go with it"
        );
        assert!(
            !app.remote_clipboard_image_reads_in_flight
                .contains(&pane_id),
            "the closing workspace's in-flight marker must go with it"
        );
        assert!(
            app.remote_image_paste_unsupported_notices
                .contains(&survivor_pane_id),
            "another workspace's notice must survive"
        );
        assert!(
            app.remote_clipboard_image_reads_in_flight
                .contains(&survivor_pane_id),
            "another workspace's in-flight marker must survive"
        );
    }

    /// The cmux shape, unchanged by the empty-paste bridge: a non-empty paste
    /// carrying a temp image path still goes through the path-shape gate.
    #[tokio::test]
    async fn bracketed_paste_of_the_measured_cmux_temp_path_still_stages() {
        let mut app = test_app();
        let (_pane_id, mut out_rx) = attach_remote_mount(&mut app, true);
        let file = TempDropFile::new(b"image-bytes");

        let mut bytes = b"\x1b[200~".to_vec();
        bytes.extend_from_slice(file.path.display().to_string().as_bytes());
        bytes.extend_from_slice(b"\x1b[201~");
        app.route_client_input(bytes);

        let ev = tokio::time::timeout(std::time::Duration::from_secs(10), app.event_rx.recv())
            .await
            .expect("the off-loop file read must answer")
            .expect("the capture sender is still alive");
        app.handle_internal_event(ev);

        assert_eq!(
            app.pending_remote_clipboard_stages.len(),
            1,
            "the path must have been staged, not forwarded as text"
        );
        assert_eq!(stage_requests(&drain(&mut out_rx)), vec!["image.png"]);
    }
}
