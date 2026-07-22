# Code Review: federation link-close cleanup + mount-failure toast visibility

Verdict: **REQUEST_CHANGES**

Scope: uncommitted diff, 8 files, ~685 lines. `src/app/api/workspaces.rs`, `src/app/state.rs`,
`src/app/api.rs`, `src/app/actions.rs`, `src/events.rs`, `src/remote/federation/client.rs`,
`src/persist/snapshot.rs`, `src/server/headless.rs`. Plan:
`plans/260721-1830-federation-link-cleanup-toast-visibility/plan.md`.

## Critical / High

**1. Phase 3 desyncs `snapshot.active`/`snapshot.selected` from the filtered workspace list — wrong workspace becomes active after any restore/handoff while a federation mount is live.**
`src/persist/snapshot.rs:276-282` filters federation workspaces out of `SessionSnapshot.workspaces`, but `active`/`selected` are passed through `capture()` unchanged from the caller (`src/app/input/mod.rs:661`, `src/server/headless.rs:1037` both pass raw `state.active`/`state.selected`, i.e. absolute indices into the *unfiltered* list). Both restore sites only bound-check the stale index against the new (shorter) length — they never re-map it: `src/app/mod.rs:464` (`snap.active.filter(|&i| i < ws.len())`) and `src/app/mod.rs:857-860` (same pattern, used by the headless handoff/graceful-restart path at `headless.rs:1037`). Any federation workspace whose original index is ≤ the user's `active`/`selected` index shifts everything after it down by one (or more) positions, so restore silently focuses a *different* local workspace — it doesn't panic, since both sites clamp/bound-check, which is exactly why this passes review at a glance. Repro: workspaces `[Federation(0), A(1), B(2), C(3)]`, user focused on `B` (`active=Some(2)`). Snapshot filters out `Federation`, leaving `[A, B, C]`; `active` stays `Some(2)` → restore lands on `C`, not `B`. This hits the headless handoff path (`headless.rs:1037`, used for graceful server restart/update), so it can silently scramble the user's active workspace on a routine update while any federation mount is attached with a lower list index. Fix: compute the new index by counting non-federation workspaces before the original `active`/`selected` position (or filter+reindex before computing `active`/`selected`), inside `capture()` itself so callers don't need to duplicate the mapping.

**2. `handle_federation_mount_ended` unconditionally overwrites `state.selected` to the closing workspace and never restores the user's actual focus.**
`src/app/api/workspaces.rs` (new `handle_federation_mount_ended`): `self.state.selected = idx;` (the federation workspace's index) runs regardless of what the user was actually focused on, then `close_selected_workspace()` clamps/repoints `selected`/`active` based on that overwritten value, not the user's prior selection. This mirrors `handle_workspace_close`'s existing pattern (`workspaces.rs:578-579`), but that path is invoked in response to an explicit, targeted user/API action; this new path fires from a fully asynchronous background event (an SSH link dying) that can name *any* host's workspace regardless of what the user is currently doing. Repro: workspaces `[A(0), Federation(1), B(2)]`, user is looking at `A` (`selected=0`). The federation link for the *other* host drops; handler forces `selected=1`, removes index 1, and `close_selected_workspace`'s clamp leaves `selected=1` pointing at the post-removal array `[A(0), B(1)]` — i.e. `B`, not `A`. The user's focus silently jumps to a workspace they weren't looking at, or occasionally an OS-notification concern rather than the one they're mid-conversation in. None of the three added tests (`federation_mount_ended_removes_workspaces_and_unmounts_registry`, `_stale_generation_is_ignored`, `_drains_detached_terminal_runtimes`) exercise a scenario where the user is focused on a workspace other than the federation one being removed — the 2-workspace fixtures happen to make the clamp coincidentally correct. Fix: capture the currently active workspace's identity (id, not index) before mutating `selected`, run the close, then re-resolve and restore focus to that identity if it still exists (adjusting only when the user actually was focused on the workspace being torn down).

## Medium

**3. `close_indices_for` (`src/app/state.rs`) is a verbatim duplicate of `close_selected_workspace`'s inline grouping (`src/app/actions.rs:1503-1520`), not wired together — real drift risk, not just style.** The plan's own step 1c called for the extraction *and* wiring `close_selected_workspace` to call the new helper; only the extraction landed (`actions.rs` diff is 2 lines, both `#[cfg(unix)] AppEvent::FederationMountEnded => Vec::new()`). Today the two bodies happen to match, so behavior is identical, but any future edit to one grouping rule (e.g. changing the `is_linked_worktree` filter or the `indices.len() >= 2` threshold) that isn't mirrored in the other will silently desync manual-close and federation-teardown grouping. This is a real bug waiting to happen, not a cosmetic nit — recommend wiring `close_selected_workspace` to call `self.close_indices_for(self.selected)` as a follow-up (2-line change, low risk, plan already scoped it).

**4. Registry entry removed before the underlying SSH child is guaranteed dead — window for a second live connection to the same host.** `src/app/api/workspaces.rs` drive-task closure: `event_tx.send(FederationMountEnded{..}).await` fires *before* the teardown block that drops `out_tx`/awaits `writer_handle` (bounded 2s) and drops `tunnel_guard` (kills the ssh child). Once the App processes that event, `end_federation_mount` removes the registry entry immediately, and `begin_federation_mount`'s `AlreadyMounted` guard only checks the in-memory map — so a user-initiated remount to the same host can start a brand-new ssh connection while the old child process may still be alive (up to ~2s). Whether this actually breaks anything depends on whether the remote peer tolerates two concurrent connections from the same client identity; worth confirming, since this repo's history has at least one documented remote-relay "single-instance" constraint in an unrelated subsystem. Low cost to harden: send the event *after* `tunnel_guard` is dropped (or block the spawned task's teardown before signalling "ended").

## Low

**5. Repo convention violation: audit-style label in a new code comment.** `src/app/api/workspaces.rs`: `// Finding 3: a mount's own drive task, not just an external caller, sends FederationMountEnded...` (new test comment above `federation_mount_ended_wiring_link_closed_reaches_event_channel`). Project rule: no plan IDs/finding codes in code comments or test names — explain the invariant directly instead of citing the report finding number.

## Verified OK

- **Generation fence correctness**: `handle_federation_mount_ended` reads `remote_mirrors.get(&host_key)`'s *current* `mount_generation` at processing time (not a value captured at spawn time), and `begin_federation_mount` refuses to insert while the old entry is still present (`AlreadyMounted`). Since app events are processed sequentially, a fresh remount for the same host can only succeed after some actor already removed the old entry, so a stale `FederationMountEnded` can never tear down a newer mount — confirmed by `federation_mount_ended_stale_generation_is_ignored`.
- **No registry leak on failed materialization**: `handle_federation_mount_ready`'s pre-existing error path (`workspaces.rs:167-183`) already calls `end_federation_mount` before returning when `materialize_federation_mount` fails; this diff doesn't touch that path and doesn't introduce a new leak.
- **Multi-workspace federation mounts close as a unit**: federation-materialized workspaces set `is_linked_worktree: false` (`creation.rs:621`), so `close_indices_for` correctly groups *all* workspaces sharing `federation:<host>` regardless of which one `.position()` happens to find first.
- **`shutdown_detached_terminal_runtimes` wiring**: confirmed it actually drains `terminal_runtimes` (not just queues), matching finding 3's fix; covered by `federation_mount_ended_drains_detached_terminal_runtimes`.
- **headless.rs `.expect()` panic-safety**: `toast_notify_kind(delivery).expect(...)` is only reached after `should_forward_toast_to_clients(delivery)` returned true for the same `delivery` value, read synchronously with no intervening `.await` — provably unreachable, and identical to the existing `UpdateReady` arm (`headless.rs:2211`).
- **`#[cfg(unix)]` gating**: consistent — `FederationMountEnded`/`FederationMountFailed` variants are unix-only in `events.rs`, and every new match arm/handler that touches them is correctly gated; `FederationMountEnded` deliberately has no dedicated `headless.rs` arm (falls through the existing catch-all), matching the plan.
- **No new `unwrap()` in production code**; test-only `.unwrap()`/`.expect()` additions are acceptable.

## Recommended actions

1. Fix the `active`/`selected` index desync in `persist::capture` (Critical — data-integrity/UX bug on every restore or handoff while a federation mount is attached).
2. Preserve/restore user focus in `handle_federation_mount_ended` instead of forcing `selected = idx` (High — background event silently steals focus).
3. Wire `close_selected_workspace` to call `close_indices_for` (Medium — close the drift gap the plan itself called for).
4. Reorder event-send vs. child-process teardown, or confirm the remote peer tolerates overlapping connections (Medium).
5. Reword the "Finding 3" comment (Low).

## Unresolved questions

- Is the double-connection window in finding 4 actually reachable/harmful given the remote peer implementation, or is it already bounded by something outside this diff (e.g. server-side connection replacement)?

Status: DONE
Summary: Two real correctness bugs found (session-restore active/selected index desync after federation-workspace filtering; user focus silently stolen when an unrelated federation link drops), plus a confirmed-unwired duplication and a minor convention nit; generation fencing, leak-safety, and toast-forwarding panic-safety all check out.
Verdict: REQUEST_CHANGES
