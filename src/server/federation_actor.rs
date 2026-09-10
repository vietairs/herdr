//! Federation command actor seam for the live server (P9.2b b0.1).
//!
//! A co-located federation connection cannot borrow the live `App` directly:
//! `HeadlessServer` owns it by value and holds `&mut self` across its whole
//! event loop, and `App` is `!Send`. Instead a federation connection sends a
//! [`FederationCommand`] through the same `ServerEvent` mpsc the classic client
//! path already uses; the server loop is the single `&mut self` dispatch point,
//! so servicing a command needs no second `App`, no lock, and no competing
//! event consumer.
//!
//! Each command that produces a value carries a `oneshot::Sender` for its
//! reply, so the connection worker can `await` the result without touching the
//! `App`. Values that leave the actor — a session snapshot, an event slice, a
//! `broadcast::Receiver` of a pane's output bytes — are all owned or `Clone`,
//! never a borrow of `App`, so they move to the worker cleanly.
//!
//! These handlers mirror the existing `AppFederationHost` (`remote::federation::
//! serve`) but run against the *live* `App`, and — crucially — go through
//! [`App::handle_api_request_after_internal_events_drained`] rather than
//! [`App::handle_api_request`]. The live server's event loop already drains
//! `App`'s internal events through its *forwarding-aware* path each tick, so
//! this actor must NOT re-drain (the non-`_after_` variant would bypass client
//! forwarding). No handler awaits an `AppEvent` before replying.
//!
//! Dormant until b0.4 exposes a federation listener that constructs these
//! commands; annotated `#[allow(dead_code)]` in the meantime, matching the
//! federation module's existing dormant-until-wired precedent (`id::map_out`,
//! `id::strip_mount_namespace`).

use bytes::Bytes;
use tokio::sync::{broadcast, oneshot};

use crate::api::schema::common::AgentStatus;
use crate::api::schema::events::EventKind;
use crate::api::schema::session::SessionSnapshot;
use crate::api::schema::{EmptyParams, Method, Request, ResponseResult, SuccessResponse};
use crate::app::App;
use crate::remote::federation::protocol::EventCursor;
use crate::remote::federation::serve::empty_snapshot;
use crate::server::federation_lease::{AcceptEpoch, Admission, ConnId, FederationLease};

/// A request from a co-located federation connection to be serviced against the
/// live `App` (and the single-controller [`FederationLease`]) on the server
/// event loop. Read-only queries and the two remote-input forwards
/// (`SendInput`/`Resize`) mirror the federation protocol host surface; each
/// value-producing variant carries its reply channel.
///
/// Lease-bearing variants carry the connection's `(epoch, connid)` so admission,
/// mount promotion, and per-command authorization are all linearized against the
/// live lease at the one dispatch point — a stale connection's command can
/// neither acquire, mount, nor mutate (v5 finding #1).
#[allow(dead_code)] // dormant until b0.4 wires the federation listener
pub(crate) enum FederationCommand {
    /// Reserve the single-controller slot for a freshly-accepted connection
    /// registered at `epoch`. The reply carries the [`Admission`] outcome.
    AcquireController {
        epoch: AcceptEpoch,
        connid: ConnId,
        reply: oneshot::Sender<Admission>,
    },
    /// Promote this connection's reservation to `Mounted` and, on success,
    /// return the atomic (snapshot, cursor). Atomic for free here: the actor
    /// holds `&mut App` exclusively, so no event can slip between the snapshot
    /// and the cursor read. `None` if the reservation is stale or absent.
    Mount {
        epoch: AcceptEpoch,
        connid: ConnId,
        reply: oneshot::Sender<Option<(SessionSnapshot, EventCursor)>>,
    },
    /// Release the lease on connection EOF (compare-and-clear; a late EOF from a
    /// superseded connection is inert).
    Release { epoch: AcceptEpoch, connid: ConnId },
    /// Post-mount pane mirroring fix (plans/260722-1327): produce a fresh
    /// (snapshot, cursor) pair on demand, answering an in-band
    /// `SnapshotRequest` from an already-mounted client. Read-only (unlike
    /// `Mount`, this never touches the lease) — any connection that can
    /// reach the reader loop at all may ask for a resync, mirroring
    /// `EventsAfter`'s no-lease-check precedent.
    Snapshot(oneshot::Sender<(SessionSnapshot, EventCursor)>),
    /// Events strictly after the given sequence number.
    EventsAfter(u64, oneshot::Sender<Vec<(u64, EventKind)>>),
    /// A subscription to one live terminal's raw output bytes, or `None` if the
    /// terminal id is unknown. Dropping the receiver never affects the PTY.
    SubscribeOutput(String, oneshot::Sender<Option<broadcast::Receiver<Bytes>>>),
    /// The scrollback history (ANSI) to seed a newly opened remote pane.
    ScrollbackReplay(String, oneshot::Sender<Vec<u8>>),
    /// Forward input bytes to a live terminal — dropped unless `(epoch, connid)`
    /// is the mounted controller. Fire-and-forget.
    SendInput {
        epoch: AcceptEpoch,
        connid: ConnId,
        terminal_id: String,
        bytes: Vec<u8>,
    },
    /// Resize a live terminal — dropped unless `(epoch, connid)` is the mounted
    /// controller. Fire-and-forget.
    Resize {
        epoch: AcceptEpoch,
        connid: ConnId,
        terminal_id: String,
        cols: u16,
        rows: u16,
    },
    /// Force a live terminal's child to repaint its full screen, by briefly
    /// jiggling the PTY window size so the child sees a `SIGWINCH` it must
    /// answer. Dropped unless `(epoch, connid)` is the mounted controller
    /// (it drives a real ioctl on the host's PTY). Fire-and-forget.
    ///
    /// A mounted client cannot repaint a remote pane by itself: its local
    /// mirror deliberately skips the replay-recovery heuristic (see
    /// `PaneTerminal::resize`'s `is_remote_backed` gate) because the serving
    /// host's screen is the sole source of truth, and the only full-paint
    /// frame in the protocol (`Open{replay}`) is emitted once per terminal
    /// and is empty for alternate-screen apps. Making the child itself
    /// repaint is therefore the only payload that works on every screen
    /// mode, and it needs no protocol change: the bytes reach the client
    /// through the ordinary output stream.
    NudgeRedraw {
        epoch: AcceptEpoch,
        connid: ConnId,
        terminal_id: String,
    },
    /// The mount stopped mirroring this terminal, so it no longer owns the
    /// terminal's size and the host may drive it again. Paired with the claim
    /// each `Resize`/`NudgeRedraw` takes; without it a terminal the mount
    /// opened once stays frozen at the mount's geometry until it unmounts.
    ReleaseTerminalSize {
        epoch: AcceptEpoch,
        connid: ConnId,
        terminal_id: String,
    },
    /// Current per-terminal agent statuses, paired with the identified
    /// agent's canonical label (`AgentInfo.agent`, e.g. `"claude"`) so the
    /// relay can populate `AgentStatusMessage::agent` for remote-mirrored
    /// panes on the client end (`None` when this host has not identified an
    /// agent for that terminal yet).
    AgentStatuses(oneshot::Sender<Vec<(String, AgentStatus, Option<String>)>>),
    /// Performs a real split of `target_pane_id` (a raw, un-namespaced
    /// remote pane id) on this host's own live workspace, mirroring
    /// `AppFederationHost::split_pane`'s contract but going through the
    /// live `App` instead of a `FederationHost` implementor (the co-located
    /// accept path never constructs one — see `federation_accept.rs`'s doc
    /// comment). Reuses the exact same JSON-API method the local TUI/CLI
    /// split action calls (`Method::PaneSplit`) rather than duplicating
    /// `Workspace::split_pane`'s logic, so remote-origin splits get the
    /// same validation/eventing/session-save behavior as a local split.
    SplitPane {
        target_pane_id: String,
        direction: crate::remote::federation::protocol::SplitDirection,
        ratio: Option<f32>,
        focus: bool,
        reply: oneshot::Sender<Result<(String, String), String>>,
    },
    /// Performs a real close of `target_pane_id` (a raw, un-namespaced
    /// remote pane id) on this host's own live workspace — the serving-host
    /// half of Gap A (plans/260724-1536-federation-pane-close-sync): a
    /// mounting client's pane-close action must tear down the pane that
    /// actually lives here, not just the client's local mirror. Reuses the
    /// same JSON-API method the local TUI/CLI close action calls
    /// (`Method::PaneClose`), same reasoning as `SplitPane` above.
    ClosePane {
        target_pane_id: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// Closes exactly one LOCAL workspace on this host's own live session —
    /// the serving-host half of close forwarding for the multi-workspace
    /// case: a mounting client's workspace-close action must tear down the
    /// workspace that actually lives here, never the mount's whole worktree
    /// group. Gated on `(epoch, connid)` being the mounted controller,
    /// unlike `SplitPane`/`ClosePane` above (a pre-existing gap those arms do
    /// not close; this arm does not repeat it). Reuses
    /// `App::close_federation_target_workspace`, which detaches the target
    /// from its worktree-space membership before closing so a sibling
    /// workspace sharing that membership is never touched.
    CloseWorkspaceRemote {
        epoch: AcceptEpoch,
        connid: ConnId,
        target_workspace_id: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// Closes exactly one LOCAL tab on this host's own live session — the
    /// serving-host half of close forwarding for the multi-tab case. Gated
    /// on `(epoch, connid)` being the mounted controller, same reasoning as
    /// `CloseWorkspaceRemote`. Reuses `App::close_federation_target_tab`,
    /// which never routes through the confirm-gated worktree-group close
    /// path, so a remote peer's request can never pop this host's own
    /// confirmation UI.
    CloseTabRemote {
        epoch: AcceptEpoch,
        connid: ConnId,
        target_tab_id: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// Creates a brand new workspace on this host's own live session — the
    /// serving-host half of multi-workspace federation: a mounting client's
    /// "new workspace" action performed inside a mounted workspace must grow
    /// the workspace set that actually lives here, not just the client's
    /// local mirror. Reuses the same JSON-API method the local TUI/CLI
    /// new-workspace action calls (`Method::WorkspaceCreate`).
    ///
    /// Gated on `(epoch, connid)` being the mounted controller, same as
    /// `CloseWorkspaceRemote`/`CloseTabRemote`: only the peer that actually
    /// holds the mount may grow this host's workspace set.
    ///
    /// `label` is the client's optional hint; `cwd` is deliberately not
    /// requestable (a client-side path is meaningless here), so this host's
    /// own `workspace.create` defaults decide it. The reply carries the raw
    /// (un-namespaced) `(workspace_id, tab_id, pane_id, terminal_id)` of the
    /// new workspace's root pane.
    CreateWorkspace {
        epoch: AcceptEpoch,
        connid: ConnId,
        label: Option<String>,
        #[allow(clippy::type_complexity)]
        // one tuple of four ids; a named struct would be single-use
        reply: oneshot::Sender<Result<(String, String, String, String), String>>,
    },
    /// Creates a brand new tab inside one of this host's own workspaces — the
    /// serving-host half of federation-forwarded tab creation: a mounting
    /// client's "new tab" action taken while a mounted workspace is in focus
    /// must grow the workspace where it really lives, not spawn a local shell
    /// stamped with a remote-looking id. Reuses the same JSON-API method the
    /// local TUI/CLI new-tab action calls (`Method::TabCreate`).
    ///
    /// Named `CreateTab`, not `CreateTabRemote`: the `*Remote` suffix on
    /// `CloseTabRemote` marks the *close* verb's local/remote fork, which
    /// create does not have.
    ///
    /// Gated on `(epoch, connid)` being the mounted controller, same as
    /// `CloseTabRemote`/`CreateWorkspace`: only the peer that actually holds
    /// the mount may grow this host's tab set.
    ///
    /// `target_workspace_id` is the workspace's own id on this host, as this
    /// host published it; `label` is the client's optional hint, already
    /// sanitized and clamped at ingress. `cwd`, `env` and `focus` are
    /// deliberately not requestable — a client-side path is meaningless here,
    /// a client-supplied environment is remote code execution on this host,
    /// and a client-supplied focus would move this user's own screen. The
    /// reply carries the raw (un-namespaced)
    /// `(workspace_id, tab_id, pane_id, terminal_id)` of the new tab's root
    /// pane, so the mount client never has to guess which workspace it
    /// landed in.
    CreateTab {
        epoch: AcceptEpoch,
        connid: ConnId,
        target_workspace_id: String,
        label: Option<String>,
        #[allow(clippy::type_complexity)]
        // one tuple of four ids; a named struct would be single-use
        reply: oneshot::Sender<Result<(String, String, String, String), String>>,
    },
}

// `ServerEvent` derives `Debug`, so its `Federation` variant needs one — but the
// reply/subscription channels are not `Debug`. Print the variant and its plain
// fields only, never the channels.
impl std::fmt::Debug for FederationCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FederationCommand::AcquireController { epoch, connid, .. } => {
                write!(f, "AcquireController(e{epoch}, c{connid})")
            }
            FederationCommand::Mount { epoch, connid, .. } => {
                write!(f, "Mount(e{epoch}, c{connid})")
            }
            FederationCommand::Release { epoch, connid } => {
                write!(f, "Release(e{epoch}, c{connid})")
            }
            FederationCommand::Snapshot(_) => f.write_str("Snapshot"),
            FederationCommand::EventsAfter(since, _) => write!(f, "EventsAfter({since})"),
            FederationCommand::SubscribeOutput(id, _) => write!(f, "SubscribeOutput({id})"),
            FederationCommand::ScrollbackReplay(id, _) => write!(f, "ScrollbackReplay({id})"),
            FederationCommand::SendInput {
                terminal_id, bytes, ..
            } => {
                write!(f, "SendInput({terminal_id}, {} bytes)", bytes.len())
            }
            FederationCommand::Resize {
                terminal_id,
                cols,
                rows,
                ..
            } => write!(f, "Resize({terminal_id}, {cols}x{rows})"),
            FederationCommand::NudgeRedraw { terminal_id, .. } => {
                write!(f, "NudgeRedraw({terminal_id})")
            }
            FederationCommand::ReleaseTerminalSize { terminal_id, .. } => {
                write!(f, "ReleaseTerminalSize({terminal_id})")
            }
            FederationCommand::AgentStatuses(_) => f.write_str("AgentStatuses"),
            FederationCommand::SplitPane { target_pane_id, .. } => {
                write!(f, "SplitPane({target_pane_id})")
            }
            FederationCommand::ClosePane { target_pane_id, .. } => {
                write!(f, "ClosePane({target_pane_id})")
            }
            FederationCommand::CloseWorkspaceRemote {
                target_workspace_id,
                ..
            } => {
                write!(f, "CloseWorkspaceRemote({target_workspace_id})")
            }
            FederationCommand::CloseTabRemote { target_tab_id, .. } => {
                write!(f, "CloseTabRemote({target_tab_id})")
            }
            FederationCommand::CreateWorkspace { label, .. } => {
                write!(f, "CreateWorkspace({label:?})")
            }
            FederationCommand::CreateTab { epoch, connid, .. } => {
                write!(f, "CreateTab(e{epoch}, c{connid})")
            }
        }
    }
}

/// Services one [`FederationCommand`] against the live `App` and the
/// single-controller `lease`. Called only from the server event loop's single
/// `&mut self` dispatch point, so lease admission/mount/authorization and the
/// `App` reads it gates are linearized with live-handoff revocation (which runs
/// on the same loop). A dropped reply receiver (worker gone) is ignored — the
/// `send` result is discarded.
#[allow(dead_code)] // dormant until b0.4 wires the federation listener
pub(crate) fn dispatch(app: &mut App, lease: &mut FederationLease, command: FederationCommand) {
    dispatch_command(app, lease, command);
    sync_terminal_size_ownership(app, lease);
}

/// Drop every federation size claim once no controller is mounted. Called
/// after every lease mutation — dispatch here and the handoff revoke/reopen
/// pair in `headless.rs` — so the lease stays the single source of truth and
/// ownership can never latch on past a revocation.
pub(crate) fn sync_terminal_size_ownership(app: &mut App, lease: &FederationLease) {
    if !lease.has_mounted_controller() {
        app.state.federation_owned_terminal_sizes.clear();
    }
}

/// Records that the mounted controller drives `terminal_id`'s size, so this
/// host's render loop and any direct attach client stop resizing it.
///
/// Resolved through `resolve_terminal_target`, the same way the resize it
/// accompanies is, so every target form that can be resized can also be
/// claimed.
fn claim_terminal_size(app: &mut App, terminal_id: &str) {
    if let Some(claimed) = resolve_owned_terminal_id(app, terminal_id) {
        app.state.federation_owned_terminal_sizes.insert(claimed);
    }
}

/// Drops the mount's claim on `terminal_id` — it stopped driving that
/// terminal, so the host may size it again. Reopening re-claims it.
fn release_terminal_size(app: &mut App, terminal_id: &str) {
    if let Some(released) = resolve_owned_terminal_id(app, terminal_id) {
        app.state.federation_owned_terminal_sizes.remove(&released);
    }
}

fn resolve_owned_terminal_id(app: &App, terminal_id: &str) -> Option<crate::terminal::TerminalId> {
    let resolved = app.resolve_terminal_target(terminal_id).ok()?;
    app.state
        .terminals
        .keys()
        .find(|id| id.to_string() == resolved.terminal_id)
        .cloned()
}

fn dispatch_command(app: &mut App, lease: &mut FederationLease, command: FederationCommand) {
    match command {
        FederationCommand::AcquireController {
            epoch,
            connid,
            reply,
        } => {
            let _ = reply.send(lease.try_acquire(epoch, connid));
        }
        FederationCommand::Mount {
            epoch,
            connid,
            reply,
        } => {
            // Promote the reservation first; only the current-epoch holder mounts.
            // A stale or non-holding Mount replies `None` and touches no `App`.
            if !lease.try_mount(epoch, connid) {
                let _ = reply.send(None);
                return;
            }
            let (snapshot, cursor) = current_snapshot(app);
            let _ = reply.send(Some((snapshot, cursor)));
        }
        FederationCommand::Release { epoch, connid } => {
            lease.release(epoch, connid);
        }
        FederationCommand::Snapshot(reply) => {
            let _ = reply.send(current_snapshot(app));
        }
        FederationCommand::EventsAfter(since, reply) => {
            let events = app
                .event_hub
                .events_after(since)
                .into_iter()
                .map(|(seq, envelope)| (seq, envelope.event))
                .collect();
            let _ = reply.send(events);
        }
        FederationCommand::SubscribeOutput(terminal_id, reply) => {
            let subscription = app
                .terminal_runtime_for_terminal_id(&terminal_id)
                .map(|runtime| runtime.subscribe_output_bytes());
            let _ = reply.send(subscription);
        }
        FederationCommand::ScrollbackReplay(terminal_id, reply) => {
            let replay = scrollback_replay(app, &terminal_id);
            let _ = reply.send(replay);
        }
        FederationCommand::SendInput {
            epoch,
            connid,
            terminal_id,
            bytes,
        } => {
            // Only the mounted controller may drive input. A stale or
            // non-controller forward is dropped before it reaches the PTY.
            if !lease.is_mounted_controller(epoch, connid) {
                return;
            }
            if let Some(runtime) = app.terminal_runtime_for_terminal_id(&terminal_id) {
                let _ = runtime.try_send_bytes(Bytes::copy_from_slice(&bytes));
            }
        }
        FederationCommand::Resize {
            epoch,
            connid,
            terminal_id,
            cols,
            rows,
        } => {
            if !lease.is_mounted_controller(epoch, connid) {
                return;
            }
            if let Some(runtime) = app.terminal_runtime_for_terminal_id(&terminal_id) {
                runtime.resize(rows, cols, 0, 0);
                // Unconditional, including when `resize` deduped the call: a
                // size that is already current here still leaves the mounted
                // client's mirror freshly reflowed and unpainted, and that leg
                // sends no `SIGWINCH` at all.
                nudge_child_redraw(runtime);
                claim_terminal_size(app, &terminal_id);
            }
        }
        FederationCommand::NudgeRedraw {
            epoch,
            connid,
            terminal_id,
        } => {
            if !lease.is_mounted_controller(epoch, connid) {
                return;
            }
            if let Some(runtime) = app.terminal_runtime_for_terminal_id(&terminal_id) {
                nudge_child_redraw(runtime);
                claim_terminal_size(app, &terminal_id);
            }
        }
        FederationCommand::ReleaseTerminalSize {
            epoch,
            connid,
            terminal_id,
        } => {
            if !lease.is_mounted_controller(epoch, connid) {
                return;
            }
            release_terminal_size(app, &terminal_id);
        }
        FederationCommand::AgentStatuses(reply) => {
            let response = app.handle_api_request_after_internal_events_drained(Request {
                id: "federation-agent-list".to_string(),
                method: Method::AgentList(EmptyParams::default()),
            });
            let statuses = serde_json::from_str::<SuccessResponse>(&response)
                .ok()
                .and_then(|success| match success.result {
                    ResponseResult::AgentList { agents } => Some(
                        agents
                            .into_iter()
                            .map(|agent| (agent.terminal_id, agent.agent_status, agent.agent))
                            .collect(),
                    ),
                    _ => None,
                })
                .unwrap_or_default();
            let _ = reply.send(statuses);
        }
        FederationCommand::SplitPane {
            target_pane_id,
            direction,
            ratio,
            focus,
            reply,
        } => {
            let direction = match direction {
                crate::remote::federation::protocol::SplitDirection::Right => {
                    crate::api::schema::SplitDirection::Right
                }
                crate::remote::federation::protocol::SplitDirection::Down => {
                    crate::api::schema::SplitDirection::Down
                }
            };
            let response = app.handle_api_request_after_internal_events_drained(Request {
                id: "federation-split-pane".to_string(),
                method: Method::PaneSplit(crate::api::schema::PaneSplitParams {
                    workspace_id: None,
                    target_pane_id: Some(target_pane_id),
                    direction,
                    ratio,
                    cwd: None,
                    focus,
                    right_click: Default::default(),
                    env: std::collections::HashMap::new(),
                }),
            });
            let outcome = serde_json::from_str::<SuccessResponse>(&response)
                .ok()
                .and_then(|success| match success.result {
                    ResponseResult::PaneInfo { pane } => Some(Ok((pane.pane_id, pane.terminal_id))),
                    _ => None,
                })
                .unwrap_or_else(|| {
                    let reason = serde_json::from_str::<serde_json::Value>(&response)
                        .ok()
                        .and_then(|value| {
                            value
                                .get("error")
                                .and_then(|error| error.get("message"))
                                .and_then(|message| message.as_str())
                                .map(str::to_string)
                        })
                        .unwrap_or_else(|| "pane split failed".to_string());
                    Err(reason)
                });
            let _ = reply.send(outcome);
        }
        FederationCommand::ClosePane {
            target_pane_id,
            reply,
        } => {
            let response = app.handle_api_request_after_internal_events_drained(Request {
                id: "federation-close-pane".to_string(),
                method: Method::PaneClose(crate::api::schema::PaneTarget {
                    pane_id: target_pane_id,
                }),
            });
            let outcome = serde_json::from_str::<SuccessResponse>(&response)
                .ok()
                .map(|_success| Ok(()))
                .unwrap_or_else(|| {
                    let error_value = serde_json::from_str::<serde_json::Value>(&response)
                        .ok()
                        .and_then(|value| value.get("error").cloned());
                    let code = error_value
                        .as_ref()
                        .and_then(|error| error.get("code"))
                        .and_then(|code| code.as_str())
                        .unwrap_or("pane_close_failed");
                    let message = error_value
                        .as_ref()
                        .and_then(|error| error.get("message"))
                        .and_then(|message| message.as_str())
                        .unwrap_or("pane close failed");
                    // Prefix the reason with the JSON-API's own `error.code`
                    // (a stable, independently-tested identifier) rather than
                    // handing back only the freeform `message`. `client.rs`'s
                    // idempotent-retry classification matches this `code:`
                    // prefix instead of sniffing the human-readable message
                    // text for "not found" — two independently-editable
                    // strings on either side of the wire that could
                    // otherwise drift out of sync.
                    Err(format!("{code}: {message}"))
                });
            let _ = reply.send(outcome);
        }
        FederationCommand::CloseWorkspaceRemote {
            epoch,
            connid,
            target_workspace_id,
            reply,
        } => {
            // Only the mounted controller may close a workspace on this
            // host. Unlike `ClosePane`/`SplitPane`/`CreateWorkspace` above,
            // this is a newly added arm with no pre-existing gap to inherit.
            if !lease.is_mounted_controller(epoch, connid) {
                let _ = reply.send(Err("not the mounted controller".to_string()));
                return;
            }
            let outcome = app.close_federation_target_workspace(&target_workspace_id);
            let _ = reply.send(outcome);
        }
        FederationCommand::CloseTabRemote {
            epoch,
            connid,
            target_tab_id,
            reply,
        } => {
            if !lease.is_mounted_controller(epoch, connid) {
                let _ = reply.send(Err("not the mounted controller".to_string()));
                return;
            }
            let outcome = app.close_federation_target_tab(&target_tab_id);
            let _ = reply.send(outcome);
        }
        FederationCommand::CreateWorkspace {
            epoch,
            connid,
            label,
            reply,
        } => {
            // Only the mounted controller may create a workspace on this host.
            if !lease.is_mounted_controller(epoch, connid) {
                let _ = reply.send(Err("not the mounted controller".to_string()));
                return;
            }
            let response = app.handle_api_request_after_internal_events_drained(Request {
                id: "federation-create-workspace".to_string(),
                method: Method::WorkspaceCreate(crate::api::schema::WorkspaceCreateParams {
                    // A remotely requested workspace has no local source
                    // workspace to inherit a `follow` cwd policy from.
                    source_workspace_id: None,
                    // A mounting client's filesystem path is meaningless
                    // here; let this host's own `workspace.create` defaults
                    // pick the root pane's cwd.
                    cwd: None,
                    // Never steal this host's own focus for a remotely
                    // requested workspace — the requesting client focuses
                    // its own mirror of it, this host's user did not ask
                    // for anything.
                    focus: false,
                    label,
                    env: std::collections::HashMap::new(),
                }),
            });
            let outcome = serde_json::from_str::<SuccessResponse>(&response)
                .ok()
                .map(|success| match success.result {
                    ResponseResult::WorkspaceCreated {
                        workspace,
                        tab,
                        root_pane,
                    } => Ok((
                        workspace.workspace_id,
                        tab.tab_id,
                        root_pane.pane_id,
                        root_pane.terminal_id,
                    )),
                    // This host answered its own `workspace.create` by
                    // redirecting it onto a host *it* has mounted (the
                    // no-`cwd` redirect in `handle_workspace_create`), so no
                    // workspace was created here and none ever will be: what
                    // the peer asked for — a workspace on THIS host — did not
                    // happen, and the workspace that does appear lives on a
                    // third host the peer never addressed. Reported under its
                    // own code so it is never confused with a real create
                    // failure, which is what the generic error path below
                    // used to call it.
                    ResponseResult::WorkspaceCreateRequested { origin } => {
                        tracing::warn!(
                            %origin,
                            "a peer's federation workspace-create was redirected onto a \
                             host this one mounts; refusing it instead of reporting a \
                             workspace the peer cannot reach"
                        );
                        Err(
                            "workspace_create_redirected: this host redirected the create \
                             onto a host it mounts, so no workspace was created here"
                                .to_string(),
                        )
                    }
                    // No other success shape can carry the ids the peer needs.
                    // Reported distinctly rather than as a create failure: the
                    // create's real outcome is unknown to this reply path.
                    other => {
                        tracing::warn!(
                            ?other,
                            "unexpected success result for a peer's federation \
                             workspace-create request"
                        );
                        Err(
                            "workspace_create_unexpected_result: this host answered the \
                             create with a result carrying no workspace ids"
                                .to_string(),
                        )
                    }
                })
                .unwrap_or_else(|| {
                    let error_value = serde_json::from_str::<serde_json::Value>(&response)
                        .ok()
                        .and_then(|value| value.get("error").cloned());
                    let code = error_value
                        .as_ref()
                        .and_then(|error| error.get("code"))
                        .and_then(|code| code.as_str())
                        .unwrap_or("workspace_create_failed");
                    let message = error_value
                        .as_ref()
                        .and_then(|error| error.get("message"))
                        .and_then(|message| message.as_str())
                        .unwrap_or("workspace create failed");
                    // The peer gets the machine-readable `code` and a fixed
                    // message, never the API message itself: a failed create
                    // is usually a PTY spawn or cwd error whose text names
                    // this host's own filesystem (default shell path, home
                    // directory). The detail stays here, in this host's logs.
                    // Same `code: message` shape `ClosePane` above replies
                    // with, so the client end has one stable classification
                    // format for every federation request failure.
                    tracing::warn!(
                        %code,
                        %message,
                        "refusing a peer's federation workspace-create request"
                    );
                    Err(format!(
                        "{code}: workspace could not be created on the remote host"
                    ))
                });
            let _ = reply.send(outcome);
        }
        FederationCommand::CreateTab {
            epoch,
            connid,
            target_workspace_id,
            label,
            reply,
        } => {
            // Only the mounted controller may create a tab on this host.
            if !lease.is_mounted_controller(epoch, connid) {
                let _ = reply.send(Err("not the mounted controller".to_string()));
                return;
            }
            // Resolve the peer's id by EXACT match, never through
            // `parse_workspace_id`'s positional CLI shorthand: a bare `"1"`
            // or `"w_1"` from the wire would otherwise resolve to whatever
            // workspace occupies that slot right now — including one this
            // host only mirrors, which classifies as local and slips past
            // the redirect guard below. A serving host acts only on ids it
            // actually issued.
            let Some(ws_idx) = app.parse_federation_workspace_id(&target_workspace_id) else {
                tracing::warn!(
                    %target_workspace_id,
                    "refusing a peer's federation tab-create for a workspace this host \
                     never published"
                );
                let _ = reply.send(Err(
                    "workspace_not_found: this host has no workspace with that id".to_string(),
                ));
                return;
            };
            // What this host published for that slot, which is what the
            // classification and the dispatch below must both act on.
            let target_workspace_id = app.public_workspace_id(ws_idx);
            // Refuse a redirect before dispatching the request at all: if
            // `target_workspace_id` names a workspace this host only mirrors
            // from a third host (an `r:` id), `handle_tab_create` would
            // forward the create onto that third host before this arm ever
            // sees the `TabCreateRequested` result below, leaving a ghost tab
            // there that the requesting peer can never reach. Classifying up
            // front stops the forward from ever leaving this host.
            if matches!(
                crate::remote::federation::id::classify(&target_workspace_id),
                crate::remote::federation::id::IdClass::Remote(_)
            ) {
                tracing::warn!(
                    %target_workspace_id,
                    "refusing a peer's federation tab-create whose target workspace is \
                     itself mirrored from a third host, instead of redirecting it there"
                );
                let _ = reply.send(Err(
                    "tab_create_redirected: this host redirected the create onto \
                     a host it mounts, so no tab was created here"
                        .to_string(),
                ));
                return;
            }
            let response = app.handle_api_request_after_internal_events_drained(Request {
                id: "federation-create-tab".to_string(),
                method: Method::TabCreate(crate::api::schema::TabCreateParams {
                    // The exact id this host published for the resolved slot,
                    // never the raw string the peer sent.
                    workspace_id: Some(target_workspace_id),
                    // A mounting client's filesystem path is meaningless
                    // here; let this host's own `tab.create` defaults pick
                    // the new tab's cwd.
                    cwd: None,
                    // Never steal this host's own focus for a remotely
                    // requested tab — the requesting client focuses its own
                    // mirror of it, this host's user did not ask for
                    // anything.
                    focus: false,
                    label,
                    // A peer-supplied launch environment is remote code
                    // execution on this host (`LD_PRELOAD`, `PATH`, ...)
                    // handed to a shell the serving user owns, so it is not
                    // requestable at all — the wire carries no `env` field
                    // to begin with.
                    env: std::collections::HashMap::new(),
                }),
            });
            let outcome = serde_json::from_str::<SuccessResponse>(&response)
                .ok()
                .map(|success| match success.result {
                    // `TabInfo` carries its owning `workspace_id`, so the
                    // workspace the tab landed in comes from this host's own
                    // answer rather than a second lookup.
                    ResponseResult::TabCreated { tab, root_pane } => Ok((
                        tab.workspace_id,
                        tab.tab_id,
                        root_pane.pane_id,
                        root_pane.terminal_id,
                    )),
                    // This host answered its own `tab.create` by forwarding
                    // it onto a host *it* has mounted (the federated
                    // redirect in `handle_tab_create`), so no tab was
                    // created here and none ever will be: what the peer
                    // asked for — a tab on THIS host — did not happen, and
                    // the tab that does appear lives on a third host the
                    // peer never addressed. Reported under its own code so
                    // it is never confused with a real create failure.
                    //
                    // The `classify(&target_workspace_id)` check above this
                    // match block is the primary guard against this case: it
                    // refuses before dispatch, so the forward this arm
                    // describes never actually happens for a peer-originated
                    // `CreateTab`. This arm is now a defensive fallback for
                    // any future path that reaches `handle_tab_create` with a
                    // non-`r:` id that still resolves to a mounted workspace.
                    ResponseResult::TabCreateRequested { origin } => {
                        tracing::warn!(
                            %origin,
                            "a peer's federation tab-create was redirected onto a host \
                             this one mounts; refusing it instead of reporting a tab the \
                             peer cannot reach"
                        );
                        Err(
                            "tab_create_redirected: this host redirected the create onto \
                             a host it mounts, so no tab was created here"
                                .to_string(),
                        )
                    }
                    // No other success shape can carry the ids the peer
                    // needs. Reported distinctly rather than as a create
                    // failure: the create's real outcome is unknown to this
                    // reply path.
                    other => {
                        tracing::warn!(
                            ?other,
                            "unexpected success result for a peer's federation tab-create \
                             request"
                        );
                        Err("tab_create_unexpected_result: this host answered the create \
                             with a result carrying no tab ids"
                            .to_string())
                    }
                })
                .unwrap_or_else(|| {
                    let error_value = serde_json::from_str::<serde_json::Value>(&response)
                        .ok()
                        .and_then(|value| value.get("error").cloned());
                    let code = error_value
                        .as_ref()
                        .and_then(|error| error.get("code"))
                        .and_then(|code| code.as_str())
                        .unwrap_or("tab_create_failed");
                    let message = error_value
                        .as_ref()
                        .and_then(|error| error.get("message"))
                        .and_then(|message| message.as_str())
                        .unwrap_or("tab create failed");
                    // The peer gets the machine-readable `code` and a fixed
                    // message, never the API message itself: a failed create
                    // is usually a PTY spawn or cwd error whose text names
                    // this host's own filesystem (default shell path, home
                    // directory). The detail stays here, in this host's
                    // logs. Same `code: message` shape every other
                    // federation request failure replies with.
                    tracing::warn!(%code, %message, "refusing a peer's federation tab-create request");
                    Err(format!("{code}: tab could not be created on the remote host"))
                });
            let _ = reply.send(outcome);
        }
    }
}

/// Make a terminal's child process repaint its whole screen, by jiggling the
/// PTY window size so the child receives a `SIGWINCH` it has to answer. The
/// resulting bytes are ordinary PTY output, so they reach a mounted client
/// through the existing output stream with no protocol involvement.
///
/// Unix-only, because the jiggle is a `TIOCSWINSZ` pair. A Windows serving
/// host keeps the pre-existing behavior (mounted panes repaint only when the
/// child volunteers it); ConPTY has no equivalent primitive today.
fn nudge_child_redraw(runtime: &crate::terminal::TerminalRuntime) {
    #[cfg(unix)]
    runtime.nudge_child_redraw_after_handoff();
    #[cfg(not(unix))]
    let _ = runtime;
}

/// Produces the atomic (snapshot, cursor) pair `Mount` and `Snapshot` both
/// answer with — extracted so a post-mount resync (`Snapshot`) reuses
/// exactly the same `Method::SessionSnapshot` construction the initial mount
/// does, rather than a second, divergent snapshot-building path. Atomic here
/// because the actor holds `&mut App` exclusively for the call's duration,
/// so no event can slip between the snapshot and the cursor read (same
/// reasoning `Mount`'s original doc comment already gave).
fn current_snapshot(app: &mut App) -> (SessionSnapshot, EventCursor) {
    let response = app.handle_api_request_after_internal_events_drained(Request {
        id: "federation-snapshot".to_string(),
        method: Method::SessionSnapshot(EmptyParams::default()),
    });
    let cursor = EventCursor(app.event_hub.current_sequence());
    let snapshot = serde_json::from_str::<SuccessResponse>(&response)
        .ok()
        .and_then(|success| match success.result {
            ResponseResult::SessionSnapshot { snapshot } => Some(*snapshot),
            _ => None,
        })
        .unwrap_or_else(empty_snapshot);
    (snapshot, cursor)
}

/// The ANSI scrollback for one live terminal, empty if unknown. Unix-only:
/// `handoff_history_ansi` is the same seam `AppFederationHost::scrollback_replay`
/// uses, and is not compiled on non-unix.
#[allow(dead_code)] // dormant until b0.4 wires the federation listener
fn scrollback_replay(app: &App, terminal_id: &str) -> Vec<u8> {
    let Some(_runtime) = app.terminal_runtime_for_terminal_id(terminal_id) else {
        return Vec::new();
    };
    #[cfg(unix)]
    {
        _runtime
            .handoff_history_ansi()
            .map(String::into_bytes)
            .unwrap_or_default()
    }
    #[cfg(not(unix))]
    {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal live `App` with session persistence disabled, mirroring the
    /// `server::headless` test harness. Enough to service federation commands.
    fn test_app() -> App {
        let config = crate::config::Config::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        crate::app::App::new(
            &config,
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        )
    }

    /// Drive a connection through admission + mount, returning its `(epoch,
    /// connid)`. The lease is left `Mounted` for that connection.
    fn acquire_and_mount(
        app: &mut App,
        lease: &mut FederationLease,
        connid: ConnId,
    ) -> AcceptEpoch {
        let epoch = lease.current_epoch();
        let (atx, mut arx) = oneshot::channel();
        dispatch(
            app,
            lease,
            FederationCommand::AcquireController {
                epoch,
                connid,
                reply: atx,
            },
        );
        assert_eq!(
            arx.try_recv().expect("admission reply"),
            Admission::Accepted
        );
        let (mtx, mut mrx) = oneshot::channel();
        dispatch(
            app,
            lease,
            FederationCommand::Mount {
                epoch,
                connid,
                reply: mtx,
            },
        );
        assert!(
            mrx.try_recv().expect("mount reply").is_some(),
            "the holder mounts and receives a snapshot"
        );
        epoch
    }

    #[test]
    fn mount_after_acquire_returns_a_snapshot_and_the_live_cursor() {
        let mut app = test_app();
        let mut lease = FederationLease::new();
        // acquire_and_mount asserts a delivered `Some((snapshot, cursor))`.
        acquire_and_mount(&mut app, &mut lease, 1);
        assert!(lease.is_mounted_controller(0, 1));
    }

    #[test]
    fn acquire_is_busy_for_a_second_connection() {
        let mut app = test_app();
        let mut lease = FederationLease::new();
        let epoch = lease.current_epoch();
        let (t1, mut r1) = oneshot::channel();
        dispatch(
            &mut app,
            &mut lease,
            FederationCommand::AcquireController {
                epoch,
                connid: 1,
                reply: t1,
            },
        );
        assert_eq!(r1.try_recv().unwrap(), Admission::Accepted);
        let (t2, mut r2) = oneshot::channel();
        dispatch(
            &mut app,
            &mut lease,
            FederationCommand::AcquireController {
                epoch,
                connid: 2,
                reply: t2,
            },
        );
        assert_eq!(r2.try_recv().unwrap(), Admission::Busy);
    }

    #[test]
    fn a_stale_mount_replies_none_without_mounting() {
        let mut app = test_app();
        let mut lease = FederationLease::new();
        let epoch = lease.current_epoch();
        let (atx, mut arx) = oneshot::channel();
        dispatch(
            &mut app,
            &mut lease,
            FederationCommand::AcquireController {
                epoch,
                connid: 1,
                reply: atx,
            },
        );
        assert_eq!(arx.try_recv().unwrap(), Admission::Accepted);
        // A handoff revocation supersedes conn 1's epoch before it mounts.
        lease.begin_revocation();
        lease.reopen_admission();
        let (mtx, mut mrx) = oneshot::channel();
        dispatch(
            &mut app,
            &mut lease,
            FederationCommand::Mount {
                epoch,
                connid: 1,
                reply: mtx,
            },
        );
        assert!(mrx.try_recv().unwrap().is_none(), "stale mount is inert");
        assert!(!lease.is_mounted_controller(epoch, 1));
    }

    #[test]
    fn input_and_resize_are_dropped_unless_the_caller_is_the_mounted_controller() {
        let mut app = test_app();
        let mut lease = FederationLease::new();
        let epoch = acquire_and_mount(&mut app, &mut lease, 1);
        // The mounted controller's forwards run (no terminal → silent no-op, but
        // no panic and the authorization gate passes).
        dispatch(
            &mut app,
            &mut lease,
            FederationCommand::SendInput {
                epoch,
                connid: 1,
                terminal_id: "no-such-terminal".to_string(),
                bytes: b"hi".to_vec(),
            },
        );
        // A non-controller connection's forwards are dropped before the PTY.
        dispatch(
            &mut app,
            &mut lease,
            FederationCommand::Resize {
                epoch,
                connid: 999,
                terminal_id: "no-such-terminal".to_string(),
                cols: 80,
                rows: 24,
            },
        );
        // Same gate for the repaint nudge: it drives a real ioctl on this
        // host's PTY, so a non-controller must not reach it either.
        dispatch(
            &mut app,
            &mut lease,
            FederationCommand::NudgeRedraw {
                epoch,
                connid: 999,
                terminal_id: "no-such-terminal".to_string(),
            },
        );
    }

    /// Adds `count` single-pane workspaces to `app` and returns their terminal
    /// ids as strings. Real panes, because size claims resolve their target the
    /// same way a federated resize does — through the workspace layout.
    fn seed_terminals(app: &mut App, count: usize) -> Vec<String> {
        let ids: Vec<_> = (0..count)
            .map(|i| {
                let ws = crate::workspace::Workspace::test_new(&format!("seeded-{i}"));
                let pane_id = ws.tabs[0].root_pane;
                let terminal_id = ws.terminal_id(pane_id).expect("terminal id").clone();
                app.state.workspaces.push(ws);
                terminal_id.to_string()
            })
            .collect();
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        ids
    }

    // While a controller drives a terminal it owns that terminal's size, so the
    // host's own render pass must stand down — otherwise it reverts the mount's
    // geometry within one frame and every repaint lands at the wrong width.
    // Ownership tracks the lease exactly, including across release.
    #[test]
    fn a_mounted_controller_owns_terminal_sizes_until_it_releases() {
        let mut app = test_app();
        let mut lease = FederationLease::new();
        let terminals = seed_terminals(&mut app, 1);
        assert!(
            app.state.federation_owned_terminal_sizes.is_empty(),
            "no mount, no lock"
        );

        let epoch = acquire_and_mount(&mut app, &mut lease, 1);
        claim_terminal_size(&mut app, &terminals[0]);
        assert_eq!(
            app.state.federation_owned_terminal_sizes.len(),
            1,
            "driving a terminal takes ownership of its size"
        );

        dispatch(
            &mut app,
            &mut lease,
            FederationCommand::Release { epoch, connid: 1 },
        );
        assert!(
            app.state.federation_owned_terminal_sizes.is_empty(),
            "releasing hands size ownership back to the host"
        );
    }

    // Revocation happens outside `dispatch` (the handoff path in headless.rs),
    // so ownership must not latch on when the lease is torn down that way.
    #[test]
    fn revoking_a_mounted_lease_hands_terminal_sizes_back() {
        let mut app = test_app();
        let mut lease = FederationLease::new();
        let terminals = seed_terminals(&mut app, 1);
        acquire_and_mount(&mut app, &mut lease, 1);
        claim_terminal_size(&mut app, &terminals[0]);
        assert!(!app.state.federation_owned_terminal_sizes.is_empty());

        lease.begin_revocation();
        sync_terminal_size_ownership(&mut app, &lease);
        assert!(
            app.state.federation_owned_terminal_sizes.is_empty(),
            "a revoked mount owns nothing"
        );
    }

    /// An `App` with one real workspace, terminal and runtime — enough for
    /// `dispatch` to resolve a federated command all the way to the runtime.
    fn test_app_with_live_terminal() -> (App, String) {
        let mut app = test_app();
        let workspace = crate::workspace::Workspace::test_new("test");
        let pane_id = workspace.tabs[0].root_pane;
        let terminal_id = workspace.terminal_id(pane_id).expect("terminal id").clone();
        app.state.workspaces = vec![workspace];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        app.state.selected = 0;
        app.terminal_runtimes.insert(
            terminal_id.clone(),
            crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b""),
        );
        let id_string = terminal_id.to_string();
        (app, id_string)
    }

    // The claim must be taken by the real command path, not just by the helper:
    // a federated resize is what tells this host the mount is driving that
    // terminal, and a close is what hands it back.
    #[test]
    fn a_federated_resize_claims_the_terminal_and_a_close_releases_it() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let _guard = rt.enter();
        let (mut app, terminal_id) = test_app_with_live_terminal();
        let mut lease = FederationLease::new();
        let epoch = acquire_and_mount(&mut app, &mut lease, 1);

        dispatch(
            &mut app,
            &mut lease,
            FederationCommand::Resize {
                epoch,
                connid: 1,
                terminal_id: terminal_id.clone(),
                cols: 100,
                rows: 30,
            },
        );
        assert!(
            app.state
                .federation_owned_terminal_sizes
                .iter()
                .any(|id| id.to_string() == terminal_id),
            "a federated resize claims the terminal's size"
        );

        dispatch(
            &mut app,
            &mut lease,
            FederationCommand::ReleaseTerminalSize {
                epoch,
                connid: 1,
                terminal_id: terminal_id.clone(),
            },
        );
        assert!(
            app.state.federation_owned_terminal_sizes.is_empty(),
            "closing the mirror hands the size back to the host"
        );
    }

    // A mounting client opens terminals lazily, so ownership must stay scoped
    // to the ones it actually drives. Session-wide ownership would freeze the
    // size of every host terminal the mount never renders.
    #[test]
    fn a_mount_owns_only_the_terminals_it_drives() {
        let mut app = test_app();
        let mut lease = FederationLease::new();
        let terminals = seed_terminals(&mut app, 2);
        acquire_and_mount(&mut app, &mut lease, 1);

        claim_terminal_size(&mut app, &terminals[0]);

        let owned = &app.state.federation_owned_terminal_sizes;
        assert!(
            owned.iter().any(|id| id.to_string() == terminals[0]),
            "the driven terminal is owned by the mount"
        );
        assert!(
            !owned.iter().any(|id| id.to_string() == terminals[1]),
            "a terminal the mount never opened stays host owned"
        );
    }

    #[test]
    fn release_frees_the_lease_for_the_next_connection() {
        let mut app = test_app();
        let mut lease = FederationLease::new();
        let epoch = acquire_and_mount(&mut app, &mut lease, 1);
        dispatch(
            &mut app,
            &mut lease,
            FederationCommand::Release { epoch, connid: 1 },
        );
        // The slot is free again; a fresh connection can acquire.
        let (atx, mut arx) = oneshot::channel();
        dispatch(
            &mut app,
            &mut lease,
            FederationCommand::AcquireController {
                epoch,
                connid: 2,
                reply: atx,
            },
        );
        assert_eq!(arx.try_recv().unwrap(), Admission::Accepted);
    }

    #[test]
    fn subscribe_output_for_an_unknown_terminal_is_none() {
        let mut app = test_app();
        let mut lease = FederationLease::new();
        let (tx, mut rx) = oneshot::channel();
        dispatch(
            &mut app,
            &mut lease,
            FederationCommand::SubscribeOutput("no-such-terminal".to_string(), tx),
        );
        assert!(rx.try_recv().expect("reply delivered").is_none());
    }

    #[test]
    fn events_after_on_a_fresh_app_is_empty() {
        let mut app = test_app();
        let mut lease = FederationLease::new();
        let (tx, mut rx) = oneshot::channel();
        dispatch(&mut app, &mut lease, FederationCommand::EventsAfter(0, tx));
        assert!(rx.try_recv().expect("reply delivered").is_empty());
    }

    // Post-mount pane mirroring fix (plans/260722-1327): `Snapshot` answers
    // with the same (snapshot, cursor) shape `Mount` does, without touching
    // the lease — proven here by calling it BEFORE any `AcquireController`/
    // `Mount`, which `Mount` itself could never do.
    #[test]
    fn snapshot_produces_a_fresh_snapshot_and_cursor_without_touching_the_lease() {
        let mut app = test_app();
        let mut lease = FederationLease::new();
        let (tx, mut rx) = oneshot::channel();
        dispatch(&mut app, &mut lease, FederationCommand::Snapshot(tx));
        let (_snapshot, cursor) = rx.try_recv().expect("reply delivered");
        assert_eq!(cursor.0, app.event_hub.current_sequence());
        assert!(
            !lease.is_mounted_controller(0, 1),
            "Snapshot must never acquire or mount the single-controller lease"
        );
    }

    /// `SplitPane` performs a real split against the live `App` (via the
    /// same `Method::PaneSplit` handler the local TUI/CLI path uses) and
    /// replies with the new pane's raw id + terminal id.
    #[tokio::test]
    async fn split_pane_against_a_known_target_pane_creates_a_real_pane_and_replies_ok() {
        let mut app = test_app();
        // Seed one workspace/pane the same way `app/api/panes.rs`'s own
        // `app_with_test_workspace` helper does.
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("metadata")];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        let mut lease = FederationLease::new();
        let response = app.handle_api_request_after_internal_events_drained(Request {
            id: "seed".to_string(),
            method: Method::PaneCurrent(crate::api::schema::PaneCurrentParams {
                caller_pane_id: None,
            }),
        });
        let target_pane_id = serde_json::from_str::<SuccessResponse>(&response)
            .ok()
            .and_then(|success| match success.result {
                ResponseResult::PaneCurrent { pane } => Some(pane.pane_id),
                _ => None,
            })
            .expect("a seeded App has one focused pane");

        let (tx, mut rx) = oneshot::channel();
        dispatch(
            &mut app,
            &mut lease,
            FederationCommand::SplitPane {
                target_pane_id,
                direction: crate::remote::federation::protocol::SplitDirection::Right,
                ratio: None,
                focus: false,
                reply: tx,
            },
        );
        let outcome = rx.try_recv().expect("reply delivered");
        let (new_pane_id, new_terminal_id) = outcome.expect("split against a known pane succeeds");
        assert!(!new_pane_id.is_empty());
        assert!(!new_terminal_id.is_empty());
    }

    /// `CreateWorkspace` performs a real create against the live `App` (via
    /// the same `Method::WorkspaceCreate` handler the local TUI/CLI
    /// new-workspace action uses) and replies with the new workspace's raw
    /// workspace/tab/pane/terminal ids.
    #[tokio::test]
    async fn create_workspace_creates_a_real_workspace_and_replies_with_its_ids() {
        let mut app = test_app();
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("metadata")];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        let mut lease = FederationLease::new();
        let epoch = acquire_and_mount(&mut app, &mut lease, 1);
        let before = app.state.workspaces.len();

        let (tx, mut rx) = oneshot::channel();
        dispatch(
            &mut app,
            &mut lease,
            FederationCommand::CreateWorkspace {
                epoch,
                connid: 1,
                label: Some("from-remote".to_string()),
                reply: tx,
            },
        );
        let outcome = rx.try_recv().expect("reply delivered");
        let (workspace_id, tab_id, pane_id, terminal_id) =
            outcome.expect("workspace create on a healthy App succeeds");
        assert!(!workspace_id.is_empty());
        assert!(!tab_id.is_empty());
        assert!(!pane_id.is_empty());
        assert!(!terminal_id.is_empty());
        assert_eq!(
            app.state.workspaces.len(),
            before + 1,
            "the serving host really gained a workspace"
        );
        assert_eq!(
            app.state.workspaces[before].display_name(),
            "from-remote",
            "the client's label hint reached the serving host's workspace"
        );
        assert_eq!(
            app.state.active,
            Some(0),
            "a remotely requested workspace must not steal the serving host's own focus"
        );
    }

    /// Root-cause regression for the client/server id-space mismatch: the
    /// federation client only ever knows a pane's raw `terminal_id`
    /// (`app/api/panes.rs::dispatch_remote_pane_split` sends
    /// `runtime.remote_terminal_id()`, never a public `w…:p…` pane id), so
    /// `SplitPane`'s `target_pane_id` here is always a raw terminal id in
    /// production. Before the fix, `Method::PaneSplit`'s handler only
    /// accepted public pane ids and this would reply `pane_not_found` for
    /// every real remote split.
    #[tokio::test]
    async fn split_pane_resolves_a_raw_terminal_id_the_same_as_a_public_pane_id() {
        let mut app = test_app();
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("metadata")];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        let mut lease = FederationLease::new();

        let root_pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let raw_terminal_id = app.state.workspaces[0]
            .terminal_id(root_pane_id)
            .expect("the seeded root pane has an attached terminal id")
            .to_string();

        let (tx, mut rx) = oneshot::channel();
        dispatch(
            &mut app,
            &mut lease,
            FederationCommand::SplitPane {
                target_pane_id: raw_terminal_id,
                direction: crate::remote::federation::protocol::SplitDirection::Right,
                ratio: None,
                focus: false,
                reply: tx,
            },
        );
        let outcome = rx.try_recv().expect("reply delivered");
        let (new_pane_id, new_terminal_id) =
            outcome.expect("split resolved via the raw terminal id must succeed");
        assert!(!new_pane_id.is_empty());
        assert!(!new_terminal_id.is_empty());
    }

    #[test]
    fn split_pane_against_an_unknown_target_pane_replies_with_a_reason() {
        let mut app = test_app();
        let mut lease = FederationLease::new();
        let (tx, mut rx) = oneshot::channel();
        dispatch(
            &mut app,
            &mut lease,
            FederationCommand::SplitPane {
                target_pane_id: "no-such-pane".to_string(),
                direction: crate::remote::federation::protocol::SplitDirection::Right,
                ratio: None,
                focus: false,
                reply: tx,
            },
        );
        let outcome = rx.try_recv().expect("reply delivered");
        assert!(
            outcome.is_err(),
            "an unknown target pane must fail, not misfile"
        );
    }

    /// `ClosePane` performs a real close against the live `App` (via the
    /// same `Method::PaneClose` handler the local TUI/CLI path uses) and
    /// replies `Ok(())` — Gap A server-side wiring
    /// (plans/260724-1536-federation-pane-close-sync).
    #[tokio::test]
    async fn close_pane_against_a_known_target_pane_closes_it_and_replies_ok() {
        let mut app = test_app();
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("metadata")];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        let mut lease = FederationLease::new();
        let response = app.handle_api_request_after_internal_events_drained(Request {
            id: "seed".to_string(),
            method: Method::PaneCurrent(crate::api::schema::PaneCurrentParams {
                caller_pane_id: None,
            }),
        });
        let target_pane_id = serde_json::from_str::<SuccessResponse>(&response)
            .ok()
            .and_then(|success| match success.result {
                ResponseResult::PaneCurrent { pane } => Some(pane.pane_id),
                _ => None,
            })
            .expect("a seeded App has one focused pane");

        let (tx, mut rx) = oneshot::channel();
        dispatch(
            &mut app,
            &mut lease,
            FederationCommand::ClosePane {
                target_pane_id,
                reply: tx,
            },
        );
        let outcome = rx.try_recv().expect("reply delivered");
        assert!(outcome.is_ok(), "closing a known pane must succeed");
        assert!(
            app.state.workspaces.is_empty(),
            "the seeded workspace's only pane closing must close the workspace too"
        );
    }

    #[test]
    fn close_pane_against_an_unknown_target_pane_replies_with_a_reason() {
        let mut app = test_app();
        let mut lease = FederationLease::new();
        let (tx, mut rx) = oneshot::channel();
        dispatch(
            &mut app,
            &mut lease,
            FederationCommand::ClosePane {
                target_pane_id: "no-such-pane".to_string(),
                reply: tx,
            },
        );
        let outcome = rx.try_recv().expect("reply delivered");
        assert!(
            outcome.is_err(),
            "an unknown target pane must fail, not misfile"
        );
    }

    /// The raw (canonical) tab id of a seeded workspace's first tab, fetched
    /// through `Method::TabList` — `ids.rs`'s `public_tab_id` is
    /// `pub(super)` (visible only inside the `app` module), so this test
    /// module, which lives in `server`, goes through the JSON API instead,
    /// the same way `close_pane_against_a_known_target_pane_...` above
    /// fetches its target pane id through `Method::PaneCurrent`.
    fn tab_id_for_workspace(app: &mut App, workspace_id: &str) -> String {
        let response = app.handle_api_request_after_internal_events_drained(Request {
            id: "seed-tab-lookup".to_string(),
            method: Method::TabList(crate::api::schema::TabListParams {
                workspace_id: Some(workspace_id.to_string()),
            }),
        });
        serde_json::from_str::<SuccessResponse>(&response)
            .ok()
            .and_then(|success| match success.result {
                ResponseResult::TabList { tabs } => tabs.into_iter().next(),
                _ => None,
            })
            .expect("the seeded workspace has one tab")
            .tab_id
    }

    /// A `WorktreeSpaceMembership` shared by two seeded workspaces, so
    /// `AppState::close_indices_for` would otherwise group-close both of
    /// them for a single close request — exactly the destructive branch
    /// `close_federation_target_workspace`/`close_federation_target_tab`
    /// must never take.
    fn shared_worktree_space(key: &str) -> crate::workspace::WorktreeSpaceMembership {
        crate::workspace::WorktreeSpaceMembership {
            key: key.to_string(),
            label: "space".to_string(),
            repo_root: std::path::PathBuf::from("/repo"),
            checkout_path: std::path::PathBuf::from("/repo"),
            is_linked_worktree: false,
        }
    }

    /// `CloseWorkspaceRemote` against one of two workspaces sharing a
    /// worktree-group key must close only the targeted workspace — the
    /// sibling must survive. This is the regression `handle_workspace_close`
    /// (the JSON-API `workspace.close` path) would NOT catch on this host,
    /// because a federation-originated target is always a LOCAL workspace,
    /// so its `federation_host_key_for_workspace` lookup returns `None` and
    /// it falls into `AppState::close_selected_workspace()`'s group close.
    #[tokio::test]
    async fn close_workspace_remote_closes_exactly_one_workspace_sibling_survives() {
        let mut app = test_app();
        app.state.workspaces = vec![
            crate::workspace::Workspace::test_new("target"),
            crate::workspace::Workspace::test_new("sibling"),
        ];
        let space = shared_worktree_space("shared-space");
        app.state.workspaces[0].worktree_space = Some(space.clone());
        app.state.workspaces[1].worktree_space = Some(space);
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        let target_id = app.state.workspaces[0].id.clone();
        let sibling_id = app.state.workspaces[1].id.clone();

        // Captured rather than hardcoded: this asserts the command left the
        // host's UI mode exactly as it found it, whatever the fixture starts in.
        let mode_before = app.state.mode;
        let mut lease = FederationLease::new();
        let epoch = acquire_and_mount(&mut app, &mut lease, 1);

        let (tx, mut rx) = oneshot::channel();
        dispatch(
            &mut app,
            &mut lease,
            FederationCommand::CloseWorkspaceRemote {
                epoch,
                connid: 1,
                target_workspace_id: target_id,
                reply: tx,
            },
        );
        let outcome = rx.try_recv().expect("reply delivered");
        assert!(
            outcome.is_ok(),
            "closing a known local workspace must succeed"
        );
        assert_eq!(
            app.state.workspaces.len(),
            1,
            "exactly one workspace closes, never the whole worktree group"
        );
        assert_eq!(
            app.state.workspaces[0].id, sibling_id,
            "the sibling sharing the worktree group survives a single remote close"
        );
        // Surviving is not enough: the target is detached from the group
        // before it closes, so a bug that detached the wrong workspace would
        // still leave two ids present. The surviving sibling must also still
        // BE in the worktree group it started in.
        assert_eq!(
            app.state.workspaces[0].worktree_space.as_ref(),
            Some(&shared_worktree_space("shared-space")),
            "the survivor must keep its worktree-group membership, not just exist"
        );
        assert_eq!(
            app.state.mode, mode_before,
            "a remote close must never pop this host's own confirmation UI"
        );
    }

    /// `CloseTabRemote` against the LAST tab of a workspace that shares a
    /// worktree-group key with a sibling must close only that one workspace
    /// (via the tab-closes-workspace branch) — the sibling must survive, and
    /// this host's own confirmation UI must never be triggered. This is the
    /// regression `handle_tab_close` (the JSON-API `tab.close` path) would
    /// NOT catch: its `AppState::confirm_implicit_worktree_group_close`
    /// mutates `mode`/`selected` before refusing, exactly the mutation a
    /// federation-originated request must never cause.
    #[tokio::test]
    async fn close_tab_remote_closes_exactly_one_workspace_sibling_survives() {
        let mut app = test_app();
        app.state.workspaces = vec![
            crate::workspace::Workspace::test_new("target"),
            crate::workspace::Workspace::test_new("sibling"),
        ];
        let space = shared_worktree_space("shared-space");
        app.state.workspaces[0].worktree_space = Some(space.clone());
        app.state.workspaces[1].worktree_space = Some(space);
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        // `confirm_close` defaults true; leaving it on proves this path never
        // reaches `confirm_implicit_worktree_group_close`.
        assert!(app.state.confirm_close);
        let target_workspace_id = app.state.workspaces[0].id.clone();
        let sibling_id = app.state.workspaces[1].id.clone();
        let target_tab_id = tab_id_for_workspace(&mut app, &target_workspace_id);

        // Captured rather than hardcoded: this asserts the command left the
        // host's UI mode exactly as it found it, whatever the fixture starts in.
        let mode_before = app.state.mode;
        let mut lease = FederationLease::new();
        let epoch = acquire_and_mount(&mut app, &mut lease, 1);

        let (tx, mut rx) = oneshot::channel();
        dispatch(
            &mut app,
            &mut lease,
            FederationCommand::CloseTabRemote {
                epoch,
                connid: 1,
                target_tab_id,
                reply: tx,
            },
        );
        let outcome = rx.try_recv().expect("reply delivered");
        assert!(
            outcome.is_ok(),
            "closing a known local tab must succeed: {outcome:?}"
        );
        assert_eq!(
            app.state.workspaces.len(),
            1,
            "closing the last tab closes exactly one workspace, never the group"
        );
        assert_eq!(
            app.state.workspaces[0].id, sibling_id,
            "the sibling sharing the worktree group survives a single remote close"
        );
        // Surviving is not enough: the target is detached from the group
        // before it closes, so a bug that detached the wrong workspace would
        // still leave two ids present. The surviving sibling must also still
        // BE in the worktree group it started in.
        assert_eq!(
            app.state.workspaces[0].worktree_space.as_ref(),
            Some(&shared_worktree_space("shared-space")),
            "the survivor must keep its worktree-group membership, not just exist"
        );
        assert_eq!(
            app.state.mode, mode_before,
            "a remote close must never pop this host's own confirmation UI, even though \
             closing this tab would trigger `confirm_implicit_worktree_group_close` on the \
             local `tab.close` JSON-API path"
        );
    }

    /// Both new close commands are refused for a stale-epoch (non-mounted)
    /// caller, mirroring `input_and_resize_are_dropped_unless_the_caller_is_
    /// the_mounted_controller` — and refusal must not mutate the App at all.
    #[tokio::test]
    async fn close_workspace_and_tab_remote_are_refused_for_a_non_controller_connid() {
        let mut app = test_app();
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("only")];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        let workspace_id = app.state.workspaces[0].id.clone();
        let tab_id = tab_id_for_workspace(&mut app, &workspace_id);

        // Captured rather than hardcoded: this asserts the command left the
        // host's UI mode exactly as it found it, whatever the fixture starts in.
        let mode_before = app.state.mode;
        let mut lease = FederationLease::new();
        let epoch = acquire_and_mount(&mut app, &mut lease, 1);

        let (tx, mut rx) = oneshot::channel();
        dispatch(
            &mut app,
            &mut lease,
            FederationCommand::CloseWorkspaceRemote {
                epoch,
                connid: 999,
                target_workspace_id: workspace_id,
                reply: tx,
            },
        );
        let outcome = rx.try_recv().expect("reply delivered");
        assert!(
            outcome.is_err(),
            "a non-controller connid must be refused, not serviced"
        );
        assert_eq!(
            app.state.workspaces.len(),
            1,
            "a refused close must not touch the workspace set"
        );

        let (tx, mut rx) = oneshot::channel();
        dispatch(
            &mut app,
            &mut lease,
            FederationCommand::CloseTabRemote {
                epoch,
                connid: 999,
                target_tab_id: tab_id,
                reply: tx,
            },
        );
        let outcome = rx.try_recv().expect("reply delivered");
        assert!(
            outcome.is_err(),
            "a non-controller connid must be refused, not serviced"
        );
        assert_eq!(
            app.state.workspaces.len(),
            1,
            "a refused close must not touch the workspace set"
        );
        assert_eq!(
            app.state.mode, mode_before,
            "a refusal must never mutate this host's UI mode"
        );
    }

    /// Creating a workspace on this host is a controller-only action: a
    /// connection that is not the mounted controller must be refused, and the
    /// refusal must not grow the host's workspace set.
    #[tokio::test]
    async fn create_workspace_is_refused_for_a_non_controller_connid() {
        let mut app = test_app();
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("only")];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);

        // Captured rather than hardcoded: this asserts the command left the
        // host's UI mode exactly as it found it, whatever the fixture starts in.
        let mode_before = app.state.mode;
        let mut lease = FederationLease::new();
        let epoch = acquire_and_mount(&mut app, &mut lease, 1);
        let before = app.state.workspaces.len();

        let (tx, mut rx) = oneshot::channel();
        dispatch(
            &mut app,
            &mut lease,
            FederationCommand::CreateWorkspace {
                epoch,
                connid: 999,
                label: Some("from-an-impostor".to_string()),
                reply: tx,
            },
        );
        let outcome = rx.try_recv().expect("reply delivered");
        assert!(
            outcome.is_err(),
            "a non-controller connid must be refused, not serviced"
        );
        assert_eq!(
            app.state.workspaces.len(),
            before,
            "a refused create must not touch the workspace set"
        );
        assert_eq!(
            app.state.mode, mode_before,
            "a refusal must never mutate this host's UI mode"
        );
    }

    /// A target that does not exist on this host reports `Failed`, never a
    /// silent drop, and never mutates the host's `mode`.
    #[tokio::test]
    async fn close_workspace_and_tab_remote_against_unknown_targets_report_failed_without_mutation()
    {
        let mut app = test_app();
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("only")];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);

        // Captured rather than hardcoded: this asserts the command left the
        // host's UI mode exactly as it found it, whatever the fixture starts in.
        let mode_before = app.state.mode;
        let mut lease = FederationLease::new();
        let epoch = acquire_and_mount(&mut app, &mut lease, 1);

        let (tx, mut rx) = oneshot::channel();
        dispatch(
            &mut app,
            &mut lease,
            FederationCommand::CloseWorkspaceRemote {
                epoch,
                connid: 1,
                target_workspace_id: "no-such-workspace".to_string(),
                reply: tx,
            },
        );
        let outcome = rx.try_recv().expect("reply delivered");
        assert!(
            outcome.is_err(),
            "an unknown workspace must fail, not misfile"
        );

        let (tx, mut rx) = oneshot::channel();
        dispatch(
            &mut app,
            &mut lease,
            FederationCommand::CloseTabRemote {
                epoch,
                connid: 1,
                target_tab_id: "no-such-tab".to_string(),
                reply: tx,
            },
        );
        let outcome = rx.try_recv().expect("reply delivered");
        assert!(outcome.is_err(), "an unknown tab must fail, not misfile");

        assert_eq!(
            app.state.workspaces.len(),
            1,
            "a failed close must not touch the workspace set"
        );
        assert_eq!(
            app.state.mode, mode_before,
            "a failed close must never mutate this host's UI mode"
        );
    }

    /// A positional id (`"1"`, `"w_1"`, `"t_1_1"`) is CLI shorthand for a
    /// human at a terminal, never something this host handed to a peer. It
    /// must be refused over the wire rather than silently resolving to
    /// whatever workspace currently occupies that slot: the peer would be
    /// closing an object it was never told about, and which one it hits would
    /// depend on the host's current ordering.
    #[tokio::test]
    async fn close_workspace_and_tab_remote_refuse_positional_index_shorthand() {
        let mut app = test_app();
        app.state.workspaces = vec![
            crate::workspace::Workspace::test_new("first"),
            crate::workspace::Workspace::test_new("second"),
        ];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        let workspaces_before = app.state.workspaces.len();

        let mut lease = FederationLease::new();
        let epoch = acquire_and_mount(&mut app, &mut lease, 1);

        for shorthand in ["1", "w_1"] {
            let (tx, mut rx) = oneshot::channel();
            dispatch(
                &mut app,
                &mut lease,
                FederationCommand::CloseWorkspaceRemote {
                    epoch,
                    connid: 1,
                    target_workspace_id: shorthand.to_string(),
                    reply: tx,
                },
            );
            let outcome = rx.try_recv().expect("reply delivered");
            assert!(
                outcome.is_err(),
                "positional workspace shorthand {shorthand:?} must be refused over the wire"
            );
        }

        for shorthand in ["t_1_1", "1:1"] {
            let (tx, mut rx) = oneshot::channel();
            dispatch(
                &mut app,
                &mut lease,
                FederationCommand::CloseTabRemote {
                    epoch,
                    connid: 1,
                    target_tab_id: shorthand.to_string(),
                    reply: tx,
                },
            );
            let outcome = rx.try_recv().expect("reply delivered");
            assert!(
                outcome.is_err(),
                "positional tab shorthand {shorthand:?} must be refused over the wire"
            );
        }

        assert_eq!(
            app.state.workspaces.len(),
            workspaces_before,
            "a refused positional id must never close anything"
        );
    }

    /// Creating a tab on this host is a controller-only action, exactly like
    /// creating a workspace: a connection that is not the mounted controller
    /// must be refused, and the refusal must not grow the target workspace's
    /// tab set.
    #[tokio::test]
    async fn a_peers_tab_create_is_refused_when_it_is_not_the_mounted_controller() {
        let mut app = test_app();
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("only")];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        let target_workspace_id = app.state.workspaces[0].id.clone();

        // Captured rather than hardcoded: a refusal must leave this host's UI
        // exactly as it found it, whatever the fixture starts in.
        let mode_before = app.state.mode;
        let mut lease = FederationLease::new();
        let epoch = acquire_and_mount(&mut app, &mut lease, 1);
        let tabs_before = app.state.workspaces[0].tabs.len();

        let (tx, mut rx) = oneshot::channel();
        dispatch(
            &mut app,
            &mut lease,
            FederationCommand::CreateTab {
                epoch,
                connid: 999,
                target_workspace_id,
                label: Some("from-an-impostor".to_string()),
                reply: tx,
            },
        );
        let outcome = rx.try_recv().expect("reply delivered");
        assert_eq!(
            outcome,
            Err("not the mounted controller".to_string()),
            "a non-controller connid must be refused, not serviced"
        );
        assert_eq!(
            app.state.workspaces[0].tabs.len(),
            tabs_before,
            "a refused create must not touch the workspace's tab set"
        );
        assert_eq!(
            app.state.mode, mode_before,
            "a refusal must never mutate this host's UI mode"
        );
    }

    /// `CreateTab` performs a real create against the live `App` (via the same
    /// `Method::TabCreate` handler the local TUI/CLI new-tab action uses) and
    /// replies with the new tab's raw workspace/tab/pane/terminal ids, without
    /// moving the serving user's own focus.
    #[tokio::test]
    async fn a_peers_tab_create_creates_a_tab_in_the_named_workspace_without_stealing_focus() {
        let mut app = test_app();
        app.state.workspaces = vec![
            crate::workspace::Workspace::test_new("first"),
            crate::workspace::Workspace::test_new("second"),
        ];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        // Deliberately NOT the host's active workspace: the peer names its
        // target, and the target is what must grow.
        let target_workspace_id = app.state.workspaces[1].id.clone();

        let mut lease = FederationLease::new();
        let epoch = acquire_and_mount(&mut app, &mut lease, 1);
        let tabs_before = app.state.workspaces[1].tabs.len();
        let active_tab_before = app.state.workspaces[1].active_tab;

        let (tx, mut rx) = oneshot::channel();
        dispatch(
            &mut app,
            &mut lease,
            FederationCommand::CreateTab {
                epoch,
                connid: 1,
                target_workspace_id: target_workspace_id.clone(),
                label: Some("from-remote".to_string()),
                reply: tx,
            },
        );
        let outcome = rx.try_recv().expect("reply delivered");
        let (workspace_id, tab_id, pane_id, terminal_id) =
            outcome.expect("tab create on a healthy App succeeds");
        assert_eq!(
            workspace_id, target_workspace_id,
            "the reply must name the workspace the peer asked for, read off this \
             host's own answer"
        );
        assert!(!tab_id.is_empty());
        assert!(!pane_id.is_empty());
        assert!(!terminal_id.is_empty());
        assert_eq!(
            app.state.workspaces[1].tabs.len(),
            tabs_before + 1,
            "the serving host really gained a tab in the named workspace"
        );
        assert_eq!(
            app.state.workspaces[0].tabs.len(),
            1,
            "an untargeted workspace must be left alone"
        );
        assert_eq!(
            app.state.workspaces[1].tabs[tabs_before]
                .custom_name
                .as_deref(),
            Some("from-remote"),
            "the peer's label hint reached the serving host's tab"
        );
        // Open question 1 in the plan: `focus: false` must be honoured all the
        // way down. If this fails, do not weaken it — the pin is insufficient
        // and that is a separate local-behavior fix.
        assert_eq!(
            app.state.workspaces[1].active_tab, active_tab_before,
            "a remotely requested tab must not move the serving host's own active tab"
        );
        assert_eq!(
            app.state.active,
            Some(0),
            "a remotely requested tab must not steal the serving host's own focus"
        );
    }

    /// A target id that resolves to nothing on this host is answered with a
    /// reason, never a panic and never a fabricated success.
    #[tokio::test]
    async fn a_peers_tab_create_for_an_unknown_workspace_is_refused() {
        let mut app = test_app();
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("only")];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        let mode_before = app.state.mode;
        let mut lease = FederationLease::new();
        let epoch = acquire_and_mount(&mut app, &mut lease, 1);
        let tabs_before = app.state.workspaces[0].tabs.len();

        let (tx, mut rx) = oneshot::channel();
        dispatch(
            &mut app,
            &mut lease,
            FederationCommand::CreateTab {
                epoch,
                connid: 1,
                target_workspace_id: "no-such-workspace".to_string(),
                label: None,
                reply: tx,
            },
        );
        let outcome = rx.try_recv().expect("reply delivered");
        let reason = outcome.expect_err("an unknown target must be refused");
        assert!(
            reason.starts_with("workspace_not_found"),
            "the peer must get this host's machine-readable code: {reason}"
        );
        assert_eq!(
            app.state.workspaces[0].tabs.len(),
            tabs_before,
            "a refused create must not touch any workspace's tab set"
        );
        assert_eq!(
            app.state.mode, mode_before,
            "a refusal must never mutate this host's UI mode"
        );
    }

    /// Chained-mount refusal: the peer named a workspace that is itself a
    /// federation mirror on THIS host, so this host's own `tab.create`
    /// forwards the create onto a third host and answers
    /// `TabCreateRequested`. No tab was created here, and the tab that will
    /// eventually appear lives somewhere the peer never addressed, so the
    /// reply must be a refusal under its own code — never a fabricated
    /// success.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_peers_tab_create_redirected_onto_a_third_host_is_refused() {
        let (mut app, mut out_rx, mirrored_workspace_id) = app_with_a_mirrored_workspace();
        // Drain whatever the mount's own materialization already queued
        // (terminal subscribe/resize messages) so the assertion below only
        // sees traffic caused by the `CreateTab` dispatch itself.
        while out_rx.try_recv().is_ok() {}
        let mut lease = FederationLease::new();
        let epoch = acquire_and_mount(&mut app, &mut lease, 1);
        let tabs_before = app.state.workspaces[0].tabs.len();

        let (tx, mut rx) = oneshot::channel();
        dispatch(
            &mut app,
            &mut lease,
            FederationCommand::CreateTab {
                epoch,
                connid: 1,
                target_workspace_id: mirrored_workspace_id,
                label: None,
                reply: tx,
            },
        );
        let outcome = rx.try_recv().expect("reply delivered");
        let reason = outcome.expect_err("a redirected create must be refused");
        assert!(
            reason.starts_with("tab_create_redirected"),
            "a redirect must be reported under its own code, never as a generic \
             create failure: {reason}"
        );
        assert_eq!(
            app.state.workspaces[0].tabs.len(),
            tabs_before,
            "a redirected create must not have created a local tab here"
        );
        assert!(
            out_rx.try_recv().is_err(),
            "the pre-dispatch classify guard must refuse before this host ever \
             forwards a TabCreateRequest over the third host's mount link"
        );
    }

    /// The positional CLI shorthand (`"1"`, `"w_1"`) must never resolve a
    /// wire id: it classifies as local, so it slips past the chained-mount
    /// guard, and then `handle_tab_create` resolves it to whatever workspace
    /// occupies that slot — here a mirror of a third host, which would send a
    /// `TabCreateRequest` off this machine and leave a ghost tab the peer can
    /// never reach.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_peers_tab_create_by_positional_shorthand_is_refused() {
        let (mut app, mut out_rx, _mirrored_workspace_id) = app_with_a_mirrored_workspace();
        while out_rx.try_recv().is_ok() {}
        let mut lease = FederationLease::new();
        let epoch = acquire_and_mount(&mut app, &mut lease, 1);
        let tabs_before = app.state.workspaces[0].tabs.len();

        let (tx, mut rx) = oneshot::channel();
        dispatch(
            &mut app,
            &mut lease,
            FederationCommand::CreateTab {
                epoch,
                connid: 1,
                target_workspace_id: "1".to_string(),
                label: None,
                reply: tx,
            },
        );

        let outcome = rx.try_recv().expect("reply delivered");
        let reason = outcome.expect_err("a positional id is not an id this host issued");
        assert!(
            reason.starts_with("workspace_not_found"),
            "a wire id must resolve exactly or not at all: {reason}"
        );
        assert_eq!(
            app.state.workspaces[0].tabs.len(),
            tabs_before,
            "a refused create must not touch any workspace's tab set"
        );
        assert!(
            out_rx.try_recv().is_err(),
            "nothing may leave this host over a third host's mount link"
        );
    }

    /// One live-mounted remote workspace on this host, so a `tab.create`
    /// against it takes the federated redirect path. Written fresh rather
    /// than shared: the `app/api/tabs.rs` twin is private to that module's
    /// test scope. The out channel's receiver is returned so the request the
    /// redirect emits keeps a live link (dropping it would close the mount's
    /// outbound side mid-test).
    #[cfg(unix)]
    fn app_with_a_mirrored_workspace() -> (
        App,
        tokio::sync::mpsc::UnboundedReceiver<
            crate::remote::federation::protocol::FederationMessage,
        >,
        String,
    ) {
        use crate::api::schema::{PaneInfo, TabInfo, WorkspaceInfo};
        use crate::remote::federation::id::{HostKey, Mount, ServerInstanceId};

        let mut app = test_app();
        let mount = Mount {
            host_key: HostKey::new("alice@10.0.0.1", "s1"),
            server_instance_id: ServerInstanceId("inst-a".to_string()),
            mount_generation: 1,
        };
        let mut mirror = crate::remote::federation::reducer::RemoteMirror::new(mount);
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
                name_source: crate::workspace::naming::NameSource::Mirrored,
                focused: false,
                pane_count: 1,
                tab_count: 1,
                active_tab_id: "w1-tab".to_string(),
                agent_status: AgentStatus::Idle,
                tokens: Default::default(),
                worktree: None,
                federation_origin: None,
            }],
            tabs: vec![TabInfo {
                tab_id: "w1-tab".to_string(),
                workspace_id: "w1".to_string(),
                number: 1,
                label: "first remote tab".to_string(),
                name_source: crate::workspace::naming::NameSource::Mirrored,
                focused: false,
                pane_count: 1,
                agent_status: AgentStatus::Idle,
            }],
            panes: vec![PaneInfo {
                pane_id: "p1".to_string(),
                terminal_id: "t1".to_string(),
                workspace_id: "w1".to_string(),
                tab_id: "w1-tab".to_string(),
                focused: false,
                cwd: Some("/home/alice/project".to_string()),
                foreground_cwd: None,
                label: Some("remote pane 1".to_string()),
                name_source: crate::workspace::naming::NameSource::default(),
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

        let mut router = crate::remote::federation::client::TerminalChannelRouter::new();
        let (out_tx, out_rx) = tokio::sync::mpsc::unbounded_channel();
        let (clipboard_tx, _clipboard_rx) = tokio::sync::mpsc::unbounded_channel();
        let created = app
            .materialize_federation_mount(&mirror, &mut router, &out_tx, &clipboard_tx)
            .expect("materialization must succeed against a loopback-shaped snapshot");
        app.state.active = Some(created[0]);
        app.state
            .begin_federation_mount(mirror)
            .expect("registering the mirror must succeed for a fresh HostKey");
        let workspace_id = app.state.workspaces[created[0]].id.clone();
        (app, out_rx, workspace_id)
    }
}
