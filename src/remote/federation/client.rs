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

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::Mutex as AsyncMutex;

use crate::api::EventHub;

use super::id::{HostKey, Mount, ServerInstanceId};
use super::protocol::{
    Capability, FederationMessage, Handshake, HandshakeResponse, MountSnapshot, RejectReason,
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
    /// A `Gap`/`Reset` was observed; the caller must remount (a fresh
    /// `connect_and_mount` call over a *new* connection — the wire has no
    /// "request a fresh snapshot on this connection" message, so a full
    /// remount is the only re-sync primitive P1/P3 provide) and then call
    /// `RemoteMirror::reconcile_by_diff` with the new snapshot.
    ResyncRequired,
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
            let Some(FederationMessage::Handshake(_)) = read_frame(&mut server_reader).await.unwrap()
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

        assert!(matches!(err, MountError::Rejected(RejectReason::Version { .. })));
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
            drive_event_channel(&mut mounted.reader, &mut mounted.mirror, generation, &local_hub),
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

        let client = Arc::new(FederationClient::new(host_key(), BTreeSet::new(), BTreeSet::new()));
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
}
