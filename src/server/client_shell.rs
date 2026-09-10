use ratatui::layout::Rect;

use crate::app;
use crate::protocol::{self, FrameData};

pub(super) fn snapshot(
    app: &app::App,
    boot_id: &str,
    revision: u64,
    config_diagnostic: Option<&str>,
    location: Option<&crate::server::clients::ClientShellLocation>,
) -> protocol::ClientShellSnapshot {
    let snapshot = app.session_snapshot();
    let focused_workspace_id = location
        .and_then(|location| location.focused_workspace_id.clone())
        .or_else(|| snapshot.focused_workspace_id.clone());
    let focused_tab_id = location
        .and_then(|location| location.focused_tab_id().map(str::to_owned))
        .or_else(|| snapshot.focused_tab_id.clone());
    let focused_pane_id = focused_tab_id
        .as_deref()
        .and_then(|tab_id| app.parse_tab_id(tab_id))
        .and_then(|(workspace_index, tab_index)| {
            let pane_id = app
                .state
                .workspaces
                .get(workspace_index)?
                .tabs
                .get(tab_index)?
                .layout
                .focused();
            app.public_pane_id(workspace_index, pane_id)
        })
        .or_else(|| snapshot.focused_pane_id.clone());
    let workspaces = snapshot
        .workspaces
        .into_iter()
        .zip(&app.state.workspaces)
        .enumerate()
        .map(|(workspace_index, (workspace, state))| {
            let mut tokens = workspace.tokens.into_iter().collect::<Vec<_>>();
            tokens.sort_by(|left, right| left.0.cmp(&right.0));
            let federation_origin = workspace.federation_origin.clone();
            let workspace_id = workspace.workspace_id;
            let active_tab_id = location
                .and_then(|location| location.active_tab_ids.get(&workspace_id))
                .cloned()
                .unwrap_or(workspace.active_tab_id);
            let active_tab_index =
                app.parse_tab_id(&active_tab_id)
                    .and_then(|(tab_workspace_index, tab_index)| {
                        (tab_workspace_index == workspace_index).then_some(tab_index)
                    });
            protocol::ClientShellWorkspace {
                focused: focused_workspace_id.as_deref() == Some(workspace_id.as_str()),
                workspace_id,
                active_tab_id,
                new_workspace_cwd: app
                    .resolved_new_workspace_cwd_from_tab(workspace_index, active_tab_index)
                    .display()
                    .to_string(),
                number: workspace.number,
                label: workspace.label,
                name_source: workspace.name_source,
                // Derived from the very rung that produced `label`, so the
                // deprecated flag keeps carrying exactly what it carried
                // before `name_source` existed. Reading `custom_name`
                // directly here would silently report `false` for a
                // federation-mounted scope, whose remote label now lives in
                // the mirror slot instead of the override slot — and an
                // older client that filters on this flag would drop the
                // mounted label entirely.
                custom_label: workspace.name_source.is_explicitly_named(),
                // A federated workspace has no local git repository to
                // probe: `state.branch()` reads locally-cached git metadata
                // that nothing populates for a federated workspace's
                // `identity_cwd` (meaningless remotely). Gate explicitly
                // rather than relying on that absence, so a future local
                // git-refresh pass can never accidentally start shelling out
                // against a federated workspace's local `identity_cwd`.
                branch: if federation_origin.is_some() {
                    None
                } else {
                    state.branch()
                },
                git_ahead_behind: state.git_ahead_behind(),
                tokens,
                worktree: workspace
                    .worktree
                    .map(|worktree| protocol::ClientShellWorktree {
                        key: worktree.repo_key,
                        label: worktree.repo_name,
                        is_linked_worktree: worktree.is_linked_worktree,
                    }),
                agent_status: workspace.agent_status,
                federation_origin,
            }
        })
        .collect();
    let tabs = snapshot
        .tabs
        .into_iter()
        .zip(
            app.state
                .workspaces
                .iter()
                .flat_map(|workspace| workspace.tabs.iter()),
        )
        .map(|(tab, state)| {
            let tab_id = tab.tab_id;
            protocol::ClientShellTab {
                focused: focused_tab_id.as_deref() == Some(tab_id.as_str()),
                tab_id,
                workspace_id: tab.workspace_id,
                number: tab.number,
                label: tab.label,
                name_source: tab.name_source,
                // Same derivation as the workspace above, and for the same
                // reason: a mounted tab's label lives in the mirror slot,
                // so `is_auto_named()` would report it as auto.
                custom_label: tab.name_source.is_explicitly_named(),
                zoomed: state.zoomed,
                agent_status: tab.agent_status,
            }
        })
        .collect();
    let panes = snapshot
        .panes
        .into_iter()
        .map(|pane| {
            let pane_id = pane.pane_id;
            let focused = focused_pane_id.as_deref() == Some(pane_id.as_str());
            let right_click_passthrough = app
                .parse_pane_id(&pane_id)
                .and_then(|(workspace_index, pane_id)| {
                    app.state
                        .workspaces
                        .get(workspace_index)?
                        .pane_state(pane_id)
                })
                .is_some_and(|pane| pane.right_click_passthrough);
            protocol::ClientShellPane {
                pane_id,
                workspace_id: pane.workspace_id,
                tab_id: pane.tab_id,
                label: pane.label,
                name_source: pane.name_source,
                cwd: pane.cwd,
                foreground_cwd: pane.foreground_cwd,
                focused,
                right_click_passthrough,
            }
        })
        .collect();
    let agents = snapshot
        .agents
        .into_iter()
        .map(|agent| {
            let pane_id = agent.pane_id;
            let focused = focused_pane_id.as_deref() == Some(pane_id.as_str());
            let mut state_labels = agent.state_labels.into_iter().collect::<Vec<_>>();
            state_labels.sort_by(|left, right| left.0.cmp(&right.0));
            let mut tokens = agent.tokens.into_iter().collect::<Vec<_>>();
            tokens.sort_by(|left, right| left.0.cmp(&right.0));
            protocol::ClientShellAgent {
                pane_id,
                workspace_id: agent.workspace_id,
                tab_id: agent.tab_id,
                name: agent.name,
                label: agent.label,
                name_source: agent.name_source,
                display_agent: agent.display_agent,
                agent: agent.agent,
                title: agent.title,
                terminal_title: agent.terminal_title,
                terminal_title_stripped: agent.terminal_title_stripped,
                agent_status: agent.agent_status,
                state_change_seq: agent.state_change_seq,
                state_labels,
                tokens,
                focused,
            }
        })
        .collect();

    let agent_view_label = app
        .state
        .agent_view_override
        .as_ref()
        .map(|view| view.label.clone().unwrap_or_else(|| "filtered".to_owned()));
    let agent_order = crate::ui::agent_panel_entries_from(&app.state, &app.terminal_runtimes)
        .into_iter()
        .filter_map(|entry| app.public_pane_id(entry.ws_idx, entry.pane_id))
        .collect();

    let zoomed = focused_tab_id
        .as_deref()
        .and_then(|tab_id| app.parse_tab_id(tab_id))
        .and_then(|(workspace_index, tab_index)| {
            app.state
                .workspaces
                .get(workspace_index)?
                .tabs
                .get(tab_index)
        })
        .is_some_and(|tab| tab.zoomed);
    let tab_bar_right = app
        .state
        .tab_bar_right
        .iter()
        .filter_map(|segment| match segment {
            crate::app::state::TabBarStatusSegment::Zoom if zoomed => {
                Some(protocol::ClientShellTabStatusSegment {
                    text: "ZOOM".to_owned(),
                    accent: true,
                })
            }
            crate::app::state::TabBarStatusSegment::Text(Some(text)) if !text.is_empty() => {
                Some(protocol::ClientShellTabStatusSegment {
                    text: text.clone(),
                    accent: false,
                })
            }
            crate::app::state::TabBarStatusSegment::Zoom
            | crate::app::state::TabBarStatusSegment::Text(_) => None,
        })
        .collect();

    let product_announcement = app.state.product_announcement.as_ref().map(|announcement| {
        protocol::ClientShellProductAnnouncement {
            version: announcement.version.clone(),
            id: announcement.id.clone(),
            title: announcement.title.clone(),
            body: announcement.body.clone(),
            preview: announcement.preview,
        }
    });
    let release_notes =
        app.state
            .latest_release_notes
            .as_ref()
            .map(|notes| protocol::ClientShellReleaseNotes {
                version: notes.version.clone(),
                body: notes.body.clone(),
                preview: notes.preview,
            });

    let remote_mount_attempts = app
        .state
        .remote_mount_attempts
        .iter()
        .map(|attempt| {
            let (mounted, mounted_workspace_id, error) = match &attempt.outcome {
                app::state::RemoteMountOutcome::Dialling => (false, None, None),
                app::state::RemoteMountOutcome::Mounted { workspace_id } => {
                    (true, workspace_id.clone(), None)
                }
                app::state::RemoteMountOutcome::Failed { reason } => {
                    (false, None, Some(reason.clone()))
                }
            };
            protocol::ClientShellRemoteMountAttempt {
                target: attempt.target.clone(),
                mounted,
                mounted_workspace_id,
                error,
            }
        })
        .collect();

    protocol::ClientShellSnapshot {
        boot_id: boot_id.to_owned(),
        revision,
        config_diagnostic: config_diagnostic.map(str::to_owned),
        product_announcement,
        update_available: app.state.update_available.clone(),
        update_install_command: app.state.update_install_command.clone(),
        server_keybindings_toml: app.client_shell_keybindings_profile().map(str::to_owned),
        latest_release_notes_available: app.state.latest_release_notes_available,
        integration_updates_available: app.state.integration_updates_available(),
        worktree_directory: app.state.worktree_directory.to_string_lossy().into_owned(),
        auto_resize_splits: app.state.auto_resize_splits,
        release_notes,
        focused_workspace_id,
        focused_tab_id,
        focused_pane_id,
        tab_bar_right,
        tab_bar_right_separator: app.state.tab_bar_right_separator.clone(),
        agent_view_label,
        agent_order,
        workspaces,
        tabs,
        panes,
        agents,
        commands: app.client_shell_command_manifest(),
        remote_mount_attempts,
        recent_remote_mount_targets: app.state.recent_remote_mount_targets.clone(),
    }
}

pub(super) struct RenderedPaneSurface {
    pub(super) frame: FrameData,
    pub(super) panes: Vec<protocol::PaneSurfacePane>,
    pub(super) splits: Vec<protocol::PaneSurfaceSplit>,
    pub(super) popup: Option<Box<protocol::ClientShellPopupSurface>>,
    pub(super) graphics: protocol::SurfaceGraphicsScene,
    pub(super) graphics_delivery: crate::kitty_graphics::surface::DeliveryCache,
}

pub(super) fn render_pane_surface(
    app: &mut app::App,
    target: Option<crate::ui::TabSurfaceTarget>,
    area: Rect,
    resize_panes: bool,
    show_popup: bool,
    cell_size: crate::kitty_graphics::HostCellSize,
    graphics_delivery: &crate::kitty_graphics::surface::DeliveryCache,
    client_id: u64,
) -> RenderedPaneSurface {
    let content_revisions_before = target
        .and_then(|target| {
            let workspace = app.state.workspaces.get(target.workspace_index)?;
            let tab = workspace.tabs.get(target.tab_index)?;
            Some(
                tab.layout
                    .pane_ids()
                    .into_iter()
                    .filter_map(|pane_id| {
                        app.state
                            .runtime_for_pane_in_workspace(
                                &app.terminal_runtimes,
                                target.workspace_index,
                                pane_id,
                            )
                            .map(|runtime| (pane_id, runtime.content_seq()))
                    })
                    .collect::<std::collections::HashMap<_, _>>(),
            )
        })
        .unwrap_or_default();
    let (buffer, cursor, hyperlinks, layout) =
        crate::server::render_stream::render_tab_surface_virtual(
            &app.state,
            &app.terminal_runtimes,
            target,
            area,
            resize_panes,
            cell_size,
        );
    let panes = target
        .map(|target| {
            let workspace_index = target.workspace_index;
            layout
                .pane_infos
                .iter()
                .filter_map(|pane| {
                    app.public_pane_id(workspace_index, pane.id).map(|pane_id| {
                        let runtime = app.state.runtime_for_pane_in_workspace(
                            &app.terminal_runtimes,
                            workspace_index,
                            pane.id,
                        );
                        let mouse_reporting =
                            runtime.is_some_and(|runtime| runtime.mouse_reporting_enabled());
                        let sgr_pixel_mouse =
                            runtime.is_some_and(|runtime| runtime.sgr_pixel_mouse_enabled());
                        let (pixel_width, pixel_height) = if cell_size.is_known() {
                            (
                                u32::from(pane.inner_rect.width) * cell_size.width_px,
                                u32::from(pane.inner_rect.height) * cell_size.height_px,
                            )
                        } else {
                            (0, 0)
                        };
                        let content_revision = runtime.map_or(0, |runtime| {
                            let after = runtime.content_seq();
                            if content_revisions_before.get(&pane.id).copied() == Some(after)
                                && after.is_multiple_of(2)
                            {
                                after
                            } else {
                                after | 1
                            }
                        });
                        protocol::PaneSurfacePane {
                            pane_id,
                            content_revision,
                            rect: pane.rect.into(),
                            inner_rect: pane.inner_rect.into(),
                            scrollbar_rect: pane.scrollbar_rect.map(Into::into),
                            scroll: runtime.and_then(|runtime| runtime.scroll_metrics()).map(
                                |metrics| protocol::PaneSurfaceScrollMetrics {
                                    offset_from_bottom: metrics.offset_from_bottom as u64,
                                    max_offset_from_bottom: metrics.max_offset_from_bottom as u64,
                                    viewport_rows: metrics.viewport_rows as u64,
                                },
                            ),
                            focused: pane.is_focused,
                            mouse_reporting,
                            sgr_pixel_mouse,
                            alternate_screen_active: runtime
                                .is_some_and(|runtime| runtime.alternate_screen_active()),
                            pixel_width,
                            pixel_height,
                        }
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let pane_frames = layout
        .pane_infos
        .iter()
        .map(|pane| pane.rect)
        .collect::<Vec<_>>();
    let splits = layout
        .split_borders
        .iter()
        .filter_map(|split| {
            let hit_rect = split_hit_rect(
                split,
                app.state.pane_borders.draws_borders(),
                app.state.pane_gaps,
                &pane_frames,
            )?;
            let direction = match split.direction {
                ratatui::layout::Direction::Horizontal => {
                    protocol::PaneSurfaceSplitDirection::Horizontal
                }
                ratatui::layout::Direction::Vertical => {
                    protocol::PaneSurfaceSplitDirection::Vertical
                }
            };
            Some(protocol::PaneSurfaceSplit {
                direction,
                pos: split.pos,
                area: split.area.into(),
                hit_rect: hit_rect.into(),
                path: split.path.clone(),
            })
        })
        .collect();
    let popup = show_popup
        .then(|| render_popup_surface(app, area, resize_panes, cell_size))
        .flatten();
    let (graphics, next_graphics_delivery) = crate::server::client_shell_graphics::collect(
        app,
        &layout.pane_infos,
        &layout.split_borders,
        popup.as_deref(),
        target,
        cell_size,
        graphics_delivery,
        client_id,
    );
    RenderedPaneSurface {
        frame: FrameData::from_ratatui_buffer_with_hyperlinks(&buffer, cursor, &hyperlinks),
        panes,
        splits,
        popup,
        graphics,
        graphics_delivery: next_graphics_delivery,
    }
}

fn render_popup_surface(
    app: &app::App,
    area: Rect,
    resize_runtime: bool,
    cell_size: crate::kitty_graphics::HostCellSize,
) -> Option<Box<protocol::ClientShellPopupSurface>> {
    let popup = app.state.popup_pane.as_ref()?;
    let geometry = if resize_runtime {
        resize_popup_runtime(app, area, cell_size)?
    } else {
        crate::popup_size::resolve_popup_geometry(popup.width, popup.height, area)?
    };
    let runtime = app.terminal_runtimes.get(&popup.terminal_id)?;
    let content_area = Rect::new(0, 0, geometry.inner.width, geometry.inner.height);
    let (buffer, cursor) =
        crate::server::render_stream::render_terminal_virtual(runtime, content_area);
    let hyperlinks = runtime.visible_hyperlinks(content_area);
    // A popup pane belongs to no tab (`PopupPaneState` carries no tab
    // context), so it degrades to the pane-only ladder: its own override,
    // then its agent identity, then the literal "popup" — never a tab
    // inheritance lookup. See `src/workspace/naming.rs`'s module doc.
    let title = app
        .state
        .terminals
        .get(&popup.terminal_id)
        .and_then(|terminal| {
            terminal.border_label(app.state.show_agent_labels_on_pane_borders, None)
        })
        .unwrap_or_else(|| "popup".to_owned());
    let (pixel_width, pixel_height) = if cell_size.is_known() {
        (
            u32::from(content_area.width) * cell_size.width_px,
            u32::from(content_area.height) * cell_size.height_px,
        )
    } else {
        (0, 0)
    };
    Some(Box::new(protocol::ClientShellPopupSurface {
        terminal_id: popup.terminal_id.to_string(),
        title,
        width: popup.width.map(client_popup_size),
        height: popup.height.map(client_popup_size),
        frame: FrameData::from_ratatui_buffer_with_hyperlinks(&buffer, cursor, &hyperlinks),
        mouse_reporting: runtime.mouse_reporting_enabled(),
        sgr_pixel_mouse: runtime.sgr_pixel_mouse_enabled(),
        pixel_width,
        pixel_height,
    }))
}

pub(super) fn resize_popup_runtime(
    app: &app::App,
    area: Rect,
    cell_size: crate::kitty_graphics::HostCellSize,
) -> Option<crate::popup_size::PopupResolvedGeometry> {
    let popup = app.state.popup_pane.as_ref()?;
    let geometry = crate::popup_size::resolve_popup_geometry(popup.width, popup.height, area)?;
    let runtime = app.terminal_runtimes.get(&popup.terminal_id)?;
    if !app
        .state
        .direct_attach_resize_locks
        .contains(&popup.terminal_id)
    {
        runtime.resize(
            geometry.inner.height,
            geometry.inner.width,
            cell_size.width_px,
            cell_size.height_px,
        );
    }
    Some(geometry)
}

fn client_popup_size(size: crate::popup_size::PopupSize) -> protocol::ClientShellPopupSize {
    match size {
        crate::popup_size::PopupSize::Cells(cells) => protocol::ClientShellPopupSize::Cells(cells),
        crate::popup_size::PopupSize::Percent(percent) => {
            protocol::ClientShellPopupSize::Percent(percent)
        }
    }
}

fn split_hit_rect(
    split: &crate::layout::SplitBorder,
    pane_borders: bool,
    pane_gaps: bool,
    pane_frames: &[Rect],
) -> Option<Rect> {
    let hit = match (split.direction, pane_borders, pane_gaps) {
        (ratatui::layout::Direction::Horizontal, true, false) => {
            Rect::new(split.pos, split.area.y, 1, split.area.height)
        }
        (ratatui::layout::Direction::Horizontal, true, true) => {
            let start = split.pos.saturating_sub(1);
            Rect::new(
                start,
                split.area.y,
                split.pos.saturating_sub(start).saturating_add(1),
                split.area.height,
            )
        }
        (ratatui::layout::Direction::Horizontal, false, true) => Rect::new(
            split.pos.checked_sub(1)?,
            split.area.y,
            1,
            split.area.height,
        ),
        (ratatui::layout::Direction::Vertical, true, false) => {
            Rect::new(split.area.x, split.pos, split.area.width, 1)
        }
        (ratatui::layout::Direction::Vertical, true, true) => {
            let start = split.pos.saturating_sub(1);
            Rect::new(
                split.area.x,
                start,
                split.area.width,
                split.pos.saturating_sub(start).saturating_add(1),
            )
        }
        (ratatui::layout::Direction::Vertical, false, true) => {
            Rect::new(split.area.x, split.pos.checked_sub(1)?, split.area.width, 1)
        }
        (_, false, false) => return None,
    };
    if !pane_borders
        && pane_frames.iter().any(|pane| {
            hit.x < pane.right()
                && hit.right() > pane.x
                && hit.y < pane.bottom()
                && hit.bottom() > pane.y
        })
    {
        return None;
    }
    Some(hit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::Workspace;

    #[test]
    fn snapshot_projects_cached_release_and_update_facts() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = crate::app::App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.integration_recommendations.clear();
        app.state.update_available = Some("0.8.3".into());
        app.state.update_install_command = "herdr update".into();
        app.state.latest_release_notes_available = true;
        app.state.latest_release_notes = Some(crate::release_notes::ReleaseNotes {
            version: "0.8.3".into(),
            body: "### Changed\n- Client shell".into(),
            preview: true,
        });

        let snapshot = snapshot(&app, "boot", 7, None, None);

        assert_eq!(snapshot.update_available.as_deref(), Some("0.8.3"));
        assert_eq!(snapshot.update_install_command, "herdr update");
        assert!(snapshot.latest_release_notes_available);
        assert!(!snapshot.integration_updates_available);
        assert_eq!(
            snapshot.release_notes.as_ref().map(|notes| (
                notes.version.as_str(),
                notes.body.as_str(),
                notes.preview
            )),
            Some(("0.8.3", "### Changed\n- Client shell", true))
        );
    }

    #[test]
    fn snapshot_carries_remote_mount_attempts_and_recent_targets() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = crate::app::App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.remote_mount_attempts = vec![
            crate::app::state::RemoteMountAttempt {
                target: "host-a".into(),
                outcome: crate::app::state::RemoteMountOutcome::Dialling,
            },
            crate::app::state::RemoteMountAttempt {
                target: "host-b".into(),
                outcome: crate::app::state::RemoteMountOutcome::Mounted {
                    workspace_id: Some("w2".into()),
                },
            },
            crate::app::state::RemoteMountAttempt {
                target: "host-c".into(),
                outcome: crate::app::state::RemoteMountOutcome::Failed {
                    reason: "connection refused".into(),
                },
            },
        ];
        app.state.recent_remote_mount_targets = vec!["host-b".into(), "host-a".into()];

        let snapshot = snapshot(&app, "boot", 1, None, None);

        assert_eq!(
            snapshot.remote_mount_attempts,
            vec![
                crate::protocol::ClientShellRemoteMountAttempt {
                    target: "host-a".into(),
                    mounted: false,
                    mounted_workspace_id: None,
                    error: None,
                },
                crate::protocol::ClientShellRemoteMountAttempt {
                    target: "host-b".into(),
                    mounted: true,
                    mounted_workspace_id: Some("w2".into()),
                    error: None,
                },
                crate::protocol::ClientShellRemoteMountAttempt {
                    target: "host-c".into(),
                    mounted: false,
                    mounted_workspace_id: None,
                    error: Some("connection refused".into()),
                },
            ]
        );
        assert_eq!(
            snapshot.recent_remote_mount_targets,
            vec!["host-b".to_string(), "host-a".to_string()]
        );
    }

    fn test_app() -> crate::app::App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        crate::app::App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        )
    }

    /// Policy C / M2: the tab loop at :90-99 zips `snapshot.tabs`
    /// (API-shaped) positionally against a fresh `workspace.tabs`
    /// traversal, unguarded by any id check. Two workspaces x two tabs, one
    /// tab renamed, asserted pairwise against `AppState` directly
    /// (workspace_id, label, name_source) — not just a count — so any later
    /// change that reorders or filters either side trips this test instead
    /// of silently misattributing a tab's rename to its neighbor.
    #[test]
    fn char_client_shell_tabs_pair_with_their_own_workspace_tab() {
        let mut app = test_app();
        let mut ws1 = Workspace::test_new("ws1");
        let ws1_second_tab = ws1.test_add_tab(None);
        ws1.tabs[ws1_second_tab].set_custom_name("renamed-tab".to_string());
        let ws1_id = ws1.id.clone();

        let mut ws2 = Workspace::test_new("ws2");
        ws2.test_add_tab(None);
        let ws2_id = ws2.id.clone();

        app.state.workspaces = vec![ws1, ws2];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        app.state.selected = 0;

        let snap = snapshot(&app, "boot", 1, None, None);

        let expected: Vec<(&str, String, crate::workspace::naming::NameSource, bool)> = app
            .state
            .workspaces
            .iter()
            .flat_map(|ws| {
                let ws_id = ws.id.as_str();
                ws.tabs.iter().enumerate().map(move |(idx, tab)| {
                    let (label, name_source) = ws.resolved_tab_display(idx).unwrap();
                    (ws_id, label, name_source, tab.zoomed)
                })
            })
            .collect();

        assert_eq!(snap.tabs.len(), expected.len());
        assert_eq!(snap.tabs.len(), 4);
        for (client_tab, (ws_id, label, name_source, zoomed)) in
            snap.tabs.iter().zip(expected.iter())
        {
            assert_eq!(&client_tab.workspace_id, ws_id);
            assert_eq!(&client_tab.label, label);
            assert_eq!(client_tab.name_source, *name_source);
            assert_eq!(client_tab.zoomed, *zoomed);
        }
        assert_eq!(snap.tabs[0].workspace_id, ws1_id);
        assert_eq!(snap.tabs[1].workspace_id, ws1_id);
        assert_eq!(snap.tabs[2].workspace_id, ws2_id);
        assert_eq!(snap.tabs[3].workspace_id, ws2_id);
        assert_ne!(
            snap.tabs[0].name_source,
            crate::workspace::naming::NameSource::Override
        );
        assert_eq!(
            snap.tabs[1].name_source,
            crate::workspace::naming::NameSource::Override
        );
        assert_eq!(snap.tabs[1].label, "renamed-tab");
        assert_ne!(
            snap.tabs[2].name_source,
            crate::workspace::naming::NameSource::Override
        );
        assert_ne!(
            snap.tabs[3].name_source,
            crate::workspace::naming::NameSource::Override
        );
    }

    /// `name_source: NameSource` on both wire structs lets a client tell a
    /// real override apart from every other rung. The `custom_label: bool`
    /// it supersedes (workspace :64, tab :108) was exactly
    /// `custom_name.is_some()` / `!is_auto_named()` — a one-bit projection
    /// of "has an override" that collapsed every other rung into "false".
    #[test]
    fn name_source_distinguishes_override_from_every_other_rung() {
        let mut app = test_app();
        let ws_named = Workspace::test_new("named-ws");
        let mut ws_auto = Workspace::test_new("ignored");
        ws_auto.custom_name = None;
        let named_tab_idx = ws_auto.test_add_tab(Some("named-tab"));
        let auto_tab_idx = ws_auto.test_add_tab(None);
        assert!(named_tab_idx > 0 && auto_tab_idx > named_tab_idx);

        app.state.workspaces = vec![ws_named, ws_auto];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        app.state.selected = 0;

        let snap = snapshot(&app, "boot", 1, None, None);

        assert_eq!(
            snap.workspaces[0].name_source,
            crate::workspace::naming::NameSource::Override,
            "named workspace"
        );
        assert_ne!(
            snap.workspaces[1].name_source,
            crate::workspace::naming::NameSource::Override,
            "auto workspace"
        );

        // ws_auto's own three tabs: root tab (auto), "named-tab" (override),
        // trailing auto tab.
        let ws_auto_tabs = &snap.tabs[1..4];
        assert_ne!(
            ws_auto_tabs[0].name_source,
            crate::workspace::naming::NameSource::Override,
            "root tab is auto-named"
        );
        assert_eq!(
            ws_auto_tabs[1].name_source,
            crate::workspace::naming::NameSource::Override,
            "explicitly named tab"
        );
        assert_ne!(
            ws_auto_tabs[2].name_source,
            crate::workspace::naming::NameSource::Override,
            "trailing auto tab"
        );
    }

    /// A mounted scope's wire `name_source` is `Mirrored`, never
    /// `Override`, even though its label came from the remote and not from
    /// this user. That follows from the mount path single-writing to
    /// `mirrored_name` and never `custom_name` (creation.rs:562-563, :744);
    /// writing the remote's own label into the override slot instead made a
    /// freshly mounted workspace and tab report `custom_label: true` by
    /// accident, whether or not the remote user had ever renamed
    /// anything.
    #[test]
    fn mounted_scopes_report_mirrored_not_override() {
        let mut app = test_app();
        let mut ws = Workspace::test_new("ignored");
        // A materialized federation workspace's id always carries the
        // `r:<host_key>:` namespace prefix (creation.rs:571); classify
        // confirms this fixture is shaped the same way a real mount is.
        ws.id = "r:alice@10.0.0.1#s1:w1".to_string();
        assert!(matches!(
            crate::remote::federation::id::classify(&ws.id),
            crate::remote::federation::id::IdClass::Remote(_)
        ));
        // Single-write: the mount path sets `mirrored_name`, never
        // `custom_name`.
        ws.custom_name = None;
        ws.mirrored_name = Some("remote workspace".to_string());
        ws.tabs[0].custom_name = None;
        ws.tabs[0].mirrored_name = Some("remote tab".to_string());

        app.state.workspaces = vec![ws];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        app.state.selected = 0;

        let snap = snapshot(&app, "boot", 1, None, None);

        assert_eq!(
            snap.workspaces[0].name_source,
            crate::workspace::naming::NameSource::Mirrored,
            "a mounted workspace reports Mirrored, not Override, though nobody \
             renamed it locally"
        );
        assert_eq!(
            snap.tabs[0].name_source,
            crate::workspace::naming::NameSource::Mirrored,
            "a mounted tab reports Mirrored, not Override, though nobody renamed \
             it locally"
        );
    }

    /// The deprecated one-bit override flag must keep reporting `true` for
    /// a federation-mounted scope. Before the naming ladder existed the
    /// mount path wrote the remote's label into the override slot, so the
    /// flag read `true`; the label now lives in the mirror slot instead,
    /// and a deployed client that filters on this flag would silently drop
    /// mounted labels if it started reading `false`.
    #[test]
    fn mounted_scopes_still_report_the_override_flag() {
        let mut app = test_app();
        let mut ws = Workspace::test_new("ignored");
        ws.id = "r:alice@10.0.0.1#s1:w1".to_string();
        ws.custom_name = None;
        ws.mirrored_name = Some("remote workspace".to_string());
        ws.tabs[0].custom_name = None;
        ws.tabs[0].mirrored_name = Some("remote tab".to_string());

        app.state.workspaces = vec![ws];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        app.state.selected = 0;

        let snap = snapshot(&app, "boot", 1, None, None);

        assert!(
            snap.workspaces[0].custom_label,
            "a mounted workspace carries a name somebody set, on the remote"
        );
        assert!(
            snap.tabs[0].custom_label,
            "a mounted tab carries a name somebody set, on the remote"
        );
    }

    /// The same flag must stay `false` for a scope whose label is derived
    /// locally, exactly as it was before the ladder existed.
    #[test]
    fn derived_scopes_do_not_report_the_override_flag() {
        let mut app = test_app();
        let mut ws = Workspace::test_new("ignored");
        ws.custom_name = None;
        ws.tabs[0].custom_name = None;

        app.state.workspaces = vec![ws];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        app.state.selected = 0;

        let snap = snapshot(&app, "boot", 1, None, None);

        assert!(!snap.workspaces[0].custom_label);
        assert!(!snap.tabs[0].custom_label);
    }

    /// The popup pane title (:471) is the legitimately scope-less resolve
    /// — a `PopupPaneState` belongs to no tab, so its title is
    /// `manual_label` or the literal `"popup"`, never an inherited tab
    /// label. That degrade is deliberate (documented in `naming.rs`, not an
    /// oversight at the call site).
    #[tokio::test]
    async fn char_popup_pane_title_is_manual_label_only() {
        let mut app = test_app();
        let pane_id = crate::layout::PaneId::alloc();
        let terminal_id = crate::terminal::TerminalId::alloc();
        let mut terminal = crate::terminal::TerminalState::new(
            terminal_id.clone(),
            std::path::PathBuf::from("/tmp"),
        );
        terminal.manual_label = Some("popup title".to_string());
        app.state.terminals.insert(terminal_id.clone(), terminal);
        let (runtime, _rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        app.terminal_runtimes.insert(terminal_id.clone(), runtime);
        app.state.popup_pane = Some(crate::app::state::PopupPaneState {
            pane_id,
            terminal_id: terminal_id.clone(),
            width: None,
            height: None,
        });

        let area = Rect::new(0, 0, 80, 24);
        let cell_size = crate::kitty_graphics::HostCellSize {
            width_px: 0,
            height_px: 0,
        };

        let surface = render_popup_surface(&app, area, false, cell_size)
            .expect("popup surface must render with a registered runtime and popup state");
        assert_eq!(surface.title, "popup title");

        app.state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .manual_label = None;
        let surface = render_popup_surface(&app, area, false, cell_size).unwrap();
        assert_eq!(
            surface.title, "popup",
            "no manual_label falls back to the literal \"popup\", never a tab label"
        );
    }

    #[test]
    fn snapshot_badges_only_outdated_integrations() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = crate::app::App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.integration_recommendations =
            vec![crate::integration::IntegrationRecommendation {
                target: crate::api::schema::IntegrationTarget::Claude,
                label: "claude",
                command: "claude",
                available: true,
                path: std::path::PathBuf::from("claude-hook"),
                state: crate::integration::IntegrationStatusKind::NotInstalled,
            }];

        assert!(!snapshot(&app, "boot", 1, None, None).integration_updates_available);

        app.state.integration_recommendations[0].state =
            crate::integration::IntegrationStatusKind::Outdated;
        assert!(snapshot(&app, "boot", 2, None, None).integration_updates_available);
    }

    #[test]
    fn split_hits_follow_released_border_and_gap_geometry() {
        let horizontal = crate::layout::SplitBorder {
            pos: 20,
            direction: ratatui::layout::Direction::Horizontal,
            ratio: 0.5,
            area: Rect::new(2, 3, 40, 12),
            path: vec![false],
        };
        assert_eq!(
            split_hit_rect(&horizontal, true, false, &[]),
            Some(Rect::new(20, 3, 1, 12))
        );
        assert_eq!(
            split_hit_rect(&horizontal, true, true, &[]),
            Some(Rect::new(19, 3, 2, 12))
        );
        assert_eq!(
            split_hit_rect(&horizontal, false, true, &[]),
            Some(Rect::new(19, 3, 1, 12))
        );
        assert_eq!(split_hit_rect(&horizontal, false, false, &[]), None);

        let vertical = crate::layout::SplitBorder {
            pos: 9,
            direction: ratatui::layout::Direction::Vertical,
            ratio: 0.5,
            area: Rect::new(2, 3, 40, 12),
            path: vec![true],
        };
        assert_eq!(
            split_hit_rect(&vertical, true, true, &[]),
            Some(Rect::new(2, 8, 40, 2))
        );

        let edge = crate::layout::SplitBorder {
            pos: 0,
            direction: ratatui::layout::Direction::Horizontal,
            ratio: 0.5,
            area: Rect::new(0, 0, 1, 4),
            path: Vec::new(),
        };
        assert_eq!(
            split_hit_rect(&edge, true, true, &[]),
            Some(Rect::new(0, 0, 1, 4))
        );
        assert_eq!(split_hit_rect(&edge, false, true, &[]), None);
        assert_eq!(
            split_hit_rect(&horizontal, false, true, &[Rect::new(19, 3, 1, 12)]),
            None
        );
    }
}
