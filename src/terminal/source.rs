//! Transport-general I/O surface for terminal byte sources, and the
//! construction/teardown lifecycle-policy seam that lets a future remote
//! (socket/ssh-relay-backed) source plug in without touching any of the
//! existing local-PTY `TerminalRuntime::spawn*` call sites.
//!
//! Handoff ops (`begin_handoff`, `duplicate_for_handoff`,
//! `foreground_process_group_id`, `rollback_handoff`, `release_after_commit`,
//! `nudge_child_redraw_after_handoff`) are local-PTY-process-handoff-specific
//! (same-host fd passing between herdr processes) and have no remote
//! equivalent. They intentionally stay OFF `TerminalSource` — see
//! `reports/arch-probe-terminalruntime-source-seam.md` §3/§4/§6 and
//! `PaneRuntimeIo`'s dedicated match arms in `src/pane.rs`.

use bytes::Bytes;
use tokio::sync::mpsc;

use crate::pty::actor::{PtyIoActor, PtyIoActorConfig, PtyIoActorHandle};

/// Transport-general operations every terminal byte source must support,
/// regardless of whether bytes come from a local PTY or (future) a remote
/// relay. Nothing PTY-specific (no fd types, no handoff ops) may live here.
pub(crate) trait TerminalSource: Send {
    /// Enqueue user input without waiting; fails immediately if the channel
    /// is full or closed.
    fn try_write_user_input(&self, bytes: Bytes) -> Result<(), mpsc::error::TrySendError<Bytes>>;

    /// Apply a terminal resize and flush any queued terminal-response bytes
    /// (e.g. cursor-position-report replies) that must be written back after
    /// applying it.
    fn resize(
        &self,
        rows: u16,
        cols: u16,
        cell_width_px: u32,
        cell_height_px: u32,
        terminal_responses: Vec<Bytes>,
    );

    /// Stop accepting further input and begin actor teardown.
    fn shutdown(&self);
}

impl TerminalSource for PtyIoActorHandle {
    fn try_write_user_input(&self, bytes: Bytes) -> Result<(), mpsc::error::TrySendError<Bytes>> {
        PtyIoActorHandle::try_write_user_input(self, bytes)
    }

    fn resize(
        &self,
        rows: u16,
        cols: u16,
        cell_width_px: u32,
        cell_height_px: u32,
        terminal_responses: Vec<Bytes>,
    ) {
        PtyIoActorHandle::resize(
            self,
            rows,
            cols,
            cell_width_px,
            cell_height_px,
            terminal_responses,
        );
    }

    fn shutdown(&self) {
        PtyIoActorHandle::shutdown(self);
    }
}

/// Whether a lifecycle policy's reader-exit path is authoritative for local
/// pane death. `LocalChild` always is (a local child process exiting means
/// the pane is gone); a future `Remote` policy will not be (losing the
/// relay connection means "disconnected", not "the remote process exited")
/// — this trait pins that contract at the construction seam so P5 cannot
/// add a remote policy that accidentally emits a local `PaneDied`.
// Only exercised by tests in this phase (P5 wires a `Remote` policy that
// makes production code query it); the contract is pinned now so P5 cannot
// add a remote policy that silently emits a local `PaneDied`.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) trait TerminalLifecyclePolicy: Send {
    fn emits_pane_died_on_reader_exit(&self) -> bool;
}

/// Construction-level lifecycle policy for a local-PTY-backed terminal
/// source. This is the only policy in this phase; a future `Remote` policy
/// (no local child, no `master_fd`, reader-exit does not imply process
/// death) is intentionally not added here (P5 scope).
///
/// `LocalChild::spawn` wraps `PtyIoActor::spawn` with zero behavior change:
/// callers still build the pane-specific `on_read`/`on_reader_exit` closures
/// (they need pane-private state — the terminal, render hooks, detection
/// sequence, etc. — that this transport-general module does not and should
/// not know about) and hand them in via `PtyIoActorConfig`. What this policy
/// type provides is a single named choke point through which every local
/// construction now flows, so a future `Remote` policy can be substituted at
/// this one seam instead of at each of the 15 `TerminalRuntime::spawn*` call
/// sites.
pub(crate) struct LocalChild;

impl LocalChild {
    /// Construct a local-PTY-backed source (spawns/attaches the actor that
    /// owns the child's `master_fd`/`master` and polls it for bytes).
    pub(crate) fn spawn(config: PtyIoActorConfig) -> std::io::Result<PtyIoActorHandle> {
        PtyIoActor::spawn(config)
    }
}

impl TerminalLifecyclePolicy for LocalChild {
    fn emits_pane_died_on_reader_exit(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drives a source only through the `TerminalSource` surface, the way
    /// `PaneRuntimeIo` delegates to it.
    fn drive_through_trait<S: TerminalSource>(
        source: &S,
        input: Bytes,
        resize_args: (u16, u16, u32, u32, Vec<Bytes>),
    ) {
        source.try_write_user_input(input).unwrap();
        let (rows, cols, cell_width_px, cell_height_px, terminal_responses) = resize_args;
        source.resize(
            rows,
            cols,
            cell_width_px,
            cell_height_px,
            terminal_responses,
        );
        source.shutdown();
    }

    // Runs the production remote source (a real `TerminalSource` impl) and
    // checks the frames it puts on the mount's outbound channel: the input
    // bytes, the resize dimensions in the right fields, and the close, all
    // tagged with the terminal id and mount generation the source was built
    // with. Terminal responses queued against a resize are not sent.
    #[tokio::test]
    async fn terminal_source_trait_sends_input_resize_close_and_drops_responses() {
        use crate::remote::federation::pane_source::{
            RemoteTerminalSourceConfig, RemoteTerminalSourceHandle,
        };
        use crate::remote::federation::protocol::{FederationMessage, TerminalChannelMessage};

        let terminal_id = "term_7";
        let mount_generation = 42_u64;
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        let (_output_tx, output_rx) = mpsc::channel::<Bytes>(1);
        let handle = RemoteTerminalSourceHandle::spawn(RemoteTerminalSourceConfig {
            terminal_id: terminal_id.to_string(),
            mount_generation,
            out_tx,
            output_rx,
            on_read: Box::new(|_: &[u8]| {}),
            pty_size: Default::default(),
        });

        let (rows, cols) = (24_u16, 80_u16);
        let input = Bytes::from_static(b"hello");
        let responses = vec![Bytes::from_static(b"\x1b[1;1R")];

        drive_through_trait(&handle, input.clone(), (rows, cols, 1600, 960, responses));

        // Input goes through a forwarding task while resize and close are
        // sent inline, so only the relative order of those two is fixed.
        let mut frames = Vec::new();
        while frames.len() < 3 {
            let frame = tokio::time::timeout(std::time::Duration::from_secs(5), out_rx.recv())
                .await
                .expect("frame arrives")
                .expect("outbound channel open");
            frames.push(frame);
        }

        let input_frame = FederationMessage::Terminal(TerminalChannelMessage::Input {
            terminal_id: terminal_id.to_string(),
            mount_generation,
            bytes: input.to_vec(),
        });
        let resize_frame = FederationMessage::Terminal(TerminalChannelMessage::Resize {
            terminal_id: terminal_id.to_string(),
            mount_generation,
            cols,
            rows,
        });
        let close_frame = FederationMessage::Terminal(TerminalChannelMessage::Close {
            terminal_id: terminal_id.to_string(),
            mount_generation,
        });
        let position = |wanted: &FederationMessage| frames.iter().position(|f| f == wanted);
        let resize_at = position(&resize_frame).expect("resize frame sent");
        let close_at = position(&close_frame).expect("close frame sent");
        assert!(position(&input_frame).is_some(), "input frame sent");
        assert!(resize_at < close_at, "resize precedes close");
        assert!(
            out_rx.try_recv().is_err(),
            "terminal responses must not be forwarded as frames"
        );
    }

    #[test]
    fn local_child_policy_emits_pane_died_on_reader_exit() {
        assert!(LocalChild.emits_pane_died_on_reader_exit());
    }

    // A remote-backed source losing its relay means "disconnected", not "the
    // remote process exited", so its reader-exit path must never report a
    // local pane death. Checked against the production remote policy type.
    #[tokio::test]
    async fn remote_shaped_policy_does_not_emit_pane_died_on_reader_exit() {
        use crate::remote::federation::pane_source::{
            RemoteTerminalSourceConfig, RemoteTerminalSourceHandle,
        };

        let (out_tx, _out_rx) = mpsc::unbounded_channel();
        let (_output_tx, output_rx) = mpsc::channel::<Bytes>(1);
        let handle = RemoteTerminalSourceHandle::spawn(RemoteTerminalSourceConfig {
            terminal_id: "term_1".to_string(),
            mount_generation: 1,
            out_tx,
            output_rx,
            on_read: Box::new(|_: &[u8]| {}),
            pty_size: Default::default(),
        });

        assert!(!handle.emits_pane_died_on_reader_exit());
    }
}
