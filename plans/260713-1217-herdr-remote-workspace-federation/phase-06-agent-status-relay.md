# Phase 06 — Remote agent-status / detection relay

**Goal:** native-ish agent status for federated panes. Screen-text detection already works source-agnostically
once bytes populate the local grid (P5); the missing signal is the remote's foreground-process probe — relay it
over the P1 agent-status stream. **Depends on:** P4 (ingest), P5 (panes hydrated). **Blocks:** nothing.
**Shippable:** yes.

## Context
- Verified: detection = two signals. **Screen-text (portable):** `terminal.detection_text()`
  (`src/pane/terminal.rs:1668-1674`) → `detect::detect_agent_with_osc` (`src/detect/mod.rs:204-227`) runs on the
  local grid regardless of byte source — **works for remote panes unmodified**. **Foreground-process (NOT
  portable):** `spawn_basic_detection_task` (`src/pane.rs:540-780`) → `foreground_process_group_id`/
  `probe_foreground_process` (`src/detect/mod.rs:266`, `src/platform/{linux,macos,windows}.rs`) needs a PID in
  the **same host's** process table — cannot observe a remote PID.
- The remote is a full herdr server (P3) so it already computes `AgentInfo`/`pane.agent_status_changed`; P3 emits
  it on the agent-status stream. Relay its value; do NOT build a new probe.
- Scenario S14.1 (must not show wrong status), S14.2 (staleness on disconnect), S14.3 (obey P4 ordering), S12.2
  (throttle cadence).

## Requirements
1. Ingest the remote `AgentStatus` stream (from P3, via P4's channel drive) as the foreground-process-equivalent
   signal for remote panes — through P1 id translate + P4 reducer ordering (S14.3: same discipline, no ad-hoc
   channel).
2. For remote panes, **suppress the local process-table probe** (`probe_foreground_process` targets a
   non-existent local PID) and use the relayed signal; keep local screen-text detection on the hydrated grid as
   the second signal (works as-is).
3. **Honesty (S14.1/S14.2):** during disconnect (P9), remote agent status renders as "last known" (dimmed/stale),
   never live truth. Fidelity limitation documented, not silently degraded.
4. **Cadence (S12.2):** throttle remote-pane detection to visible/attended panes; do not port the local per-pane
   timer to every remote pane.

## Files
- **Modify** `src/remote/federation/reducer.rs` — map remote `AgentStatus` → local agent status for namespaced
  panes (extends P4's reducer; additive event kind handling).
- **Modify** `src/pane.rs` (`spawn_basic_detection_task` / detection wiring) — for `Remote` panes skip the local
  foreground-process probe; accept a relayed status input; keep screen-text path.
- **Modify** `src/detect/mod.rs` — only if a small seam is needed to inject an external foreground-signal for
  remote panes (prefer feeding through pane detection state, not forking `detect` core).

## TDD test plan (tests FIRST) — uses P3 loopback
Run `cargo test -p herdr detect:: pane:: federation::reducer`:
1. **Screen-text detection on remote grid:** feed agent-CLI output bytes into a remote pane's grid →
   `detect_agent_with_osc` reports the agent (portability proof, no new code).
2. **Relayed foreground signal:** ingest `AgentStatus(running→waiting)` → the namespaced local pane's status
   updates WITHOUT any local process probe (assert probe NOT invoked for `Remote`).
3. **Ordering (S14.3):** a status event and a snapshot reconcile arriving out of order apply per P4 sequencing;
   final status causally correct.
4. **Staleness marker (S14.2):** with the disconnect flag set, status query returns "last known/stale", not live.
5. **Cadence (S12.2):** N remote panes, 1 visible → detection timer active only for the visible one.

## Implementation steps
1. Write failing tests (1-5) with fixture remote agent-status events.
2. Route relayed status through the reducer → pane status; gate off the local probe for `Remote` panes.
3. Add the stale/last-known flag consumed by P9 disconnect UI. Full suite green.

## Risks + rollback
- **Risk:** silently-wrong status misleads a multi-agent user (core value prop). Mitigation: relay real remote
  signal + explicit stale marker + documented fidelity. **Rollback:** revert; remote panes fall back to
  screen-text-only status (still functional, lower fidelity).

## File ownership
Exclusive-ish: extends `src/remote/federation/reducer.rs` (after P4). Shared: `src/pane.rs` (detection wiring —
after P5), `src/detect/mod.rs`. No overlap with P7/P8 file sets.
