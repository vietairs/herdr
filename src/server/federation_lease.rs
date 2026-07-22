//! Single-controller admission + linearized revocation for the co-located
//! federation listener (P9.2b b0.2).
//!
//! v1 federation allows exactly one controlling connection per live server at a
//! time. The hard part is not the "one at a time" — it is making admission and
//! revocation *linearizable* against live-handoff, which runs synchronously on
//! the same server event loop that services federation commands. Codex's v5
//! review (finding #1) showed the naive version has a resurrection hole: a
//! connection accepted before handoff can have an `AcquireController` still
//! queued behind it; closing its stream does not remove the queued command, so
//! after a rolled-back handoff frees the lease a stale `Acquire` could reserve
//! authority for a dead connection.
//!
//! The fix is a monotonic `AcceptEpoch`. Every accepted connection is stamped
//! with the epoch current at registration. Handoff revocation does, in order:
//! close admission → increment the epoch → free the slot. Any command tagged
//! with the old epoch then fails validation, so it can never re-acquire or
//! mutate. Rollback reopens admission at the already-incremented epoch; the
//! epoch is never restored to an older value.
//!
//! This module is a pure state machine with no I/O, unit-tested in isolation
//! (the `id::fence` precedent). Dormant until a later brick wires it into
//! `HeadlessServer` and `perform_live_handoff`.
#![allow(dead_code)] // dormant until the federation listener + handoff wiring land (b0.2 tail / b0.4)

/// Identifies one accepted federation connection. Globally unique and never
/// reused within a server process (the accept path mints them monotonically),
/// so `(epoch, connid)` names an exact connection for the lease's lifetime.
pub(crate) type ConnId = u64;

/// Monotonic admission epoch. Bumped on every revocation; never decremented.
pub(crate) type AcceptEpoch = u64;

/// Which phase of ownership a held lease is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Admitted (won the single-controller slot) but has not completed `Mount`.
    Reserved,
    /// Completed `Mount`; may issue mutating commands (input/resize).
    Mounted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    Free,
    Held {
        phase: Phase,
        epoch: AcceptEpoch,
        connid: ConnId,
    },
}

/// Outcome of an admission attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Admission {
    /// Reserved the single-controller slot.
    Accepted,
    /// Another connection already holds the slot.
    Busy,
    /// The connection's epoch is not the current one (superseded by a
    /// revocation) — it must not be admitted.
    StaleEpoch,
    /// Admission is closed (a handoff revocation is in progress).
    Closed,
}

/// The single-controller lease. Construct one per live server; drive every
/// admission, mount, release, and handoff transition through it.
#[derive(Debug, Clone)]
pub(crate) struct FederationLease {
    slot: Slot,
    epoch: AcceptEpoch,
    admission_open: bool,
}

impl Default for FederationLease {
    fn default() -> Self {
        Self {
            slot: Slot::Free,
            epoch: 0,
            admission_open: true,
        }
    }
}

impl FederationLease {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// The epoch newly accepted connections must be stamped with. The accept
    /// path reads this at registration and carries it on every command the
    /// connection later enqueues.
    pub(crate) fn current_epoch(&self) -> AcceptEpoch {
        self.epoch
    }

    /// Attempt to reserve the single-controller slot for a connection that was
    /// registered at `epoch`. Rejects a stale epoch or a closed admission
    /// before the busy check, so a superseded connection can never take the
    /// slot even if it is momentarily free.
    pub(crate) fn try_acquire(&mut self, epoch: AcceptEpoch, connid: ConnId) -> Admission {
        if !self.admission_open {
            return Admission::Closed;
        }
        if epoch != self.epoch {
            return Admission::StaleEpoch;
        }
        match self.slot {
            Slot::Free => {
                self.slot = Slot::Held {
                    phase: Phase::Reserved,
                    epoch,
                    connid,
                };
                Admission::Accepted
            }
            Slot::Held { .. } => Admission::Busy,
        }
    }

    /// Promote the caller's reservation to `Mounted`. Succeeds only if the
    /// caller holds the reservation at the current epoch, so a stale or
    /// non-holder `Mount` is a no-op.
    pub(crate) fn try_mount(&mut self, epoch: AcceptEpoch, connid: ConnId) -> bool {
        if epoch != self.epoch {
            return false;
        }
        match self.slot {
            Slot::Held {
                phase: Phase::Reserved,
                epoch: held_epoch,
                connid: held_connid,
            } if held_epoch == epoch && held_connid == connid => {
                self.slot = Slot::Held {
                    phase: Phase::Mounted,
                    epoch,
                    connid,
                };
                true
            }
            _ => false,
        }
    }

    /// Whether `(epoch, connid)` is the current mounted controller — the
    /// authorization check every mutating command (input/resize) must pass.
    pub(crate) fn is_mounted_controller(&self, epoch: AcceptEpoch, connid: ConnId) -> bool {
        matches!(
            self.slot,
            Slot::Held {
                phase: Phase::Mounted,
                epoch: held_epoch,
                connid: held_connid,
            } if held_epoch == epoch && held_connid == connid
        )
    }

    /// Whether any connection currently holds a mounted lease, regardless of
    /// which. A mounted controller drives this host's terminal sizes, so the
    /// host's own render loop must stop resizing them to its geometry while
    /// this holds (see `AppState::federation_owned_terminal_sizes`).
    pub(crate) fn has_mounted_controller(&self) -> bool {
        matches!(
            self.slot,
            Slot::Held {
                phase: Phase::Mounted,
                ..
            }
        )
    }

    /// Compare-and-clear release on connection EOF. Frees the slot only if the
    /// exact `(epoch, connid)` still holds it, so a late EOF from a superseded
    /// connection can never drop a newer lease. Returns whether it released.
    pub(crate) fn release(&mut self, epoch: AcceptEpoch, connid: ConnId) -> bool {
        match self.slot {
            Slot::Held {
                epoch: held_epoch,
                connid: held_connid,
                ..
            } if held_epoch == epoch && held_connid == connid => {
                self.slot = Slot::Free;
                true
            }
            _ => false,
        }
    }

    /// Begin handoff revocation, linearizably: close admission, bump the epoch,
    /// and free the slot. Returns the revoked holder's `ConnId` if one was held,
    /// so the caller can tear that connection down. After this, every command
    /// tagged with the old epoch fails validation, and no new connection is
    /// admitted until [`reopen_admission`] is called.
    pub(crate) fn begin_revocation(&mut self) -> Option<ConnId> {
        self.admission_open = false;
        self.epoch += 1;
        let revoked = match self.slot {
            Slot::Held { connid, .. } => Some(connid),
            Slot::Free => None,
        };
        self.slot = Slot::Free;
        revoked
    }

    /// Reopen admission after a rolled-back (or completed-and-superseded)
    /// handoff. The epoch stays at its already-incremented value — it is never
    /// restored — so connections stamped before the revocation remain stale.
    pub(crate) fn reopen_admission(&mut self) {
        self.admission_open = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_lease_is_free_admitting_at_epoch_zero() {
        let lease = FederationLease::new();
        assert_eq!(lease.current_epoch(), 0);
        assert!(lease.admission_open);
    }

    #[test]
    fn single_controller_second_acquire_is_busy() {
        let mut lease = FederationLease::new();
        assert_eq!(lease.try_acquire(0, 1), Admission::Accepted);
        assert_eq!(lease.try_acquire(0, 2), Admission::Busy);
    }

    #[test]
    fn acquire_with_a_stale_epoch_is_rejected() {
        let mut lease = FederationLease::new();
        lease.begin_revocation(); // epoch -> 1, admission closed
        lease.reopen_admission();
        // A connection stamped at the old epoch 0 must not be admitted.
        assert_eq!(lease.try_acquire(0, 1), Admission::StaleEpoch);
        assert_eq!(lease.try_acquire(1, 1), Admission::Accepted);
    }

    #[test]
    fn mount_requires_the_holding_reservation() {
        let mut lease = FederationLease::new();
        lease.try_acquire(0, 1);
        assert!(!lease.try_mount(0, 2), "non-holder cannot mount");
        assert!(lease.try_mount(0, 1), "holder mounts");
        assert!(lease.is_mounted_controller(0, 1));
        assert!(!lease.is_mounted_controller(0, 2));
    }

    #[test]
    fn release_is_compare_and_clear() {
        let mut lease = FederationLease::new();
        lease.try_acquire(0, 1);
        lease.try_mount(0, 1);
        // A late EOF from a different connection must not release the lease.
        assert!(!lease.release(0, 2));
        assert!(lease.is_mounted_controller(0, 1));
        // The exact holder releases.
        assert!(lease.release(0, 1));
        assert_eq!(lease.try_acquire(0, 3), Admission::Accepted);
    }

    #[test]
    fn revocation_frees_the_slot_closes_admission_and_bumps_the_epoch() {
        let mut lease = FederationLease::new();
        lease.try_acquire(0, 7);
        lease.try_mount(0, 7);
        assert_eq!(lease.begin_revocation(), Some(7));
        assert_eq!(lease.current_epoch(), 1);
        // Admission is closed: even a current-epoch acquire is refused.
        assert_eq!(lease.try_acquire(1, 8), Admission::Closed);
        lease.reopen_admission();
        assert_eq!(lease.try_acquire(1, 8), Admission::Accepted);
    }

    #[test]
    fn a_queued_stale_command_cannot_resurrect_authority_after_rollback() {
        // The exact resurrection hole codex flagged: conn 1 acquired, handoff
        // revokes (bump to epoch 1) then rolls back (reopen). A stale Mount or
        // Acquire from conn 1 at epoch 0 must be inert.
        let mut lease = FederationLease::new();
        lease.try_acquire(0, 1);
        lease.begin_revocation();
        lease.reopen_admission();
        assert!(!lease.try_mount(0, 1), "stale mount is inert");
        assert_eq!(lease.try_acquire(0, 1), Admission::StaleEpoch);
        assert!(!lease.is_mounted_controller(0, 1));
    }

    #[test]
    fn revocation_of_a_free_lease_returns_no_holder() {
        let mut lease = FederationLease::new();
        assert_eq!(lease.begin_revocation(), None);
        assert_eq!(lease.current_epoch(), 1);
    }
}
