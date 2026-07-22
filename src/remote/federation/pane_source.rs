//! `RemoteTerminalSource` (P5): the raw-byte-channel counterpart to
//! `terminal::LocalChild`/`PtyIoActorHandle`. Fed by bytes arriving over the
//! P1 raw terminal channel (never a local PTY); `write_user_input`/`resize`/
//! `shutdown` serialize outward as `TerminalChannelMessage::Input`/`Resize`/
//! `Close`, tagged `{terminal_id, mount_generation}`.
//!
//! Lifecycle (codex #5, pinned by P2's `TerminalLifecyclePolicy` contract):
//! this type never spawns/kills a local child (there is no PTY master fd, no
//! `portable_pty` child anywhere in this module) and never emits a local
//! `PaneDied` on reader-exit — there is no `on_reader_exit` hook at all in
//! `RemoteTerminalSourceConfig`, so no code path in this module can produce
//! that event even by mistake. `RemoteTerminalSourceHandle` implements both
//! `TerminalSource` (the transport-general surface) and
//! `TerminalLifecyclePolicy` (pinned `false`) — this one type *is* P2's
//! `Remote` transport/lifecycle policy; it lives here rather than in
//! `terminal::source` because that file is not in this phase's file
//! ownership (see `implementation-notes.md`).
//!
//! Per the plan's risk/rollback note ("every phase is additive and dormant
//! behind the absence of a live mount"), nothing in production code
//! constructs a `RemoteTerminalSourceHandle` yet — only `pane::PaneRuntime::
//! spawn_remote`/`terminal::TerminalRuntime::spawn_remote` (themselves
//! uncalled outside tests until P8) and this module's own tests do — so most
//! of this module is dead code outside `#[cfg(test)]` until P8 wires a real
//! call site; allowed at module scope, matching `client.rs`'s identical
//! precedent for the same reason.
#![allow(dead_code)]

use bytes::Bytes;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::terminal::{TerminalLifecyclePolicy, TerminalSource};

use super::protocol::{ClipboardMessage, FederationMessage, TerminalChannelMessage};

/// Bounded capacity of one remote pane's outbound-input queue. Mirrors the
/// spirit of `PtyIoActorHandle`'s channel: `write_user_input` waits for
/// capacity, `try_write_user_input` fails fast — the same backpressure shape
/// the local path already gives callers.
const INPUT_CHANNEL_CAPACITY: usize = 256;

/// The `on_read` closure shape: invoked for every byte chunk a remote pane
/// receives (replay and live alike), mirroring the local `PtyIoActor`'s
/// `on_read` in everything except its return type — there is no
/// `PtyReadResult` here since a remote source has no PTY to write
/// `terminal_responses` back to (see `RemoteTerminalSourceHandle::resize`).
pub(crate) type RemoteOnRead = Box<dyn FnMut(&[u8]) + Send>;

/// Everything needed to wire one remote pane's byte-in/out plumbing onto a
/// mount's shared federation link.
pub(crate) struct RemoteTerminalSourceConfig {
    /// Raw remote terminal id — already `id::map_out`-stripped of any local
    /// namespace prefix by the caller, since this is the id placed verbatim
    /// on the wire (the remote `federation-serve` host never sees a
    /// namespaced id).
    pub(crate) terminal_id: String,
    pub(crate) mount_generation: u64,
    /// Shared outbound sink for the whole mount's federation link — every
    /// open remote pane's `Input`/`Resize`/`Close` messages multiplex onto
    /// this ONE channel (single mount tunnel, requirement 9).
    pub(crate) out_tx: mpsc::UnboundedSender<FederationMessage>,
    /// Per-terminal demultiplexed byte stream. The caller (`client.rs`'s
    /// `TerminalChannelRouter`) reads every inbound message off the ONE
    /// mount connection and pushes `Open.replay` bytes then every
    /// subsequent `Output.bytes` for this terminal id here, in wire order
    /// (RT-F6: replay before live).
    pub(crate) output_rx: mpsc::Receiver<Bytes>,
    /// Invoked for every byte chunk received (replay and live alike) — the
    /// SAME `on_read` closure a local `PtyIoActor` would call, so bytes feed
    /// `process_pty_bytes` identically regardless of transport (requirement
    /// 2). No return value and no reader-exit hook: unlike
    /// `PtyIoActorConfig`, there is nothing here that can ever produce a
    /// `PaneDied`.
    pub(crate) on_read: RemoteOnRead,
}

/// Handle for one remote-backed pane's `TerminalSource`.
pub(crate) struct RemoteTerminalSourceHandle {
    terminal_id: String,
    mount_generation: u64,
    out_tx: mpsc::UnboundedSender<FederationMessage>,
    input_tx: mpsc::Sender<Bytes>,
    reader_task: JoinHandle<()>,
    forward_task: JoinHandle<()>,
}

impl RemoteTerminalSourceHandle {
    /// This pane's raw (un-namespaced) remote terminal id, as placed verbatim
    /// on the wire — the same id a `SplitPaneRequest.target_pane_id` must
    /// carry so the remote host can resolve which of its own panes to split.
    pub(crate) fn raw_terminal_id(&self) -> &str {
        &self.terminal_id
    }

    /// Clone of this pane's shared mount out-tx, so a caller (e.g.
    /// `handle_pane_split`) can enqueue a control-channel message (like
    /// `SplitPaneRequest`) on the SAME mount tunnel this pane's
    /// input/resize/close frames already ride, without needing a separate
    /// mount registry.
    pub(crate) fn out_tx(&self) -> mpsc::UnboundedSender<FederationMessage> {
        self.out_tx.clone()
    }

    /// Spawns the byte-in task (drains `output_rx`, calls `on_read`) and the
    /// input-forwarding task (drains a per-pane bounded queue onto the
    /// shared `out_tx`). Neither task touches a local child process or PTY
    /// fd — this is the whole point of the `Remote` policy.
    pub(crate) fn spawn(config: RemoteTerminalSourceConfig) -> Self {
        let RemoteTerminalSourceConfig {
            terminal_id,
            mount_generation,
            out_tx,
            mut output_rx,
            mut on_read,
        } = config;

        // Byte-in task (requirement 2 + isolation requirement 6): isolated
        // per pane so a flooding remote pane's bytes never stall another
        // pane's task or the shared render loop — the only resource this
        // task owns is its own exclusive `output_rx`.
        let reader_task = tokio::spawn(async move {
            while let Some(bytes) = output_rx.recv().await {
                on_read(&bytes);
            }
            // `output_rx` closed: the mount's demux dropped this pane (link
            // closed, or the router removed it on `Close`). Per the pinned
            // lifecycle contract this is NEVER a local process death — no
            // event is emitted here. Surfacing "disconnected" state is P9
            // scope; deliberately a silent return.
        });

        let (input_tx, mut input_rx) = mpsc::channel::<Bytes>(INPUT_CHANNEL_CAPACITY);
        let forward_terminal_id = terminal_id.clone();
        let forward_out_tx = out_tx.clone();
        let forward_task = tokio::spawn(async move {
            while let Some(bytes) = input_rx.recv().await {
                // One `Input` message per received `Bytes` chunk — bracketed
                // paste (S8.3) arrives here as a single chunk from the pane
                // layer already, so this never re-splits it.
                let _ = forward_out_tx.send(FederationMessage::Terminal(
                    TerminalChannelMessage::Input {
                        terminal_id: forward_terminal_id.clone(),
                        mount_generation,
                        bytes: bytes.to_vec(),
                    },
                ));
            }
        });

        Self {
            terminal_id,
            mount_generation,
            out_tx,
            input_tx,
            reader_task,
            forward_task,
        }
    }
}

impl Drop for RemoteTerminalSourceHandle {
    fn drop(&mut self) {
        self.reader_task.abort();
        self.forward_task.abort();
    }
}

impl TerminalSource for RemoteTerminalSourceHandle {
    async fn write_user_input(&self, bytes: Bytes) -> Result<(), mpsc::error::SendError<Bytes>> {
        self.input_tx.send(bytes).await
    }

    fn try_write_user_input(&self, bytes: Bytes) -> Result<(), mpsc::error::TrySendError<Bytes>> {
        self.input_tx.try_send(bytes)
    }

    fn resize(
        &self,
        rows: u16,
        cols: u16,
        _cell_width_px: u32,
        _cell_height_px: u32,
        _terminal_responses: Vec<Bytes>,
    ) {
        // RT-F10 (pinned): only the visual resize crosses the wire.
        // `terminal_responses` the local mirror emulator queued against this
        // resize (e.g. a synthesized cursor-position report) are dropped —
        // the authoritative PTY driving this pane's bytes lives on the
        // remote host, which applies its own resize against its own
        // emulator and produces its own authoritative responses there.
        // Writing these locally-synthesized ones back (as `LocalChild` does
        // to its PTY master fd) has no destination; sending them onward as
        // `Input` would inject a reply for a resize the remote application
        // has not even observed applied yet — dropped by design.
        let _ = self.out_tx.send(FederationMessage::Terminal(
            TerminalChannelMessage::Resize {
                terminal_id: self.terminal_id.clone(),
                mount_generation: self.mount_generation,
                cols,
                rows,
            },
        ));
    }

    fn shutdown(&self) {
        let _ = self
            .out_tx
            .send(FederationMessage::Terminal(TerminalChannelMessage::Close {
                terminal_id: self.terminal_id.clone(),
                mount_generation: self.mount_generation,
            }));
    }
}

impl TerminalLifecyclePolicy for RemoteTerminalSourceHandle {
    fn emits_pane_died_on_reader_exit(&self) -> bool {
        false
    }
}

/// P7 requirement 4 (S11.3 OSC52 policy parity): drains a mount's
/// origin-tagged [`ClipboardMessage`] stream — the same `mpsc::
/// UnboundedReceiver<ClipboardMessage>` type `pane::PaneRuntime::
/// spawn_remote`'s dormant `clipboard_tx` parameter already produces (P5;
/// `pane.rs` is not in this phase's file ownership, so this function is
/// built to be a drop-in consumer of that exact existing channel once P8/P9
/// wires a real receiver into it) — and applies `apply` to every payload
/// with **no additional trust check based on origin**. This is the whole
/// point: a remote-origin clipboard write receives exactly the same
/// unconditional-apply policy `apply` embodies for a local one, never more
/// (an elevated confirmation step) and never less (a silent drop) — S11.3
/// requires parity, not elevation or suppression. `origin_tag` is passed
/// through to `apply` unmodified so a future call site (P8's badge/audit
/// surface) can log/attribute provenance without this function deciding
/// trust on its behalf.
///
/// # Known consideration (requirement 4, not silently decided)
/// herdr's actual local OSC52 policy (`selection::write_osc52_bytes`, the
/// classic `--remote` client's `forward_clipboard`) auto-applies every
/// clipboard write unconditionally — there is no local confirmation/consent
/// gate today. Parity therefore means this function also auto-applies.
/// Flagged here rather than silently decided: a future hardening pass may
/// want to add friction (e.g. a confirmation prompt) specifically for
/// clipboard writes whose `origin_tag` is remote, which would be an
/// intentional *escalation* beyond today's parity baseline, deferred to
/// that future work (see `implementation-notes.md`).
///
/// Runs until the channel closes (mount torn down / sender dropped); never
/// panics, never blocks on anything other than `output_rx.recv()` itself.
pub(crate) async fn apply_remote_clipboard_writes(
    mut clipboard_rx: mpsc::UnboundedReceiver<ClipboardMessage>,
    mut apply: impl FnMut(&str, &[u8]),
) {
    while let Some(ClipboardMessage {
        origin_tag,
        payload,
    }) = clipboard_rx.recv().await
    {
        apply(&origin_tag, &payload);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use super::*;

    type CapturingFixture = (
        RemoteTerminalSourceHandle,
        mpsc::Sender<Bytes>,
        mpsc::UnboundedReceiver<FederationMessage>,
        Arc<Mutex<Vec<u8>>>,
    );

    fn spawn_capturing(terminal_id: &str, mount_generation: u64) -> CapturingFixture {
        let (out_tx, out_rx) = mpsc::unbounded_channel();
        let (output_tx, output_rx) = mpsc::channel::<Bytes>(64);
        let captured = Arc::new(Mutex::new(Vec::new()));
        let captured_for_closure = captured.clone();
        let on_read = Box::new(move |bytes: &[u8]| {
            captured_for_closure
                .lock()
                .unwrap()
                .extend_from_slice(bytes);
        });
        let handle = RemoteTerminalSourceHandle::spawn(RemoteTerminalSourceConfig {
            terminal_id: terminal_id.to_string(),
            mount_generation,
            out_tx,
            output_rx,
            on_read,
        });
        (handle, output_tx, out_rx, captured)
    }

    // Test 1 (CX-4, transport half): every byte chunk placed on the demuxed
    // output channel reaches the SAME `on_read` closure a local source would
    // call, byte-for-byte, in order. (The "same emulator" half of CX-4 — a
    // real ghostty grid — is proven by `pane.rs`'s own remote-hydrate test,
    // which wires this exact plumbing to a real `PaneTerminal`.)
    #[tokio::test]
    async fn byte_in_reaches_on_read_in_order() {
        let (_handle, output_tx, _out_rx, captured) = spawn_capturing("term_1", 1);
        output_tx.send(Bytes::from_static(b"hello ")).await.unwrap();
        output_tx.send(Bytes::from_static(b"world")).await.unwrap();

        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(&*captured.lock().unwrap(), b"hello world");
    }

    // Test 2: write_user_input/resize/shutdown serialize outward as
    // `TerminalChannelMessage` tagged with the raw (map_out-stripped)
    // terminal id and the mount generation this source was constructed
    // with — never a caller's local/namespaced id.
    #[tokio::test]
    async fn input_resize_close_serialize_with_the_tagged_remote_id_and_generation() {
        let (handle, _output_tx, mut out_rx, _captured) = spawn_capturing("term_9", 7);

        handle
            .write_user_input(Bytes::from_static(b"typed"))
            .await
            .unwrap();
        let Some(FederationMessage::Terminal(TerminalChannelMessage::Input {
            terminal_id,
            mount_generation,
            bytes,
        })) = out_rx.recv().await
        else {
            panic!("expected an Input message");
        };
        assert_eq!(terminal_id, "term_9");
        assert_eq!(mount_generation, 7);
        assert_eq!(bytes, b"typed".to_vec());

        TerminalSource::resize(
            &handle,
            40,
            100,
            0,
            0,
            vec![Bytes::from_static(b"\x1b[1;1R")],
        );
        let Some(FederationMessage::Terminal(TerminalChannelMessage::Resize {
            terminal_id,
            mount_generation,
            cols,
            rows,
        })) = out_rx.recv().await
        else {
            panic!("expected a Resize message");
        };
        assert_eq!(terminal_id, "term_9");
        assert_eq!(mount_generation, 7);
        assert_eq!((cols, rows), (100, 40));

        TerminalSource::shutdown(&handle);
        let Some(FederationMessage::Terminal(TerminalChannelMessage::Close {
            terminal_id,
            mount_generation,
        })) = out_rx.recv().await
        else {
            panic!("expected a Close message");
        };
        assert_eq!(terminal_id, "term_9");
        assert_eq!(mount_generation, 7);
    }

    // Test 3 (S2.1): typing alone never mutates the byte-in path — nothing
    // this module owns loops input back into `on_read` (dumb relay, no
    // predicted local echo).
    #[tokio::test]
    async fn typing_alone_produces_no_local_echo() {
        let (handle, _output_tx, _out_rx, captured) = spawn_capturing("term_1", 1);
        handle
            .write_user_input(Bytes::from_static(b"echo me?"))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(captured.lock().unwrap().is_empty());
    }

    // Test 4 (S2.2 isolation): a flooding output channel for one pane never
    // stalls a second, independent pane's byte-in task — each pane owns its
    // own `tokio::spawn`'d reader with no shared lock between them. Run on a
    // multi-thread runtime so the two tasks can genuinely run concurrently
    // rather than merely interleave cooperatively.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_flooding_pane_does_not_stall_a_second_pane() {
        let (_flooding_handle, flooding_tx, _out_rx_a, _captured_a) =
            spawn_capturing("flooding", 1);
        let (_quiet_handle, quiet_tx, _out_rx_b, captured_b) = spawn_capturing("quiet", 1);

        for _ in 0..2000 {
            let _ = flooding_tx.try_send(Bytes::from_static(b"x"));
        }
        let progressed = tokio::time::timeout(Duration::from_millis(500), async {
            quiet_tx
                .send(Bytes::from_static(b"still here"))
                .await
                .unwrap();
            loop {
                if captured_b.lock().unwrap().as_slice() == b"still here" {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await;
        assert!(
            progressed.is_ok(),
            "a flooding remote pane must not stall a second pane's byte-in task"
        );
    }

    // Test 5 (codex #5 lifecycle): closing the byte-in channel (simulating a
    // remote disconnect) never emits any event — there is no reader-exit
    // hook in this module's config at all, and
    // `emits_pane_died_on_reader_exit` is pinned to `false`.
    #[tokio::test]
    async fn closing_the_channel_emits_no_pane_died_and_the_policy_is_pinned_false() {
        let (handle, output_tx, _out_rx, _captured) = spawn_capturing("term_1", 1);
        assert!(!handle.emits_pane_died_on_reader_exit());
        drop(output_tx);
        // The reader task must exit cleanly (no panic) once the channel
        // closes; nothing in this module can send a `PaneDied`-shaped event
        // because no such event type is even reachable from here.
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Test 7 (S8.3 paste atomicity): a bracketed paste handed in as one
    // `Bytes` chunk crosses the wire as exactly one `Input` message, never
    // split into per-byte messages.
    #[tokio::test]
    async fn a_paste_chunk_serializes_as_one_atomic_input_message() {
        let (handle, _output_tx, mut out_rx, _captured) = spawn_capturing("term_1", 1);
        let paste = Bytes::from_static(b"\x1b[200~pasted text\x1b[201~");
        handle.write_user_input(paste.clone()).await.unwrap();

        let Some(FederationMessage::Terminal(TerminalChannelMessage::Input { bytes, .. })) =
            out_rx.recv().await
        else {
            panic!("expected an Input message");
        };
        assert_eq!(bytes, paste.to_vec());
        // No further message arrives for the same paste.
        assert!(
            tokio::time::timeout(Duration::from_millis(20), out_rx.recv())
                .await
                .is_err()
        );
    }

    // Phase 07 test 4 (S11.3 OSC52 policy parity): a remote-origin clipboard
    // write reaches the SAME apply callback a local-origin one would, with
    // no extra gate/skip based on `origin_tag` — proving parity (no elevated
    // OR reduced trust) rather than a silent drop or a silently-added
    // confirmation step.
    #[tokio::test]
    async fn remote_and_local_origin_clipboard_writes_are_applied_through_the_same_policy() {
        let (tx, rx) = mpsc::unbounded_channel::<ClipboardMessage>();
        let applied: Arc<Mutex<Vec<(String, Vec<u8>)>>> = Arc::new(Mutex::new(Vec::new()));
        let applied_for_closure = applied.clone();

        let drain = tokio::spawn(apply_remote_clipboard_writes(
            rx,
            move |origin_tag, payload| {
                applied_for_closure
                    .lock()
                    .unwrap()
                    .push((origin_tag.to_string(), payload.to_vec()));
            },
        ));

        tx.send(ClipboardMessage {
            origin_tag: "remote".to_string(),
            payload: b"remote payload".to_vec(),
        })
        .unwrap();
        tx.send(ClipboardMessage {
            origin_tag: "local".to_string(),
            payload: b"local payload".to_vec(),
        })
        .unwrap();
        drop(tx);
        drain.await.unwrap();

        let applied = applied.lock().unwrap();
        assert_eq!(
            *applied,
            vec![
                ("remote".to_string(), b"remote payload".to_vec()),
                ("local".to_string(), b"local payload".to_vec()),
            ],
            "both origins must reach `apply` unconditionally and identically — no origin-based gate"
        );
    }

    // Phase 07 (S11.3): the drain task exits cleanly (never panics) once the
    // channel closes, even with zero messages applied — mirrors the same
    // "closing never misbehaves" discipline as
    // `closing_the_channel_emits_no_pane_died_and_the_policy_is_pinned_false`.
    #[tokio::test]
    async fn clipboard_drain_exits_cleanly_when_the_channel_closes_with_no_messages() {
        let (tx, rx) = mpsc::unbounded_channel::<ClipboardMessage>();
        let drain = tokio::spawn(apply_remote_clipboard_writes(
            rx,
            |_origin_tag, _payload| {
                panic!("apply must never be called when no message was ever sent");
            },
        ));
        drop(tx);
        drain.await.unwrap();
    }
}
