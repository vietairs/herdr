# Phase 09 — Lifecycle: reconnect/disconnect, restart re-fencing, cold-resume, warm-handoff exclusion

**Goal:** make federation survive the real world — explicit disconnected UI, transient-drop resilience without
remount, remote/local restart handling with mount-generation re-fencing, cold-resume for federated workspaces,
warm-handoff carve-out, and shutdown-disconnects-never-kills. **Depends on:** P8. **Blocks:** nothing.
**Shippable:** yes.

## Context
- Scenario §6/§7/§9/§15 — the network-feature heart. Blockers: S6.1, S7.1, S9.1, S9.2, S15.1.
- Verified: cold snapshot is data-only/portable (`PaneSnapshot` stores `cwd`/`launch_argv`/ANSI scrollback,
  `src/persist/snapshot.rs:97-120`); restore respawns local PTYs (`src/persist/restore.rs:579,597`); additive
  `#[serde(default)]` schema change is cheap.
- Verified: warm handoff is `SCM_RIGHTS` fd-passing over `UnixStream` (`src/server/handoff.rs:377-450`) — cannot
  cross a network; federated panes have no local fd (S9.2, architecturally impossible). `HandoffManifest`/
  `ImportedHandoffRuntime.master_fd: RawFd` assume every pane has an fd — carve-out needs its own review pass.
- Verified: `server_instance_id` + `mount_generation` (P1) are the fencing substrate for remote-restart detection
  (codex #2).
- Verified: SSH keepalive/control-socket reuse already configured (`ManagedSshOptions`, `src/remote/unix.rs:634`).

## Requirements
1. **Explicit disconnected state (S6.1/S7.1 Blocker):** on SSH/client drop, mark the remote workspace
   "disconnected" in the sidebar (visual, not removal), stop delivering stale frames, dim agent status (P6 stale
   marker), offer reconnect. Never a silently-frozen "fine" pane.
2. **Transient-drop resilience (S7.1 Blocker):** rely on SSH keepalive/control-socket reuse to survive
   seconds-scale blips transparently; otherwise a clean detect→reconnect cycle. A 2-second hiccup must NOT force
   full unmount/remount.
3. **Bounded reconnect (S7.2/S7.3):** retry with backoff, then an explicit "click to reconnect" state — no
   infinite silent retry. Reconnect is idempotent (single in-flight attempt).
4. **Remote-restart re-fencing (codex #2 / S6.2/S9.3):** on reconnect, read the remote's `server_instance_id`; if
   it changed (remote rebooted), bump `mount_generation`, tombstone the old namespace, and re-mount as a fresh
   atomic snapshot rather than splicing unrelated panes into stale slots. If unchanged, re-fetch a snapshot and
   reconcile by diff (P4 reducer). Stale-generation traffic from the pre-restart connection is rejected (P1 fence).
5. **Cold-resume across local restart (S9.1 Blocker):** add `remote_origin: Option<RemoteOrigin>` to
   `WorkspaceSnapshot` (additive `#[serde(default)]`). On restore, federated workspaces re-establish the SSH+
   federation link (P4) instead of respawning a local PTY; if the remote is unreachable, reappear in
   "disconnected, tap to reconnect" — never silently dropped, never erroring the whole session restore.
6. **Warm-handoff exclusion (S9.2 Blocker):** federated panes are removed from the warm-handoff set; after the
   new process comes up they reconnect via the cold path (5). The handoff code (which assumes every pane has a
   `master_fd`) must not panic or lose the remote workspace — dedicated review pass.
7. **Shutdown disconnects, never kills (S15.1 Blocker):** local quit closes SSH/federation links cleanly and
   writes a cold snapshot recording the mounted remotes; the remote `federation-serve`/server is left running.
   Remote-side channel/consumer slot freed by SSH EOF (verify P3 tee cleanup on client disconnect).

## Files
- **Modify** `src/persist/snapshot.rs` — `WorkspaceSnapshot.remote_origin: Option<RemoteOrigin>` (additive) +
  `RemoteOrigin` struct (host key, target, session).
- **Modify** `src/persist/restore.rs` — branch: federated ws re-establish link (P4) instead of local PTY spawn
  (`:579,:597` are the local-spawn sites to guard).
- **Modify** `src/server/handoff.rs` + `src/handoff_runtime.rs` — exclude federated panes from the fd-passing
  manifest; assert no `master_fd` demanded for them.
- **Modify** `src/remote/federation/client.rs` — reconnect state machine (backoff, idempotent), instance-id
  change detection, generation bump + re-mount, snapshot reconcile-on-reconnect.
- **Modify** `src/remote/federation/serve.rs` — free tee/channel consumer slot on client EOF (S15.3).
- **Modify** `src/ui/sidebar.rs` — disconnected visual state + reconnect affordance.
- **Modify** shutdown path (server quit) — close-not-kill remotes; snapshot mounted remotes.

## TDD test plan (tests FIRST) — uses P3 loopback
Run `cargo test -p herdr persist:: handoff:: federation::client sidebar::`:
1. **Disconnect surfaces (S6.1):** simulate client drop → workspace marked disconnected, stale frames stopped,
   status dimmed; pane not silently "live".
2. **Transient reconnect (S7.1):** brief drop → single reconnect succeeds, no unmount, no id churn.
3. **Idempotent reconnect (S7.3):** concurrent auto-retry + manual reconnect → exactly one in-flight attempt.
4. **Remote-restart re-fencing (codex #2 / S6.2):** reconnect sees a NEW `server_instance_id` → `mount_generation`
   bumped, old namespace tombstoned, re-mounted fresh; a late message with the OLD generation is rejected (P1
   fence). Same instance-id → diff reconcile.
5. **Cold-resume schema (S9.1):** old snapshot without `remote_origin` deserializes unchanged (additive proof); a
   federated snapshot restores by reconnecting; an unreachable remote restores to "disconnected" without erroring
   the whole restore.
6. **Warm-handoff exclusion (S9.2):** a handoff with a federated pane present → federated pane excluded, no
   `master_fd` demanded, no panic; local PTY panes still hand off (existing handoff tests green).
7. **Shutdown disconnects not kills (S15.1):** server quit with a mount → link closed, remote server NOT killed,
   mounted-remotes recorded in the snapshot; remote tee consumer slot freed on EOF.

## Implementation steps
1. Write failing tests (1-7).
2. Additive snapshot schema + restore branch (5). Handoff carve-out with its own review pass (6).
3. Reconnect state machine + instance-id/generation re-fencing + reconcile (2,3,4). Disconnected sidebar (1).
4. Shutdown close-not-kill + snapshot-on-quit + remote EOF cleanup (7). Full suite green.

## Risks + rollback
- **Risk (top-3):** stale post-restart traffic misrouted into a live pane. Mitigation: instance-id change →
  generation bump + P1 fence; test 4. **Risk:** warm handoff panics on a federated pane (S9.2). Mitigation:
  explicit exclusion + dedicated review + test 6. **Risk:** killing a user's remote session on quit (S15.1).
  Mitigation: close-not-kill + test 7. **Rollback:** revert; snapshot field is additive/back-compatible so old
  snapshots still load after rollback.

## File ownership
Shared (all after P8): `src/persist/snapshot.rs`, `src/persist/restore.rs`, `src/server/handoff.rs`,
`src/handoff_runtime.rs`, `src/ui/sidebar.rs` (disconnect state — coordinate with P8 badge), shutdown path.
Extends `src/remote/federation/client.rs`, `serve.rs`. Last phase — no downstream contention.
