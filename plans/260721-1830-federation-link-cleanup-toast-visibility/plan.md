---
title: "Federation link-close cleanup + mount-failure toast visibility"
description: "Generation-fenced mount teardown when a federation link ends, plus Terminal/System toast forwarding through the headless server"
status: pending
priority: P1
effort: 5h
branch: feat/remote-workspace-federation
tags: [federation, toast, cleanup, tdd]
created: 2026-07-21
---

Env: `ZIG=$HOME/.local/zig-0.15.2/zig`, `PATH=$HOME/.local/zig-0.15.2/xcrun-shim:$PATH`.
Test: `cargo test --bin herdr <filter> -- --test-threads=4` (no nextest here).
Single implementer, sequential phases, no parallel file ownership needed.

## Report findings -> disposition

1. Generation race (stale cleanup nukes fresh remount) -> Phase 1, fenced.
2. Terminal/System toast silent in headless -> Phase 2, both `workspaces.rs` + `headless.rs` halves.
3. `shutdown_detached_terminal_runtimes` never drained on new event path -> Phase 1 handler calls it explicitly.
4. Persist federation-blindness (3->5 duplication) -> Phase 3, separable, optional per scope rules.
5. `close_selected_workspace` reuse + per-workspace `WorkspaceClosed` event parity -> Phase 1, via extracted `close_indices_for`.
6. `DriveOutcome::Faulted`/`ResyncRequired` not literally "LinkClosed/error" -> scope decision below, Phase 1.

## Scope decision (flag for user)

`drive_mount_channel` (`src/remote/federation/client.rs:395-402`) returns `Ok(LinkClosed)`, `Ok(Faulted(reason))` (doc at `client.rs:228-231`: "caller ends the session, it does not remount"), `Ok(ResyncRequired)` (doc at `client.rs:232-237`: caller must remount + reconcile, mirror/workspaces must survive), or `Err(io::Error)`. User's ask says "LinkClosed/error." Treating `Faulted` as session-ending too (cleanup fires) matches its own doc comment and avoids leaving an identical-symptom gap next to the fix. `ResyncRequired` must NOT cleanup (no remount wiring exists anywhere in the codebase today — grep confirms `ResyncRequired` has zero non-test consumers), so it is a **pre-existing, unaffected gap**, not silently fixed. Net: cleanup fires for `LinkClosed` + `Faulted` + `Err`; `ResyncRequired` deferred (see Deferrals).

---

## Phase 1: Link-close cleanup (generation-fenced)

**Files:** `src/events.rs`, `src/remote/federation/client.rs`, `src/app/api/workspaces.rs`, `src/app/api.rs`, `src/app/state.rs`

### 1a. New event + pure outcome-classifier (TDD first)

- `src/events.rs`: add, alongside `FederationMountFailed` (`events.rs:171`, same `#[cfg(unix)]` gate as `events.rs:170`):
  ```rust
  FederationMountEnded { host_key: crate::remote::federation::id::HostKey, generation: u64, target: String, reason: String },
  ```
- `src/remote/federation/client.rs`: add `pub(crate) fn drive_outcome_ended_reason(outcome: &Result<DriveOutcome, std::io::Error>) -> Option<String>` near `DriveOutcome` (`client.rs:225-238`). `Some(..)` for `Ok(LinkClosed)`, `Ok(Faulted(r))` (`format!("{r:?}")`), `Err(e)` (`e.to_string()`); `None` for `Ok(ResyncRequired)`.
- Tests (`client.rs` `mod tests`, `client.rs:494`):
  - `drive_outcome_ended_reason_link_closed_faulted_err_return_some`: asserts `Some` for all three.
  - `drive_outcome_ended_reason_resync_required_returns_none`: asserts `None`.

### 1b. Wire drive-task teardown to send the event

- `src/app/api/workspaces.rs:193-221`: add `let event_tx = self.event_tx.clone();` before the `tokio::spawn`; clone `host_key` (already bound `workspaces.rs:151`) and `target` (fn param) into the closure. Replace the `match outcome { Ok(..) => log, Err(..) => log }` block (`workspaces.rs:206-213`) with: keep the tracing, then `if let Some(reason) = crate::remote::federation::client::drive_outcome_ended_reason(&outcome) { let _ = event_tx.send(AppEvent::FederationMountEnded{host_key, generation, target, reason}).await; }`.
- Test (`workspaces.rs` `mod tests`, `#[cfg(unix)]`, reuses `spawn_test_tunnel` at `workspaces.rs:833-847`):
  - `federation_mount_ended_wiring_link_closed_reaches_event_channel`: mount ready via `handle_federation_mount_ready` (as in `coexistence_local_and_remote_render_together`, `workspaces.rs:855-877`), drop the `cat` child's stdin/kill it to force `LinkClosed`, poll `app.event_rx.recv()` (pattern at `workspaces.rs:964-978`) for `AppEvent::FederationMountEnded{host_key, generation, ..}` matching the mounted host/gen.

### 1c. Extract shared close-index logic (small refactor, DRY)

- `src/app/state.rs`: extract `close_selected_workspace`'s inline index computation (`state.rs:1503-1520`) into `pub(crate) fn close_indices_for(&self, index: usize) -> Vec<usize>` (same body); `close_selected_workspace` calls `self.close_indices_for(self.selected)`.
- Test (`state.rs` `mod tests`): `close_indices_for_groups_shared_worktree_space_key_else_falls_back_to_single` — 2 workspaces sharing a non-linked `worktree_space.key` -> both indices; a lone/linked workspace -> `vec![index]`. Regression guard for the extraction.

### 1d. Handler + dispatch

- `src/app/api/workspaces.rs`: new `#[cfg(unix)] pub(crate) fn handle_federation_mount_ended(&mut self, host_key: HostKey, generation: u64, target: String, reason: String)`:
  1. `if self.state.remote_mirrors.get(&host_key).map(|m| m.mount().mount_generation) != Some(generation) { tracing::debug!(...); return; }` (fences the race — finding 1).
  2. `self.state.end_federation_mount(&host_key);` (`state.rs:1539-1544`).
  3. Find `idx` via `worktree_space().key == format!("federation:{}", host_key.as_str())` (matches `creation.rs:617`); if none, return (mount had no materialized workspaces).
  4. `let closing: Vec<_> = self.state.close_indices_for(idx).iter().map(|&i| (self.public_workspace_id(i), self.workspace_info(i))).collect();` (both helpers already `pub(super)`/reused at `workspaces.rs:467-468`).
  5. `self.state.selected = idx; self.state.close_selected_workspace(); self.shutdown_detached_terminal_runtimes();` (finding 3 — App-level drain, mirrors `handle_workspace_close`, `workspaces.rs:480-483`).
  6. Emit one `EventEnvelope{WorkspaceClosed}` per entry in `closing` (finding 5 — per-workspace parity, unlike `handle_workspace_close`'s single-event gap).
  7. `render_dirty`/`render_notify` (mirrors `handle_federation_mount_failed`, `workspaces.rs:242-243`).
- `src/app/api.rs`: add `#[cfg(unix)] if let AppEvent::FederationMountEnded{host_key, generation, target, reason} = ev { self.handle_federation_mount_ended(host_key, generation, target, reason); return; }` next to the existing arms (`api.rs:146-156`). No `headless.rs` change needed here — the catch-all (`headless.rs:2251-2254`) already calls `self.app.handle_internal_event(ev)`, and workspace removal is visible to attached TUI clients via state-diff render sync (`headless.rs:923` `sync_foreground_client_state`), not per-event forwarding.

### Tests (`workspaces.rs` `mod tests`, `#[cfg(unix)]`)

- `federation_mount_ended_removes_workspaces_and_unmounts_registry`: materialize via `handle_federation_mount_ready` (gen=1); call `handle_federation_mount_ended(host_key, 1, target, reason)`; assert `remote_mirrors.is_empty()`, federation workspace gone from `state.workspaces`, `event_hub.events_after(0)` contains a `WorkspaceClosed` for it.
- `federation_mount_ended_stale_generation_is_ignored`: materialize gen=1, then simulate a completed remount (`end_federation_mount` + `begin_federation_mount` with a mirror at gen=2, reusing `test_federation_mirror`-style construction at `workspaces.rs:821-828`); call handler with the **stale** gen=1; assert registry still shows gen=2 and workspaces/state unchanged (the race in finding 1).
- `federation_mount_ended_drains_detached_terminal_runtimes`: after materializing (which inserts into `app.terminal_runtimes` per `creation.rs:584`), call the handler; assert the pane's terminal id is gone from `app.terminal_runtimes` (not just queued in `terminal_runtime_shutdowns`) — proves finding 3 is closed.

**Risk:** Medium (generation race is the highest-ranked report finding). **Mitigation:** explicit fence in step 1 + dedicated stale-generation test. **Rollback:** revert `events.rs`/`client.rs`/`workspaces.rs`/`api.rs`/`state.rs` diffs; no persisted-state or wire-protocol shape changed, safe to drop independently of Phase 2/3.

---

## Phase 2: Mount-failure toast visibility (Terminal/System + headless forwarding)

**Files:** `src/app/api/workspaces.rs`, `src/server/headless.rs`

### 2a. Local Terminal/System delivery (monolithic + in-process gate)

- `src/app/api/workspaces.rs::handle_federation_mount_failed` (`workspaces.rs:227-244`): restructure the `if matches!(.., Herdr)` block into a `match self.state.toast_config.delivery`: keep `Herdr` branch as-is; add `Terminal | System if self.local_terminal_notifications` branch calling `crate::terminal_notify::show_notification` / `crate::platform::show_desktop_notification` (mirrors the shared tail at `api.rs:305-323`, message `format!("federated mount to {target} failed")` / body `reason`); `_ => {}` otherwise. `local_terminal_notifications` is `false` in headless (`headless.rs` `run()` sets it, e.g. `headless.rs:4138`), so this branch is a no-op there by design — matches existing pattern.

### 2b. Headless forwarding (server-mode Terminal/System toast)

- `src/server/headless.rs::handle_internal_event_with_forwarding` (`headless.rs:1982`): add a dedicated arm before the catch-all (`headless.rs:2251-2254`), modeled on `UpdateReady` (`headless.rs:2182-2219`):
  ```rust
  AppEvent::FederationMountFailed { target, reason } => {
      let (target, reason) = (target.clone(), reason.clone());
      self.app.handle_internal_event(ev);
      if should_forward_toast_to_clients(self.app.state.toast_config.delivery) {
          self.send_flat_toast_to_foreground_client(
              toast_notify_kind(self.app.state.toast_config.delivery)
                  .expect("toast forwarding requires a client notification kind"),
              format!("federated mount to {target} failed: {reason}"),
          );
      }
      true
  }
  ```
  Uses `should_forward_toast_to_clients`/`toast_notify_kind` (`src/server/notifications.rs:9-19`) and `send_flat_toast_to_foreground_client` (`headless.rs:1841-1848`), same as `UpdateReady`. `#[cfg(unix)]` on the arm (variant is unix-only, `events.rs:170-171`).

### Tests

- `workspaces.rs` `mod tests`, extend `coexistence_mount_failure_keeps_local_session_alive` (`workspaces.rs:883-908`, currently Herdr-only) with two new `#[cfg(unix)]` tests: `mount_failure_terminal_delivery_calls_local_notify_when_enabled` and `mount_failure_system_delivery_is_noop_when_local_notifications_disabled` (set `app.local_terminal_notifications = false`, assert no panic/no state.toast set — can't easily assert the OS call fired without mocking `terminal_notify`; assert instead that `state.toast` stays `None` for Terminal/System, proving no cross-wiring into the Herdr path).
- `headless.rs` `mod tests`, new `#[cfg(unix)]` test `federation_mount_failed_system_toast_forwards_to_foreground_client`, modeled exactly on `system_toast_delivery_forwards_system_notify_kind` (`headless.rs:8288-8333`): build `test_headless_server()`, register a foreground client, set `toast_config.delivery = System`, call `server.handle_internal_event_with_forwarding(AppEvent::FederationMountFailed{target:"host1".into(), reason:"connection refused".into()})`, assert the forwarded `ServerMessage::Notify{kind: SystemToast, message, ..}` contains "host1" and "connection refused". Add a `Terminal`-delivery sibling test the same way.

**Risk:** Low (additive branches, no state-shape change). **Mitigation:** n/a. **Rollback:** revert the two match/branch diffs independently of Phase 1/3.

---

## Phase 3 (optional, separable): persist federation-blindness

Only if kept small — it is: single-function change, one call site fanning to 3 real callers (`session.rs:43`, `input/mod.rs:661`, `headless.rs:1037`).

- `src/persist/snapshot.rs::capture` (`snapshot.rs:250-275`) / `capture_workspace` (`snapshot.rs:277-299`): skip workspaces whose `worktree_space.as_ref().is_some_and(|m| m.key.starts_with("federation:"))` before mapping — federation workspaces are remount-derived, never restorable as local sessions.
- Test (`snapshot.rs` `mod tests:478`, model on `capture_contract_tracks_worktree_space_membership` at `snapshot.rs:842`): `capture_skips_federation_materialized_workspaces` — one local + one `worktree_space.key = "federation:host1"` workspace in, assert snapshot has only the local one.

**Risk:** Low, but touches all 3 session-save call sites' output shape — verify no test asserts federation workspaces currently round-trip through snapshots (predict report confirms zero `federation`/`host_key` hits in `src/persist/*.rs`, so unlikely). **Rollback:** revert `snapshot.rs` diff alone.

If time-boxed out: **document as explicit deferral** (see below) — this alone does not require Phase 1 to be correct or vice versa.

---

## Acceptance criteria

- Phase 1: killing/erroring a mount's tunnel unmounts the registry entry and removes exactly that host's workspaces, exactly once, even under a remount race (stale-generation test passes); `terminal_runtimes` entries for removed federation panes are actually shut down, not just queued.
- Phase 2: `ToastDelivery::Terminal`/`::System` federation-mount-failure notices reach (a) the local terminal/OS in monolithic mode and (b) the attached TUI client in headless/server mode, via the same code paths every other Terminal/System notification uses.
- Phase 3 (if done): a session save while a federation mount is live does not persist federation workspaces; restart no longer duplicates them.
- `cargo test --bin herdr federation_mount_ended -- --test-threads=4`, `... mount_failure -- --test-threads=4`, `... drive_outcome_ended_reason -- --test-threads=4` (and `snapshot_skips_federation` if Phase 3) all green; `just check`-equivalent (`cargo test --bin herdr -- --test-threads=4` broad run) has no new failures.

## Explicit deferrals

- `DriveOutcome::ResyncRequired` never triggers cleanup (correct per its doc) but also has zero remount-wiring anywhere in the codebase — a mount stuck in a gap/reset state today just silently stops updating forever, pre-existing, unaffected by this plan.
- Manually closing a federation workspace via `workspace.close` (`handle_workspace_close`, `workspaces.rs:460-493`) does not call `end_federation_mount` — the registry entry outlives the workspaces, blocking remount via `double_attach_conflict` (`state.rs:1556-1561`) until the link itself later ends. Pre-existing, out of the two approved fixes' scope.
- Phase 3 persist-blindness fix ships only if scope allows; otherwise defer as a separate follow-up (it independently explains the reported 3->5 duplication per report finding 4).

## Unresolved questions

- Confirm the `Faulted`-also-ends-session scope decision above matches intent, or should `Faulted` be excluded (cleanup only on literal `LinkClosed`/`Err`)?
- Is Phase 3 (persist blindness) wanted in this same change, or tracked as a separate follow-up ticket?

Status: DONE
Summary: TDD plan for both approved federation fixes (generation-fenced link-close cleanup reusing close_selected_workspace; Terminal/System toast visibility in both monolithic and headless paths), plus an optional separable Phase 3 for the persist-blindness adjacent bug and explicit deferrals for ResyncRequired and manual-close-doesn't-unmount.
Riskiest trade-off: treating `DriveOutcome::Faulted` as session-ending (same as `LinkClosed`) beyond the literal "LinkClosed/error" wording — matches the code's own doc comment but is an interpretive scope expansion the user should confirm.
