# Implementation Notes — federation link-close cleanup + toast visibility

Plan: plan.md (approved 260721 18:50: direction + Faulted-included + Phase 3 included)
Append-only log. Entry format: What / Why / Evidence / Reversibility.

## Decisions locked at plan approval

- Cleanup fires for `LinkClosed` + `Faulted` + `Err`; `ResyncRequired` excluded (documented deferral — zero remount wiring exists).
- Mount lifetime = workspace lifetime: link end removes the host's federated workspaces; remount recreates.
- Phase 3 (snapshot.rs skips `federation:*` workspaces) included.

## Implementation deviations

- What: added one `#[cfg(unix)] AppEvent::FederationMountEnded { .. } => Vec::new()` arm to `src/app/actions.rs::handle_app_event`, a file not in the plan's owned-files list.
- Why: `AppEvent` is matched exhaustively there (not just in `api.rs`); adding the new event variant broke the build with `E0004: non-exhaustive patterns` until this arm existed.
- Evidence: `cargo test --bin herdr drive_outcome_ended_reason` failed to compile, pointing at `src/app/actions.rs:2518`, before this one-line addition; compiles clean after.
- Reversibility: trivially revertible; single line mirroring the adjacent `FederationMountFailed` no-op arm (event is fully handled earlier in `App::handle_internal_event`/`api.rs`, this arm exists only for exhaustiveness).

- What: 1c's "extract `close_selected_workspace`'s inline index computation into `close_indices_for`, have `close_selected_workspace` call it" was only half-done — `close_indices_for` was added fresh to `src/app/state.rs` (owned file) with identical body/logic, but `close_selected_workspace` itself (which actually lives in `src/app/actions.rs`, not `state.rs` as the plan's citation said) was left untouched, so its inline copy of the same grouping logic still exists as a duplicate rather than calling the new method.
- Why: `actions.rs` is not in the owned-files list; editing `close_selected_workspace` to call the new method is a pure DRY cleanup (not required by any test or acceptance criterion) and out of file-boundary scope. The mandatory match-arm fix above already crossed that boundary once for a compile-blocking reason; a second, non-essential edit there was not justified.
- Evidence: `grep -n "fn close_selected_workspace" src/app/actions.rs` shows it at `actions.rs:1496`, with the identical inline grouping logic still at `actions.rs:1503-1520`; plan's `state.rs:1503-1520` citation pointed at the wrong file for this item.
- Reversibility: fully reversible/completable later — a follow-up can change `close_selected_workspace`'s body to `let close_indices = self.close_indices_for(self.selected);` in `actions.rs` with no behavior change (own test `close_indices_for_groups_shared_worktree_space_key_else_falls_back_to_single` plus `actions.rs`'s existing `close_parent_worktree_workspace_closes_group` both already assert the identical grouping behavior).

## Code-review fixes (260721, all 5 findings)

- What: while reordering the drive task's teardown to run before the `FederationMountEnded` send (finding 4), fixed a pre-existing bug in `federation_mount_ended_wiring_link_closed_reaches_event_channel`'s polling loop: its `match` had `_ => break` catching both `Ok(None)` (channel closed) and `Err(Elapsed)` (a single 200ms poll timing out), so the loop only ever got one 200ms window before giving up.
- Why: the event is now sent only after teardown (drop `out_tx`, bounded-await `writer_handle` up to 2s, drop `tunnel_guard`) instead of immediately, so it can legitimately take longer than one 200ms poll to arrive; the old loop deterministically failed once the send was no longer near-instant. Confirmed the bug was pre-existing (not caused by the reorder) by isolating the test before vs. after: it consistently failed post-reorder with the old loop body and consistently passed once `Err(_) => continue` replaced `_ => break` (kept `Ok(None) => break` as the only real "stop early" signal).
- Evidence: `cargo test --bin herdr federation_mount_ended_wiring_link_closed_reaches_event_channel -- --test-threads=1` failed 5/5 runs before the loop fix, passed 5/5 after (some runs took ~2.24s, consistent with hitting the bounded 2s writer-handle timeout in the new teardown-before-send ordering). Full broad suite (`cargo test --bin herdr -- --test-threads=4`) is 2696 passed, 0 failed after this fix.
- Reversibility: test-only change, fully reversible; no production-code behavior depends on it.
