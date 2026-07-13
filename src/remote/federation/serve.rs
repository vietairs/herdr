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
const MOUNT_GENERATION: u64 = 1;

/// How often the event-stream and agent-status pollers check their sources.
/// A poll (not a push/notify) keeps `FederationHost` a plain sync trait; the
/// interval is short enough that federation consumers do not perceive it as
/// added latency relative to human-typing/terminal-output cadences.
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
/// (until the peer disconnects or a fatal protocol error occurs). Reused
/// verbatim by the real `federation-serve` subcommand (over stdio, riding
/// `SshStdioBridge`) and by `LoopbackFederationServer` (over an in-memory
/// duplex).
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

// ---------------------------------------------------------------------------
// Production host: `herdr federation-serve` wired to a real, in-process
// `App`/`AppState`/`EventHub` (the counterpart to `loopback::FixtureHost`).
// ---------------------------------------------------------------------------

/// Deviation from the phase file's stated file list (logged in
/// `implementation-notes.md`): `App::session_snapshot`/`handle_event`
/// internals live in `app/*`, which this phase does not own. This host
/// deliberately stays on `App`'s already-`pub`/`pub(crate)` surface —
/// `handle_api_request` (JSON API, reused for `SessionSnapshot`/`AgentList`)
/// and the two small `pub(crate)` accessors added to `app/creation.rs` and
/// `pane.rs`/`terminal/runtime.rs` for the raw-byte tap — rather than adding
/// new private-module reach-ins.
pub(crate) struct AppFederationHost {
    server_instance_id: ServerInstanceId,
    event_hub: crate::api::EventHub,
    app: std::sync::Mutex<crate::app::App>,
}

impl AppFederationHost {
    /// Builds a fresh `App` the same way `server::headless::run_server`
    /// does (session persistence enabled, so a remote's existing
    /// workspaces/panes are what gets mounted), but does not bind the
    /// classic client/API unix sockets — this host speaks the federation
    /// protocol over the caller-supplied stdio instead, so it never
    /// conflicts with (and never touches) a classic `herdr server` process.
    pub(crate) fn boot() -> Self {
        let loaded_config = crate::config::Config::load();
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let no_session = false;
        let app = crate::app::App::new(
            &loaded_config.config,
            no_session,
            crate::config::config_diagnostic_summary(&loaded_config.diagnostics),
            api_rx,
            event_hub.clone(),
        );

        Self {
            server_instance_id: ServerInstanceId(fresh_server_instance_id()),
            event_hub,
            app: std::sync::Mutex::new(app),
        }
    }

    fn json_request(
        &self,
        method: crate::api::schema::Method,
    ) -> crate::api::schema::ResponseResult {
        let mut app = self.app.lock().expect("federation host app mutex poisoned");
        let response = app.handle_api_request(crate::api::schema::Request {
            id: "federation-serve".to_string(),
            method,
        });
        drop(app);
        serde_json::from_str::<crate::api::schema::SuccessResponse>(&response)
            .map(|success| success.result)
            .unwrap_or(crate::api::schema::ResponseResult::Ok {})
    }
}

fn fresh_server_instance_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    format!(
        "{}-{}-{}",
        std::process::id(),
        nanos,
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

impl FederationHost for AppFederationHost {
    fn server_instance_id(&self) -> ServerInstanceId {
        self.server_instance_id.clone()
    }

    fn capabilities(&self) -> BTreeSet<Capability> {
        [
            Capability::new(Capability::SCROLLBACK_REPLAY),
            Capability::new(Capability::AGENT_STATUS),
        ]
        .into_iter()
        .collect()
    }

    fn mount(&self) -> (SessionSnapshot, EventCursor) {
        // Hold the app lock across both the snapshot call and the cursor
        // read so no event can slip between them (RT-F2/CX-2 atomic pair).
        let mut app = self.app.lock().expect("federation host app mutex poisoned");
        let response = app.handle_api_request(crate::api::schema::Request {
            id: "federation-mount".to_string(),
            method: crate::api::schema::Method::SessionSnapshot(
                crate::api::schema::EmptyParams::default(),
            ),
        });
        let cursor = EventCursor(self.event_hub.current_sequence());
        drop(app);

        let snapshot = serde_json::from_str::<crate::api::schema::SuccessResponse>(&response)
            .ok()
            .and_then(|success| match success.result {
                crate::api::schema::ResponseResult::SessionSnapshot { snapshot } => {
                    Some(*snapshot)
                }
                _ => None,
            })
            .unwrap_or_else(empty_snapshot);

        (snapshot, cursor)
    }

    fn events_after(&self, since: u64) -> Vec<(u64, EventKind)> {
        self.event_hub
            .events_after(since)
            .into_iter()
            .map(|(seq, envelope)| (seq, envelope.event))
            .collect()
    }

    fn subscribe_output(&self, terminal_id: &str) -> Option<broadcast::Receiver<Bytes>> {
        let app = self.app.lock().expect("federation host app mutex poisoned");
        app.terminal_runtime_for_terminal_id(terminal_id)
            .map(|runtime| runtime.subscribe_output_bytes())
    }

    fn scrollback_replay(&self, terminal_id: &str) -> Vec<u8> {
        let app = self.app.lock().expect("federation host app mutex poisoned");
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

    fn send_input(&self, terminal_id: &str, bytes: &[u8]) {
        let app = self.app.lock().expect("federation host app mutex poisoned");
        if let Some(runtime) = app.terminal_runtime_for_terminal_id(terminal_id) {
            let _ = runtime.try_send_bytes(Bytes::copy_from_slice(bytes));
        }
    }

    fn resize(&self, terminal_id: &str, cols: u16, rows: u16) {
        let app = self.app.lock().expect("federation host app mutex poisoned");
        if let Some(runtime) = app.terminal_runtime_for_terminal_id(terminal_id) {
            runtime.resize(rows, cols, 0, 0);
        }
    }

    fn agent_statuses(&self) -> Vec<(String, AgentStatus)> {
        let result = self.json_request(crate::api::schema::Method::AgentList(
            crate::api::schema::EmptyParams::default(),
        ));
        match result {
            crate::api::schema::ResponseResult::AgentList { agents } => agents
                .into_iter()
                .map(|agent| (agent.terminal_id, agent.agent_status))
                .collect(),
            _ => Vec::new(),
        }
    }

    /// Closes GAP #1 (`implementation-notes.md`, P8 entry): drains `App`'s
    /// `event_rx` and applies each pending `AppEvent` via
    /// `App::handle_internal_event` — the same call `HeadlessServer::
    /// handle_internal_event_with_forwarding` (`server/headless.rs:1845`)
    /// makes, minus the client-forwarding side effects (sound/clipboard/
    /// prefix-input relayed to an attached terminal client), which do not
    /// apply here: `federation-serve` has no attached interactive client of
    /// its own, only federation-protocol subscribers reading `EventHub`/
    /// `agent_statuses`/the raw-byte tee, none of which this drain bypasses.
    /// Bounded per tick (not drained to exhaustion) so a burst of internal
    /// events cannot starve the read loop that shares this task via
    /// `tokio::select!`.
    fn drain_internal_events(&self) {
        const MAX_EVENTS_PER_DRAIN: usize = 256;
        let mut app = self.app.lock().expect("federation host app mutex poisoned");
        for _ in 0..MAX_EVENTS_PER_DRAIN {
            match app.event_rx.try_recv() {
                Ok(ev) => app.handle_internal_event(ev),
                Err(_) => break,
            }
        }
    }
}

/// Entry point for the `herdr federation-serve` subcommand (`main.rs`).
/// Boots a dedicated, session-persistence-enabled `App` (mirrors
/// `server::headless::run_server`'s construction) and speaks the federation
/// protocol over stdin/stdout — the same transport `remote-client-bridge`
/// rides over `SshStdioBridge`, so this subcommand needs no bespoke SSH
/// plumbing on the local side (P4 dials it exactly like the bridge).
pub(crate) fn run_federation_serve_over_stdio() -> std::io::Result<()> {
    crate::platform::raise_server_nofile_limit();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(std::io::Error::other)?;

    rt.block_on(async {
        // `AppFederationHost` is intentionally `!Send`/`!Sync` (see the
        // `FederationHost` trait doc comment) — this `Arc` is never handed
        // to `tokio::spawn`/another thread, only driven via `block_on` on
        // this one, which has no `Send` requirement.
        #[allow(clippy::arc_with_non_send_sync)]
        let host = Arc::new(AppFederationHost::boot());
        let stdin = tokio::io::stdin();
        let stdout = tokio::io::stdout();
        run(host, stdin, stdout).await
    })
}

#[cfg(test)]
mod gap1_app_event_drain_tests {
    //! Proves GAP #1 is closed: an `AppEvent` emitted by a real, in-process
    //! `App` (not a fixture stand-in) reaches the federation event stream
    //! over the actual `serve::run` protocol handler. `AppFederationHost` is
    //! `!Send`/`!Sync`, so — like production's `run_federation_serve_over_stdio`
    //! — this drives `run()` via `tokio::join!` on one `#[tokio::test]` task
    //! rather than `tokio::spawn`ing the host across threads.

    use super::*;
    use crate::api::EventHub;
    use crate::app::App;
    use crate::events::AppEvent;

    #[tokio::test]
    async fn a_real_app_event_reaches_the_federation_event_stream() {
        let event_hub = EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            true,
            None,
            api_rx,
            event_hub.clone(),
        );
        let event_tx = app.event_tx.clone();

        let mut workspace = crate::workspace::Workspace::test_new("gap1-drain");
        let dead_pane = workspace.test_split(ratatui::layout::Direction::Horizontal);
        app.state.workspaces = vec![workspace];
        app.state.ensure_test_terminals();

        // Same non-Send/Sync situation as `run_federation_serve_over_stdio`'s
        // own `Arc::new(AppFederationHost::boot())` — never handed to
        // `tokio::spawn`, only driven via `tokio::join!` on this one task.
        #[allow(clippy::arc_with_non_send_sync)]
        let host = Arc::new(AppFederationHost {
            server_instance_id: ServerInstanceId("gap1-test".to_string()),
            event_hub: event_hub.clone(),
            app: std::sync::Mutex::new(app),
        });

        let (client, server) = tokio::io::duplex(1 << 20);
        let (server_reader, server_writer) = tokio::io::split(server);
        let (mut client_reader, mut client_writer) = tokio::io::split(client);

        let run_fut = run(host, server_reader, server_writer);
        let drive_fut = async move {
            write_frame(
                &mut client_writer,
                &FederationMessage::Handshake(Handshake {
                    federation_protocol_version: FEDERATION_PROTOCOL_VERSION,
                    capabilities: BTreeSet::new(),
                    server_instance_id: ServerInstanceId("gap1-test-client".to_string()),
                }),
            )
            .await
            .unwrap();

            // Handshake accept, then the atomic mount — both server->client
            // only, unconditional on a successful negotiation.
            assert!(matches!(
                read_frame(&mut client_reader).await.unwrap(),
                Some(FederationMessage::HandshakeResponse(
                    HandshakeResponse::Accept { .. }
                ))
            ));
            assert!(matches!(
                read_frame(&mut client_reader).await.unwrap(),
                Some(FederationMessage::MountSnapshot(_))
            ));

            // Now that the mount is established, emit a real `AppEvent` the
            // same way local pane activity would — through `event_tx`, the
            // exact channel `App::handle_internal_event` normally only gets
            // drained from by `HeadlessServer::run`'s select loop
            // (`server/headless.rs:570`). Before GAP #1's fix, nothing drove
            // this channel inside `federation-serve`, so this event would
            // never reach `event_hub` and this test would time out below.
            event_tx
                .send(AppEvent::PaneDied {
                    pane_id: dead_pane,
                })
                .await
                .unwrap();

            // Poll the federation event stream (server-driven, `POLL_INTERVAL`
            // = 25ms) until the `pane.exited` frame produced by that event
            // arrives, or fail on timeout — proving the drain path is live.
            let saw_pane_exited = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    match read_frame(&mut client_reader).await.unwrap() {
                        Some(FederationMessage::Event(EventChannelMessage::Frame(frame))) => {
                            if frame.kind == EventKind::PaneExited {
                                return;
                            }
                        }
                        Some(_) => continue,
                        None => panic!("federation link closed before pane.exited arrived"),
                    }
                }
            })
            .await;
            assert!(
                saw_pane_exited.is_ok(),
                "pane.exited never reached the federation event stream (GAP #1 regressed)"
            );
        };

        let (run_result, ()) = tokio::join!(run_fut, drive_fut);
        assert!(run_result.is_ok());
    }
}
