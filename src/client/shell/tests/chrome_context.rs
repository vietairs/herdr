use super::*;

#[test]
fn tab_overflow_controls_scroll_the_client_owned_tab_bar() {
    let mut snapshot = snapshot();
    snapshot.tabs.extend((2..=8).map(|number| ClientShellTab {
        tab_id: format!("tab_{number}"),
        workspace_id: "ws_1".into(),
        number,
        label: number.to_string(),
        custom_label: false,
        zoomed: false,
        focused: false,
        agent_status: AgentStatus::Idle,
    }));
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot));
    state.set_pane_surface(surface());
    state.compose(80, 20).expect("overflow tab bar");

    assert!(state.hits.tab_scroll_right.width > 0);
    let scroll_right = state.hits.tab_scroll_right;
    let outcome =
        state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: scroll_right.x + 1,
            row: scroll_right.y,
            modifiers: KeyModifiers::empty(),
        })]);
    assert!(outcome.repaint);
    assert_eq!(state.tab_scroll, 1);

    let mut update = state.snapshot.as_deref().expect("snapshot").clone();
    update.focused_tab_id = Some("tab_8".into());
    for tab in &mut update.tabs {
        tab.focused = tab.tab_id == "tab_8";
    }
    state.set_snapshot(Box::new(update));
    state.compose(80, 20).expect("focused overflow tab");
    assert!(state.hits.tabs.iter().any(|(_, tab_id)| tab_id == "tab_8"));

    state.compose(300, 20).expect("tabs without overflow");
    assert_eq!(state.tab_scroll, 0);
    assert_eq!(state.hits.tabs.len(), 8);
    state.compose(80, 20).expect("focused tab after narrowing");
    assert!(state.hits.tabs.iter().any(|(_, tab_id)| tab_id == "tab_8"));
}

#[test]
fn focused_workspace_change_reveals_new_workspace_in_full_sidebar() {
    let mut initial = snapshot();
    let template = initial.workspaces[0].clone();
    initial.workspaces = (1..=12)
        .map(|number| ClientShellWorkspace {
            workspace_id: format!("ws_{number}"),
            number,
            label: format!("space-{number}"),
            branch: None,
            focused: number == 1,
            ..template.clone()
        })
        .collect();

    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(initial));
    state.set_pane_surface(surface());
    state.compose(106, 20).expect("full sidebar");
    assert!(state.hits.workspace_max_scroll > 0);
    assert!(state
        .hits
        .workspaces
        .iter()
        .all(|hit| hit.workspace_id != "ws_12"));

    let mut update = state.snapshot.as_deref().expect("snapshot").clone();
    update.revision = 2;
    update.focused_workspace_id = Some("ws_12".into());
    for workspace in &mut update.workspaces {
        workspace.focused = workspace.workspace_id == "ws_12";
    }
    let mut updated_surface = surface();
    updated_surface.projection_revision = 2;
    state.set_snapshot(Box::new(update));
    state.set_pane_surface(updated_surface);
    state.compose(106, 2).expect("zero-height workspace body");
    assert!(state.reveal_focused_workspace);
    state.compose(106, 20).expect("updated full sidebar");

    assert!(state
        .hits
        .workspaces
        .iter()
        .any(|hit| hit.workspace_id == "ws_12"));
}

#[test]
fn client_owned_sidebar_dividers_resize_live() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    state.compose(106, 30).expect("expanded sidebar");
    let workspace_body = state.hits.workspace_body;
    let needless_scroll =
        state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: workspace_body.x,
            row: workspace_body.y,
            modifiers: KeyModifiers::empty(),
        })]);
    assert_eq!(state.hits.workspace_max_scroll, 0);
    assert_eq!(state.workspace_scroll, 0);
    assert!(!needless_scroll.repaint);
    let width_divider = state.hits.sidebar_divider;
    state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: width_divider.x,
        row: width_divider.y + 2,
        modifiers: KeyModifiers::empty(),
    })]);
    let resize =
        state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 31,
            row: width_divider.y + 2,
            modifiers: KeyModifiers::empty(),
        })]);
    assert_eq!(state.sidebar_width, 32);
    assert!(state.sidebar_width_manual);
    assert!(resize.repaint);
    assert!(resize.resize);
    state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Up(MouseButton::Left),
        column: 31,
        row: width_divider.y + 2,
        modifiers: KeyModifiers::empty(),
    })]);

    state.set_pane_surface(surface());
    state.compose(106, 30).expect("resized sidebar");
    let section_divider = state.hits.sidebar_section_divider;
    state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: section_divider.x + 2,
        row: section_divider.y,
        modifiers: KeyModifiers::empty(),
    })]);
    let split = state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Drag(MouseButton::Left),
        column: section_divider.x + 2,
        row: 20,
        modifiers: KeyModifiers::empty(),
    })]);
    assert!(state.sidebar_section_split > 0.6);
    assert!(split.repaint);
    assert!(!split.resize);
}

#[test]
fn context_menus_capture_stable_targets_and_route_actions() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    state.compose(106, 20).expect("composed frame");

    let workspace = state.hits.workspaces[0].rect;
    let open_workspace_menu =
        state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Right),
            column: workspace.x + 2,
            row: workspace.y,
            modifiers: KeyModifiers::empty(),
        })]);
    assert!(open_workspace_menu.actions.is_empty());
    assert!(matches!(
        state.overlay,
        Some(ClientShellOverlay::ContextMenu(ClientContextMenuOverlay {
            target: ClientContextMenuTarget::Workspace { ref workspace_id, .. },
            ..
        })) if workspace_id == "ws_1"
    ));
    let workspace_items = match state.overlay.as_ref() {
        Some(ClientShellOverlay::ContextMenu(menu)) => menu.items(),
        _ => panic!("workspace context menu"),
    };
    assert!(workspace_items
        .iter()
        .any(|item| item.action == ClientContextMenuAction::NewWorktree));
    state.compose(106, 20).expect("workspace context menu");
    let rename = state.hits.context_menu_rows[0].0;
    state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: rename.x + 1,
        row: rename.y,
        modifiers: KeyModifiers::empty(),
    })]);
    assert!(matches!(
        state.overlay,
        Some(ClientShellOverlay::Rename(ClientRenameOverlay {
            target: ClientRenameTarget::Workspace { ref workspace_id },
            ..
        })) if workspace_id == "ws_1"
    ));

    state.overlay = None;
    state.compose(106, 20).expect("composed frame");
    let pane = state.hits.panes[0].rect;
    state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Right),
        column: pane.x + 1,
        row: pane.y,
        modifiers: KeyModifiers::empty(),
    })]);
    state.compose(106, 20).expect("pane context menu");
    let split_index = match state.overlay.as_ref() {
        Some(ClientShellOverlay::ContextMenu(menu)) => menu
            .items()
            .iter()
            .position(|item| item.action == ClientContextMenuAction::SplitRight)
            .expect("split right item"),
        _ => panic!("pane context menu"),
    };
    let split = state.hits.context_menu_rows[split_index].0;
    let outcome =
        state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: split.x + 1,
            row: split.y,
            modifiers: KeyModifiers::empty(),
        })]);
    let [ClientShellAction::Endpoint { request, .. }] = &outcome.actions[..] else {
        panic!("pane split context action should use endpoint API");
    };
    assert!(matches!(
        &request.method,
        crate::api::schema::Method::PaneSplit(params)
            if params.target_pane_id.as_deref() == Some("pane_1")
                && params.direction == crate::api::schema::SplitDirection::Right
    ));
}

#[test]
fn global_menu_opens_from_sidebar_and_routes_client_actions() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    state.compose(106, 30).expect("shell frame");
    let launcher = state.hits.global_launcher;
    assert_ne!(launcher, Rect::default());

    let open = state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: launcher.x,
        row: launcher.y,
        modifiers: KeyModifiers::empty(),
    })]);
    assert!(open.repaint);
    let menu = state.compose(106, 30).expect("global menu");
    let text = menu
        .cells
        .chunks(menu.width as usize)
        .map(|row| {
            row.iter()
                .map(|cell| cell.symbol.as_str())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("settings"));
    assert!(text.contains("keybinds"));
    assert!(text.contains("reload config"));
    assert!(text.contains("detach"));

    let keybinds = state.hits.global_menu_rows[1].0;
    let help = state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: keybinds.x,
        row: keybinds.y,
        modifiers: KeyModifiers::empty(),
    })]);
    assert!(help.actions.is_empty());
    assert!(matches!(state.overlay, Some(ClientShellOverlay::Help(_))));

    state.overlay = Some(ClientShellOverlay::GlobalMenu(ClientGlobalMenuOverlay {
        highlighted: 3,
    }));
    let detach = state.handle_input_bytes(b"\r");
    assert!(detach.detach);
    assert!(state.overlay.is_none());
}

#[test]
fn new_tab_overlay_owns_text_cursor_and_submits_public_api_request() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    let mut open = ClientShellInput::default();
    state.record_binding(
        crate::input::KeybindMatch::Action(crate::input::KeybindAction::NewTab),
        &mut open,
    );
    assert!(open.actions.is_empty());
    let frame = state.compose(106, 20).expect("new tab overlay");
    let text = frame
        .cells
        .chunks(frame.width as usize)
        .map(|row| {
            row.iter()
                .map(|cell| cell.symbol.as_str())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("new tab"));
    assert!(text.contains("save"));
    let restored = frame.to_ratatui_buffer().expect("overlay frame");
    assert!(!restored
        .cell((26, 7))
        .expect("overlay title cell")
        .modifier
        .contains(Modifier::DIM));
    assert!(frame.cursor.as_ref().is_some_and(|cursor| cursor.visible));

    assert!(state.handle_input_bytes(b"logs").actions.is_empty());
    let create = state.handle_input_bytes(b"\r");
    let [ClientShellAction::Endpoint { request, .. }] = &create.actions[..] else {
        panic!("new tab save should use endpoint API");
    };
    assert!(matches!(
        &request.method,
        crate::api::schema::Method::TabCreate(params)
            if params.workspace_id.as_deref() == Some("ws_1")
                && params.label.as_deref() == Some("logs")
    ));
    assert!(state.overlay.is_none());
}

#[test]
fn close_confirmation_error_becomes_client_owned_overlay_and_stable_group_close() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    let mut close = ClientShellInput::default();
    state.record_binding(
        crate::input::KeybindMatch::Action(crate::input::KeybindAction::ClosePane),
        &mut close,
    );
    let [ClientShellAction::Endpoint { request, .. }] = &close.actions[..] else {
        panic!("pane close should use endpoint API");
    };
    let request_id = request.id.clone();
    assert!(
        state
            .handle_endpoint_result(
                "boot-1",
                &request_id,
                Err(ClientShellEndpointError {
                    code: Some("confirmation_required".into()),
                    message: "confirmation required".into(),
                }),
            )
            .0
    );
    let frame = state.compose(106, 20).expect("confirmation overlay");
    let text = frame
        .cells
        .chunks(frame.width as usize)
        .map(|row| {
            row.iter()
                .map(|cell| cell.symbol.as_str())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("Close workspace?"));
    assert!(text.contains("1 pane"));

    let confirm = state.handle_input_bytes(b"\r");
    let [ClientShellAction::Endpoint { request, .. }] = &confirm.actions[..] else {
        panic!("confirmation should use endpoint API");
    };
    assert!(matches!(
        &request.method,
        crate::api::schema::Method::WorkspaceClose(params)
            if params.workspace_id == "ws_1" && params.close_group
    ));
}

/// Builds a snapshot whose single workspace/tab is federation-mounted from
/// `origin`, using a remote-shaped `workspace_id` so the tests below can tell
/// apart "the client trusted the server's origin field" from "the client
/// re-parsed the id".
fn federated_snapshot(origin: Option<&str>, remote_shaped_id: bool) -> ClientShellSnapshot {
    let mut snapshot = snapshot();
    if remote_shaped_id {
        let workspace_id = "r:host-key-1:1".to_string();
        snapshot.focused_workspace_id = Some(workspace_id.clone());
        snapshot.workspaces[0].workspace_id = workspace_id.clone();
        snapshot.tabs[0].workspace_id = workspace_id.clone();
        snapshot.panes[0].workspace_id = workspace_id;
    }
    snapshot.workspaces[0].federation_origin = origin.map(str::to_owned);
    snapshot
}

fn workspace_menu_labels(state: &ClientShellState) -> Vec<&'static str> {
    match state.overlay.as_ref() {
        Some(ClientShellOverlay::ContextMenu(menu)) => {
            menu.items().iter().map(|item| item.label).collect()
        }
        _ => panic!("context menu overlay"),
    }
}

/// The federated-target gate reads the server-populated `federation_origin`,
/// never the shape of `workspace_id`. A remote-looking id with no origin must
/// not surface the action, and a local-looking id the server marked federated
/// must.
#[test]
fn close_on_host_follows_the_server_origin_field_not_the_workspace_id_shape() {
    let open = |snapshot: ClientShellSnapshot| {
        let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
        let workspace_id = snapshot.workspaces[0].workspace_id.clone();
        state.set_snapshot(Box::new(snapshot));
        state.open_workspace_context_menu(workspace_id, 0, 0);
        workspace_menu_labels(&state)
    };

    // Remote-shaped id, but the server did not mark it federated.
    assert!(
        !open(federated_snapshot(None, true)).contains(&"Close on host"),
        "a remote-shaped workspace_id must not by itself unlock close-on-host"
    );
    // Local-shaped id the server did mark federated.
    assert!(
        open(federated_snapshot(Some("dev@10.0.0.5"), false)).contains(&"Close on host"),
        "the server's federation_origin must unlock close-on-host"
    );
    assert!(!open(snapshot()).contains(&"Close on host"));
}

/// Menu rows are activated by index, so a conditional item must never displace
/// an unconditional one. Close-on-host is appended last for exactly that
/// reason.
#[test]
fn close_on_host_appends_last_and_never_shifts_the_items_above_it() {
    let labels_for = |origin: Option<&str>| {
        let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
        let snapshot = federated_snapshot(origin, true);
        let workspace_id = snapshot.workspaces[0].workspace_id.clone();
        state.set_snapshot(Box::new(snapshot));
        state.open_workspace_context_menu(workspace_id, 0, 0);
        workspace_menu_labels(&state)
    };

    let local = labels_for(None);
    let federated = labels_for(Some("dev@10.0.0.5"));
    assert_eq!(
        federated.len(),
        local.len() + 1,
        "close-on-host should add exactly one row"
    );
    assert_eq!(
        &federated[..local.len()],
        &local[..],
        "close-on-host must not reorder the rows above it"
    );
    assert_eq!(federated.last().copied(), Some("Close on host"));
}

#[test]
fn close_on_host_asks_the_serving_host_to_close_its_own_workspace() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    let snapshot = federated_snapshot(Some("dev@10.0.0.5"), true);
    let workspace_id = snapshot.workspaces[0].workspace_id.clone();
    state.set_snapshot(Box::new(snapshot));
    state.open_workspace_context_menu(workspace_id.clone(), 0, 0);
    let index = workspace_menu_labels(&state)
        .iter()
        .position(|label| *label == "Close on host")
        .expect("close on host item");

    let mut outcome = ClientShellInput::default();
    state.activate_context_menu_item(index, &mut outcome);

    let [ClientShellAction::Endpoint { request, .. }] = &outcome.actions[..] else {
        panic!("close on host should use the endpoint API");
    };
    assert!(
        matches!(
            &request.method,
            crate::api::schema::Method::WorkspaceCloseRemote(target)
                if target.workspace_id == workspace_id
        ),
        "expected workspace.close_remote for the menu target, got {:?}",
        request.method
    );
}

/// A tab carries no origin of its own, so its close-on-host gate must resolve
/// through the workspace it belongs to.
#[test]
fn tab_close_on_host_resolves_through_the_owning_workspace_origin() {
    let open = |origin: Option<&str>| {
        let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
        let snapshot = federated_snapshot(origin, true);
        state.set_snapshot(Box::new(snapshot));
        state.open_tab_context_menu("tab_1".into(), 0, 0);
        state
    };

    assert!(!workspace_menu_labels(&open(None)).contains(&"Close on host"));

    let mut state = open(Some("dev@10.0.0.5"));
    let labels = workspace_menu_labels(&state);
    assert_eq!(labels.last().copied(), Some("Close on host"));
    let index = labels.len() - 1;

    let mut outcome = ClientShellInput::default();
    state.activate_context_menu_item(index, &mut outcome);

    let close_remote = outcome.actions.iter().any(|action| {
        matches!(
            action,
            ClientShellAction::Endpoint { request, .. }
                if matches!(
                    &request.method,
                    crate::api::schema::Method::TabCloseRemote(target) if target.tab_id == "tab_1"
                )
        )
    });
    assert!(
        close_remote,
        "expected tab.close_remote for the menu target"
    );
}

/// Exercises the row the user actually clicks, not just `items()`, so an
/// index/label drift between the menu model and the rendered rows fails here.
#[test]
fn balance_splits_row_rebalances_the_clicked_panes_tab() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    state.compose(106, 20).expect("composed frame");

    let pane = state.hits.panes[0].rect;
    state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Right),
        column: pane.x + 1,
        row: pane.y,
        modifiers: KeyModifiers::empty(),
    })]);
    state.compose(106, 20).expect("pane context menu");

    let balance_index = match state.overlay.as_ref() {
        Some(ClientShellOverlay::ContextMenu(menu)) => menu
            .items()
            .iter()
            .position(|item| item.action == ClientContextMenuAction::BalanceSplits)
            .expect("balance splits item"),
        _ => panic!("pane context menu"),
    };
    let row = state.hits.context_menu_rows[balance_index].0;
    let outcome =
        state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: row.x + 1,
            row: row.y,
            modifiers: KeyModifiers::empty(),
        })]);

    let [ClientShellAction::Endpoint { request, .. }] = &outcome.actions[..] else {
        panic!("balance splits should use the endpoint API");
    };
    assert!(
        matches!(
            &request.method,
            crate::api::schema::Method::LayoutBalance(params)
                if params.pane_id.as_deref() == Some("pane_1") && params.tab_id.is_none()
        ),
        "expected layout.balance scoped to the clicked pane, got {:?}",
        request.method
    );
}
