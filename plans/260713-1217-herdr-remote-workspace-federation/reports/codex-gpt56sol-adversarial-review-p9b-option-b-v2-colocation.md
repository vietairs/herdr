# Codex gpt-5.6-sol adversarial review — P9.2b option (b) v2 (co-location)

Reviewer: codex-cli, `gpt-5.6-sol`, xhigh, read-only, workdir `~/Projects/herdr`. Date 260714.
Target: `phase-09b-option-b-own-in-proc-session.md` (v2). Verified against source. No files changed.
Prior reviews consumed as context: option-(a) report, option-(b)-v1 report, plan.md.

## Verdict: UNSOUND (direction correct + feasible; v2 not yet an executable design)

The live-daemon co-location direction (D1) is CORRECT and feasible, and the two potential
option-(b) killers are both CONFIRMED non-blockers:
- **CRIT2 supervision IS possible** — outer `tokio::select!` drives `app.run()` + tunnel
  completion concurrently. `App` `!Send` is irrelevant when the future is driven inline by
  `block_on`, not spawned (app/mod.rs:873, :1060).
- **Co-location correctly satisfies "disconnects never kills"** — HeadlessServer owns the PTYs
  (headless.rs:192); federation output is a broadcast SUBSCRIBER not runtime ownership
  (serve.rs:582); connection EOF only aborts that connection's forwarders (serve.rs:281). A
  dropped proxy cannot SIGHUP live shells — PROVIDED b0 uses an actor/proxy seam and never moves
  PaneRuntime ownership into a connection. Lock down with a live-runtime disconnect test.

But v2 remains non-buildable: 2 CRITICAL + 6 MAJOR.

## Findings

1. **[CRITICAL] b0 has no legal live-`App` sharing model.** HeadlessServer owns `app: App` by value
   (headless.rs:192), holds `&mut self` across its whole event loop awaiting `app.api_rx`/
   `app.event_rx` in its own select! (headless.rs:430,564). `FederationHost` is deliberately
   `!Send+!Sync` because `App` is `!Send` (serve.rs:46) — works only because the duplicate App
   stays in one future (serve.rs:248). Headless requires every internal event through its
   forwarding-aware handler (headless.rs:1836); a second consumer draining `App::event_rx` would
   STEAL events + bypass clipboard/notification forwarding. `Arc<Mutex<App>>` risks a sync lock
   across async work. **Fix:** HeadlessServer stays the sole App/event_rx authority. Introduce a
   bounded async **`FederationCommand` actor seam** (reply channels) OR refactor the protocol
   handler into a state machine driven FROM the headless loop. Mount executes atomically:
   forwarding-aware drain → snapshot → cursor. Input/resize/runtime-lookup/subscription all enter
   through that seam. Live host `drain_internal_events` becomes a no-op (Headless owns the drain).

2. **[CRITICAL] Federation listener absent from server + live-handoff lifecycle.** Headless owns
   only the classic client socket (headless.rs:199), polls it inline not via accept task
   (headless.rs:496), disconnects only classic clients on handoff (headless.rs:2293); replacement
   waits only on API + classic sockets to close (headless.rs:4110). Adding a socket without
   shutdown/handoff/rollback/readiness integration can block the replacement server or strand
   controllers on transferred runtimes. The "legacy boot" fallback v2 still allows (phase:43) is
   invalid — `AppFederationHost::boot` builds a separate App (serve.rs:470), reversing D1.
   **Fix:** first-class server-owned federation socket — session-scoped path, owner-only perms,
   stale-inode protection, accept-shutdown, active-conn termination, unlink-before-handoff,
   rollback recreation, replacement-readiness inclusion. Remove duplicate-App fallback entirely:
   start the live server or fail cleanly.

3. **[MAJOR] Proxy bootstrap/session/timeout wrong or missing.** stdio↔unix-socket copy pattern
   already exists (unix.rs:437); session config precedes subcommand dispatch (main.rs:434) so a
   same-user proxy can resolve a session-scoped socket. BUT v2 cites the wrong fn:
   `ensure_remote_server_ready` returns success for NotRunning (unix.rs:1275);
   `ensure_remote_server_running` is the one that starts+waits (unix.rs:464). Readiness/status
   commands omit `--session` (classic bridge includes it, unix.rs:1869). `connect_and_mount` does
   unbounded handshake reads (client.rs:147,186) → a silent proxy blocks pre-run fallback forever.
   **Fix:** `--session … federation-serve` ensures/locates that exact session's daemon, connect to
   its federation socket; typed connect/handshake/mount timeouts; on timeout kill+wait SSH before
   classic fallback. (Single-dial restructure itself is sound: replace the whole opt-in mount/route
   block, dial once, move the `MountedConnection` into b2.)

4. **[MAJOR] Close/lag/overflow/backpressure not implementable as written.** Server outbound queue
   is unbounded per-connection (serve.rs:233); terminal forwarder ignores drain-lag (serve.rs:424),
   resumes after broadcast lag (serve.rs:440), emits no Close on source close (serve.rs:451). Client
   silently drops try_send overflow (client.rs:341). v2's "DESYNC + reopen/resnapshot" (phase:76)
   is impossible — `open_terminal` is a one-shot receiver (client.rs:313), no reset/source-swap
   protocol. Silent byte loss corrupts terminal state; unbounded egress lets one slow client eat
   the live daemon's memory. **Fix (this D2 milestone): fail-fast typed fatal tunnel outcome** for
   server close / tee lag / generation mismatch / local overflow / event gap / egress overflow →
   supervisor restores terminal + exits. Per-pane reopen/resnapshot deferred to a later protocol.
   REAL b0→b2 dependency: b0 must GENERATE the close/desync signal before b2 can consume it.

5. **[MAJOR] Federated mode does not gate all local topology/PTY creation.** v2 gates run-loop ws/tab
   actions + default-ws (phase:69) but MISSES: direct ws/tab key actions (navigate.rs:179,294),
   named-tab modal (modal.rs:929), pane split + local PTY (panes.rs:32), layout apply
   (layouts.rs:34), agent-created ws/splits (agents.rs:98), plugin panes + deferred worktrees
   (plugins/panes.rs:11, worktrees/deferred.rs:15). **Fix:** `SessionKind::Federated` + central
   forbidden-mutation policy at normal AND deferred API dispatch + input/modal entry +
   `ensure_default_workspace`. Tests cover EVERY topology/PTY creator.

6. **[MAJOR] Multiple-client control + daemon identity undefined.** Protocol assumes one connection
   per `federation-serve` lifetime (serve.rs:33); a daemon listener breaks that. Any connection can
   input/resize an arbitrary terminal without opening it first (serve.rs:392) → two local Herdr
   instances concurrently drive the same remote PTYs. Per-proxy `server_instance_id` would falsely
   present one daemon as many. **Fix (v1):** one active federation controller per live server/
   session; later connections rejected or observers; require successful `Open` ownership before
   input/resize; release lease on EOF; `server_instance_id` once per daemon lifetime. (Protocol
   auth not required under trusted-remote + owner-only socket; controller isolation still is.)

7. **[MAJOR] Supervision possible, teardown needs stronger RAII.** Outer select! around a pinned
   `app.run()` + one tunnel-supervisor future is valid (app/mod.rs:873,1060). When tunnel wins, end
   the nested scope so `app.run()` releases borrows, then drop App — no internal App disconnect
   branch needed. `tokio-util` absent (Cargo.toml:38); adding its `rt` `CancellationToken` is
   reasonable. **Fix:** armed terminal-mode guard on entering ratatui + `ChildGuard`; supervision
   observes reader+writer+child termination; tasks select on cancellation. Teardown: end App future
   scope → drop App → cancel/await reader → drop outbound senders → await/abort writer → kill+wait
   child → restore terminal. Drain SSH stderr via task. Restore on early errors/panics too, not
   just tail-position.

8. **[MAJOR] D2 conflicts with parent plan acceptance criterion.** D2 = exit-to-shell on post-start
   failure (phase:14); plan says a network blip must NOT require full remount + must show
   disconnected state (plan.md:141; the "disconnects never kills" invariant is actually plan.md:147
   not :141). **Fix:** explicitly declare D2 a flagged P9.2b intermediate behavior with acceptance
   criterion 4 deferred to P9.3, OR revise final acceptance criteria. Don't claim both.

## Slice readiness

| Slice | Status |
|---|---|
| b0 | **Blocked** — App actor seam, event authority, socket/handoff lifecycle, controller policy, bounded egress all missing |
| b1 | **Conditionally buildable** — single dial sound; add session-correct bootstrap, timeouts, guards |
| b2 | **Blocked** — central federated-mode gate + typed close/desync outcomes missing; supervision itself feasible |
| b3 | **Not ready** — depends b0–b2 + D2/global-acceptance resolution |

Start with **b0** — the compile-tested headless command/response seam + socket lifecycle. First
acceptance test: mount the actual live App, stream an existing PTY, disconnect, prove PTY survives.
b1/b2 must NOT fix their final failure contract before b0 establishes the signals they receive.

## What v2 got right (verified)

- Live daemon replacing the duplicate restored App = correct architecture.
- Post-start classic fallback removed → no competing TUI/stdin ownership.
- Outer select! supervision around `App::run()` is feasible.
- One dial/mount, reader/writer/child moved into the live session = correct.
- Drop App before tunnel teardown = right remote-pane ownership order.
- Bounded wire-clipboard sink vs unbounded local-OSC52 sink are genuinely different types, keep separate.
- Release-mode generation validation necessary (routing currently ignores generation).
- `--session <name>` before `federation-serve` correct in shape.

## Unresolved decisions (need user)

- Global acceptance criterion 4 explicitly deferred to P9.3, preserving D2 for P9.2b?
- Missing selected-session daemon: lazily start or clean pre-run fallback? (Legacy duplicate-App
  boot NOT acceptable.)
- v1 federation strictly single-controller, or must observer connections exist?
