//! Typed federation tunnel outcomes and first-cause capture (P9.2b b0.3).
//!
//! v1 federation is fail-fast: any transport or correctness fault ends the whole
//! mount, the supervisor restores the terminal, and the session exits (D2). For
//! that to be reliable, two things must hold (codex v5, findings #2 + #4):
//!
//!   1. Every terminating condition — peer close, a blocked/failed writer, the
//!      SSH child exiting, a panicked task, a remote terminal closing, tee lag,
//!      egress/local-queue overflow, a generation mismatch, an event gap —
//!      resolves to ONE typed [`TunnelExit`] the supervisor's `select!` can win.
//!   2. The *first* cause is the one reported. Faults race across the reader,
//!      the writer, and the child watcher; a forced transport shutdown then
//!      provokes secondary EOFs. Those must not overwrite the initiating cause.
//!
//! [`FirstCauseCell`] provides (2): the first `set` wins and every later `set`
//! is a no-op. It is `Send + Sync` so the connection supervisor can share one
//! (behind an `Arc`) across its reader/writer/child tasks.
//!
//! Pure, unit-tested, and dormant until the connection supervisor and the wire
//! fault frame are wired up (b0.3 tail / b0.4).
#![allow(dead_code)] // dormant until the federation connection supervisor lands

use std::sync::Mutex;

/// The single typed reason a federation tunnel terminated. Fail-fast for v1:
/// any of these ends the mount. `PeerClosed` is the ordinary clean shutdown;
/// the rest are faults. The remote-origin reason for an overflow may be
/// best-effort, but the local exit is always typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TunnelExit {
    /// Orderly end of stream (clean EOF).
    PeerClosed,
    /// The outbound serializer failed or was force-closed mid-write.
    WriterFailed,
    /// The SSH transport child process exited.
    ChildExited,
    /// A supervised task panicked.
    TaskPanicked,
    /// A remote terminal we were mirroring closed (authoritative Close). In v1
    /// this ends the whole mount (per-pane close needs topology projection,
    /// deferred to P9.3).
    ServerTerminalClosed,
    /// A broadcast/tee subscriber lagged and dropped output bytes — the mirror
    /// can no longer be trusted, so fail fast rather than render corrupted VT.
    Lagged,
    /// A bounded egress queue overflowed (a peer stopped reading).
    EgressOverflow,
    /// A bounded local demux queue overflowed.
    LocalQueueOverflow,
    /// An inbound frame carried a stale mount generation.
    GenerationMismatch,
    /// A gap was detected in the event sequence.
    EventGap,
}

impl TunnelExit {
    /// Whether this outcome is a clean shutdown rather than a fault.
    pub(crate) fn is_clean(&self) -> bool {
        matches!(self, TunnelExit::PeerClosed)
    }
}

/// Records the first [`TunnelExit`] observed and ignores every later one, so a
/// forced-shutdown cascade cannot bury the initiating cause. Shareable across
/// the supervisor's tasks (`Send + Sync`; wrap in an `Arc`).
#[derive(Debug, Default)]
pub(crate) struct FirstCauseCell {
    cause: Mutex<Option<TunnelExit>>,
}

impl FirstCauseCell {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Record `cause` if none is set yet. Returns `true` if this call is the one
    /// that won (i.e. the caller should drive teardown), `false` if a cause was
    /// already recorded.
    pub(crate) fn set(&self, cause: TunnelExit) -> bool {
        let mut slot = self.cause.lock().expect("first-cause cell poisoned");
        if slot.is_none() {
            *slot = Some(cause);
            true
        } else {
            false
        }
    }

    /// The winning cause, if any.
    pub(crate) fn get(&self) -> Option<TunnelExit> {
        self.cause.lock().expect("first-cause cell poisoned").clone()
    }

    pub(crate) fn is_set(&self) -> bool {
        self.cause.lock().expect("first-cause cell poisoned").is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_cell_has_no_cause() {
        let cell = FirstCauseCell::new();
        assert!(!cell.is_set());
        assert_eq!(cell.get(), None);
    }

    #[test]
    fn the_first_cause_wins_and_later_ones_are_ignored() {
        let cell = FirstCauseCell::new();
        assert!(cell.set(TunnelExit::Lagged), "first set wins");
        assert!(
            !cell.set(TunnelExit::PeerClosed),
            "a secondary EOF must not win"
        );
        assert!(!cell.set(TunnelExit::WriterFailed));
        // The initiating cause is preserved, not the secondary shutdown noise.
        assert_eq!(cell.get(), Some(TunnelExit::Lagged));
    }

    #[test]
    fn shared_across_threads_only_one_winner() {
        use std::sync::Arc;
        use std::thread;

        let cell = Arc::new(FirstCauseCell::new());
        let causes = [
            TunnelExit::WriterFailed,
            TunnelExit::ChildExited,
            TunnelExit::EgressOverflow,
            TunnelExit::PeerClosed,
        ];
        let winners: usize = thread::scope(|scope| {
            let handles: Vec<_> = causes
                .iter()
                .map(|c| {
                    let cell = Arc::clone(&cell);
                    let c = c.clone();
                    scope.spawn(move || usize::from(cell.set(c)))
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).sum()
        });
        assert_eq!(winners, 1, "exactly one set call wins");
        assert!(cell.is_set());
    }

    #[test]
    fn peer_closed_is_clean_faults_are_not() {
        assert!(TunnelExit::PeerClosed.is_clean());
        assert!(!TunnelExit::Lagged.is_clean());
        assert!(!TunnelExit::EgressOverflow.is_clean());
    }
}
