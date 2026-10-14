//! Server tests for `[server] client_size_policy`.
//!
//! The shared test server pins `latest` so upstream tests keep their "latest
//! client wins" expectations; every test here sets the policy it exercises.

use super::*;

use crate::config::ClientSizePolicy;
use crate::server::federation_actor::FederationCommand;

const LARGE: (u16, u16) = (100, 35);
const SMALL: (u16, u16) = (68, 17);
/// PTY `(rows, cols)` for the small surface: one column goes to the scrollbar
/// gutter on the main screen.
const SMALL_PTY: (u16, u16) = (17, 67);
const LARGE_PTY: (u16, u16) = (35, 99);

fn smallest_server() -> HeadlessServer {
    let mut server = test_headless_server();
    server.app.state.client_size_policy = ClientSizePolicy::Smallest;
    server
}

fn shared_pty_size(server: &HeadlessServer, pane_id: crate::layout::PaneId) -> (u16, u16) {
    server.app.state.workspaces[0].test_runtimes[&pane_id].current_size()
}

fn connect_sized_shell(
    server: &mut HeadlessServer,
    client_id: u64,
    (cols, rows): (u16, u16),
) -> (
    std::sync::mpsc::Receiver<Vec<u8>>,
    std::sync::mpsc::Receiver<Vec<u8>>,
) {
    let (control, render) = connect_test_shell(server, client_id, cols, rows);
    let _ = control.recv().expect("client shell snapshot");
    (control, render)
}

fn resize_shell(server: &mut HeadlessServer, client_id: u64, (cols, rows): (u16, u16)) {
    assert!(server.handle_server_event(ServerEvent::ClientShellResize {
        client_id,
        surface_cols: cols,
        surface_rows: rows,
        cell_width_px: 0,
        cell_height_px: 0,
        pixel_mouse: false,
    }));
}

fn activate_surface(server: &mut HeadlessServer, client_id: u64, request_id: &str) {
    let boot_id = server.client_shell_boot_id.clone();
    assert!(
        server.handle_server_event(ServerEvent::ClientShellEndpointRequest {
            client_id,
            boot_id,
            request: Box::new(api::schema::Request {
                id: request_id.into(),
                method: api::schema::Method::ClientShellSurfaceSet(
                    api::schema::ClientShellSurfaceSetParams { active: true },
                ),
            }),
        })
    );
}

fn shell_api_request(server: &mut HeadlessServer, client_id: u64, method: api::schema::Method) {
    let (respond_to, _response_rx) = std::sync::mpsc::channel();
    server.handle_client_shell_api_request(
        client_id,
        api::ApiRequestMessage {
            request: api::schema::Request {
                id: "size-policy-request".into(),
                method,
            },
            respond_to,
            response_write_complete: None,
        },
    );
}

/// Expected PTY `(rows, cols)` of every pane laid out in `area`, read from the
/// same layout the clients draw.
fn pane_sizes_in(
    server: &HeadlessServer,
    (cols, rows): (u16, u16),
) -> Vec<(crate::layout::PaneId, (u16, u16))> {
    crate::ui::compute_tab_surface_for(
        &server.app.state,
        &server.app.terminal_runtimes,
        server.default_shell_target(),
        Rect::new(0, 0, cols, rows),
        false,
        crate::kitty_graphics::HostCellSize::default(),
    )
    .pane_infos
    .into_iter()
    .map(|pane| (pane.id, (pane.inner_rect.height, pane.inner_rect.width)))
    .collect()
}

#[test]
fn production_default_client_size_policy_is_smallest() {
    let config = crate::config::Config::default();
    let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
    let app = crate::app::App::new(
        &config,
        crate::app::AppPolicy::TEST,
        None,
        api_rx,
        api::EventHub::default(),
    );
    assert_eq!(app.state.client_size_policy, ClientSizePolicy::Smallest);
    assert_eq!(
        test_headless_server().client_size_policy(),
        ClientSizePolicy::Latest,
        "the shared test server pins upstream's latest-wins rule"
    );
}

#[tokio::test]
async fn smallest_sizes_a_shared_tab_to_the_smaller_viewer_whoever_claims_it() {
    for (first, second) in [(LARGE, SMALL), (SMALL, LARGE)] {
        let mut server = smallest_server();
        let pane_id = install_shared_view_test_runtime(&mut server);
        let _first = connect_sized_shell(&mut server, 7, first);
        let _second = connect_sized_shell(&mut server, 8, second);
        assert_eq!(
            shared_pty_size(&server, pane_id),
            SMALL_PTY,
            "after connect"
        );

        for client_id in [7, 8, 7] {
            for other in [7, 8] {
                server.handle_server_event(ServerEvent::ClientShellFocus {
                    client_id: other,
                    focused: other == client_id,
                });
            }
            assert_eq!(shared_pty_size(&server, pane_id), SMALL_PTY, "after focus");
            // Typing claims the tab the same way.
            server.claim_shell_tab_geometry(client_id, false);
            assert_eq!(shared_pty_size(&server, pane_id), SMALL_PTY, "after claim");
            let tab_id = server.shell_tab_id_for_client(client_id).expect("tab");
            assert_eq!(
                server.tab_geometry_controllers.get(&tab_id),
                Some(&client_id)
            );
        }
        shutdown_test_runtimes(&mut server);
    }
}

#[tokio::test]
async fn smallest_grows_the_tab_when_the_small_viewer_detaches() {
    let mut server = smallest_server();
    let pane_id = install_shared_view_test_runtime(&mut server);
    let _large = connect_sized_shell(&mut server, 7, LARGE);
    let _small = connect_sized_shell(&mut server, 8, SMALL);
    server.claim_shell_tab_geometry(7, false);
    assert_eq!(shared_pty_size(&server, pane_id), SMALL_PTY);

    assert!(server.handle_server_event(ServerEvent::ClientDisconnected { client_id: 8 }));

    assert_eq!(shared_pty_size(&server, pane_id), LARGE_PTY);
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn smallest_grows_the_tab_when_the_small_viewer_switches_tab() {
    let mut server = smallest_server();
    let pane_id = install_shared_view_test_runtime(&mut server);
    let second_tab = server.app.state.workspaces[0].test_add_tab(Some("second"));
    let second_pane = server.app.state.workspaces[0].tabs[second_tab].root_pane;
    server.app.state.workspaces[0].insert_test_runtime(
        second_pane,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 23, b""),
    );
    let second_tab_id = server.app.public_tab_id(0, second_tab).expect("tab id");
    let _large = connect_sized_shell(&mut server, 7, LARGE);
    let _small = connect_sized_shell(&mut server, 8, SMALL);
    server.claim_shell_tab_geometry(7, false);
    assert_eq!(shared_pty_size(&server, pane_id), SMALL_PTY);

    shell_api_request(
        &mut server,
        8,
        api::schema::Method::TabFocus(api::schema::TabTarget {
            tab_id: second_tab_id,
        }),
    );

    assert_eq!(
        shared_pty_size(&server, pane_id),
        LARGE_PTY,
        "the tab the small viewer left is sized by the remaining viewer"
    );
    assert_eq!(
        server.app.state.workspaces[0].test_runtimes[&second_pane].current_size(),
        SMALL_PTY
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn smallest_ignores_a_resize_storm_from_the_larger_viewer() {
    let mut server = smallest_server();
    let pane_id = install_shared_view_test_runtime(&mut server);
    let _large = connect_sized_shell(&mut server, 7, LARGE);
    let _small = connect_sized_shell(&mut server, 8, SMALL);
    server.claim_shell_tab_geometry(7, false);

    for step in 0..40_u16 {
        resize_shell(&mut server, 7, (LARGE.0 + step % 7, LARGE.1 + step % 5));
        assert_eq!(shared_pty_size(&server, pane_id), SMALL_PTY);
    }
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn smallest_lets_a_viewer_that_is_not_the_controller_shrink_the_tab() {
    let mut server = smallest_server();
    let pane_id = install_shared_view_test_runtime(&mut server);
    let _large = connect_sized_shell(&mut server, 7, LARGE);
    let _other = connect_sized_shell(&mut server, 8, LARGE);
    server.claim_shell_tab_geometry(7, false);
    assert_eq!(shared_pty_size(&server, pane_id), LARGE_PTY);
    let tab_id = server.shell_tab_id_for_client(7).expect("tab");

    resize_shell(&mut server, 8, SMALL);

    assert_eq!(server.tab_geometry_controllers.get(&tab_id), Some(&7));
    assert_eq!(shared_pty_size(&server, pane_id), SMALL_PTY);
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn smallest_pads_the_larger_frame_and_crops_no_pty_cell() {
    let mut server = smallest_server();
    let pane_id = install_shared_view_test_runtime(&mut server);
    let (_large_control, large_render) = connect_sized_shell(&mut server, 7, LARGE);
    let (_small_control, small_render) = connect_sized_shell(&mut server, 8, SMALL);
    server.claim_shell_tab_geometry(7, false);
    // Mark the PTY's last column and last row, wherever they are.
    write_shared_test_pane(&mut server, pane_id, b"\x1b[2J\x1b[1;999HE\x1b[999;1HL");

    server.render_and_stream();
    let large = recv_pane_surface(&large_render, "large surface");
    let small = recv_pane_surface(&small_render, "small surface");

    let (pty_rows, pty_cols) = shared_pty_size(&server, pane_id);
    for (surface, name) in [(&large, "large"), (&small, "small")] {
        let inner = surface.panes[0].inner_rect;
        assert!(
            inner.width >= pty_cols && inner.height >= pty_rows,
            "{name}"
        );
        let cell = |x: u16, y: u16| {
            surface.frame.cells[usize::from(inner.y + y) * usize::from(surface.frame.width)
                + usize::from(inner.x + x)]
            .symbol
            .as_str()
        };
        assert_eq!(cell(pty_cols - 1, 0), "E", "{name}: last PTY column kept");
        assert_eq!(cell(0, pty_rows - 1), "L", "{name}: last PTY row kept");
        for x in pty_cols..inner.width {
            assert_eq!(cell(x, 0).trim(), "", "{name}: column {x} is padding");
        }
        for y in pty_rows..inner.height {
            assert_eq!(cell(0, y).trim(), "", "{name}: row {y} is padding");
        }
    }
    assert!(large.panes[0].inner_rect.width > pty_cols);
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn smallest_lays_out_zoom_split_ratio_and_popup_in_the_minimum() {
    let mut server = smallest_server();
    let mut workspace = crate::workspace::Workspace::test_new("size-policy-layout");
    let first_pane = workspace.tabs[0].root_pane;
    let second_pane = workspace.test_split(ratatui::layout::Direction::Horizontal);
    for pane_id in [first_pane, second_pane] {
        workspace.insert_test_runtime(
            pane_id,
            crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b""),
        );
    }
    server.app.state.workspaces = vec![workspace];
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;
    let tab_id = server.app.public_tab_id(0, 0).expect("tab id");
    let popup_runtime = crate::terminal::TerminalRuntime::test_with_screen_bytes(40, 12, b"");
    let (_, popup_id) = server.app.install_test_popup_runtime(popup_runtime);
    server.popup_owner_tab_id = Some(tab_id.clone());
    let _large = connect_sized_shell(&mut server, 7, LARGE);
    let _small = connect_sized_shell(&mut server, 8, SMALL);
    server.claim_shell_tab_geometry(7, false);

    let assert_laid_out_in_minimum = |server: &HeadlessServer, context: &str| {
        for (pane_id, expected) in pane_sizes_in(server, SMALL) {
            assert_eq!(
                server.app.state.workspaces[0].test_runtimes[&pane_id].current_size(),
                expected,
                "{context}: pane {pane_id:?}"
            );
        }
        let popup = server.app.state.popup_pane.as_ref().expect("popup");
        let geometry = crate::popup_size::resolve_popup_geometry(
            popup.width,
            popup.height,
            Rect::new(0, 0, SMALL.0, SMALL.1),
        )
        .expect("popup geometry");
        assert_eq!(
            server
                .app
                .terminal_runtimes
                .get(&popup_id)
                .expect("popup runtime")
                .current_size(),
            (geometry.inner.height, geometry.inner.width),
            "{context}: popup"
        );
    };
    assert_laid_out_in_minimum(&server, "initial split");

    shell_api_request(
        &mut server,
        7,
        api::schema::Method::LayoutSetSplitRatio(api::schema::LayoutSetSplitRatioParams {
            tab_id: Some(tab_id),
            pane_id: None,
            path: Vec::new(),
            ratio: 0.8,
        }),
    );
    assert_laid_out_in_minimum(&server, "split ratio");

    shell_api_request(
        &mut server,
        7,
        api::schema::Method::PaneZoom(api::schema::PaneZoomParams {
            pane_id: None,
            mode: api::schema::PaneZoomMode::On,
        }),
    );
    assert!(server.app.state.workspaces[0].tabs[0].zoomed);
    assert_laid_out_in_minimum(&server, "zoomed");

    shell_api_request(
        &mut server,
        7,
        api::schema::Method::PaneZoom(api::schema::PaneZoomParams {
            pane_id: None,
            mode: api::schema::PaneZoomMode::Off,
        }),
    );
    assert_laid_out_in_minimum(&server, "unzoomed");
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn smallest_alternate_screen_switch_keeps_the_minimum() {
    let mut server = smallest_server();
    let pane_id = install_shared_view_test_runtime(&mut server);
    let (_large_control, large_render) = connect_sized_shell(&mut server, 7, LARGE);
    let (_small_control, small_render) = connect_sized_shell(&mut server, 8, SMALL);
    server.claim_shell_tab_geometry(7, false);
    server.render_and_stream();
    let _ = recv_pane_surface(&large_render, "large initial");
    let _ = recv_pane_surface(&small_render, "small initial");

    write_shared_test_pane(&mut server, pane_id, b"\x1b[?1049hALT");
    server.render_and_stream();

    // The alternate screen drops the scrollbar gutter, so the PTY gains the
    // small viewer's last column, not the controller's width.
    assert_eq!(
        shared_pty_size(&server, pane_id),
        (SMALL_PTY.0, SMALL_PTY.1 + 1)
    );
    shutdown_test_runtimes(&mut server);
}

// Smallest counterparts of the upstream tests that pin "latest client wins".

#[tokio::test]
async fn smallest_background_surface_activation_bounds_the_focused_viewers_tab() {
    for (focused, background, expected) in [(SMALL, LARGE, SMALL_PTY), (LARGE, SMALL, SMALL_PTY)] {
        let mut server = smallest_server();
        let pane_id = install_shared_view_test_runtime(&mut server);
        let _focused = connect_sized_shell(&mut server, 7, focused);
        assert!(server.handle_server_event(ServerEvent::ClientShellFocus {
            client_id: 7,
            focused: true,
        }));
        let shared_tab_id = server.shell_tab_id_for_client(7).expect("focused tab");

        let (writer, background_control, _) = test_client_writer();
        assert!(
            server.handle_server_event(ServerEvent::ClientShellConnected {
                surface_reuse: false,
                surface_delta: false,
                surface_scroll: false,
                client_id: 8,
                surface_cols: background.0,
                surface_rows: background.1,
                cell_width_px: 0,
                cell_height_px: 0,
                pixel_mouse: false,
                direct_graphics: false,
                endpoint_keybindings: false,
                mouse_capture: false,
                surface_active: false,
                writer,
            })
        );
        let _ = background_control.recv().expect("background snapshot");
        activate_surface(&mut server, 8, "activate-background-surface");

        // The focused viewer keeps the controller role either way; the
        // background viewer still bounds the size when it is the smaller one.
        assert_eq!(shared_pty_size(&server, pane_id), expected);
        assert_eq!(
            server.tab_geometry_controllers.get(&shared_tab_id),
            Some(&7)
        );
        shutdown_test_runtimes(&mut server);
    }
}

#[tokio::test]
async fn smallest_focused_surface_reassertion_keeps_the_smaller_viewer_bound() {
    let mut server = smallest_server();
    let pane_id = install_shared_view_test_runtime(&mut server);
    let _focused = connect_sized_shell(&mut server, 8, LARGE);
    assert!(server.handle_server_event(ServerEvent::ClientShellFocus {
        client_id: 8,
        focused: true,
    }));
    let shared_tab_id = server.shell_tab_id_for_client(8).expect("focused tab");
    let _other = connect_sized_shell(&mut server, 7, SMALL);
    server.claim_shell_tab_geometry(7, false);
    assert_eq!(shared_pty_size(&server, pane_id), SMALL_PTY);

    activate_surface(&mut server, 8, "reassert-focused-surface");

    // Reasserting takes the controller role, but the smaller viewer still
    // bounds the size instead of being cropped to the focused client.
    assert_eq!(
        server.tab_geometry_controllers.get(&shared_tab_id),
        Some(&8)
    );
    assert_eq!(shared_pty_size(&server, pane_id), SMALL_PTY);
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn smallest_geometry_reapply_replaces_a_controller_that_left_the_tab() {
    let mut server = smallest_server();
    let mut workspace = crate::workspace::Workspace::test_new("smallest-controller-viewer");
    let first_pane = workspace.tabs[0].root_pane;
    let second_tab = workspace.test_add_tab(Some("second"));
    let second_pane = workspace.tabs[second_tab].root_pane;
    let third_tab = workspace.test_add_tab(Some("third"));
    let third_pane = workspace.tabs[third_tab].root_pane;
    for pane_id in [first_pane, second_pane, third_pane] {
        workspace.insert_test_runtime(
            pane_id,
            crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b""),
        );
    }
    server.app.state.workspaces = vec![workspace];
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;
    let second_tab_id = server.app.public_tab_id(0, second_tab).unwrap();
    let third_tab_id = server.app.public_tab_id(0, third_tab).unwrap();
    let _first = connect_sized_shell(&mut server, 67, (100, 30));
    let _second = connect_sized_shell(&mut server, 68, (70, 20));

    assert!(server.focus_shell_client_on_tab(67, &second_tab_id));
    assert!(server.claim_shell_tab_geometry(67, false));
    assert!(server.focus_shell_client_on_tab(67, &third_tab_id));
    assert!(server.claim_shell_tab_geometry(67, false));
    assert!(server.focus_shell_client_on_tab(68, &second_tab_id));
    let stale_size = server.app.state.workspaces[0].test_runtimes[&second_pane].current_size();

    assert!(server.reapply_controlled_shell_tab_geometry(false));

    assert_eq!(
        server.tab_geometry_controllers.get(&second_tab_id),
        Some(&68)
    );
    let resized = server.app.state.workspaces[0].test_runtimes[&second_pane].current_size();
    assert_ne!(resized, stale_size);
    assert_eq!(resized, (20, 69));
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn reloading_into_latest_hands_the_tab_back_to_its_controller() {
    let mut server = smallest_server();
    let pane_id = install_shared_view_test_runtime(&mut server);
    let _large = connect_sized_shell(&mut server, 7, LARGE);
    let _small = connect_sized_shell(&mut server, 8, SMALL);
    server.claim_shell_tab_geometry(7, false);
    assert_eq!(shared_pty_size(&server, pane_id), SMALL_PTY);

    server.app.state.client_size_policy = ClientSizePolicy::Latest;
    server.apply_reloaded_client_size_policy();

    assert_eq!(shared_pty_size(&server, pane_id), LARGE_PTY);
    shutdown_test_runtimes(&mut server);
}

// Federation: the mount's size is one more per-terminal vote.

struct MountedTerminal {
    pane_id: crate::layout::PaneId,
    terminal_id: String,
    epoch: crate::server::federation_lease::AcceptEpoch,
}

const MOUNT_CONNID: crate::server::federation_lease::ConnId = 1;

fn mount_shared_terminal(server: &mut HeadlessServer) -> MountedTerminal {
    let pane_id = install_shared_view_test_runtime(server);
    server.app.state.ensure_test_terminals();
    let terminal_id = server.app.state.workspaces[0]
        .terminal_id(pane_id)
        .expect("terminal id")
        .to_string();
    let epoch = server.federation_lease.current_epoch();
    assert_eq!(
        server.federation_lease.try_acquire(epoch, MOUNT_CONNID),
        crate::server::federation_lease::Admission::Accepted
    );
    assert!(server.federation_lease.try_mount(epoch, MOUNT_CONNID));
    MountedTerminal {
        pane_id,
        terminal_id,
        epoch,
    }
}

fn mount_resize(server: &mut HeadlessServer, mounted: &MountedTerminal, (cols, rows): (u16, u16)) {
    server.handle_server_event(ServerEvent::Federation(FederationCommand::Resize {
        epoch: mounted.epoch,
        connid: MOUNT_CONNID,
        terminal_id: mounted.terminal_id.clone(),
        cols,
        rows,
    }));
}

#[tokio::test]
async fn smallest_a_larger_mount_cannot_crop_the_host_viewer() {
    let mut server = smallest_server();
    let mounted = mount_shared_terminal(&mut server);
    let _host = connect_sized_shell(&mut server, 7, SMALL);
    assert_eq!(shared_pty_size(&server, mounted.pane_id), SMALL_PTY);

    mount_resize(&mut server, &mounted, (120, 40));
    assert_eq!(shared_pty_size(&server, mounted.pane_id), SMALL_PTY);

    // Typing on the host keeps it bounded by the host viewer as well.
    server.claim_shell_tab_geometry(7, false);
    resize_shell(&mut server, 7, SMALL);
    assert_eq!(shared_pty_size(&server, mounted.pane_id), SMALL_PTY);
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn smallest_a_smaller_mount_bounds_the_host_viewer() {
    let mut server = smallest_server();
    let mounted = mount_shared_terminal(&mut server);
    let _host = connect_sized_shell(&mut server, 7, LARGE);
    assert_eq!(shared_pty_size(&server, mounted.pane_id), LARGE_PTY);

    mount_resize(&mut server, &mounted, (50, 20));
    assert_eq!(shared_pty_size(&server, mounted.pane_id), (20, 50));

    // A host resize re-lays the tab out but keeps the mount's bound.
    resize_shell(&mut server, 7, (110, 38));
    assert_eq!(shared_pty_size(&server, mounted.pane_id), (20, 50));
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn smallest_the_mount_vote_applies_once_the_host_viewer_leaves() {
    let mut server = smallest_server();
    let mounted = mount_shared_terminal(&mut server);
    let _host = connect_sized_shell(&mut server, 7, SMALL);
    mount_resize(&mut server, &mounted, (120, 40));
    assert_eq!(shared_pty_size(&server, mounted.pane_id), SMALL_PTY);

    assert!(server.handle_server_event(ServerEvent::ClientDisconnected { client_id: 7 }));

    assert_eq!(shared_pty_size(&server, mounted.pane_id), (40, 120));
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn smallest_a_mount_redraw_without_a_size_stays_exclusive() {
    let mut server = smallest_server();
    let mounted = mount_shared_terminal(&mut server);
    let _host = connect_sized_shell(&mut server, 7, LARGE);
    assert_eq!(shared_pty_size(&server, mounted.pane_id), LARGE_PTY);

    server.handle_server_event(ServerEvent::Federation(FederationCommand::NudgeRedraw {
        epoch: mounted.epoch,
        connid: MOUNT_CONNID,
        terminal_id: mounted.terminal_id.clone(),
    }));
    assert!(server
        .app
        .state
        .federation_owned_terminal_sizes
        .values()
        .all(Option::is_none));

    resize_shell(&mut server, 7, SMALL);
    assert_eq!(
        shared_pty_size(&server, mounted.pane_id),
        LARGE_PTY,
        "no size from the mount yet, so the host does not take the terminal"
    );

    // A later redraw keeps a size the mount already sent.
    mount_resize(&mut server, &mounted, (50, 20));
    server.handle_server_event(ServerEvent::Federation(FederationCommand::NudgeRedraw {
        epoch: mounted.epoch,
        connid: MOUNT_CONNID,
        terminal_id: mounted.terminal_id.clone(),
    }));
    assert!(server
        .app
        .state
        .federation_owned_terminal_sizes
        .values()
        .all(|vote| *vote == Some((50, 20))));
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn smallest_releasing_the_mount_vote_resizes_from_the_host_viewer() {
    let mut server = smallest_server();
    let mounted = mount_shared_terminal(&mut server);
    // A second mounted terminal keeps voting, so the release itself (not the
    // whole mount going away) must hand the first terminal back.
    let other_pane =
        server.app.state.workspaces[0].test_split(ratatui::layout::Direction::Vertical);
    server.app.state.workspaces[0].insert_test_runtime(
        other_pane,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 23, b""),
    );
    server.app.state.ensure_test_terminals();
    let other_terminal_id = server.app.state.workspaces[0]
        .terminal_id(other_pane)
        .expect("terminal id")
        .to_string();
    let _host = connect_sized_shell(&mut server, 7, LARGE);
    let host_sizes = pane_sizes_in(&server, LARGE)
        .into_iter()
        .collect::<std::collections::HashMap<_, _>>();
    mount_resize(&mut server, &mounted, (50, 8));
    mount_resize(
        &mut server,
        &MountedTerminal {
            pane_id: other_pane,
            terminal_id: other_terminal_id,
            epoch: mounted.epoch,
        },
        (50, 8),
    );
    assert_eq!(shared_pty_size(&server, mounted.pane_id), (8, 50));
    assert_eq!(shared_pty_size(&server, other_pane), (8, 50));

    server.handle_server_event(ServerEvent::Federation(
        FederationCommand::ReleaseTerminalSize {
            epoch: mounted.epoch,
            connid: MOUNT_CONNID,
            terminal_id: mounted.terminal_id.clone(),
        },
    ));

    assert_eq!(server.app.state.federation_owned_terminal_sizes.len(), 1);
    assert_eq!(
        shared_pty_size(&server, mounted.pane_id),
        host_sizes[&mounted.pane_id]
    );
    assert_eq!(shared_pty_size(&server, other_pane), (8, 50));
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn latest_keeps_the_mount_exclusive() {
    let mut server = test_headless_server();
    assert_eq!(server.client_size_policy(), ClientSizePolicy::Latest);
    let mounted = mount_shared_terminal(&mut server);
    let _host = connect_sized_shell(&mut server, 7, SMALL);
    assert_eq!(shared_pty_size(&server, mounted.pane_id), SMALL_PTY);

    mount_resize(&mut server, &mounted, (120, 40));
    assert_eq!(shared_pty_size(&server, mounted.pane_id), (40, 120));

    server.claim_shell_tab_geometry(7, false);
    resize_shell(&mut server, 7, (60, 15));
    assert_eq!(
        shared_pty_size(&server, mounted.pane_id),
        (40, 120),
        "under latest the host viewer cannot take a mounted terminal back"
    );
    shutdown_test_runtimes(&mut server);
}
