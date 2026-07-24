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
        crate::app::App::new(&config, true, None, api_rx, crate::api::EventHub::default())
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
}
