//! Local federation client (P4): dials a `federation-serve` host over an
//! already-established transport, runs the P1 handshake + atomic mount, and
//! hands the caller a live [`RemoteMirror`] plus the still-open
//! reader/writer so [`drive_event_channel`] can apply the ordered event
//! stream. One in-flight mount attempt at a time per client (idempotent —
//! requirement 1).
//!
//! Generic over `AsyncRead + AsyncWrite` so tests dial the P3 in-process
//! `LoopbackFederationServer`; real SSH wiring
//! (`prepare_remote_herdr`/`ensure_remote_server_ready`/`SshStdioBridge` in
//! `src/remote/unix.rs`) is P8's CLI trigger (see the phase file's Files
//! section) — this module never calls into `unix.rs`.
//!
//! No pane bytes here (P5 on focus) — this module only drives the
//! handshake/mount/event channels.
//!
//! Per the plan's own risk/rollback note ("new modules unused by any live
//! path until P8 triggers a mount"), nothing in production `App`/`AppState`
//! constructs a `FederationClient` yet — only this module's own tests do —
//! so most of this module is dead code outside `#[cfg(test)]` until P8/P9
//! wire a real call site; allowed at module scope rather than sprinkled
//! per-item.
#![allow(dead_code)]

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;
use tokio::sync::Mutex as AsyncMutex;

#[cfg(test)]
use crate::api::schema::common::AgentStatus;
use crate::api::EventHub;
use crate::pane::RelayedAgentStatus;

use super::id::{HostKey, Mount, ServerInstanceId};
use super::protocol::{
    Capability, ClipboardMessage, FaultReason, FederationMessage, Handshake, HandshakeResponse,
    MountSnapshot, RejectReason, ScrollbackReplay, TerminalChannelMessage,
    FEDERATION_PROTOCOL_VERSION,
};
use super::reducer::{ReducerAction, RemoteMirror};
use super::serve::{read_frame, write_frame};

/// Reason a mount attempt did not produce a live mirror. Every variant maps
/// to an actionable status a caller (P8's sidebar) can render; none of them
/// panic, and `connect_and_mount` never blocks longer than its own I/O (see
/// `spawn_non_interactive_mount` for the "never blocks the event loop"
/// guarantee, S1.3).
#[derive(Debug)]
pub(crate) enum MountError {
    /// The peer rejected the handshake outright (protocol version skew).
    Rejected(RejectReason),
    /// The handshake was accepted, but the agreed capability set is missing
    /// one this client requires (RT-F3/F4). `negotiate()` itself never
    /// fails on a capability mismatch alone (unknown/absent capabilities are
    /// just excluded from the agreed set) — this is a client-side gate
    /// applied *after* a successful `Accept`, which is what actually
    /// produces a "federation unsupported" outcome for a peer that never
    /// advertised a capability this client needs.
    MissingCapability(Capability),
    /// The peer closed the link (or sent an unexpected message) before the
    /// handshake/mount sequence completed.
    ProtocolViolation(&'static str),
    /// Underlying transport I/O failed.
    Io(std::io::Error),
    /// A mount attempt was already in flight on this client.
    AlreadyInFlight,
}

impl std::fmt::Display for MountError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rejected(reason) => write!(f, "federation handshake rejected: {reason:?}"),
            Self::MissingCapability(cap) => {
                write!(f, "remote does not support required capability {cap:?}")
            }
            Self::ProtocolViolation(what) => write!(f, "federation protocol violation: {what}"),
            Self::Io(err) => write!(f, "federation link I/O error: {err}"),
            Self::AlreadyInFlight => write!(f, "a federation mount attempt is already in flight"),
        }
    }
}

impl std::error::Error for MountError {}

impl From<std::io::Error> for MountError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

/// Successful outcome of `connect_and_mount`: a live [`RemoteMirror`]
/// (already seeded from the atomic snapshot) and the still-open
/// reader/writer halves so the caller can drive the event-channel loop.
pub(crate) struct MountedConnection<R, W> {
    pub(crate) mirror: RemoteMirror,
    pub(crate) agreed_capabilities: BTreeSet<Capability>,
    pub(crate) reader: R,
    pub(crate) writer: W,
}

/// Owns one mount's lifecycle: connect, handshake, mount, and (via
/// `drive_event_channel`) apply the ordered event stream into a
/// `RemoteMirror`. `required_capabilities` lets a caller declare which
/// capabilities must be present in the agreed set for the mount to be
/// usable; P4 requires none by default (metadata-only, no pane bytes / no
/// agent-status relay yet).
pub(crate) struct FederationClient {
    host_key: HostKey,
    local_capabilities: BTreeSet<Capability>,
    required_capabilities: BTreeSet<Capability>,
    next_generation: AtomicU64,
    in_flight: AsyncMutex<()>,
}

impl FederationClient {
    pub(crate) fn new(
        host_key: HostKey,
        local_capabilities: BTreeSet<Capability>,
        required_capabilities: BTreeSet<Capability>,
    ) -> Self {
        Self {
            host_key,
            local_capabilities,
            required_capabilities,
            next_generation: AtomicU64::new(0),
            in_flight: AsyncMutex::new(()),
        }
    }

    /// Performs the handshake + atomic mount over `reader`/`writer`. Runs
    /// to completion inline — callers that must not block their own event
    /// loop should invoke this from a dedicated task; see
    /// `spawn_non_interactive_mount`.
    pub(crate) async fn connect_and_mount<R, W>(
        &self,
        mut reader: R,
        mut writer: W,
    ) -> Result<MountedConnection<R, W>, MountError>
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let Ok(_guard) = self.in_flight.try_lock() else {
            return Err(MountError::AlreadyInFlight);
        };

        write_frame(
            &mut writer,
            &FederationMessage::Handshake(Handshake {
                federation_protocol_version: FEDERATION_PROTOCOL_VERSION,
                capabilities: self.local_capabilities.clone(),
                // The client does not run a `FederationHost` of its own; this
                // id only identifies a host-side boot. It is not used by the
                // server (which fences on its *own* `server_instance_id`,
                // returned in the `MountSnapshot` below).
                server_instance_id: ServerInstanceId("local-client".to_string()),
            }),
        )
        .await?;

        let agreed_capabilities = match read_frame(&mut reader).await? {
            Some(FederationMessage::HandshakeResponse(HandshakeResponse::Accept {
                agreed_capabilities,
            })) => agreed_capabilities,
            Some(FederationMessage::HandshakeResponse(HandshakeResponse::Reject { reason })) => {
                return Err(MountError::Rejected(reason));
            }
            Some(_) => {
                return Err(MountError::ProtocolViolation(
                    "expected a HandshakeResponse after Handshake",
                ))
            }
            None => {
                return Err(MountError::ProtocolViolation(
                    "link closed before a HandshakeResponse arrived",
                ))
            }
        };

        for required in &self.required_capabilities {
            if !agreed_capabilities.contains(required) {
                return Err(MountError::MissingCapability(required.clone()));
            }
        }

        let (server_instance_id, snapshot, cursor) = match read_frame(&mut reader).await? {
            Some(FederationMessage::MountSnapshot(MountSnapshot {
                server_instance_id,
                snapshot,
                cursor,
            })) => (server_instance_id, snapshot, cursor),
            Some(_) => {
                return Err(MountError::ProtocolViolation(
                    "expected a MountSnapshot after HandshakeResponse::Accept",
                ))
            }
            None => {
                return Err(MountError::ProtocolViolation(
                    "link closed before a MountSnapshot arrived",
                ))
            }
        };

        let generation = self.next_generation.fetch_add(1, Ordering::SeqCst) + 1;
        let mount = Mount {
            host_key: self.host_key.clone(),
            server_instance_id,
            mount_generation: generation,
        };
        let mut mirror = RemoteMirror::new(mount);
        mirror.apply_snapshot(&snapshot, cursor);

        Ok(MountedConnection {
            mirror,
            agreed_capabilities,
            reader,
            writer,
        })
    }
}

/// Why `drive_event_channel` returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DriveOutcome {
    /// The peer closed the link.
    LinkClosed,
    /// The peer sent a control-channel fault frame naming why it is tearing the
    /// mount down. Fail-fast (v1): the caller ends the session, it does not
    /// remount. The reason is best-effort context for the exit.
    Faulted(FaultReason),
    /// A `Gap`/`Reset` was observed; the caller must remount (a fresh
    /// `connect_and_mount` call over a *new* connection — the wire has no
    /// "request a fresh snapshot on this connection" message, so a full
    /// remount is the only re-sync primitive P1/P3 provide) and then call
    /// `RemoteMirror::reconcile_by_diff` with the new snapshot.
    ResyncRequired,
}

/// Classifies a drive task's exit as session-ending or not. `Some(reason)`
/// for `LinkClosed`/`Faulted`/`Err` — the caller should tear the mount and
/// its materialized workspaces down. `None` for `Ok(ResyncRequired)`, which
/// requires a remount rather than teardown.
pub(crate) fn drive_outcome_ended_reason(
    outcome: &Result<DriveOutcome, std::io::Error>,
) -> Option<String> {
    match outcome {
        Ok(DriveOutcome::LinkClosed) => Some("link closed".to_string()),
        Ok(DriveOutcome::Faulted(reason)) => Some(format!("{reason:?}")),
        Ok(DriveOutcome::ResyncRequired) => None,
        Err(err) => Some(err.to_string()),
    }
}

/// Reads event-channel messages from `reader` in a loop, applying each to
/// `mirror`. Runs until the link closes or a gap/reset requires a remount.
/// `generation` is the mount generation this task was spawned to serve —
/// fenced on every message via `RemoteMirror::apply_event_message`, so a
/// task left running after a reconnect (whose generation the mirror has
/// since moved past) can never mutate the newer mirror (codex #2).
///
/// `hub` is accepted for the shape the phase's replica-reducer requirement
/// expects (event application feeding local `EventHub::push`); see
/// `reducer`'s module docs for why a bare wire `EventFrame` cannot itself be
/// turned into a push here — the actual local pushes happen in
/// `RemoteMirror::reconcile_by_diff`, called by the caller after a resync.
pub(crate) async fn drive_event_channel<R: AsyncRead + Unpin>(
    reader: &mut R,
    mirror: &mut RemoteMirror,
    generation: u64,
    hub: &EventHub,
) -> Result<DriveOutcome, std::io::Error> {
    let _ = hub;
    loop {
        let Some(msg) = read_frame(reader).await? else {
            return Ok(DriveOutcome::LinkClosed);
        };
        let FederationMessage::Event(event_msg) = msg else {
            // Terminal/AgentStatus/Clipboard channels are P5/P6/P7 scope;
            // this loop only drives the event channel.
            continue;
        };
        match mirror.apply_event_message(&event_msg, generation) {
            ReducerAction::RejectedStale | ReducerAction::Ignored => continue,
            ReducerAction::Applied { .. } => continue,
            ReducerAction::GapDetected { .. } | ReducerAction::ResetRequired => {
                return Ok(DriveOutcome::ResyncRequired);
            }
        }
    }
}

/// Capacity (in messages) of one remote pane's demuxed byte-in channel — the
/// `RemoteTerminalSourceConfig::output_rx` end this router feeds. Sized
/// generously so a pane that is briefly slower than the wire does not
/// immediately drop bytes; a pane that stays behind this simply misses
/// bytes (see `route_inbound`'s `try_send`, S2.2 isolation: this router's
/// single read loop must never block on one slow/unfocused pane).
const TERMINAL_OUTPUT_CHANNEL_CAPACITY: usize = 4096;

/// Capacity (in messages) of one mount's inbound `Clipboard`-channel queue
/// (P7 requirement 5 / S2.2: per-mount channel budget). Each individual
/// `ClipboardMessage` is already bounded by `Channel::Clipboard::max_len()`
/// (16 MiB) at the codec, but an unbounded *queue depth* would still let a
/// remote that floods `Clipboard` frames faster than the consumer drains
/// them grow this buffer without limit; bounding it here closes that gap.
/// `drive_mount_channel` uses `try_send` (never `.await`s), so a full queue
/// degrades by dropping the newest overflow message rather than stalling
/// the ONE mount tunnel's shared read loop (same isolation shape as
/// `TerminalChannelRouter::route_inbound`).
const CLIPBOARD_CHANNEL_CAPACITY: usize = 64;

/// Everything [`drive_mount_channel`] needs to spawn a real local
/// `TerminalRuntime` for a remote-created pane (`SplitPaneResponse::Created`)
/// and hand it back to `App`. Mirrors the constants `App::
/// materialize_federation_mount`/`build_remote_pane` (`app/creation.rs`) use
/// at mount time — `rows`/`cols`/`scrollback_limit_bytes`/
/// `host_terminal_theme` are captured once when the mount's drive task is
/// spawned rather than re-queried per split (same "v1: splits materialize as
/// a simple chain" simplification already accepted for mount-time panes).
pub(crate) struct SplitMaterializationContext {
    pub(crate) rows: u16,
    pub(crate) cols: u16,
    pub(crate) scrollback_limit_bytes: usize,
    pub(crate) host_terminal_theme: crate::terminal_theme::TerminalTheme,
    pub(crate) events: mpsc::Sender<crate::events::AppEvent>,
    pub(crate) render_notify: std::sync::Arc<tokio::sync::Notify>,
    pub(crate) render_dirty: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// This mount's own `HostKey`, stamped onto every `FederationSplitPaneReady`/
    /// `FederationSplitPaneFailed` this drive task emits. `App` compares it
    /// against the originating `PendingRemoteSplit::origin` before splicing a
    /// pane in, so a second, differently-mounted host cannot answer a split
    /// request `dispatch_remote_pane_split` sent to a different mount (a
    /// predicted/observed process-global `request_id` is not enough on its
    /// own to splice a foreign host's pane into this mount's workspace).
    pub(crate) origin: crate::remote::federation::id::HostKey,
}

/// Demultiplexes ONE mount's single `Terminal`-channel wire stream by
/// `terminal_id` (requirement 9: every open remote pane's byte channel rides
/// the ONE mount tunnel, never a per-pane connection). Built once per mount;
/// panes register with `open_terminal` as they focus (P8 lazy hydrate,
/// requirement 9) and deregister on `Close`.
#[derive(Default)]
pub(crate) struct TerminalChannelRouter {
    output_senders: HashMap<String, mpsc::Sender<Bytes>>,
    /// Per-pane sink for relayed `AgentStatus` (P6), keyed by the same raw
    /// (un-namespaced) `terminal_id` `output_senders` uses — the wire's
    /// `AgentStatusMessage::terminal_id` is the remote's raw id too, so no
    /// extra namespace mapping is needed to route it back to the pane that
    /// `build_remote_pane` registered it for.
    agent_status_senders: HashMap<String, mpsc::Sender<RelayedAgentStatus>>,
}

impl TerminalChannelRouter {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Registers `terminal_id`'s relayed-agent-status sink
    /// (`PaneRuntime::relayed_agent_status_sender`) so `route_agent_status`
    /// can forward this mount's inbound `AgentStatus` frames into the
    /// matching pane's detection loop.
    pub(crate) fn register_agent_status_sender(
        &mut self,
        terminal_id: String,
        sender: mpsc::Sender<RelayedAgentStatus>,
    ) {
        self.agent_status_senders.insert(terminal_id, sender);
    }

    /// Routes one inbound `AgentStatus` (+ identity) value to the registered
    /// pane, if any. `try_send` (never `.await`) so a slow/unfocused pane
    /// can never stall this router's single read loop (same isolation shape
    /// as `route_inbound`).
    pub(crate) fn route_agent_status(&mut self, terminal_id: &str, relayed: RelayedAgentStatus) {
        if let Some(tx) = self.agent_status_senders.get(terminal_id) {
            let _ = tx.try_send(relayed);
        }
    }

    /// Registers `terminal_id` as open and sends the `Open` request
    /// outbound. Returns the receiver end `RemoteTerminalSourceConfig::
    /// output_rx` consumes — the server's `Open` acknowledgement (carrying
    /// scrollback replay) and every subsequent `Output` frame for this
    /// terminal id are pushed onto it, in wire order (RT-F6: replay first).
    pub(crate) fn open_terminal(
        &mut self,
        terminal_id: String,
        mount_generation: u64,
        out_tx: &mpsc::UnboundedSender<FederationMessage>,
    ) -> mpsc::Receiver<Bytes> {
        let (tx, rx) = mpsc::channel::<Bytes>(TERMINAL_OUTPUT_CHANNEL_CAPACITY);
        self.output_senders.insert(terminal_id.clone(), tx);
        let _ = out_tx.send(FederationMessage::Terminal(TerminalChannelMessage::Open {
            terminal_id,
            mount_generation,
            replay: ScrollbackReplay { bytes: Vec::new() },
        }));
        rx
    }

    /// Explicitly deregisters `terminal_id` (e.g. the pane lost focus and is
    /// going back to metadata-only, S12.1/S12.3) without waiting for a
    /// server-side `Close` echo.
    pub(crate) fn forget(&mut self, terminal_id: &str) {
        self.output_senders.remove(terminal_id);
        self.agent_status_senders.remove(terminal_id);
    }

    /// Routes one inbound `Terminal` message to the registered pane, if
    /// any. Uses `try_send` (never `.await`) so a slow/unfocused pane can
    /// never stall this router's caller — the ONE mount tunnel's single
    /// read loop must keep servicing every other pane and the event/agent
    /// channels regardless (S2.2 isolation).
    pub(crate) fn route_inbound(&mut self, msg: TerminalChannelMessage) {
        match msg {
            TerminalChannelMessage::Open {
                terminal_id,
                replay,
                ..
            } => {
                if let Some(tx) = self.output_senders.get(&terminal_id) {
                    if !replay.bytes.is_empty() {
                        let _ = tx.try_send(Bytes::from(replay.bytes));
                    }
                }
            }
            TerminalChannelMessage::Output {
                terminal_id, bytes, ..
            } => {
                if let Some(tx) = self.output_senders.get(&terminal_id) {
                    let _ = tx.try_send(Bytes::from(bytes));
                }
            }
            TerminalChannelMessage::Close { terminal_id, .. } => {
                self.output_senders.remove(&terminal_id);
            }
            // Input/Resize are outbound-only from this client's perspective
            // (sent by `RemoteTerminalSourceHandle`, never received back).
            TerminalChannelMessage::Input { .. } | TerminalChannelMessage::Resize { .. } => {}
        }
    }
}

/// Wraps a local paste (including image payloads, RT-F7) as an origin-tagged
/// `Clipboard` message and sends it outward on the shared mount link. Dormant
/// helper: no production call site until P8 wires a real paste keybinding to
/// a live mount.
pub(crate) fn send_local_clipboard_to_remote(
    out_tx: &mpsc::UnboundedSender<FederationMessage>,
    origin_tag: impl Into<String>,
    payload: Vec<u8>,
) {
    let _ = out_tx.send(FederationMessage::Clipboard(ClipboardMessage {
        origin_tag: origin_tag.into(),
        payload,
    }));
}

/// Like `drive_event_channel`, but additionally routes inbound `Terminal`
/// and `Clipboard` messages — the P5 counterpart needed once a live mount
/// opens per-pane channels. Event-channel handling (ordering/gap/reset) is
/// identical to `drive_event_channel`; kept as a separate, additive function
/// (rather than changing `drive_event_channel`'s signature) so P4's
/// mount-only callers/tests are untouched.
pub(crate) async fn drive_mount_channel<R: AsyncRead + Unpin>(
    reader: &mut R,
    mirror: &mut RemoteMirror,
    generation: u64,
    hub: &EventHub,
    router: &mut TerminalChannelRouter,
    clipboard_tx: &mpsc::Sender<ClipboardMessage>,
    out_tx: &mpsc::UnboundedSender<FederationMessage>,
    outbound_clipboard_tx: &mpsc::UnboundedSender<ClipboardMessage>,
    split_materialization: Option<&SplitMaterializationContext>,
) -> Result<DriveOutcome, std::io::Error> {
    // Post-mount pane mirroring fix (plans/260722-1327): a structural
    // `EventFrame` (pane/tab created/closed) carries no entity payload (see
    // `reducer.rs`'s module docs), so it is applied purely for cursor
    // bookkeeping above and cannot itself update the mirror. Instead, ask
    // the server for a fresh full snapshot over this same tunnel
    // (`SnapshotRequest`/`SnapshotResponse`, additive within
    // `FEDERATION_PROTOCOL_VERSION` 3) and diff it in via
    // `RemoteMirror::reconcile_by_diff` — the same resync primitive the
    // `Gap`/`Reset` path already uses. Coalesced: a burst of structural
    // frames while a request is already in flight sends only one.
    let mut resync_in_flight = false;
    loop {
        let Some(msg) = read_frame(reader).await? else {
            return Ok(DriveOutcome::LinkClosed);
        };
        match msg {
            FederationMessage::Event(event_msg) => {
                match mirror.apply_event_message(&event_msg, generation) {
                    ReducerAction::RejectedStale | ReducerAction::Ignored => continue,
                    ReducerAction::Applied { kind, .. } => {
                        if !resync_in_flight && is_structural_event_kind(kind) {
                            resync_in_flight = out_tx
                                .send(FederationMessage::SnapshotRequest(
                                    super::protocol::SnapshotRequest,
                                ))
                                .is_ok();
                        }
                        continue;
                    }
                    ReducerAction::GapDetected { .. } | ReducerAction::ResetRequired => {
                        return Ok(DriveOutcome::ResyncRequired);
                    }
                }
            }
            FederationMessage::SnapshotResponse(MountSnapshot {
                snapshot, cursor, ..
            }) => {
                resync_in_flight = false;
                // `MountSnapshot` carries no per-message generation tag to
                // fence against (only `TerminalChannelMessage`/
                // `AgentStatusMessage` do); this drive task already only
                // runs for the live generation it was spawned with (v1: no
                // remount within one session, matching the rest of this
                // module's fencing scope), so this is safe as-is.
                let mount = mirror.mount().clone();
                let diff = mirror.reconcile_by_diff(&snapshot, cursor, hub);
                // Post-mount pane mirroring fix, part 2
                // (plans/260722-1327): splice a newly-resynced remote pane
                // into (or tear one out of) the already-live mounted `App`
                // layout — `reconcile_by_diff` above only updated the
                // mirror's own metadata and the sidebar-facing local
                // `EventHub`. `split_materialization` is `None` for
                // callers that never wire a live session behind this mount
                // (e.g. tests), same convention as `SplitPaneResponse::
                // Created` above.
                if let Some(ctx) = split_materialization {
                    for pane_info in diff.created_panes {
                        materialize_resync_pane(
                            &mount,
                            generation,
                            router,
                            out_tx,
                            outbound_clipboard_tx,
                            ctx,
                            pane_info,
                        )
                        .await;
                    }
                    for pane_id in diff.removed_pane_ids {
                        let _ = ctx
                            .events
                            .send(crate::events::AppEvent::FederationResyncPaneRemoved {
                                origin: ctx.origin.clone(),
                                pane_id,
                            })
                            .await;
                    }
                }
            }
            FederationMessage::Terminal(term_msg) => {
                router.route_inbound(term_msg);
            }
            FederationMessage::Clipboard(clip_msg) => {
                // Bounded, non-blocking (S2.2): a full clipboard queue must
                // never stall this router's single read loop, which also
                // services the event/terminal channels for the whole mount.
                let _ = clipboard_tx.try_send(clip_msg);
            }
            FederationMessage::Fault(fault) => {
                // The server is tearing the mount down and named the cause;
                // fail-fast — end the drive, do not remount.
                return Ok(DriveOutcome::Faulted(fault.reason));
            }
            FederationMessage::AgentStatus(status_msg) => {
                // Update the mirror (S14.1/S14.2 honest display source) and
                // forward the same status straight to the pane's own
                // detection loop via the router's raw-terminal-id-keyed
                // sink — `apply_agent_status`'s `ReducerAction` return is
                // display bookkeeping only, `route_agent_status` here is
                // the actual sidebar-visible relay (P6/P8/P9 wiring).
                match mirror.apply_agent_status(&status_msg, generation, hub) {
                    ReducerAction::RejectedStale => continue,
                    ReducerAction::GapDetected { .. } | ReducerAction::ResetRequired => {
                        return Ok(DriveOutcome::ResyncRequired);
                    }
                    ReducerAction::Ignored | ReducerAction::Applied { .. } => {}
                }
                router.route_agent_status(
                    &status_msg.terminal_id,
                    RelayedAgentStatus {
                        status: status_msg.status,
                        agent: status_msg.agent.clone(),
                    },
                );
            }
            // Handshake/HandshakeResponse/MountSnapshot are already
            // consumed during `connect_and_mount`.
            FederationMessage::Handshake(_)
            | FederationMessage::HandshakeResponse(_)
            | FederationMessage::MountSnapshot(_) => continue,
            // `SplitPaneRequest`/`SnapshotRequest` are client->server only
            // (this loop drives the client side of a mount); the peer
            // should never send either.
            FederationMessage::SplitPaneRequest(_) => {
                tracing::debug!("federation client received a SplitPaneRequest; ignoring");
            }
            FederationMessage::SnapshotRequest(_) => {
                tracing::debug!("federation client received a SnapshotRequest; ignoring");
            }
            // The remote host already performed the real split (see
            // `server::federation_actor::FederationCommand::SplitPane`) by
            // the time this arrives. This loop owns `router`/`out_tx` (the
            // only place the mount's `TerminalChannelRouter`/writer handle
            // live), so it opens the new pane's terminal channel and spawns
            // its `TerminalRuntime` directly here, then hands the finished
            // runtime back to `App` via `AppEvent::FederationSplitPaneReady`
            // for layout insertion (which needs `&mut App`, unavailable in
            // this task). `split_materialization` is `None` for callers that
            // never wire a live session behind this mount (e.g. tests, or a
            // future path with nothing to materialize into).
            FederationMessage::SplitPaneResponse(response) => match response {
                super::protocol::SplitPaneResponse::Created {
                    request_id,
                    new_pane_id,
                    new_terminal_id,
                } => {
                    let Some(ctx) = split_materialization else {
                        tracing::info!(
                            request_id,
                            %new_terminal_id,
                            "remote split succeeded; no live session to materialize it into"
                        );
                        continue;
                    };
                    let output_rx =
                        router.open_terminal(new_terminal_id.clone(), generation, out_tx);
                    let pane_id = crate::layout::PaneId::alloc();
                    let terminal_id = crate::terminal::TerminalId::alloc();
                    match crate::terminal::TerminalRuntime::spawn_remote(
                        pane_id,
                        ctx.rows,
                        ctx.cols,
                        ctx.scrollback_limit_bytes,
                        ctx.host_terminal_theme,
                        None,
                        new_terminal_id.clone(),
                        generation,
                        out_tx.clone(),
                        output_rx,
                        outbound_clipboard_tx.clone(),
                        ctx.events.clone(),
                        ctx.render_notify.clone(),
                        ctx.render_dirty.clone(),
                    ) {
                        Ok(runtime) => {
                            if let Some(sender) = runtime.relayed_agent_status_sender() {
                                router
                                    .register_agent_status_sender(new_terminal_id.clone(), sender);
                            }
                            // C1 fix (plans/260722-1327 review): register this
                            // split-created pane in the mirror BEFORE it can
                            // ever be seen again through a resync snapshot,
                            // so `reconcile_by_diff` never re-classifies it
                            // as newly created and double-materializes it.
                            // The returned namespaced id also lets `App`
                            // register the same pane in its own
                            // `remote_resync_pane_index` reverse index, so a
                            // later resync-driven removal of this pane can
                            // find and tear it down too (M1).
                            let remote_pane_id =
                                mirror.register_split_pane(&new_pane_id, &new_terminal_id);
                            let terminal = crate::terminal::TerminalState::new(
                                terminal_id.clone(),
                                std::path::PathBuf::from("/"),
                            );
                            let pane_state = crate::pane::PaneState::new(terminal_id.clone());
                            let ready = crate::events::FederationSplitPaneReady {
                                request_id,
                                origin: ctx.origin.clone(),
                                remote_pane_id,
                                pane_id,
                                terminal_id,
                                terminal,
                                runtime,
                                pane_state,
                            };
                            let _ = ctx
                                .events
                                .send(crate::events::AppEvent::FederationSplitPaneReady(Box::new(
                                    ready,
                                )))
                                .await;
                        }
                        Err(err) => {
                            // Tell `App` so it can drop the matching
                            // `pending_remote_splits` entry (via
                            // `handle_federation_split_pane_failed`) instead
                            // of leaving it orphaned in the map forever.
                            tracing::warn!(
                                request_id,
                                %err,
                                "failed to spawn a local runtime for the remote-split pane"
                            );
                            let _ = ctx
                                .events
                                .send(crate::events::AppEvent::FederationSplitPaneFailed {
                                    request_id,
                                    reason: err.to_string(),
                                    origin: ctx.origin.clone(),
                                })
                                .await;
                        }
                    }
                }
                super::protocol::SplitPaneResponse::Failed { request_id, reason } => {
                    tracing::warn!(request_id, %reason, "remote split failed");
                    if let Some(ctx) = split_materialization {
                        let _ = ctx
                            .events
                            .send(crate::events::AppEvent::FederationSplitPaneFailed {
                                request_id,
                                reason,
                                origin: ctx.origin.clone(),
                            })
                            .await;
                    }
                }
            },
        }
    }
}

/// Builds one resync-revealed remote pane's real local `TerminalRuntime`
/// (mirrors the `SplitPaneResponse::Created` arm above and `App::
/// build_remote_pane`'s mount-time counterpart) and hands it back to `App`
/// via `AppEvent::FederationResyncPaneCreated` for layout insertion — this
/// drive task owns `router`/`out_tx`, unavailable to `&mut App`. A spawn
/// failure is logged and dropped rather than surfaced as a toast (unlike a
/// user-initiated split, there is no pending local request/toast target to
/// fail here — the remote pane simply stays mirror-only metadata until the
/// next resync retries).
async fn materialize_resync_pane(
    mount: &super::id::Mount,
    generation: u64,
    router: &mut TerminalChannelRouter,
    out_tx: &mpsc::UnboundedSender<FederationMessage>,
    outbound_clipboard_tx: &mpsc::UnboundedSender<ClipboardMessage>,
    ctx: &SplitMaterializationContext,
    pane_info: crate::api::schema::panes::PaneInfo,
) {
    let raw_terminal_id = super::id::strip_mount_namespace(mount, &pane_info.terminal_id);
    let output_rx = router.open_terminal(raw_terminal_id.clone(), generation, out_tx);
    let pane_id = crate::layout::PaneId::alloc();
    let terminal_id = crate::terminal::TerminalId::alloc();
    match crate::terminal::TerminalRuntime::spawn_remote(
        pane_id,
        ctx.rows,
        ctx.cols,
        ctx.scrollback_limit_bytes,
        ctx.host_terminal_theme,
        None,
        raw_terminal_id.clone(),
        generation,
        out_tx.clone(),
        output_rx,
        outbound_clipboard_tx.clone(),
        ctx.events.clone(),
        ctx.render_notify.clone(),
        ctx.render_dirty.clone(),
    ) {
        Ok(runtime) => {
            if let Some(sender) = runtime.relayed_agent_status_sender() {
                router.register_agent_status_sender(raw_terminal_id, sender);
            }
            let mut terminal = crate::terminal::TerminalState::new(
                terminal_id.clone(),
                pane_info
                    .cwd
                    .clone()
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| std::path::PathBuf::from("/")),
            );
            terminal.manual_label = pane_info.label.clone();
            let pane_state = crate::pane::PaneState::new(terminal_id.clone());
            let ready = crate::events::FederationResyncPaneCreated {
                origin: ctx.origin.clone(),
                workspace_id: pane_info.workspace_id,
                pane_id: pane_info.pane_id,
                local_pane_id: pane_id,
                terminal_id,
                terminal,
                runtime,
                pane_state,
            };
            let _ = ctx
                .events
                .send(crate::events::AppEvent::FederationResyncPaneCreated(
                    Box::new(ready),
                ))
                .await;
        }
        Err(err) => {
            tracing::warn!(
                pane_id = %pane_info.pane_id,
                %err,
                "failed to spawn a local runtime for a resync-revealed remote pane"
            );
        }
    }
}

/// Whether `kind` is a structural mutation a bare `EventFrame` cannot itself
/// apply to the mirror (see `reducer.rs`'s module docs) and therefore must
/// trigger a `SnapshotRequest` resync (post-mount pane mirroring fix,
/// plans/260722-1327). Deliberately narrow: `PaneUpdated`/`*Focused`/etc.
/// change a field on an entity the mirror already has, which the relayed
/// `AgentStatus` channel (or a future field-carrying event) can update
/// in place — only entity create/close/move needs a full resync to learn
/// about an id the mirror has never seen (or must forget).
fn is_structural_event_kind(kind: crate::api::schema::events::EventKind) -> bool {
    use crate::api::schema::events::EventKind;
    matches!(
        kind,
        EventKind::PaneCreated
            | EventKind::PaneClosed
            | EventKind::PaneMoved
            | EventKind::TabCreated
            | EventKind::TabClosed
            | EventKind::TabMoved
    )
}

/// Spawns the client-side counterpart of `serve::run`'s `writer_task`:
/// drains an internal outbound queue and writes each `FederationMessage` to
/// `writer` in order. Returns the sender callers use — directly, or via
/// `TerminalChannelRouter::open_terminal`/`send_local_clipboard_to_remote` —
/// to enqueue outbound frames, plus the task's `JoinHandle` so a caller can
/// await it on teardown (drop the sender to end the loop, mirroring
/// `serve.rs`'s `drop(out_tx); let _ = writer_task.await;` shutdown
/// sequence). P9 materialization's first real production call site for this
/// exact pattern (client-side); `serve.rs`'s `run` has carried the identical
/// shape since P3.
pub(crate) fn spawn_mount_writer<W>(
    mut writer: W,
) -> (
    mpsc::UnboundedSender<FederationMessage>,
    tokio::task::JoinHandle<()>,
)
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<FederationMessage>();
    let handle = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            if write_frame(&mut writer, &msg).await.is_err() {
                break;
            }
        }
    });
    (out_tx, handle)
}

/// Runs `connect_and_mount` on its own task so the caller's event loop is
/// never blocked by federation I/O — including a peer that never responds
/// at all (e.g. an SSH prompt; the actual TTY-prompt pre-resolution is
/// wired in P8 — this is the structural non-blocking guarantee P4 owns,
/// S1.3). The result — success or an actionable [`MountError`] — is
/// delivered over the returned `JoinHandle`; the spawned task itself never
/// panics (every failure path is a typed `Err`).
pub(crate) fn spawn_non_interactive_mount<R, W>(
    client: std::sync::Arc<FederationClient>,
    reader: R,
    writer: W,
) -> tokio::task::JoinHandle<Result<MountedConnection<R, W>, MountError>>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    tokio::spawn(async move { client.connect_and_mount(reader, writer).await })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use crate::remote::federation::loopback::{FixtureHost, LoopbackFederationServer};
    use crate::remote::federation::protocol::Capability;

    use super::*;

    fn host_key() -> HostKey {
        HostKey::new("alice@10.0.0.1", "s1")
    }

    // Test 1: handshake+mount over loopback succeeds and yields a live
    // mirror fenced to a fresh generation; no collision with a pre-existing
    // local id is exercised at the reducer level (`reducer::tests`), since
    // that requires real snapshot content the shared `FixtureHost` fixture
    // does not carry.
    #[tokio::test]
    async fn handshake_and_mount_over_loopback_yields_a_live_mirror() {
        let host = Arc::new(FixtureHost::new());
        let (duplex, _server) = LoopbackFederationServer::spawn(host);
        let (reader, writer) = tokio::io::split(duplex);

        let client = FederationClient::new(host_key(), BTreeSet::new(), BTreeSet::new());
        let mounted = client
            .connect_and_mount(reader, writer)
            .await
            .expect("loopback handshake+mount should succeed");

        assert_eq!(mounted.mirror.mount().host_key, host_key());
        assert_eq!(mounted.mirror.mount().mount_generation, 1);
        assert_eq!(mounted.mirror.cursor(), 0);
    }

    // Test 2 (RT-F3/F4): a required capability the peer never advertises
    // makes the mount unusable, even though the wire-level handshake itself
    // Accepts (capability mismatch is never a protocol-level Reject).
    #[tokio::test]
    async fn missing_required_capability_is_rejected_without_creating_a_mirror() {
        let host = Arc::new(FixtureHost::new()); // advertises only SCROLLBACK_REPLAY
        let (duplex, _server) = LoopbackFederationServer::spawn(host);
        let (reader, writer) = tokio::io::split(duplex);

        let required: BTreeSet<Capability> = [Capability::new(Capability::AGENT_STATUS)].into();
        let client = FederationClient::new(host_key(), BTreeSet::new(), required);

        // `MountedConnection` is not `Debug` (its `R`/`W` transport halves
        // aren't), so `Result::expect_err` (which requires `T: Debug`)
        // cannot be used here — match instead.
        let result = client.connect_and_mount(reader, writer).await;
        let err = match result {
            Ok(_) => panic!("missing a required capability must fail the mount"),
            Err(err) => err,
        };

        assert!(matches!(err, MountError::MissingCapability(_)));
    }

    // Test 2 (version half): a protocol-level Reject (version skew) is
    // surfaced as a typed `MountError::Rejected`, not a panic or a silent
    // partial mount. Driven directly (not via `FixtureHost`/`serve::run`,
    // which always speaks the current `FEDERATION_PROTOCOL_VERSION`) so the
    // skew is genuinely exercised.
    #[tokio::test]
    async fn version_mismatch_is_surfaced_as_a_typed_rejection() {
        let (client_side, server_side) = tokio::io::duplex(1 << 16);
        let (client_reader, client_writer) = tokio::io::split(client_side);
        let (mut server_reader, mut server_writer) = tokio::io::split(server_side);

        let fake_server = tokio::spawn(async move {
            let Some(FederationMessage::Handshake(_)) =
                read_frame(&mut server_reader).await.unwrap()
            else {
                panic!("expected a Handshake");
            };
            write_frame(
                &mut server_writer,
                &FederationMessage::HandshakeResponse(HandshakeResponse::Reject {
                    reason: RejectReason::Version {
                        local: FEDERATION_PROTOCOL_VERSION + 1,
                        remote: FEDERATION_PROTOCOL_VERSION,
                    },
                }),
            )
            .await
            .unwrap();
        });

        let client = FederationClient::new(host_key(), BTreeSet::new(), BTreeSet::new());
        let result = client.connect_and_mount(client_reader, client_writer).await;
        let err = match result {
            Ok(_) => panic!("a Reject response must never produce a mount"),
            Err(err) => err,
        };

        assert!(matches!(
            err,
            MountError::Rejected(RejectReason::Version { .. })
        ));
        fake_server.await.unwrap();
    }

    // Test 3 (codex #1 / RT-F5): applying a burst of frames end-to-end
    // (loopback push -> wire -> `drive_event_channel` -> reducer) advances
    // the cursor strictly in order, never reordering/duplicating — the
    // ordering guarantee the single-source EventHub design depends on.
    #[tokio::test]
    async fn a_burst_of_remote_events_is_applied_in_strict_source_order() {
        let host = Arc::new(FixtureHost::new());
        let event_hub = host.event_hub().clone();
        let (duplex, _server) = LoopbackFederationServer::spawn(host);
        let (reader, writer) = tokio::io::split(duplex);

        // `agreed_capabilities` is the *intersection* of local and remote
        // capabilities (`negotiate`) — advertise SCROLLBACK_REPLAY locally
        // too, matching what `FixtureHost` advertises, so the assertion
        // below actually exercises a non-empty agreed set.
        let local_capabilities: BTreeSet<Capability> =
            [Capability::new(Capability::SCROLLBACK_REPLAY)].into();
        let client = FederationClient::new(host_key(), local_capabilities, BTreeSet::new());
        let mut mounted = client.connect_and_mount(reader, writer).await.unwrap();
        assert!(mounted
            .agreed_capabilities
            .contains(&Capability::new(Capability::SCROLLBACK_REPLAY)));
        let generation = mounted.mirror.mount().mount_generation;

        for _ in 0..25 {
            event_hub.push(crate::api::schema::EventEnvelope {
                event: crate::api::schema::events::EventKind::WorkspaceFocused,
                data: crate::api::schema::EventData::WorkspaceFocused {
                    workspace_id: "w1".to_string(),
                },
            });
        }

        let local_hub = EventHub::default();
        // `drive_event_channel` loops until the link closes or a gap/reset
        // forces a resync — neither happens for a clean burst — so it is
        // raced against a timeout; on timeout the future (and its `&mut`
        // borrows of `reader`/`mirror`) is dropped, letting the assertions
        // below inspect the mirror's post-burst state directly.
        let _ = tokio::time::timeout(
            Duration::from_millis(500),
            drive_event_channel(
                &mut mounted.reader,
                &mut mounted.mirror,
                generation,
                &local_hub,
            ),
        )
        .await;

        // 25 pushed events, no gaps possible on a fresh mount with a live
        // poller — cursor must have advanced by exactly 25, strictly in
        // order (each `Applied` requires `source_seq == cursor + 1`, so a
        // final cursor of exactly 25 is only reachable via strict order).
        assert_eq!(mounted.mirror.cursor(), 25);

        // The writer half stays usable after the mount completes (P5 will
        // drive terminal-channel traffic over it) — proven with a benign
        // `Close` for an unopened terminal, which the server harmlessly
        // no-ops on.
        write_frame(
            &mut mounted.writer,
            &FederationMessage::Terminal(
                crate::remote::federation::protocol::TerminalChannelMessage::Close {
                    terminal_id: "never-opened".to_string(),
                    mount_generation: 1,
                },
            ),
        )
        .await
        .unwrap();
    }

    // Test 6 (S1.3): a mount whose peer never responds (standing in for a
    // TTY-prompt-blocked SSH connection, whose real pre-resolution is P8's
    // job) must never block the caller's event loop. Proven by running a
    // concurrent "event loop mock" tick loop alongside the spawned mount
    // attempt and asserting it completes on schedule.
    #[tokio::test]
    async fn a_never_responding_mount_never_blocks_the_event_loop() {
        let (client_side, _server_side_never_reads_or_writes) = tokio::io::duplex(1024);
        let (reader, writer) = tokio::io::split(client_side);

        let client = Arc::new(FederationClient::new(
            host_key(),
            BTreeSet::new(),
            BTreeSet::new(),
        ));
        let mount_task = spawn_non_interactive_mount(client, reader, writer);

        // The "event loop mock": ticks a fixed number of times regardless
        // of the still-pending mount task, proving the caller was never
        // blocked waiting on federation I/O.
        let ticks = tokio::time::timeout(Duration::from_millis(200), async {
            let mut n = 0;
            for _ in 0..3 {
                tokio::time::sleep(Duration::from_millis(5)).await;
                n += 1;
            }
            n
        })
        .await
        .expect("the event-loop mock must never be blocked by the pending mount");
        assert_eq!(ticks, 3);

        assert!(
            !mount_task.is_finished(),
            "the mount attempt should still be pending (peer never responded)"
        );
        mount_task.abort();
    }

    // P5: `TerminalChannelRouter` + `drive_mount_channel` — the client-side
    // channel routing a live mount needs before `RemoteTerminalSourceHandle`
    // can hydrate a pane. Loopback-only (no real SSH), per the phase's TDD
    // plan.

    /// Completes a handshake+mount over a fresh loopback connection, using
    /// the exact same wire steps `loopback::tests::connect_and_mount` does
    /// (that helper is private to `loopback.rs`, so this is a thin local
    /// re-implementation rather than a cross-module reach-in).
    async fn open_mount(
        host: Arc<FixtureHost>,
    ) -> (
        tokio::io::ReadHalf<tokio::io::DuplexStream>,
        tokio::io::WriteHalf<tokio::io::DuplexStream>,
    ) {
        let (duplex, _server) = LoopbackFederationServer::spawn(host);
        let (mut reader, mut writer) = tokio::io::split(duplex);
        write_frame(
            &mut writer,
            &FederationMessage::Handshake(Handshake {
                federation_protocol_version: FEDERATION_PROTOCOL_VERSION,
                capabilities: [Capability::new(Capability::SCROLLBACK_REPLAY)].into(),
                server_instance_id: ServerInstanceId("client-test".to_string()),
            }),
        )
        .await
        .unwrap();
        let Some(FederationMessage::HandshakeResponse(HandshakeResponse::Accept { .. })) =
            read_frame(&mut reader).await.unwrap()
        else {
            panic!("expected HandshakeResponse::Accept");
        };
        let Some(FederationMessage::MountSnapshot(_)) = read_frame(&mut reader).await.unwrap()
        else {
            panic!("expected MountSnapshot");
        };
        (reader, writer)
    }

    // Test 6 (RT-F6, S12.1/S12.3): opening a terminal registers it with the
    // router, and both the `Open` acknowledgement's scrollback replay and
    // every subsequent `Output` frame reach the SAME per-terminal channel,
    // in wire order (replay before live).
    #[tokio::test]
    async fn open_terminal_delivers_replay_then_live_bytes_on_the_same_channel() {
        let (runtime, _rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        let scrollback = b"earlier history".to_vec();
        let host =
            Arc::new(FixtureHost::new().with_terminal("term_1", runtime, scrollback.clone()));
        let (mut reader, writer) = open_mount(host.clone()).await;
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<FederationMessage>();
        let writer_task = tokio::spawn(async move {
            let mut writer = writer;
            while let Some(msg) = out_rx.recv().await {
                if write_frame(&mut writer, &msg).await.is_err() {
                    break;
                }
            }
        });

        let mut router = TerminalChannelRouter::new();
        let mut output_rx = router.open_terminal("term_1".to_string(), 1, &out_tx);

        // Server's `Open` acknowledgement (carrying the scrollback replay) —
        // real wire round-trip against `FixtureHost`/`serve::run`, proving
        // the router applies a genuine server-produced replay payload.
        let Some(FederationMessage::Terminal(open_ack)) = read_frame(&mut reader).await.unwrap()
        else {
            panic!("expected the server's Open acknowledgement");
        };
        router.route_inbound(open_ack);

        // Live bytes: `loopback.rs`'s own tests already prove CX-4 fidelity
        // (a real fixture terminal's tee producing an `Output` frame
        // byte-for-byte); this router-level test only needs to prove
        // ordering (replay before live) once a live `Output` frame for this
        // terminal id arrives, so it is injected directly rather than
        // reaching into `FixtureHost`'s private terminal registry (not this
        // phase's file to modify).
        router.route_inbound(TerminalChannelMessage::Output {
            terminal_id: "term_1".to_string(),
            mount_generation: 1,
            bytes: b"live bytes after replay".to_vec(),
        });

        let first = tokio::time::timeout(std::time::Duration::from_millis(200), output_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first, Bytes::from(scrollback));
        let second = tokio::time::timeout(std::time::Duration::from_millis(200), output_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(second, Bytes::from_static(b"live bytes after replay"));

        writer_task.abort();
    }

    // Test 4 (S2.2 isolation, router half): routing inbound `Output` frames
    // for a pane whose channel is full (`try_send` fails) must never block
    // or panic this router's caller — the ONE mount tunnel's read loop must
    // keep servicing every other pane regardless of one slow consumer.
    #[test]
    fn routing_to_a_full_channel_never_blocks_or_panics() {
        let mut router = TerminalChannelRouter::new();
        let (out_tx, _out_rx) = mpsc::unbounded_channel::<FederationMessage>();
        let mut output_rx = router.open_terminal("term_1".to_string(), 1, &out_tx);

        // Fill the bounded channel without ever draining it.
        for i in 0..(TERMINAL_OUTPUT_CHANNEL_CAPACITY + 10) {
            router.route_inbound(TerminalChannelMessage::Output {
                terminal_id: "term_1".to_string(),
                mount_generation: 1,
                bytes: vec![i as u8],
            });
        }

        // The receiver still has a bounded, non-exploded backlog — proving
        // the router degraded (dropped) rather than panicked or blocked.
        assert!(output_rx.try_recv().is_ok());
    }

    // Test 8 (RT-F7 clipboard, outbound half): a local paste (standing in
    // for an image payload too — this helper is payload-agnostic) crosses
    // the wire as an origin-tagged `Clipboard` message.
    #[tokio::test]
    async fn local_clipboard_paste_crosses_the_wire_origin_tagged() {
        let (client_side, server_side) = tokio::io::duplex(1 << 16);
        let (_client_reader, client_writer) = tokio::io::split(client_side);
        let (mut server_reader, _server_writer) = tokio::io::split(server_side);
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<FederationMessage>();
        let writer_task = tokio::spawn(async move {
            let mut writer = client_writer;
            while let Some(msg) = out_rx.recv().await {
                if write_frame(&mut writer, &msg).await.is_err() {
                    break;
                }
            }
        });

        send_local_clipboard_to_remote(&out_tx, "local", b"pasted payload".to_vec());

        let Some(FederationMessage::Clipboard(ClipboardMessage {
            origin_tag,
            payload,
        })) = read_frame(&mut server_reader).await.unwrap()
        else {
            panic!("expected a Clipboard message");
        };
        assert_eq!(origin_tag, "local");
        assert_eq!(payload, b"pasted payload".to_vec());

        writer_task.abort();
    }

    // P9 materialization: `spawn_mount_writer` is the extracted, reusable
    // form of the writer-pump loop the test above hand-rolls inline — proves
    // the extraction behaves identically (frame arrives, in order) and that
    // dropping the returned sender cleanly ends the spawned task (mirrors
    // `serve.rs`'s own `drop(out_tx); let _ = writer_task.await;` shutdown).
    #[tokio::test]
    async fn spawn_mount_writer_delivers_frames_and_exits_when_sender_drops() {
        let (client_side, server_side) = tokio::io::duplex(1 << 16);
        let (_client_reader, client_writer) = tokio::io::split(client_side);
        let (mut server_reader, _server_writer) = tokio::io::split(server_side);

        let (out_tx, handle) = spawn_mount_writer(client_writer);
        send_local_clipboard_to_remote(&out_tx, "local", b"via spawn_mount_writer".to_vec());

        let Some(FederationMessage::Clipboard(ClipboardMessage {
            origin_tag,
            payload,
        })) = read_frame(&mut server_reader).await.unwrap()
        else {
            panic!("expected a Clipboard message");
        };
        assert_eq!(origin_tag, "local");
        assert_eq!(payload, b"via spawn_mount_writer".to_vec());

        drop(out_tx);
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("writer task must exit once its sender is dropped")
            .expect("writer task must not panic");
    }

    // Test 8 (RT-F7 clipboard, inbound half) + Test 9 (event-channel
    // parity): `drive_mount_channel` routes `Clipboard` messages to the
    // caller's queue and `Terminal` messages to the router, while still
    // applying `Event` frames to the mirror exactly like
    // `drive_event_channel` — proving the additive driver does not regress
    // P4's event-application behavior.
    // Driven against a hand-rolled fake server (like
    // `version_mismatch_is_surfaced_as_a_typed_rejection` above) rather than
    // `FixtureHost`/`LoopbackFederationServer`, so this test can inject an
    // `Event` + `Terminal::Output` + `Clipboard` message in one deterministic
    // sequence without reaching into `loopback.rs`'s private fixture state
    // (not this phase's file to modify).
    #[tokio::test]
    async fn drive_mount_channel_routes_terminal_and_clipboard_while_still_applying_events() {
        let (client_side, server_side) = tokio::io::duplex(1 << 16);
        let (client_reader, client_writer) = tokio::io::split(client_side);
        let (mut server_reader, mut server_writer) = tokio::io::split(server_side);

        let fake_server = tokio::spawn(async move {
            let Some(FederationMessage::Handshake(_)) =
                read_frame(&mut server_reader).await.unwrap()
            else {
                panic!("expected a Handshake");
            };
            write_frame(
                &mut server_writer,
                &FederationMessage::HandshakeResponse(HandshakeResponse::Accept {
                    agreed_capabilities: BTreeSet::new(),
                }),
            )
            .await
            .unwrap();
            write_frame(
                &mut server_writer,
                &FederationMessage::MountSnapshot(MountSnapshot {
                    server_instance_id: ServerInstanceId("fake-server".to_string()),
                    snapshot: crate::remote::federation::serve::empty_snapshot(),
                    cursor: crate::remote::federation::protocol::EventCursor(0),
                }),
            )
            .await
            .unwrap();

            write_frame(
                &mut server_writer,
                &FederationMessage::Event(
                    crate::remote::federation::protocol::EventChannelMessage::Frame(
                        crate::remote::federation::protocol::EventFrame {
                            source_seq: 1,
                            kind: crate::api::schema::events::EventKind::WorkspaceFocused,
                        },
                    ),
                ),
            )
            .await
            .unwrap();
            write_frame(
                &mut server_writer,
                &FederationMessage::Terminal(TerminalChannelMessage::Output {
                    terminal_id: "term_1".to_string(),
                    mount_generation: 1,
                    bytes: b"routed live bytes".to_vec(),
                }),
            )
            .await
            .unwrap();
            write_frame(
                &mut server_writer,
                &FederationMessage::Clipboard(ClipboardMessage {
                    origin_tag: "remote".to_string(),
                    payload: b"remote clip".to_vec(),
                }),
            )
            .await
            .unwrap();

            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        });

        let client = FederationClient::new(host_key(), BTreeSet::new(), BTreeSet::new());
        let mounted = client
            .connect_and_mount(client_reader, client_writer)
            .await
            .unwrap();
        let generation = mounted.mirror.mount().mount_generation;
        let MountedConnection {
            mut mirror,
            mut reader,
            ..
        } = mounted;

        let mut router = TerminalChannelRouter::new();
        let (out_tx, _out_rx) = mpsc::unbounded_channel::<FederationMessage>();
        let mut output_rx = router.open_terminal("term_1".to_string(), generation, &out_tx);

        let (clipboard_tx, mut clipboard_rx) =
            mpsc::channel::<ClipboardMessage>(CLIPBOARD_CHANNEL_CAPACITY);
        let (outbound_clip_tx, _outbound_clip_rx) = mpsc::unbounded_channel::<ClipboardMessage>();
        let hub = EventHub::default();
        let _ = tokio::time::timeout(
            std::time::Duration::from_millis(300),
            drive_mount_channel(
                &mut reader,
                &mut mirror,
                generation,
                &hub,
                &mut router,
                &clipboard_tx,
                &out_tx,
                &outbound_clip_tx,
                None,
            ),
        )
        .await;

        assert_eq!(mirror.cursor(), 1);
        let bytes = tokio::time::timeout(std::time::Duration::from_millis(200), output_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(bytes, Bytes::from_static(b"routed live bytes"));
        let clip = tokio::time::timeout(std::time::Duration::from_millis(200), clipboard_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(clip.origin_tag, "remote");
        assert_eq!(clip.payload, b"remote clip".to_vec());

        fake_server.await.unwrap();
    }

    // Post-mount pane mirroring fix (plans/260722-1327): a burst of
    // structural `EventFrame`s (pane created) sent while a resync is
    // already in flight must coalesce into exactly ONE outbound
    // `SnapshotRequest`, and the eventual `SnapshotResponse` must diff
    // into the mirror via `reconcile_by_diff` so a pane the mount never
    // saw at mount time (or in an earlier `Frame`'s bare payload) becomes
    // visible.
    #[tokio::test]
    async fn a_burst_of_structural_frames_coalesces_into_one_snapshot_request_and_the_response_updates_the_mirror(
    ) {
        let (client_side, server_side) = tokio::io::duplex(1 << 16);
        let (client_reader, client_writer) = tokio::io::split(client_side);
        let (mut server_reader, mut server_writer) = tokio::io::split(server_side);

        let fake_server = tokio::spawn(async move {
            let Some(FederationMessage::Handshake(_)) =
                read_frame(&mut server_reader).await.unwrap()
            else {
                panic!("expected a Handshake");
            };
            write_frame(
                &mut server_writer,
                &FederationMessage::HandshakeResponse(HandshakeResponse::Accept {
                    agreed_capabilities: BTreeSet::new(),
                }),
            )
            .await
            .unwrap();
            write_frame(
                &mut server_writer,
                &FederationMessage::MountSnapshot(MountSnapshot {
                    server_instance_id: ServerInstanceId("fake-server".to_string()),
                    snapshot: crate::remote::federation::serve::empty_snapshot(),
                    cursor: crate::remote::federation::protocol::EventCursor(0),
                }),
            )
            .await
            .unwrap();

            // A burst of three structural frames (a new pane spawned on the
            // serving side after mount, e.g. `agent.start`) — a bare
            // `EventFrame` carries no payload, so the client cannot itself
            // learn the new pane's identity from these alone.
            for seq in 1..=3u64 {
                write_frame(
                    &mut server_writer,
                    &FederationMessage::Event(
                        crate::remote::federation::protocol::EventChannelMessage::Frame(
                            crate::remote::federation::protocol::EventFrame {
                                source_seq: seq,
                                kind: crate::api::schema::events::EventKind::PaneCreated,
                            },
                        ),
                    ),
                )
                .await
                .unwrap();
            }

            let mut snapshot = crate::remote::federation::serve::empty_snapshot();
            snapshot.panes.push(crate::api::schema::panes::PaneInfo {
                pane_id: "pane_new".to_string(),
                terminal_id: "term_new".to_string(),
                workspace_id: "w1".to_string(),
                tab_id: "w1-tab".to_string(),
                focused: false,
                cwd: None,
                foreground_cwd: None,
                label: None,
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
            write_frame(
                &mut server_writer,
                &FederationMessage::SnapshotResponse(MountSnapshot {
                    server_instance_id: ServerInstanceId("fake-server".to_string()),
                    snapshot,
                    cursor: crate::remote::federation::protocol::EventCursor(3),
                }),
            )
            .await
            .unwrap();

            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        });

        let client = FederationClient::new(host_key(), BTreeSet::new(), BTreeSet::new());
        let mounted = client
            .connect_and_mount(client_reader, client_writer)
            .await
            .unwrap();
        let generation = mounted.mirror.mount().mount_generation;
        let MountedConnection {
            mut mirror,
            mut reader,
            ..
        } = mounted;

        let mut router = TerminalChannelRouter::new();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<FederationMessage>();
        let (clipboard_tx, _clipboard_rx) =
            mpsc::channel::<ClipboardMessage>(CLIPBOARD_CHANNEL_CAPACITY);
        let (outbound_clip_tx, _outbound_clip_rx) = mpsc::unbounded_channel::<ClipboardMessage>();
        let hub = EventHub::default();
        let _ = tokio::time::timeout(
            std::time::Duration::from_millis(300),
            drive_mount_channel(
                &mut reader,
                &mut mirror,
                generation,
                &hub,
                &mut router,
                &clipboard_tx,
                &out_tx,
                &outbound_clip_tx,
                None,
            ),
        )
        .await;

        let mut snapshot_requests = 0;
        while let Ok(msg) = out_rx.try_recv() {
            if matches!(msg, FederationMessage::SnapshotRequest(_)) {
                snapshot_requests += 1;
            }
        }
        assert_eq!(
            snapshot_requests, 1,
            "a burst of structural frames must coalesce into exactly one SnapshotRequest"
        );

        assert_eq!(mirror.panes().len(), 1, "the resync must add the new pane");
        assert!(mirror
            .panes()
            .values()
            .any(|pane| pane.terminal_id.ends_with(":term_new")));

        fake_server.await.unwrap();
    }

    // Root-cause fix regression (H3, plans/260721-2353-federation-agents-
    // sidebar-remote-detection): an inbound `AgentStatus` frame must both
    // update the mirror AND reach the pane's registered relayed-status sink
    // via `TerminalChannelRouter::route_agent_status`, keyed by the same
    // raw `terminal_id` `open_terminal` uses — not just get silently
    // dropped as it was before this fix.
    #[tokio::test]
    async fn drive_mount_channel_relays_agent_status_to_the_registered_pane_sink() {
        let (client_side, server_side) = tokio::io::duplex(1 << 16);
        let (client_reader, client_writer) = tokio::io::split(client_side);
        let (mut server_reader, mut server_writer) = tokio::io::split(server_side);

        let fake_server = tokio::spawn(async move {
            let Some(FederationMessage::Handshake(_)) =
                read_frame(&mut server_reader).await.unwrap()
            else {
                panic!("expected a Handshake");
            };
            write_frame(
                &mut server_writer,
                &FederationMessage::HandshakeResponse(HandshakeResponse::Accept {
                    agreed_capabilities: BTreeSet::new(),
                }),
            )
            .await
            .unwrap();

            let mut snapshot = crate::remote::federation::serve::empty_snapshot();
            snapshot.panes.push(crate::api::schema::panes::PaneInfo {
                pane_id: "pane_1".to_string(),
                terminal_id: "term_1".to_string(),
                workspace_id: "w1".to_string(),
                tab_id: "w1-tab".to_string(),
                focused: false,
                cwd: None,
                foreground_cwd: None,
                label: None,
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
            write_frame(
                &mut server_writer,
                &FederationMessage::MountSnapshot(MountSnapshot {
                    server_instance_id: ServerInstanceId("fake-server".to_string()),
                    snapshot,
                    cursor: crate::remote::federation::protocol::EventCursor(0),
                }),
            )
            .await
            .unwrap();

            write_frame(
                &mut server_writer,
                &FederationMessage::AgentStatus(
                    crate::remote::federation::protocol::AgentStatusMessage {
                        terminal_id: "term_1".to_string(),
                        mount_generation: 1,
                        status: AgentStatus::Working,
                        agent: Some("claude".to_string()),
                    },
                ),
            )
            .await
            .unwrap();

            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        });

        let client = FederationClient::new(host_key(), BTreeSet::new(), BTreeSet::new());
        let mounted = client
            .connect_and_mount(client_reader, client_writer)
            .await
            .unwrap();
        let generation = mounted.mirror.mount().mount_generation;
        let MountedConnection {
            mut mirror,
            mut reader,
            ..
        } = mounted;

        let mut router = TerminalChannelRouter::new();
        let (out_tx, _out_rx) = mpsc::unbounded_channel::<FederationMessage>();
        let _output_rx = router.open_terminal("term_1".to_string(), generation, &out_tx);
        let (status_tx, mut status_rx) = mpsc::channel::<RelayedAgentStatus>(8);
        router.register_agent_status_sender("term_1".to_string(), status_tx);

        let (clipboard_tx, _clipboard_rx) =
            mpsc::channel::<ClipboardMessage>(CLIPBOARD_CHANNEL_CAPACITY);
        let (outbound_clip_tx, _outbound_clip_rx) = mpsc::unbounded_channel::<ClipboardMessage>();
        let hub = EventHub::default();
        let _ = tokio::time::timeout(
            std::time::Duration::from_millis(300),
            drive_mount_channel(
                &mut reader,
                &mut mirror,
                generation,
                &hub,
                &mut router,
                &clipboard_tx,
                &out_tx,
                &outbound_clip_tx,
                None,
            ),
        )
        .await;

        let relayed = tokio::time::timeout(std::time::Duration::from_millis(200), status_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(relayed.status, AgentStatus::Working);
        assert_eq!(relayed.agent.as_deref(), Some("claude"));

        fake_server.await.unwrap();
    }

    // Remote-split materialization (260722 follow-up): a `SplitPaneResponse::
    // Created` arriving while `split_materialization` is `Some` must spawn a
    // real local `TerminalRuntime` (via `router.open_terminal` + `TerminalRuntime::
    // spawn_remote`, both already proven independently by `build_remote_pane`'s
    // own tests) and hand it back on `AppEvent::FederationSplitPaneReady`.
    #[tokio::test]
    async fn drive_mount_channel_materializes_a_runtime_on_split_pane_created() {
        let (client_side, server_side) = tokio::io::duplex(1 << 16);
        let (client_reader, client_writer) = tokio::io::split(client_side);
        let (mut server_reader, mut server_writer) = tokio::io::split(server_side);

        let fake_server = tokio::spawn(async move {
            let Some(FederationMessage::Handshake(_)) =
                read_frame(&mut server_reader).await.unwrap()
            else {
                panic!("expected a Handshake");
            };
            write_frame(
                &mut server_writer,
                &FederationMessage::HandshakeResponse(HandshakeResponse::Accept {
                    agreed_capabilities: BTreeSet::new(),
                }),
            )
            .await
            .unwrap();
            write_frame(
                &mut server_writer,
                &FederationMessage::MountSnapshot(MountSnapshot {
                    server_instance_id: ServerInstanceId("fake-server".to_string()),
                    snapshot: crate::remote::federation::serve::empty_snapshot(),
                    cursor: crate::remote::federation::protocol::EventCursor(0),
                }),
            )
            .await
            .unwrap();
            write_frame(
                &mut server_writer,
                &FederationMessage::SplitPaneResponse(
                    crate::remote::federation::protocol::SplitPaneResponse::Created {
                        request_id: 42,
                        new_pane_id: "pane_2".to_string(),
                        new_terminal_id: "term_2".to_string(),
                    },
                ),
            )
            .await
            .unwrap();

            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        });

        let client = FederationClient::new(host_key(), BTreeSet::new(), BTreeSet::new());
        let mounted = client
            .connect_and_mount(client_reader, client_writer)
            .await
            .unwrap();
        let generation = mounted.mirror.mount().mount_generation;
        let MountedConnection {
            mut mirror,
            mut reader,
            ..
        } = mounted;

        let mut router = TerminalChannelRouter::new();
        let (out_tx, _out_rx) = mpsc::unbounded_channel::<FederationMessage>();
        let (clipboard_tx, _clipboard_rx) =
            mpsc::channel::<ClipboardMessage>(CLIPBOARD_CHANNEL_CAPACITY);
        let (outbound_clip_tx, _outbound_clip_rx) = mpsc::unbounded_channel::<ClipboardMessage>();
        let (events_tx, mut events_rx) = mpsc::channel::<crate::events::AppEvent>(4);
        let ctx = SplitMaterializationContext {
            rows: 24,
            cols: 80,
            scrollback_limit_bytes: 1 << 16,
            host_terminal_theme: crate::terminal_theme::TerminalTheme::default(),
            origin: crate::remote::federation::id::HostKey::new("test-host", "s1"),
            events: events_tx,
            render_notify: std::sync::Arc::new(tokio::sync::Notify::new()),
            render_dirty: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        let hub = EventHub::default();
        let _ = tokio::time::timeout(
            std::time::Duration::from_millis(300),
            drive_mount_channel(
                &mut reader,
                &mut mirror,
                generation,
                &hub,
                &mut router,
                &clipboard_tx,
                &out_tx,
                &outbound_clip_tx,
                Some(&ctx),
            ),
        )
        .await;

        let ready = tokio::time::timeout(std::time::Duration::from_millis(200), events_rx.recv())
            .await
            .unwrap()
            .unwrap();
        match ready {
            crate::events::AppEvent::FederationSplitPaneReady(ready) => {
                assert_eq!(ready.request_id, 42);
            }
            other => panic!("expected FederationSplitPaneReady, got {other:?}"),
        }

        fake_server.await.unwrap();
    }

    // C1 regression (code review, plans/260722-1327/post-mount-pane-mirroring):
    // a locally-initiated split (`SplitPaneResponse::Created`) must register
    // its new pane in the mirror BEFORE a subsequent resync (triggered by the
    // server's own `PaneCreated` hub event for that same split) can ever see
    // it — otherwise `reconcile_by_diff` classifies it as newly created and
    // `App` would materialize a SECOND `TerminalRuntime`/pane for the same
    // remote terminal. Proves: pane count stays 1 in the mirror after the
    // resync, and no second `AppEvent` is emitted for the same pane.
    #[tokio::test]
    async fn a_split_created_pane_is_not_double_materialized_by_a_later_resync() {
        let (client_side, server_side) = tokio::io::duplex(1 << 16);
        let (client_reader, client_writer) = tokio::io::split(client_side);
        let (mut server_reader, mut server_writer) = tokio::io::split(server_side);

        let fake_server = tokio::spawn(async move {
            let Some(FederationMessage::Handshake(_)) =
                read_frame(&mut server_reader).await.unwrap()
            else {
                panic!("expected a Handshake");
            };
            write_frame(
                &mut server_writer,
                &FederationMessage::HandshakeResponse(HandshakeResponse::Accept {
                    agreed_capabilities: BTreeSet::new(),
                }),
            )
            .await
            .unwrap();
            write_frame(
                &mut server_writer,
                &FederationMessage::MountSnapshot(MountSnapshot {
                    server_instance_id: ServerInstanceId("fake-server".to_string()),
                    snapshot: crate::remote::federation::serve::empty_snapshot(),
                    cursor: crate::remote::federation::protocol::EventCursor(0),
                }),
            )
            .await
            .unwrap();

            // The split completes first...
            write_frame(
                &mut server_writer,
                &FederationMessage::SplitPaneResponse(
                    crate::remote::federation::protocol::SplitPaneResponse::Created {
                        request_id: 42,
                        new_pane_id: "pane_2".to_string(),
                        new_terminal_id: "term_2".to_string(),
                    },
                ),
            )
            .await
            .unwrap();

            // ...then the server's own `PaneCreated` hub event for that same
            // split arrives on the event channel (real-world ordering is not
            // guaranteed between the two channel classes), triggering a
            // resync.
            write_frame(
                &mut server_writer,
                &FederationMessage::Event(
                    crate::remote::federation::protocol::EventChannelMessage::Frame(
                        crate::remote::federation::protocol::EventFrame {
                            source_seq: 1,
                            kind: crate::api::schema::events::EventKind::PaneCreated,
                        },
                    ),
                ),
            )
            .await
            .unwrap();

            // The resync snapshot reports the SAME pane the split already
            // materialized.
            let mut snapshot = crate::remote::federation::serve::empty_snapshot();
            snapshot.panes.push(crate::api::schema::panes::PaneInfo {
                pane_id: "pane_2".to_string(),
                terminal_id: "term_2".to_string(),
                workspace_id: "w1".to_string(),
                tab_id: "w1-tab".to_string(),
                focused: false,
                cwd: None,
                foreground_cwd: None,
                label: None,
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
            write_frame(
                &mut server_writer,
                &FederationMessage::SnapshotResponse(MountSnapshot {
                    server_instance_id: ServerInstanceId("fake-server".to_string()),
                    snapshot,
                    cursor: crate::remote::federation::protocol::EventCursor(1),
                }),
            )
            .await
            .unwrap();

            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        });

        let this_host_key = crate::remote::federation::id::HostKey::new("test-host", "s1");
        let client = FederationClient::new(this_host_key.clone(), BTreeSet::new(), BTreeSet::new());
        let mounted = client
            .connect_and_mount(client_reader, client_writer)
            .await
            .unwrap();
        let generation = mounted.mirror.mount().mount_generation;
        let MountedConnection {
            mut mirror,
            mut reader,
            ..
        } = mounted;

        let mut router = TerminalChannelRouter::new();
        let (out_tx, _out_rx) = mpsc::unbounded_channel::<FederationMessage>();
        let (clipboard_tx, _clipboard_rx) =
            mpsc::channel::<ClipboardMessage>(CLIPBOARD_CHANNEL_CAPACITY);
        let (outbound_clip_tx, _outbound_clip_rx) = mpsc::unbounded_channel::<ClipboardMessage>();
        let (events_tx, mut events_rx) = mpsc::channel::<crate::events::AppEvent>(8);
        let ctx = SplitMaterializationContext {
            rows: 24,
            cols: 80,
            scrollback_limit_bytes: 1 << 16,
            host_terminal_theme: crate::terminal_theme::TerminalTheme::default(),
            origin: this_host_key.clone(),
            events: events_tx,
            render_notify: std::sync::Arc::new(tokio::sync::Notify::new()),
            render_dirty: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        let hub = EventHub::default();
        let _ = tokio::time::timeout(
            std::time::Duration::from_millis(300),
            drive_mount_channel(
                &mut reader,
                &mut mirror,
                generation,
                &hub,
                &mut router,
                &clipboard_tx,
                &out_tx,
                &outbound_clip_tx,
                Some(&ctx),
            ),
        )
        .await;

        // Exactly one materialization event for the split-created pane; the
        // resync must not produce a second one.
        let first = tokio::time::timeout(std::time::Duration::from_millis(200), events_rx.recv())
            .await
            .unwrap()
            .unwrap();
        match first {
            crate::events::AppEvent::FederationSplitPaneReady(ready) => {
                assert_eq!(ready.request_id, 42);
                assert_eq!(
                    ready.remote_pane_id,
                    format!("r:{}:pane_2", this_host_key.as_str())
                );
            }
            other => panic!("expected FederationSplitPaneReady, got {other:?}"),
        }
        let second =
            tokio::time::timeout(std::time::Duration::from_millis(150), events_rx.recv()).await;
        assert!(
            second.is_err() || second.unwrap().is_none(),
            "the resync must not double-materialize the split-created pane with a second event"
        );

        // The mirror must contain exactly one pane entry for the split
        // (registered at split time, not re-added as "created" by the diff).
        assert_eq!(
            mirror.panes().len(),
            1,
            "the resync must not add a duplicate mirror entry for the split-created pane"
        );

        fake_server.await.unwrap();
    }

    // Post-mount pane mirroring fix, part 2 (plans/260722-1327): a resync
    // diff (`SnapshotResponse` -> `reconcile_by_diff`) revealing a pane the
    // mirror never saw before must materialize a real local `TerminalRuntime`
    // for it (same shape `SplitPaneResponse::Created` above already proves)
    // and hand it back on `AppEvent::FederationResyncPaneCreated`, carrying
    // the workspace id a live `App` needs to splice it into the already-
    // mounted layout.
    #[tokio::test]
    async fn drive_mount_channel_materializes_a_runtime_on_resync_created_pane() {
        let (client_side, server_side) = tokio::io::duplex(1 << 16);
        let (client_reader, client_writer) = tokio::io::split(client_side);
        let (mut server_reader, mut server_writer) = tokio::io::split(server_side);

        let fake_server = tokio::spawn(async move {
            let Some(FederationMessage::Handshake(_)) =
                read_frame(&mut server_reader).await.unwrap()
            else {
                panic!("expected a Handshake");
            };
            write_frame(
                &mut server_writer,
                &FederationMessage::HandshakeResponse(HandshakeResponse::Accept {
                    agreed_capabilities: BTreeSet::new(),
                }),
            )
            .await
            .unwrap();
            write_frame(
                &mut server_writer,
                &FederationMessage::MountSnapshot(MountSnapshot {
                    server_instance_id: ServerInstanceId("fake-server".to_string()),
                    snapshot: crate::remote::federation::serve::empty_snapshot(),
                    cursor: crate::remote::federation::protocol::EventCursor(0),
                }),
            )
            .await
            .unwrap();

            write_frame(
                &mut server_writer,
                &FederationMessage::Event(
                    crate::remote::federation::protocol::EventChannelMessage::Frame(
                        crate::remote::federation::protocol::EventFrame {
                            source_seq: 1,
                            kind: crate::api::schema::events::EventKind::PaneCreated,
                        },
                    ),
                ),
            )
            .await
            .unwrap();

            let mut snapshot = crate::remote::federation::serve::empty_snapshot();
            snapshot.panes.push(crate::api::schema::panes::PaneInfo {
                pane_id: "w1:p2".to_string(),
                terminal_id: "term_new".to_string(),
                workspace_id: "w1".to_string(),
                tab_id: "w1-tab".to_string(),
                focused: false,
                cwd: None,
                foreground_cwd: None,
                label: None,
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
            write_frame(
                &mut server_writer,
                &FederationMessage::SnapshotResponse(MountSnapshot {
                    server_instance_id: ServerInstanceId("fake-server".to_string()),
                    snapshot,
                    cursor: crate::remote::federation::protocol::EventCursor(1),
                }),
            )
            .await
            .unwrap();

            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        });

        let this_host_key = crate::remote::federation::id::HostKey::new("test-host", "s1");
        let client = FederationClient::new(this_host_key.clone(), BTreeSet::new(), BTreeSet::new());
        let mounted = client
            .connect_and_mount(client_reader, client_writer)
            .await
            .unwrap();
        let generation = mounted.mirror.mount().mount_generation;
        let MountedConnection {
            mut mirror,
            mut reader,
            ..
        } = mounted;

        let mut router = TerminalChannelRouter::new();
        let (out_tx, _out_rx) = mpsc::unbounded_channel::<FederationMessage>();
        let (clipboard_tx, _clipboard_rx) =
            mpsc::channel::<ClipboardMessage>(CLIPBOARD_CHANNEL_CAPACITY);
        let (outbound_clip_tx, _outbound_clip_rx) = mpsc::unbounded_channel::<ClipboardMessage>();
        let (events_tx, mut events_rx) = mpsc::channel::<crate::events::AppEvent>(4);
        let ctx = SplitMaterializationContext {
            rows: 24,
            cols: 80,
            scrollback_limit_bytes: 1 << 16,
            host_terminal_theme: crate::terminal_theme::TerminalTheme::default(),
            origin: this_host_key.clone(),
            events: events_tx,
            render_notify: std::sync::Arc::new(tokio::sync::Notify::new()),
            render_dirty: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        let hub = EventHub::default();
        let _ = tokio::time::timeout(
            std::time::Duration::from_millis(300),
            drive_mount_channel(
                &mut reader,
                &mut mirror,
                generation,
                &hub,
                &mut router,
                &clipboard_tx,
                &out_tx,
                &outbound_clip_tx,
                Some(&ctx),
            ),
        )
        .await;

        let ready = tokio::time::timeout(std::time::Duration::from_millis(200), events_rx.recv())
            .await
            .unwrap()
            .unwrap();
        match ready {
            crate::events::AppEvent::FederationResyncPaneCreated(ready) => {
                assert_eq!(ready.origin, this_host_key);
                assert_eq!(
                    ready.workspace_id,
                    format!("r:{}:w1", this_host_key.as_str())
                );
                assert_eq!(ready.pane_id, format!("r:{}:w1:p2", this_host_key.as_str()));
            }
            other => panic!("expected FederationResyncPaneCreated, got {other:?}"),
        }

        fake_server.await.unwrap();
    }

    // Post-mount pane mirroring fix, part 2 (plans/260722-1327): a resync
    // diff revealing a pane the mirror previously mirrored but the remote no
    // longer reports must surface `AppEvent::FederationResyncPaneRemoved`
    // (namespaced pane id + this mount's origin) so `App` can tear down the
    // matching local runtime/layout entry.
    #[tokio::test]
    async fn drive_mount_channel_emits_resync_pane_removed_for_a_pane_dropped_from_the_snapshot() {
        let (client_side, server_side) = tokio::io::duplex(1 << 16);
        let (client_reader, client_writer) = tokio::io::split(client_side);
        let (mut server_reader, mut server_writer) = tokio::io::split(server_side);

        let fake_server = tokio::spawn(async move {
            let Some(FederationMessage::Handshake(_)) =
                read_frame(&mut server_reader).await.unwrap()
            else {
                panic!("expected a Handshake");
            };
            write_frame(
                &mut server_writer,
                &FederationMessage::HandshakeResponse(HandshakeResponse::Accept {
                    agreed_capabilities: BTreeSet::new(),
                }),
            )
            .await
            .unwrap();

            let mut mount_snapshot = crate::remote::federation::serve::empty_snapshot();
            mount_snapshot
                .panes
                .push(crate::api::schema::panes::PaneInfo {
                    pane_id: "w1:p2".to_string(),
                    terminal_id: "term_old".to_string(),
                    workspace_id: "w1".to_string(),
                    tab_id: "w1-tab".to_string(),
                    focused: false,
                    cwd: None,
                    foreground_cwd: None,
                    label: None,
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
            write_frame(
                &mut server_writer,
                &FederationMessage::MountSnapshot(MountSnapshot {
                    server_instance_id: ServerInstanceId("fake-server".to_string()),
                    snapshot: mount_snapshot,
                    cursor: crate::remote::federation::protocol::EventCursor(0),
                }),
            )
            .await
            .unwrap();

            write_frame(
                &mut server_writer,
                &FederationMessage::Event(
                    crate::remote::federation::protocol::EventChannelMessage::Frame(
                        crate::remote::federation::protocol::EventFrame {
                            source_seq: 1,
                            kind: crate::api::schema::events::EventKind::PaneClosed,
                        },
                    ),
                ),
            )
            .await
            .unwrap();

            // The pane mounted above is gone from this resync snapshot.
            let empty = crate::remote::federation::serve::empty_snapshot();
            write_frame(
                &mut server_writer,
                &FederationMessage::SnapshotResponse(MountSnapshot {
                    server_instance_id: ServerInstanceId("fake-server".to_string()),
                    snapshot: empty,
                    cursor: crate::remote::federation::protocol::EventCursor(1),
                }),
            )
            .await
            .unwrap();

            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        });

        let this_host_key = crate::remote::federation::id::HostKey::new("test-host", "s1");
        let client = FederationClient::new(this_host_key.clone(), BTreeSet::new(), BTreeSet::new());
        let mounted = client
            .connect_and_mount(client_reader, client_writer)
            .await
            .unwrap();
        let generation = mounted.mirror.mount().mount_generation;
        let MountedConnection {
            mut mirror,
            mut reader,
            ..
        } = mounted;

        let mut router = TerminalChannelRouter::new();
        let (out_tx, _out_rx) = mpsc::unbounded_channel::<FederationMessage>();
        let (clipboard_tx, _clipboard_rx) =
            mpsc::channel::<ClipboardMessage>(CLIPBOARD_CHANNEL_CAPACITY);
        let (outbound_clip_tx, _outbound_clip_rx) = mpsc::unbounded_channel::<ClipboardMessage>();
        let (events_tx, mut events_rx) = mpsc::channel::<crate::events::AppEvent>(4);
        let ctx = SplitMaterializationContext {
            rows: 24,
            cols: 80,
            scrollback_limit_bytes: 1 << 16,
            host_terminal_theme: crate::terminal_theme::TerminalTheme::default(),
            origin: this_host_key.clone(),
            events: events_tx,
            render_notify: std::sync::Arc::new(tokio::sync::Notify::new()),
            render_dirty: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        let hub = EventHub::default();
        let _ = tokio::time::timeout(
            std::time::Duration::from_millis(300),
            drive_mount_channel(
                &mut reader,
                &mut mirror,
                generation,
                &hub,
                &mut router,
                &clipboard_tx,
                &out_tx,
                &outbound_clip_tx,
                Some(&ctx),
            ),
        )
        .await;

        let ready = tokio::time::timeout(std::time::Duration::from_millis(200), events_rx.recv())
            .await
            .unwrap()
            .unwrap();
        match ready {
            crate::events::AppEvent::FederationResyncPaneRemoved { origin, pane_id } => {
                assert_eq!(origin, this_host_key);
                assert_eq!(pane_id, format!("r:{}:w1:p2", this_host_key.as_str()));
            }
            other => panic!("expected FederationResyncPaneRemoved, got {other:?}"),
        }

        fake_server.await.unwrap();
    }

    // Phase 07 test 2 (S11.2 Blocker): an over-cap `Clipboard` frame placed
    // directly on the wire (bypassing every higher-level API this module
    // exposes) is rejected by `read_frame` — the ONE choke point every
    // federation call site in this file uses (see the module's `use super::
    // serve::{read_frame, write_frame}` at the top) — with the exact same
    // typed rejection `codec::decode` produces when called directly. A
    // hand-rolled second decode path that forgot the cap would diverge from
    // this assertion.
    #[tokio::test]
    async fn oversized_clipboard_frame_is_rejected_by_the_shared_codec_not_a_bypass() {
        use crate::remote::federation::protocol::codec;
        use crate::remote::federation::protocol::Channel;

        let oversized = FederationMessage::Clipboard(ClipboardMessage {
            origin_tag: "remote".to_string(),
            payload: vec![0u8; Channel::Clipboard.max_len() + 1],
        });

        let frame = codec::encode(&oversized).expect("encode has no cap (read-side check only)");
        let direct_err = codec::decode::<FederationMessage>(&frame, Channel::Clipboard.max_len())
            .expect_err("direct codec::decode must reject an over-cap clipboard payload");

        // `serde_json` encodes a raw `Vec<u8>` payload as a numeric array
        // (e.g. `[0,0,...]`), which bloats a 16 MiB+1 byte payload to roughly
        // 2x its raw size on the wire (see `implementation-notes.md`'s P1
        // JSON-bloat follow-up). A duplex buffer sized off the *raw* payload
        // length is therefore smaller than the actual encoded frame, and
        // `write_frame`'s `write_all` would block forever waiting for buffer
        // space with no concurrent reader draining it (deadlock — this is
        // exactly what hung the suite). Sizing the buffer off the already-
        // encoded `frame`'s own length sidesteps the encoding-format detail
        // entirely and keeps this test's single-task, no-concurrent-reader
        // shape (it only needs the header-only rejection, never the full
        // payload consumed).
        let (client_side, server_side) = tokio::io::duplex(frame.len() + 1_048_576);
        let (mut client_reader, _client_writer) = tokio::io::split(client_side);
        let (_server_reader, mut server_writer) = tokio::io::split(server_side);
        write_frame(&mut server_writer, &oversized)
            .await
            .expect("the cap is enforced on read, not write");

        let wire_err = read_frame(&mut client_reader)
            .await
            .expect_err("read_frame must reject the same over-cap frame the wire delivered");

        assert!(
            wire_err.to_string().contains("exceeds"),
            "read_frame rejection: {wire_err}"
        );
        assert!(
            direct_err.to_string().contains("exceeds"),
            "direct codec::decode rejection: {direct_err}"
        );
    }

    // Phase 07 test 5 (S2.2 Bounded ingestion): a remote flooding the
    // `Clipboard` channel faster than the local consumer drains it can never
    // grow the ingestion queue past its fixed per-mount budget — `try_send`
    // degrades by dropping the newest overflow message, never blocking the
    // shared mount read loop and never allocating unbounded memory.
    #[test]
    fn flooding_the_clipboard_channel_never_exceeds_its_bounded_budget() {
        let (clipboard_tx, mut clipboard_rx) =
            mpsc::channel::<ClipboardMessage>(CLIPBOARD_CHANNEL_CAPACITY);

        // Flood well past capacity; every send after the channel fills must
        // fail fast (never block) rather than growing the queue.
        for i in 0..(CLIPBOARD_CHANNEL_CAPACITY * 10) {
            let _ = clipboard_tx.try_send(ClipboardMessage {
                origin_tag: "remote".to_string(),
                payload: vec![i as u8],
            });
        }

        // The queue never holds more than its configured capacity.
        let mut drained = 0usize;
        while clipboard_rx.try_recv().is_ok() {
            drained += 1;
        }
        assert!(
            drained <= CLIPBOARD_CHANNEL_CAPACITY,
            "drained {drained} messages, expected at most the configured budget of {CLIPBOARD_CHANNEL_CAPACITY}"
        );
        assert!(
            drained > 0,
            "at least the first burst up to capacity must have been queued"
        );
    }

    #[test]
    fn drive_outcome_ended_reason_link_closed_faulted_err_return_some() {
        assert!(drive_outcome_ended_reason(&Ok(DriveOutcome::LinkClosed)).is_some());
        assert!(
            drive_outcome_ended_reason(&Ok(DriveOutcome::Faulted(FaultReason::PeerClosed)))
                .is_some()
        );
        assert!(drive_outcome_ended_reason(&Err(std::io::Error::other("boom"))).is_some());
    }

    #[test]
    fn drive_outcome_ended_reason_resync_required_returns_none() {
        assert_eq!(
            drive_outcome_ended_reason(&Ok(DriveOutcome::ResyncRequired)),
            None
        );
    }
}
