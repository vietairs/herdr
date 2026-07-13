# Implementation Notes — herdr remote→local-workspace federation (Tier-2, two-server)

Append-only. 4-line entries: What / Why / Evidence / Reversibility. Log decisions, deviations,
surprises the moment they happen.

Plan: plan.md (v2, supersedes v1). Branch: feat/remote-workspace-federation (off master).
Architecture: our herdr on BOTH ends; remote = headless federation server; new federation protocol
over the SSH bridge; local single-source EventHub + per-mount replica reducer; TerminalSource seam
for remote-backed panes; capability negotiation preserves legacy full-screen `--remote` fallback.

## Deviations / Decisions / Surprises

- What: Started cook on root phases P1 (federation protocol + id-fencing) and P2 (TerminalSource seam).
  Why: both are independent roots per plan.md; P1 unblocks P3/P4, P2 unblocks P5.
  Evidence: plan.md dependency graph; phase-01/phase-02 disjoint file ownership.
  Reversibility: pure-additive lib (P1) + behavior-preserving refactor (P2) — trivially revertible before P3+.

- What: BLOCKED — herdr will not compile in this environment. Vendored libghostty-vt requires zig
  0.15 (build.zig uses 0.15 std.Build API; zig 0.16 breaks it), but zig 0.15.2 cannot link on
  macOS 27 / Darwin 27 — its bundled darwin libSystem stubs predate this OS, so even the zig build
  RUNNER fails with undefined _sigaction/_waitpid/_realpath$DARWIN_EXTSN/_sysctlbyname/_posix_memalign.
  Why: macOS 27 is newer than zig 0.15.2; nix (flake pins zig_0_15 + sysroot) is not installed here.
  Evidence: build.rs:78 zig panic; vendor/libghostty-vt/build.zig.zon minimum_zig_version="0.15.2";
  flake.nix:88 zig_0_15; clang links fine (SDK 27.0 present, Xcode-beta). 4 attempts: 0.16(API),
  0.15.2, +SDKROOT, +MACOSX_DEPLOYMENT_TARGET — all fail at the build-runner link step.
  Reversibility: N/A (environment). P1 code is written+committed (WIP, unvalidated: cannot cargo
  build/test). Unblock options: build on a Linux remote, install nix + `nix develop`, use an older
  macOS, or wait for zig macOS-27 support. Cook is PAUSED at P1 pending a buildable environment.

- What: P1 codec payload switched bincode → serde_json (encode/decode in protocol/codec.rs).
  Why: federation frames carry api-schema types (SessionSnapshot, EventKind) using
  `#[serde(default, skip_serializing_if=Option::is_none)]`; bincode is non-self-describing and
  cannot round-trip omitted fields → test failed `UnexpectedEnd{additional:1}`. serde_json is
  self-describing, already a dep, and is the format these types are authored for.
  Evidence: cargo test on remote (gpu-ml/nix): 15 pass/1 fail; src/api/schema/*.rs skip_serializing_if.
  Reversibility: trivial (2 fns). FOLLOW-UP (P5): raw terminal Output/Input bytes JSON-bloat ~4x —
  move the high-volume terminal byte path off the serde codec onto a raw length-prefixed sub-framing.

- What: BUILD ENV UNBLOCKED + P1 VALIDATED. Installed Determinate Nix on appn-ltu-vm-100 (=gpu-ml,
  Ubuntu 24.04 x86_64); cloned fork to ~/Projects/herdr there; `nix develop` provides rust 1.96.1 +
  zig 0.15 + cmake/ninja. Iteration loop: edit local (source of truth) → git push → remote pull +
  `nix develop -c cargo test`. P1 federation tests 16/16 green.
  Why: local macOS 27 cannot build (zig link). Remote linux via nix is the build/test executor.
  Evidence: ~/herdr-test.log on gpu-ml: "16 passed; 0 failed"; commit d619c20 pushed.
  Reversibility: env-only. Loop proven; P2-P9 proceed via same push→remote-test cycle.

- What: P2 `LocalChild::spawn` is a thin wrapper over `PtyIoActor::spawn` — it does NOT itself
  build the `on_read`/`on_reader_exit` closures; callers (`spawn_command_builder`/`from_handoff_fd`
  in pane.rs) still assemble those, since they close over pane-private state (terminal Arc, render
  hooks, detection seq) that `terminal/source.rs` (a transport-general module) should not know about.
  Why: the real DRY-able seam per phase-02 req 3 is "one named construction choke point P5 can swap",
  not literally relocating pane-private closure-building code into source.rs (would need many pane.rs
  privates made pub(crate), higher blast radius for zero behavior gain).
  Evidence: phase-02 spec req 3 "(a) how byte source created... this phase adds ONLY the LocalChild
  policy"; both spawn_command_builder (pane.rs:1798) and from_handoff_fd (pane.rs:1653) still build
  their own on_read/on_reader_exit, now passed into `LocalChild::spawn(PtyIoActorConfig{...})`.
  Reversibility: trivial — LocalChild::spawn body is one line (`PtyIoActor::spawn(config)`).

- What: `runtime.rs` left unmodified (spec listed it as a file to modify, "route
  send_bytes/try_send_bytes/resize/shutdown through TerminalSource"). The actual duplicated
  Actor-vs-TestChannel match arms live in pane.rs's `PaneRuntimeIo` impl, not runtime.rs (runtime.rs
  is already a flat 1:1 delegate to `PaneRuntime`, nothing to de-duplicate). Routed those pane.rs
  match arms' `Actor(actor)` calls through `TerminalSource::{write_user_input,try_write_user_input,
  resize,shutdown}` (UFCS) instead of the inherent methods.
  Why: matches the literal intent (route the local actor's I/O ops through the trait) without a
  no-op edit to runtime.rs just to satisfy the file list.
  Evidence: src/pane.rs `PaneRuntimeIo::{shutdown,resize,send_bytes,try_send_bytes}`.
  Reversibility: trivial — revert the 4 call sites to inherent-method syntax, byte-identical behavior.

- What: Lifecycle-policy contract test (spec item 3, "a LocalChild policy fixture spawns→exits and
  emits PaneDied exactly as today") implemented as a compile-time/type-level contract
  (`TerminalLifecyclePolicy::emits_pane_died_on_reader_exit`: true for `LocalChild`, false for a
  test-only `RemoteShapedStub`) rather than an end-to-end test that actually forks a child process
  and asserts a `PaneDied` `AppEvent` arrives.
  Why: smallest reversible option — a real spawn-based fixture would be the first PTY-child-spawning
  unit test in pane.rs (none exist today, grep confirmed), adds process-lifecycle flakiness risk on
  the remote CI runner, and phase-02 is a behavior-preserving refactor where the existing
  `spawn_command_builder`/`from_handoff_fd` code (unchanged internals, only re-routed through
  `LocalChild::spawn`) is already exercised by the full existing suite as the "no behavior change"
  oracle per the phase's own req 4/TDD item 1.
  Evidence: `grep -n "PaneDied\|#\[test\]" src/pane.rs` — zero existing real-spawn PaneDied tests.
  Reversibility: additive — a real fixture test can be added later without touching this one.

- What: P3 touched two files outside its stated ownership list (main.rs/pane.rs/serve.rs/tee.rs/
  loopback.rs/mod.rs): added one `pub(crate)` accessor each to `src/app/creation.rs`
  (`terminal_runtime_for_terminal_id`, a thin combinator over the already-`pub(crate)`
  `resolve_terminal_target` + `lookup_runtime_sender`) and `src/terminal/runtime.rs`
  (`subscribe_output_bytes`/`test_process_pty_bytes_and_tee`, one-line pass-throughs matching that
  file's existing 1:1-delegate style).
  Why: the raw-byte tap (req 5) is structurally unreachable from `remote::federation::serve` without
  a way to resolve a `terminal_id` to its live `TerminalRuntime`; that resolution logic
  (`resolve_terminal_target`) already exists in `app/terminal_targets.rs` but is `pub(super)`-gated
  one level short of crate-wide, and `TerminalRuntime` had no tee-subscribe wrapper at all.
  Duplicating pane/workspace resolution logic into `serve.rs` was rejected (YAGNI/DRY; higher blast
  radius than two 1-line accessors).
  Evidence: `src/app/terminal_targets.rs:41` (`pub(crate) fn resolve_terminal_target`, pre-existing,
  `#[allow(dead_code)]`, "Staged for #00f" — untested but panic-free: filter/find only, no unwraps);
  `src/app/creation.rs` `lookup_runtime_sender` (pre-existing, `pub(super)`).
  Reversibility: trivial — both additions are new methods, zero changes to existing signatures/logic.

- What: Federation's `terminal_id` (protocol `TerminalChannelMessage`) is mapped to the SAME
  namespace as `AgentInfo::terminal_id` / `resolve_terminal_target` (not the pane's public
  `pane_id`). `AppFederationHost` resolves it via `resolve_terminal_target`, not `parse_pane_id`.
  Why: `AgentInfo` (returned by the JSON API's `AgentList`, reused for the agent-status poller)
  already carries a `terminal_id` field distinct from pane ids; using one resolver for both raw-tap
  and agent-status keeps the two channels addressing the same terminal consistently.
  Evidence: `src/api/schema/agents.rs:52` `AgentInfo.terminal_id`; `src/app/terminal_targets.rs`
  `TerminalTarget{ pane_id, terminal_id, .. }`.
  Reversibility: isolated to `AppFederationHost`'s four lookup call sites in `serve.rs`.

- What: `AppFederationHost::boot()` constructs a full session-persistence-enabled `App` (mirrors
  `server::headless::run_server`'s construction) but does NOT drive `App`'s internal `AppEvent`
  channel (the loop `HeadlessServer::run()` normally owns — `PaneDied`, OSC-52 clipboard writes,
  agent-detection publish, reported-cwd). `federation-serve`'s `EventHub`/`SessionSnapshot` state
  therefore only advances via synchronous JSON-API calls this host issues itself
  (`SessionSnapshot`, `AgentList`) — not from async pane activity.
  Why: reimplementing/reusing `HeadlessServer`'s dispatch loop needs either an `Arc`-shared `App`
  handle into a running `HeadlessServer` (would require modifying `server/headless.rs`, not owned by
  this phase) or duplicating its event-handling match arms (large, error-prone, out of scope for one
  phase's remaining budget). The raw-byte tap itself is unaffected (tapped directly at `on_read`,
  independent of the `AppEvent` channel) — this gap is specifically PaneDied/clipboard/agent-status
  *event* propagation into `EventHub`, not the terminal byte stream.
  Evidence: `src/server/headless.rs:3919` `run_server` constructs `HeadlessServer::new(app, ...)`
  then `server.run().await` — that `run()` loop (not `App::new`) is what drains `AppEvent`.
  Reversibility: additive gap, does not corrupt state — a follow-up (P4 integration, or a small
  `App::drain_pending_events()` accessor) can wire this without touching this phase's files.
  FOLLOW-UP: required before `federation-serve` is genuinely production-usable end-to-end.

- What: `federation/loopback.rs` (`FixtureHost`/`LoopbackFederationServer`) is gated
  `#[cfg(test)]` at the `mod` declaration in `federation/mod.rs`, rather than compiled
  unconditionally as the phase file's file list literally reads.
  Why: nothing outside test code constructs `FixtureHost`/`LoopbackFederationServer` — compiling it
  into release builds would make every item in it `dead_code` and fail the "no new clippy warnings"
  acceptance gate. The phase file's own prose calls it "test infra" and "usable `#[cfg(test)]` from
  P4+", which is exactly what this gate implements.
  Evidence: phase-03 spec, Requirements item 9 wording; `cargo clippy --all-targets` still exercises
  it (via the `test` target), satisfying the same acceptance criterion.
  Reversibility: trivial — remove the `#[cfg(test)]` line if a later phase needs it unconditionally.

- What: `federation-serve`'s event-stream/agent-status pollers use fixed-interval polling
  (`tokio::time::interval`, 25ms/100ms) against `FederationHost::events_after`/`agent_statuses`
  rather than an async notify-driven push.
  Why: keeps `FederationHost` a plain synchronous trait (no `async-trait` dependency, no `Pin<Box<dyn
  Future>>` boilerplate) — simpler, and the added latency (single-digit-to-low-double-digit ms) is
  far below human-perceptible terminal-output/agent-status-change cadences.
  Evidence: `src/remote/federation/serve.rs` `POLL_INTERVAL`, `spawn_event_stream_task`,
  `spawn_agent_status_task`.
  Reversibility: isolated — swapping to a notify-driven design only touches those two functions plus
  adding an async method to the trait.
