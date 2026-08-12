use std::path::PathBuf;

use crate::api::schema::{
    EventData, EventEnvelope, EventKind, ResponseResult, TabCreateParams, TabListParams,
    TabMoveParams, TabRenameParams, TabTarget,
};
use crate::app::{App, Mode};

use super::responses::{encode_error, encode_success};

impl App {
    pub(super) fn handle_tab_list(&mut self, id: String, params: TabListParams) -> String {
        let tabs = if let Some(workspace_id) = params.workspace_id {
            let Some(ws_idx) = self.parse_workspace_id(&workspace_id) else {
                return workspace_not_found(id, &workspace_id);
            };
            let Some(_) = self.state.workspaces.get(ws_idx) else {
                return workspace_not_found(id, &workspace_id);
            };
            self.tab_list_info(ws_idx)
        } else {
            let mut tabs = Vec::new();
            for (ws_idx, ws) in self.state.workspaces.iter().enumerate() {
                for tab_idx in 0..ws.tabs.len() {
                    if let Some(tab) = self.tab_info(ws_idx, tab_idx) {
                        tabs.push(tab);
                    }
                }
            }
            tabs
        };

        encode_success(id, ResponseResult::TabList { tabs })
    }

    pub(super) fn handle_tab_get(&mut self, id: String, target: TabTarget) -> String {
        let Some((ws_idx, tab_idx)) = self.parse_tab_id(&target.tab_id) else {
            return tab_not_found(id, &target.tab_id);
        };
        let Some(tab) = self.tab_info(ws_idx, tab_idx) else {
            return tab_not_found(id, &target.tab_id);
        };

        encode_success(id, ResponseResult::TabInfo { tab })
    }

    pub(super) fn handle_tab_create(&mut self, id: String, params: TabCreateParams) -> String {
        let TabCreateParams {
            workspace_id,
            cwd,
            focus,
            label,
            env,
        } = params;
        let ws_idx = if let Some(workspace_id) = workspace_id {
            let Some(ws_idx) = self.parse_workspace_id(&workspace_id) else {
                return workspace_not_found(id, &workspace_id);
            };
            ws_idx
        } else if let Some(active) = self.state.active {
            active
        } else {
            return encode_error(id, "workspace_not_found", "no active workspace");
        };
        // Federation-mount awareness: a federated workspace's pane ids are
        // namespaced `r:<host>:...` (`crate::remote::federation::id`). The
        // local-spawn path below (`Workspace::create_tab`) always spawns a
        // LOCAL PTY and stamps it with a public tab/pane id derived from the
        // target workspace, which for a mounted workspace looks exactly like
        // a real remote pane even though the backing shell actually runs on
        // this machine. Refuse instead of silently misattributing a local
        // shell to the remote host (mirrors the `remote_split_unsupported`
        // refusal in `app/api/panes.rs::handle_pane_split`; remote tab
        // creation over the federation link is not implemented yet).
        if matches!(
            crate::remote::federation::id::classify(&self.public_workspace_id(ws_idx)),
            crate::remote::federation::id::IdClass::Remote(_)
        ) {
            return encode_error(
                id,
                "remote_tab_unsupported",
                "remote workspace: new tab not supported yet",
            );
        }
        let cwd = cwd.map(PathBuf::from).unwrap_or_else(|| {
            self.resolve_new_terminal_cwd(self.focused_pane_cwd_in_workspace(ws_idx))
        });
        let (rows, cols) = self.state.estimate_pane_size();
        let default_shell = self.state.default_shell.clone();
        let scrollback_limit_bytes = self.state.pane_scrollback_limit_bytes;
        let host_terminal_theme = self.state.host_terminal_theme;
        let host_terminal_appearance = self.state.host_terminal_appearance;
        let extra_env = match super::env::normalize_launch_env(env) {
            Ok(env) => env,
            Err((code, message)) => return encode_error(id, &code, message),
        };
        let result = self
            .state
            .workspaces
            .get_mut(ws_idx)
            .ok_or_else(|| std::io::Error::other("workspace disappeared"))
            .and_then(|ws| {
                ws.create_tab(
                    rows,
                    cols,
                    cwd,
                    scrollback_limit_bytes,
                    host_terminal_theme,
                    host_terminal_appearance,
                    crate::pane::PaneShellConfig::new(&default_shell, self.state.shell_mode),
                    extra_env,
                )
            });
        match result {
            Ok((tab_idx, terminal, runtime)) => {
                self.terminal_runtimes.insert(terminal.id.clone(), runtime);
                self.state.terminals.insert(terminal.id.clone(), terminal);
                self.state.remove_alias_shadowed_by_new_pane(
                    self.state.workspaces[ws_idx].tabs[tab_idx].root_pane,
                );
                if let Some(label) = label {
                    let workspace_id = self.state.workspaces[ws_idx].id.clone();
                    let tab_id = self.public_tab_id(ws_idx, tab_idx).unwrap_or_else(|| {
                        crate::workspace::public_tab_id_for_number(&workspace_id, tab_idx + 1)
                    });
                    if let Some(tab) = self
                        .state
                        .workspaces
                        .get_mut(ws_idx)
                        .and_then(|ws| ws.tabs.get_mut(tab_idx))
                    {
                        tab.set_custom_name(label);
                        crate::logging::tab_renamed(&workspace_id, &tab_id);
                    }
                }
                if focus {
                    self.state.switch_workspace_tab(ws_idx, tab_idx);
                    self.state.mode = Mode::Terminal;
                }
                self.schedule_session_save();
                self.emit_tab_created_events(ws_idx, tab_idx);
                encode_success(
                    id,
                    self.tab_created_result(ws_idx, tab_idx)
                        .expect("new tab should produce a complete create response"),
                )
            }
            Err(err) => encode_error(id, "tab_create_failed", err.to_string()),
        }
    }

    pub(super) fn handle_tab_focus(&mut self, id: String, target: TabTarget) -> String {
        let Some((ws_idx, tab_idx)) = self.parse_tab_id(&target.tab_id) else {
            return tab_not_found(id, &target.tab_id);
        };
        self.state.switch_workspace_tab(ws_idx, tab_idx);
        let tab = self.tab_info(ws_idx, tab_idx).unwrap();

        encode_success(id, ResponseResult::TabInfo { tab })
    }

    pub(super) fn handle_tab_rename(&mut self, id: String, params: TabRenameParams) -> String {
        let Some((ws_idx, tab_idx)) = self.parse_tab_id(&params.tab_id) else {
            return tab_not_found(id, &params.tab_id);
        };
        let workspace_id = self.state.workspaces[ws_idx].id.clone();
        let tab_id = self.public_tab_id(ws_idx, tab_idx).unwrap_or_else(|| {
            crate::workspace::public_tab_id_for_number(&workspace_id, tab_idx + 1)
        });
        let Some(tab) = self
            .state
            .workspaces
            .get_mut(ws_idx)
            .and_then(|ws| ws.tabs.get_mut(tab_idx))
        else {
            return tab_not_found(id, &params.tab_id);
        };
        tab.set_custom_name(params.label.clone());
        crate::logging::tab_renamed(&workspace_id, &tab_id);
        if self.state.active == Some(ws_idx) {
            // Reflow the tab bar so the new label width takes effect immediately.
            // The tab bar renders into cached hit areas; without this refresh the
            // old geometry lingers until the next refresh (e.g. a tab switch),
            // leaving the visible label stale. Mirrors handle_tab_move.
            self.state.refresh_tab_bar_view();
        }
        self.schedule_session_save();
        self.emit_event(EventEnvelope {
            event: EventKind::TabRenamed,
            data: EventData::TabRenamed {
                tab_id: self.public_tab_id(ws_idx, tab_idx).unwrap(),
                workspace_id: self.public_workspace_id(ws_idx),
                label: params.label,
            },
        });
        let tab = self.tab_info(ws_idx, tab_idx).unwrap();

        encode_success(id, ResponseResult::TabInfo { tab })
    }

    pub(super) fn handle_tab_move(&mut self, id: String, params: TabMoveParams) -> String {
        let Some((ws_idx, tab_idx)) = self.parse_tab_id(&params.tab_id) else {
            return tab_not_found(id, &params.tab_id);
        };
        let Some(ws) = self.state.workspaces.get(ws_idx) else {
            return tab_not_found(id, &params.tab_id);
        };
        if params.insert_index > ws.tabs.len() {
            return encode_error(
                id,
                "tab_move_failed",
                format!("insert_index {} is out of bounds", params.insert_index),
            );
        }

        let tab_id = self
            .public_tab_id(ws_idx, tab_idx)
            .unwrap_or_else(|| crate::workspace::public_tab_id_for_number(&ws.id, tab_idx + 1));
        let workspace_id = self.public_workspace_id(ws_idx);
        let insert_index = params.insert_index;
        let moved = self
            .state
            .workspaces
            .get_mut(ws_idx)
            .is_some_and(|ws| ws.move_tab(tab_idx, insert_index));
        let tabs = self.tab_list_info(ws_idx);
        if moved {
            self.schedule_session_save();
            if self.state.active == Some(ws_idx) {
                self.state.tab_scroll_follow_active = true;
                self.state.refresh_tab_bar_view();
            }
            self.emit_event(EventEnvelope {
                event: EventKind::TabMoved,
                data: EventData::TabMoved {
                    tab_id,
                    workspace_id,
                    insert_index,
                    tabs: tabs.clone(),
                },
            });
        }

        encode_success(id, ResponseResult::TabList { tabs })
    }

    pub(super) fn handle_tab_close(&mut self, id: String, target: TabTarget) -> String {
        let Some((ws_idx, tab_idx)) = self.parse_tab_id(&target.tab_id) else {
            return tab_not_found(id, &target.tab_id);
        };
        let Some(tab_id) = self.public_tab_id(ws_idx, tab_idx) else {
            return tab_not_found(id, &target.tab_id);
        };
        let workspace_id = self.public_workspace_id(ws_idx);
        let Some(ws) = self.state.workspaces.get(ws_idx) else {
            return tab_not_found(id, &target.tab_id);
        };
        let closes_workspace = ws.tabs.len() <= 1;
        let terminal_ids = self.state.terminal_ids_for_tab(ws_idx, tab_idx);
        let pane_ids = ws
            .tabs
            .get(tab_idx)
            .map(|tab| tab.layout.pane_ids())
            .unwrap_or_default();

        // Federation-mount awareness, the close-side counterpart of the
        // `remote_tab_unsupported` refusal in `handle_tab_create` above. Every
        // workspace one mount materializes shares that mount's single
        // `federation:<host_key>` worktree space, so the group close below
        // would take *all* of them down — the whole mirror of a still-live
        // mount — for one workspace's last tab. Close exactly the one
        // workspace instead, the same single teardown the federation resync
        // handlers use. (The mount itself stays up; unmounting is
        // `workspace.close` on the mount's own workspace group.)
        let federated = matches!(
            crate::remote::federation::id::classify(&workspace_id),
            crate::remote::federation::id::IdClass::Remote(_)
        );
        if closes_workspace && federated {
            let workspace = self.workspace_info(ws_idx);
            let closing_ids: std::collections::HashSet<String> = self
                .state
                .workspaces
                .get(ws_idx)
                .map(|ws| ws.id.clone())
                .into_iter()
                .collect();
            self.purge_federation_state_for_workspaces(&closing_ids);
            self.close_single_workspace_at(ws_idx);
            self.state.remove_plugin_pane_records(pane_ids);
            self.shutdown_detached_terminal_runtimes();
            self.schedule_session_save();
            self.emit_event(EventEnvelope {
                event: EventKind::TabClosed,
                data: EventData::TabClosed {
                    tab_id,
                    workspace_id: workspace_id.clone(),
                },
            });
            self.emit_event(EventEnvelope {
                event: EventKind::WorkspaceClosed,
                data: EventData::WorkspaceClosed {
                    workspace_id,
                    workspace: Some(workspace),
                },
            });
            return encode_success(id, ResponseResult::Ok {});
        }

        if closes_workspace {
            if self.state.confirm_implicit_worktree_group_close(ws_idx) {
                return encode_error(
                    id,
                    "confirmation_required",
                    "closing this tab would close a worktree group",
                );
            }
            let workspace = self.workspace_info(ws_idx);
            self.state.selected = ws_idx;
            self.state.close_selected_workspace();
            self.state.remove_plugin_pane_records(pane_ids);
            self.shutdown_detached_terminal_runtimes();
            self.emit_event(EventEnvelope {
                event: EventKind::TabClosed,
                data: EventData::TabClosed {
                    tab_id,
                    workspace_id: workspace_id.clone(),
                },
            });
            self.emit_event(EventEnvelope {
                event: EventKind::WorkspaceClosed,
                data: EventData::WorkspaceClosed {
                    workspace_id,
                    workspace: Some(workspace),
                },
            });
            return encode_success(id, ResponseResult::Ok {});
        }

        let Some(ws) = self.state.workspaces.get_mut(ws_idx) else {
            return tab_not_found(id, &target.tab_id);
        };
        if !ws.close_tab(tab_idx) {
            return encode_error(
                id,
                "tab_close_failed",
                format!("tab {} could not be closed", target.tab_id),
            );
        }
        // A plain local `tab.close` is still allowed on a mirrored
        // workspace, independently of `tab.close_remote` — nothing stops a
        // caller having a remote close in flight for this same tab when the
        // local close beats its ack. Purge it now (a no-op if none exists)
        // so a late/never-arriving `TabCloseResponse` for a now-gone tab
        // cannot act on whatever later reuses the same slot. Harmless on a
        // non-federated tab: nothing in `pending_remote_closes` would ever
        // match its (never namespaced) tab id.
        self.purge_pending_remote_close_for_tab(&tab_id);
        self.state.remove_plugin_pane_records(pane_ids);
        self.state.remove_unattached_terminal_ids(terminal_ids);
        self.shutdown_detached_terminal_runtimes();
        self.schedule_session_save();
        self.emit_event(EventEnvelope {
            event: EventKind::TabClosed,
            data: EventData::TabClosed {
                tab_id,
                workspace_id,
            },
        });

        encode_success(id, ResponseResult::Ok {})
    }

    /// `tab.close_remote`: forwards a close to the serving host of a
    /// federated tab instead of retiring the local mirror
    /// (`handle_tab_close` NEVER reaches the wire — this is the distinct
    /// opt-in verb for that). Mirrors `dispatch_remote_pane_close`
    /// (`api/panes.rs`)'s exact shape/reasoning.
    pub(super) fn handle_tab_close_remote(&mut self, id: String, target: TabTarget) -> String {
        let Some((ws_idx, tab_idx)) = self.parse_tab_id(&target.tab_id) else {
            return tab_not_found(id, &target.tab_id);
        };
        self.dispatch_remote_tab_close(id, ws_idx, tab_idx)
    }

    fn dispatch_remote_tab_close(&mut self, id: String, ws_idx: usize, tab_idx: usize) -> String {
        let workspace_id = self.public_workspace_id(ws_idx);
        if !matches!(
            crate::remote::federation::id::classify(&workspace_id),
            crate::remote::federation::id::IdClass::Remote(_)
        ) {
            return encode_error(
                id,
                "remote_close_unsupported",
                "tab.close_remote requires a federated workspace",
            );
        }
        // CANONICAL public tab id, never the caller-supplied `target.tab_id`
        // string: `parse_tab_id`'s `t_<ws>_<idx>` form is positional, so a
        // neighbour tab closing (and renumbering) before the eventual ack
        // arrives could otherwise redirect the teardown onto a live tab that
        // reused the same index. This is the id stored in
        // `RemoteCloseTarget::Tab` and matched against by
        // `handle_federation_tab_close_ready`.
        let Some(tab_id) = self.public_tab_id(ws_idx, tab_idx) else {
            return tab_not_found(id, &format!("{workspace_id}:t{}", tab_idx + 1));
        };

        let pane_ids = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|ws| ws.tabs.get(tab_idx))
            .map(|tab| tab.layout.pane_ids())
            .unwrap_or_default();
        let out_tx = pane_ids.into_iter().find_map(|pane_id| {
            self.state.workspaces[ws_idx]
                .terminal_id(pane_id)
                .cloned()
                .and_then(|terminal_id| self.terminal_runtimes.get(&terminal_id))
                .and_then(|runtime| runtime.remote_out_tx())
        });
        let Some(out_tx) = out_tx else {
            return encode_error(
                id,
                "remote_close_unsupported",
                "closing a tab in a remote-federated workspace requires a live mount; \
                 this tab's mount is not connected",
            );
        };

        let Some(origin) = self.federation_host_key_for_workspace(ws_idx) else {
            return encode_error(
                id,
                "remote_close_unsupported",
                "closing a tab in a remote-federated workspace requires a live mount; \
                 this workspace has no registered federation mount",
            );
        };
        let Some(mirror) = self.state.remote_mirrors.get(&origin) else {
            return encode_error(
                id,
                "remote_close_unsupported",
                "closing a tab in a remote-federated workspace requires a live mount; \
                 this workspace's mount is not connected",
            );
        };

        // Unlike a workspace id, the local tab id's public form
        // (`<workspace_id>:t<n>`) does NOT wrap the raw remote tab id — only
        // the mirror's OWN namespaced tab id does
        // (`RemoteMirror::tabs()`'s key, indexed here by
        // `remote_resync_tab_index`). Reverse that index by
        // (workspace_id, tab_number) to find it, then strip the namespace
        // the same way the workspace path does.
        let Some(tab_number) = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|ws| ws.public_tab_number(tab_idx))
        else {
            return tab_not_found(id, &tab_id);
        };
        let Some(mirror_tab_id) =
            self.remote_resync_tab_index
                .iter()
                .find_map(|(mirror_tab_id, tab_ref)| {
                    (tab_ref.workspace_id == workspace_id && tab_ref.tab_number == Some(tab_number))
                        .then(|| mirror_tab_id.clone())
                })
        else {
            return encode_error(
                id,
                "remote_close_unsupported",
                "this tab has no recorded remote identity yet; try again once the mount \
                 has synced",
            );
        };
        let mount = crate::remote::federation::id::Mount {
            host_key: origin.clone(),
            server_instance_id: crate::remote::federation::id::ServerInstanceId(String::new()),
            mount_generation: 0,
        };
        let raw_target_tab_id =
            crate::remote::federation::id::strip_mount_namespace(&mount, &mirror_tab_id);

        let request_id = super::panes::next_remote_close_request_id();
        let sent = crate::remote::federation::client::send_tab_close_request(
            mirror,
            &out_tx,
            crate::remote::federation::protocol::TabCloseRequest {
                request_id,
                target_tab_id: raw_target_tab_id,
            },
        );
        if let Err(err) = sent {
            let message = match err {
                crate::remote::federation::client::CloseRequestSendError::CapabilityNotAgreed => {
                    "the remote host does not support tab.close_remote"
                }
                crate::remote::federation::client::CloseRequestSendError::LinkClosed => {
                    "the remote mount's link is closing; the close request could not be sent"
                }
            };
            return encode_error(id, "remote_close_unsupported", message);
        }

        let origin_label = origin.as_str().to_string();
        self.register_pending_remote_close(
            request_id,
            crate::app::creation::PendingRemoteClose {
                workspace_id,
                origin,
                target: crate::app::creation::RemoteCloseTarget::Tab(tab_id),
            },
        );

        encode_success(
            id,
            ResponseResult::TabCloseRequested {
                origin: origin_label,
            },
        )
    }

    fn tab_list_info(&self, ws_idx: usize) -> Vec<crate::api::schema::TabInfo> {
        self.state
            .workspaces
            .get(ws_idx)
            .map(|ws| {
                (0..ws.tabs.len())
                    .filter_map(|idx| self.tab_info(ws_idx, idx))
                    .collect()
            })
            .unwrap_or_default()
    }
}

fn workspace_not_found(id: String, workspace_id: &str) -> String {
    encode_error(
        id,
        "workspace_not_found",
        format!("workspace {workspace_id} not found"),
    )
}

fn tab_not_found(id: String, tab_id: &str) -> String {
    encode_error(id, "tab_not_found", format!("tab {tab_id} not found"))
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{exiting_test_command, shutdown_test_runtimes};
    use super::*;
    use crate::{
        api::schema::SuccessResponse,
        config::{Config, ShellModeConfig},
        workspace::Workspace,
    };

    #[test]
    fn api_tab_close_last_tab_closes_workspace_and_emits_both_events() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&Config::default(), true, None, api_rx, event_hub.clone());
        app.state.workspaces = vec![Workspace::test_new("tabs")];
        app.state.active = Some(0);
        app.state.selected = 0;
        let tab_id = app.public_tab_id(0, 0).unwrap();
        let workspace_id = app.public_workspace_id(0);

        let response = app.handle_tab_close(
            "req".into(),
            TabTarget {
                tab_id: tab_id.clone(),
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(success.result, ResponseResult::Ok {});
        assert!(app.state.workspaces.is_empty());
        assert!(app.state.active.is_none());
        let events = event_hub.events_after(0);
        assert_eq!(
            events
                .iter()
                .map(|(_, event)| event.event)
                .collect::<Vec<_>>(),
            [EventKind::TabClosed, EventKind::WorkspaceClosed]
        );
        assert!(matches!(
            &events[0].1.data,
            EventData::TabClosed {
                tab_id: closed_tab_id,
                workspace_id: closed_workspace_id,
            } if closed_tab_id == &tab_id && closed_workspace_id == &workspace_id
        ));
        assert!(matches!(
            &events[1].1.data,
            EventData::WorkspaceClosed {
                workspace_id: closed_workspace_id,
                workspace: Some(workspace),
            } if closed_workspace_id == &workspace_id
                && workspace.workspace_id == workspace_id
        ));
    }

    #[test]
    fn api_tab_move_reorders_tabs_in_target_workspace() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&Config::default(), true, None, api_rx, event_hub.clone());
        let mut workspace = Workspace::test_new("tabs");
        workspace.test_add_tab(Some("two"));
        workspace.test_add_tab(Some("three"));
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.selected = 0;
        let moved_root = app.state.workspaces[0].tabs[0].root_pane;
        let moved_id = app.public_tab_id(0, 0).unwrap();

        let response = app.handle_tab_move(
            "req".into(),
            TabMoveParams {
                tab_id: moved_id.clone(),
                insert_index: 3,
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::TabList { tabs } = success.result else {
            panic!("expected tab list");
        };
        assert_eq!(app.state.workspaces[0].tabs[2].root_pane, moved_root);
        assert_eq!(tabs[2].tab_id, app.public_tab_id(0, 2).unwrap());
        let events = event_hub.events_after(0);
        assert!(events.iter().any(|(_, event)| {
            matches!(
                &event.data,
                EventData::TabMoved {
                    tab_id,
                    workspace_id,
                    insert_index: 3,
                    tabs,
                } if tab_id == &moved_id
                    && workspace_id == &app.public_workspace_id(0)
                    && tabs[2].tab_id == moved_id
            )
        }));
    }

    #[test]
    fn api_tab_rename_reflows_active_tab_bar() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&Config::default(), true, None, api_rx, event_hub);
        let workspace = Workspace::test_new("tabs");
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.view.tab_bar_rect = ratatui::layout::Rect::new(0, 0, 60, 1);
        app.state.refresh_tab_bar_view();

        let tab_id = app.public_tab_id(0, 0).unwrap();
        let width_before = app.state.view.tab_hit_areas[0].width;

        app.handle_tab_rename(
            "req".into(),
            TabRenameParams {
                tab_id,
                label: "a much longer custom tab label".into(),
            },
        );

        let width_after = app.state.view.tab_hit_areas[0].width;
        assert!(
            width_after > width_before,
            "tab bar should reflow to the new label width immediately: \
             before={width_before}, after={width_after}"
        );
    }

    #[tokio::test]
    async fn tab_create_follows_cached_focused_pane_cwd_without_runtime() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&Config::default(), true, None, api_rx, event_hub);
        app.state.default_shell = exiting_test_command().into();
        app.state.shell_mode = ShellModeConfig::NonLogin;
        let workspace = Workspace::test_new("tabs");
        let focused_pane = workspace.tabs[0].root_pane;
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.ensure_test_terminals();
        let cached_cwd = std::env::temp_dir();
        let terminal_id = app.state.workspaces[0]
            .terminal_id(focused_pane)
            .cloned()
            .unwrap();
        app.state.terminals.get_mut(&terminal_id).unwrap().cwd = cached_cwd.clone();

        let response = app.handle_tab_create(
            "req".into(),
            TabCreateParams {
                workspace_id: None,
                cwd: None,
                focus: false,
                label: None,
                env: Default::default(),
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert!(matches!(success.result, ResponseResult::TabCreated { .. }));
        let created = &app.state.workspaces[0].tabs[1];
        let created_terminal_id = created.terminal_id(created.root_pane).unwrap();
        let created_cwd = &app.state.terminals.get(created_terminal_id).unwrap().cwd;
        assert_eq!(
            crate::worktree::canonical_or_original(created_cwd),
            crate::worktree::canonical_or_original(&cached_cwd)
        );
        shutdown_test_runtimes(&mut app);
    }

    // Regression: creating a tab in a federated remote workspace must not
    // spawn a LOCAL shell stamped with a remote-looking pane id (mirrors
    // `panes.rs::pane_split_in_a_federated_workspace_is_refused_not_misfiled_locally`).
    #[test]
    fn api_tab_create_in_a_federated_workspace_is_refused_not_misfiled_locally() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&Config::default(), true, None, api_rx, event_hub);
        let mut workspace = Workspace::test_new("tabs");
        let remote_workspace_id = "r:alice@10.0.0.1#s1:default".to_string();
        workspace.id = remote_workspace_id.clone();
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.selected = 0;
        let tabs_before = app.state.workspaces[0].tabs.len();

        let response = app.handle_tab_create(
            "req".into(),
            TabCreateParams {
                workspace_id: Some(remote_workspace_id),
                cwd: None,
                focus: false,
                label: None,
                env: Default::default(),
            },
        );

        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "remote_tab_unsupported");
        assert_eq!(
            app.state.workspaces[0].tabs.len(),
            tabs_before,
            "a refused remote tab create must not create any local tab"
        );
    }

    /// A two-tab remote workspace, mounted live: `materialize_federation_
    /// mount` builds the local workspace/tabs, then `begin_federation_mount`
    /// registers the mirror in `remote_mirrors` so `federation_host_key_for_
    /// workspace`/`remote_mirrors.get` (both used by `dispatch_remote_tab_
    /// close`) resolve it. Mirrors `creation.rs`'s `mount_two_tab_mirror`
    /// fixture, but keeps the drive channel's receiver instead of
    /// discarding it, so a test can assert on what was (or was not) sent.
    /// When `agree_close_capability` is false the mirror never agrees
    /// `WORKSPACE_TAB_CLOSE`, exercising the capability-gated refusal path.
    #[cfg(unix)]
    fn app_with_federation_mounted_two_tab_workspace(
        agree_close_capability: bool,
    ) -> (
        App,
        tokio::sync::mpsc::UnboundedReceiver<
            crate::remote::federation::protocol::FederationMessage,
        >,
        usize,
    ) {
        use crate::api::schema::common::AgentStatus;
        use crate::api::schema::session::SessionSnapshot;
        use crate::api::schema::{
            PaneInfo as RemotePaneInfo, TabInfo as RemoteTabInfo, WorkspaceInfo,
        };
        use crate::remote::federation::id::{HostKey, Mount, ServerInstanceId};
        use crate::remote::federation::protocol::EventCursor;

        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );

        let mount = Mount {
            host_key: HostKey::new("alice@10.0.0.1", "s1"),
            server_instance_id: ServerInstanceId("inst-a".to_string()),
            mount_generation: 1,
        };
        let mut mirror = crate::remote::federation::reducer::RemoteMirror::new(mount);
        if agree_close_capability {
            mirror.set_agreed_capabilities(
                [crate::remote::federation::protocol::Capability::new(
                    crate::remote::federation::protocol::Capability::WORKSPACE_TAB_CLOSE,
                )]
                .into_iter()
                .collect(),
            );
        }
        let snapshot = SessionSnapshot {
            version: "0.0.0-test".to_string(),
            protocol: 1,
            focused_workspace_id: None,
            focused_tab_id: None,
            focused_pane_id: None,
            workspaces: vec![WorkspaceInfo {
                workspace_id: "w1".to_string(),
                number: 1,
                label: "remote workspace".to_string(),
                focused: false,
                pane_count: 2,
                tab_count: 2,
                active_tab_id: "w1-tab".to_string(),
                agent_status: AgentStatus::Idle,
                tokens: Default::default(),
                worktree: None,
            }],
            tabs: vec![
                RemoteTabInfo {
                    tab_id: "w1-tab".to_string(),
                    workspace_id: "w1".to_string(),
                    number: 1,
                    label: "first remote tab".to_string(),
                    focused: false,
                    pane_count: 1,
                    agent_status: AgentStatus::Idle,
                },
                RemoteTabInfo {
                    tab_id: "w1-tab2".to_string(),
                    workspace_id: "w1".to_string(),
                    number: 2,
                    label: "second remote tab".to_string(),
                    focused: false,
                    pane_count: 1,
                    agent_status: AgentStatus::Idle,
                },
            ],
            panes: vec![
                RemotePaneInfo {
                    pane_id: "p1".to_string(),
                    terminal_id: "t1".to_string(),
                    workspace_id: "w1".to_string(),
                    tab_id: "w1-tab".to_string(),
                    focused: false,
                    cwd: Some("/home/alice/project".to_string()),
                    foreground_cwd: None,
                    label: Some("remote pane 1".to_string()),
                    agent: None,
                    title: None,
                    terminal_title: None,
                    terminal_title_stripped: None,
                    display_agent: None,
                    agent_status: AgentStatus::Idle,
                    state_labels: Default::default(),
                    tokens: Default::default(),
                    agent_session: None,
                    scroll: None,
                    revision: 0,
                },
                RemotePaneInfo {
                    pane_id: "p2".to_string(),
                    terminal_id: "t2".to_string(),
                    workspace_id: "w1".to_string(),
                    tab_id: "w1-tab2".to_string(),
                    focused: false,
                    cwd: Some("/home/alice/project".to_string()),
                    foreground_cwd: None,
                    label: Some("remote pane 2".to_string()),
                    agent: None,
                    title: None,
                    terminal_title: None,
                    terminal_title_stripped: None,
                    display_agent: None,
                    agent_status: AgentStatus::Idle,
                    state_labels: Default::default(),
                    tokens: Default::default(),
                    agent_session: None,
                    scroll: None,
                    revision: 0,
                },
            ],
            layouts: Vec::new(),
            agents: Vec::new(),
        };
        mirror.apply_snapshot(&snapshot, EventCursor(0));

        let mut router = crate::remote::federation::client::TerminalChannelRouter::new();
        let (out_tx, out_rx) = tokio::sync::mpsc::unbounded_channel();
        let (clipboard_tx, _clipboard_rx) = tokio::sync::mpsc::unbounded_channel();
        let created = app
            .materialize_federation_mount(&mirror, &mut router, &out_tx, &clipboard_tx)
            .expect("materialization must succeed against a loopback-shaped snapshot");
        let ws_idx = created[0];
        app.state
            .begin_federation_mount(mirror)
            .expect("registering the mirror must succeed for a fresh HostKey");
        (app, out_rx, ws_idx)
    }

    /// `dispatch_remote_tab_close`: closing a NON-last federated tab via
    /// `tab.close_remote` must send a `TabCloseRequest` over the mount's
    /// link, register a pending-close entry keyed by the CANONICAL public
    /// tab id, acknowledge with the `tab_close_requested` success, and NOT remove the
    /// local mirror tab. Closing a non-last tab is the originally reported
    /// failure, and it exercises the reverse-index lookup into
    /// `remote_resync_tab_index`, not just the trivial single-tab case.
    #[cfg(unix)]
    #[tokio::test]
    async fn dispatch_remote_tab_close_sends_a_request_and_registers_pending() {
        let (mut app, mut out_rx, ws_idx) = app_with_federation_mounted_two_tab_workspace(true);
        while out_rx.try_recv().is_ok() {}
        let tab_id = app.public_tab_id(ws_idx, 1).unwrap();
        let tabs_before = app.state.workspaces[ws_idx].tabs.len();

        let response = app.handle_tab_close_remote(
            "req".into(),
            TabTarget {
                tab_id: tab_id.clone(),
            },
        );

        let success: crate::api::schema::SuccessResponse = serde_json::from_str(&response).unwrap();
        assert!(
            matches!(success.result, ResponseResult::TabCloseRequested { .. }),
            "close_remote must answer with a success, not an error envelope: {response}"
        );
        assert_eq!(
            app.state.workspaces[ws_idx].tabs.len(),
            tabs_before,
            "dispatching a remote close must not remove the local mirror tab; only the \
             eventual TabCloseResponse does"
        );

        let request = match out_rx.try_recv().expect("a TabCloseRequest was sent") {
            crate::remote::federation::protocol::FederationMessage::TabCloseRequest(request) => {
                request
            }
            other => panic!("expected a TabCloseRequest, got {other:?}"),
        };
        assert_eq!(request.target_tab_id, "w1-tab2");
        assert_eq!(app.pending_remote_closes.len(), 1);
    }

    /// Echo-rule safety test: `tab.close` on a federated workspace must
    /// NEVER reach the wire. `tab.close_remote` is the distinct opt-in verb
    /// for that.
    #[cfg(unix)]
    #[tokio::test]
    async fn tab_close_on_a_mirrored_workspace_sends_nothing() {
        let (mut app, mut out_rx, ws_idx) = app_with_federation_mounted_two_tab_workspace(true);
        while out_rx.try_recv().is_ok() {}
        let tab_id = app.public_tab_id(ws_idx, 1).unwrap();

        let response = app.handle_tab_close(
            "req".into(),
            TabTarget {
                tab_id: tab_id.clone(),
            },
        );
        let _: SuccessResponse = serde_json::from_str(&response).unwrap();

        // `tab.close` legitimately still sends ordinary local-unmount
        // teardown traffic (e.g. `Terminal(Close)` for the closed tab's own
        // terminal channel) — the echo rule under test is narrower: no
        // `TabCloseRequest` (the wire message that would ask the SERVING
        // host to close something) may ever be sent by this verb.
        while let Ok(msg) = out_rx.try_recv() {
            assert!(
                !matches!(
                    msg,
                    crate::remote::federation::protocol::FederationMessage::TabCloseRequest(_)
                ),
                "tab.close on a federated workspace must never send a TabCloseRequest; only \
                 tab.close_remote may"
            );
        }
    }

    /// An ungated send is fatal, not merely useless — `dispatch_remote_
    /// tab_close` must refuse instead of sending when the peer never agreed
    /// `WORKSPACE_TAB_CLOSE`.
    #[cfg(unix)]
    #[tokio::test]
    async fn dispatch_remote_tab_close_without_the_capability_agreed_is_refused() {
        let (mut app, mut out_rx, ws_idx) = app_with_federation_mounted_two_tab_workspace(false);
        while out_rx.try_recv().is_ok() {}
        let tab_id = app.public_tab_id(ws_idx, 1).unwrap();

        let response = app.handle_tab_close_remote(
            "req".into(),
            TabTarget {
                tab_id: tab_id.clone(),
            },
        );

        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "remote_close_unsupported");
        assert!(
            out_rx.try_recv().is_err(),
            "a capability that was never agreed must never be sent over the wire"
        );
        assert!(app.pending_remote_closes.is_empty());
    }

    /// A plain LOCAL `tab.close` on a
    /// mirrored workspace is still allowed independently of
    /// `tab.close_remote` — nothing stops a caller having a remote close in
    /// flight for the same tab when the local close beats its ack. The
    /// pending entry must not be left stranded (`handle_tab_close`'s own
    /// purge call, `creation::App::purge_pending_remote_close_for_tab`), so
    /// the eventual (now-late) `TabCloseResponse::Closed` finds nothing to
    /// tear down and is a silent idempotent no-op — no panic, no second
    /// `TabClosed`/`WorkspaceClosed` emission — the same idempotency
    /// guarantee the racing-RESYNC-removal case already has, extended to
    /// cover a racing LOCAL close too.
    #[cfg(unix)]
    #[tokio::test]
    async fn tab_close_remote_ack_after_a_racing_local_tab_close_is_idempotent() {
        let (mut app, mut out_rx, ws_idx) = app_with_federation_mounted_two_tab_workspace(true);
        while out_rx.try_recv().is_ok() {}
        let tab_id = app.public_tab_id(ws_idx, 1).unwrap();

        // Register the pending remote close through the real dispatch path
        // so the recorded origin matches production exactly.
        let dispatch_response = app.handle_tab_close_remote(
            "req".into(),
            TabTarget {
                tab_id: tab_id.clone(),
            },
        );
        let _: SuccessResponse = serde_json::from_str(&dispatch_response).unwrap();
        assert_eq!(app.pending_remote_closes.len(), 1);
        let (&request_id, pending) = app.pending_remote_closes.iter().next().unwrap();
        let origin = pending.origin.clone();

        // The local close beats the ack — allowed by design, and the only
        // realistic way this race happens (nothing correlates the two
        // verbs).
        let close_response = app.handle_tab_close(
            "req2".into(),
            TabTarget {
                tab_id: tab_id.clone(),
            },
        );
        let _: SuccessResponse = serde_json::from_str(&close_response).unwrap();
        assert_eq!(
            app.state.workspaces[ws_idx].tabs.len(),
            1,
            "the local close must have removed the tab"
        );
        assert!(
            app.pending_remote_closes.is_empty(),
            "a local close on a mirror must purge any pending remote close for that same \
             tab, not leave it stranded"
        );

        // The now-late TabCloseResponse::Closed finally arrives.
        app.handle_federation_tab_close_ready(request_id, origin);

        assert_eq!(
            app.state.workspaces[ws_idx].tabs.len(),
            1,
            "the late ack must not re-close anything, double-emit, or panic"
        );
    }
}
