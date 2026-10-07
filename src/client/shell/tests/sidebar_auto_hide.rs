use super::*;

fn auto_hide_state() -> ClientShellState {
    let mut config = Config::default();
    config.ui.sidebar_auto_hide = true;
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&config));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    state
}

fn docked_state(collapsed: bool) -> ClientShellState {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    state.sidebar_collapsed = collapsed;
    state
}

fn press_toggle(state: &mut ClientShellState) -> ClientShellInput {
    let mut outcome = ClientShellInput::default();
    state.record_binding(
        crate::input::KeybindMatch::Action(crate::input::KeybindAction::ToggleSidebar),
        &mut outcome,
    );
    outcome
}

#[test]
fn auto_hide_layout_matches_collapsed_docked_layout() {
    let auto = auto_hide_state().layout(106, 30).pane_surface;
    assert_eq!(auto, docked_state(true).layout(106, 30).pane_surface);
    assert_ne!(auto, docked_state(false).layout(106, 30).pane_surface);
}

#[test]
fn auto_hide_toggle_pins_and_unpins_without_touching_preference() {
    let mut state = auto_hide_state();
    let before = state.sidebar_collapsed;

    let outcome = press_toggle(&mut state);
    assert!(state.sidebar_auto_hide_pinned);
    assert!(!state.sidebar_layout_collapsed());
    assert!(outcome.resize);
    assert_eq!(state.sidebar_collapsed, before);
    assert!(!state.sidebar_collapsed_manual);

    let outcome = press_toggle(&mut state);
    assert!(!state.sidebar_auto_hide_pinned);
    assert!(state.sidebar_layout_collapsed());
    assert!(outcome.resize);
}

#[test]
fn auto_hide_navigate_reveals_drawer_without_surface_resize() {
    let mut state = auto_hide_state();
    let size = state.surface_size(106, 30);

    let frame = state.compose(106, 30).expect("docked frame");
    assert!(frame_rows(&frame).join("\n").contains("LIVE"));
    assert_eq!(state.hits.sidebar_overlay, Rect::default());

    state.mode = ClientShellMode::Navigate;
    let frame = state.compose(106, 30).expect("drawer frame");
    assert_eq!(state.surface_size(106, 30), size);
    assert_eq!(state.hits.sidebar_overlay.width, state.sidebar_width);
    assert_eq!(state.hits.sidebar_overlay.x, 0);
    let rows = frame_rows(&frame).join("\n");
    assert!(rows.contains("spaces"));
    assert!(!rows.contains("LIVE"));
    assert!(frame.cursor.is_none());
}

#[test]
fn auto_hide_drawer_hidden_while_overlay_open() {
    let mut state = auto_hide_state();
    state.mode = ClientShellMode::Navigate;
    assert!(state.sidebar_overlay_visible());

    state.open_settings_overlay();
    assert!(!state.sidebar_overlay_visible());
}

#[test]
fn auto_hide_off_keeps_toggle_behaviour() {
    let mut state = docked_state(false);

    let outcome = press_toggle(&mut state);
    assert!(state.sidebar_collapsed);
    assert!(state.sidebar_collapsed_manual);
    assert!(outcome.resize);
    assert!(!state.sidebar_auto_hide_pinned);
}

#[test]
fn auto_hide_initial_surface_size_is_collapsed() {
    let mut auto = Config::default();
    auto.ui.sidebar_auto_hide = true;
    let mut collapsed = Config::default();
    collapsed.ui.sidebar_start_collapsed = true;

    assert_eq!(
        ClientShellConfig::from_config(&auto).initial_surface_size(106, 30),
        ClientShellConfig::from_config(&collapsed).initial_surface_size(106, 30)
    );
}

fn moved(column: u16, row: u16) -> RawInputEvent {
    RawInputEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Moved,
        column,
        row,
        modifiers: KeyModifiers::empty(),
    })
}

fn mouse_event(kind: MouseEventKind, column: u16, row: u16) -> RawInputEvent {
    RawInputEvent::Mouse(MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::empty(),
    })
}

#[test]
fn auto_hide_pointer_on_strip_reveals_and_leaving_hides_without_resize() {
    let mut state = auto_hide_state();
    state.compose(106, 30).expect("docked frame");
    let size = state.surface_size(106, 30);

    let outcome = state.handle_raw_events(vec![moved(1, 5)]);
    assert!(state.sidebar_hover_reveal);
    assert!(outcome.repaint);
    assert!(!outcome.resize);
    state.compose(106, 30).expect("drawer frame");
    assert_eq!(state.hits.sidebar_overlay.width, state.sidebar_width);

    let outside = state.hits.sidebar_overlay.right() + 2;
    let outcome = state.handle_raw_events(vec![moved(outside, 5)]);
    assert!(!state.sidebar_hover_reveal);
    assert!(outcome.repaint);
    assert!(!outcome.resize);
    assert_eq!(state.surface_size(106, 30), size);
}

#[test]
fn auto_hide_hidden_mode_reveals_only_from_column_zero() {
    let mut config = Config::default();
    config.ui.sidebar_auto_hide = true;
    config.ui.sidebar_collapsed_mode = crate::config::SidebarCollapsedModeConfig::Hidden;
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&config));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    state.compose(106, 30).expect("docked frame");

    state.handle_raw_events(vec![moved(1, 5)]);
    assert!(!state.sidebar_hover_reveal);
    state.handle_raw_events(vec![moved(0, 5)]);
    assert!(state.sidebar_hover_reveal);
}

#[test]
fn auto_hide_drawer_masks_pane_clicks() {
    let is_pane_focus = |actions: &[ClientShellAction]| {
        actions.iter().any(|action| {
            matches!(
                action,
                ClientShellAction::Endpoint { request, .. }
                    if matches!(request.method, crate::api::schema::Method::PaneFocus(_))
            )
        })
    };

    // Positive control: with the drawer closed the same click starts a selection.
    let mut control = auto_hide_state();
    control.compose(106, 30).expect("docked frame");
    let inner = control.hits.panes[0].inner_rect;
    let down = |inner: Rect| {
        mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            inner.x + 2,
            inner.y + 1,
        )
    };
    control.handle_raw_events(vec![down(inner)]);
    assert!(control.selection.is_some());

    let mut state = auto_hide_state();
    state.compose(106, 30).expect("docked frame");
    let inner = state.hits.panes[0].inner_rect;
    state.handle_raw_events(vec![moved(1, 5)]);
    state.compose(106, 30).expect("drawer frame");
    assert!(super::contains(
        state.hits.sidebar_overlay,
        (inner.x + 2, inner.y + 1)
    ));

    let outcome = state.handle_raw_events(vec![down(inner)]);
    assert!(state.selection.is_none());
    assert!(!is_pane_focus(&outcome.actions));
    assert_eq!(state.hits.panes.len(), 1);
}

#[test]
fn auto_hide_drawer_width_drag_repaints_without_resize() {
    let mut state = auto_hide_state();
    state.compose(106, 30).expect("docked frame");
    state.handle_raw_events(vec![moved(1, 5)]);
    state.compose(106, 30).expect("drawer frame");
    let divider = state.hits.sidebar_divider;
    let row = divider.y + 2;

    state.handle_raw_events(vec![mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        divider.x,
        row,
    )]);
    let outcome = state.handle_raw_events(vec![mouse_event(
        MouseEventKind::Drag(MouseButton::Left),
        31,
        row,
    )]);
    assert_eq!(state.sidebar_width, 32);
    assert!(outcome.repaint);
    assert!(!outcome.resize);
    assert!(state.sidebar_hover_reveal);

    state.handle_raw_events(vec![mouse_event(
        MouseEventKind::Up(MouseButton::Left),
        31,
        row,
    )]);
    state.handle_raw_events(vec![moved(60, 5)]);
    assert!(!state.sidebar_hover_reveal);
}

#[test]
fn auto_hide_reveal_cleared_when_pinned() {
    let mut state = auto_hide_state();
    state.compose(106, 30).expect("docked frame");
    state.handle_raw_events(vec![moved(1, 5)]);
    assert!(state.sidebar_hover_reveal);

    press_toggle(&mut state);
    assert!(!state.sidebar_hover_reveal);
    assert!(state.sidebar_auto_hide_pinned);
}

#[test]
fn settings_sidebar_section_reflects_auto_hide_config() {
    let mut on = auto_hide_state();
    on.open_settings_overlay();
    on.select_settings_section(
        ClientSettingsSection::Sidebar,
        &mut ClientShellInput::default(),
    );
    assert!(matches!(
        on.overlay,
        Some(ClientShellOverlay::Settings(ClientSettingsOverlay {
            selected: 0,
            ..
        }))
    ));
    let frame = on.compose(106, 30).expect("settings frame");
    assert!(frame_rows(&frame).join("\n").contains("auto-hide sidebar"));

    let mut off = docked_state(false);
    off.open_settings_overlay();
    off.select_settings_section(
        ClientSettingsSection::Sidebar,
        &mut ClientShellInput::default(),
    );
    assert!(matches!(
        off.overlay,
        Some(ClientShellOverlay::Settings(ClientSettingsOverlay {
            selected: 1,
            ..
        }))
    ));
}
