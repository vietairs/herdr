use std::path::PathBuf;
#[cfg(unix)]
use std::sync::atomic::Ordering;

use crate::api::schema::{
    EventData, EventEnvelope, EventKind, ResponseResult, WorkspaceCreateParams,
    WorkspaceMountRemoteParams, WorkspaceMoveParams, WorkspaceRenameParams,
    WorkspaceReportMetadataParams, WorkspaceTarget,
};
use crate::app::App;
#[cfg(unix)]
use crate::app::ToastKind;

use super::super::api_helpers::{normalize_metadata_source, normalize_metadata_ttl};
use super::responses::{encode_error, encode_success};

impl App {
    /// REVISED Phase A (multi-remote federated workspace launch): mounts a
    /// federation target as server-daemon-owned state, spawned inside this
    /// daemon's own tokio runtime (`app.run()` is polled inside
    /// `rt.block_on`, so `tokio::spawn` here is always in-context). The
    /// response only acknowledges the dial+mount task was started — success
    /// materializes into real workspaces (`AppEvent::FederationMountReady`,
    /// handled by `App::run`'s own event loop, which owns `&mut App`);
    /// failure surfaces as a sidebar notice
    /// (`AppEvent::FederationMountFailed`). Neither outcome ever tears down
    /// the local session or the server daemon itself.
    #[cfg(not(unix))]
    pub(super) fn handle_workspace_mount_remote(
        &mut self,
        id: String,
        _params: WorkspaceMountRemoteParams,
    ) -> String {
        encode_error(
            id,
            "unsupported_platform",
            "workspace.mount_remote is not supported on this platform",
        )
    }

    /// Phase B requirement 3/9: one request carries the full target list;
    /// each non-empty, non-duplicate target is spawned as its own
    /// `tokio::spawn` dial+mount task. Because each task is independently
    /// spawned rather than awaited together in this handler, all N dials
    /// already run concurrently against the daemon's tokio runtime (no
    /// per-target serial stacking) — each task carries its own ~25s dial
    /// budget internally (`dial_and_mount`'s `FEDERATION_CONNECT_TIMEOUT` +
    /// `FEDERATION_MOUNT_TIMEOUT`), so N targets still complete in ~25s
    /// wall-clock, not 25s×N. A target whose `HostKey` is already mounted is
    /// rejected immediately (before spawning any dial) with a per-host
    /// failure event, isolating it from the other targets in the same
    /// request (requirement 4).
    #[cfg(unix)]
    pub(super) fn handle_workspace_mount_remote(
        &mut self,
        id: String,
        params: WorkspaceMountRemoteParams,
    ) -> String {
        let targets: Vec<String> = params
            .targets
            .into_iter()
            .map(|target| target.trim().to_string())
            .filter(|target| !target.is_empty())
            .collect();
        if targets.is_empty() {
            return encode_error(
                id,
                "invalid_request",
                "workspace.mount_remote requires at least one non-empty target",
            );
        }

        let session_name = crate::session::active_name()
            .unwrap_or_else(|| crate::session::DEFAULT_SESSION_NAME.to_string());

        for target in &targets {
            let host_key = crate::remote::federation::id::HostKey::new(target, &session_name);
            if self.state.double_attach_conflict(&host_key) {
                tracing::warn!(%target, "federation mount requested but this host is already mounted");
                let event_tx = self.event_tx.clone();
                let target = target.clone();
                tokio::spawn(async move {
                    let _ = event_tx
                        .send(crate::events::AppEvent::FederationMountFailed {
                            target,
                            reason: "a federation mount for this host is already live"
                                .to_string(),
                        })
                        .await;
                });
                continue;
            }

            let event_tx = self.event_tx.clone();
            let task_target = target.clone();
            let task_session_name = session_name.clone();
            tokio::spawn(async move {
                let result = crate::remote::prepare_and_mount_federation_target(
                    task_target.clone(),
                    task_session_name,
                )
                .await;
                let event = match result {
                    Ok(outcome) => crate::events::AppEvent::FederationMountReady(Box::new(
                        crate::events::FederationMountReady {
                            target: task_target,
                            mirror: outcome.mirror,
                            generation: outcome.generation,
                            tunnel_guard: outcome.tunnel_guard,
                            tunnel_reader: outcome.tunnel_reader,
                            tunnel_writer: outcome.tunnel_writer,
                        },
                    )),
                    Err(err) => crate::events::AppEvent::FederationMountFailed {
                        target: task_target,
                        reason: err.to_string(),
                    },
                };
                let _ = event_tx.send(event).await;
            });
        }

        encode_success(id, ResponseResult::WorkspaceMountRemoteRequested { targets })
    }

    /// `AppEvent::FederationMountReady` handler — runs inside `App::run`'s
    /// own tick, so it owns `&mut App` and can call
    /// `materialize_federation_mount` (session.rs's exact
    /// materialize-then-move-router disposition, relocated here). Records a
    /// mount-time snapshot in `AppState.remote_mirror` for bookkeeping
    /// (`double_attach_conflict`), then hands the live-syncing mirror off to
    /// a spawned drive task exactly like `run_federated_session` does.
    #[cfg(unix)]
    pub(crate) fn handle_federation_mount_ready(
        &mut self,
        ready: crate::events::FederationMountReady,
    ) {
        let crate::events::FederationMountReady {
            target,
            mirror,
            generation,
            tunnel_guard,
            tunnel_reader,
            tunnel_writer,
        } = ready;

        if self.state.begin_federation_mount(mirror.clone()).is_err() {
            tracing::warn!(%target, "federation mount ready but this host is already mounted; dropping");
            return;
        }
        let host_key = mirror.mount().host_key.clone();

        let (out_tx, writer_handle) =
            crate::remote::federation::client::spawn_mount_writer(tunnel_writer);
        let (inbound_clip_tx, _inbound_clip_rx) =
            tokio::sync::mpsc::channel::<crate::remote::federation::protocol::ClipboardMessage>(64);
        let (outbound_clip_tx, _outbound_clip_rx) = tokio::sync::mpsc::unbounded_channel::<
            crate::remote::federation::protocol::ClipboardMessage,
        >();

        let mut router = crate::remote::federation::client::TerminalChannelRouter::new();
        let opened = match self.materialize_federation_mount(
            &mirror,
            &mut router,
            &out_tx,
            &outbound_clip_tx,
        ) {
            Ok(opened) => opened,
            Err(err) => {
                tracing::warn!(%target, %err, "failed to materialize federation mount");
                self.state.end_federation_mount(&host_key);
                // Mirror the success path's teardown order below (drop
                // `out_tx` first so the writer task drains and exits,
                // bounded so a half-open peer can never hang this, then
                // kill the ssh child) instead of dropping `out_tx` /
                // `writer_handle` / `tunnel_guard` un-awaited in whatever
                // order they happen to be declared in.
                tokio::spawn(async move {
                    drop(out_tx);
                    let _ =
                        tokio::time::timeout(std::time::Duration::from_secs(2), writer_handle)
                            .await;
                    drop(tunnel_guard);
                });
                return;
            }
        };
        let _ = opened;

        self.render_dirty.store(true, Ordering::Release);
        self.render_notify.notify_one();

        let event_hub = self.event_hub.clone();
        let event_tx = self.event_tx.clone();
        let mut mirror_task = mirror;
        let drive_host_key = host_key.clone();
        let drive_target = target.clone();
        tokio::spawn(async move {
            let mut reader = tunnel_reader;
            let outcome = crate::remote::federation::client::drive_mount_channel(
                &mut reader,
                &mut mirror_task,
                generation,
                &event_hub,
                &mut router,
                &inbound_clip_tx,
            )
            .await;
            match &outcome {
                Ok(outcome) => {
                    tracing::info!(?outcome, "federated mount ended");
                }
                Err(err) => {
                    tracing::warn!(%err, "federated mount I/O error");
                }
            }
            // Teardown mirrors `run_federated_session`: drop the writer
            // sender first so the writer task drains and exits, bounded so
            // a half-open peer can never hang this task, then kill the ssh
            // child regardless. This runs before the `FederationMountEnded`
            // send below so the registry entry (freed by the handler that
            // processes that event) is only released once the old
            // connection is actually dying — otherwise a remount to the
            // same host could start a second live connection while the old
            // ssh child might still be alive.
            drop(out_tx);
            let _ = tokio::time::timeout(std::time::Duration::from_secs(2), writer_handle).await;
            drop(tunnel_guard);

            if let Some(reason) =
                crate::remote::federation::client::drive_outcome_ended_reason(&outcome)
            {
                let _ = event_tx
                    .send(crate::events::AppEvent::FederationMountEnded {
                        host_key: drive_host_key,
                        generation,
                        target: drive_target,
                        reason,
                    })
                    .await;
            }
        });
    }

    /// `AppEvent::FederationMountFailed` handler: surfaces a sidebar notice
    /// through the existing toast mechanism — local session and server
    /// daemon stay up unaffected (requirement 3).
    #[cfg(unix)]
    pub(crate) fn handle_federation_mount_failed(&mut self, target: String, reason: String) {
        tracing::warn!(%target, %reason, "federation mount failed");
        match self.state.toast_config.delivery {
            crate::config::ToastDelivery::Herdr => {
                self.state.toast = Some(crate::app::state::ToastNotification {
                    kind: ToastKind::NeedsAttention,
                    title: format!("federated mount to {target} failed"),
                    context: reason,
                    position: None,
                    target: None,
                });
            }
            crate::config::ToastDelivery::Terminal | crate::config::ToastDelivery::System
                if self.local_terminal_notifications =>
            {
                let notify = match self.state.toast_config.delivery {
                    crate::config::ToastDelivery::Terminal => {
                        crate::terminal_notify::show_notification
                    }
                    crate::config::ToastDelivery::System => {
                        crate::platform::show_desktop_notification
                    }
                    _ => unreachable!("toast delivery was matched above"),
                };
                let _ = notify(&format!("federated mount to {target} failed"), Some(&reason));
            }
            _ => {}
        }
        self.render_dirty.store(true, Ordering::Release);
        self.render_notify.notify_one();
    }

    /// `AppEvent::FederationMountEnded` handler: the mount's drive task
    /// exited for a session-ending reason (link closed, faulted, or an I/O
    /// error) — tear down the registry entry and the workspaces it
    /// materialized. Fenced on `generation` so a stale ended-notice from a
    /// drive task superseded by a fresh remount can never nuke the newer
    /// mount (the race the generation field exists to prevent).
    #[cfg(unix)]
    pub(crate) fn handle_federation_mount_ended(
        &mut self,
        host_key: crate::remote::federation::id::HostKey,
        generation: u64,
        target: String,
        reason: String,
    ) {
        if self
            .state
            .remote_mirrors
            .get(&host_key)
            .map(|mirror| mirror.mount().mount_generation)
            != Some(generation)
        {
            tracing::debug!(
                %target,
                generation,
                "federation mount ended notice for a superseded generation; ignoring"
            );
            return;
        }

        self.state.end_federation_mount(&host_key);

        let space_key = format!("federation:{}", host_key.as_str());
        let Some(idx) = self
            .state
            .workspaces
            .iter()
            .position(|ws| ws.worktree_space().is_some_and(|space| space.key == space_key))
        else {
            tracing::warn!(%target, %reason, "federation mount ended but no workspaces to remove");
            self.render_dirty.store(true, Ordering::Release);
            self.render_notify.notify_one();
            return;
        };

        let closing_indices = self.state.close_indices_for(idx);
        let closing_ids: std::collections::HashSet<String> = closing_indices
            .iter()
            .filter_map(|&i| self.state.workspaces.get(i).map(|ws| ws.id.clone()))
            .collect();
        let closing: Vec<_> = closing_indices
            .iter()
            .map(|&i| (self.public_workspace_id(i), self.workspace_info(i)))
            .collect();

        // Capture the user's actual focus by identity (not index — indices
        // shift once the closing workspaces are removed) so this background
        // event doesn't steal focus onto whichever workspace the close-clamp
        // happens to land on when the user was looking at something else.
        let previously_selected_id = self
            .state
            .workspaces
            .get(self.state.selected)
            .filter(|ws| !closing_ids.contains(&ws.id))
            .map(|ws| ws.id.clone());

        self.state.selected = idx;
        self.state.close_selected_workspace();
        self.shutdown_detached_terminal_runtimes();

        if let Some(prev_id) = previously_selected_id {
            if let Some(new_idx) = self.state.workspaces.iter().position(|ws| ws.id == prev_id) {
                self.state.switch_workspace(new_idx);
            }
        }

        for (workspace_id, workspace) in closing {
            self.emit_event(EventEnvelope {
                event: EventKind::WorkspaceClosed,
                data: EventData::WorkspaceClosed {
                    workspace_id,
                    workspace: Some(workspace),
                },
            });
        }

        self.render_dirty.store(true, Ordering::Release);
        self.render_notify.notify_one();
    }

    pub(super) fn handle_workspace_list(&mut self, id: String) -> String {
        encode_success(
            id,
            ResponseResult::WorkspaceList {
                workspaces: self.workspace_list_info(),
            },
        )
    }

    pub(super) fn handle_workspace_get(&mut self, id: String, target: WorkspaceTarget) -> String {
        let Some(index) = self.parse_workspace_id(&target.workspace_id) else {
            return workspace_not_found(id, &target.workspace_id);
        };
        let Some(_) = self.state.workspaces.get(index) else {
            return workspace_not_found(id, &target.workspace_id);
        };

        encode_success(
            id,
            ResponseResult::WorkspaceInfo {
                workspace: self.workspace_info(index),
            },
        )
    }

    pub(super) fn handle_workspace_create(
        &mut self,
        id: String,
        params: WorkspaceCreateParams,
    ) -> String {
        let cwd = params.cwd.map(PathBuf::from).unwrap_or_else(|| {
            let follow_cwd = self.workspace_creation_source().and_then(|ws_idx| {
                self.focused_pane_cwd_in_workspace(ws_idx)
                    .or_else(|| self.seed_cwd_from_workspace(ws_idx))
            });
            self.resolve_new_terminal_cwd(follow_cwd)
        });
        let extra_env = match super::env::normalize_launch_env(params.env) {
            Ok(env) => env,
            Err((code, message)) => return encode_error(id, &code, message),
        };
        match self.create_workspace_with_launch_env(cwd, params.focus, extra_env) {
            Ok(index) => {
                if let Some(label) = params.label {
                    if let Some(workspace) = self.state.workspaces.get_mut(index) {
                        workspace.set_custom_name(label);
                        crate::logging::workspace_renamed(&workspace.id);
                    }
                }
                self.emit_workspace_open_events(index);
                encode_success(
                    id,
                    self.workspace_created_result(index)
                        .expect("new workspace should produce a complete create response"),
                )
            }
            Err(err) => encode_error(id, "workspace_create_failed", err.to_string()),
        }
    }

    pub(super) fn handle_workspace_focus(&mut self, id: String, target: WorkspaceTarget) -> String {
        let Some(index) = self.parse_workspace_id(&target.workspace_id) else {
            return workspace_not_found(id, &target.workspace_id);
        };
        if self.state.workspaces.get(index).is_none() {
            return workspace_not_found(id, &target.workspace_id);
        }
        self.state.switch_workspace(index);

        encode_success(
            id,
            ResponseResult::WorkspaceInfo {
                workspace: self.workspace_info(index),
            },
        )
    }

    pub(super) fn handle_workspace_rename(
        &mut self,
        id: String,
        params: WorkspaceRenameParams,
    ) -> String {
        let Some(index) = self.parse_workspace_id(&params.workspace_id) else {
            return workspace_not_found(id, &params.workspace_id);
        };
        let Some(ws) = self.state.workspaces.get_mut(index) else {
            return workspace_not_found(id, &params.workspace_id);
        };
        ws.set_custom_name(params.label.clone());
        crate::logging::workspace_renamed(&ws.id);
        self.schedule_session_save();
        self.emit_event(EventEnvelope {
            event: EventKind::WorkspaceRenamed,
            data: EventData::WorkspaceRenamed {
                workspace_id: self.public_workspace_id(index),
                label: params.label,
            },
        });

        encode_success(
            id,
            ResponseResult::WorkspaceInfo {
                workspace: self.workspace_info(index),
            },
        )
    }

    pub(super) fn handle_workspace_move(
        &mut self,
        id: String,
        params: WorkspaceMoveParams,
    ) -> String {
        let Some(index) = self.parse_workspace_id(&params.workspace_id) else {
            return workspace_not_found(id, &params.workspace_id);
        };
        if self.state.workspaces.get(index).is_none() {
            return workspace_not_found(id, &params.workspace_id);
        }
        if params.insert_index > self.state.workspaces.len() {
            return encode_error(
                id,
                "workspace_move_failed",
                format!("insert_index {} is out of bounds", params.insert_index),
            );
        }

        let workspace_id = self.public_workspace_id(index);
        let insert_index = params.insert_index;
        let moved = self.state.move_workspace(index, insert_index);
        let workspaces = self.workspace_list_info();
        if moved {
            self.emit_event(EventEnvelope {
                event: EventKind::WorkspaceMoved,
                data: EventData::WorkspaceMoved {
                    workspace_id,
                    insert_index,
                    workspaces: workspaces.clone(),
                },
            });
        }

        encode_success(id, ResponseResult::WorkspaceList { workspaces })
    }

    pub(super) fn handle_workspace_report_metadata(
        &mut self,
        id: String,
        params: WorkspaceReportMetadataParams,
    ) -> String {
        let Some(index) = self.parse_workspace_id(&params.workspace_id) else {
            return workspace_not_found(id, &params.workspace_id);
        };
        let source = match normalize_metadata_source(params.source) {
            Ok(source) => source,
            Err(message) => return encode_error(id, "invalid_metadata_source", message),
        };
        let ttl = match normalize_metadata_ttl(params.ttl_ms) {
            Ok(ttl) => ttl,
            Err(message) => return encode_error(id, "invalid_metadata_ttl", message),
        };
        let tokens = match super::super::api_helpers::normalize_metadata_tokens(params.tokens) {
            Ok(tokens) => tokens,
            Err(message) => return encode_error(id, "invalid_metadata_token", message),
        };
        let Some(workspace) = self.state.workspaces.get_mut(index) else {
            return workspace_not_found(id, &params.workspace_id);
        };
        if !crate::metadata_tokens::sequence_is_fresh(
            &workspace.metadata_token_sequences,
            &source,
            params.seq,
        ) {
            return encode_success(id, ResponseResult::Ok {});
        }
        if workspace.metadata_tokens.key_count_after_patch(&tokens)
            > super::super::api_helpers::MAX_METADATA_TOKEN_KEYS_PER_RESOURCE
        {
            return encode_error(
                id,
                "metadata_token_limit",
                format!(
                    "workspace metadata may contain at most {} tokens",
                    super::super::api_helpers::MAX_METADATA_TOKEN_KEYS_PER_RESOURCE
                ),
            );
        }
        match crate::metadata_tokens::accept_sequence(
            &mut workspace.metadata_token_sequences,
            &source,
            params.seq,
        ) {
            Ok(true) => {}
            Ok(false) => return encode_success(id, ResponseResult::Ok {}),
            Err(()) => {
                return encode_error(
                    id,
                    "metadata_sequence_source_limit",
                    format!(
                        "workspace metadata may track at most {} sequenced sources",
                        crate::metadata_tokens::MAX_SEQUENCE_SOURCES
                    ),
                );
            }
        }
        let changed = workspace
            .metadata_tokens
            .patch(tokens, ttl, std::time::Instant::now());
        if changed {
            self.sync_agent_metadata_deadline();
            self.emit_workspace_token_updated(index);
        }
        encode_success(id, ResponseResult::Ok {})
    }

    pub(super) fn handle_workspace_close(&mut self, id: String, target: WorkspaceTarget) -> String {
        let Some(index) = self.parse_workspace_id(&target.workspace_id) else {
            return workspace_not_found(id, &target.workspace_id);
        };
        if self.state.workspaces.get(index).is_none() {
            return workspace_not_found(id, &target.workspace_id);
        }
        let workspace_id = self.public_workspace_id(index);
        let workspace = self.workspace_info(index);
        let pane_ids = self
            .state
            .workspaces
            .get(index)
            .map(|ws| {
                ws.tabs
                    .iter()
                    .flat_map(|tab| tab.layout.pane_ids())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        self.state.selected = index;
        self.state.close_selected_workspace();
        self.state.remove_plugin_pane_records(pane_ids);
        self.shutdown_detached_terminal_runtimes();
        self.emit_event(EventEnvelope {
            event: EventKind::WorkspaceClosed,
            data: EventData::WorkspaceClosed {
                workspace_id,
                workspace: Some(workspace),
            },
        });

        encode_success(id, ResponseResult::Ok {})
    }

    fn workspace_list_info(&self) -> Vec<crate::api::schema::WorkspaceInfo> {
        self.state
            .workspaces
            .iter()
            .enumerate()
            .map(|(idx, _)| self.workspace_info(idx))
            .collect()
    }
}

fn workspace_not_found(id: String, workspace_id: &str) -> String {
    encode_error(
        id,
        "workspace_not_found",
        format!("workspace {workspace_id} not found"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{api::schema::SuccessResponse, config::Config, workspace::Workspace};

    // `new_cwd = follow` must anchor on the focused pane for every creation
    // surface. Splits and tabs already do; a new workspace must follow the
    // focused pane too, not the source workspace's first-tab root pane.
    #[tokio::test]
    async fn workspace_create_follows_focused_pane_cwd_not_first_tab_root() {
        use super::super::test_support::{exiting_test_command, shutdown_test_runtimes};
        use crate::config::ShellModeConfig;

        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.default_shell = exiting_test_command().into();
        app.state.shell_mode = ShellModeConfig::NonLogin;
        app.state.workspaces = vec![Workspace::test_new("spaces")];
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.ensure_test_terminals();

        // Second tab becomes the focused pane, away from tab 1's root pane.
        let response = app.handle_tab_create(
            "tab".into(),
            crate::api::schema::TabCreateParams {
                workspace_id: None,
                cwd: None,
                focus: true,
                label: None,
                env: Default::default(),
            },
        );
        let _: SuccessResponse = serde_json::from_str(&response).unwrap();
        // Drop runtimes so cwd resolution deterministically uses cached state.
        shutdown_test_runtimes(&mut app);

        let focused_cwd = std::env::temp_dir().join(format!(
            "herdr-ws-follow-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&focused_cwd).unwrap();
        let ws = &app.state.workspaces[0];
        let root_cwd = ws.identity_cwd.clone();
        let focused_pane = ws.focused_pane_id().unwrap();
        assert_ne!(focused_pane, ws.tabs[0].root_pane);
        let terminal_id = ws.terminal_id(focused_pane).cloned().unwrap();
        app.state.terminals.get_mut(&terminal_id).unwrap().cwd = focused_cwd.clone();

        let response = app.handle_workspace_create(
            "req".into(),
            WorkspaceCreateParams {
                cwd: None,
                focus: false,
                label: None,
                env: Default::default(),
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert!(matches!(
            success.result,
            ResponseResult::WorkspaceCreated { .. }
        ));
        let created_cwd = &app.state.workspaces[1].identity_cwd;
        assert_eq!(
            crate::worktree::canonical_or_original(created_cwd),
            crate::worktree::canonical_or_original(&focused_cwd)
        );
        assert_ne!(
            crate::worktree::canonical_or_original(created_cwd),
            crate::worktree::canonical_or_original(&root_cwd)
        );
        shutdown_test_runtimes(&mut app);
        let _ = std::fs::remove_dir_all(&focused_cwd);
    }

    fn app_with_linked_worktree() -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![Workspace::test_new("issue")];
        app.state.workspaces[0].worktree_space = Some(crate::workspace::WorktreeSpaceMembership {
            key: "repo-key".into(),
            label: "herdr".into(),
            repo_root: "/repo/herdr".into(),
            checkout_path: "/repo/herdr-issue".into(),
            is_linked_worktree: true,
        });
        app
    }

    #[test]
    fn api_workspace_close_closes_linked_worktree_workspace_only() {
        let mut app = app_with_linked_worktree();

        let response = app.handle_workspace_close(
            "req".into(),
            WorkspaceTarget {
                workspace_id: app.state.workspaces[0].id.clone(),
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(success.id, "req");
        assert_eq!(app.state.request_remove_linked_worktree, None);
        assert!(app.state.workspaces.is_empty());
    }

    #[test]
    fn api_workspace_close_event_includes_final_worktree_snapshot() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&Config::default(), true, None, api_rx, event_hub.clone());
        app.state.workspaces = app_with_linked_worktree().state.workspaces;
        let workspace_id = app.state.workspaces[0].id.clone();

        let response = app.handle_workspace_close(
            "req".into(),
            WorkspaceTarget {
                workspace_id: workspace_id.clone(),
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(success.id, "req");
        let events = event_hub.events_after(0);
        assert!(events.iter().any(|(_, event)| {
            matches!(
                &event.data,
                EventData::WorkspaceClosed {
                    workspace_id: closed_id,
                    workspace: Some(workspace),
                } if closed_id == &workspace_id
                    && workspace
                        .worktree
                        .as_ref()
                        .is_some_and(|worktree| worktree.is_linked_worktree)
            )
        }));
    }

    #[test]
    fn workspace_metadata_tokens_patch_clear_and_emit_snapshot() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&Config::default(), true, None, api_rx, event_hub.clone());
        app.state.workspaces = vec![Workspace::test_new("one")];
        let workspace_id = app.public_workspace_id(0);

        for (tokens, expected) in [
            (
                std::collections::HashMap::from([
                    ("summary".into(), Some("reviewing auth".into())),
                    ("jj_status".into(), Some("2 changes".into())),
                ]),
                std::collections::HashMap::from([
                    ("summary".into(), "reviewing auth".into()),
                    ("jj_status".into(), "2 changes".into()),
                ]),
            ),
            (
                std::collections::HashMap::from([
                    ("summary".into(), Some("done".into())),
                    ("jj_status".into(), None),
                ]),
                std::collections::HashMap::from([("summary".into(), "done".into())]),
            ),
        ] {
            let response = app.handle_api_request(crate::api::schema::Request {
                id: "req".into(),
                method: crate::api::schema::Method::WorkspaceReportMetadata(
                    WorkspaceReportMetadataParams {
                        workspace_id: workspace_id.clone(),
                        source: "user:test".into(),
                        tokens,
                        seq: None,
                        ttl_ms: None,
                    },
                ),
            });
            let success: SuccessResponse = serde_json::from_str(&response).unwrap();
            assert_eq!(success.result, ResponseResult::Ok {});
            assert_eq!(app.workspace_info(0).tokens, expected);
        }

        assert!(event_hub.events_after(0).iter().any(|(_, event)| matches!(
            &event.data,
            EventData::WorkspaceMetadataUpdated { workspace }
                if workspace.tokens.get("summary").map(String::as_str) == Some("done")
                    && !workspace.tokens.contains_key("jj_status")
        )));
    }

    #[test]
    fn workspace_token_ttl_expires_through_runtime_and_emits_update() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&Config::default(), true, None, api_rx, event_hub.clone());
        app.state.workspaces = vec![Workspace::test_new("one")];
        let workspace_id = app.public_workspace_id(0);
        let response = app.handle_workspace_report_metadata(
            "req".into(),
            WorkspaceReportMetadataParams {
                workspace_id,
                source: "user:test".into(),
                tokens: std::collections::HashMap::from([(
                    "summary".into(),
                    Some("temporary".into()),
                )]),
                seq: None,
                ttl_ms: Some(1),
            },
        );
        let _: SuccessResponse = serde_json::from_str(&response).unwrap();
        let deadline = app.agent_metadata_deadline.expect("token deadline");

        app.expire_metadata_at(deadline, deadline);

        assert!(app.workspace_info(0).tokens.is_empty());
        assert!(event_hub.events_after(0).iter().any(|(_, event)| matches!(
            &event.data,
            EventData::WorkspaceMetadataUpdated { workspace } if workspace.tokens.is_empty()
        )));
    }

    #[test]
    fn api_workspace_move_reorders_workspaces() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&Config::default(), true, None, api_rx, event_hub.clone());
        app.state.workspaces = vec![
            Workspace::test_new("one"),
            Workspace::test_new("two"),
            Workspace::test_new("three"),
        ];
        app.state.active = Some(0);
        app.state.selected = 0;
        let moved_id = app.public_workspace_id(0);

        let response = app.handle_workspace_move(
            "req".into(),
            WorkspaceMoveParams {
                workspace_id: moved_id.clone(),
                insert_index: 3,
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::WorkspaceList { workspaces } = success.result else {
            panic!("expected workspace list");
        };
        assert_eq!(workspaces[2].workspace_id, moved_id);
        assert_eq!(app.state.workspaces[2].display_name(), "one");
        let events = event_hub.events_after(0);
        assert!(events.iter().any(|(_, event)| {
            matches!(
                &event.data,
                EventData::WorkspaceMoved {
                    workspace_id,
                    insert_index: 3,
                    workspaces,
                } if workspace_id == &moved_id
                    && workspaces[2].workspace_id == moved_id
            )
        }));
    }

    #[test]
    fn api_workspace_move_noop_does_not_emit_event() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&Config::default(), true, None, api_rx, event_hub.clone());
        app.state.workspaces = vec![Workspace::test_new("one"), Workspace::test_new("two")];
        let moved_id = app.public_workspace_id(0);

        let response = app.handle_workspace_move(
            "req".into(),
            WorkspaceMoveParams {
                workspace_id: moved_id.clone(),
                insert_index: 1,
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::WorkspaceList { workspaces } = success.result else {
            panic!("expected workspace list");
        };
        assert_eq!(workspaces[0].workspace_id, moved_id);
        assert!(event_hub.events_after(0).is_empty());
    }

    #[cfg(unix)]
    fn test_federation_mirror(target: &str) -> crate::remote::federation::reducer::RemoteMirror {
        test_federation_mirror_at_generation(target, 1)
    }

    #[cfg(unix)]
    fn test_federation_mirror_at_generation(
        target: &str,
        generation: u64,
    ) -> crate::remote::federation::reducer::RemoteMirror {
        use crate::remote::federation::id::{HostKey, Mount, ServerInstanceId};
        crate::remote::federation::reducer::RemoteMirror::new(Mount {
            host_key: HostKey::new(target, "s1"),
            server_instance_id: ServerInstanceId("inst-1".to_string()),
            mount_generation: generation,
        })
    }

    /// A mirror carrying one materializable workspace/tab/pane, so
    /// `handle_federation_mount_ready` actually produces a federation
    /// workspace (a bare `test_federation_mirror` has no panes to
    /// materialize). Mirrors `federation_materialization_tests`'
    /// `two_pane_snapshot` fixture in `creation.rs`, narrowed to one pane.
    #[cfg(unix)]
    fn test_federation_mirror_with_workspace(
        target: &str,
        generation: u64,
    ) -> crate::remote::federation::reducer::RemoteMirror {
        use crate::api::schema::common::AgentStatus;
        use crate::api::schema::session::SessionSnapshot;
        use crate::api::schema::{PaneInfo as RemotePaneInfo, TabInfo as RemoteTabInfo, WorkspaceInfo};
        use crate::remote::federation::protocol::EventCursor;

        let mut mirror = test_federation_mirror_at_generation(target, generation);
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
                pane_count: 1,
                tab_count: 1,
                active_tab_id: "w1-tab".to_string(),
                agent_status: AgentStatus::Idle,
                tokens: Default::default(),
                worktree: None,
            }],
            tabs: vec![RemoteTabInfo {
                tab_id: "w1-tab".to_string(),
                workspace_id: "w1".to_string(),
                number: 1,
                label: "remote tab".to_string(),
                focused: false,
                pane_count: 1,
                agent_status: AgentStatus::Idle,
            }],
            panes: vec![RemotePaneInfo {
                pane_id: "p1".to_string(),
                terminal_id: "t1".to_string(),
                workspace_id: "w1".to_string(),
                tab_id: "w1-tab".to_string(),
                focused: false,
                cwd: Some("/home/alice/project".to_string()),
                foreground_cwd: None,
                label: Some("remote pane".to_string()),
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
            }],
            layouts: Vec::new(),
            agents: Vec::new(),
        };
        mirror.apply_snapshot(&snapshot, EventCursor(0));
        mirror
    }

    /// Real spawned child (`cat`) so `ChildGuard`/`ChildStdout`/`ChildStdin`
    /// are the genuine types `handle_federation_mount_ready` expects —
    /// there is no fabricated stand-in for these process-backed types.
    #[cfg(unix)]
    async fn spawn_test_tunnel() -> (
        crate::remote::ChildGuard,
        tokio::process::ChildStdout,
        tokio::process::ChildStdin,
    ) {
        let (guard, stdout, stdin, _pid) = spawn_test_tunnel_with_pid().await;
        (guard, stdout, stdin)
    }

    /// Like `spawn_test_tunnel`, but also returns the child's OS pid so a
    /// test can kill it externally (the `ChildGuard`/reader/writer are all
    /// consumed by `handle_federation_mount_ready`, leaving no other handle
    /// to end the process from outside) — the mechanism
    /// `federation_mount_ended_wiring_link_closed_reaches_event_channel`
    /// uses to force the drive task's `read_frame` to observe EOF.
    #[cfg(unix)]
    async fn spawn_test_tunnel_with_pid() -> (
        crate::remote::ChildGuard,
        tokio::process::ChildStdout,
        tokio::process::ChildStdin,
        u32,
    ) {
        let mut child = tokio::process::Command::new("cat")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("spawn cat for test tunnel");
        let pid = child.id().expect("cat pid");
        let stdin = child.stdin.take().expect("cat stdin");
        let stdout = child.stdout.take().expect("cat stdout");
        (crate::remote::ChildGuard::for_test(child), stdout, stdin, pid)
    }

    // Phase-a TDD test 1: after a successful mount task completes,
    // `AppState.workspaces` (local) and `AppState.remote_mirror` (remote)
    // are both populated in the same instance — proves no more
    // "federated-alone" branch (REVISED Phase A reverses P9.2b).
    #[cfg(unix)]
    #[tokio::test]
    async fn coexistence_local_and_remote_render_together() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&Config::default(), true, None, api_rx, event_hub.clone());
        app.state.workspaces = vec![Workspace::test_new("local")];
        app.state.active = Some(0);
        app.state.selected = 0;

        let mirror = test_federation_mirror("remote-host");
        let (guard, tunnel_reader, tunnel_writer) = spawn_test_tunnel().await;

        app.handle_federation_mount_ready(crate::events::FederationMountReady {
            target: "remote-host".to_string(),
            mirror,
            generation: 1,
            tunnel_guard: guard,
            tunnel_reader,
            tunnel_writer,
        });

        assert_eq!(app.state.workspaces.len(), 1);
        assert!(!app.state.remote_mirrors.is_empty());
    }

    // Phase-a TDD test 2: mount failure keeps the local session alive — no
    // process-exit path, `AppState.workspaces` unchanged, sidebar notice
    // (toast) fired.
    #[cfg(unix)]
    #[tokio::test]
    async fn coexistence_mount_failure_keeps_local_session_alive() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&Config::default(), true, None, api_rx, event_hub.clone());
        app.state.workspaces = vec![Workspace::test_new("local")];
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.toast_config.delivery = crate::config::ToastDelivery::Herdr;

        app.handle_federation_mount_failed(
            "remote-host".to_string(),
            "federation dial failed: connection refused".to_string(),
        );

        assert_eq!(app.state.workspaces.len(), 1);
        assert!(app.state.remote_mirrors.is_empty());
        assert!(app.state.toast.is_some());
        assert!(app
            .state
            .toast
            .as_ref()
            .unwrap()
            .title
            .contains("remote-host"));
    }

    // Terminal/System delivery must never populate `state.toast` (that
    // field drives the Herdr in-app toast only) — it goes out through
    // `terminal_notify`/`platform::show_desktop_notification` instead, same
    // as every other Terminal/System notification.
    #[cfg(unix)]
    #[tokio::test]
    async fn mount_failure_terminal_delivery_calls_local_notify_when_enabled() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&Config::default(), true, None, api_rx, event_hub.clone());
        app.state.workspaces = vec![Workspace::test_new("local")];
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.toast_config.delivery = crate::config::ToastDelivery::Terminal;
        assert!(app.local_terminal_notifications);

        app.handle_federation_mount_failed(
            "remote-host".to_string(),
            "federation dial failed: connection refused".to_string(),
        );

        assert_eq!(app.state.workspaces.len(), 1);
        assert!(app.state.toast.is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn mount_failure_system_delivery_is_noop_when_local_notifications_disabled() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&Config::default(), true, None, api_rx, event_hub.clone());
        app.state.workspaces = vec![Workspace::test_new("local")];
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.toast_config.delivery = crate::config::ToastDelivery::System;
        app.local_terminal_notifications = false;

        app.handle_federation_mount_failed(
            "remote-host".to_string(),
            "federation dial failed: connection refused".to_string(),
        );

        assert_eq!(app.state.workspaces.len(), 1);
        assert!(app.state.toast.is_none());
    }

    // Phase-a TDD test 6: a mount task that produced an `Err` (the async
    // catch-unwind-free failure path `handle_workspace_mount_remote`
    // already routes through `AppEvent::FederationMountFailed`) never
    // reaches `handle_federation_mount_ready`/panics the process — asserted
    // at the type level: the spawned task's `match result` in
    // `handle_workspace_mount_remote` is exhaustive over `Result`, so a
    // `dial_and_mount` panic inside the spawned `tokio::task` is caught by
    // Tokio's own task boundary (a panicking task fails its `JoinHandle`,
    // it does not unwind into `App::run`) — the same isolation
    // `run_federated_session`'s drive-task `select!` arm already relies on
    // (session.rs's `Err(join_err) => ... "drive task aborted/panicked"`).
    // No separate `catch_unwind` wrapper is needed for a `tokio::spawn`ed
    // future; this test documents the isolation this relies on.
    // Phase B test 6/8: a duplicate `HostKey` target in the same
    // `workspace.mount_remote` request is rejected immediately (no SSH dial
    // spawned) with a per-host `FederationMountFailed` naming the host,
    // while the pre-existing mount stays untouched.
    #[cfg(unix)]
    #[tokio::test]
    async fn duplicate_host_key_target_is_isolated_and_named_in_failure_event() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&Config::default(), true, None, api_rx, event_hub);
        app.state.workspaces = vec![Workspace::test_new("local")];

        let session_name =
            crate::session::active_name().unwrap_or_else(|| crate::session::DEFAULT_SESSION_NAME.to_string());
        let mirror = crate::remote::federation::reducer::RemoteMirror::new(
            crate::remote::federation::id::Mount {
                host_key: crate::remote::federation::id::HostKey::new(
                    "already-mounted-host",
                    &session_name,
                ),
                server_instance_id: crate::remote::federation::id::ServerInstanceId(
                    "inst-1".to_string(),
                ),
                mount_generation: 1,
            },
        );
        app.state.begin_federation_mount(mirror).unwrap();

        let response = app.handle_workspace_mount_remote(
            "req".into(),
            WorkspaceMountRemoteParams {
                targets: vec!["already-mounted-host".to_string()],
                remote_keybindings: false,
            },
        );
        let _: SuccessResponse = serde_json::from_str(&response).unwrap();

        // The pre-existing mount is untouched (still exactly one entry).
        assert_eq!(app.state.remote_mirrors.len(), 1);

        // Drain the fire-and-forget failure event.
        let mut saw_failure = false;
        for _ in 0..10 {
            match tokio::time::timeout(std::time::Duration::from_millis(200), app.event_rx.recv())
                .await
            {
                Ok(Some(crate::events::AppEvent::FederationMountFailed { target, reason })) => {
                    assert_eq!(target, "already-mounted-host");
                    assert!(reason.contains("already"));
                    saw_failure = true;
                    break;
                }
                Ok(Some(_)) => continue,
                _ => break,
            }
        }
        assert!(saw_failure, "expected a FederationMountFailed event naming the duplicate host");
    }

    #[test]
    fn coexistence_mount_panic_isolated_by_tokio_task_boundary() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let outcome: Result<(), _> = rt.block_on(async {
            tokio::spawn(async { panic!("simulated dial+mount panic") })
                .await
                .map(|_: ()| ())
        });
        assert!(
            outcome.is_err(),
            "a panicking tokio::spawn task must fail its JoinHandle, not unwind the caller"
        );
    }

    // A mount's own drive task, not just an external caller, sends
    // `FederationMountEnded` once its `drive_mount_channel` loop ends for a
    // session-ending reason.
    #[cfg(unix)]
    #[tokio::test]
    async fn federation_mount_ended_wiring_link_closed_reaches_event_channel() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&Config::default(), true, None, api_rx, event_hub.clone());
        app.state.workspaces = vec![Workspace::test_new("local")];
        app.state.active = Some(0);
        app.state.selected = 0;

        let mirror = test_federation_mirror_with_workspace("remote-host", 1);
        let (guard, tunnel_reader, tunnel_writer, pid) = spawn_test_tunnel_with_pid().await;

        app.handle_federation_mount_ready(crate::events::FederationMountReady {
            target: "remote-host".to_string(),
            mirror,
            generation: 1,
            tunnel_guard: guard,
            tunnel_reader,
            tunnel_writer,
        });

        let host_key = crate::remote::federation::id::HostKey::new("remote-host", "s1");

        // Kill the tunnel's child so the drive task's `read_frame` observes
        // EOF and the loop exits with `DriveOutcome::LinkClosed`.
        unsafe {
            libc::kill(pid as libc::c_int, libc::SIGKILL);
        }

        // The event is sent only after the drive task's teardown (dropping
        // the writer sender, awaiting it with a bounded timeout, dropping
        // the tunnel guard) completes, so poll past several 200ms timeouts
        // rather than stopping at the first one — only a closed channel
        // (`Ok(None)`) means no event is coming.
        let mut saw_ended = false;
        for _ in 0..25 {
            match tokio::time::timeout(std::time::Duration::from_millis(200), app.event_rx.recv())
                .await
            {
                Ok(Some(crate::events::AppEvent::FederationMountEnded {
                    host_key: got_host_key,
                    generation,
                    ..
                })) => {
                    assert_eq!(got_host_key, host_key);
                    assert_eq!(generation, 1);
                    saw_ended = true;
                    break;
                }
                Ok(Some(_)) => continue,
                Ok(None) => break,
                Err(_) => continue,
            }
        }
        assert!(
            saw_ended,
            "expected a FederationMountEnded event after the tunnel's child died"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn federation_mount_ended_removes_workspaces_and_unmounts_registry() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&Config::default(), true, None, api_rx, event_hub.clone());
        app.state.workspaces = vec![Workspace::test_new("local")];
        app.state.active = Some(0);
        app.state.selected = 0;

        let mirror = test_federation_mirror_with_workspace("remote-host", 1);
        let (guard, tunnel_reader, tunnel_writer) = spawn_test_tunnel().await;
        app.handle_federation_mount_ready(crate::events::FederationMountReady {
            target: "remote-host".to_string(),
            mirror,
            generation: 1,
            tunnel_guard: guard,
            tunnel_reader,
            tunnel_writer,
        });

        let host_key = crate::remote::federation::id::HostKey::new("remote-host", "s1");
        assert!(app.state.remote_mirrors.contains_key(&host_key));
        assert_eq!(app.state.workspaces.len(), 2, "the federation mount must have materialized a workspace");

        app.handle_federation_mount_ended(
            host_key.clone(),
            1,
            "remote-host".to_string(),
            "link closed".to_string(),
        );

        assert!(app.state.remote_mirrors.is_empty());
        assert_eq!(app.state.workspaces.len(), 1);
        assert!(app
            .state
            .workspaces
            .iter()
            .all(|ws| ws.worktree_space().is_none()));
        assert!(event_hub
            .events_after(0)
            .iter()
            .any(|(_, event)| matches!(&event.data, EventData::WorkspaceClosed { .. })));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn federation_mount_ended_stale_generation_is_ignored() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&Config::default(), true, None, api_rx, event_hub.clone());
        app.state.workspaces = vec![Workspace::test_new("local")];
        app.state.active = Some(0);
        app.state.selected = 0;

        let mirror = test_federation_mirror_with_workspace("remote-host", 1);
        let (guard, tunnel_reader, tunnel_writer) = spawn_test_tunnel().await;
        app.handle_federation_mount_ready(crate::events::FederationMountReady {
            target: "remote-host".to_string(),
            mirror,
            generation: 1,
            tunnel_guard: guard,
            tunnel_reader,
            tunnel_writer,
        });
        assert_eq!(app.state.workspaces.len(), 2);

        let host_key = crate::remote::federation::id::HostKey::new("remote-host", "s1");
        // Simulate a completed remount that already replaced the gen-1 mirror
        // with a gen-2 one before the gen-1 drive task's stale ended-notice
        // arrives.
        app.state.end_federation_mount(&host_key);
        let remounted = test_federation_mirror_at_generation("remote-host", 2);
        app.state.begin_federation_mount(remounted).unwrap();

        app.handle_federation_mount_ended(
            host_key.clone(),
            1,
            "remote-host".to_string(),
            "stale link closed".to_string(),
        );

        assert_eq!(
            app.state
                .remote_mirrors
                .get(&host_key)
                .map(|mirror| mirror.mount().mount_generation),
            Some(2),
            "the stale gen-1 notice must not touch the fresh gen-2 registry entry"
        );
        assert_eq!(
            app.state.workspaces.len(),
            2,
            "the stale gen-1 notice must not remove workspaces materialized under a newer mount"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn federation_mount_ended_drains_detached_terminal_runtimes() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&Config::default(), true, None, api_rx, event_hub.clone());
        app.state.workspaces = vec![Workspace::test_new("local")];
        app.state.active = Some(0);
        app.state.selected = 0;

        let mirror = test_federation_mirror_with_workspace("remote-host", 1);
        let (guard, tunnel_reader, tunnel_writer) = spawn_test_tunnel().await;
        app.handle_federation_mount_ready(crate::events::FederationMountReady {
            target: "remote-host".to_string(),
            mirror,
            generation: 1,
            tunnel_guard: guard,
            tunnel_reader,
            tunnel_writer,
        });
        assert!(app.terminal_runtimes.len() > 0);

        let host_key = crate::remote::federation::id::HostKey::new("remote-host", "s1");
        app.handle_federation_mount_ended(
            host_key,
            1,
            "remote-host".to_string(),
            "link closed".to_string(),
        );

        assert_eq!(
            app.terminal_runtimes.len(),
            0,
            "terminal runtimes for removed federation panes must be actually shut down, not just queued"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn federation_mount_ended_preserves_user_focus_on_a_later_unrelated_workspace() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&Config::default(), true, None, api_rx, event_hub.clone());
        app.state.workspaces = vec![Workspace::test_new("local-a")];
        app.state.active = Some(0);
        app.state.selected = 0;

        let mirror = test_federation_mirror_with_workspace("remote-host", 1);
        let (guard, tunnel_reader, tunnel_writer) = spawn_test_tunnel().await;
        app.handle_federation_mount_ready(crate::events::FederationMountReady {
            target: "remote-host".to_string(),
            mirror,
            generation: 1,
            tunnel_guard: guard,
            tunnel_reader,
            tunnel_writer,
        });
        assert_eq!(app.state.workspaces.len(), 2, "local-a plus the materialized federation workspace");

        // A workspace created after the federation mount sits after the
        // federation group in index order.
        app.state.workspaces.push(Workspace::test_new("local-b"));
        let local_b_id = app.state.workspaces[2].id.clone();
        app.state.active = Some(2);
        app.state.selected = 2;

        let host_key = crate::remote::federation::id::HostKey::new("remote-host", "s1");
        app.handle_federation_mount_ended(
            host_key,
            1,
            "remote-host".to_string(),
            "link closed".to_string(),
        );

        assert_eq!(app.state.workspaces.len(), 2, "local-a and local-b remain");
        assert_eq!(
            app.state.workspaces.get(app.state.selected).map(|ws| ws.id.clone()),
            Some(local_b_id.clone()),
            "selection must still point at local-b, not wherever the federation group's clamp landed"
        );
        assert_eq!(
            app.state.active,
            Some(app.state.selected),
            "active must track the restored selection"
        );
    }
}
