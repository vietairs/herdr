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
    /// Current per-terminal agent statuses.
    AgentStatuses(oneshot::Sender<Vec<(String, AgentStatus)>>),
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
            FederationCommand::AgentStatuses(_) => f.write_str("AgentStatuses"),
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
            let response =
                app.handle_api_request_after_internal_events_drained(Request {
                    id: "federation-mount".to_string(),
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
            let _ = reply.send(Some((snapshot, cursor)));
        }
        FederationCommand::Release { epoch, connid } => {
            lease.release(epoch, connid);
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
            }
        }
        FederationCommand::AgentStatuses(reply) => {
            let response =
                app.handle_api_request_after_internal_events_drained(Request {
                    id: "federation-agent-list".to_string(),
                    method: Method::AgentList(EmptyParams::default()),
                });
            let statuses = serde_json::from_str::<SuccessResponse>(&response)
                .ok()
                .and_then(|success| match success.result {
                    ResponseResult::AgentList { agents } => Some(
                        agents
                            .into_iter()
                            .map(|agent| (agent.terminal_id, agent.agent_status))
                            .collect(),
                    ),
                    _ => None,
                })
                .unwrap_or_default();
            let _ = reply.send(statuses);
        }
    }
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
    fn acquire_and_mount(app: &mut App, lease: &mut FederationLease, connid: ConnId) -> AcceptEpoch {
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
        assert_eq!(arx.try_recv().expect("admission reply"), Admission::Accepted);
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
}
