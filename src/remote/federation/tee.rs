//! Broadcast tee attaching at the `on_read` raw-PTY-byte source
//! (`pane::PaneRuntime::subscribe_output_bytes` / `terminal::TerminalRuntime::
//! subscribe_output_bytes`).
//!
//! The actual fork happens inside `pane.rs`'s `on_read` closures (the exact
//! bytes `process_pty_bytes` consumes, never rendered frames) — this module
//! defines the shared capacity/type so the fork is a broadcast (never a
//! takeover): the local render path and any number of federation channels
//! all read the same bytes independently, and a subscriber that falls behind
//! observes `RecvError::Lagged` (a detectable gap) instead of blocking the
//! writer or starving another subscriber.

use bytes::Bytes;
use tokio::sync::broadcast;

/// Capacity (in messages) of a pane's output tee. Mirrors
/// `pane::OUTPUT_TEE_CAPACITY` — kept as a named constant here too so
/// federation code that reasons about lag tolerance has one place to look,
/// independent of `pane.rs` internals.
pub(crate) const CAPACITY: usize = 4096;

/// Reads every available message currently buffered for `rx` without
/// blocking, coalescing them into one `Vec<u8>` in order. Used by the
/// terminal-channel forwarding task to drain a burst of PTY reads into a
/// single `Output` frame instead of one frame per read syscall.
pub(crate) fn drain_available(rx: &mut broadcast::Receiver<Bytes>) -> (Vec<u8>, bool) {
    let mut out = Vec::new();
    let mut lagged = false;
    loop {
        match rx.try_recv() {
            Ok(bytes) => out.extend_from_slice(&bytes),
            Err(broadcast::error::TryRecvError::Lagged(_)) => {
                lagged = true;
            }
            Err(broadcast::error::TryRecvError::Empty | broadcast::error::TryRecvError::Closed) => {
                break;
            }
        }
    }
    (out, lagged)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broadcast_tee_delivers_identical_bytes_to_every_subscriber_without_blocking_the_sender() {
        let (tx, mut render_rx) = broadcast::channel::<Bytes>(CAPACITY);
        let mut federation_rx = tx.subscribe();

        // The local render path (`render_rx`) and a federation consumer
        // (`federation_rx`) both subscribe; sending never blocks even though
        // neither has read yet.
        tx.send(Bytes::from_static(b"hello")).unwrap();
        tx.send(Bytes::from_static(b" world")).unwrap();

        assert_eq!(render_rx.try_recv().unwrap(), Bytes::from_static(b"hello"));
        assert_eq!(
            render_rx.try_recv().unwrap(),
            Bytes::from_static(b" world")
        );
        assert_eq!(
            federation_rx.try_recv().unwrap(),
            Bytes::from_static(b"hello")
        );
        assert_eq!(
            federation_rx.try_recv().unwrap(),
            Bytes::from_static(b" world")
        );
    }

    #[test]
    fn sending_with_zero_subscribers_never_panics_or_blocks() {
        let (tx, _rx) = broadcast::channel::<Bytes>(CAPACITY);
        drop(_rx);
        // No subscribers left; `send` returns an error but must not panic.
        assert!(tx.send(Bytes::from_static(b"orphaned")).is_err());
    }

    #[test]
    fn drain_available_coalesces_a_burst_of_reads_into_one_buffer() {
        let (tx, mut rx) = broadcast::channel::<Bytes>(CAPACITY);
        tx.send(Bytes::from_static(b"ab")).unwrap();
        tx.send(Bytes::from_static(b"cd")).unwrap();

        let (drained, lagged) = drain_available(&mut rx);

        assert_eq!(drained, b"abcd".to_vec());
        assert!(!lagged);
    }

    #[test]
    fn drain_available_reports_lag_without_panicking() {
        let (tx, mut rx) = broadcast::channel::<Bytes>(2);
        for i in 0..8u8 {
            let _ = tx.send(Bytes::from(vec![i]));
        }

        let (_drained, lagged) = drain_available(&mut rx);

        assert!(lagged);
    }
}
