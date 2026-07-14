# Codex gpt-5.6-sol adversarial review — P9.2b option (b) v5

Reviewer: codex-cli, `gpt-5.6-sol`, xhigh, read-only, workdir `~/Projects/herdr`. Date 260714.
Target: `phase-09b-option-b-own-in-proc-session.md` (v5). Verified against source. No files changed.

## Verdict: SOUND-WITH-CHANGES — **BUILD NOW** (codex verbatim)

"No CRITICAL architecture blocker remains. The changes below are compiler/test-guided implementation
constraints, small enough to fold into b0.2/b0.3/b2 without a sixth design round." "Recommendation:
BUILD NOW. No further design round." "Unresolved questions: none requiring a product or architecture
decision." After 5 rounds the D1 co-location architecture is validated and converged. 0 CRIT, 5 MAJ,
3 MIN — all implementation constraints to enforce in the relevant slice TESTS before advancing.

## Load-bearing answers (all YES)
1. **Linearization: YES** with monotonic-epoch + fairness folds. Actor-owned acceptance registers
   {epoch,connid}, retains shutdown/cancel handles, THEN spawns the supervisor. Queued old commands
   reject after rollback; later backlog connections get the new epoch, can't resurrect old authority.
2. **EOF: YES.** An independently retained unix `shutdown(Both)` breaks a serializer blocked in
   `write_frame().await`; mid-frame closure → EOF/truncation, NOT a misparsed frame. Fault frame
   best-effort; TunnelExit guaranteed if both outcomes mapped.
3. **Persistence: YES** after gating the direct history-clear path (below).
4. **Allowlist: enforceable**, but NOT at one choke — need one exhaustive `Method` classifier at BOTH
   routing entrances + a pre-child spawn gate. Remote-terminal resize IS cleanly separate from local
   split resize (panes.rs:388-418 layout vs pane_source.rs:162-187 remote resize).
5. **New races: none blocking.** Replacement server constructed before commit but doesn't enter its
   actor loop until commit (headless.rs:4082-4101). First-cause must be stored BEFORE shutdown so a
   secondary EOF can't overwrite it.

## Findings (fold into slice tests during implementation)

- **[MAJOR] Unbounded actor drain can starve the handoff linearization point.** `drain_server_events`
  runs until empty (headless.rs:1384-1392); sustained federation ingress can prevent the loop from
  reaching handoff (headless.rs:2805-2821). Revocation is linearizable once it BEGINS but may never
  begin. **Fix:** fixed per-iteration federation/server-event BUDGET + a saturation test proving handoff
  reaches admission closure within a bound. (b0.1/b0.2)

- **[MAJOR] Byte budgets must apply BEFORE producers allocate/encode frames.** `drain_available`
  concatenates the whole tee (tee.rs:27-41); tee holds 4096 msgs (pane.rs:1041-1046), each PTY read up
  to 8192B (pty/actor/unix.rs:627-639), frames cap 2MiB (protocol/mod.rs:203-210); replay embedded whole
  in Open; lag silently resumed; encoding doesn't enforce the channel cap (serve.rs:383-449,
  codec.rs:51-69). Bounded queues alone still permit oversized allocations / over-cap frames / silent
  loss. **Fix:** chunk/cap replay + live output BEFORE message construction; acquire byte permits for
  encoded frames; every tee-lag + demux-full → first-fault + EOF. Test max replay, 4096-msg bursts,
  saturated routing. (b0.3)

- **[MAJOR] Closed allowlist has multiple dispatch entrances.** Normal dispatch at api.rs:829, but
  worktree mutations intercepted earlier (runtime.rs:60-86) + internal callers enter the deferred
  handler directly (api.rs:41-57, worktrees/deferred.rs:15-30). **Fix:** ONE exhaustive `Method`
  classifier (enum centralized schema.rs:45-208) enforced before BOTH sync and deferred dispatch.
  Per-method classification IS required and compatible with the closed-allowlist policy. (b2)

- **[MAJOR] Local-runtime guard must move BEFORE child creation + cover detached commands.** PTY child
  created at pane.rs:2182-2215 BEFORE `LocalChild::spawn` (pane.rs:2266-2275); custom Shell actions
  launch a detached process with no terminal runtime (navigate.rs:769-783,845-860). A guard at
  `LocalChild::spawn` is too late + misses detached. **Fix:** require an immutable local-spawn PERMIT
  before `spawn_with_portable_pty`; separately reject detached custom commands/plugin actions in
  federated mode. Add "no child launched" tests. (b2)

- **[MAJOR] Session-history deletion bypasses the persistence policy.** Restore/save-schedule/final-clear
  centralizable (app/mod.rs:377-405, session.rs:14-106, app/mod.rs:1107-1110) — BUT config application
  directly calls `clear_history()` (app/mod.rs:1432-1436). **Fix:** gate it on
  SessionPersistencePolicy; seed + byte-compare BOTH session.json AND session-history.json across
  success/empty/pre-start-failure/post-start-fault. No required P9.2b write lost (cold resume deferred).
  (b2)

- **[MINOR] Admission epochs must be monotonic, never restored on rollback.** **Fix:** `close admission
  → increment epoch → revoke`; rollback reopens the INCREMENTED epoch; Acquire requires current epoch +
  registered connection; ops require the exact mounted token. (b0.2)

- **[MINOR] Partial-header shutdown reported as clean EOF.** `UnexpectedEof` during the 8-byte header →
  `Ok(None)` while payload truncation is an error (serve.rs:129-165). Loses diagnostics (can't misparse
  or defeat TunnelExit). **Fix:** distinguish zero-byte boundary EOF from partial-header EOF; test every
  header/body cut point. (b0.3)

- **[MINOR] Eager-open must use the split-materialization alternative.** `open_terminal` combines
  registration + Open (client.rs:308-326) while the reader owns `&mut TerminalChannelRouter`
  (client.rs:390-415). **Fix:** register/model all panes first → start reader → issue bounded Opens. (b2)

## What v5 got right (verified)
- Closes v4's stale-authority/resurrection flaw. Registration-before-enqueue mechanically possible.
- Fixes blocked-writer EOF + contradictory writer ownership. ServerInstanceId durable owner + rotation.
- Proxy byte-transparent, handshake/mount with the local client. Remote-terminal resize separated from
  local split geometry. P9.2b AC4 exception recorded (plan.md:143-146).
- **b0.1 is independently buildable while DORMANT; expose no listener until b0.4. Land the
  protocol-version bump with the first wire-shape change.**

## Build order (codex-endorsed)
Start b0.1 (actor seam + ServerInstanceId + connection supervisor) — buildable + dormant, no listener
until b0.4 → b0.2 (linearized lease, monotonic epoch, drain budget) → b0.3 (fault lane, byte permits,
EOF, header-EOF precision) → b0.4 (socket, expose listener) → b0-proxy → b1 → b2 (persistence policy,
allowlist classifier, spawn permit, eager-open split) → b3. Enforce each finding in that slice's tests.

Unresolved: NONE requiring product/architecture decision.
