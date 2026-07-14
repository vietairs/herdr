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

/// A request from a co-located federation connection to be serviced against the
/// live `App` on the server event loop. Read-only requests and the two
/// remote-input forwards (`SendInput`/`Resize`) mirror the federation protocol
/// host surface; each value-producing variant carries its reply channel.
#[allow(dead_code)] // dormant until b0.4 wires the federation listener
pub(crate) enum FederationCommand {
    /// Atomic (snapshot, cursor) pair for a fresh mount. Atomic for free here:
    /// the actor holds `&mut App` exclusively, so no event can slip between the
    /// snapshot and the cursor read.
    Mount(oneshot::Sender<(SessionSnapshot, EventCursor)>),
    /// Events strictly after the given sequence number.
    EventsAfter(u64, oneshot::Sender<Vec<(u64, EventKind)>>),
    /// A subscription to one live terminal's raw output bytes, or `None` if the
    /// terminal id is unknown. Dropping the receiver never affects the PTY.
    SubscribeOutput(String, oneshot::Sender<Option<broadcast::Receiver<Bytes>>>),
    /// The scrollback history (ANSI) to seed a newly opened remote pane.
    ScrollbackReplay(String, oneshot::Sender<Vec<u8>>),
    /// Forward input bytes to a live terminal (fire-and-forget).
    SendInput(String, Vec<u8>),
    /// Resize a live terminal (fire-and-forget).
    Resize {
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
            FederationCommand::Mount(_) => f.write_str("Mount"),
            FederationCommand::EventsAfter(since, _) => write!(f, "EventsAfter({since})"),
            FederationCommand::SubscribeOutput(id, _) => write!(f, "SubscribeOutput({id})"),
            FederationCommand::ScrollbackReplay(id, _) => write!(f, "ScrollbackReplay({id})"),
            FederationCommand::SendInput(id, bytes) => {
                write!(f, "SendInput({id}, {} bytes)", bytes.len())
            }
            FederationCommand::Resize {
                terminal_id,
                cols,
                rows,
            } => write!(f, "Resize({terminal_id}, {cols}x{rows})"),
            FederationCommand::AgentStatuses(_) => f.write_str("AgentStatuses"),
        }
    }
}

/// Services one [`FederationCommand`] against the live `App`. Called only from
/// the server event loop's single `&mut self` dispatch point. A dropped reply
/// receiver (worker gone) is ignored — the `send` result is discarded.
#[allow(dead_code)] // dormant until b0.4 wires the federation listener
pub(crate) fn dispatch(app: &mut App, command: FederationCommand) {
    match command {
        FederationCommand::Mount(reply) => {
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
            let _ = reply.send((snapshot, cursor));
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
        FederationCommand::SendInput(terminal_id, bytes) => {
            if let Some(runtime) = app.terminal_runtime_for_terminal_id(&terminal_id) {
                let _ = runtime.try_send_bytes(Bytes::copy_from_slice(&bytes));
            }
        }
        FederationCommand::Resize {
            terminal_id,
            cols,
            rows,
        } => {
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

    #[test]
    fn mount_returns_a_snapshot_and_the_live_cursor() {
        let mut app = test_app();
        let (tx, mut rx) = oneshot::channel();
        dispatch(&mut app, FederationCommand::Mount(tx));
        // dispatch is synchronous and replies before returning; a delivered
        // (snapshot, cursor) pair is the assertion.
        let (_snapshot, _cursor) = rx.try_recv().expect("mount reply delivered");
    }

    #[test]
    fn subscribe_output_for_an_unknown_terminal_is_none() {
        let mut app = test_app();
        let (tx, mut rx) = oneshot::channel();
        dispatch(
            &mut app,
            FederationCommand::SubscribeOutput("no-such-terminal".to_string(), tx),
        );
        assert!(rx.try_recv().expect("reply delivered").is_none());
    }

    #[test]
    fn send_input_and_resize_for_an_unknown_terminal_are_silent_noops() {
        let mut app = test_app();
        // Neither should panic nor require the terminal to exist.
        dispatch(
            &mut app,
            FederationCommand::SendInput("no-such-terminal".to_string(), b"hi".to_vec()),
        );
        dispatch(
            &mut app,
            FederationCommand::Resize {
                terminal_id: "no-such-terminal".to_string(),
                cols: 80,
                rows: 24,
            },
        );
    }

    #[test]
    fn events_after_on_a_fresh_app_is_empty() {
        let mut app = test_app();
        let (tx, mut rx) = oneshot::channel();
        dispatch(&mut app, FederationCommand::EventsAfter(0, tx));
        assert!(rx.try_recv().expect("reply delivered").is_empty());
    }
}
