use std::path::PathBuf;

use crate::api::schema::{
    EventData, EventEnvelope, EventKind, ResponseResult, WorkspaceCloseParams,
    WorkspaceCreateParams, WorkspaceMountRemoteParams, WorkspaceMoveBlockParams,
    WorkspaceMoveParams, WorkspaceRenameParams, WorkspaceReportMetadataParams, WorkspaceTarget,
};
use crate::app::App;
#[cfg(unix)]
use crate::app::ToastKind;

use super::super::api_helpers::{normalize_metadata_source, normalize_metadata_ttl};
use super::responses::{encode_error, encode_success};

/// Mints a fresh, process-wide-unique `WorkspaceCreateRequest::request_id`.
/// Its own counter, separate from the split/close ones in `api/panes.rs`, for
/// the same reason those two are separate from each other: two kinds minted
/// "at the same time" must never collide. A bare counter is enough because
/// the response is fire-and-forget (see `App::dispatch_remote_workspace_create`).
fn next_remote_workspace_create_request_id() -> u64 {
    static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

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
    ///
    /// Phase 01 (server-side target validation): every target is validated
    /// with `crate::remote::validate_remote_target` (the same rule the CLI's
    /// `--remote` flag already enforces) and checked against
    /// `crate::remote::is_local_target` before any `tokio::spawn`. This API
    /// method is reachable by anything that can open `herdr.sock`, not just
    /// the CLI parser, so a leading-`-` target (e.g.
    /// `-oProxyCommand=<cmd>`) must be rejected here too — `ssh` parses a
    /// leading-`-` argv element as an option, not a hostname. Rejections are
    /// synchronous `invalid_request` errors returned before any dial is
    /// spawned, so a caller (dialog or CLI) sees the failure immediately.
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

        for target in &targets {
            if let Err(err) = crate::remote::validate_remote_target(target) {
                tracing::warn!(?target, %err, "rejected workspace.mount_remote target");
                return encode_error(
                    id,
                    "invalid_request",
                    format!("invalid remote target {target:?}: {err}"),
                );
            }
            if crate::remote::is_local_target(target) {
                tracing::warn!(?target, "rejected workspace.mount_remote target");
                return encode_error(
                    id,
                    "invalid_request",
                    "workspace.mount_remote requires a remote target; \"localhost\" is not a remote host",
                );
            }
        }

        let session_name = crate::session::active_name()
            .unwrap_or_else(|| crate::session::DEFAULT_SESSION_NAME.to_string());

        for target in &targets {
            let host_key = crate::remote::federation::id::HostKey::new(target, &session_name);
            if self.state.double_attach_conflict(&host_key) {
                tracing::warn!(
                    ?target,
                    "federation mount requested but this host is already mounted"
                );
                let event_tx = self.event_tx.clone();
                let target = target.clone();
                tokio::spawn(async move {
                    let _ = event_tx
                        .send(crate::events::AppEvent::FederationMountFailed {
                            target,
                            reason: "a federation mount for this host is already live".to_string(),
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

        encode_success(
            id,
            ResponseResult::WorkspaceMountRemoteRequested { targets },
        )
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
        let (outbound_clip_tx, outbound_clip_rx) = tokio::sync::mpsc::unbounded_channel::<
            crate::remote::federation::protocol::ClipboardMessage,
        >();
        // Drain remote-origin OSC 52 clipboard writes emitted by the mirror
        // emulator into the same `AppEvent::ClipboardWrite` path every local
        // pane uses. That path is the only one that reaches the operator's
        // system clipboard in BOTH run modes: monolithic writes it directly,
        // while the headless server forwards it to the foreground client as
        // `ServerMessage::Clipboard`. Writing the clipboard here instead would
        // target the server host, not the operator's machine.
        //
        // Without this receiver the mirror's sender had no consumer, so a TUI
        // running in a federated pane could report "copied" while the bytes
        // were dropped on the floor. Ends when the mount drops its senders.
        //
        // The mount's `host_key` is used as the event origin rather than the
        // channel's generic `"remote"` tag so the copy toast can name the host
        // that wrote the clipboard, and so `remote.accept_clipboard_writes` has
        // something meaningful to log when it refuses one. Policy is applied by
        // the event handler, not here, so `reload_config` takes effect without
        // remounting.
        {
            let clipboard_event_tx = self.event_tx.clone();
            // `HostKey` is `user@ip#session-discriminator`; the toast shows the
            // address only, since the discriminator is noise to a reader.
            let clipboard_origin = host_key
                .as_str()
                .split_once('#')
                .map(|(address, _)| address)
                .unwrap_or_else(|| host_key.as_str())
                .to_string();
            tokio::spawn(
                crate::remote::federation::pane_source::apply_remote_clipboard_writes(
                    outbound_clip_rx,
                    move |_origin_tag, payload| {
                        if clipboard_event_tx
                            .try_send(crate::events::AppEvent::ClipboardWrite {
                                content: payload.to_vec(),
                                origin: Some(clipboard_origin.clone()),
                            })
                            .is_err()
                        {
                            tracing::warn!(
                                "dropped remote clipboard write: event channel full or closed"
                            );
                        }
                    },
                ),
            );
        }

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
                    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), writer_handle)
                        .await;
                    drop(tunnel_guard);
                });
                return;
            }
        };
        let _ = opened;

        self.render_dirty.request_generic();
        self.render_notify.notify_one();

        let event_hub = self.event_hub.clone();
        let event_tx = self.event_tx.clone();
        // Read before the mirror moves into the drive task: this is the value
        // that tells an end-notice from *this* connection apart from one a
        // later remount to the same host has already superseded.
        let connection_epoch = mirror.connection_epoch();
        let mut mirror_task = mirror;
        let drive_host_key = host_key.clone();
        let drive_target = target.clone();
        // Captured once at mount time (same simplification
        // `materialize_federation_mount`/`build_remote_pane` already make for
        // mount-time panes) so a later `SplitPaneResponse::Created` can spawn
        // a real local `TerminalRuntime` for the new remote pane without
        // needing `&mut App` inside the drive task.
        let (rows, cols) = self.state.estimate_pane_size();
        let split_materialization =
            crate::remote::federation::client::SplitMaterializationContext {
                rows,
                cols,
                scrollback_limit_bytes: self.state.pane_scrollback_limit_bytes,
                host_terminal_theme: self.state.host_terminal_theme,
                events: event_tx.clone(),
                render_notify: self.render_notify.clone(),
                render_dirty: self.render_dirty.clone(),
                origin: host_key.clone(),
            };
        let drive_handle = tokio::spawn(async move {
            let mut reader = tunnel_reader;
            let outcome = crate::remote::federation::client::drive_mount_channel(
                &mut reader,
                &mut mirror_task,
                generation,
                &event_hub,
                &mut router,
                &inbound_clip_tx,
                &out_tx,
                &outbound_clip_tx,
                Some(&split_materialization),
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
                        connection_epoch,
                        target: drive_target,
                        reason,
                    })
                    .await;
            }
        });
        self.state.mount_drive_tasks.insert(host_key, drive_handle);
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
                    context: reason.clone(),
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
                let _ = notify(
                    &format!("federated mount to {target} failed"),
                    Some(&reason),
                );
            }
            _ => {}
        }
        self.render_dirty.request_generic();
        self.render_notify.notify_one();
    }

    /// `AppEvent::FederationMountEnded` handler: the mount's drive task
    /// exited for a session-ending reason (link closed, faulted, or an I/O
    /// error) — tear down the registry entry and the workspaces it
    /// materialized.
    ///
    /// `generation` cannot actually fence this: both hosts mint a constant
    /// `mount_generation` of 1, so a stale ended-notice from a superseded
    /// drive task matches a fresh remount to the same host just as well as
    /// the live one. `connection_epoch` is minted locally, once per
    /// successful mount, and is the value that tells the two apart.
    #[cfg(unix)]
    pub(crate) fn handle_federation_mount_ended(
        &mut self,
        host_key: crate::remote::federation::id::HostKey,
        generation: u64,
        connection_epoch: crate::remote::federation::client::MountConnectionEpoch,
        target: String,
        reason: String,
    ) {
        // Fence on the locally minted epoch first, and refuse to act at all on
        // a mismatch. A delayed end-notice from a connection that has already
        // been replaced carries the same host key and the same (constant)
        // generation as the live one, so without this it would tear down a
        // healthy remount and destroy the work in flight on it.
        if self
            .state
            .remote_mirrors
            .get(&host_key)
            .map(|mirror| mirror.connection_epoch())
            != Some(connection_epoch)
        {
            tracing::debug!(
                %target,
                ?connection_epoch,
                "federation mount ended notice from a superseded connection; ignoring"
            );
            return;
        }
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

        // Above the "no workspaces to remove" early return below: a mount that
        // dies on that path would otherwise leak its in-flight stages until
        // each one's budget expires. Keyed by connection as well as host, so it
        // can only ever reach the work of the connection that actually ended.
        self.purge_pending_remote_clipboard_stages_for_origin(&host_key, connection_epoch);

        self.state.end_federation_mount(&host_key);

        let space_key = format!("federation:{}", host_key.as_str());
        let Some(idx) = self.state.workspaces.iter().position(|ws| {
            ws.worktree_space()
                .is_some_and(|space| space.key == space_key)
        }) else {
            tracing::warn!(%target, %reason, "federation mount ended but no workspaces to remove");
            self.render_dirty.request_generic();
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

        self.purge_pending_remote_splits_for_workspaces(&closing_ids);
        self.purge_pending_remote_closes_for_workspaces(&closing_ids);
        // Same purge the locally-initiated close path runs before removing the
        // workspaces. Without it a remote-initiated teardown (LinkClosed /
        // Faulted) leaks `remote_resync_pane_index` entries whose local
        // `PaneId` is already dead, so a later remount to the same host can
        // route a resync pane-removal at a stale mapping.
        self.purge_remote_resync_pane_index_for_workspaces(&closing_ids);
        self.purge_remote_resync_tab_index_for_workspaces(&closing_ids);
        self.purge_remote_resync_workspace_index_for_workspaces(&closing_ids);
        // Clipboard staging and image paste are Unix-only, so there is
        // no per-workspace state of theirs to purge on other platforms.
        #[cfg(unix)]
        self.purge_remote_image_paste_pane_state_for_workspaces(&closing_ids);

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

        self.render_dirty.request_generic();
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
        // A plain "new workspace" performed while a mounted remote workspace
        // is in focus grows the *mounted host's* workspace set, mirroring how
        // `pane.split` inside a federated workspace splits on the remote
        // (`dispatch_remote_pane_split`). Only when no `cwd` was requested and
        // no explicit `source_workspace_id` was given: an explicit path or
        // explicit source is a deliberate local-directory choice, and the
        // remote host's filesystem is a different namespace entirely. Every
        // TUI path sends neither — including the name-prompt dialog, which
        // asks for a name only and lets this handler derive the path from
        // `workspace_creation_source` — so only an API/CLI caller that named a
        // directory or source itself is treated as having chosen one.
        if params.cwd.is_none() && params.source_workspace_id.is_none() {
            if let Some(source_ws_idx) = self.workspace_creation_source() {
                if let Some(origin) = self.federation_host_key_for_workspace(source_ws_idx) {
                    return self.dispatch_remote_workspace_create(
                        id,
                        source_ws_idx,
                        origin,
                        params.label,
                        params.focus,
                    );
                }
            }
        }

        let source_workspace_index = if params.cwd.is_some() {
            None
        } else {
            match params.source_workspace_id.as_deref() {
                Some(workspace_id) => match self
                    .parse_workspace_id(workspace_id)
                    .filter(|index| self.state.workspaces.get(*index).is_some())
                {
                    Some(index) => Some(index),
                    None => return workspace_not_found(id, workspace_id),
                },
                None => self.workspace_creation_source(),
            }
        };
        let cwd = params.cwd.map(PathBuf::from).unwrap_or_else(|| {
            source_workspace_index.map_or_else(
                || self.resolve_new_terminal_cwd(None),
                |index| self.resolved_new_workspace_cwd_from(index),
            )
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

    /// Sends a `WorkspaceCreateRequest` over `source_ws_idx`'s mount instead
    /// of creating a workspace locally (see the caller). Fire-and-forget for
    /// the same hard reason `dispatch_remote_pane_split` is: this JSON-API
    /// handler runs synchronously inline with `App`'s own tick and cannot
    /// await the `WorkspaceCreateResponse` the mount's async drive task will
    /// eventually read. The new workspace materializes through the ordinary
    /// resync path once the remote confirms
    /// (`AppEvent::FederationResyncWorkspaceCreated` then the pane event that
    /// builds it), so this acknowledges with
    /// `ResponseResult::WorkspaceCreateRequested` — a *success*, because the
    /// request really was accepted and sent — rather than fabricating a
    /// `WorkspaceInfo` it cannot yet produce. Same "requested, not yet done"
    /// contract `workspace.mount_remote` already answers with
    /// (`WorkspaceMountRemoteRequested`).
    ///
    /// Falls back to an error — never to a silent local workspace — when the
    /// mount has no live link, so a stale/disconnected mount cannot quietly
    /// produce a local workspace the user asked to be remote.
    fn dispatch_remote_workspace_create(
        &mut self,
        id: String,
        source_ws_idx: usize,
        origin: crate::remote::federation::id::HostKey,
        label: Option<String>,
        focus: bool,
    ) -> String {
        // Any live remote-backed pane in this workspace carries the mount's
        // outbound handle; the request is workspace-scoped on the remote, so
        // which one is irrelevant.
        let pane_ids: Vec<crate::layout::PaneId> = self
            .state
            .workspaces
            .get(source_ws_idx)
            .map(|ws| {
                ws.tabs
                    .iter()
                    .flat_map(|tab| tab.layout.pane_ids())
                    .collect()
            })
            .unwrap_or_default();
        let out_tx = pane_ids
            .into_iter()
            .filter_map(|pane_id| {
                let terminal_id = self
                    .state
                    .workspaces
                    .get(source_ws_idx)?
                    .terminal_id(pane_id)?
                    .clone();
                self.terminal_runtimes.get(&terminal_id)?.remote_out_tx()
            })
            .next();

        let Some(out_tx) = out_tx else {
            return encode_error(
                id,
                "remote_workspace_create_unsupported",
                "creating a workspace on a remote-federated host requires a live mount; \
                 this workspace's mount is not connected",
            );
        };

        let request_id = next_remote_workspace_create_request_id();
        // Routed through the single gated send point that owns this variant's
        // wire invariants (including the label clamp), the same discipline the
        // close requests follow.
        let sent = crate::remote::federation::client::send_workspace_create_request(
            &out_tx,
            crate::remote::federation::protocol::WorkspaceCreateRequest { request_id, label },
        );
        if sent.is_err() {
            return encode_error(
                id,
                "remote_workspace_create_unsupported",
                "the remote mount's link is closing; the workspace-create request could not \
                 be sent",
            );
        }
        tracing::info!(
            request_id,
            %origin,
            "sent a workspace-create request to a mounted remote host"
        );
        if focus {
            // Claim the workspace this request creates, so the mirror focuses
            // it when the resync materializes it instead of leaving the user
            // where they were with no sign the keypress did anything. Only
            // this request's own answer can redeem the claim
            // (`App::handle_federation_workspace_create_accepted`), and only
            // on the mount it went out on: the claim carries that mount's key
            // so an answer arriving on another link cannot consume it.
            self.pending_remote_workspace_create_focus
                .insert((origin.clone(), request_id));
        }

        encode_success(
            id,
            ResponseResult::WorkspaceCreateRequested {
                origin: origin.as_str().to_string(),
            },
        )
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

    pub(super) fn handle_workspace_move_block(
        &mut self,
        id: String,
        params: WorkspaceMoveBlockParams,
    ) -> String {
        if params.workspace_ids.is_empty() {
            return encode_error(
                id,
                "workspace_move_block_failed",
                "workspace_ids must not be empty",
            );
        }

        let mut workspace_ids = Vec::with_capacity(params.workspace_ids.len());
        let mut seen_ids = std::collections::HashSet::new();
        for requested_id in &params.workspace_ids {
            let Some(index) = self.parse_workspace_id(requested_id) else {
                return workspace_not_found(id, requested_id);
            };
            let Some(workspace) = self.state.workspaces.get(index) else {
                return workspace_not_found(id, requested_id);
            };
            if !seen_ids.insert(workspace.id.clone()) {
                return encode_error(
                    id,
                    "workspace_move_block_failed",
                    format!("workspace {requested_id} appears more than once"),
                );
            }
            workspace_ids.push(workspace.id.clone());
        }

        let before_workspace_id = match params.before_workspace_id {
            Some(requested_id) => {
                let Some(index) = self.parse_workspace_id(&requested_id) else {
                    return workspace_not_found(id, &requested_id);
                };
                let Some(workspace) = self.state.workspaces.get(index) else {
                    return workspace_not_found(id, &requested_id);
                };
                if seen_ids.contains(&workspace.id) {
                    return encode_error(
                        id,
                        "workspace_move_block_failed",
                        "before_workspace_id must not be part of workspace_ids",
                    );
                }
                Some(workspace.id.clone())
            }
            None => None,
        };

        let moved = self
            .state
            .move_workspace_block(&workspace_ids, before_workspace_id.as_deref());
        let workspaces = self.workspace_list_info();
        if moved {
            self.emit_event(EventEnvelope {
                event: EventKind::WorkspaceReordered,
                data: EventData::WorkspaceReordered {
                    workspace_ids,
                    before_workspace_id,
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

    pub(super) fn handle_workspace_close(
        &mut self,
        id: String,
        params: WorkspaceCloseParams,
    ) -> String {
        let Some(index) = self.parse_workspace_id(&params.workspace_id) else {
            return workspace_not_found(id, &params.workspace_id);
        };
        if self.state.workspaces.get(index).is_none() {
            return workspace_not_found(id, &params.workspace_id);
        }
        // Federation shares one `federation:<host_key>` `worktree_space` key
        // across every mirror workspace a mount materializes (see
        // `close_single_workspace_at`), so `workspace_close_indices` reports
        // every sibling mirror as a close group even though closing one
        // federated workspace has always meant retiring only that one, never
        // the whole mount. Treat a federated target as its own singleton
        // group so upstream's group-close guard below, and the
        // `closed_workspaces` event list, never conflate mount membership
        // with a worktree-linked close group.
        let is_federated = self.federation_host_key_for_workspace(index).is_some();
        let close_indices = if is_federated {
            vec![index]
        } else {
            self.state.workspace_close_indices(index)
        };
        if close_indices.len() >= 2 && !params.close_group {
            return encode_error(
                id,
                "workspace_group_close_required",
                "workspace has linked worktree workspaces; use --group (close_group=true in the API) to close the group",
            );
        }
        let closed_workspaces = close_indices
            .iter()
            .map(|index| {
                (
                    self.public_workspace_id(*index),
                    self.workspace_info(*index),
                )
            })
            .collect::<Vec<_>>();

        // pane_ids covers only the directly-targeted workspace: group members
        // beyond `index` (the non-federated worktree-linked-group case) are
        // cleaned up by `close_selected_workspace` itself, which removes
        // plugin pane records for every index in its own close group.
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

        // The normal close path must end any live federation mount this
        // workspace belongs to (not just remove its UI/runtime state) — a
        // remote workspace closed here otherwise leaves the SSH link and its
        // drive task alive invisibly (remount later reports "already live")
        // and skips purging `pending_remote_splits` for it before removal,
        // since that purge otherwise only runs from
        // `handle_federation_mount_ended`, which never fires for a close the
        // user initiated locally. One mount can carry several workspaces, so
        // closing one is the mount's last-workspace case only when no sibling
        // from the same host remains: ending the mount while siblings are
        // still mirrored would pull the link out from under them, and closing
        // the whole worktree-space group would retire them outright. Purge
        // this workspace's pending state alone and retire it on its own.
        #[cfg(unix)]
        let federated_origin = self.federation_host_key_for_workspace(index);
        #[cfg(unix)]
        if let Some(host_key) = federated_origin.as_ref() {
            let closing_ids: std::collections::HashSet<String> = self
                .state
                .workspaces
                .get(index)
                .map(|ws| std::iter::once(ws.id.clone()).collect())
                .unwrap_or_default();
            self.purge_pending_remote_splits_for_workspaces(&closing_ids);
            self.purge_pending_remote_closes_for_workspaces(&closing_ids);
            // Clipboard staging and image paste are Unix-only, so there is
            // no per-workspace state of theirs to purge on other platforms.
            #[cfg(unix)]
            self.purge_pending_remote_clipboard_stages_for_workspaces(&closing_ids);
            // Clipboard staging and image paste are Unix-only, so there is
            // no per-workspace state of theirs to purge on other platforms.
            #[cfg(unix)]
            self.purge_remote_image_paste_pane_state_for_workspaces(&closing_ids);
            self.purge_remote_resync_pane_index_for_workspaces(&closing_ids);
            self.purge_remote_resync_tab_index_for_workspaces(&closing_ids);
            self.purge_remote_resync_workspace_index_for_workspaces(&closing_ids);

            let siblings_remain = (0..self.state.workspaces.len()).any(|other| {
                other != index
                    && self.federation_host_key_for_workspace(other).as_ref() == Some(host_key)
            });
            if !siblings_remain {
                self.state.end_federation_mount(host_key);
            }
        }

        self.state.selected = index;
        #[cfg(unix)]
        let retire_one_only = federated_origin.is_some();
        #[cfg(not(unix))]
        let retire_one_only = false;
        if retire_one_only {
            self.close_single_workspace_at(index);
        } else {
            self.state.close_selected_workspace();
        }
        self.state.remove_plugin_pane_records(pane_ids);
        self.shutdown_detached_terminal_runtimes();
        for (workspace_id, workspace) in closed_workspaces {
            self.emit_event(EventEnvelope {
                event: EventKind::WorkspaceClosed,
                data: EventData::WorkspaceClosed {
                    workspace_id,
                    workspace: Some(workspace),
                },
            });
        }

        encode_success(id, ResponseResult::Ok {})
    }

    /// `workspace.close_remote`: forwards a close to the serving host of a
    /// federated workspace instead of retiring the local mirror
    /// (`handle_workspace_close` NEVER reaches the wire — this is the
    /// distinct opt-in verb for that). Mirrors `dispatch_remote_pane_close`
    /// (`api/panes.rs`)'s exact shape/reasoning: this JSON-API handler runs
    /// synchronously and cannot await the eventual `WorkspaceCloseResponse`,
    /// so it sends the request now and acknowledges the send with the
    /// `workspace_close_requested` success. Falls back to
    /// `remote_close_unsupported` when
    /// the target is not a federated workspace, has no live mount, or the
    /// mount's peer never agreed `WORKSPACE_TAB_CLOSE`.
    pub(super) fn handle_workspace_close_remote(
        &mut self,
        id: String,
        target: WorkspaceTarget,
    ) -> String {
        let Some(ws_idx) = self.parse_workspace_id(&target.workspace_id) else {
            return workspace_not_found(id, &target.workspace_id);
        };
        if self.state.workspaces.get(ws_idx).is_none() {
            return workspace_not_found(id, &target.workspace_id);
        }
        self.dispatch_remote_workspace_close(id, ws_idx)
    }

    fn dispatch_remote_workspace_close(&mut self, id: String, ws_idx: usize) -> String {
        let workspace_id = self.public_workspace_id(ws_idx);
        if !matches!(
            crate::remote::federation::id::classify(&workspace_id),
            crate::remote::federation::id::IdClass::Remote(_)
        ) {
            return encode_error(
                id,
                "remote_close_unsupported",
                "workspace.close_remote requires a federated workspace",
            );
        }

        let live_pane_ids: Vec<crate::layout::PaneId> = self
            .state
            .workspaces
            .get(ws_idx)
            .map(|ws| {
                ws.tabs
                    .iter()
                    .flat_map(|tab| tab.layout.pane_ids())
                    .collect()
            })
            .unwrap_or_default();
        let out_tx = live_pane_ids.into_iter().find_map(|pane_id| {
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
                "closing a remote-federated workspace requires a live mount; this \
                 workspace's mount is not connected",
            );
        };

        let Some(origin) = self.federation_host_key_for_workspace(ws_idx) else {
            return encode_error(
                id,
                "remote_close_unsupported",
                "closing a remote-federated workspace requires a live mount; this \
                 workspace has no registered federation mount",
            );
        };
        let Some(mirror) = self.state.remote_mirrors.get(&origin) else {
            return encode_error(
                id,
                "remote_close_unsupported",
                "closing a remote-federated workspace requires a live mount; this \
                 workspace's mount is not connected",
            );
        };

        // `strip_mount_namespace` only reads `mount.host_key` — the other
        // two `Mount` fields are unused by it and irrelevant here (the
        // public workspace id itself carries no generation/instance data),
        // so a placeholder `Mount` built from `origin` alone is enough to
        // reverse the namespacing.
        let mount = crate::remote::federation::id::Mount {
            host_key: origin.clone(),
            server_instance_id: crate::remote::federation::id::ServerInstanceId(String::new()),
            mount_generation: 0,
        };
        let raw_target_workspace_id =
            crate::remote::federation::id::strip_mount_namespace(&mount, &workspace_id);

        let request_id = super::panes::next_remote_close_request_id();
        let sent = crate::remote::federation::client::send_workspace_close_request(
            mirror,
            &out_tx,
            crate::remote::federation::protocol::WorkspaceCloseRequest {
                request_id,
                target_workspace_id: raw_target_workspace_id,
            },
        );
        if let Err(err) = sent {
            let message = match err {
                crate::remote::federation::client::CloseRequestSendError::CapabilityNotAgreed => {
                    "the remote host does not support workspace.close_remote"
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
                target: crate::app::creation::RemoteCloseTarget::Workspace,
            },
        );

        encode_success(
            id,
            ResponseResult::WorkspaceCloseRequested {
                origin: origin_label,
            },
        )
    }

    /// Resolves the live federation mount's `HostKey` that `index`'s
    /// workspace belongs to, if any — matches its `worktree_space` key
    /// (`federation:<host_key>`, set by `materialize_federation_mount`)
    /// against the live `remote_mirrors` registry.
    /// Reads only cross-platform state, so it stays ungated: the callers in
    /// `api/panes.rs` are ungated too, and on a target without the dial/mount
    /// primitives `remote_mirrors` is always empty, which makes this return
    /// `None` and those callers report "not a federated workspace".
    pub(crate) fn federation_host_key_for_workspace(
        &self,
        index: usize,
    ) -> Option<crate::remote::federation::id::HostKey> {
        let space_key = &self.state.workspaces.get(index)?.worktree_space()?.key;
        self.state
            .remote_mirrors
            .keys()
            .find(|host_key| format!("federation:{}", host_key.as_str()) == *space_key)
            .cloned()
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
    use crate::{
        api::schema::{ErrorResponse, SuccessResponse},
        config::Config,
        workspace::Workspace,
    };

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
            crate::app::AppPolicy::TEST,
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
                source_workspace_id: None,
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

    #[tokio::test]
    async fn workspace_create_uses_explicit_source_workspace() {
        use super::super::test_support::{exiting_test_command, shutdown_test_runtimes};
        use crate::config::ShellModeConfig;

        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.default_shell = exiting_test_command().into();
        app.state.shell_mode = ShellModeConfig::NonLogin;
        app.state.workspaces = vec![Workspace::test_new("first"), Workspace::test_new("source")];
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.ensure_test_terminals();
        shutdown_test_runtimes(&mut app);

        let source_cwd =
            std::env::temp_dir().join(format!("herdr-ws-explicit-source-{}", std::process::id()));
        std::fs::create_dir_all(&source_cwd).unwrap();
        let pane_id = app.state.workspaces[1].focused_pane_id().unwrap();
        let terminal_id = app.state.workspaces[1]
            .terminal_id(pane_id)
            .cloned()
            .unwrap();
        app.state.terminals.get_mut(&terminal_id).unwrap().cwd = source_cwd.clone();
        let source_workspace_id = app.public_workspace_id(1);

        let response = app.handle_workspace_create(
            "req".into(),
            WorkspaceCreateParams {
                source_workspace_id: Some(source_workspace_id),
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
        assert_eq!(
            crate::worktree::canonical_or_original(&app.state.workspaces[2].identity_cwd),
            crate::worktree::canonical_or_original(&source_cwd)
        );

        let invalid = app.handle_workspace_create(
            "invalid".into(),
            WorkspaceCreateParams {
                source_workspace_id: Some("w_999".into()),
                cwd: None,
                focus: false,
                label: None,
                env: Default::default(),
            },
        );
        let error: ErrorResponse = serde_json::from_str(&invalid).unwrap();
        assert_eq!(error.error.code, "workspace_not_found");

        let captured = app.handle_workspace_create(
            "captured".into(),
            WorkspaceCreateParams {
                source_workspace_id: Some("w_999".into()),
                cwd: Some(source_cwd.display().to_string()),
                focus: false,
                label: None,
                env: Default::default(),
            },
        );
        let success: SuccessResponse = serde_json::from_str(&captured).unwrap();
        assert!(matches!(
            success.result,
            ResponseResult::WorkspaceCreated { .. }
        ));
        assert_eq!(
            crate::worktree::canonical_or_original(&app.state.workspaces[3].identity_cwd),
            crate::worktree::canonical_or_original(&source_cwd)
        );
        shutdown_test_runtimes(&mut app);
        let _ = std::fs::remove_dir_all(&source_cwd);
    }

    fn app_with_linked_worktree() -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
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

    fn app_with_worktree_group() -> App {
        let mut app = app_with_linked_worktree();
        let mut parent = Workspace::test_new("parent");
        parent.worktree_space = Some(crate::workspace::WorktreeSpaceMembership {
            key: "repo-key".into(),
            label: "herdr".into(),
            repo_root: "/repo/herdr".into(),
            checkout_path: "/repo/herdr".into(),
            is_linked_worktree: false,
        });
        app.state.workspaces.insert(0, parent);
        app.state.active = Some(1);
        app.state.selected = 1;
        app.state.mode = crate::app::Mode::Terminal;
        app
    }

    #[test]
    fn api_workspace_close_parent_group_requires_explicit_group_intent() {
        for confirm_close in [true, false] {
            let mut app = app_with_worktree_group();
            app.state.confirm_close = confirm_close;
            let parent_id = app.public_workspace_id(0);
            let workspace_ids = app
                .state
                .workspaces
                .iter()
                .map(|workspace| workspace.id.clone())
                .collect::<Vec<_>>();

            let request: crate::api::schema::Request = serde_json::from_value(serde_json::json!({
                "id": "req",
                "method": "workspace.close",
                "params": { "workspace_id": parent_id }
            }))
            .unwrap();
            let response = app.handle_api_request(request);

            let response: serde_json::Value = serde_json::from_str(&response).unwrap();
            assert_eq!(response["error"]["code"], "workspace_group_close_required");
            assert!(app.event_hub.events_after(0).is_empty());
            assert_eq!(app.state.mode, crate::app::Mode::Terminal);
            assert_eq!(app.state.active, Some(1));
            assert_eq!(app.state.selected, 1);
            assert_eq!(
                app.state
                    .workspaces
                    .iter()
                    .map(|workspace| workspace.id.clone())
                    .collect::<Vec<_>>(),
                workspace_ids
            );
        }
    }

    #[test]
    fn api_workspace_close_noncontiguous_group_preserves_adversarial_identity_state() {
        let mut app = app_with_worktree_group();
        let parent = app.state.workspaces.remove(0);
        let linked = app.state.workspaces.remove(0);
        app.state = crate::app::state::AppState::test_with_adversarial_identity_state();
        let survivor_id = app.state.workspaces[0].id.clone();
        app.state.workspaces.insert(0, parent);
        app.state.workspaces.push(linked);
        app.state.active = Some(1);
        app.state.selected = 1;
        app.state.mode = crate::app::Mode::Terminal;
        app.state.ensure_test_terminals();
        let closed_pane_ids = [0, 2].map(|index| app.state.workspaces[index].tabs[0].root_pane);
        let closed_terminal_ids = [0, 2].map(|index| {
            app.state
                .terminal_id_for_pane(index, app.state.workspaces[index].tabs[0].root_pane)
                .expect("closed workspace pane has a terminal")
        });
        for pane_id in closed_pane_ids {
            app.state.plugin_panes.insert(
                pane_id,
                crate::app::state::PluginPaneRecord {
                    plugin_id: "example.pane".into(),
                    entrypoint: "board".into(),
                },
            );
        }
        app.state.assert_invariants_for_test();

        let parent_id = app.public_workspace_id(0);
        let closed = [0, 2]
            .into_iter()
            .map(|index| (app.public_workspace_id(index), app.workspace_info(index)))
            .collect::<Vec<_>>();

        let response = app.handle_workspace_close(
            "req".into(),
            WorkspaceCloseParams {
                workspace_id: parent_id,
                close_group: true,
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(success.id, "req");
        assert_eq!(app.state.workspaces.len(), 1);
        assert_eq!(app.state.workspaces[0].id, survivor_id);
        for terminal_id in closed_terminal_ids {
            assert!(!app.state.terminals.contains_key(&terminal_id));
        }
        for pane_id in closed_pane_ids {
            assert!(!app.state.plugin_panes.contains_key(&pane_id));
        }
        assert!(app.state.terminal_runtime_shutdowns.is_empty());
        app.state.assert_invariants_for_test();
        let events = app.event_hub.events_after(0);
        assert_eq!(events.len(), closed.len());
        for ((_, event), (workspace_id, workspace)) in events.iter().zip(closed) {
            assert!(matches!(event.event, EventKind::WorkspaceClosed));
            assert!(matches!(
                &event.data,
                EventData::WorkspaceClosed {
                    workspace_id: closed_id,
                    workspace: Some(closed_workspace),
                } if closed_id == &workspace_id && closed_workspace == &workspace
            ));
        }
    }

    #[test]
    fn api_workspace_close_closes_linked_worktree_workspace_only() {
        let mut app = app_with_worktree_group();
        let linked_id = app.public_workspace_id(1);

        let response = app.handle_workspace_close(
            "req".into(),
            WorkspaceCloseParams {
                workspace_id: linked_id,
                close_group: true,
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(success.id, "req");
        assert_eq!(app.state.workspaces.len(), 1);
        assert_eq!(app.state.workspaces[0].display_name(), "parent");
    }

    #[test]
    fn api_workspace_close_event_includes_final_worktree_snapshot() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            event_hub.clone(),
        );
        app.state.workspaces = app_with_linked_worktree().state.workspaces;
        let workspace_id = app.state.workspaces[0].id.clone();

        let response = app.handle_workspace_close(
            "req".into(),
            WorkspaceCloseParams {
                workspace_id: workspace_id.clone(),
                close_group: false,
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
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            event_hub.clone(),
        );
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
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            event_hub.clone(),
        );
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
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            event_hub.clone(),
        );
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
    fn api_workspace_move_block_reorders_atomically() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            event_hub.clone(),
        );
        app.state.workspaces = vec![
            Workspace::test_new("child"),
            Workspace::test_new("normal"),
            Workspace::test_new("parent"),
            Workspace::test_new("tail"),
        ];
        let parent_id = app.public_workspace_id(2);
        let child_id = app.public_workspace_id(0);
        let tail_id = app.public_workspace_id(3);

        let response = app.handle_workspace_move_block(
            "req".into(),
            WorkspaceMoveBlockParams {
                workspace_ids: vec![parent_id.clone(), child_id.clone()],
                before_workspace_id: Some(tail_id.clone()),
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::WorkspaceList { workspaces } = success.result else {
            panic!("expected workspace list");
        };
        assert_eq!(
            app.state
                .workspaces
                .iter()
                .map(|workspace| workspace.display_name())
                .collect::<Vec<_>>(),
            ["normal", "parent", "child", "tail"]
        );
        assert_eq!(workspaces[1].workspace_id, parent_id);
        assert_eq!(workspaces[2].workspace_id, child_id);
        let events = event_hub.events_after(0);
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0].1.data,
            EventData::WorkspaceReordered {
                workspace_ids,
                before_workspace_id,
                workspaces,
            } if workspace_ids.first() == Some(&parent_id)
                && workspace_ids.get(1) == Some(&child_id)
                && workspace_ids.len() == 2
                && before_workspace_id.as_deref() == Some(tail_id.as_str())
                && workspaces[1].workspace_id == parent_id
        ));
    }

    #[test]
    fn api_workspace_move_noop_does_not_emit_event() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            event_hub.clone(),
        );
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
        test_federation_mirror_with_workspaces(target, generation, 1)
    }

    /// `test_federation_mirror_with_workspace` for `workspace_count` remote
    /// workspaces on the same mount, so a test can exercise the multi-
    /// workspace mount shape a real `federation-serve` host reports. Every
    /// workspace `w<n>` owns exactly one tab (`w<n>-tab`) and one pane
    /// (`w<n>-p1`), which keeps each workspace's `remote_resync_*_index`
    /// entries distinguishable by key.
    #[cfg(unix)]
    fn test_federation_mirror_with_workspaces(
        target: &str,
        generation: u64,
        workspace_count: usize,
    ) -> crate::remote::federation::reducer::RemoteMirror {
        use crate::api::schema::common::AgentStatus;
        use crate::api::schema::session::SessionSnapshot;
        use crate::api::schema::{
            PaneInfo as RemotePaneInfo, TabInfo as RemoteTabInfo, WorkspaceInfo,
        };
        use crate::remote::federation::protocol::EventCursor;

        let mut mirror = test_federation_mirror_at_generation(target, generation);
        let mut workspaces = Vec::new();
        let mut tabs = Vec::new();
        let mut panes = Vec::new();
        for n in 1..=workspace_count {
            let workspace_id = format!("w{n}");
            let tab_id = format!("w{n}-tab");
            workspaces.push(WorkspaceInfo {
                workspace_id: workspace_id.clone(),
                number: n,
                label: format!("remote workspace {n}"),
                focused: false,
                pane_count: 1,
                tab_count: 1,
                active_tab_id: tab_id.clone(),
                agent_status: AgentStatus::Idle,
                tokens: Default::default(),
                worktree: None,
            });
            tabs.push(RemoteTabInfo {
                tab_id: tab_id.clone(),
                workspace_id: workspace_id.clone(),
                number: 1,
                label: format!("remote tab {n}"),
                focused: false,
                pane_count: 1,
                agent_status: AgentStatus::Idle,
            });
            panes.push(RemotePaneInfo {
                pane_id: format!("w{n}-p1"),
                terminal_id: format!("t{n}"),
                workspace_id,
                tab_id,
                focused: false,
                cwd: Some("/home/alice/project".to_string()),
                foreground_cwd: None,
                label: Some(format!("remote pane {n}")),
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
            });
        }
        let snapshot = SessionSnapshot {
            version: "0.0.0-test".to_string(),
            protocol: 1,
            focused_workspace_id: None,
            focused_tab_id: None,
            focused_pane_id: None,
            workspaces,
            tabs,
            panes,
            layouts: Vec::new(),
            agents: Vec::new(),
        };
        mirror.apply_snapshot(&snapshot, EventCursor(0));
        mirror
    }

    /// A live mount with one materialized federated workspace, wired the
    /// same way `panes.rs`'s `app_with_federation_mounted_pane` wires a
    /// pane: `materialize_federation_mount` builds the local workspace, then
    /// `begin_federation_mount` registers the mirror in `remote_mirrors` so
    /// `federation_host_key_for_workspace`/`remote_mirrors.get` (both used
    /// by `dispatch_remote_workspace_close`) resolve it. When
    /// `agree_close_capability` is false the mirror never agrees
    /// `WORKSPACE_TAB_CLOSE`, exercising the capability-gated refusal path.
    #[cfg(unix)]
    fn app_with_federation_mounted_workspace(
        agree_close_capability: bool,
    ) -> (
        App,
        tokio::sync::mpsc::UnboundedReceiver<
            crate::remote::federation::protocol::FederationMessage,
        >,
        usize,
    ) {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );

        let mut mirror = test_federation_mirror_with_workspace("alice@10.0.0.1", 1);
        if agree_close_capability {
            mirror.set_agreed_capabilities(
                [crate::remote::federation::protocol::Capability::new(
                    crate::remote::federation::protocol::Capability::WORKSPACE_TAB_CLOSE,
                )]
                .into_iter()
                .collect(),
            );
        }

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

    /// `dispatch_remote_workspace_close`: closing a federated workspace via
    /// `workspace.close_remote` must send a `WorkspaceCloseRequest` over the
    /// mount's link, register a pending-close entry, acknowledge with the
    /// `workspace_close_requested` success, and NOT remove the local mirror workspace —
    /// the real close decision belongs to the serving host, answered
    /// asynchronously by `App::handle_federation_workspace_close_ready`.
    #[cfg(unix)]
    #[tokio::test]
    async fn dispatch_remote_workspace_close_sends_a_request_and_registers_pending() {
        let (mut app, mut out_rx, ws_idx) = app_with_federation_mounted_workspace(true);
        let workspace_id = app.state.workspaces[ws_idx].id.clone();
        while out_rx.try_recv().is_ok() {}

        let response = app.handle_workspace_close_remote(
            "req".into(),
            WorkspaceTarget {
                workspace_id: workspace_id.clone(),
            },
        );

        let success: crate::api::schema::SuccessResponse = serde_json::from_str(&response).unwrap();
        assert!(
            matches!(
                success.result,
                ResponseResult::WorkspaceCloseRequested { .. }
            ),
            "close_remote must answer with a success, not an error envelope: {response}"
        );
        assert!(
            app.state.workspaces.iter().any(|ws| ws.id == workspace_id),
            "dispatching a remote close must not remove the local mirror workspace; only \
             the eventual WorkspaceCloseResponse does"
        );

        let request = match out_rx.try_recv().expect("a WorkspaceCloseRequest was sent") {
            crate::remote::federation::protocol::FederationMessage::WorkspaceCloseRequest(
                request,
            ) => request,
            other => panic!("expected a WorkspaceCloseRequest, got {other:?}"),
        };
        assert_eq!(request.target_workspace_id, "w1");
        assert_eq!(app.pending_remote_closes.len(), 1);
    }

    /// Every close kind correlates through the ONE `pending_remote_closes`
    /// map, so they must all mint from the ONE close-id counter. A second
    /// counter would restart at 1 and hand out an id already in flight,
    /// popping the wrong pending entry — and the response handler's origin
    /// check could not catch it, because two closes on the same mount carry
    /// the same `HostKey`. Pinned by observing that dispatching a real close
    /// advances that shared counter, rather than by asserting a literal id.
    #[cfg(unix)]
    #[tokio::test]
    async fn dispatching_a_remote_workspace_close_mints_from_the_shared_close_id_counter() {
        let (mut app, mut out_rx, ws_idx) = app_with_federation_mounted_workspace(true);
        let workspace_id = app.state.workspaces[ws_idx].id.clone();
        while out_rx.try_recv().is_ok() {}

        let before = super::super::panes::next_remote_close_request_id();
        let _ = app.handle_workspace_close_remote(
            "req".into(),
            WorkspaceTarget {
                workspace_id: workspace_id.clone(),
            },
        );
        let after = super::super::panes::next_remote_close_request_id();

        let dispatched = *app
            .pending_remote_closes
            .keys()
            .next()
            .expect("the dispatch registered exactly one pending close");
        assert!(
            dispatched > before && dispatched < after,
            "the workspace-close dispatcher must mint from the shared close-id counter \
             (got {dispatched}, expected strictly between {before} and {after})"
        );
    }

    /// Echo-rule safety test (the most important one in this file):
    /// `workspace.close` on a federated workspace must NEVER reach the wire.
    /// `workspace.close_remote` is the distinct opt-in verb for that.
    #[cfg(unix)]
    #[tokio::test]
    async fn workspace_close_on_a_mirrored_workspace_sends_nothing() {
        let (mut app, mut out_rx, ws_idx) = app_with_federation_mounted_workspace(true);
        let workspace_id = app.state.workspaces[ws_idx].id.clone();
        while out_rx.try_recv().is_ok() {}

        let response = app.handle_workspace_close(
            "req".into(),
            WorkspaceCloseParams {
                workspace_id: workspace_id.clone(),
                close_group: false,
            },
        );
        let _: SuccessResponse = serde_json::from_str(&response).unwrap();

        // `workspace.close` legitimately still sends ordinary local-unmount
        // teardown traffic (e.g. `Terminal(Close)` for the mirror's own
        // terminal channels) — the echo rule under test is narrower: no
        // `WorkspaceCloseRequest` (the wire message that would ask the
        // SERVING host to close something) may ever be sent by this verb.
        while let Ok(msg) = out_rx.try_recv() {
            assert!(
                !matches!(
                    msg,
                    crate::remote::federation::protocol::FederationMessage::WorkspaceCloseRequest(
                        _
                    )
                ),
                "workspace.close on a federated workspace must never send a \
                 WorkspaceCloseRequest; only workspace.close_remote may"
            );
        }
    }

    /// An ungated send is fatal, not merely useless — a peer that never
    /// agreed `WORKSPACE_TAB_CLOSE` has no decoder for `WorkspaceCloseRequest`.
    /// `dispatch_remote_workspace_close` must refuse instead of sending when
    /// the capability was never agreed.
    #[cfg(unix)]
    #[tokio::test]
    async fn dispatch_remote_workspace_close_without_the_capability_agreed_is_refused() {
        let (mut app, mut out_rx, ws_idx) = app_with_federation_mounted_workspace(false);
        let workspace_id = app.state.workspaces[ws_idx].id.clone();
        while out_rx.try_recv().is_ok() {}

        let response = app.handle_workspace_close_remote(
            "req".into(),
            WorkspaceTarget {
                workspace_id: workspace_id.clone(),
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
        (
            crate::remote::ChildGuard::for_test(child),
            stdout,
            stdin,
            pid,
        )
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
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            event_hub.clone(),
        );
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
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            event_hub.clone(),
        );
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
        assert!(
            app.state
                .toast
                .as_ref()
                .unwrap()
                .title
                .contains("remote-host")
        );
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
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            event_hub.clone(),
        );
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
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            event_hub.clone(),
        );
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
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            event_hub,
        );
        app.state.workspaces = vec![Workspace::test_new("local")];

        let session_name = crate::session::active_name()
            .unwrap_or_else(|| crate::session::DEFAULT_SESSION_NAME.to_string());
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
        assert!(
            saw_failure,
            "expected a FederationMountFailed event naming the duplicate host"
        );
    }

    // Phase 01: server-side target validation. `handle_workspace_mount_remote`
    // must reject option-like / empty / localhost targets synchronously,
    // before any `tokio::spawn`, so the collector (dialog or CLI) sees the
    // rejection immediately and no dial or mirror mutation ever happens for
    // an invalid target.
    #[cfg(unix)]
    #[tokio::test]
    async fn mount_remote_rejects_option_like_target_without_spawning_a_dial() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            event_hub,
        );
        app.state.workspaces = vec![Workspace::test_new("local")];

        let response = app.handle_workspace_mount_remote(
            "req".into(),
            WorkspaceMountRemoteParams {
                targets: vec![
                    "good-host".to_string(),
                    "-oProxyCommand=touch /tmp/pwn".to_string(),
                ],
                remote_keybindings: false,
            },
        );
        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "invalid_request");
        assert!(
            error
                .error
                .message
                .contains("-oProxyCommand=touch /tmp/pwn")
        );

        assert!(app.state.remote_mirrors.is_empty());
        // No dial and no async event: nothing was spawned for this request.
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(200), app.event_rx.recv())
                .await
                .is_err()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn mount_remote_rejects_localhost_target() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            event_hub,
        );
        app.state.workspaces = vec![Workspace::test_new("local")];

        let response = app.handle_workspace_mount_remote(
            "req".into(),
            WorkspaceMountRemoteParams {
                targets: vec!["localhost".to_string()],
                remote_keybindings: false,
            },
        );
        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "invalid_request");

        assert!(app.state.remote_mirrors.is_empty());
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(200), app.event_rx.recv())
                .await
                .is_err()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn mount_remote_rejects_blank_only_targets() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            event_hub,
        );
        app.state.workspaces = vec![Workspace::test_new("local")];

        let response = app.handle_workspace_mount_remote(
            "req".into(),
            WorkspaceMountRemoteParams {
                targets: vec!["  ".to_string(), "".to_string()],
                remote_keybindings: false,
            },
        );
        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "invalid_request");
        assert!(app.state.remote_mirrors.is_empty());
    }

    // A pre-mounted host takes the "already mounted" early-return branch
    // (`double_attach_conflict`), which never spawns a real ssh dial — safe
    // to exercise end to end without any network I/O. That branch still
    // fire-and-forget-spawns a `FederationMountFailed` notice
    // (`src/app/api/workspaces.rs:82-89`), so this test asserts on the
    // mirror count staying untouched and the ack being a success response,
    // never on "no event at all".
    #[cfg(unix)]
    #[tokio::test]
    async fn mount_remote_accepts_plain_and_user_at_host_targets() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            event_hub,
        );
        app.state.workspaces = vec![Workspace::test_new("local")];

        let session_name = crate::session::active_name()
            .unwrap_or_else(|| crate::session::DEFAULT_SESSION_NAME.to_string());
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
                targets: vec!["  already-mounted-host  ".to_string()],
                remote_keybindings: false,
            },
        );
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        match success.result {
            ResponseResult::WorkspaceMountRemoteRequested { targets } => {
                assert_eq!(targets, vec!["already-mounted-host".to_string()]);
            }
            other => panic!("expected WorkspaceMountRemoteRequested, got {other:?}"),
        }

        // Mirror count is untouched by this request (no new mount, the
        // pre-existing one stays exactly once) — no real ssh dial spawned.
        assert_eq!(app.state.remote_mirrors.len(), 1);
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
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            event_hub.clone(),
        );
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
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            event_hub.clone(),
        );
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
        assert_eq!(
            app.state.workspaces.len(),
            2,
            "the federation mount must have materialized a workspace"
        );

        app.handle_federation_mount_ended(
            host_key.clone(),
            1,
            crate::remote::federation::client::MountConnectionEpoch::UNMOUNTED,
            "remote-host".to_string(),
            "link closed".to_string(),
        );

        assert!(app.state.remote_mirrors.is_empty());
        assert_eq!(app.state.workspaces.len(), 1);
        assert!(
            app.state
                .workspaces
                .iter()
                .all(|ws| ws.worktree_space().is_none())
        );
        assert!(
            event_hub
                .events_after(0)
                .iter()
                .any(|(_, event)| matches!(&event.data, EventData::WorkspaceClosed { .. }))
        );
    }

    /// Memory-leak regression: the locally-initiated close path purges
    /// `remote_resync_pane_index` for the workspaces it removes, but the
    /// remote-initiated teardown here (LinkClosed / Faulted) did not — so
    /// every remote-initiated unmount leaked one entry per mount-time pane
    /// and left a `remote_pane_id -> dead PaneId` mapping that a later
    /// remount to the same host could route a resync pane-removal at.
    #[cfg(unix)]
    #[tokio::test]
    async fn federation_mount_ended_purges_remote_resync_pane_index_for_its_workspaces() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            event_hub.clone(),
        );
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
        assert!(
            !app.remote_resync_pane_index.is_empty(),
            "the mount must have indexed its mount-time panes, or this test checks nothing"
        );

        // An entry for a different, still-live workspace must survive.
        let other_remote_pane_id = "other-workspace:p1".to_string();
        let other_local_pane_id = crate::layout::PaneId::alloc();
        app.remote_resync_pane_index
            .insert(other_remote_pane_id.clone(), other_local_pane_id);

        let host_key = crate::remote::federation::id::HostKey::new("remote-host", "s1");
        app.handle_federation_mount_ended(
            host_key,
            1,
            crate::remote::federation::client::MountConnectionEpoch::UNMOUNTED,
            "remote-host".to_string(),
            "link closed".to_string(),
        );

        assert_eq!(app.state.workspaces.len(), 1);
        assert_eq!(
            app.remote_resync_pane_index.len(),
            1,
            "only the unrelated entry may remain: {:?}",
            app.remote_resync_pane_index
        );
        assert_eq!(
            app.remote_resync_pane_index.get(&other_remote_pane_id),
            Some(&other_local_pane_id)
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn federation_mount_ended_purges_pending_remote_splits_for_its_workspaces() {
        // Regression test for the stale-index splice hazard: a
        // `pending_remote_splits` entry registered for a federated
        // workspace must be purged when that workspace's mount ends, so a
        // late/never-arriving `SplitPaneResponse` can't later splice its
        // pane into whatever workspace ends up reusing the same `Vec`
        // index (see `handle_federation_mount_ended` and
        // `PendingRemoteSplit::workspace_id`).
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            event_hub.clone(),
        );
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
        assert_eq!(
            app.state.workspaces.len(),
            2,
            "the federation mount must have materialized a workspace"
        );
        let remote_ws_idx = 1;
        let remote_workspace_id = app.public_workspace_id(remote_ws_idx);

        // Register a pending remote split as `dispatch_remote_pane_split`
        // would, targeting the federated workspace.
        let request_id = 4242u64;
        app.register_pending_remote_split(
            request_id,
            crate::app::creation::PendingRemoteSplit {
                workspace_id: remote_workspace_id,
                target_pane_id: crate::layout::PaneId::from_raw(1),
                direction: ratatui::layout::Direction::Horizontal,
                ratio: 0.5,
                focus: false,
                origin: crate::remote::federation::id::HostKey::new("remote-host", "s1"),
            },
        );
        assert!(app.pending_remote_splits.contains_key(&request_id));

        let host_key = crate::remote::federation::id::HostKey::new("remote-host", "s1");
        app.handle_federation_mount_ended(
            host_key,
            1,
            crate::remote::federation::client::MountConnectionEpoch::UNMOUNTED,
            "remote-host".to_string(),
            "link closed".to_string(),
        );

        assert!(
            !app.pending_remote_splits.contains_key(&request_id),
            "the pending split for a torn-down mount's workspace must be purged, not leaked"
        );
        assert_eq!(app.state.workspaces.len(), 1);

        // Simulate a late `SplitPaneResponse` arriving after the purge: the
        // ready-handler must find no pending registration and drop it
        // instead of panicking or splicing the pane into the local
        // workspace that now occupies index 1's old slot.
        let (events_tx, _events_rx) = tokio::sync::mpsc::channel::<crate::events::AppEvent>(4);
        let (out_tx, _out_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_output_tx, output_rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(4);
        let (clipboard_tx, _clipboard_rx) = tokio::sync::mpsc::unbounded_channel();
        let render_notify = std::sync::Arc::new(tokio::sync::Notify::new());
        let render_dirty = std::sync::Arc::new(crate::render_signal::RenderSignal::new());
        let terminal_id = crate::terminal::TerminalId::alloc();
        let runtime = crate::terminal::TerminalRuntime::spawn_remote(
            crate::layout::PaneId::alloc(),
            24,
            80,
            1 << 16,
            crate::terminal_theme::TerminalTheme::default(),
            None,
            terminal_id.to_string(),
            1,
            out_tx,
            output_rx,
            clipboard_tx,
            events_tx,
            render_notify,
            render_dirty,
        )
        .expect("spawn_remote must succeed for a fresh channel pair");
        let terminal =
            crate::terminal::TerminalState::new(terminal_id.clone(), std::path::PathBuf::from("/"));
        let pane_state = crate::pane::PaneState::new(terminal_id.clone());
        let pane_id = crate::layout::PaneId::alloc();
        let workspaces_before = app.state.workspaces.len();

        app.handle_federation_split_pane_ready(crate::events::FederationSplitPaneReady {
            request_id,
            origin: crate::remote::federation::id::HostKey::new("remote-host", "s1"),
            remote_pane_id: "remote-pane".to_string(),
            pane_id,
            terminal_id,
            terminal,
            runtime,
            pane_state,
        });

        assert_eq!(
            app.state.workspaces.len(),
            workspaces_before,
            "a late response for a purged request must not splice a pane into any workspace"
        );
        assert!(
            app.state.workspaces[0].pane_state(pane_id).is_none(),
            "the local workspace that now occupies the old index must not receive the stale pane"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn closing_a_federated_workspace_ends_its_mount_and_purges_pending_splits() {
        // Regression test: closing a federated remote workspace via the
        // normal `workspace.close` path must not just remove UI/runtime
        // state. It must also end the federation mount (so a remount to the
        // same host doesn't report "already live") and purge
        // `pending_remote_splits` for that workspace id before it's removed
        // (the purge previously only ran from `handle_federation_mount_ended`,
        // which never fires for a locally initiated close).
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            event_hub.clone(),
        );
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
        assert_eq!(
            app.state.workspaces.len(),
            2,
            "the federation mount must have materialized a workspace"
        );
        assert!(!app.state.remote_mirrors.is_empty());
        assert!(!app.state.mount_drive_tasks.is_empty());
        let remote_ws_idx = 1;
        let remote_workspace_id = app.public_workspace_id(remote_ws_idx);

        let request_id = 4343u64;
        app.register_pending_remote_split(
            request_id,
            crate::app::creation::PendingRemoteSplit {
                workspace_id: remote_workspace_id.clone(),
                target_pane_id: crate::layout::PaneId::from_raw(1),
                direction: ratatui::layout::Direction::Horizontal,
                ratio: 0.5,
                focus: false,
                origin: crate::remote::federation::id::HostKey::new("remote-host", "s1"),
            },
        );
        assert!(app.pending_remote_splits.contains_key(&request_id));

        let response = app.handle_workspace_close(
            "close-1".to_string(),
            WorkspaceCloseParams {
                workspace_id: remote_workspace_id,
                close_group: false,
            },
        );
        let decoded: SuccessResponse =
            serde_json::from_str(&response).expect("workspace.close must succeed");
        assert!(matches!(decoded.result, ResponseResult::Ok {}));

        assert!(
            app.state.remote_mirrors.is_empty(),
            "closing the federated workspace must end its mount, not just remove UI state"
        );
        assert!(
            app.state.mount_drive_tasks.is_empty(),
            "the mount's drive task must be signaled/cancelled on close, not left running"
        );
        assert!(
            !app.pending_remote_splits.contains_key(&request_id),
            "pending splits for the closed workspace must be purged before removal"
        );
        assert_eq!(app.state.workspaces.len(), 1);
        assert!(
            app.state
                .workspaces
                .iter()
                .all(|ws| ws.worktree_space().is_none())
        );
    }

    /// Every workspace of a mount shares one worktree-space key
    /// (`federation:<host_key>`), so a close path that treats that group as
    /// the unit of closure destroys the whole mount. Live-observed: closing
    /// one of four mirrored workspaces removed all four and killed the link.
    #[cfg(unix)]
    #[tokio::test]
    async fn closing_one_of_several_federated_workspaces_keeps_siblings_and_mount() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            event_hub.clone(),
        );
        app.state.workspaces = vec![Workspace::test_new("local")];
        app.state.active = Some(0);
        app.state.selected = 0;

        let mirror = test_federation_mirror_with_workspaces("remote-host", 1, 3);
        let (guard, tunnel_reader, tunnel_writer) = spawn_test_tunnel().await;
        app.handle_federation_mount_ready(crate::events::FederationMountReady {
            target: "remote-host".to_string(),
            mirror,
            generation: 1,
            tunnel_guard: guard,
            tunnel_reader,
            tunnel_writer,
        });
        assert_eq!(
            app.state.workspaces.len(),
            4,
            "the mount must materialize all three remote workspaces"
        );

        // `Workspace::test_new` attaches a terminal it never registers, so
        // the seeded local workspace needs one before the state invariants
        // can be asserted at all.
        app.state.ensure_test_terminals();

        let host_key = crate::remote::federation::id::HostKey::new("remote-host", "s1");
        let space_key = format!("federation:{}", host_key.as_str());
        // Close the middle mirrored workspace; its two siblings must live on.
        let closing_workspace_id = app.public_workspace_id(2);
        let surviving_ids = [
            app.state.workspaces[1].id.clone(),
            app.state.workspaces[3].id.clone(),
        ];

        let response = app.handle_workspace_close(
            "close-mid".to_string(),
            WorkspaceCloseParams {
                workspace_id: closing_workspace_id,
                close_group: false,
            },
        );
        let decoded: SuccessResponse =
            serde_json::from_str(&response).expect("workspace.close must succeed");
        assert!(matches!(decoded.result, ResponseResult::Ok {}));

        assert_eq!(
            app.state.workspaces.len(),
            3,
            "closing one mirrored workspace must remove exactly one, not the whole mount group"
        );
        for id in &surviving_ids {
            assert!(
                app.state.workspaces.iter().any(|ws| &ws.id == id),
                "sibling workspace {id} of the same mount must survive"
            );
        }
        assert_eq!(
            app.state
                .workspaces
                .iter()
                .filter(|ws| ws.worktree_space().is_some_and(|s| s.key == space_key))
                .count(),
            2,
            "surviving siblings must keep their federation grouping"
        );
        assert!(
            app.state.remote_mirrors.contains_key(&host_key),
            "the mount must stay registered while it still owns workspaces"
        );
        assert!(
            app.state.mount_drive_tasks.contains_key(&host_key),
            "the mount's drive task must not be aborted while siblings still mirror it"
        );
        app.state.assert_invariants_for_test();
    }

    /// The last-workspace case the original close path was written for: with
    /// no sibling left, the mount itself must still be torn down.
    #[cfg(unix)]
    #[tokio::test]
    async fn closing_the_final_federated_workspace_of_a_mount_ends_it() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            event_hub.clone(),
        );
        app.state.workspaces = vec![Workspace::test_new("local")];
        app.state.active = Some(0);
        app.state.selected = 0;

        let mirror = test_federation_mirror_with_workspaces("remote-host", 1, 2);
        let (guard, tunnel_reader, tunnel_writer) = spawn_test_tunnel().await;
        app.handle_federation_mount_ready(crate::events::FederationMountReady {
            target: "remote-host".to_string(),
            mirror,
            generation: 1,
            tunnel_guard: guard,
            tunnel_reader,
            tunnel_writer,
        });
        assert_eq!(app.state.workspaces.len(), 3);
        app.state.ensure_test_terminals();
        let host_key = crate::remote::federation::id::HostKey::new("remote-host", "s1");

        let first = app.public_workspace_id(1);
        let response = app.handle_workspace_close(
            "close-1".to_string(),
            WorkspaceCloseParams {
                workspace_id: first,
                close_group: false,
            },
        );
        let decoded: SuccessResponse =
            serde_json::from_str(&response).expect("workspace.close must succeed");
        assert!(matches!(decoded.result, ResponseResult::Ok {}));
        assert!(
            app.state.remote_mirrors.contains_key(&host_key),
            "one mirrored workspace still remains, so the mount must stay live"
        );

        let last = app.public_workspace_id(1);
        let response = app.handle_workspace_close(
            "close-2".to_string(),
            WorkspaceCloseParams {
                workspace_id: last,
                close_group: false,
            },
        );
        let decoded: SuccessResponse =
            serde_json::from_str(&response).expect("workspace.close must succeed");
        assert!(matches!(decoded.result, ResponseResult::Ok {}));

        assert!(
            app.state.remote_mirrors.is_empty(),
            "closing the mount's last workspace must deregister the mount"
        );
        assert!(
            app.state.mount_drive_tasks.is_empty(),
            "closing the mount's last workspace must cancel its drive task"
        );
        assert_eq!(app.state.workspaces.len(), 1);
        assert!(
            app.state
                .workspaces
                .iter()
                .all(|ws| ws.worktree_space().is_none())
        );
        app.state.assert_invariants_for_test();
    }

    /// The resync indexes are per-workspace bookkeeping, so a close must
    /// purge only the closing workspace's entries. Purging by mount group
    /// (every workspace sharing `federation:<host_key>`) blinds the still-
    /// mounted siblings to their own panes/tabs.
    #[cfg(unix)]
    #[tokio::test]
    async fn closing_one_federated_workspace_purges_only_its_own_resync_entries() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            event_hub.clone(),
        );
        app.state.workspaces = vec![Workspace::test_new("local")];
        app.state.active = Some(0);
        app.state.selected = 0;

        let mirror = test_federation_mirror_with_workspaces("remote-host", 1, 2);
        let (guard, tunnel_reader, tunnel_writer) = spawn_test_tunnel().await;
        app.handle_federation_mount_ready(crate::events::FederationMountReady {
            target: "remote-host".to_string(),
            mirror,
            generation: 1,
            tunnel_guard: guard,
            tunnel_reader,
            tunnel_writer,
        });
        assert_eq!(app.state.workspaces.len(), 3);
        app.state.ensure_test_terminals();
        let host_key = crate::remote::federation::id::HostKey::new("remote-host", "s1");
        // Mirrored entities are indexed under their mount-namespaced ids.
        let ns = |raw: &str| format!("r:{}:{raw}", host_key.as_str());

        // The mount indexes its own panes and tabs; the workspace index only
        // holds host-announced workspaces awaiting materialization, so seed
        // one entry per mirrored workspace by hand.
        for raw in ["w1", "w2"] {
            app.remote_resync_workspace_index.insert(
                ns(raw),
                crate::app::creation::RemoteWorkspaceRef {
                    origin: host_key.clone(),
                    label: format!("{raw} label"),
                },
            );
        }
        assert!(app.remote_resync_pane_index.contains_key(&ns("w1-p1")));
        assert!(app.remote_resync_pane_index.contains_key(&ns("w2-p1")));
        assert!(app.remote_resync_tab_index.contains_key(&ns("w1-tab")));
        assert!(app.remote_resync_tab_index.contains_key(&ns("w2-tab")));

        let closing_workspace_id = app.public_workspace_id(1);
        let response = app.handle_workspace_close(
            "close-1".to_string(),
            WorkspaceCloseParams {
                workspace_id: closing_workspace_id,
                close_group: false,
            },
        );
        let decoded: SuccessResponse =
            serde_json::from_str(&response).expect("workspace.close must succeed");
        assert!(matches!(decoded.result, ResponseResult::Ok {}));

        assert!(
            !app.remote_resync_pane_index.contains_key(&ns("w1-p1")),
            "the closed workspace's pane index entry must be purged"
        );
        assert!(
            app.remote_resync_pane_index.contains_key(&ns("w2-p1")),
            "a sibling workspace's pane index entry must survive"
        );
        assert!(
            !app.remote_resync_tab_index.contains_key(&ns("w1-tab")),
            "the closed workspace's tab index entry must be purged"
        );
        assert!(
            app.remote_resync_tab_index.contains_key(&ns("w2-tab")),
            "a sibling workspace's tab index entry must survive"
        );
        assert!(
            !app.remote_resync_workspace_index.contains_key(&ns("w1")),
            "the closed workspace's workspace index entry must be purged"
        );
        assert!(
            app.remote_resync_workspace_index.contains_key(&ns("w2")),
            "a sibling workspace's workspace index entry must survive"
        );
        app.state.assert_invariants_for_test();
    }

    /// Guard against over-narrowing the federated close: a genuine shared
    /// (non-linked) worktree space still closes as one group.
    #[test]
    fn api_workspace_close_still_closes_a_whole_shared_worktree_space_group() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![
            Workspace::test_new("main"),
            Workspace::test_new("sibling"),
            Workspace::test_new("unrelated"),
        ];
        for idx in 0..2 {
            app.state.workspaces[idx].worktree_space =
                Some(crate::workspace::WorktreeSpaceMembership {
                    key: "repo-key".into(),
                    label: "herdr".into(),
                    repo_root: "/repo/herdr".into(),
                    checkout_path: "/repo/herdr".into(),
                    is_linked_worktree: false,
                });
        }
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.ensure_test_terminals();
        let unrelated_id = app.state.workspaces[2].id.clone();

        let response = app.handle_workspace_close(
            "req".into(),
            WorkspaceCloseParams {
                workspace_id: app.state.workspaces[0].id.clone(),
                close_group: false,
            },
        );
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(success.id, "req");

        assert_eq!(
            app.state.workspaces.len(),
            1,
            "both members of the shared worktree space must close together"
        );
        assert_eq!(app.state.workspaces[0].id, unrelated_id);
        app.state.assert_invariants_for_test();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn federation_mount_ended_stale_generation_is_ignored() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            event_hub.clone(),
        );
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
            crate::remote::federation::client::MountConnectionEpoch::UNMOUNTED,
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
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            event_hub.clone(),
        );
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
            crate::remote::federation::client::MountConnectionEpoch::UNMOUNTED,
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
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            event_hub.clone(),
        );
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
        assert_eq!(
            app.state.workspaces.len(),
            2,
            "local-a plus the materialized federation workspace"
        );

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
            crate::remote::federation::client::MountConnectionEpoch::UNMOUNTED,
            "remote-host".to_string(),
            "link closed".to_string(),
        );

        assert_eq!(app.state.workspaces.len(), 2, "local-a and local-b remain");
        assert_eq!(
            app.state
                .workspaces
                .get(app.state.selected)
                .map(|ws| ws.id.clone()),
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
