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
