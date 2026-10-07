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

fn reveal_drawer(state: &mut ClientShellState) {
    state.compose(106, 30).expect("docked frame");
    state.handle_raw_events(vec![moved(1, 5)]);
    state.compose(106, 30).expect("drawer frame");
    assert_eq!(state.hits.sidebar_overlay.width, state.sidebar_width);
}

#[test]
fn auto_hide_pointer_on_strip_under_an_overlay_does_not_reveal() {
    let mut state = auto_hide_state();
    state.compose(106, 30).expect("docked frame");
    state.open_settings_overlay();

    state.handle_raw_events(vec![moved(1, 5)]);
    assert!(!state.sidebar_hover_reveal);

    state.overlay = None;
    assert!(!state.sidebar_overlay_visible());
}

#[test]
fn auto_hide_overlay_opened_over_the_drawer_drops_the_reveal() {
    let mut state = auto_hide_state();
    reveal_drawer(&mut state);

    state.open_settings_overlay();
    state.handle_raw_events(Vec::new());
    assert!(!state.sidebar_hover_reveal);

    state.overlay = None;
    assert!(!state.sidebar_overlay_visible());
}

#[test]
fn auto_hide_focus_loss_drops_the_reveal() {
    let mut state = auto_hide_state();
    reveal_drawer(&mut state);

    let outcome = state.handle_raw_events(vec![RawInputEvent::OuterFocusLost]);
    assert!(!state.sidebar_hover_reveal);
    assert!(outcome.repaint);
    assert!(!outcome.resize);
}

#[test]
fn auto_hide_drawer_stays_under_a_workspace_menu_opened_from_it() {
    let mut state = auto_hide_state();
    reveal_drawer(&mut state);
    let workspace = state.hits.workspaces[0].rect;

    state.handle_raw_events(vec![mouse_event(
        MouseEventKind::Down(MouseButton::Right),
        workspace.x + 2,
        workspace.y,
    )]);
    assert!(matches!(
        state.overlay,
        Some(ClientShellOverlay::ContextMenu(_))
    ));
    state.compose(106, 30).expect("menu frame");
    assert_eq!(state.hits.sidebar_overlay.width, state.sidebar_width);

    // Reaching a menu row that extends past the drawer edge keeps the drawer.
    let outside = state.hits.sidebar_overlay.right() + 2;
    state.handle_raw_events(vec![moved(outside, workspace.y + 1)]);
    assert!(state.sidebar_hover_reveal);
    state.compose(106, 30).expect("menu frame");
    assert_eq!(state.hits.sidebar_overlay.width, state.sidebar_width);
}

#[test]
fn auto_hide_global_menu_opens_beside_the_drawer_launcher() {
    let mut state = auto_hide_state();
    reveal_drawer(&mut state);
    let launcher = state.hits.global_launcher;
    assert!(launcher.width > 0);

    state.handle_raw_events(vec![mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        launcher.x,
        launcher.y,
    )]);
    assert!(matches!(
        state.overlay,
        Some(ClientShellOverlay::GlobalMenu(_))
    ));
    state.compose(106, 30).expect("menu frame");
    assert_eq!(state.hits.sidebar_overlay.width, state.sidebar_width);
    let (last_row, _) = *state.hits.global_menu_rows.last().expect("menu rows");
    assert_eq!(last_row.bottom() + 1, launcher.y);
    assert!(last_row.x < launcher.right());
}

#[test]
fn collapsed_sidebar_anchors_the_global_menu_at_the_bottom_left() {
    for mode in [
        crate::config::SidebarCollapsedModeConfig::Compact,
        crate::config::SidebarCollapsedModeConfig::Hidden,
    ] {
        let mut config = Config::default();
        config.ui.sidebar_auto_hide = true;
        config.ui.sidebar_collapsed_mode = mode;
        let mut state = ClientShellState::new(ClientShellConfig::from_config(&config));
        state.set_snapshot(Box::new(snapshot()));
        state.set_pane_surface(surface());
        state.compose(106, 30).expect("docked frame");
        let launcher = state.hits.global_launcher;
        assert_eq!(launcher.y, 29);
        assert!(!super::contains(launcher, (launcher.x, launcher.y)));

        state.toggle_global_menu();
        state.compose(106, 30).expect("menu frame");
        let (last_row, _) = *state.hits.global_menu_rows.last().expect("menu rows");
        assert_eq!(last_row.bottom() + 1, 29);
        assert_eq!(last_row.x, 1);
    }
}

#[test]
fn auto_hide_drag_started_in_the_drawer_keeps_it_open_until_release() {
    let mut state = auto_hide_state();
    reveal_drawer(&mut state);
    let workspace = state.hits.workspaces[0].rect;
    let outside = state.hits.sidebar_overlay.right() + 4;

    state.handle_raw_events(vec![mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        workspace.x + 2,
        workspace.y,
    )]);
    state.handle_raw_events(vec![mouse_event(
        MouseEventKind::Drag(MouseButton::Left),
        outside,
        workspace.y + 1,
    )]);
    assert!(state.sidebar_hover_reveal);
    state.compose(106, 30).expect("drag frame");
    assert_eq!(state.hits.sidebar_overlay.width, state.sidebar_width);

    state.handle_raw_events(vec![mouse_event(
        MouseEventKind::Up(MouseButton::Left),
        outside,
        workspace.y + 1,
    )]);
    assert!(!state.sidebar_hover_reveal);
}

#[test]
fn auto_hide_drawer_collapse_button_hides_without_docking() {
    let mut state = auto_hide_state();
    reveal_drawer(&mut state);
    let toggle = state.hits.sidebar_toggle;
    assert!(super::contains(
        state.hits.sidebar_overlay,
        (toggle.x, toggle.y)
    ));

    let outcome = state.handle_raw_events(vec![mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        toggle.x,
        toggle.y,
    )]);
    assert!(!state.sidebar_hover_reveal);
    assert!(!state.sidebar_auto_hide_pinned);
    assert!(outcome.repaint);
    assert!(!outcome.resize);
}

#[test]
fn auto_hide_drawer_leaves_the_tab_bar_in_place() {
    let mut state = auto_hide_state();
    let mut snapshot = snapshot();
    let first = snapshot.tabs[0].clone();
    for number in 2..=8 {
        let mut tab = first.clone();
        tab.tab_id = format!("tab_{number}");
        tab.number = number;
        tab.label = number.to_string();
        tab.focused = false;
        snapshot.tabs.push(tab);
    }
    state.set_snapshot(Box::new(snapshot));
    state.compose(106, 30).expect("docked frame");
    let docked_tabs = state.hits.tabs.clone();
    let docked_width = state.last_tab_bar_width;

    state.handle_raw_events(vec![moved(1, 5)]);
    state.compose(106, 30).expect("drawer frame");
    let edge = state.hits.sidebar_overlay.right();
    assert!(edge > 0);
    assert_eq!(state.last_tab_bar_width, docked_width);
    assert!(state.hits.tabs.iter().all(|(rect, _)| rect.x >= edge));
    let uncovered = docked_tabs
        .iter()
        .filter(|(rect, _)| rect.x >= edge)
        .collect::<Vec<_>>();
    assert!(!uncovered.is_empty());
    for (rect, tab_id) in uncovered {
        assert!(state.hits.tabs.contains(&(*rect, tab_id.clone())));
    }
    assert!(state.hits.tabs.len() < docked_tabs.len());
}

#[test]
fn settings_auto_hide_off_then_on_drops_the_session_pin() {
    let _guard = crate::config::test_config_env_lock().lock().unwrap();
    let path = std::env::temp_dir().join(format!(
        "herdr-sidebar-auto-hide-setting-{}.toml",
        std::process::id()
    ));
    std::fs::write(&path, "[ui]\nsidebar_auto_hide = true\n").unwrap();
    std::env::set_var(crate::config::CONFIG_PATH_ENV_VAR, &path);

    let mut state = auto_hide_state();
    press_toggle(&mut state);
    assert!(state.sidebar_auto_hide_pinned);
    state.open_settings_overlay();
    state.select_settings_section(
        ClientSettingsSection::Sidebar,
        &mut ClientShellInput::default(),
    );
    for (selected, enabled) in [(1, false), (0, true)] {
        if let Some(ClientShellOverlay::Settings(settings)) = state.overlay.as_mut() {
            settings.selected = selected;
        }
        let mut outcome = ClientShellInput::default();
        state.apply_settings_choice(&mut outcome);
        assert_eq!(state.config.sidebar_auto_hide, enabled);
        assert!(outcome.resize);
    }

    std::env::remove_var(crate::config::CONFIG_PATH_ENV_VAR);
    let _ = std::fs::remove_file(&path);
    assert!(!state.sidebar_auto_hide_pinned);
    assert!(state.sidebar_layout_collapsed());
}

#[test]
fn auto_hide_drawer_blanks_the_tab_bar_cells_it_covers() {
    let mut state = auto_hide_state();
    let mut snapshot = snapshot();
    let first = snapshot.tabs[0].clone();
    for number in 2..=8 {
        let mut tab = first.clone();
        tab.tab_id = format!("tab_{number}");
        tab.number = number;
        tab.label = format!("tablabel{number}");
        tab.focused = false;
        snapshot.tabs.push(tab);
    }
    state.set_snapshot(Box::new(snapshot));
    state.compose(106, 30).expect("docked frame");
    let tab_row = state.hits.tabs[0].0.y;

    state.handle_raw_events(vec![moved(1, 5)]);
    let frame = state.compose(106, 30).expect("drawer frame");
    let overlay = state.hits.sidebar_overlay;
    assert!(super::contains(overlay, (overlay.x, tab_row)));
    let row = &frame_rows(&frame)[tab_row as usize];
    let drawer: String = row
        .chars()
        .skip(overlay.x as usize)
        .take(overlay.width as usize)
        .collect();
    let header = drawer.trim_end_matches('│').trim_end();
    assert_eq!(header.trim(), "spaces", "drawer row: {drawer:?}");
}
