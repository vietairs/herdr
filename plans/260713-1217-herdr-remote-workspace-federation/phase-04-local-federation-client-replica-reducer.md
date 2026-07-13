# Phase 04 — Local federation client + per-mount replica reducer

**Goal:** the LOCAL-side ingest. A long-lived `FederationClient` inside the local server connects to a remote
`federation-serve` over the reused SSH bridge, performs handshake+mount, and drives a per-mount **replica
reducer** that translates the remote stream into local workspace/pane METADATA + locally-emitted events.
**EventHub stays single-source** (codex #1 / RT-F5). No pane bytes yet (P5). **Depends on:** P1, P3.
**Blocks:** P5, P6. **Shippable:** yes (headless, no UI switch).

## Context
- **Decision (RT-F5, spike eliminated):** NO multi-source EventHub. The reducer owns its own resumable cursor
  and converts remote events into ordinary local `EventHub::push` calls on the namespaced mirror — so all local
  consumers (`events.wait`, subscriptions) keep today's single-source semantics unchanged.
- Verified: EventHub single-source, `push`/`events_after`/`current_sequence` (`src/api/event_hub.rs:15,28,40`);
  per-sub serial cursors (`src/api/subscriptions.rs:54,317`) — untouched by this phase.
- Verified: reusable transport seams `prepare_remote_herdr` (`src/remote/unix.rs:672`),
  `ensure_remote_server_ready` (`:1025`), `SshStdioBridge` (`:1685`), `ManagedSshOptions.control_path`
  multiplexing (`:634`). These already degrade to hard errors under no-TTY (`:1103-1107`) — reuse for
  non-interactive mount.
- Verified: `App`/`AppState` single in-process owner + event loop (`src/app/mod.rs`); `AppState.workspaces`
  (`src/app/state.rs:1323`). Mirror lives alongside local workspaces (no render switch here).
- Scenario S1.3 (non-interactive mount Blocker), S6.2 (reconcile-by-diff), S3.1 (capability).

## Requirements
1. **`FederationClient`** — owns one mount's lifecycle: open `SshStdioBridge` → `federation-serve`, run P1
   handshake+negotiation, receive the atomic `MountSnapshot`, then drive the event/agent channels. One in-flight
   mount attempt (idempotent).
2. **Capability negotiation + fallback signal (RT-F3/F4):** on version/capability mismatch, return a typed
   status (`FederationUnsupported`) that P8 turns into legacy-attach fallback; no partial mount.
3. **`server_instance_id` + `mount_generation` (codex #2):** record them at mount; establish the P1 `Mount`
   fence. Every inbound message is generation-checked (`fence`) before it touches state; stale traffic rejected.
4. **Replica reducer:** apply `MountSnapshot` → namespaced mirror (`map_in` for every id) in `AppState`; apply
   each `EventFrame` in `source_seq` order into the mirror and emit the equivalent local `EventHub::push`. Own a
   resumable cursor; on `Gap`/`Reset`, re-request a fresh atomic snapshot and reconcile-by-diff (add/remove/
   rename, tombstone retired ids) rather than blind-append (S6.2 substrate).
5. **Non-interactive mount (S1.3 Blocker):** the mount runs on its own task off the main event loop; all
   `prepare_remote_herdr`/`ensure_remote_server_ready` prompts are pre-resolved to fail-closed with an actionable
   status (consumed by P8 sidebar); never blocks/crashes the loop. Hard prerequisite: ssh-agent/keyfile.
6. **Lazy metadata only:** ingest snapshot as lightweight metadata entries; do NOT instantiate ghostty terminals
   for remote panes (that is P5 on focus).
7. All ids cross the boundary through P1 `map_in`/`map_out` at this single ingress/egress choke point.

## Files
- **Create** `src/remote/federation/client.rs` — `FederationClient` (connect, handshake, mount, channel drive,
  reconnect hook for P9).
- **Create** `src/remote/federation/reducer.rs` — replica reducer: snapshot→mirror, event→mirror+local push,
  cursor/gap/reset, reconcile-by-diff, generation fence.
- **Modify** `src/app/mod.rs` / `src/app/state.rs` — hold an optional per-mount remote-workspace mirror alongside
  `AppState.workspaces` (metadata; no render switch). Additive.
- **Modify** `src/remote/federation/mod.rs` — `pub mod client; pub mod reducer;`.
- **Reuse (no move yet):** `prepare_remote_herdr`/`ensure_remote_server_ready`/`SshStdioBridge` from
  `src/remote/unix.rs` (P8 wires the CLI trigger).
- **DO NOT modify** `src/api/event_hub.rs` or `src/api/subscriptions.rs` — single-source stays intact.

## TDD test plan (tests FIRST) — uses P3 `LoopbackFederationServer`
Run `cargo test -p herdr federation::client federation::reducer`:
1. **Handshake+mount over loopback:** connect to the loopback server, negotiate, receive `MountSnapshot`, mirror
   populated with namespaced ids; no collision with a pre-existing local `w1` (S4.1 at ingest).
2. **Capability mismatch ⇒ fallback status:** loopback advertising no `Federation` cap (or bad version) ⇒
   `FederationUnsupported`, no mirror created (RT-F3/F4).
3. **Single-source ordering preserved (codex #1 / RT-F5):** remote events applied via reducer become local
   `EventHub::push` in per-source order; a burst does not reorder earlier local events; existing `event_hub`
   tests stay green (local path identical).
4. **Gap/reset re-sync (codex #1):** loopback forces a `Gap`; reducer re-requests a fresh snapshot and reconciles
   by diff (added/removed/renamed panes; retired ids tombstoned) — no silent corruption.
5. **Generation fence (codex #2, top risk 3):** an `EventFrame` tagged with a stale `mount_generation` is
   rejected, never applied.
6. **Non-interactive policy (S1.3):** a prompt-requiring mount under no-TTY returns an actionable status; the
   event-loop mock never blocks/panics.

## Implementation steps
1. Write failing tests (1-6) against the loopback harness (no real SSH).
2. Implement `FederationClient` handshake/mount + capability negotiation.
3. Implement the reducer: snapshot→mirror, event→mirror+local push, cursor/gap/reset, reconcile-by-diff, fence.
4. Add the AppState mirror (no render switch); non-interactive mount task. Full suite green.

## Risks + rollback
- **Risk (top-2):** reducer mishandles a gap ⇒ mirrored state drifts from remote. Mitigation: fresh-snapshot
  re-sync on gap; property tests 3/4. **Risk:** mount blocks the loop. Mitigation: dedicated task + injected
  transport. **Rollback:** revert; new modules unused by any live path until P8 triggers a mount.

## File ownership
Exclusive: `src/remote/federation/client.rs`, `reducer.rs`. Shared (additive fields): `src/app/mod.rs`,
`src/app/state.rs` (mirror field — coordinate with P8's focus-barrier edits to the same files; P4 adds the field,
P8 adds routing). Forbidden: `src/api/event_hub.rs`, `src/api/subscriptions.rs`.
