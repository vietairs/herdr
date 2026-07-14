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
//! Sub-brick 2 stops at the handshake: it proves the accept + negotiate +
//! server-identity wire before the mount + `select!`-equivalent command loop
//! (sub-brick 2c) is layered on. A connection is dropped after its handshake.
//!
//! Unix-only: gated at the module declaration (`server::mod`), mirroring the
//! federation socket fields it accepts on.

use std::collections::BTreeSet;
use std::io::{self, Read, Write};
use std::time::Duration;

use interprocess::local_socket::traits::{Listener as _, Stream as _};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, warn};

use crate::api::schema::session::SessionSnapshot;
use crate::ipc::{LocalListener, LocalStream};
use crate::remote::federation::id::ServerInstanceId;
use crate::remote::federation::protocol::codec;
use crate::remote::federation::protocol::negotiate::{negotiate, AgreedCaps};
use crate::remote::federation::protocol::{
    Capability, Channel, EventCursor, FederationMessage, Handshake, HandshakeResponse,
    MountSnapshot, FEDERATION_PROTOCOL_VERSION,
};
use crate::server::client_transport::ServerEvent;
use crate::server::federation_actor::FederationCommand;
use crate::server::federation_lease::{AcceptEpoch, Admission, ConnId};

/// How long a federation connection has to complete its handshake before the
/// server drops it, mirroring the thin-client path's `HANDSHAKE_TIMEOUT`. Keeps
/// a silent peer from pinning a handshake thread indefinitely.
const FEDERATION_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(4);

/// The capabilities the co-located federation host advertises. Mirrors
/// `AppFederationHost::capabilities` (`remote::federation::serve`); becomes the
/// sole definition once sub-brick 4 deletes that duplicate host.
fn federation_capabilities() -> BTreeSet<Capability> {
    [
        Capability::new(Capability::SCROLLBACK_REPLAY),
        Capability::new(Capability::AGENT_STATUS),
    ]
    .into_iter()
    .collect()
}

/// The largest cap across every channel, bounding a read-side allocation before
/// a frame's true channel (and cap) is known — mirrors `serve::global_max_frame`.
fn global_max_frame() -> usize {
    Channel::Clipboard.max_len()
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

    let claimed_len = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
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

    if drive_handshake(&mut stream, connid, server_instance_id)?.is_none() {
        debug!(connid, "federation handshake rejected or absent");
        return Ok(());
    }

    drive_mount(
        &mut stream,
        epoch,
        connid,
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
fn drive_mount<S: Read + Write>(
    stream: &mut S,
    epoch: AcceptEpoch,
    connid: ConnId,
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
            cursor,
        }),
    )?;

    // Sub-brick 2c-2 enters the command loop here (reader → SendInput/Resize,
    // writer draining events/output). 2c-1 closes after the snapshot; the guard
    // releases the lease on return.
    Ok(())
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
        .blocking_send(ServerEvent::Federation(FederationCommand::AcquireController {
            epoch,
            connid,
            reply,
        }))
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
        let _ = self
            .server_event_tx
            .blocking_send(ServerEvent::Federation(FederationCommand::Release {
                epoch: self.epoch,
                connid: self.connid,
            }));
    }
}

/// The stream-generic handshake exchange: read the peer's `Handshake`, negotiate
/// against this server's identity + capabilities, and reply `Accept`/`Reject`.
/// Returns the agreed capabilities on acceptance (sub-brick 2c continues into
/// mount with them), `None` on a rejection or a missing handshake. Generic over
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
        drive_mount(&mut server, 0, 1, &sid, &tx).expect("mount runs end to end");

        client_thread.join().expect("client thread");

        // drive_mount's guard already released the lease on return; dropping the
        // last sender ends the mock loop.
        drop(server);
        drop(tx);
        loop_handle.join().expect("mock loop joins");
    }
}
