//! Server-side federation protocol handler: handshake, atomic mount, ordered
//! event stream (with gap detection), raw terminal byte channels (tapped at
//! the `on_read` source via `tee.rs`), agent-status relay, and clipboard
//! forwarding.
//!
//! Generic over a `FederationHost` so the exact same handler drives both the
//! real remote (`herdr federation-serve`, `main.rs`) and the in-process
//! `LoopbackFederationServer` (`loopback.rs`) used by this phase's tests and
//! by P4-P9.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{broadcast, mpsc};

use crate::api::schema::common::AgentStatus;
use crate::api::schema::events::EventKind;
use crate::api::schema::session::SessionSnapshot;

use super::id::ServerInstanceId;
use super::protocol::codec;
use super::protocol::negotiate::negotiate;
use super::protocol::{
    Capability, Channel, EventChannelMessage, EventCursor, EventFrame, FederationMessage,
    Handshake, HandshakeResponse, MountSnapshot, ScrollbackReplay, TerminalChannelMessage,
    FEDERATION_PROTOCOL_VERSION,
};
use super::tee;

/// The single mount generation a `federation-serve` process assigns for its
/// own lifetime. This process serves exactly one connection/mount from boot
/// to shutdown, so there is only ever one live generation to fence against
/// (a fresh remote boot — and therefore a fresh `server_instance_id` — is
/// what a remount after a crash produces; see `id::Mount`/`id::fence`).
///
/// `#[allow(dead_code)]`: only `loopback.rs`'s tests drive `run()` (and
/// therefore this constant) now that the co-located `federation_accept.rs`
/// host owns production traffic — kept for those tests, not a real bin-build
/// dead end.
#[allow(dead_code)]
const MOUNT_GENERATION: u64 = 1;

/// How often the event-stream and agent-status pollers check their sources.
/// A poll (not a push/notify) keeps `FederationHost` a plain sync trait; the
/// interval is short enough that federation consumers do not perceive it as
/// added latency relative to human-typing/terminal-output cadences.
#[allow(dead_code)]
const POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Everything the protocol handler needs from the process hosting the actual
/// session (real `AppState`-backed in production, a fixture in tests). Plain
/// sync methods only — implementations that need interior locking use a
/// `std::sync::Mutex`, never held across an `.await`.
///
/// Deliberately NOT `Send + Sync`: the real production host wraps `App`
/// (`src/app/mod.rs`), which holds a `Box<dyn PrefixInputSource>` with no
/// `Send` bound and is therefore itself `!Send`. `run()` below never spawns
/// a separate task that needs to move a host reference across threads — the
/// event-stream and agent-status polling loops run inline in the same
/// future as the read loop — so this bound is never required. Only
/// `LoopbackFederationServer::spawn` (test-only, always instantiated with
/// `FixtureHost`) needs `Send + Sync`, and adds it locally.
///
/// `#[allow(dead_code)]`: only `loopback::FixtureHost` (test-only) implements
/// this trait now that `federation_accept.rs` hosts production traffic
/// without going through it — kept for those tests, not dead in the sense of
/// unreachable production code.
#[allow(dead_code)]
pub(crate) trait FederationHost: 'static {
    fn server_instance_id(&self) -> ServerInstanceId;
    fn capabilities(&self) -> BTreeSet<Capability>;

    /// Atomic mount: a `SessionSnapshot` and the `EventCursor` it is
    /// consistent with. Implementations must capture both under one
    /// exclusive borrow so no event can slip between them.
    fn mount(&self) -> (SessionSnapshot, EventCursor);

    /// Events strictly after `since`, oldest first. May start with a
    /// `source_seq` greater than `since + 1` if the underlying ring
    /// overflowed between polls — the caller (this module) detects that and
    /// emits `Gap`.
    fn events_after(&self, since: u64) -> Vec<(u64, EventKind)>;

    /// Subscribe to a terminal's raw-byte tee; `None` if the terminal id is
    /// unknown.
    fn subscribe_output(&self, terminal_id: &str) -> Option<broadcast::Receiver<Bytes>>;

    /// Bounded scrollback to replay when a channel opens (RT-F6). Empty if
    /// unavailable.
    fn scrollback_replay(&self, terminal_id: &str) -> Vec<u8>;

    fn send_input(&self, terminal_id: &str, bytes: &[u8]);
    fn resize(&self, terminal_id: &str, cols: u16, rows: u16);

    /// Snapshot of every known terminal's current agent status, polled and
    /// diffed by this module so only changes are sent over the wire.
    fn agent_statuses(&self) -> Vec<(String, AgentStatus)>;

    /// Drain and apply any pending internal `AppEvent`s (pane-content
    /// changed, agent state changed, clipboard writes, ...) so this host's
    /// `EventHub`/session state actually advances from live pane activity
    /// between mount and the next `events_after`/`agent_statuses` poll
    /// (GAP #1, `implementation-notes.md`). Default no-op: test hosts
    /// (`loopback::FixtureHost`) have no such channel to drain.
    fn drain_internal_events(&self) {}
}

/// Minimal, valid `SessionSnapshot` with no workspaces/tabs/panes. Used as
/// the fallback if `AppFederationHost::mount` cannot parse the JSON API's
/// `SessionSnapshot` response, and by `loopback::FixtureHost` (test-only) so
/// both hosts build the mount payload the same way — the federation
/// protocol carries it opaquely; terminal channels are addressed by
/// `terminal_id` string directly, independent of this snapshot's contents.
pub(crate) fn empty_snapshot() -> SessionSnapshot {
    SessionSnapshot {
        version: "0.0.0-unknown".to_string(),
        protocol: FEDERATION_PROTOCOL_VERSION,
        focused_workspace_id: None,
        focused_tab_id: None,
        focused_pane_id: None,
        workspaces: Vec::new(),
        tabs: Vec::new(),
        panes: Vec::new(),
        layouts: Vec::new(),
        agents: Vec::new(),
    }
}

/// The largest cap across every channel (`Channel::Clipboard`), used to
/// bound the read-side allocation before a frame's true channel (and
/// therefore its true cap) is known from the decoded message.
fn global_max_frame() -> usize {
    Channel::Clipboard.max_len()
}

/// `pub(crate)` (not just module-private) so `loopback.rs`'s tests can drive
/// the client side of a connection with the exact same framing this module
/// uses on the server side.
pub(crate) async fn read_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> std::io::Result<Option<FederationMessage>> {
    let mut header = [0u8; 8];
    if let Err(err) = reader.read_exact(&mut header).await {
        if err.kind() == std::io::ErrorKind::UnexpectedEof {
            return Ok(None);
        }
        return Err(err);
    }

    let claimed_len = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
    let max = global_max_frame();
    if claimed_len > max {
        return Err(std::io::Error::other(format!(
            "federation frame size {claimed_len} exceeds the largest channel cap {max}"
        )));
    }

    let mut frame = vec![0u8; 8 + claimed_len];
    frame[..8].copy_from_slice(&header);
    reader.read_exact(&mut frame[8..]).await?;

    let (msg, _consumed) = codec::decode::<FederationMessage>(&frame, max)
        .map_err(|err| std::io::Error::other(err.to_string()))?;

    // Defense in depth: re-check against the message's *own* channel cap now
    // that we know which channel it is, since the pre-decode check above
    // only bounds against the largest cap of any channel.
    if claimed_len > msg.channel().max_len() {
        return Err(std::io::Error::other(format!(
            "federation frame size {claimed_len} exceeds its channel's cap {}",
            msg.channel().max_len()
        )));
    }

    Ok(Some(msg))
}

pub(crate) async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    msg: &FederationMessage,
) -> std::io::Result<()> {
    let frame = codec::encode(msg).map_err(|err| std::io::Error::other(err.to_string()))?;
    writer.write_all(&frame).await?;
    writer.flush().await
}

/// Runs the federation protocol handler for one connection to completion
/// (until the peer disconnects or a fatal protocol error occurs). Historically
/// reused verbatim by the real `federation-serve` subcommand and by
/// `LoopbackFederationServer` (over an in-memory duplex); `federation-serve`
/// is now a transparent proxy to the co-located `federation_accept.rs` host
/// instead, so `loopback.rs`'s tests are the sole caller.
///
/// `#[allow(dead_code)]`: unreachable from production `main.rs` after that
/// change, kept for `loopback.rs`'s tests.
#[allow(dead_code)]
pub(crate) async fn run<R, W, H>(host: Arc<H>, mut reader: R, mut writer: W) -> std::io::Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
    H: FederationHost,
{
    let Some(FederationMessage::Handshake(remote_handshake)) = read_frame(&mut reader).await?
    else {
        return Err(std::io::Error::other(
            "federation link did not open with a Handshake",
        ));
    };

    let local_handshake = Handshake {
        federation_protocol_version: FEDERATION_PROTOCOL_VERSION,
        capabilities: host.capabilities(),
        server_instance_id: host.server_instance_id(),
    };

    let agreed = match negotiate(&local_handshake, &remote_handshake) {
        Ok(agreed) => agreed,
        Err(reason) => {
            write_frame(
                &mut writer,
                &FederationMessage::HandshakeResponse(HandshakeResponse::Reject { reason }),
            )
            .await?;
            return Ok(());
        }
    };
    write_frame(
        &mut writer,
        &FederationMessage::HandshakeResponse(HandshakeResponse::Accept {
            agreed_capabilities: agreed.0,
        }),
    )
    .await?;

    // Atomic mount, pushed unprompted right after a successful handshake —
    // there is exactly one mount per connection in this protocol.
    let (snapshot, cursor) = host.mount();
    write_frame(
        &mut writer,
        &FederationMessage::MountSnapshot(MountSnapshot {
            server_instance_id: host.server_instance_id(),
            snapshot,
            cursor,
        }),
    )
    .await?;

    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<FederationMessage>();
    let writer_task = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            if write_frame(&mut writer, &msg).await.is_err() {
                break;
            }
        }
    });

    let mut open_terminals: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();
    let mut event_cursor = cursor.0;
    let mut event_ticker = tokio::time::interval(POLL_INTERVAL);
    let mut agent_status_ticker = tokio::time::interval(POLL_INTERVAL.saturating_mul(4));
    let mut last_agent_statuses: HashMap<String, AgentStatus> = HashMap::new();

    // Everything below runs in this one future (never a separately spawned
    // task) so `H` never needs to be `Send`/`Sync` — see the trait doc
    // comment. `select!` interleaves the read loop with the two poll
    // tickers without giving up single-task ownership of `host`.
    let result = loop {
        tokio::select! {
            biased;
            frame = read_frame(&mut reader) => {
                match frame {
                    Ok(Some(msg)) => handle_inbound(&host, msg, &out_tx, &mut open_terminals),
                    Ok(None) => break Ok(()),
                    Err(err) => break Err(err),
                }
            }
            _ = event_ticker.tick() => {
                // Apply any pending internal `AppEvent`s BEFORE polling the
                // event hub for this tick, so state that just changed (a
                // pane's content, an agent's status, a clipboard write) is
                // visible to `events_after`/`agent_statuses` in the same
                // tick that produced it, not one tick later.
                host.drain_internal_events();
                if !poll_events(&*host, &mut event_cursor, &out_tx) {
                    break Ok(());
                }
            }
            _ = agent_status_ticker.tick() => {
                if !poll_agent_statuses(&*host, &mut last_agent_statuses, &out_tx) {
                    break Ok(());
                }
            }
        }
    };

    for (_, handle) in open_terminals.drain() {
        handle.abort();
    }
    drop(out_tx);
    let _ = writer_task.await;

    result
}

/// Polls `host.events_after` once, emitting `Gap`/`Frame` messages and
/// advancing `cursor`. Returns `false` if the outbound channel closed (the
/// writer task exited) and the caller should stop the connection.
///
/// `#[allow(dead_code)]`: only reachable through `run()`, kept for
/// `loopback.rs`'s tests (see `run`'s doc comment).
#[allow(dead_code)]
fn poll_events<H: FederationHost>(
    host: &H,
    cursor: &mut u64,
    out_tx: &mpsc::UnboundedSender<FederationMessage>,
) -> bool {
    let frames = host.events_after(*cursor);
    if frames.is_empty() {
        return true;
    }
    let first_seq = frames[0].0;
    if first_seq != *cursor + 1
        && out_tx
            .send(FederationMessage::Event(EventChannelMessage::Gap {
                from: *cursor,
                to: first_seq - 1,
            }))
            .is_err()
    {
        return false;
    }
    for (seq, kind) in &frames {
        if out_tx
            .send(FederationMessage::Event(EventChannelMessage::Frame(
                EventFrame {
                    source_seq: *seq,
                    kind: *kind,
                },
            )))
            .is_err()
        {
            return false;
        }
    }
    *cursor = frames.last().map(|(seq, _)| *seq).unwrap_or(*cursor);
    true
}

/// Polls `host.agent_statuses` once, emitting only the entries that changed
/// since the last poll. Returns `false` if the outbound channel closed.
///
/// `#[allow(dead_code)]`: only reachable through `run()`, kept for
/// `loopback.rs`'s tests (see `run`'s doc comment).
#[allow(dead_code)]
fn poll_agent_statuses<H: FederationHost>(
    host: &H,
    last: &mut HashMap<String, AgentStatus>,
    out_tx: &mpsc::UnboundedSender<FederationMessage>,
) -> bool {
    for (terminal_id, status) in host.agent_statuses() {
        if last.get(&terminal_id) == Some(&status) {
            continue;
        }
        last.insert(terminal_id.clone(), status);
        if out_tx
            .send(FederationMessage::AgentStatus(
                super::protocol::AgentStatusMessage {
                    terminal_id,
                    mount_generation: MOUNT_GENERATION,
                    status,
                },
            ))
            .is_err()
        {
            return false;
        }
    }
    true
}

/// `#[allow(dead_code)]`: only reachable through `run()`, kept for
/// `loopback.rs`'s tests (see `run`'s doc comment).
#[allow(dead_code)]
fn handle_inbound<H: FederationHost>(
    host: &Arc<H>,
    msg: FederationMessage,
    out_tx: &mpsc::UnboundedSender<FederationMessage>,
    open_terminals: &mut HashMap<String, tokio::task::JoinHandle<()>>,
) {
    let FederationMessage::Terminal(term_msg) = msg else {
        // Handshake/MountSnapshot/Event/AgentStatus are server->client only;
        // Clipboard forwarding is deferred (see implementation-notes.md).
        // Anything else inbound is simply not actionable here.
        return;
    };
    if term_msg.mount_generation() != MOUNT_GENERATION {
        // Stale traffic from a prior mount generation must never be routed.
        return;
    }

    match term_msg {
        TerminalChannelMessage::Open { terminal_id, .. } => {
            if open_terminals.contains_key(&terminal_id) {
                return;
            }
            let Some(rx) = host.subscribe_output(&terminal_id) else {
                return;
            };
            let replay = host.scrollback_replay(&terminal_id);
            let _ = out_tx.send(FederationMessage::Terminal(TerminalChannelMessage::Open {
                terminal_id: terminal_id.clone(),
                mount_generation: MOUNT_GENERATION,
                replay: ScrollbackReplay { bytes: replay },
            }));
            let handle = spawn_terminal_forward_task(terminal_id.clone(), rx, out_tx.clone());
            open_terminals.insert(terminal_id, handle);
        }
        TerminalChannelMessage::Input {
            terminal_id, bytes, ..
        } => host.send_input(&terminal_id, &bytes),
        TerminalChannelMessage::Resize {
            terminal_id,
            cols,
            rows,
            ..
        } => host.resize(&terminal_id, cols, rows),
        TerminalChannelMessage::Close { terminal_id, .. } => {
            if let Some(handle) = open_terminals.remove(&terminal_id) {
                handle.abort();
            }
        }
        // The server never receives `Output` — that variant is this
        // module's own outbound direction only.
        TerminalChannelMessage::Output { .. } => {}
    }
}

/// Forwards a terminal's tee onto the wire as `Output` frames, coalescing
/// bursts (`tee::drain_available`) so a flooding pane produces one frame per
/// poll tick rather than one per PTY read syscall (bounded-framing
/// backpressure: the tick interval, not an unbounded outbound queue, is what
/// paces a flooding pane).
///
/// `#[allow(dead_code)]`: only reachable through `run()`, kept for
/// `loopback.rs`'s tests (see `run`'s doc comment).
#[allow(dead_code)]
fn spawn_terminal_forward_task(
    terminal_id: String,
    mut rx: broadcast::Receiver<Bytes>,
    out_tx: mpsc::UnboundedSender<FederationMessage>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(first) => {
                    let mut bytes = first.to_vec();
                    let (rest, _lagged) = tee::drain_available(&mut rx);
                    bytes.extend_from_slice(&rest);
                    if out_tx
                        .send(FederationMessage::Terminal(TerminalChannelMessage::Output {
                            terminal_id: terminal_id.clone(),
                            mount_generation: MOUNT_GENERATION,
                            bytes,
                        }))
                        .is_err()
                    {
                        return;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    // The tee itself already caps memory (bounded ring); a
                    // lag here means bytes were dropped between the pane and
                    // this consumer. The event stream's Gap/Reset machinery
                    // covers state events; a lagged raw-byte tee simply
                    // resumes from the next available bytes (the client's
                    // terminal emulator, like a real terminal reconnecting
                    // mid-stream, may show a brief artifact but never
                    // corrupts framing).
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    })
}
