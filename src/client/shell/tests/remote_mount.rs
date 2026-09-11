use super::*;

fn state_with_mount_overlay(recents: Vec<&str>) -> ClientShellState {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    let mut projection = snapshot();
    projection.recent_remote_mount_targets = recents.into_iter().map(str::to_owned).collect();
    state.set_snapshot(Box::new(projection));
    state.set_pane_surface(surface());
    state.open_remote_mount_overlay();
    state
}

fn mount_remote_params(
    actions: &[ClientShellAction],
) -> &crate::api::schema::WorkspaceMountRemoteParams {
    let [ClientShellAction::Endpoint { request, .. }] = actions else {
        panic!("expected exactly one endpoint request, got {actions:?}");
    };
    match &request.method {
        crate::api::schema::Method::WorkspaceMountRemote(params) => params,
        other => panic!("expected WorkspaceMountRemote, got {other:?}"),
    }
}

fn frame_text(frame: &FrameData) -> String {
    frame
        .cells
        .iter()
        .map(|cell| cell.symbol.as_str())
        .collect()
}

// A hand-mutated `parse_remote_mount_targets` that returned `Ok(vec![])`
// instead of erroring on blank input was used to confirm this test actually
// exercises the guard: it failed as expected (no error, and the mutated
// build sends an endpoint request for zero targets).
#[test]
fn blank_input_is_rejected_with_inline_error_and_sends_nothing() {
    let mut state = state_with_mount_overlay(vec![]);
    let mut outcome = ClientShellInput::default();

    state.submit_remote_mount(&mut outcome);

    assert!(outcome.actions.is_empty(), "no request should be sent");
    assert!(matches!(
        &state.overlay,
        Some(ClientShellOverlay::MountRemote(overlay)) if overlay.error.is_some()
    ));
}

// Mutation-checked: temporarily rejecting a leading `-` or "localhost"
// client-side (mirroring the server's own `validate_remote_target`/
// `is_local_target` rules) made this test fail, confirming it would catch
// the client re-implementing target validation instead of deferring to the
// server.
#[test]
fn option_like_and_localhost_targets_are_forwarded_unmodified() {
    let mut state = state_with_mount_overlay(vec![]);
    state.insert_remote_mount_overlay_text("-oProxyCommand=x localhost");
    let mut outcome = ClientShellInput::default();

    state.submit_remote_mount(&mut outcome);

    let params = mount_remote_params(&outcome.actions);
    assert_eq!(
        params.targets,
        vec!["-oProxyCommand=x".to_string(), "localhost".to_string()]
    );
    assert!(!params.remote_keybindings);
}

#[test]
fn multiple_whitespace_separated_targets_reach_one_request() {
    let mut state = state_with_mount_overlay(vec![]);
    state.insert_remote_mount_overlay_text("host-a   host-b");
    let mut outcome = ClientShellInput::default();

    state.submit_remote_mount(&mut outcome);

    assert_eq!(outcome.actions.len(), 1, "one request carries every target");
    let params = mount_remote_params(&outcome.actions);
    assert_eq!(
        params.targets,
        vec!["host-a".to_string(), "host-b".to_string()]
    );
}

// Mutation-checked: swapping the render's `if error.is_some() { .. } else if
// dialling { .. }` for an `else if` gated the other way (dialling shadowing
// the failure) made this test fail, confirming it actually exercises note 2
// from the rebuild spec rather than passing by accident.
#[test]
fn failed_attempts_error_renders_even_while_a_sibling_is_still_dialling() {
    let mut state = state_with_mount_overlay(vec![]);
    let mut projection = (**state.snapshot.as_ref().unwrap()).clone();
    projection.remote_mount_attempts = vec![
        crate::protocol::ClientShellRemoteMountAttempt {
            target: "host-a".into(),
            mounted: false,
            mounted_workspace_id: None,
            error: None,
        },
        crate::protocol::ClientShellRemoteMountAttempt {
            target: "host-b".into(),
            mounted: false,
            mounted_workspace_id: None,
            error: Some("connection refused".into()),
        },
    ];
    state.set_snapshot(Box::new(projection));

    let frame = state.compose(100, 24).expect("mount overlay frame");
    let text = frame_text(&frame);

    assert!(
        text.contains("host-b: connection refused"),
        "expected the failed target's error to be visible: {text}"
    );
}

/// A mount can succeed against a host that exposes no workspaces, leaving it
/// with no workspace id to report. That state must still read as finished:
/// inferring "still dialling" from the absence of an id would leave the
/// dialog claiming to be mounting something that already finished.
#[test]
fn a_mount_that_exposed_no_workspaces_is_not_reported_as_still_dialling() {
    let mut state = state_with_mount_overlay(vec![]);
    let mut projection = (**state.snapshot.as_ref().unwrap()).clone();
    projection.remote_mount_attempts = vec![crate::protocol::ClientShellRemoteMountAttempt {
        target: "host-a".into(),
        mounted: true,
        mounted_workspace_id: None,
        error: None,
    }];
    state.set_snapshot(Box::new(projection));

    let frame = state.compose(100, 24).expect("mount overlay frame");
    let text = frame_text(&frame);

    assert!(
        !text.contains("mounting"),
        "a finished mount must not still read as dialling: {text}"
    );
}

#[test]
fn up_from_a_freshly_opened_dialog_selects_the_most_recent_target_first() {
    let mut state = state_with_mount_overlay(vec!["host-b", "host-a"]);
    assert!(matches!(
        &state.overlay,
        Some(ClientShellOverlay::MountRemote(overlay)) if overlay.recents_highlighted.is_none()
    ));

    state.move_remote_mount_recent_selection(-1);

    let Some(ClientShellOverlay::MountRemote(overlay)) = &state.overlay else {
        panic!("mount overlay");
    };
    assert_eq!(overlay.recents_highlighted, Some(0));
    assert_eq!(overlay.input, "host-b");
}

#[test]
fn down_key_navigates_recents_and_stops_at_the_last_entry() {
    let mut state = state_with_mount_overlay(vec!["host-b", "host-a"]);

    state.move_remote_mount_recent_selection(1);
    assert!(matches!(
        &state.overlay,
        Some(ClientShellOverlay::MountRemote(overlay))
            if overlay.recents_highlighted == Some(0) && overlay.input == "host-b"
    ));

    state.move_remote_mount_recent_selection(1);
    assert!(matches!(
        &state.overlay,
        Some(ClientShellOverlay::MountRemote(overlay))
            if overlay.recents_highlighted == Some(1) && overlay.input == "host-a"
    ));

    // Clamps at the last entry rather than wrapping or going out of range.
    state.move_remote_mount_recent_selection(1);
    assert!(matches!(
        &state.overlay,
        Some(ClientShellOverlay::MountRemote(overlay))
            if overlay.recents_highlighted == Some(1) && overlay.input == "host-a"
    ));
}

// Mutation-checked: deriving the visible row budget from the stored recents
// count instead of the actually-drawn (possibly clamped) list rect made this
// test fail with an unchanged row count at a short terminal height,
// confirming it catches the exact regression the rebuild spec calls out
// (note 1): a clamped popup must shrink the recents list rather than paint
// it over the buttons.
#[test]
fn visible_recents_row_count_shrinks_at_a_clamped_terminal_height() {
    let recents = vec!["host-a", "host-b", "host-c", "host-d", "host-e"];
    let mut tall = state_with_mount_overlay(recents.clone());
    let tall_frame = tall.compose(100, 40).expect("tall mount overlay frame");
    let tall_rows = tall.hits.remote_mount_recents.len();
    assert_eq!(tall_rows, recents.len(), "a tall screen fits every recent");
    drop(tall_frame);

    let mut short = state_with_mount_overlay(recents.clone());
    short.compose(100, 14).expect("clamped mount overlay frame");
    let short_rows = short.hits.remote_mount_recents.len();

    assert!(
        short_rows < tall_rows,
        "a clamped popup must show fewer recents rows ({short_rows} >= {tall_rows})"
    );
}

// Mutation-checked: removing the "mount remote workspace…" entry from
// `global_menu_items` (or wiring it to a no-op) made this test fail,
// confirming it actually opens the overlay rather than passing regardless.
#[test]
fn global_menu_entry_opens_the_mount_remote_overlay() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    state.toggle_global_menu();

    let index = super::super::global_menu::global_menu_items(state.snapshot.as_deref().unwrap())
        .iter()
        .position(|(_, action)| {
            *action == super::super::global_menu::ClientGlobalMenuAction::MountRemote
        })
        .expect("mount remote entry is present");

    let mut outcome = ClientShellInput::default();
    state.activate_global_menu_item(index, &mut outcome);

    assert!(matches!(
        &state.overlay,
        Some(ClientShellOverlay::MountRemote(_))
    ));
}

// Regression guard for the mount button doing nothing at all: the dialog was
// rebuilt onto the client-shell endpoint lane, but `workspace.mount_remote`
// was never added to that lane's advertised method list, so every submit was
// dropped client-side before a request was ever queued. Binding the fixture
// to the server's real list (rather than a hand-written one) is the point --
// a hand-written list would have kept passing through the outage.
// Mutation-checked: removing "workspace.mount_remote" from
// `CLIENT_SHELL_METHODS` makes this test fail.
#[test]
fn the_real_server_method_list_admits_a_mount_submit() {
    let mut state = state_with_mount_overlay(vec![]);
    state.set_endpoint_methods(Some(
        crate::server::client_commands::supported_client_shell_method_names()
            .iter()
            .map(|method| (*method).to_owned())
            .collect(),
    ));
    state.insert_remote_mount_overlay_text("alice@host-b");
    let mut outcome = ClientShellInput::default();

    state.submit_remote_mount(&mut outcome);

    assert_eq!(
        mount_remote_params(&outcome.actions).targets,
        vec!["alice@host-b".to_string()],
        "a server advertising its real method list must accept a mount submit"
    );
}

// A server that does not advertise the method reports it through an endpoint
// notice, which this modal draws over -- so the rejection also has to land in
// the dialog's own error line or the button looks inert.
// Mutation-checked: dropping the `supports_endpoint_method` guard from
// `submit_remote_mount` makes this test fail (no inline error is set).
#[test]
fn an_unsupported_server_rejects_the_submit_inline_instead_of_silently() {
    let mut state = state_with_mount_overlay(vec![]);
    state.set_endpoint_methods(Some(vec!["pane.focus".into()]));
    state.insert_remote_mount_overlay_text("alice@host-b");
    let mut outcome = ClientShellInput::default();

    state.submit_remote_mount(&mut outcome);

    assert!(outcome.actions.is_empty(), "no request should be sent");
    let Some(ClientShellOverlay::MountRemote(overlay)) = &state.overlay else {
        panic!("mount overlay should stay open");
    };
    let error = overlay.error.as_deref().expect("inline rejection");
    assert!(
        error.contains("does not support"),
        "rejection should say why, got {error:?}"
    );
}
