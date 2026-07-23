//! Co-located federation listener: accept loop + per-connection handshake
//! (P9.2b b0.4 sub-brick 2, decision B — sync thread topology).
//!
//! The keystone crux (async serve loop vs. the sync `interprocess` stream the
//! accept path yields) is resolved in favour of a **sync thread topology**:
//! each accepted federation connection is driven on its own `std::thread` with
//! blocking, sync-framed I/O, exactly as the classic thin-client transport
//! already does (`client_transport::handle_client_handshake` →
//! `try_clone` → writer thread + read loop). The federation codec
//! (`protocol::codec`) is pure — it frames in-memory byte slices with no
//! `Read`/`Write` coupling — so the same frames the async `serve::run` path
//! writes are produced here over a blocking `Read`/`Write`. No per-connection
//! tokio runtime, no async bridge over the local socket.
//!
//! A mounted connection is driven bidirectionally (sub-brick 2c): after the
//! handshake and mount snapshot, the accepting thread reads inbound controller
//! commands while a writer thread — fed by an event/agent ticker and one output
//! pump per opened terminal — serialises every outbound frame. This is the sync
//! analogue of `serve::run`'s async `select!` loop; the output pumps poll with
//! `tee::drain_available` because this tokio's `broadcast::Receiver` offers no
//! blocking receive.
//!
//! Unix-only: gated at the module declaration (`server::mod`), mirroring the
//! federation socket fields it accepts on.

use std::collections::{BTreeSet, HashMap};
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use bytes::Bytes;
use interprocess::local_socket::traits::{Listener as _, Stream as _};
use interprocess::TryClone as _;
use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::{debug, error, warn};

use crate::api::schema::common::AgentStatus;
use crate::api::schema::events::EventKind;
use crate::api::schema::session::SessionSnapshot;
use crate::ipc::{LocalListener, LocalStream};
use crate::remote::federation::id::ServerInstanceId;
use crate::remote::federation::protocol::codec;
use crate::remote::federation::protocol::negotiate::{negotiate, AgreedCaps};
use crate::remote::federation::protocol::{
    AgentStatusMessage, Capability, Channel, ClipboardStageFailure, ClipboardStageRequest,
    ClipboardStageResponse, EventChannelMessage, EventCursor, EventFrame, FaultMessage,
    FederationMessage, Handshake, HandshakeResponse, MountSnapshot, ScrollbackReplay,
    SplitPaneRequest, SplitPaneResponse, TerminalChannelMessage, FEDERATION_PROTOCOL_VERSION,
};
use crate::remote::federation::tee;
use crate::server::client_transport::ServerEvent;
use crate::server::federation_actor::FederationCommand;
use crate::server::federation_fault::{FirstCauseCell, TunnelExit};
use crate::server::federation_lease::{AcceptEpoch, Admission, ConnId};

/// How long a federation connection has to complete its handshake before the
/// server drops it, mirroring the thin-client path's `HANDSHAKE_TIMEOUT`. Keeps
/// a silent peer from pinning a handshake thread indefinitely.
const FEDERATION_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(4);

/// The mount generation stamped on every outbound terminal/agent frame. A fixed
/// constant in v1 — remounts within one session (the generation-fencing story)
/// are P9.3 scope; mirrors `serve::MOUNT_GENERATION`.
const MOUNT_GENERATION: u64 = 1;

/// Poll cadence for the outbound side (output pumps + the event/agent ticker).
/// This tokio's `broadcast::Receiver` has no blocking receive, so the output
/// pump coalesces with `tee::drain_available` on this interval rather than
/// waking per byte; mirrors `serve::POLL_INTERVAL` (25 ms).
const OUTBOUND_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Agent-status polling runs once every Nth event tick (~100 ms), matching
/// `serve.rs`'s `POLL_INTERVAL * 4` agent cadence — status changes are far
/// coarser than terminal output, so they need no per-tick poll.
const AGENT_STATUS_POLL_DIVISOR: u32 = 4;

/// Depth of the bounded outbound queue. A peer that stops reading cannot make
/// the server buffer without limit: once this many frames are undrained, the
/// next enqueue fails fast as an [`TunnelExit::EgressOverflow`] and tears the
/// connection down (v1 fail-fast — no reopen protocol). Sized for a healthy
/// burst (scrollback replay + a few ticks of coalesced output) without letting
/// a stuck peer pin unbounded memory.
const EGRESS_QUEUE_CAP: usize = 1024;

/// The capabilities the co-located federation host advertises. Mirrors
/// `AppFederationHost::capabilities` (`remote::federation::serve`); becomes the
/// sole definition once sub-brick 4 deletes that duplicate host.
fn federation_capabilities() -> BTreeSet<Capability> {
    [
        Capability::new(Capability::SCROLLBACK_REPLAY),
        Capability::new(Capability::AGENT_STATUS),
        // This module is Unix-only (`server::mod` gates its declaration), and
        // so is the staging module behind this capability, so the two can
        // never disagree about which targets can honour a stage request.
        Capability::new(Capability::FILE_STAGING),
    ]
    .into_iter()
    .collect()
}

/// The largest cap across every channel, bounding a read-side allocation before
/// a frame's true channel (and cap) is known — mirrors `serve::global_max_frame`.
/// Derived from the channel list rather than naming one channel: hardcoding a
/// particular channel's cap here silently rejects every frame on any channel
/// whose cap is larger.
fn global_max_frame() -> usize {
    Channel::largest_max_len()
}

/// Blocking sync read of one framed federation message; `Ok(None)` on a clean
/// EOF. The sync counterpart of `serve::read_frame`, used because the co-located
/// connection owns its own `std::thread` (decision B). The same two-stage cap
/// check the async path uses is preserved: bound by the global max before decode,
/// then re-check against the decoded message's own channel cap.
fn read_frame_blocking(reader: &mut impl Read) -> io::Result<Option<FederationMessage>> {
    let mut header = [0u8; 8];
    match reader.read_exact(&mut header) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(err) => return Err(err),
    }

    let claimed_len = u32::from_le_bytes(
        header[4..8]
            .try_into()
            .map_err(|_| io::Error::other("truncated federation frame length field"))?,
    ) as usize;
    let max = global_max_frame();
    if claimed_len > max {
        return Err(io::Error::other(format!(
            "federation frame size {claimed_len} exceeds the largest channel cap {max}"
        )));
    }

    let mut frame = vec![0u8; 8 + claimed_len];
    frame[..8].copy_from_slice(&header);
    reader.read_exact(&mut frame[8..])?;

    let (msg, _consumed) = codec::decode::<FederationMessage>(&frame, max)
        .map_err(|err| io::Error::other(err.to_string()))?;

    if claimed_len > msg.channel().max_len() {
        return Err(io::Error::other(format!(
            "federation frame size {claimed_len} exceeds its channel's cap {}",
            msg.channel().max_len()
        )));
    }

    Ok(Some(msg))
}

/// Blocking sync write of one framed federation message; the sync counterpart of
/// `serve::write_frame`.
fn write_frame_blocking(writer: &mut impl Write, msg: &FederationMessage) -> io::Result<()> {
    let frame = codec::encode(msg).map_err(|err| io::Error::other(err.to_string()))?;
    writer.write_all(&frame)?;
    writer.flush()
}

/// Accept pending co-located federation connections and drive each one on its
/// own `std::thread`. Mirrors `accept_pending_client_connections`; called each
/// server tick. `epoch` is the lease's current accept-epoch, read
/// synchronously by the caller on the event loop and stamped onto every command
/// this connection later enqueues — a connection accepted before a handoff
/// revocation carries the pre-revocation epoch, so its queued commands fail
/// validation and cannot resurrect authority (federation_lease v5 finding #1).
pub(crate) fn accept_pending_federation_connections(
    listener: &LocalListener,
    next_id: &mut u64,
    epoch: AcceptEpoch,
    server_instance_id: &ServerInstanceId,
    server_event_tx: &mpsc::Sender<ServerEvent>,
) -> io::Result<()> {
    loop {
        match listener.accept() {
            Ok(stream) => {
                let connid = *next_id;
                *next_id = next_id.saturating_add(1);

                if let Err(err) = stream.set_nonblocking(true) {
                    warn!(connid, err = %err, "failed to set federation stream nonblocking");
                    continue;
                }

                let server_instance_id = server_instance_id.clone();
                let server_event_tx = server_event_tx.clone();
                std::thread::spawn(move || {
                    if let Err(err) = handle_federation_connection(
                        stream,
                        epoch,
                        connid,
                        &server_instance_id,
                        server_event_tx,
                    ) {
                        debug!(connid, err = %err, "federation connection failed");
                    }
                });
            }
            Err(ref err) if err.kind() == io::ErrorKind::WouldBlock => break,
            Err(err) => {
                error!(err = %err, "federation listener accept failed");
                break;
            }
        }
    }

    Ok(())
}

/// Drain pending federation connections without handshaking — used during live
/// handoff so a peer does not sit in the backlog awaiting a mount the draining
/// server will never send. Mirrors `reject_pending_client_connections`.
pub(crate) fn reject_pending_federation_connections(listener: &LocalListener) -> io::Result<()> {
    loop {
        match listener.accept() {
            Ok(_stream) => {}
            Err(ref err) if err.kind() == io::ErrorKind::WouldBlock => break,
            Err(err) => {
                error!(err = %err, "federation listener reject failed");
                break;
            }
        }
    }

    Ok(())
}

/// Drive one federation connection: handshake, then (on acceptance) acquire the
/// single-controller lease and mount, streaming the initial snapshot. Sets
/// blocking I/O with a bounded receive timeout for the handshake, then delegates
/// the wire exchange. Sub-brick 2c-1 closes the connection after the mount
/// snapshot; sub-brick 2c-2 layers the command loop on top.
fn handle_federation_connection(
    mut stream: LocalStream,
    epoch: AcceptEpoch,
    connid: ConnId,
    server_instance_id: &ServerInstanceId,
    server_event_tx: mpsc::Sender<ServerEvent>,
) -> io::Result<()> {
    // The accept loop set the stream nonblocking; the handshake needs blocking
    // I/O, bounded by a timeout so a silent peer cannot pin this thread.
    stream.set_nonblocking(false)?;
    if let Err(err) = stream.set_recv_timeout(Some(FEDERATION_HANDSHAKE_TIMEOUT)) {
        if err.kind() != io::ErrorKind::Unsupported {
            return Err(err);
        }
        debug!(connid, err = %err, "federation socket receive timeout unavailable");
    }

    // The negotiated set is carried, not discarded: it is what decides
    // whether this connection may construct a stage frame at all. A peer that
    // did not negotiate `file_staging` has no decoder for either stage
    // variant, and one such frame ends its whole mount.
    let Some(agreed) = drive_handshake(&mut stream, connid, server_instance_id)? else {
        debug!(connid, "federation handshake rejected or absent");
        return Ok(());
    };

    // Clear the handshake read timeout: a mounted federated session idles
    // between the controller's inputs, so the command loop must not time out.
    if let Err(err) = stream.set_recv_timeout(None) {
        if err.kind() != io::ErrorKind::Unsupported {
            return Err(err);
        }
        debug!(connid, err = %err, "clearing federation recv timeout unsupported");
    }

    drive_mount(
        &mut stream,
        epoch,
        connid,
        &agreed,
        server_instance_id,
        &server_event_tx,
    )
}

/// Acquire the single-controller lease, mount, and stream the initial
/// `MountSnapshot` to the peer. A connection that loses the admission race (or
/// whose reservation is superseded before it mounts) simply closes. Once the
/// slot is reserved, a [`LeaseReleaseGuard`] guarantees the lease is released on
/// every return or panic. Sub-brick 2c-1 stops after the snapshot; 2c-2
/// continues into the command loop where the `_` binding below lives.
fn drive_mount<S: FederationStream>(
    stream: &mut S,
    epoch: AcceptEpoch,
    connid: ConnId,
    agreed: &AgreedCaps,
    server_instance_id: &ServerInstanceId,
    server_event_tx: &mpsc::Sender<ServerEvent>,
) -> io::Result<()> {
    match acquire_controller(epoch, connid, server_event_tx)? {
        Admission::Accepted => {}
        other => {
            debug!(connid, ?other, "federation admission refused");
            return Ok(());
        }
    }

    // The slot is now reserved for (epoch, connid); guarantee its release on
    // every exit from here — including a panic — so a connection that dies
    // before (or without) a handoff cannot pin the single-controller slot.
    let _release = LeaseReleaseGuard::new(epoch, connid, server_event_tx.clone());

    let Some((snapshot, cursor)) = mount(epoch, connid, server_event_tx)? else {
        debug!(connid, "federation mount refused (reservation superseded)");
        return Ok(());
    };

    write_frame_blocking(
        stream,
        &FederationMessage::MountSnapshot(MountSnapshot {
            server_instance_id: server_instance_id.clone(),
            snapshot,
            // `EventCursor` is `Copy`; the ticker resumes event streaming from
            // exactly the snapshot's cursor so no event is duplicated or skipped.
            cursor,
        }),
    )?;

    // The snapshot is fully flushed before any outbound machinery starts, so it
    // is always the first frame after the handshake. Now run the connection
    // bidirectionally: inbound commands on this thread, outbound frames on a
    // writer thread + ticker + per-terminal output pumps. The guard (above)
    // releases the lease when this returns.
    run_connection(
        stream,
        epoch,
        connid,
        cursor,
        agreed,
        Arc::new(crate::remote::federation::file_staging::stage_remote_clipboard_image),
        server_instance_id,
        server_event_tx,
    )
}

/// A federation connection stream: blocking `Read + Write` plus a `try_clone`
/// into an independent handle, so the reader (this thread) and the writer thread
/// each own one. Implemented for the production `LocalStream` and, under test,
/// for `UnixStream`, so the whole bidirectional driver is exercised over a
/// socket pair — the same reader/writer split the thin-client transport uses.
trait FederationStream: Read + Write + Send + 'static {
    fn try_clone_stream(&self) -> io::Result<Self>
    where
        Self: Sized;
}

impl FederationStream for LocalStream {
    fn try_clone_stream(&self) -> io::Result<Self> {
        // `interprocess::TryClone`, in scope as `_` — resolves via method syntax.
        self.try_clone()
    }
}

/// Run a mounted federation connection bidirectionally until it ends. Inbound
/// controller commands are serviced on THIS thread; outbound frames go through a
/// single writer thread fed by an event/agent ticker and one output pump per
/// opened terminal. All outbound producers funnel through one mpsc so writes
/// never race on the shared socket. Returns when the peer closes, a read fails,
/// or the writer fails — whichever comes first (recorded in `first_cause`).
#[allow(clippy::too_many_arguments)]
fn run_connection<S: FederationStream>(
    stream: &mut S,
    epoch: AcceptEpoch,
    connid: ConnId,
    initial_cursor: EventCursor,
    agreed: &AgreedCaps,
    staging_op: StagingOp,
    server_instance_id: &ServerInstanceId,
    server_event_tx: &mpsc::Sender<ServerEvent>,
) -> io::Result<()> {
    let shutdown = Arc::new(AtomicBool::new(false));
    let first_cause = Arc::new(FirstCauseCell::new());

    // The writer thread is the connection's single serializer: every outbound
    // frame is funnelled through `out_tx` so two producers never interleave a
    // write on the shared socket. It drains until all senders drop (clean
    // teardown) or a write fails (writer-initiated teardown).
    let (out_tx, out_rx) = std_mpsc::sync_channel::<FederationMessage>(EGRESS_QUEUE_CAP);
    let write_half = stream.try_clone_stream()?;
    let writer = {
        let first_cause = Arc::clone(&first_cause);
        let shutdown = Arc::clone(&shutdown);
        std::thread::spawn(move || writer_loop(write_half, out_rx, &first_cause, &shutdown))
    };

    // The ticker polls the live App for new events + agent-status changes and
    // pushes frames to the writer, resuming events exactly at the snapshot's
    // cursor so nothing is duplicated or skipped.
    let ticker = {
        let out_tx = out_tx.clone();
        let server_event_tx = server_event_tx.clone();
        let shutdown = Arc::clone(&shutdown);
        let first_cause = Arc::clone(&first_cause);
        std::thread::spawn(move || {
            ticker_loop(
                &out_tx,
                &server_event_tx,
                initial_cursor,
                &shutdown,
                &first_cause,
            )
        })
    };

    // The staging worker exists ONLY when both peers negotiated file staging.
    // Making its absence the reason no stage frame can be built is what keeps
    // "a newer host never answers an older controller" structural: with no
    // worker there is no response path to reach.
    let staging = if agreed
        .0
        .contains(&Capability::new(Capability::FILE_STAGING))
    {
        let (channel, out) = spawn_staging_worker(&shutdown, &first_cause, staging_op);
        match out.lock() {
            Ok(mut handle) => *handle = Some(out_tx.clone()),
            Err(_) => debug!(
                connid,
                "federation staging outbound handle poisoned at spawn"
            ),
        }
        Some((channel, out))
    } else {
        None
    };

    // Inbound loop on THIS thread; it spawns an output pump per opened terminal.
    let mut pumps: HashMap<String, OutputPump> = HashMap::new();
    let read_result = reader_loop(
        stream,
        epoch,
        connid,
        server_instance_id,
        &out_tx,
        &shutdown,
        &first_cause,
        server_event_tx,
        &mut pumps,
        staging.as_ref().map(|(channel, _out)| channel),
    );

    // Teardown in an order that cannot deadlock the writer:
    //   1. signal every subordinate to stop,
    //   2. stop + join the output pumps and the ticker — they drop their
    //      `out_tx` clones as they exit,
    //   3. drop OUR `out_tx` so the writer's receiver has no senders left,
    //   4. join the writer (its `recv()` now returns `Err` and it exits).
    // Dropping `out_tx` BEFORE joining the writer is load-bearing: hold it and
    // the writer's `recv()` never disconnects, hanging the join.
    shutdown.store(true, Ordering::SeqCst);
    for (_id, pump) in pumps.drain() {
        pump.stop.store(true, Ordering::SeqCst);
        let _ = pump.handle.join();
    }
    let _ = ticker.join();
    // The staging worker is torn down without being joined: dropping its queue
    // only wakes a worker parked in `recv()`, and a join would hold the lease
    // (released by `LeaseReleaseGuard` when this returns) for as long as one
    // filesystem call is stuck, refusing every later mount to this host.
    // Revoking the outbound handle drops the worker's only sender right here,
    // so `drop(out_tx)` below still disconnects the writer and its join stays
    // bounded; a worker that finishes late discards its result.
    if let Some((channel, staging_out)) = staging {
        drop(channel);
        match staging_out.lock() {
            Ok(mut handle) => *handle = None,
            Err(_) => debug!(
                connid,
                "federation staging outbound handle poisoned at teardown"
            ),
        }
    }
    // If a fault (not a clean peer close) ended the connection, best-effort tell
    // the peer why before the socket closes. It may not arrive if the link is
    // already broken (e.g. a WriterFailed cause), hence best-effort; enqueued
    // after the ticker/pumps are joined so nothing races it, before out_tx drops.
    if let Some(cause) = first_cause.get() {
        if !cause.is_clean() {
            let _ = out_tx.try_send(FederationMessage::Fault(FaultMessage {
                reason: cause.to_wire(),
            }));
        }
    }
    drop(out_tx);
    let _ = writer.join();

    if let Some(cause) = first_cause.get() {
        debug!(connid, ?cause, "federation connection torn down");
    }
    read_result
}

/// Read inbound frames from the mounted controller until EOF or a read error.
/// Input/resize route to the live App via the actor (fire-and-forget; the actor
/// drops them unless `(epoch, connid)` is the mounted controller). Open/Close
/// manage the per-terminal output pumps on the outbound side.
#[allow(clippy::too_many_arguments)]
fn reader_loop<S: Read>(
    reader: &mut S,
    epoch: AcceptEpoch,
    connid: ConnId,
    server_instance_id: &ServerInstanceId,
    out_tx: &std_mpsc::SyncSender<FederationMessage>,
    shutdown: &Arc<AtomicBool>,
    first_cause: &Arc<FirstCauseCell>,
    server_event_tx: &mpsc::Sender<ServerEvent>,
    pumps: &mut HashMap<String, OutputPump>,
    staging: Option<&StagingChannel>,
) -> io::Result<()> {
    loop {
        match read_frame_blocking(reader) {
            Ok(None) => {
                first_cause.set(TunnelExit::PeerClosed);
                return Ok(());
            }
            Ok(Some(FederationMessage::Terminal(message))) => {
                handle_terminal_inbound(
                    message,
                    epoch,
                    connid,
                    out_tx,
                    shutdown,
                    first_cause,
                    server_event_tx,
                    pumps,
                )?;
            }
            Ok(Some(FederationMessage::Fault(fault))) => {
                // The peer is tearing down and told us why; adopt it as the
                // first cause (if none is set yet) and end the connection.
                first_cause.set(TunnelExit::from_wire(fault.reason));
                return Ok(());
            }
            Ok(Some(FederationMessage::SplitPaneRequest(request))) => {
                handle_split_pane_request(request, out_tx, shutdown, first_cause, server_event_tx);
            }
            Ok(Some(FederationMessage::ClipboardStageRequest(request))) => {
                handle_clipboard_stage_request(request, staging, out_tx, shutdown, first_cause);
            }
            Ok(Some(FederationMessage::SnapshotRequest(_))) => {
                handle_snapshot_request(
                    server_instance_id,
                    out_tx,
                    shutdown,
                    first_cause,
                    server_event_tx,
                );
            }
            Ok(Some(_other)) => {
                // The controller drives only the terminal channel inbound;
                // other inbound frames are ignored, not treated as fatal.
            }
            Err(err) => {
                // A read error means the peer link is gone; record it as the
                // first cause (no reconnect story yet — P9.3) and tear down.
                first_cause.set(TunnelExit::PeerClosed);
                return Err(err);
            }
        }
    }
}

/// Services one inbound `SplitPaneRequest` against the live `App`: a
/// blocking round-trip through `FederationCommand::SplitPane` (legal here —
/// this reader runs on its own `std::thread`, never a tokio worker), replying
/// with `SplitPaneResponse::Created`/`Failed` on the shared outbound queue.
/// A dropped/gone actor (server shutting down) replies `Failed` rather than
/// silently dropping the peer's request.
fn handle_split_pane_request(
    request: SplitPaneRequest,
    out_tx: &std_mpsc::SyncSender<FederationMessage>,
    shutdown: &Arc<AtomicBool>,
    first_cause: &Arc<FirstCauseCell>,
    server_event_tx: &mpsc::Sender<ServerEvent>,
) {
    let SplitPaneRequest {
        request_id,
        target_pane_id,
        direction,
        ratio,
        focus,
    } = request;

    let (reply, rx) = oneshot::channel();
    let sent =
        server_event_tx.blocking_send(ServerEvent::Federation(FederationCommand::SplitPane {
            target_pane_id,
            direction,
            ratio,
            focus,
            reply,
        }));
    let outcome = if sent.is_err() {
        Err("server event loop is gone".to_string())
    } else {
        rx.blocking_recv()
            .unwrap_or_else(|_| Err("federation split-pane reply dropped".to_string()))
    };

    let response = match outcome {
        Ok((new_pane_id, new_terminal_id)) => SplitPaneResponse::Created {
            request_id,
            new_pane_id,
            new_terminal_id,
        },
        Err(reason) => SplitPaneResponse::Failed { request_id, reason },
    };
    let _ = enqueue_outbound(
        out_tx,
        FederationMessage::SplitPaneResponse(response),
        first_cause,
        shutdown,
    );
}

/// Services one inbound `SnapshotRequest` (post-mount pane mirroring fix,
/// plans/260722-1327): a blocking round-trip through
/// `FederationCommand::Snapshot`, replying with a `SnapshotResponse`
/// carrying the same atomic (snapshot, cursor) shape the mount handshake's
/// own `MountSnapshot` does. A dropped/gone actor (server shutting down)
/// replies with an empty snapshot at the connection's already-known cursor
/// isn't available here, so this falls back to `EventCursor(0)` — matching
/// `mount()`'s own `unwrap_or_else(empty_snapshot)` fallback shape; the
/// client's reducer only advances its cursor on the reply it actually
/// receives, so a best-effort empty answer here is inert, not corrupting.
fn handle_snapshot_request(
    server_instance_id: &ServerInstanceId,
    out_tx: &std_mpsc::SyncSender<FederationMessage>,
    shutdown: &Arc<AtomicBool>,
    first_cause: &Arc<FirstCauseCell>,
    server_event_tx: &mpsc::Sender<ServerEvent>,
) {
    let (reply, rx) = oneshot::channel();
    let sent =
        server_event_tx.blocking_send(ServerEvent::Federation(FederationCommand::Snapshot(reply)));
    let (snapshot, cursor) = if sent.is_err() {
        (
            crate::remote::federation::serve::empty_snapshot(),
            EventCursor(0),
        )
    } else {
        rx.blocking_recv().unwrap_or_else(|_| {
            (
                crate::remote::federation::serve::empty_snapshot(),
                EventCursor(0),
            )
        })
    };

    let _ = enqueue_outbound(
        out_tx,
        FederationMessage::SnapshotResponse(MountSnapshot {
            server_instance_id: server_instance_id.clone(),
            snapshot,
            cursor,
        }),
        first_cause,
        shutdown,
    );
}

/// Route one inbound `TerminalChannelMessage`: input/resize to the actor;
/// Open subscribes + spawns an output pump; Close stops that pump.
#[allow(clippy::too_many_arguments)]
fn handle_terminal_inbound(
    message: TerminalChannelMessage,
    epoch: AcceptEpoch,
    connid: ConnId,
    out_tx: &std_mpsc::SyncSender<FederationMessage>,
    shutdown: &Arc<AtomicBool>,
    first_cause: &Arc<FirstCauseCell>,
    server_event_tx: &mpsc::Sender<ServerEvent>,
    pumps: &mut HashMap<String, OutputPump>,
) -> io::Result<()> {
    match message {
        TerminalChannelMessage::Input {
            terminal_id, bytes, ..
        } => send_command(
            server_event_tx,
            FederationCommand::SendInput {
                epoch,
                connid,
                terminal_id,
                bytes,
            },
        ),
        TerminalChannelMessage::Resize {
            terminal_id,
            cols,
            rows,
            ..
        } => send_command(
            server_event_tx,
            FederationCommand::Resize {
                epoch,
                connid,
                terminal_id,
                cols,
                rows,
            },
        ),
        TerminalChannelMessage::Open { terminal_id, .. } => {
            open_terminal(
                terminal_id,
                epoch,
                connid,
                out_tx,
                shutdown,
                first_cause,
                server_event_tx,
                pumps,
            );
            Ok(())
        }
        TerminalChannelMessage::Close { terminal_id, .. } => {
            if let Some(pump) = pumps.remove(&terminal_id) {
                pump.stop.store(true, Ordering::SeqCst);
                let _ = pump.handle.join();
            }
            // The mount stopped mirroring this terminal, so it must not keep
            // owning the size: otherwise the host stays locked out of resizing
            // a terminal nobody drives until the whole mount goes away.
            send_command(
                server_event_tx,
                FederationCommand::ReleaseTerminalSize {
                    epoch,
                    connid,
                    terminal_id,
                },
            )
        }
        // The controller never streams Output upstream; ignore it defensively.
        TerminalChannelMessage::Output { .. } => Ok(()),
    }
}

/// Subscribe to a terminal's live output, emit the `Open{replay}` frame
/// carrying its scrollback, then spawn the pump that streams live bytes. A
/// terminal the App does not know is silently skipped (no frame, no pump),
/// matching `serve::handle_inbound`; a duplicate Open is a no-op.
///
/// Once the pump is live, ask the child to repaint. `Open{replay}` carries
/// `handoff_history_ansi`, which is empty for an alternate-screen app, so an
/// agent pane would otherwise mirror as a blank grid until the agent happened
/// to write something on its own.
#[allow(clippy::too_many_arguments)]
fn open_terminal(
    terminal_id: String,
    epoch: AcceptEpoch,
    connid: ConnId,
    out_tx: &std_mpsc::SyncSender<FederationMessage>,
    shutdown: &Arc<AtomicBool>,
    first_cause: &Arc<FirstCauseCell>,
    server_event_tx: &mpsc::Sender<ServerEvent>,
    pumps: &mut HashMap<String, OutputPump>,
) {
    if pumps.contains_key(&terminal_id) {
        return;
    }

    let Some(rx) = request_subscribe_output(server_event_tx, &terminal_id) else {
        debug!(%terminal_id, "federation open for an unknown terminal ignored");
        return;
    };
    let replay = request_scrollback_replay(server_event_tx, &terminal_id);

    // The Open frame (with scrollback) must precede any live Output, so it is
    // enqueued before the pump starts producing. A full queue here is already an
    // egress overflow — skip spawning the pump; teardown is under way.
    if !enqueue_outbound(
        out_tx,
        FederationMessage::Terminal(TerminalChannelMessage::Open {
            terminal_id: terminal_id.clone(),
            mount_generation: MOUNT_GENERATION,
            replay: ScrollbackReplay { bytes: replay },
        }),
        first_cause,
        shutdown,
    ) {
        return;
    }

    let stop = Arc::new(AtomicBool::new(false));
    let handle = {
        let pump_terminal_id = terminal_id.clone();
        let pump_out_tx = out_tx.clone();
        let pump_stop = Arc::clone(&stop);
        let pump_shutdown = Arc::clone(shutdown);
        let pump_first_cause = Arc::clone(first_cause);
        std::thread::spawn(move || {
            output_pump(
                pump_terminal_id,
                rx,
                pump_out_tx,
                pump_stop,
                pump_shutdown,
                pump_first_cause,
            )
        })
    };
    // After the pump, so the repaint bytes stream to the client instead of
    // being produced while nothing is listening — and after the `Open` frame
    // is enqueued, preserving replay-before-live ordering (RT-F6).
    let _ = send_command(
        server_event_tx,
        FederationCommand::NudgeRedraw {
            epoch,
            connid,
            terminal_id: terminal_id.clone(),
        },
    );

    pumps.insert(terminal_id, OutputPump { stop, handle });
}

/// A per-terminal output pump: the thread streaming a mounted terminal's live
/// bytes to the controller, plus its own stop flag so a `Close` (or a global
/// teardown) can end just that pump.
struct OutputPump {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<()>,
}

/// Stream one terminal's live output to the writer queue. This tokio's
/// `broadcast::Receiver` has no blocking receive, so the pump coalesces whatever
/// is buffered with `tee::drain_available` on each tick, then sleeps — waking
/// per byte is neither possible nor desirable. Lag is folded into the available
/// bytes (output is a byte stream; a dropped middle just shortens the tail, as
/// in `serve.rs`). Exits on its own stop flag, a global shutdown, or a writer
/// that has gone away.
fn output_pump(
    terminal_id: String,
    mut rx: broadcast::Receiver<Bytes>,
    out_tx: std_mpsc::SyncSender<FederationMessage>,
    stop: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    first_cause: Arc<FirstCauseCell>,
) {
    loop {
        if stop.load(Ordering::SeqCst) || shutdown.load(Ordering::SeqCst) {
            return;
        }
        let (bytes, _lagged) = tee::drain_available(&mut rx);
        if !bytes.is_empty()
            && !enqueue_outbound(
                &out_tx,
                FederationMessage::Terminal(TerminalChannelMessage::Output {
                    terminal_id: terminal_id.clone(),
                    mount_generation: MOUNT_GENERATION,
                    bytes,
                }),
                &first_cause,
                &shutdown,
            )
        {
            return; // egress overflow or the writer is gone; stop streaming.
        }
        std::thread::sleep(OUTBOUND_POLL_INTERVAL);
    }
}

/// Enqueue one outbound frame on the bounded writer queue. A full queue means
/// the peer is draining slower than we produce: fail fast — record
/// `EgressOverflow` (first-cause) and signal teardown rather than block a
/// producer thread or buffer without bound. `false` tells the caller to stop
/// (overflow or the writer already gone).
fn enqueue_outbound(
    out_tx: &std_mpsc::SyncSender<FederationMessage>,
    msg: FederationMessage,
    first_cause: &FirstCauseCell,
    shutdown: &AtomicBool,
) -> bool {
    match out_tx.try_send(msg) {
        Ok(()) => true,
        Err(std_mpsc::TrySendError::Full(_)) => {
            first_cause.set(TunnelExit::EgressOverflow);
            shutdown.store(true, Ordering::SeqCst);
            false
        }
        Err(std_mpsc::TrySendError::Disconnected(_)) => false,
    }
}

/// How many stage requests one connection may have in the system at once,
/// **counting the one being staged right now**. A payload is held whole in
/// memory (encoded, decoded, and written), so this is the only bound on how
/// much a single controller can make this host allocate for pastes.
///
/// Deliberately not expressed as the staging queue's depth: once the worker has
/// `recv`d a request, a queue of this depth would accept two more, putting
/// three payloads in flight.
const MAX_CONCURRENT_STAGES: usize = 2;

/// Performs the actual filesystem write for one stage request. One shared
/// closure rather than a bare `fn` pointer so a test can substitute an
/// operation that parks on a channel *it* owns; a `fn` pointer cannot capture,
/// which would force every such test onto one process-global rendezvous and
/// make them race each other. Production always passes
/// [`crate::remote::federation::file_staging::stage_remote_clipboard_image`].
type StagingOp = Arc<dyn Fn(&str, &[u8]) -> Result<PathBuf, ClipboardStageFailure> + Send + Sync>;

/// The connection's revocable outbound handle for the staging worker.
///
/// The worker must not hold a plain `out_tx` clone: the writer's `recv()` has
/// no timeout, so a live sender held by a thread stuck in a filesystem call
/// would keep the writer's join from ever completing. Teardown sets this to
/// `None`, which drops the worker's only sender synchronously; a worker that
/// finishes afterwards finds `None` and discards its result.
type RevocableOut = Arc<Mutex<Option<std_mpsc::SyncSender<FederationMessage>>>>;

/// One admitted stage request's share of [`MAX_CONCURRENT_STAGES`], released
/// when this value is dropped.
///
/// RAII rather than a `fetch_add`/`fetch_sub` pair is load-bearing. The permit
/// is taken by the reader *before* the request is handed to the worker, so a
/// hand-off that fails (a full queue, or a worker already gone at teardown)
/// hands the request — and this guard with it — straight back to the reader,
/// where dropping it returns the permit. A decrement that lived only in the
/// worker loop would leak on exactly that path, and the connection would
/// answer `Busy` for the rest of its life with nothing actually staging.
struct StagePermit {
    live: Arc<AtomicUsize>,
}

impl StagePermit {
    /// Takes one permit, or `None` when the cap is already reached. The
    /// compare-and-swap keeps the count exact under the reader thread racing
    /// the worker's release.
    fn acquire(live: &Arc<AtomicUsize>) -> Option<Self> {
        let mut current = live.load(Ordering::Acquire);
        loop {
            if current >= MAX_CONCURRENT_STAGES {
                return None;
            }
            match live.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Some(Self {
                        live: Arc::clone(live),
                    })
                }
                Err(observed) => current = observed,
            }
        }
    }
}

impl Drop for StagePermit {
    fn drop(&mut self) {
        self.live.fetch_sub(1, Ordering::AcqRel);
    }
}

/// One request queued for the staging worker, carrying its own admission
/// permit so the permit's lifetime is exactly the request's.
struct StageJob {
    request: ClipboardStageRequest,
    permit: StagePermit,
}

/// The reader side's handle on a connection's staging worker. Absent
/// (`None` at the call site) whenever `file_staging` was not agreed — which is
/// what makes "a host never emits a stage frame it did not negotiate"
/// structural rather than a check somebody could forget.
struct StagingChannel {
    jobs: std_mpsc::SyncSender<StageJob>,
    live: Arc<AtomicUsize>,
}

/// Spawns one connection's staging worker and returns the reader's handle on
/// it plus the revocable outbound handle teardown must clear.
///
/// The worker is **detached — never joined**. Dropping the job queue only wakes
/// a worker parked in `recv()`; it cannot interrupt a base64 decode or a
/// `write_all` already under way. Joining it would therefore pin
/// `LeaseReleaseGuard` — and with it this host's single-controller slot — for
/// as long as one filesystem call is stuck, refusing every later mount.
fn spawn_staging_worker(
    shutdown: &Arc<AtomicBool>,
    first_cause: &Arc<FirstCauseCell>,
    op: StagingOp,
) -> (StagingChannel, RevocableOut) {
    let out: RevocableOut = Arc::new(Mutex::new(None));
    let (jobs, rx) = std_mpsc::sync_channel::<StageJob>(MAX_CONCURRENT_STAGES);
    let worker_out = Arc::clone(&out);
    let worker_shutdown = Arc::clone(shutdown);
    let worker_first_cause = Arc::clone(first_cause);
    std::thread::spawn(move || {
        staging_worker_loop(rx, &worker_out, &worker_shutdown, &worker_first_cause, op)
    });
    (
        StagingChannel {
            jobs,
            live: Arc::new(AtomicUsize::new(0)),
        },
        out,
    )
}

/// Stage one request at a time until the queue disconnects.
///
/// The ordering is the contract: **decode before touching the filesystem**, so
/// a frame-legal but undecodable payload is answered `InvalidPayload` with no
/// file created and is never misreported as a disk failure.
///
/// The response goes out through [`enqueue_outbound`] like every other producer
/// on this connection. A silently dropped answer would be the worst outcome
/// available here: the file exists on this host, the controller's pending entry
/// is already claimed, and nothing downstream can retry — the controller would
/// simply time out and report that this host never answered. Failing the link
/// fast instead leaves the controller something it can retry.
fn staging_worker_loop(
    jobs: std_mpsc::Receiver<StageJob>,
    out: &RevocableOut,
    shutdown: &AtomicBool,
    first_cause: &FirstCauseCell,
    op: StagingOp,
) {
    use base64::Engine as _;

    while let Ok(job) = jobs.recv() {
        // Dropping the job here releases its permit; nothing was written.
        if shutdown.load(Ordering::SeqCst) {
            continue;
        }
        let StageJob { request, permit } = job;
        let ClipboardStageRequest {
            request_id,
            payload_base64,
            original_filename,
        } = request;

        let response = match base64::engine::general_purpose::STANDARD.decode(&payload_base64) {
            Err(err) => {
                debug!(request_id, %err, "federation stage payload is not decodable base64");
                ClipboardStageResponse::Failed {
                    request_id,
                    failure: ClipboardStageFailure::InvalidPayload,
                }
            }
            Ok(bytes) => match op(&original_filename, &bytes) {
                Ok(path) => match path.to_str() {
                    Some(path) => ClipboardStageResponse::Staged {
                        request_id,
                        path: path.to_string(),
                    },
                    // The staging module only ever returns a path built from
                    // a root it already proved losslessly UTF-8, so this is
                    // unreachable in practice; answering rather than panicking
                    // keeps a surprise from killing a live connection.
                    None => ClipboardStageResponse::Failed {
                        request_id,
                        failure: ClipboardStageFailure::StagingUnavailable,
                    },
                },
                Err(failure) => ClipboardStageResponse::Failed {
                    request_id,
                    failure,
                },
            },
        };

        // Locked only for the enqueue itself, never across filesystem work.
        match out.lock() {
            Ok(handle) => match handle.as_ref() {
                Some(tx) => {
                    if !enqueue_outbound(
                        tx,
                        FederationMessage::ClipboardStageResponse(response),
                        first_cause,
                        shutdown,
                    ) {
                        warn!(
                            request_id,
                            "federation stage response could not be queued; tearing down the link"
                        );
                    }
                }
                None => debug!(
                    request_id,
                    "federation stage finished after teardown; discarding the response"
                ),
            },
            Err(poisoned) => {
                debug!(request_id, "federation stage outbound handle poisoned");
                drop(poisoned);
            }
        }
        drop(permit);
    }
}

/// Route one inbound `ClipboardStageRequest`: admit it and hand it to the
/// worker, or refuse it — never stage on this thread. The reader also services
/// every pane's input, resize, open and close, so a multi-MiB write here would
/// stall every pane on the mount for the duration of the write.
///
/// With no worker (`staging` is `None`) the capability was not agreed, and the
/// frame is dropped without any answer: emitting a `ClipboardStageResponse` to
/// a peer that never negotiated the variant fails its decoder and tears down
/// its whole mount. An unnegotiated request is a protocol violation by the
/// peer, not a user-visible failure.
fn handle_clipboard_stage_request(
    request: ClipboardStageRequest,
    staging: Option<&StagingChannel>,
    out_tx: &std_mpsc::SyncSender<FederationMessage>,
    shutdown: &Arc<AtomicBool>,
    first_cause: &Arc<FirstCauseCell>,
) {
    let request_id = request.request_id;
    let Some(staging) = staging else {
        warn!(
            request_id,
            "dropping a federation stage request on a link that never agreed file staging"
        );
        return;
    };

    let refuse = |reason: &'static str| {
        debug!(request_id, reason, "refusing a federation stage request");
        let _ = enqueue_outbound(
            out_tx,
            FederationMessage::ClipboardStageResponse(ClipboardStageResponse::Failed {
                request_id,
                // Backpressure, not a disk failure: the client maps
                // `WriteFailed` to "the remote host ran out of space", which
                // would be a lie about a retryable queue limit.
                failure: ClipboardStageFailure::Busy,
            }),
            first_cause,
            shutdown,
        );
    };

    let Some(permit) = StagePermit::acquire(&staging.live) else {
        refuse("concurrent stage limit reached");
        return;
    };

    // The permit travels with the request. A `try_send` error hands the whole
    // `StageJob` back inside the error value, so the permit is dropped with it
    // right here and the connection does not wedge at `Busy`.
    if let Err(err) = staging.jobs.try_send(StageJob { request, permit }) {
        drop(err);
        refuse("staging worker queue unavailable");
    }
}

/// The connection's single outbound serializer: drain framed messages from the
/// queue and write them over the `try_clone`d handle. Exits cleanly when every
/// sender is dropped (`recv` returns `Err`), or records `WriterFailed` and
/// signals teardown on the first write error.
fn writer_loop<S: Write>(
    mut write_half: S,
    out_rx: std_mpsc::Receiver<FederationMessage>,
    first_cause: &FirstCauseCell,
    shutdown: &AtomicBool,
) {
    while let Ok(msg) = out_rx.recv() {
        if let Err(err) = write_frame_blocking(&mut write_half, &msg) {
            debug!(err = %err, "federation writer failed");
            first_cause.set(TunnelExit::WriterFailed);
            shutdown.store(true, Ordering::SeqCst);
            break;
        }
    }
}

/// Poll the live App on a fixed cadence for new events and (less often) agent
/// status changes, pushing frames to the writer. Ends when a shutdown is
/// signalled or the server event loop is gone.
fn ticker_loop(
    out_tx: &std_mpsc::SyncSender<FederationMessage>,
    server_event_tx: &mpsc::Sender<ServerEvent>,
    initial_cursor: EventCursor,
    shutdown: &AtomicBool,
    first_cause: &FirstCauseCell,
) {
    let mut cursor = initial_cursor.0;
    let mut last_agent: HashMap<String, (AgentStatus, Option<String>)> = HashMap::new();
    let mut tick: u32 = 0;
    while !shutdown.load(Ordering::SeqCst) {
        std::thread::sleep(OUTBOUND_POLL_INTERVAL);
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        if !poll_events(server_event_tx, &mut cursor, out_tx, first_cause, shutdown) {
            break;
        }
        tick = tick.wrapping_add(1);
        if tick.is_multiple_of(AGENT_STATUS_POLL_DIVISOR)
            && !poll_agent_statuses(
                server_event_tx,
                &mut last_agent,
                out_tx,
                first_cause,
                shutdown,
            )
        {
            break;
        }
    }
}

/// Fetch events after `cursor` from the actor and emit a `Gap` (if the sequence
/// skipped) followed by one `Frame` per event, advancing `cursor`. Returns
/// `false` if the actor or the writer is gone (the ticker should stop). Mirrors
/// `serve::poll_events`.
fn poll_events(
    server_event_tx: &mpsc::Sender<ServerEvent>,
    cursor: &mut u64,
    out_tx: &std_mpsc::SyncSender<FederationMessage>,
    first_cause: &FirstCauseCell,
    shutdown: &AtomicBool,
) -> bool {
    let Some(frames) = request_events_after(server_event_tx, *cursor) else {
        return false;
    };
    if frames.is_empty() {
        return true;
    }
    let first_seq = frames[0].0;
    if first_seq != *cursor + 1
        && !enqueue_outbound(
            out_tx,
            FederationMessage::Event(EventChannelMessage::Gap {
                from: *cursor,
                to: first_seq - 1,
            }),
            first_cause,
            shutdown,
        )
    {
        return false;
    }
    for (seq, kind) in &frames {
        if !enqueue_outbound(
            out_tx,
            FederationMessage::Event(EventChannelMessage::Frame(EventFrame {
                source_seq: *seq,
                kind: *kind,
            })),
            first_cause,
            shutdown,
        ) {
            return false;
        }
    }
    *cursor = frames.last().map(|(seq, _)| *seq).unwrap_or(*cursor);
    true
}

/// Fetch agent statuses from the actor and emit a frame for each one whose
/// status OR identified agent changed since the last poll (a pane's agent
/// identity can become known after its status was already sent once, e.g.
/// screen-text detection resolving after the first `Working` poll — that
/// must still reach the client so `AgentStatusMessage::agent` isn't
/// permanently `None` for it). Returns `false` if the actor or the writer is
/// gone. Mirrors `serve::poll_agent_statuses`.
fn poll_agent_statuses(
    server_event_tx: &mpsc::Sender<ServerEvent>,
    last: &mut HashMap<String, (AgentStatus, Option<String>)>,
    out_tx: &std_mpsc::SyncSender<FederationMessage>,
    first_cause: &FirstCauseCell,
    shutdown: &AtomicBool,
) -> bool {
    let Some(statuses) = request_agent_statuses(server_event_tx) else {
        return false;
    };
    for (terminal_id, status, agent) in statuses {
        if last.get(&terminal_id) == Some(&(status, agent.clone())) {
            continue;
        }
        last.insert(terminal_id.clone(), (status, agent.clone()));
        if !enqueue_outbound(
            out_tx,
            FederationMessage::AgentStatus(AgentStatusMessage {
                terminal_id,
                mount_generation: MOUNT_GENERATION,
                status,
                agent,
            }),
            first_cause,
            shutdown,
        ) {
            return false;
        }
    }
    true
}

/// Blocking actor round-trip: events after `since`. `None` if the event loop
/// dropped the request or the reply.
fn request_events_after(
    server_event_tx: &mpsc::Sender<ServerEvent>,
    since: u64,
) -> Option<Vec<(u64, EventKind)>> {
    let (reply, rx) = oneshot::channel();
    server_event_tx
        .blocking_send(ServerEvent::Federation(FederationCommand::EventsAfter(
            since, reply,
        )))
        .ok()?;
    rx.blocking_recv().ok()
}

/// Blocking actor round-trip: the current agent statuses. `None` if the event
/// loop is gone.
fn request_agent_statuses(
    server_event_tx: &mpsc::Sender<ServerEvent>,
) -> Option<Vec<(String, AgentStatus, Option<String>)>> {
    let (reply, rx) = oneshot::channel();
    server_event_tx
        .blocking_send(ServerEvent::Federation(FederationCommand::AgentStatuses(
            reply,
        )))
        .ok()?;
    rx.blocking_recv().ok()
}

/// Blocking actor round-trip: subscribe to a terminal's output. `None` for an
/// unknown terminal or a gone event loop.
fn request_subscribe_output(
    server_event_tx: &mpsc::Sender<ServerEvent>,
    terminal_id: &str,
) -> Option<broadcast::Receiver<Bytes>> {
    let (reply, rx) = oneshot::channel();
    server_event_tx
        .blocking_send(ServerEvent::Federation(FederationCommand::SubscribeOutput(
            terminal_id.to_string(),
            reply,
        )))
        .ok()?;
    rx.blocking_recv().ok().flatten()
}

/// Blocking actor round-trip: a terminal's scrollback. Empty if the terminal is
/// unknown or the event loop is gone (an empty replay is a valid Open payload).
fn request_scrollback_replay(
    server_event_tx: &mpsc::Sender<ServerEvent>,
    terminal_id: &str,
) -> Vec<u8> {
    let (reply, rx) = oneshot::channel();
    if server_event_tx
        .blocking_send(ServerEvent::Federation(
            FederationCommand::ScrollbackReplay(terminal_id.to_string(), reply),
        ))
        .is_err()
    {
        return Vec::new();
    }
    rx.blocking_recv().unwrap_or_default()
}

/// Fire-and-forget send of a federation command to the server event loop.
fn send_command(
    server_event_tx: &mpsc::Sender<ServerEvent>,
    command: FederationCommand,
) -> io::Result<()> {
    server_event_tx
        .blocking_send(ServerEvent::Federation(command))
        .map_err(|_| io::Error::other("server event loop is gone"))
}

/// Reserve the single-controller slot via the actor: a blocking round-trip to
/// the server event loop (legal here — a plain `std::thread`, no tokio runtime).
fn acquire_controller(
    epoch: AcceptEpoch,
    connid: ConnId,
    server_event_tx: &mpsc::Sender<ServerEvent>,
) -> io::Result<Admission> {
    let (reply, rx) = oneshot::channel();
    server_event_tx
        .blocking_send(ServerEvent::Federation(
            FederationCommand::AcquireController {
                epoch,
                connid,
                reply,
            },
        ))
        .map_err(|_| io::Error::other("server event loop is gone"))?;
    rx.blocking_recv()
        .map_err(|_| io::Error::other("federation acquire reply dropped"))
}

/// Promote the reservation to `Mounted` and fetch the atomic (snapshot, cursor)
/// via the actor. `None` if the reservation was superseded before the mount.
fn mount(
    epoch: AcceptEpoch,
    connid: ConnId,
    server_event_tx: &mpsc::Sender<ServerEvent>,
) -> io::Result<Option<(SessionSnapshot, EventCursor)>> {
    let (reply, rx) = oneshot::channel();
    server_event_tx
        .blocking_send(ServerEvent::Federation(FederationCommand::Mount {
            epoch,
            connid,
            reply,
        }))
        .map_err(|_| io::Error::other("server event loop is gone"))?;
    rx.blocking_recv()
        .map_err(|_| io::Error::other("federation mount reply dropped"))
}

/// Releases the single-controller lease for `(epoch, connid)` when the
/// connection thread returns or unwinds — the RAII backstop that keeps a
/// connection which dies after acquiring (but before a handoff bumps the epoch)
/// from pinning the slot forever. The lease's `release` is compare-and-clear, so
/// a release that races a newer lease is inert.
struct LeaseReleaseGuard {
    epoch: AcceptEpoch,
    connid: ConnId,
    server_event_tx: mpsc::Sender<ServerEvent>,
}

impl LeaseReleaseGuard {
    fn new(epoch: AcceptEpoch, connid: ConnId, server_event_tx: mpsc::Sender<ServerEvent>) -> Self {
        Self {
            epoch,
            connid,
            server_event_tx,
        }
    }
}

impl Drop for LeaseReleaseGuard {
    fn drop(&mut self) {
        // `blocking_send` is legal here (a plain std::thread, never a tokio
        // runtime thread) and returns immediately if the loop is gone (the
        // server is shutting down — the lease dies with it), so Drop never hangs.
        let _ = self.server_event_tx.blocking_send(ServerEvent::Federation(
            FederationCommand::Release {
                epoch: self.epoch,
                connid: self.connid,
            },
        ));
    }
}

/// The stream-generic handshake exchange: read the peer's `Handshake`, negotiate
/// against this server's identity + capabilities, and reply `Accept`/`Reject`.
/// Returns the agreed capabilities on acceptance so the mount can gate every
/// later send on them, `None` on a rejection or a missing handshake. Generic over
/// `Read + Write` so it is unit-tested over a `UnixStream` pair.
fn drive_handshake<S: Read + Write>(
    stream: &mut S,
    connid: u64,
    server_instance_id: &ServerInstanceId,
) -> io::Result<Option<AgreedCaps>> {
    let Some(FederationMessage::Handshake(remote)) = read_frame_blocking(stream)? else {
        debug!(connid, "federation link did not open with a Handshake");
        return Ok(None);
    };

    let local = Handshake {
        federation_protocol_version: FEDERATION_PROTOCOL_VERSION,
        capabilities: federation_capabilities(),
        server_instance_id: server_instance_id.clone(),
    };

    match negotiate(&local, &remote) {
        Ok(agreed) => {
            write_frame_blocking(
                stream,
                &FederationMessage::HandshakeResponse(HandshakeResponse::Accept {
                    agreed_capabilities: agreed.0.clone(),
                }),
            )?;
            Ok(Some(agreed))
        }
        Err(reason) => {
            write_frame_blocking(
                stream,
                &FederationMessage::HandshakeResponse(HandshakeResponse::Reject { reason }),
            )?;
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    // Exercise the bidirectional driver over a real socket pair: `UnixStream`
    // clones into independent handles exactly like the production `LocalStream`.
    impl FederationStream for UnixStream {
        fn try_clone_stream(&self) -> io::Result<Self> {
            self.try_clone()
        }
    }

    #[test]
    fn handshake_accepts_a_compatible_peer_and_replies_with_an_accept() {
        let (mut client, mut server) = UnixStream::pair().expect("socket pair");
        let sid = ServerInstanceId::fresh();

        let hello = FederationMessage::Handshake(Handshake {
            federation_protocol_version: FEDERATION_PROTOCOL_VERSION,
            capabilities: BTreeSet::new(),
            server_instance_id: ServerInstanceId("peer".to_string()),
        });
        write_frame_blocking(&mut client, &hello).expect("client writes hello");

        let outcome = drive_handshake(&mut server, 1, &sid).expect("handshake runs");
        assert!(outcome.is_some(), "a compatible peer is accepted");

        let response = read_frame_blocking(&mut client)
            .expect("client reads response")
            .expect("response present");
        assert!(matches!(
            response,
            FederationMessage::HandshakeResponse(HandshakeResponse::Accept { .. })
        ));
    }

    #[test]
    fn handshake_rejects_a_protocol_version_mismatch() {
        let (mut client, mut server) = UnixStream::pair().expect("socket pair");
        let sid = ServerInstanceId::fresh();

        // The frame header still carries this build's codec version (so it
        // decodes); the *payload's* federation_protocol_version is what negotiate
        // rejects — the two are independent layers.
        let hello = FederationMessage::Handshake(Handshake {
            federation_protocol_version: FEDERATION_PROTOCOL_VERSION + 1,
            capabilities: BTreeSet::new(),
            server_instance_id: ServerInstanceId("peer".to_string()),
        });
        write_frame_blocking(&mut client, &hello).expect("client writes hello");

        let outcome = drive_handshake(&mut server, 1, &sid).expect("handshake runs");
        assert!(outcome.is_none(), "a version mismatch is rejected");

        let response = read_frame_blocking(&mut client)
            .expect("client reads response")
            .expect("response present");
        assert!(matches!(
            response,
            FederationMessage::HandshakeResponse(HandshakeResponse::Reject { .. })
        ));
    }

    #[test]
    fn a_link_that_does_not_open_with_a_handshake_is_dropped() {
        let (mut client, mut server) = UnixStream::pair().expect("socket pair");
        let sid = ServerInstanceId::fresh();

        // A non-Handshake first frame (an Event) must not be accepted.
        let wrong = FederationMessage::Event(
            crate::remote::federation::protocol::EventChannelMessage::Reset,
        );
        write_frame_blocking(&mut client, &wrong).expect("client writes wrong first frame");

        let outcome = drive_handshake(&mut server, 1, &sid).expect("handshake runs");
        assert!(outcome.is_none(), "a non-handshake opener is dropped");
    }

    /// A minimal live `App` with session persistence disabled, mirroring the
    /// federation_actor test harness — enough to service Acquire/Mount.
    fn test_app() -> crate::app::App {
        let config = crate::config::Config::default();
        let (_api_tx, api_rx) = mpsc::unbounded_channel();
        crate::app::App::new(&config, true, None, api_rx, crate::api::EventHub::default())
    }

    #[test]
    fn drive_mount_acquires_the_lease_and_streams_a_snapshot_through_the_actor() {
        use crate::server::federation_lease::FederationLease;

        // A mock server event loop: service Federation commands against a test
        // App + lease, exactly as HeadlessServer's handle_server_event arm does.
        let (tx, mut rx) = mpsc::channel::<ServerEvent>(64);
        let loop_handle = std::thread::spawn(move || {
            let mut app = test_app();
            let mut lease = FederationLease::new();
            while let Some(ev) = rx.blocking_recv() {
                if let ServerEvent::Federation(cmd) = ev {
                    crate::server::federation_actor::dispatch(&mut app, &mut lease, cmd);
                }
            }
        });

        let (mut client, mut server) = UnixStream::pair().expect("socket pair");
        let sid = ServerInstanceId::fresh();

        // The peer reads the mount snapshot the server streams after mounting.
        let peer_sid = sid.clone();
        let client_thread = std::thread::spawn(move || {
            let snapshot = read_frame_blocking(&mut client)
                .expect("client reads snapshot")
                .expect("snapshot present");
            match snapshot {
                FederationMessage::MountSnapshot(mount) => {
                    assert_eq!(
                        mount.server_instance_id, peer_sid,
                        "snapshot carries this server's identity"
                    );
                }
                other => panic!("expected MountSnapshot, got {other:?}"),
            }
        });

        // Acquire → Mount → stream the snapshot, all bridged through the actor.
        drive_mount(&mut server, 0, 1, &AgreedCaps(BTreeSet::new()), &sid, &tx)
            .expect("mount runs end to end");

        client_thread.join().expect("client thread");

        // drive_mount's guard already released the lease on return; dropping the
        // last sender ends the mock loop.
        drop(server);
        drop(tx);
        loop_handle.join().expect("mock loop joins");
    }

    #[test]
    fn command_loop_routes_controller_input_and_resize_to_the_actor() {
        use std::sync::{Arc, Mutex};

        // A recording mock loop captures the routed commands (bypassing the App
        // + lease authz, which federation_actor already tests) so we assert the
        // reader's frame→command routing directly.
        let (tx, mut rx) = mpsc::channel::<ServerEvent>(64);
        // (kind, terminal_id, bytes) tuples the mock loop records for assertion.
        type Recorded = Arc<Mutex<Vec<(&'static str, String, Vec<u8>)>>>;
        let recorded: Recorded = Arc::new(Mutex::new(Vec::new()));
        let sink = recorded.clone();
        let loop_handle = std::thread::spawn(move || {
            while let Some(ev) = rx.blocking_recv() {
                if let ServerEvent::Federation(cmd) = ev {
                    match cmd {
                        FederationCommand::SendInput {
                            terminal_id, bytes, ..
                        } => sink.lock().unwrap().push(("input", terminal_id, bytes)),
                        FederationCommand::Resize {
                            terminal_id,
                            cols,
                            rows,
                            ..
                        } => sink.lock().unwrap().push((
                            "resize",
                            terminal_id,
                            vec![cols as u8, rows as u8],
                        )),
                        _ => {}
                    }
                }
            }
        });

        let (mut client, mut server) = UnixStream::pair().expect("socket pair");
        write_frame_blocking(
            &mut client,
            &FederationMessage::Terminal(TerminalChannelMessage::Input {
                terminal_id: "t1".to_string(),
                mount_generation: 1,
                bytes: b"hi".to_vec(),
            }),
        )
        .expect("client writes input");
        write_frame_blocking(
            &mut client,
            &FederationMessage::Terminal(TerminalChannelMessage::Resize {
                terminal_id: "t1".to_string(),
                mount_generation: 1,
                cols: 80,
                rows: 24,
            }),
        )
        .expect("client writes resize");
        drop(client); // EOF ends the loop

        let shutdown = Arc::new(AtomicBool::new(false));
        let first_cause = Arc::new(FirstCauseCell::new());
        let (out_tx, _out_rx) = std_mpsc::sync_channel::<FederationMessage>(EGRESS_QUEUE_CAP);
        let mut pumps = HashMap::new();
        reader_loop(
            &mut server,
            0,
            1,
            &ServerInstanceId("test-inst".to_string()),
            &out_tx,
            &shutdown,
            &first_cause,
            &tx,
            &mut pumps,
            None,
        )
        .expect("reader loop drains to EOF");
        drop(tx);
        loop_handle.join().expect("mock loop joins");

        let rec = recorded.lock().unwrap();
        assert_eq!(rec.len(), 2, "both frames routed");
        assert_eq!(rec[0], ("input", "t1".to_string(), b"hi".to_vec()));
        assert_eq!(rec[1].0, "resize");
        assert_eq!(rec[1].2, vec![80u8, 24u8]);
    }

    #[test]
    fn reader_loop_routes_a_split_pane_request_and_replies_created() {
        // A mock actor loop answers `FederationCommand::SplitPane` directly
        // (the real dispatch is `federation_actor`'s own responsibility,
        // already tested there) so this asserts only the reader's
        // frame -> command -> response routing.
        let (tx, mut rx) = mpsc::channel::<ServerEvent>(64);
        let loop_handle = std::thread::spawn(move || {
            while let Some(ev) = rx.blocking_recv() {
                if let ServerEvent::Federation(FederationCommand::SplitPane {
                    target_pane_id,
                    reply,
                    ..
                }) = ev
                {
                    let _ = reply.send(Ok((
                        format!("{target_pane_id}-split"),
                        format!("{target_pane_id}-split-term"),
                    )));
                }
            }
        });

        let (mut client, mut server) = UnixStream::pair().expect("socket pair");
        write_frame_blocking(
            &mut client,
            &FederationMessage::SplitPaneRequest(
                crate::remote::federation::protocol::SplitPaneRequest {
                    request_id: 7,
                    target_pane_id: "p1".to_string(),
                    direction: crate::remote::federation::protocol::SplitDirection::Right,
                    ratio: None,
                    focus: false,
                },
            ),
        )
        .expect("client writes split request");
        drop(client); // EOF ends the reader after servicing the one frame

        let shutdown = Arc::new(AtomicBool::new(false));
        let first_cause = Arc::new(FirstCauseCell::new());
        let (out_tx, out_rx) = std_mpsc::sync_channel::<FederationMessage>(EGRESS_QUEUE_CAP);
        let mut pumps = HashMap::new();
        reader_loop(
            &mut server,
            0,
            1,
            &ServerInstanceId("test-inst".to_string()),
            &out_tx,
            &shutdown,
            &first_cause,
            &tx,
            &mut pumps,
            None,
        )
        .expect("reader loop drains to EOF");
        drop(tx);
        loop_handle.join().expect("mock loop joins");

        let response = out_rx.try_recv().expect("a response was enqueued");
        match response {
            FederationMessage::SplitPaneResponse(
                crate::remote::federation::protocol::SplitPaneResponse::Created {
                    request_id,
                    new_pane_id,
                    new_terminal_id,
                },
            ) => {
                assert_eq!(request_id, 7);
                assert_eq!(new_pane_id, "p1-split");
                assert_eq!(new_terminal_id, "p1-split-term");
            }
            other => panic!("expected SplitPaneResponse::Created, got {other:?}"),
        }
    }

    // Post-mount pane mirroring fix (plans/260722-1327): a `SnapshotRequest`
    // routes to `FederationCommand::Snapshot` and the reply comes back as a
    // `SnapshotResponse` carrying the mock actor's canned snapshot/cursor,
    // tagged with this connection's `server_instance_id`.
    #[test]
    fn reader_loop_routes_a_snapshot_request_and_replies_with_a_snapshot() {
        let (tx, mut rx) = mpsc::channel::<ServerEvent>(64);
        let loop_handle = std::thread::spawn(move || {
            while let Some(ev) = rx.blocking_recv() {
                if let ServerEvent::Federation(FederationCommand::Snapshot(reply)) = ev {
                    let _ = reply.send((
                        crate::remote::federation::serve::empty_snapshot(),
                        EventCursor(42),
                    ));
                }
            }
        });

        let (mut client, mut server) = UnixStream::pair().expect("socket pair");
        write_frame_blocking(
            &mut client,
            &FederationMessage::SnapshotRequest(
                crate::remote::federation::protocol::SnapshotRequest,
            ),
        )
        .expect("client writes snapshot request");
        drop(client); // EOF ends the reader after servicing the one frame

        let shutdown = Arc::new(AtomicBool::new(false));
        let first_cause = Arc::new(FirstCauseCell::new());
        let (out_tx, out_rx) = std_mpsc::sync_channel::<FederationMessage>(EGRESS_QUEUE_CAP);
        let mut pumps = HashMap::new();
        reader_loop(
            &mut server,
            0,
            1,
            &ServerInstanceId("test-inst".to_string()),
            &out_tx,
            &shutdown,
            &first_cause,
            &tx,
            &mut pumps,
            None,
        )
        .expect("reader loop drains to EOF");
        drop(tx);
        loop_handle.join().expect("mock loop joins");

        let response = out_rx.try_recv().expect("a response was enqueued");
        match response {
            FederationMessage::SnapshotResponse(MountSnapshot {
                server_instance_id,
                cursor,
                ..
            }) => {
                assert_eq!(
                    server_instance_id,
                    ServerInstanceId("test-inst".to_string())
                );
                assert_eq!(cursor.0, 42);
            }
            other => panic!("expected SnapshotResponse, got {other:?}"),
        }
    }

    #[test]
    fn writer_loop_drains_the_queue_and_stops_when_senders_drop() {
        // The writer thread serialises queued frames to the wire and exits
        // cleanly once every sender is gone — the clean-teardown path.
        let (mut client, server) = UnixStream::pair().expect("socket pair");
        let (out_tx, out_rx) = std_mpsc::channel::<FederationMessage>();
        let first_cause = Arc::new(FirstCauseCell::new());
        let shutdown = Arc::new(AtomicBool::new(false));

        let writer = {
            let first_cause = Arc::clone(&first_cause);
            let shutdown = Arc::clone(&shutdown);
            std::thread::spawn(move || writer_loop(server, out_rx, &first_cause, &shutdown))
        };

        out_tx
            .send(FederationMessage::Event(EventChannelMessage::Reset))
            .expect("queue first frame");
        out_tx
            .send(FederationMessage::Event(EventChannelMessage::Gap {
                from: 1,
                to: 3,
            }))
            .expect("queue second frame");
        drop(out_tx); // no senders left -> writer exits after draining

        let first = read_frame_blocking(&mut client)
            .expect("read first")
            .expect("first present");
        assert!(matches!(
            first,
            FederationMessage::Event(EventChannelMessage::Reset)
        ));
        let second = read_frame_blocking(&mut client)
            .expect("read second")
            .expect("second present");
        assert!(matches!(
            second,
            FederationMessage::Event(EventChannelMessage::Gap { from: 1, to: 3 })
        ));

        writer.join().expect("writer joins on sender drop");
        // A clean drain records no fault.
        assert!(!first_cause.is_set(), "clean teardown sets no first cause");
    }

    #[test]
    fn open_terminal_emits_the_scrollback_replay_frame() {
        // A mock actor answers SubscribeOutput + ScrollbackReplay; open_terminal
        // must emit an Open frame carrying that scrollback before any live byte.
        let (tx, mut rx) = mpsc::channel::<ServerEvent>(64);
        let loop_handle = std::thread::spawn(move || {
            // Keep the broadcast senders alive so the spawned pump's receiver
            // does not see an immediate close during the test.
            let mut keep_alive: Vec<broadcast::Sender<Bytes>> = Vec::new();
            while let Some(ev) = rx.blocking_recv() {
                if let ServerEvent::Federation(cmd) = ev {
                    match cmd {
                        FederationCommand::SubscribeOutput(_id, reply) => {
                            let (btx, brx) = broadcast::channel::<Bytes>(16);
                            keep_alive.push(btx);
                            let _ = reply.send(Some(brx));
                        }
                        FederationCommand::ScrollbackReplay(_id, reply) => {
                            let _ = reply.send(b"scrollback".to_vec());
                        }
                        _ => {}
                    }
                }
            }
        });

        let (out_tx, out_rx) = std_mpsc::sync_channel::<FederationMessage>(EGRESS_QUEUE_CAP);
        let shutdown = Arc::new(AtomicBool::new(false));
        let first_cause = Arc::new(FirstCauseCell::new());
        let mut pumps: HashMap<String, OutputPump> = HashMap::new();

        open_terminal(
            "t1".to_string(),
            1,
            1,
            &out_tx,
            &shutdown,
            &first_cause,
            &tx,
            &mut pumps,
        );

        let frame = out_rx.recv().expect("open frame emitted");
        match frame {
            FederationMessage::Terminal(TerminalChannelMessage::Open {
                terminal_id,
                mount_generation,
                replay,
            }) => {
                assert_eq!(terminal_id, "t1");
                assert_eq!(mount_generation, MOUNT_GENERATION);
                assert_eq!(replay.bytes, b"scrollback".to_vec());
            }
            other => panic!("expected Open with replay, got {other:?}"),
        }
        assert!(pumps.contains_key("t1"), "a pump is registered for t1");

        // Tear the pump down and end the mock loop.
        shutdown.store(true, Ordering::SeqCst);
        for (_id, pump) in pumps.drain() {
            pump.stop.store(true, Ordering::SeqCst);
            let _ = pump.handle.join();
        }
        drop(out_tx);
        drop(tx);
        loop_handle.join().expect("mock loop joins");
    }

    #[test]
    fn enqueue_outbound_fails_fast_on_a_full_queue() {
        // A peer that stops reading fills the bounded queue; the next enqueue
        // must fail fast as EgressOverflow and signal teardown, never block.
        let (tx, _rx) = std_mpsc::sync_channel::<FederationMessage>(1);
        let first_cause = FirstCauseCell::new();
        let shutdown = AtomicBool::new(false);

        assert!(
            enqueue_outbound(
                &tx,
                FederationMessage::Event(EventChannelMessage::Reset),
                &first_cause,
                &shutdown,
            ),
            "the first frame fits the one slot"
        );
        assert!(
            !enqueue_outbound(
                &tx,
                FederationMessage::Event(EventChannelMessage::Reset),
                &first_cause,
                &shutdown,
            ),
            "the second frame overflows and fails fast"
        );
        assert_eq!(first_cause.get(), Some(TunnelExit::EgressOverflow));
        assert!(shutdown.load(Ordering::SeqCst), "teardown is signalled");
    }

    // ---- file staging -------------------------------------------------

    const PNG_BYTES: &[u8] = b"\x89PNG\r\n\x1a\nfake";

    fn caps(with_staging: bool) -> AgreedCaps {
        let mut set: BTreeSet<Capability> = BTreeSet::new();
        set.insert(Capability::new(Capability::AGENT_STATUS));
        if with_staging {
            set.insert(Capability::new(Capability::FILE_STAGING));
        }
        AgreedCaps(set)
    }

    fn stage_frame(request_id: u64, payload_base64: &str) -> FederationMessage {
        FederationMessage::ClipboardStageRequest(ClipboardStageRequest {
            request_id,
            payload_base64: payload_base64.to_string(),
            original_filename: "image.png".to_string(),
        })
    }

    fn png_payload() -> String {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(PNG_BYTES)
    }

    fn input_frame(terminal_id: &str) -> FederationMessage {
        FederationMessage::Terminal(TerminalChannelMessage::Input {
            terminal_id: terminal_id.to_string(),
            mount_generation: MOUNT_GENERATION,
            bytes: b"x".to_vec(),
        })
    }

    /// One live `run_connection` over a socket pair, plus a mock server event
    /// loop that reports the terminal ids whose input actually reached it —
    /// which is how these tests observe that the reader kept servicing panes.
    struct ConnectionHarness {
        client: UnixStream,
        inputs: std_mpsc::Receiver<String>,
        conn: Option<std::thread::JoinHandle<io::Result<()>>>,
        mock: Option<std::thread::JoinHandle<()>>,
    }

    impl ConnectionHarness {
        fn spawn(agreed: AgreedCaps, op: StagingOp) -> Self {
            let (client, mut server) = UnixStream::pair().expect("socket pair");
            let (tx, mut rx) = mpsc::channel::<ServerEvent>(64);
            let (input_tx, inputs) = std_mpsc::channel::<String>();
            let mock = std::thread::spawn(move || {
                while let Some(ev) = rx.blocking_recv() {
                    if let ServerEvent::Federation(FederationCommand::SendInput {
                        terminal_id,
                        ..
                    }) = ev
                    {
                        let _ = input_tx.send(terminal_id);
                    }
                }
            });
            let conn = std::thread::spawn(move || {
                run_connection(
                    &mut server,
                    0,
                    1,
                    EventCursor(0),
                    &agreed,
                    op,
                    &ServerInstanceId("test-inst".to_string()),
                    &tx,
                )
            });
            Self {
                client,
                inputs,
                conn: Some(conn),
                mock: Some(mock),
            }
        }

        fn send(&mut self, msg: &FederationMessage) {
            write_frame_blocking(&mut self.client, msg).expect("client writes a frame");
        }

        /// Next `ClipboardStageResponse` written by the connection, skipping
        /// unrelated outbound traffic. `None` once the read window elapses —
        /// a window long enough that expiring it means the test failed, so a
        /// half-read frame afterwards cannot matter.
        fn next_stage_response(&mut self) -> Option<ClipboardStageResponse> {
            self.client
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("client read timeout");
            let response = loop {
                match read_frame_blocking(&mut self.client) {
                    Ok(Some(FederationMessage::ClipboardStageResponse(response))) => {
                        break Some(response)
                    }
                    Ok(Some(_other)) => continue,
                    Ok(None) => break None,
                    Err(_) => break None,
                }
            };
            self.client
                .set_read_timeout(None)
                .expect("clear client read timeout");
            response
        }

        /// Asserts nothing that looks like a stage frame reaches the peer in
        /// `window`. The read timeout is what bounds each attempt.
        fn assert_no_stage_frame_within(&mut self, window: Duration) {
            self.client
                .set_read_timeout(Some(Duration::from_millis(100)))
                .expect("client read timeout");
            let deadline = std::time::Instant::now() + window;
            while std::time::Instant::now() < deadline {
                match read_frame_blocking(&mut self.client) {
                    Ok(Some(FederationMessage::ClipboardStageResponse(response))) => {
                        panic!("a stage frame reached the peer: {response:?}")
                    }
                    Ok(Some(_other)) => continue,
                    Ok(None) => break,
                    Err(_) => continue,
                }
            }
            self.client
                .set_read_timeout(None)
                .expect("clear client read timeout");
        }

        fn expect_input(&self, terminal_id: &str) {
            let got = self
                .inputs
                .recv_timeout(Duration::from_secs(5))
                .expect("the reader kept servicing terminal input");
            assert_eq!(got, terminal_id);
        }

        /// Ends the connection and returns `run_connection`'s result.
        fn finish(mut self) -> io::Result<()> {
            let _ = self.client.shutdown(std::net::Shutdown::Both);
            let result = self
                .conn
                .take()
                .expect("connection thread")
                .join()
                .expect("connection thread joins");
            let _ = self.mock.take().expect("mock thread").join();
            result
        }
    }

    /// A staging op that parks until the test releases it, reporting each
    /// request it actually reached.
    struct ParkedOp {
        op: StagingOp,
        entered: std_mpsc::Receiver<String>,
        release: std_mpsc::SyncSender<()>,
    }

    fn parked_op() -> ParkedOp {
        let (entered_tx, entered) = std_mpsc::channel::<String>();
        let (release, release_rx) = std_mpsc::sync_channel::<()>(8);
        let release_rx = Arc::new(Mutex::new(release_rx));
        let op: StagingOp = Arc::new(move |name: &str, _bytes: &[u8]| {
            let _ = entered_tx.send(name.to_string());
            let guard = release_rx.lock().expect("release channel");
            let _ = guard.recv();
            Err(ClipboardStageFailure::WriteFailed)
        });
        ParkedOp {
            op,
            entered,
            release,
        }
    }

    /// The reader thread also services every pane's input, resize and close,
    /// so a stage request must be handed off, never staged where it can stall
    /// them. The queue still holding the request while a later input frame has
    /// already been serviced is exactly that property.
    #[test]
    fn reader_loop_hands_a_clipboard_stage_request_to_the_staging_worker_without_blocking() {
        let (tx, mut rx) = mpsc::channel::<ServerEvent>(64);
        let (input_tx, inputs) = std_mpsc::channel::<String>();
        let mock = std::thread::spawn(move || {
            while let Some(ev) = rx.blocking_recv() {
                if let ServerEvent::Federation(FederationCommand::SendInput {
                    terminal_id, ..
                }) = ev
                {
                    let _ = input_tx.send(terminal_id);
                }
            }
        });

        let (mut client, mut server) = UnixStream::pair().expect("socket pair");
        write_frame_blocking(&mut client, &stage_frame(1, &png_payload()))
            .expect("client writes a stage request");
        write_frame_blocking(&mut client, &input_frame("t1")).expect("client writes input");
        drop(client);

        // No worker is spawned here: the test holds the queue's receiving end
        // itself, so a reader that staged inline could never reach the input
        // frame behind it.
        let (jobs, jobs_rx) = std_mpsc::sync_channel::<StageJob>(MAX_CONCURRENT_STAGES);
        let staging = StagingChannel {
            jobs,
            live: Arc::new(AtomicUsize::new(0)),
        };
        let shutdown = Arc::new(AtomicBool::new(false));
        let first_cause = Arc::new(FirstCauseCell::new());
        let (out_tx, out_rx) = std_mpsc::sync_channel::<FederationMessage>(EGRESS_QUEUE_CAP);
        let mut pumps = HashMap::new();
        reader_loop(
            &mut server,
            0,
            1,
            &ServerInstanceId("test-inst".to_string()),
            &out_tx,
            &shutdown,
            &first_cause,
            &tx,
            &mut pumps,
            Some(&staging),
        )
        .expect("reader loop drains to EOF");
        drop(tx);
        mock.join().expect("mock loop joins");

        assert_eq!(
            inputs
                .recv_timeout(Duration::from_secs(5))
                .expect("the input frame behind the stage request was serviced"),
            "t1"
        );
        let job = jobs_rx
            .try_recv()
            .expect("the stage request is still queued, not staged on the reader");
        assert_eq!(job.request.request_id, 1);
        assert!(
            out_rx.try_recv().is_err(),
            "an admitted request must not answer before it is staged"
        );
    }

    /// The serving half of the capability gate. A host that negotiated no file
    /// staging must not put *either* stage frame on the wire: an older
    /// controller has no decoder for the variant, its `read_frame` fails, and
    /// its whole mount — every pane on the link — dies.
    #[test]
    fn a_host_without_the_agreed_file_staging_capability_never_emits_a_stage_frame() {
        // An op that *would* answer if it were ever reached, so dropping the
        // gate produces a frame this test can see. A panicking op would not:
        // it would kill the worker silently and leave the wire exactly as
        // quiet as a correctly gated host.
        let staged = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&staged);
        let answering_op: StagingOp = Arc::new(move |_name: &str, _bytes: &[u8]| {
            counter.fetch_add(1, Ordering::SeqCst);
            Err(ClipboardStageFailure::WriteFailed)
        });
        let mut harness = ConnectionHarness::spawn(caps(false), answering_op);
        harness.send(&stage_frame(1, &png_payload()));
        harness.send(&input_frame("t1"));

        harness.expect_input("t1");
        harness.assert_no_stage_frame_within(Duration::from_millis(600));
        assert_eq!(
            staged.load(Ordering::SeqCst),
            0,
            "an ungated request reached the staging operation"
        );
        harness
            .finish()
            .expect("the connection stays up and ends cleanly");

        // Positive control: the identical exchange on a link that DID agree
        // the capability answers. Without it this test would also pass against
        // an implementation that never answers anybody.
        let refusing_op: StagingOp =
            Arc::new(|_name: &str, _bytes: &[u8]| Err(ClipboardStageFailure::WriteFailed));
        let mut harness = ConnectionHarness::spawn(caps(true), refusing_op);
        harness.send(&stage_frame(1, &png_payload()));
        harness.send(&input_frame("t1"));
        harness.expect_input("t1");
        assert!(
            matches!(
                harness.next_stage_response(),
                Some(ClipboardStageResponse::Failed { request_id: 1, .. })
            ),
            "a negotiated link must answer the same request"
        );
        harness.finish().expect("the connection ends cleanly");
    }

    /// The cap counts the request the worker is actively staging, not just the
    /// queued ones: a `sync_channel(2)` alone would admit a third payload and
    /// triple the memory a single controller can pin.
    #[test]
    fn a_third_concurrent_stage_request_on_one_connection_is_refused_as_busy() {
        let parked = parked_op();
        let mut harness = ConnectionHarness::spawn(caps(true), Arc::clone(&parked.op));

        harness.send(&stage_frame(1, &png_payload()));
        harness.send(&stage_frame(2, &png_payload()));
        // The first reaches the op; the second is admitted and waits behind it.
        assert_eq!(
            parked
                .entered
                .recv_timeout(Duration::from_secs(5))
                .expect("the first request reaches the staging operation"),
            "image.png"
        );

        harness.send(&stage_frame(3, &png_payload()));
        match harness.next_stage_response() {
            Some(ClipboardStageResponse::Failed {
                request_id,
                failure,
            }) => {
                assert_eq!(request_id, 3, "the third request is the refused one");
                assert_eq!(
                    failure,
                    ClipboardStageFailure::Busy,
                    "backpressure must not be reported as a disk failure"
                );
            }
            other => panic!("expected a Busy refusal, got {other:?}"),
        }

        // Release both admitted requests; their permits come back and a fourth
        // request is admitted again, proving the refusal was a live cap and
        // not a wedged connection.
        let _ = parked.release.send(());
        let _ = parked.release.send(());
        assert_eq!(
            parked
                .entered
                .recv_timeout(Duration::from_secs(5))
                .expect("the second admitted request reaches the operation"),
            "image.png"
        );
        let _ = parked.release.send(());

        harness.send(&stage_frame(4, &png_payload()));
        assert_eq!(
            parked
                .entered
                .recv_timeout(Duration::from_secs(5))
                .expect("a fourth request is admitted once permits are returned"),
            "image.png"
        );
        let _ = parked.release.send(());
        harness.finish().expect("the connection ends cleanly");
    }

    /// The permit is taken before the hand-off, so the hand-off's failure path
    /// is the one that leaks it. A permit released only inside the worker loop
    /// would never come back here, and the connection would answer `Busy` for
    /// the rest of its life with nothing actually staging.
    #[test]
    fn a_request_refused_by_a_full_staging_queue_still_returns_its_permit() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let first_cause = Arc::new(FirstCauseCell::new());
        let (out_tx, out_rx) = std_mpsc::sync_channel::<FederationMessage>(EGRESS_QUEUE_CAP);

        // A queue whose receiver is gone: every `try_send` fails, which is the
        // state a request meets when it arrives during teardown.
        let (jobs, jobs_rx) = std_mpsc::sync_channel::<StageJob>(MAX_CONCURRENT_STAGES);
        drop(jobs_rx);
        let live = Arc::new(AtomicUsize::new(0));
        let dead = StagingChannel {
            jobs,
            live: Arc::clone(&live),
        };
        handle_clipboard_stage_request(
            ClipboardStageRequest {
                request_id: 1,
                payload_base64: png_payload(),
                original_filename: "image.png".to_string(),
            },
            Some(&dead),
            &out_tx,
            &shutdown,
            &first_cause,
        );
        match out_rx.try_recv().expect("the refusal is answered") {
            FederationMessage::ClipboardStageResponse(ClipboardStageResponse::Failed {
                request_id,
                failure,
            }) => {
                assert_eq!(request_id, 1);
                assert_eq!(failure, ClipboardStageFailure::Busy);
            }
            other => panic!("expected a Busy refusal, got {other:?}"),
        }
        assert_eq!(
            live.load(Ordering::SeqCst),
            0,
            "the failed hand-off leaked its admission permit"
        );

        // The same counter, now with a live queue, still admits its full cap.
        let (jobs, jobs_rx) = std_mpsc::sync_channel::<StageJob>(MAX_CONCURRENT_STAGES);
        let alive = StagingChannel { jobs, live };
        for request_id in 2..=3 {
            handle_clipboard_stage_request(
                ClipboardStageRequest {
                    request_id,
                    payload_base64: png_payload(),
                    original_filename: "image.png".to_string(),
                },
                Some(&alive),
                &out_tx,
                &shutdown,
                &first_cause,
            );
        }
        assert!(
            out_rx.try_recv().is_err(),
            "both further requests must be admitted, not refused"
        );
        assert_eq!(
            jobs_rx.try_recv().map(|job| job.request.request_id).ok(),
            Some(2)
        );
        assert_eq!(
            jobs_rx.try_recv().map(|job| job.request.request_id).ok(),
            Some(3)
        );
    }

    /// The controller lease is released only when `run_connection` returns, so
    /// a join on a stuck filesystem call would pin this host's single
    /// controller slot and refuse every later mount.
    #[test]
    fn a_blocked_staging_operation_does_not_delay_connection_teardown() {
        let parked = parked_op();
        let mut harness = ConnectionHarness::spawn(caps(true), Arc::clone(&parked.op));
        harness.send(&stage_frame(1, &png_payload()));
        assert_eq!(
            parked
                .entered
                .recv_timeout(Duration::from_secs(5))
                .expect("the request reaches the staging operation"),
            "image.png"
        );

        // Teardown runs on its own thread and is waited for with a bound: an
        // implementation that joins the worker does not merely take longer,
        // it never returns, and a plain call here would hang the suite rather
        // than report a failure.
        let (done_tx, done) = std_mpsc::channel::<io::Result<()>>();
        std::thread::spawn(move || {
            let _ = done_tx.send(harness.finish());
        });
        done.recv_timeout(Duration::from_secs(5))
            .expect("teardown must not wait on the parked staging operation")
            .expect("the connection ends cleanly");

        // Releasing it afterwards must write nothing: the outbound handle was
        // revoked at teardown, so a late result is discarded.
        let _ = parked.release.send(());
        std::thread::sleep(Duration::from_millis(200));
    }

    /// Decode before any filesystem access: an undecodable payload is a peer
    /// error, not a disk error, and must not create anything on this host.
    #[test]
    fn a_malformed_base64_payload_is_reported_as_invalid_payload_without_touching_the_filesystem() {
        let touched = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&touched);
        let op: StagingOp = Arc::new(move |_name: &str, _bytes: &[u8]| {
            counter.fetch_add(1, Ordering::SeqCst);
            Err(ClipboardStageFailure::WriteFailed)
        });
        let mut harness = ConnectionHarness::spawn(caps(true), op);

        harness.send(&stage_frame(1, "!!!! not base64 !!!!"));
        match harness.next_stage_response() {
            Some(ClipboardStageResponse::Failed {
                request_id,
                failure,
            }) => {
                assert_eq!(request_id, 1);
                assert_eq!(failure, ClipboardStageFailure::InvalidPayload);
            }
            other => panic!("expected InvalidPayload, got {other:?}"),
        }
        assert_eq!(
            touched.load(Ordering::SeqCst),
            0,
            "a malformed payload reached the filesystem"
        );

        // Positive control: a decodable payload on the same connection does
        // reach the staging operation.
        harness.send(&stage_frame(2, &png_payload()));
        assert!(
            matches!(
                harness.next_stage_response(),
                Some(ClipboardStageResponse::Failed {
                    request_id: 2,
                    failure: ClipboardStageFailure::WriteFailed,
                })
            ),
            "a decodable payload must reach the staging operation"
        );
        assert_eq!(touched.load(Ordering::SeqCst), 1);
        harness.finish().expect("the connection ends cleanly");
    }

    /// Runs one job through `staging_worker_loop` on this thread against an
    /// egress queue pre-filled with `prefill` frames, and reports what the
    /// worker did: the first outbound frame past the prefill, and the exit
    /// cause it recorded.
    fn stage_one_job_with_egress_prefill(
        prefill: usize,
    ) -> (Option<FederationMessage>, Option<TunnelExit>, bool) {
        let (out_tx, out_rx) = std_mpsc::sync_channel::<FederationMessage>(EGRESS_QUEUE_CAP);
        for _ in 0..prefill {
            out_tx
                .try_send(input_frame("filler"))
                .expect("the egress queue accepts frames up to its capacity");
        }
        let out: RevocableOut = Arc::new(Mutex::new(Some(out_tx)));
        let shutdown = Arc::new(AtomicBool::new(false));
        let first_cause = Arc::new(FirstCauseCell::new());

        let (jobs, jobs_rx) = std_mpsc::sync_channel::<StageJob>(MAX_CONCURRENT_STAGES);
        let live = Arc::new(AtomicUsize::new(0));
        let permit = StagePermit::acquire(&live).expect("a permit is available");
        jobs.try_send(StageJob {
            request: ClipboardStageRequest {
                request_id: 7,
                payload_base64: png_payload(),
                original_filename: "image.png".to_string(),
            },
            permit,
        })
        .expect("the job queue accepts the request");
        // Dropping the sender ends the loop after the single job.
        drop(jobs);

        let op: StagingOp =
            Arc::new(|_name: &str, _bytes: &[u8]| Ok(PathBuf::from("/tmp/staged/image.png")));
        staging_worker_loop(jobs_rx, &out, &shutdown, &first_cause, op);

        for _ in 0..prefill {
            let _ = out_rx.try_recv();
        }
        (
            out_rx.try_recv().ok(),
            first_cause.get(),
            shutdown.load(Ordering::SeqCst),
        )
    }

    /// A successful stage answered into a full egress queue must surface as an
    /// overflow teardown, never be discarded. Dropping it is the worst failure
    /// this path has: the file is written on this host and the controller's
    /// pending entry is claimed, so nothing can retry and the controller can
    /// only report that this host never answered.
    #[test]
    fn a_stage_response_that_cannot_be_queued_tears_the_link_down_instead_of_vanishing() {
        let (frame, cause, shutdown) = stage_one_job_with_egress_prefill(EGRESS_QUEUE_CAP);
        assert!(
            frame.is_none(),
            "a full egress queue cannot have accepted the response"
        );
        assert_eq!(
            cause,
            Some(TunnelExit::EgressOverflow),
            "a dropped stage response must be recorded as an egress overflow"
        );
        assert!(shutdown, "an unqueueable response must signal teardown");

        // Positive control: the same fixture with room in the queue delivers
        // the response and records no fault. Without it this test would also
        // pass against a worker that tore the link down unconditionally.
        let (frame, cause, shutdown) = stage_one_job_with_egress_prefill(0);
        assert!(
            matches!(
                frame,
                Some(FederationMessage::ClipboardStageResponse(
                    ClipboardStageResponse::Staged { request_id: 7, .. }
                ))
            ),
            "a queue with room must deliver the staged response, got {frame:?}"
        );
        assert_eq!(cause, None, "a delivered response records no fault");
        assert!(!shutdown, "a delivered response must not signal teardown");
    }
}
