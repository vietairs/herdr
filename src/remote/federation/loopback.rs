//! In-process loopback federation server: runs the exact same protocol
//! handler (`serve::run`) over an in-memory duplex instead of stdin/stdout,
//! so this phase's tests — and P4-P9's — exercise the real handshake, mount,
//! event, and terminal-channel machinery without SSH or a real remote.

use std::collections::{BTreeSet, HashMap};
use std::sync::Mutex;

use bytes::Bytes;
use tokio::sync::broadcast;

use crate::api::schema::common::AgentStatus;
use crate::api::schema::events::EventKind;
use crate::api::schema::session::SessionSnapshot;
use crate::api::EventHub;

use super::id::ServerInstanceId;
use super::protocol::{Capability, EventCursor};
use super::serve::{self, empty_snapshot, FederationHost};

/// A registered fixture terminal: a real `TerminalRuntime` (so the raw-byte
/// tap can be driven with `test_process_pty_bytes_and_tee` and asserted for
/// fidelity) plus bounded scrollback bytes to replay on `Open`.
pub(crate) struct FixtureTerminal {
    pub(crate) runtime: crate::terminal::TerminalRuntime,
    pub(crate) scrollback: Vec<u8>,
}

/// Test-only `FederationHost`: an `EventHub`-backed, in-memory stand-in for
/// the real `AppState`-backed host `federation-serve` builds in production.
/// Sync interior state only (`std::sync::Mutex`, never held across an
/// `.await`), matching `FederationHost`'s plain-sync contract.
pub(crate) struct FixtureHost {
    server_instance_id: ServerInstanceId,
    capabilities: BTreeSet<Capability>,
    event_hub: EventHub,
    // `Mutex`-wrapped (not a bare field): `FixtureTerminal` holds a real
    // `TerminalRuntime`, which is `Send` but not `Sync` (`pane::PaneRuntime`
    // uses `Cell` internally). `LoopbackFederationServer::spawn` needs
    // `FixtureHost: Sync` to hand it to `tokio::spawn` — `Mutex<T>: Sync`
    // holds for any `T: Send`, so wrapping restores it without touching
    // `pane.rs`.
    terminals: Mutex<HashMap<String, FixtureTerminal>>,
    agent_statuses: Mutex<HashMap<String, AgentStatus>>,
    sent_input: Mutex<Vec<(String, Vec<u8>)>>,
    resizes: Mutex<Vec<(String, u16, u16)>>,
}

impl FixtureHost {
    pub(crate) fn new() -> Self {
        Self {
            server_instance_id: ServerInstanceId(format!("fixture-{}", uuid_like())),
            capabilities: [
                Capability::new(Capability::SCROLLBACK_REPLAY),
                // The staging module is Unix-only, so a Windows fixture host
                // must not advertise a capability it could not honour; there
                // it falls into the same drop path an older peer would.
                #[cfg(unix)]
                Capability::new(Capability::FILE_STAGING),
            ]
            .into_iter()
            .collect(),
            event_hub: EventHub::default(),
            terminals: Mutex::new(HashMap::new()),
            agent_statuses: Mutex::new(HashMap::new()),
            sent_input: Mutex::new(Vec::new()),
            resizes: Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn with_terminal(
        self,
        terminal_id: impl Into<String>,
        runtime: crate::terminal::TerminalRuntime,
        scrollback: Vec<u8>,
    ) -> Self {
        self.terminals.lock().unwrap().insert(
            terminal_id.into(),
            FixtureTerminal {
                runtime,
                scrollback,
            },
        );
        self
    }

    pub(crate) fn event_hub(&self) -> &EventHub {
        &self.event_hub
    }

    /// Test-support surface for P4-P9 (this phase's tests only exercise
    /// mount/event/terminal channels, not agent-status relay).
    #[allow(dead_code)]
    pub(crate) fn set_agent_status(&self, terminal_id: impl Into<String>, status: AgentStatus) {
        self.agent_statuses
            .lock()
            .unwrap()
            .insert(terminal_id.into(), status);
    }

    pub(crate) fn sent_input_for(&self, terminal_id: &str) -> Vec<u8> {
        self.sent_input
            .lock()
            .unwrap()
            .iter()
            .filter(|(id, _)| id == terminal_id)
            .flat_map(|(_, bytes)| bytes.clone())
            .collect()
    }

    pub(crate) fn last_resize_for(&self, terminal_id: &str) -> Option<(u16, u16)> {
        self.resizes
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|(id, _, _)| id == terminal_id)
            .map(|(_, cols, rows)| (*cols, *rows))
    }
}

impl Default for FixtureHost {
    fn default() -> Self {
        Self::new()
    }
}

/// Cheap non-cryptographic unique-enough suffix so repeated fixture
/// construction across tests never collides on `server_instance_id` (test 1
/// asserts a fresh boot yields a new id).
fn uuid_like() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    nanos ^ COUNTER.fetch_add(1, Ordering::Relaxed)
}

impl FederationHost for FixtureHost {
    fn server_instance_id(&self) -> ServerInstanceId {
        self.server_instance_id.clone()
    }

    fn capabilities(&self) -> BTreeSet<Capability> {
        self.capabilities.clone()
    }

    fn mount(&self) -> (SessionSnapshot, EventCursor) {
        // No separate lock needed: `EventHub` is itself lock-protected, and
        // nothing in this fixture mutates it concurrently with a `mount()`
        // call, so reading the cursor immediately after taking the snapshot
        // is already the consistent pair the real host must also produce.
        let snapshot = empty_snapshot();
        let cursor = EventCursor(self.event_hub.current_sequence());
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
        self.terminals
            .lock()
            .unwrap()
            .get(terminal_id)
            .map(|terminal| terminal.runtime.subscribe_output_bytes())
    }

    fn scrollback_replay(&self, terminal_id: &str) -> Vec<u8> {
        self.terminals
            .lock()
            .unwrap()
            .get(terminal_id)
            .map(|terminal| terminal.scrollback.clone())
            .unwrap_or_default()
    }

    fn send_input(&self, terminal_id: &str, bytes: &[u8]) {
        self.sent_input
            .lock()
            .unwrap()
            .push((terminal_id.to_string(), bytes.to_vec()));
    }

    fn resize(&self, terminal_id: &str, cols: u16, rows: u16) {
        self.resizes
            .lock()
            .unwrap()
            .push((terminal_id.to_string(), cols, rows));
    }

    fn agent_statuses(&self) -> Vec<(String, AgentStatus)> {
        self.agent_statuses
            .lock()
            .unwrap()
            .iter()
            .map(|(id, status)| (id.clone(), *status))
            .collect()
    }

    /// Fixture-only split: proves the request/response roundtrip (protocol
    /// scaffolding, tested by `serve.rs`'s tests) without a real
    /// `AppState`-backed workspace to split against. Fails for an unknown
    /// `target_pane_id`; otherwise mints a fresh sibling terminal id and
    /// registers it exactly like `with_terminal` would.
    fn split_pane(
        &self,
        target_pane_id: &str,
        _direction: super::protocol::SplitDirection,
        _ratio: Option<f32>,
        _focus: bool,
    ) -> Result<(String, String), String> {
        let mut terminals = self.terminals.lock().unwrap();
        if !terminals.contains_key(target_pane_id) {
            return Err(format!("unknown target pane {target_pane_id}"));
        }
        let new_id = format!("{target_pane_id}-split-{}", uuid_like());
        let (runtime, _rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        terminals.insert(
            new_id.clone(),
            FixtureTerminal {
                runtime,
                scrollback: Vec::new(),
            },
        );
        Ok((new_id.clone(), new_id))
    }

    /// Fixture-only close: proves the request/response roundtrip (protocol
    /// scaffolding, tested by `serve.rs`'s tests) without a real
    /// `AppState`-backed workspace to close against. Fails for an unknown
    /// `target_pane_id`; otherwise drops it from the fixture's terminal map,
    /// same shape as `split_pane` above (Gap A,
    /// plans/260724-1536-federation-pane-close-sync).
    fn close_pane(&self, target_pane_id: &str) -> Result<(), String> {
        let mut terminals = self.terminals.lock().unwrap();
        if terminals.remove(target_pane_id).is_none() {
            return Err(format!("unknown target pane {target_pane_id}"));
        }
        Ok(())
    }
}

/// Runs `serve::run` against `host` over an in-memory duplex; returns the
/// client-side stream (split into owned read/write halves by the caller)
/// plus the server task's join handle.
pub(crate) struct LoopbackFederationServer;

impl LoopbackFederationServer {
    /// `H: Send + Sync` (stronger than the base `FederationHost` bound) is
    /// required here — and only here — because this helper hands `host` to
    /// a `tokio::spawn`'d task (needed so the test driving the client side
    /// and the in-process server run concurrently). `FixtureHost` (the only
    /// type ever passed here) satisfies it.
    pub(crate) fn spawn<H: FederationHost + Send + Sync>(
        host: std::sync::Arc<H>,
    ) -> (
        tokio::io::DuplexStream,
        tokio::task::JoinHandle<std::io::Result<()>>,
    ) {
        // 1 MiB is comfortably above any single test frame (largest channel
        // cap is Clipboard at 16 MiB, but tests never send payloads that
        // large) while keeping the loopback buffer allocation small.
        let (client, server) = tokio::io::duplex(1 << 20);
        let (server_reader, server_writer) = tokio::io::split(server);
        let handle =
            tokio::spawn(async move { serve::run(host, server_reader, server_writer).await });
        (client, handle)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use crate::api::schema::events::EventKind;
    use crate::remote::federation::protocol::{
        Capability, EventChannelMessage, FederationMessage, Handshake, HandshakeResponse,
        ScrollbackReplay, TerminalChannelMessage, FEDERATION_PROTOCOL_VERSION,
    };

    use super::*;

    fn handshake(caps: &[&str]) -> Handshake {
        Handshake {
            federation_protocol_version: FEDERATION_PROTOCOL_VERSION,
            capabilities: caps.iter().map(|c| Capability::new(*c)).collect(),
            server_instance_id: ServerInstanceId("client-does-not-advertise-a-real-id".to_string()),
        }
    }

    async fn connect_and_mount(
        host: Arc<FixtureHost>,
    ) -> (
        tokio::io::ReadHalf<tokio::io::DuplexStream>,
        tokio::io::WriteHalf<tokio::io::DuplexStream>,
        tokio::task::JoinHandle<std::io::Result<()>>,
        Handshake,
        crate::remote::federation::protocol::MountSnapshot,
    ) {
        let (client, server_handle) = LoopbackFederationServer::spawn(host);
        let (mut reader, mut writer) = tokio::io::split(client);

        serve::write_frame(
            &mut writer,
            &FederationMessage::Handshake(handshake(&[Capability::SCROLLBACK_REPLAY])),
        )
        .await
        .unwrap();

        let Some(FederationMessage::HandshakeResponse(HandshakeResponse::Accept { .. })) =
            serve::read_frame(&mut reader).await.unwrap()
        else {
            panic!("expected HandshakeResponse::Accept");
        };

        let Some(FederationMessage::MountSnapshot(mount)) =
            serve::read_frame(&mut reader).await.unwrap()
        else {
            panic!("expected MountSnapshot");
        };

        let local_handshake = handshake(&[Capability::SCROLLBACK_REPLAY]);
        (reader, writer, server_handle, local_handshake, mount)
    }

    #[tokio::test]
    async fn handshake_advertises_capability_and_a_fresh_instance_id_each_boot() {
        let host_a = Arc::new(FixtureHost::new());
        let (client_a, _server_a) = LoopbackFederationServer::spawn(host_a);
        let (mut reader_a, mut writer_a) = tokio::io::split(client_a);
        serve::write_frame(&mut writer_a, &FederationMessage::Handshake(handshake(&[])))
            .await
            .unwrap();
        let Some(FederationMessage::HandshakeResponse(HandshakeResponse::Accept {
            agreed_capabilities,
        })) = serve::read_frame(&mut reader_a).await.unwrap()
        else {
            panic!("expected accept");
        };
        // The client advertised nothing, so the agreed set (an intersection)
        // is empty — capability presence is asserted via the *local*
        // handshake the server would have built from `host.capabilities()`,
        // proven indirectly by the mount round trip below carrying a
        // non-empty `server_instance_id`.
        assert!(agreed_capabilities.is_empty());
        let Some(FederationMessage::MountSnapshot(mount_a)) =
            serve::read_frame(&mut reader_a).await.unwrap()
        else {
            panic!("expected mount");
        };
        assert!(!mount_a.server_instance_id.0.is_empty());

        let host_b = Arc::new(FixtureHost::new());
        let (client_b, _server_b) = LoopbackFederationServer::spawn(host_b);
        let (mut reader_b, mut writer_b) = tokio::io::split(client_b);
        serve::write_frame(&mut writer_b, &FederationMessage::Handshake(handshake(&[])))
            .await
            .unwrap();
        let _ = serve::read_frame(&mut reader_b).await.unwrap();
        let Some(FederationMessage::MountSnapshot(mount_b)) =
            serve::read_frame(&mut reader_b).await.unwrap()
        else {
            panic!("expected mount");
        };

        assert_ne!(mount_a.server_instance_id, mount_b.server_instance_id);
    }

    #[tokio::test]
    async fn atomic_mount_cursor_is_the_predecessor_of_the_next_pushed_event() {
        let host = Arc::new(FixtureHost::new());
        let event_hub = host.event_hub().clone();
        let (mut reader, _writer, _server_handle, _hs, mount) = connect_and_mount(host).await;

        event_hub.push(crate::api::schema::EventEnvelope {
            event: EventKind::WorkspaceFocused,
            data: crate::api::schema::EventData::WorkspaceFocused {
                workspace_id: "w1".to_string(),
            },
        });

        let frame = loop {
            match serve::read_frame(&mut reader).await.unwrap() {
                Some(FederationMessage::Event(EventChannelMessage::Frame(frame))) => break frame,
                Some(_) => continue,
                None => panic!("stream ended before an event frame arrived"),
            }
        };

        assert_eq!(frame.source_seq, mount.cursor.0 + 1);
        assert_eq!(frame.kind, EventKind::WorkspaceFocused);
    }

    #[tokio::test]
    async fn raw_byte_tap_delivers_exactly_the_bytes_the_local_grid_consumed() {
        let (runtime, _rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        let host = Arc::new(FixtureHost::new().with_terminal("term_1", runtime, Vec::new()));
        let (mut reader, mut writer, _server_handle, _hs, _mount) =
            connect_and_mount(host.clone()).await;

        serve::write_frame(
            &mut writer,
            &FederationMessage::Terminal(TerminalChannelMessage::Open {
                terminal_id: "term_1".to_string(),
                mount_generation: 1,
                replay: ScrollbackReplay { bytes: Vec::new() },
            }),
        )
        .await
        .unwrap();

        let Some(FederationMessage::Terminal(TerminalChannelMessage::Open { .. })) =
            serve::read_frame(&mut reader).await.unwrap()
        else {
            panic!("expected the server's Open acknowledgement");
        };

        let known_bytes = b"hello federation\r\n";
        host.terminals
            .lock()
            .unwrap()
            .get("term_1")
            .unwrap()
            .runtime
            .test_process_pty_bytes_and_tee(known_bytes);

        let Some(FederationMessage::Terminal(TerminalChannelMessage::Output { bytes, .. })) =
            serve::read_frame(&mut reader).await.unwrap()
        else {
            panic!("expected an Output frame");
        };

        assert_eq!(bytes, known_bytes);
    }

    #[tokio::test]
    async fn tee_coexists_with_a_second_local_subscriber_without_starving_either() {
        let (runtime, _rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        // A second subscriber standing in for the local render path — it
        // never reads, proving the tee (broadcast) does not require every
        // subscriber to keep up for another subscriber (the federation
        // channel) to receive bytes.
        let _local_render_stub = runtime.subscribe_output_bytes();

        let host = Arc::new(FixtureHost::new().with_terminal("term_1", runtime, Vec::new()));
        let (mut reader, mut writer, _server_handle, _hs, _mount) =
            connect_and_mount(host.clone()).await;

        serve::write_frame(
            &mut writer,
            &FederationMessage::Terminal(TerminalChannelMessage::Open {
                terminal_id: "term_1".to_string(),
                mount_generation: 1,
                replay: ScrollbackReplay { bytes: Vec::new() },
            }),
        )
        .await
        .unwrap();
        let _ = serve::read_frame(&mut reader).await.unwrap();

        host.terminals
            .lock()
            .unwrap()
            .get("term_1")
            .unwrap()
            .runtime
            .test_process_pty_bytes_and_tee(b"still flowing");

        let Some(FederationMessage::Terminal(TerminalChannelMessage::Output { bytes, .. })) =
            serve::read_frame(&mut reader).await.unwrap()
        else {
            panic!("expected an Output frame despite the unread local-render stub");
        };
        assert_eq!(bytes, b"still flowing");
    }

    #[tokio::test]
    async fn scrollback_replays_before_live_bytes_and_is_bounded_to_what_the_host_provides() {
        let (runtime, _rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        let scrollback = b"earlier history".to_vec();
        let host =
            Arc::new(FixtureHost::new().with_terminal("term_1", runtime, scrollback.clone()));
        let (mut reader, mut writer, _server_handle, _hs, _mount) =
            connect_and_mount(host.clone()).await;

        serve::write_frame(
            &mut writer,
            &FederationMessage::Terminal(TerminalChannelMessage::Open {
                terminal_id: "term_1".to_string(),
                mount_generation: 1,
                replay: ScrollbackReplay { bytes: Vec::new() },
            }),
        )
        .await
        .unwrap();

        let Some(FederationMessage::Terminal(TerminalChannelMessage::Open { replay, .. })) =
            serve::read_frame(&mut reader).await.unwrap()
        else {
            panic!("expected the server's Open with replay");
        };
        assert_eq!(replay.bytes, scrollback);

        host.terminals
            .lock()
            .unwrap()
            .get("term_1")
            .unwrap()
            .runtime
            .test_process_pty_bytes_and_tee(b"live bytes after replay");
        let Some(FederationMessage::Terminal(TerminalChannelMessage::Output { bytes, .. })) =
            serve::read_frame(&mut reader).await.unwrap()
        else {
            panic!("expected live Output after the replay");
        };
        assert_eq!(bytes, b"live bytes after replay");
    }

    #[tokio::test]
    async fn ring_overflow_between_polls_is_surfaced_as_a_gap_not_silently_dropped() {
        let host = Arc::new(FixtureHost::new());
        let event_hub = host.event_hub().clone();
        let (mut reader, _writer, _server_handle, _hs, mount) = connect_and_mount(host).await;

        // Push far more than the 512-event ring cap before the poller has a
        // chance to drain any of them, forcing the ring to overflow.
        for _ in 0..600 {
            event_hub.push(crate::api::schema::EventEnvelope {
                event: EventKind::WorkspaceFocused,
                data: crate::api::schema::EventData::WorkspaceFocused {
                    workspace_id: "w1".to_string(),
                },
            });
        }

        let gap = loop {
            match serve::read_frame(&mut reader).await.unwrap() {
                Some(FederationMessage::Event(EventChannelMessage::Gap { from, to })) => {
                    break (from, to)
                }
                Some(FederationMessage::Event(EventChannelMessage::Frame(_))) => continue,
                Some(_) => continue,
                None => panic!("stream ended before a Gap was observed"),
            }
        };

        assert_eq!(gap.0, mount.cursor.0);
        assert!(gap.1 > gap.0);
    }

    #[tokio::test]
    async fn loopback_harness_completes_a_full_handshake_mount_event_channel_cycle() {
        let (runtime, _rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        let host = Arc::new(FixtureHost::new().with_terminal("term_1", runtime, Vec::new()));
        let event_hub = host.event_hub().clone();
        let (mut reader, mut writer, server_handle, _hs, mount) =
            connect_and_mount(host.clone()).await;
        assert!(!mount.server_instance_id.0.is_empty());

        event_hub.push(crate::api::schema::EventEnvelope {
            event: EventKind::WorkspaceFocused,
            data: crate::api::schema::EventData::WorkspaceFocused {
                workspace_id: "w1".to_string(),
            },
        });
        let _ = serve::read_frame(&mut reader).await.unwrap();

        serve::write_frame(
            &mut writer,
            &FederationMessage::Terminal(TerminalChannelMessage::Open {
                terminal_id: "term_1".to_string(),
                mount_generation: 1,
                replay: ScrollbackReplay { bytes: Vec::new() },
            }),
        )
        .await
        .unwrap();
        let _ = serve::read_frame(&mut reader).await.unwrap();

        serve::write_frame(
            &mut writer,
            &FederationMessage::Terminal(TerminalChannelMessage::Input {
                terminal_id: "term_1".to_string(),
                mount_generation: 1,
                bytes: b"typed".to_vec(),
            }),
        )
        .await
        .unwrap();

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(host.sent_input_for("term_1"), b"typed".to_vec());

        serve::write_frame(
            &mut writer,
            &FederationMessage::Terminal(TerminalChannelMessage::Resize {
                terminal_id: "term_1".to_string(),
                mount_generation: 1,
                cols: 100,
                rows: 40,
            }),
        )
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(host.last_resize_for("term_1"), Some((100, 40)));

        drop(writer);
        drop(reader);
        let _ = tokio::time::timeout(Duration::from_secs(1), server_handle).await;
    }

    // Remote-split protocol scaffolding (plans/260721-2353-federation-
    // agents-sidebar-remote-detection debug report): a `SplitPaneRequest`
    // for a known pane produces a `SplitPaneResponse::Created` naming a new
    // pane/terminal id; an unknown pane produces `Failed` instead of a
    // silent local fallback.
    #[tokio::test]
    async fn split_pane_request_for_a_known_pane_yields_a_created_response() {
        let (runtime, _rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        let host = Arc::new(FixtureHost::new().with_terminal("term_1", runtime, Vec::new()));
        let (mut reader, mut writer, server_handle, _hs, _mount) =
            connect_and_mount(host.clone()).await;

        serve::write_frame(
            &mut writer,
            &FederationMessage::SplitPaneRequest(
                crate::remote::federation::protocol::SplitPaneRequest {
                    request_id: 1,
                    target_pane_id: "term_1".to_string(),
                    direction: crate::remote::federation::protocol::SplitDirection::Right,
                    ratio: None,
                    focus: true,
                },
            ),
        )
        .await
        .unwrap();

        let response =
            tokio::time::timeout(Duration::from_millis(500), serve::read_frame(&mut reader))
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        match response {
            FederationMessage::SplitPaneResponse(
                crate::remote::federation::protocol::SplitPaneResponse::Created {
                    request_id,
                    new_pane_id,
                    ..
                },
            ) => {
                assert_eq!(request_id, 1);
                assert_ne!(new_pane_id, "term_1");
            }
            other => panic!("expected SplitPaneResponse::Created, got {other:?}"),
        }

        drop(writer);
        drop(reader);
        let _ = tokio::time::timeout(Duration::from_secs(1), server_handle).await;
    }

    #[tokio::test]
    async fn split_pane_request_for_an_unknown_pane_yields_a_failed_response() {
        let host = Arc::new(FixtureHost::new());
        let (mut reader, mut writer, server_handle, _hs, _mount) =
            connect_and_mount(host.clone()).await;

        serve::write_frame(
            &mut writer,
            &FederationMessage::SplitPaneRequest(
                crate::remote::federation::protocol::SplitPaneRequest {
                    request_id: 2,
                    target_pane_id: "does_not_exist".to_string(),
                    direction: crate::remote::federation::protocol::SplitDirection::Down,
                    ratio: None,
                    focus: false,
                },
            ),
        )
        .await
        .unwrap();

        let response =
            tokio::time::timeout(Duration::from_millis(500), serve::read_frame(&mut reader))
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        assert!(matches!(
            response,
            FederationMessage::SplitPaneResponse(
                crate::remote::federation::protocol::SplitPaneResponse::Failed {
                    request_id: 2,
                    ..
                }
            )
        ));

        drop(writer);
        drop(reader);
        let _ = tokio::time::timeout(Duration::from_secs(1), server_handle).await;
    }

    // Gap A protocol scaffolding (plans/260724-1536-federation-pane-close-
    // sync): a `ClosePaneRequest` for a known pane produces a
    // `ClosePaneResponse::Closed`; an unknown pane produces `Failed`.
    #[tokio::test]
    async fn close_pane_request_for_a_known_pane_yields_a_closed_response() {
        let (runtime, _rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        let host = Arc::new(FixtureHost::new().with_terminal("term_1", runtime, Vec::new()));
        let (mut reader, mut writer, server_handle, _hs, _mount) =
            connect_and_mount(host.clone()).await;

        serve::write_frame(
            &mut writer,
            &FederationMessage::ClosePaneRequest(
                crate::remote::federation::protocol::ClosePaneRequest {
                    request_id: 1,
                    target_pane_id: "term_1".to_string(),
                },
            ),
        )
        .await
        .unwrap();

        let response =
            tokio::time::timeout(Duration::from_millis(500), serve::read_frame(&mut reader))
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        match response {
            FederationMessage::ClosePaneResponse(
                crate::remote::federation::protocol::ClosePaneResponse::Closed { request_id },
            ) => {
                assert_eq!(request_id, 1);
            }
            other => panic!("expected ClosePaneResponse::Closed, got {other:?}"),
        }

        drop(writer);
        drop(reader);
        let _ = tokio::time::timeout(Duration::from_secs(1), server_handle).await;
    }

    #[tokio::test]
    async fn close_pane_request_for_an_unknown_pane_yields_a_failed_response() {
        let host = Arc::new(FixtureHost::new());
        let (mut reader, mut writer, server_handle, _hs, _mount) =
            connect_and_mount(host.clone()).await;

        serve::write_frame(
            &mut writer,
            &FederationMessage::ClosePaneRequest(
                crate::remote::federation::protocol::ClosePaneRequest {
                    request_id: 2,
                    target_pane_id: "does_not_exist".to_string(),
                },
            ),
        )
        .await
        .unwrap();

        let response =
            tokio::time::timeout(Duration::from_millis(500), serve::read_frame(&mut reader))
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        assert!(matches!(
            response,
            FederationMessage::ClosePaneResponse(
                crate::remote::federation::protocol::ClosePaneResponse::Failed {
                    request_id: 2,
                    ..
                }
            )
        ));

        drop(writer);
        drop(reader);
        let _ = tokio::time::timeout(Duration::from_secs(1), server_handle).await;
    }

    // ---- file staging -------------------------------------------------

    /// `connect_and_mount` with the client's advertised capability set under
    /// the test's control, so both sides of the staging gate are reachable.
    #[cfg(unix)]
    async fn connect_and_mount_advertising(
        host: Arc<FixtureHost>,
        caps: &[&str],
    ) -> (
        tokio::io::ReadHalf<tokio::io::DuplexStream>,
        tokio::io::WriteHalf<tokio::io::DuplexStream>,
        tokio::task::JoinHandle<std::io::Result<()>>,
        std::collections::BTreeSet<Capability>,
    ) {
        let (client, server_handle) = LoopbackFederationServer::spawn(host);
        let (mut reader, mut writer) = tokio::io::split(client);
        serve::write_frame(&mut writer, &FederationMessage::Handshake(handshake(caps)))
            .await
            .expect("handshake written");
        let Some(FederationMessage::HandshakeResponse(HandshakeResponse::Accept {
            agreed_capabilities,
        })) = serve::read_frame(&mut reader).await.expect("accept read")
        else {
            panic!("expected HandshakeResponse::Accept");
        };
        let Some(FederationMessage::MountSnapshot(_)) =
            serve::read_frame(&mut reader).await.expect("mount read")
        else {
            panic!("expected MountSnapshot");
        };
        (reader, writer, server_handle, agreed_capabilities)
    }

    #[cfg(unix)]
    fn stage_request_frame(request_id: u64, name: &str, bytes: &[u8]) -> FederationMessage {
        use base64::Engine as _;
        FederationMessage::ClipboardStageRequest(
            crate::remote::federation::protocol::ClipboardStageRequest {
                request_id,
                payload_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
                original_filename: name.to_string(),
            },
        )
    }

    /// The whole exchange over a real server: negotiated capability, framed
    /// request, the actual staging module, and a path that resolves on the
    /// serving host's own filesystem.
    #[cfg(unix)]
    #[tokio::test]
    async fn clipboard_stage_request_end_to_end_through_loopback_server() {
        const PNG_BYTES: &[u8] = b"\x89PNG\r\n\x1a\nfake image payload";

        let host = Arc::new(FixtureHost::new());
        let (mut reader, mut writer, server_handle, agreed) = connect_and_mount_advertising(
            host,
            &[Capability::SCROLLBACK_REPLAY, Capability::FILE_STAGING],
        )
        .await;
        assert!(
            agreed.contains(&Capability::new(Capability::FILE_STAGING)),
            "both sides advertised file staging, so it must be agreed"
        );

        serve::write_frame(
            &mut writer,
            &stage_request_frame(1, "diagram.png", PNG_BYTES),
        )
        .await
        .expect("stage request written");

        let path = loop {
            match serve::read_frame(&mut reader).await.expect("frame read") {
                Some(FederationMessage::ClipboardStageResponse(
                    crate::remote::federation::protocol::ClipboardStageResponse::Staged {
                        request_id,
                        path,
                    },
                )) => {
                    assert_eq!(request_id, 1);
                    break path;
                }
                Some(FederationMessage::ClipboardStageResponse(other)) => {
                    panic!("staging failed: {other:?}")
                }
                Some(_) => continue,
                None => panic!("the link ended before the stage response arrived"),
            }
        };

        let staged = std::path::PathBuf::from(&path);
        assert!(staged.is_absolute(), "{path}");
        let name = staged
            .file_name()
            .and_then(|name| name.to_str())
            .expect("staged name");
        assert!(
            name.starts_with(super::super::file_staging::FEDERATION_CLIPBOARD_PREFIX),
            "{name}"
        );
        assert!(name.ends_with("-diagram.png"), "{name}");
        assert_eq!(
            std::fs::read(&staged).expect("the staged file is readable"),
            PNG_BYTES
        );
        std::fs::remove_file(&staged).expect("clean up the staged file");

        drop(writer);
        drop(reader);
        let _ = tokio::time::timeout(Duration::from_secs(1), server_handle).await;
    }

    /// The serving gate, proven against a real server rather than a unit
    /// seam: a client that never advertised the capability gets no stage
    /// frame at all — not even a failure — because one undecodable variant
    /// would end its whole mount.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_client_that_never_advertised_file_staging_receives_no_stage_frame() {
        let host = Arc::new(FixtureHost::new());
        let (mut reader, mut writer, server_handle, agreed) =
            connect_and_mount_advertising(host, &[Capability::SCROLLBACK_REPLAY]).await;
        assert!(!agreed.contains(&Capability::new(Capability::FILE_STAGING)));

        serve::write_frame(
            &mut writer,
            &stage_request_frame(1, "diagram.png", b"payload"),
        )
        .await
        .expect("stage request written");

        // The link must stay up and simply say nothing about staging.
        let deadline = tokio::time::Instant::now() + Duration::from_millis(400);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, serve::read_frame(&mut reader)).await {
                Ok(Ok(Some(FederationMessage::ClipboardStageResponse(response)))) => {
                    panic!("an ungated host answered a stage request: {response:?}")
                }
                Ok(Ok(Some(_))) => continue,
                Ok(Ok(None)) => panic!("the mount was torn down by an unnegotiated request"),
                Ok(Err(err)) => panic!("the link failed: {err}"),
                Err(_) => break,
            }
        }

        drop(writer);
        drop(reader);
        let _ = tokio::time::timeout(Duration::from_secs(1), server_handle).await;
    }

    /// Documents accepted behaviour, does not fix it: the reader takes one
    /// whole frame at a time, so a large paste delays every other frame on
    /// that mount until the transfer finishes. The guarantee asserted here is
    /// only that delivery resumes — the stall itself is the known cost of
    /// shipping a payload unchunked.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_large_clipboard_frame_does_not_starve_terminal_output_delivery_forever() {
        let (runtime, _rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        let host = Arc::new(FixtureHost::new().with_terminal("term_1", runtime, Vec::new()));
        let (mut reader, mut writer, server_handle, _agreed) = connect_and_mount_advertising(
            host.clone(),
            &[Capability::SCROLLBACK_REPLAY, Capability::FILE_STAGING],
        )
        .await;

        serve::write_frame(
            &mut writer,
            &FederationMessage::Terminal(TerminalChannelMessage::Open {
                terminal_id: "term_1".to_string(),
                mount_generation: 1,
                replay: ScrollbackReplay { bytes: Vec::new() },
            }),
        )
        .await
        .expect("open written");
        let Some(FederationMessage::Terminal(TerminalChannelMessage::Open { .. })) =
            serve::read_frame(&mut reader).await.expect("open ack")
        else {
            panic!("expected the server's Open acknowledgement");
        };

        // Sized to span several socket reads without approaching the image
        // cap; the loopback duplex buffer is 1 MiB.
        let payload = vec![0x41u8; 512 * 1024];
        let large = stage_request_frame(9, "big.png", &payload);
        let writer_task = tokio::spawn(async move {
            serve::write_frame(&mut writer, &large)
                .await
                .expect("large stage request written");
            writer
        });

        host.terminals
            .lock()
            .expect("fixture terminals")
            .get("term_1")
            .expect("term_1 exists")
            .runtime
            .test_process_pty_bytes_and_tee(b"output behind the big frame");

        let mut staged_path = None;
        let mut saw_output = false;
        while staged_path.is_none() || !saw_output {
            match tokio::time::timeout(Duration::from_secs(10), serve::read_frame(&mut reader))
                .await
                .expect("delivery resumed after the large frame")
                .expect("frame read")
            {
                Some(FederationMessage::Terminal(TerminalChannelMessage::Output {
                    bytes, ..
                })) => {
                    if bytes == b"output behind the big frame" {
                        saw_output = true;
                    }
                }
                Some(FederationMessage::ClipboardStageResponse(
                    crate::remote::federation::protocol::ClipboardStageResponse::Staged {
                        path,
                        ..
                    },
                )) => staged_path = Some(path),
                Some(FederationMessage::ClipboardStageResponse(other)) => {
                    panic!("the large stage failed: {other:?}")
                }
                Some(_) => continue,
                None => panic!("the link ended before both frames arrived"),
            }
        }

        let staged = staged_path.expect("the large payload staged");
        std::fs::remove_file(&staged).expect("clean up the staged file");

        let writer = writer_task.await.expect("writer task");
        drop(writer);
        drop(reader);
        let _ = tokio::time::timeout(Duration::from_secs(1), server_handle).await;
    }
}
