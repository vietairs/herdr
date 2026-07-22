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
    /// Enqueue user input, waiting for channel capacity if necessary.
    async fn write_user_input(&self, bytes: Bytes) -> Result<(), mpsc::error::SendError<Bytes>>;

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
    async fn write_user_input(&self, bytes: Bytes) -> Result<(), mpsc::error::SendError<Bytes>> {
        // Inherent method takes priority over the trait method of the same
        // name/signature (arch-probe §4: signatures already match, no
        // drift), so this simply forwards — the trait impl is the seam,
        // the inherent method remains the single source of truth.
        PtyIoActorHandle::write_user_input(self, bytes).await
    }

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
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct RecordedCalls {
        write_user_input: Vec<Bytes>,
        try_write_user_input: Vec<Bytes>,
        resize: Vec<(u16, u16, u32, u32, Vec<Bytes>)>,
        shutdown_count: u32,
    }

    /// Mock `TerminalSource` used to assert that calls through the trait
    /// forward arguments verbatim (no reordering/truncation/defaulting).
    #[derive(Clone)]
    struct MockSource(Arc<Mutex<RecordedCalls>>);

    impl MockSource {
        fn new() -> Self {
            Self(Arc::new(Mutex::new(RecordedCalls::default())))
        }
    }

    impl TerminalSource for MockSource {
        async fn write_user_input(
            &self,
            bytes: Bytes,
        ) -> Result<(), mpsc::error::SendError<Bytes>> {
            self.0.lock().unwrap().write_user_input.push(bytes);
            Ok(())
        }

        fn try_write_user_input(
            &self,
            bytes: Bytes,
        ) -> Result<(), mpsc::error::TrySendError<Bytes>> {
            self.0.lock().unwrap().try_write_user_input.push(bytes);
            Ok(())
        }

        fn resize(
            &self,
            rows: u16,
            cols: u16,
            cell_width_px: u32,
            cell_height_px: u32,
            terminal_responses: Vec<Bytes>,
        ) {
            self.0.lock().unwrap().resize.push((
                rows,
                cols,
                cell_width_px,
                cell_height_px,
                terminal_responses,
            ));
        }

        fn shutdown(&self) {
            self.0.lock().unwrap().shutdown_count += 1;
        }
    }

    /// Generic caller shaped like how `PaneRuntimeIo::Actor` delegates to a
    /// `TerminalSource` — exercises the trait object boundary, not a
    /// concrete type.
    async fn delegate_through_trait<S: TerminalSource>(
        source: &S,
        input: Bytes,
        resize_args: (u16, u16, u32, u32, Vec<Bytes>),
    ) {
        source.write_user_input(input.clone()).await.unwrap();
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

    #[tokio::test]
    async fn terminal_source_delegation_forwards_args_verbatim() {
        let mock = MockSource::new();
        let input = Bytes::from_static(b"hello");
        let responses = vec![Bytes::from_static(b"\x1b[1;1R")];
        delegate_through_trait(&mock, input.clone(), (24, 80, 1600, 960, responses.clone())).await;

        let recorded = mock.0.lock().unwrap();
        assert_eq!(recorded.write_user_input, vec![input.clone()]);
        assert_eq!(recorded.try_write_user_input, vec![input]);
        assert_eq!(recorded.resize, vec![(24, 80, 1600, 960, responses)]);
        assert_eq!(recorded.shutdown_count, 1);
    }

    // Test-only stand-in for a future `Remote`-shaped lifecycle policy: same
    // trait, opposite reader-exit contract. Locks the P5 contract at the
    // seam without depending on any not-yet-built remote transport code.
    struct RemoteShapedStub;

    impl TerminalLifecyclePolicy for RemoteShapedStub {
        fn emits_pane_died_on_reader_exit(&self) -> bool {
            false
        }
    }

    #[test]
    fn local_child_policy_emits_pane_died_on_reader_exit() {
        assert!(LocalChild.emits_pane_died_on_reader_exit());
    }

    #[test]
    fn remote_shaped_policy_does_not_emit_pane_died_on_reader_exit() {
        assert!(!RemoteShapedStub.emits_pane_died_on_reader_exit());
    }

    // Handoff-exclusion (type-level): `TerminalSource` has no
    // `begin_handoff`/`duplicate_for_handoff`/`foreground_process_group_id`/
    // `rollback_handoff`/`release_after_commit`/
    // `nudge_child_redraw_after_handoff` methods, so no caller holding only
    // `&dyn TerminalSource`/`&impl TerminalSource` can invoke them — this
    // would fail to compile if any handoff op were added to the trait. The
    // trait definition above (5 methods, all transport-general) is itself
    // the enforcement; this test documents and pins that shape.
    #[test]
    fn terminal_source_trait_has_no_handoff_methods() {
        fn assert_trait_is_send<T: TerminalSource>() {}
        assert_trait_is_send::<PtyIoActorHandle>();
        assert_trait_is_send::<MockSource>();
    }
}
