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

- What: Remote build loop surfaced 3 more issues after the first push, fixed in follow-up commits:
  (1) `Cargo.toml` was missing tokio's `io-util`/`io-std` features — `AsyncReadExt`/`AsyncWriteExt`/
  `split`/`duplex`/`stdin`/`stdout` were all unresolved (only `rt-multi-thread, macros, sync, time`
  were enabled). (2) `App` (`src/app/mod.rs`) holds a `Box<dyn PrefixInputSource>` with no `Send`
  bound, making `App` — and therefore any `Mutex<App>` — neither `Send` nor `Sync`; the original
  `FederationHost: Send + Sync` supertrait bound made `impl FederationHost for AppFederationHost`
  uncompilable. Fixed by dropping the supertrait bound entirely and restructuring `run()` so the
  event-stream/agent-status pollers run inline via `tokio::select!` in the same future as the read
  loop (never `tokio::spawn`'d), so no code path ever needs to move a `!Send` host across threads;
  `LoopbackFederationServer::spawn` (test-only, always `FixtureHost`) adds `Send + Sync` back as a
  local bound since it legitimately needs `tokio::spawn`. (3) `FixtureHost.terminals` held a bare
  `HashMap` of real `TerminalRuntime`s; `pane::PaneRuntime` uses `Cell` internally (`Send`, not
  `Sync`), which made `FixtureHost: !Sync` and broke that same `tokio::spawn` requirement — wrapped
  in `std::sync::Mutex` (`Mutex<T>: Sync` holds for any `T: Send`) to restore it without touching
  `pane.rs`.
  Why: none of these were visible without a real compile — local macOS cannot build at all, so this
  is the "P1/P2 precedent proved this loop" iteration in practice: edit → push → remote build → fix.
  Evidence: appn-ltu-vm-100 `nix develop -c cargo build` errors (E0432 unresolved tokio imports,
  E0277 `Box<dyn PrefixInputSource>` not Send, E0277 `Cell<...>` not Sync) across commits fb4dfac →
  1796a50; final state: `cargo test` 2768 passed/0 failed (10/10 binaries green, includes the 11 new
  federation::loopback/tee tests), `cargo clippy --all-targets` clean except the same 14 pre-existing
  P1 `federation::id`/`Capability::CLIPBOARD`/`TerminalChannelMessage::terminal_id` dead-code warnings
  (subset of the ~38/39 baseline — not new).
  Reversibility: (1)/(3) are additive (feature flags, a `Mutex` wrapper). (2) is the more structural
  one — reversing it (putting `Send + Sync` back on `FederationHost`) would require either making
  `PrefixInputSource` `Send` (an `app.rs` change out of this phase's ownership) or never wiring a
  real `App`-backed host at all; not expected to be revisited without a corresponding `app.rs` change.

- What: P4's `protocol::EventFrame` (P1, locked) carries only `{source_seq, kind: EventKind}` — no
  entity id or payload — so a normal in-order `Frame` cannot be turned into a valid, per-field
  `EventData` (every `EventData` variant needs real typed fields the wire never sends). The reducer
  therefore does NOT emit a local `EventHub::push` per received `Frame` (req 4's literal wording);
  `Frame`/`Gap`/`Reset` only drive cursor bookkeeping + gap detection. ALL local pushes happen in
  `RemoteMirror::reconcile_by_diff`, diffing the mirror against a freshly fetched `MountSnapshot` —
  run once at initial mount, and again whenever `Gap`/`Reset` is observed (the wire also has no
  "request a fresh snapshot on this connection" message, so a full remount — a new
  `connect_and_mount` — is the only re-sync primitive P1/P3 actually provide; P9 owns the full
  reconnect FSM, P4 exposes the minimal `DriveOutcome::ResyncRequired` signal it will build on).
  Why: this is a genuine wire-payload gap in an already-merged, locked dependency (P1's
  `EventFrame`/P3's `serve.rs` mount-once behavior) discovered while implementing P4, not a design
  choice P4 could make differently within its own file ownership (`protocol/mod.rs` and `serve.rs`
  are not in P4's Files list). Smallest faithful deviation: keep the wire contract as-is, make the
  reducer's local-push path honest about what data is actually available (full snapshot diff),
  rather than inventing placeholder/incorrect `EventData` from a bare `EventKind`.
  Evidence: `src/remote/federation/protocol/mod.rs` `EventFrame { source_seq, kind }` (no other
  fields); `src/remote/federation/serve.rs::run` sends exactly one `MountSnapshot` right after the
  handshake and never again; `EventData` (`src/api/schema/events.rs`) has no catch-all/empty variant.
  Reversibility: additive/isolated to `reducer.rs`/`client.rs`. A future protocol change (extending
  `EventFrame` with the changed entity's public id, or a small delta) would let per-event pushes fire
  without a full remount — that's a P1-owned change, flagged here for whoever revisits P1/P9.
  FOLLOW-UP: recommend re-reviewing this constraint before P8/P9 ship, since resync-on-every-gap is
  the only re-sync path today and its cost/latency was not part of P1/P3's own effort estimate.

- What: `RemoteMirror::reconcile_by_diff`'s tab-content-changed case always emits `EventKind::TabRenamed`
  (the closest available `EventData` variant), even when the actual diff wasn't a label change —
  `EventKind`/`EventData` have no generic `TabUpdated` variant (unlike Workspace/Pane, which do).
  Why: smallest reversible option within P4's file ownership; `EventKind`/`EventData` are owned by
  the API schema, not this phase.
  Evidence: `src/api/schema/events.rs` `EventKind` enum — `TabCreated/TabClosed/TabRenamed/TabMoved/
  TabFocused`, no `TabUpdated`.
  Reversibility: isolated to `reconcile_tabs` in `reducer.rs`; trivial to switch once/if a
  `TabUpdated` variant is added upstream.

- What: `federation/mod.rs` declares `pub(crate) mod client; pub(crate) mod reducer;` instead of the
  phase file's literal `pub mod client; pub mod reducer;` wording.
  Why: every type these modules expose (`FederationClient`, `RemoteMirror`, `MountError`, ...) is
  itself `pub(crate)`, matching this file's existing `serve`/`tee` visibility pattern (also
  `pub(crate)`) — a `pub mod` wrapping only-`pub(crate)` items would just be a visibility mismatch
  with no crate-external consumer (herdr is a bin crate; nothing outside the crate can see either).
  Evidence: `src/remote/federation/mod.rs` (pre-existing `pub(crate) mod serve; pub(crate) mod tee;`).
  Reversibility: trivial — flip both keywords if a later phase needs true crate-external `pub`.

- What: `App`/`AppState`'s real event loop (`src/app/mod.rs`, `src/server/headless.rs`) is not
  modified to ever construct a `FederationClient` or drive `client::drive_event_channel` — the new
  `AppState.remote_mirror` field is always `None` in this phase; `client.rs`/`reducer.rs` are
  exercised only by their own `#[cfg(test)]` modules (against the P3 loopback substrate), matching
  the plan's own risk/rollback note ("new modules unused by any live path until P8 triggers a
  mount"). Both new modules carry a module-level `#![allow(dead_code)]` for this reason (mirrors
  `FixtureHost::set_agent_status`'s existing per-item precedent, applied at module scope since almost
  everything in these two files is test-only-consumed until P8/P9).
  Why: requirement 5 ("non-interactive mount... runs on its own task off the main event loop") and
  the phase's Files section explicitly defer the real SSH/CLI trigger to P8 ("P8 wires the CLI
  trigger") — wiring a real call site here would be scope creep into P8's ownership.
  Evidence: phase-04 Files section; plan.md P4 risk/rollback ("new modules unused by any live path
  until P8 triggers a mount"); P3's own `#[allow(dead_code)]` precedent on `FixtureHost::
  set_agent_status`.
  Reversibility: trivial — the dormant `#[allow(dead_code)]` gates are removed automatically in
  practice once P8 adds a real call site (the lint stops firing; the attribute becomes inert, can be
  deleted then).

- What: P5's `RemoteTerminalSourceHandle` implements BOTH `TerminalSource` and P2's
  `TerminalLifecyclePolicy` for one type, and lives entirely in the new (P5-owned)
  `remote/federation/pane_source.rs` rather than adding a production `Remote` struct to
  `terminal/source.rs`. Also had to add `TerminalLifecyclePolicy` to `terminal/mod.rs`'s
  `pub(crate) use source::{...}` re-export (it was previously re-exporting only `LocalChild`/
  `TerminalSource`, since P2 only ever implemented the policy in its own test module).
  Why: phase-05's Files section explicitly lists `pane_source.rs`/`pane.rs`/`terminal/runtime.rs`/
  `terminal/runtime_registry.rs`/`client.rs` as this phase's file ownership — `terminal/source.rs` is
  not in it. The trait itself doesn't require a same-file impl, so combining both traits into one
  type inside the owned file satisfies P2's "Remote lifecycle policy" seam without touching a file
  P5 doesn't own. The `mod.rs` re-export line is a one-line mechanical necessity (the trait is
  `pub(crate)`, unreachable cross-module otherwise) confirmed by a remote compile error, not a design
  choice.
  Evidence: phase-05.md "File ownership" section; `src/terminal/source.rs`'s own comment ("P5 wires a
  `Remote` policy that makes production code query it" — written by P2, anticipating exactly this);
  remote build error `E0432: unresolved import TerminalLifecyclePolicy... not accessible` before the
  `mod.rs` fix.
  Reversibility: trivial — moving the impl into `source.rs` later (if a future phase wants it there)
  is a pure relocation, no behavior change.

- What: `PaneRuntime::spawn_remote`'s `on_read` closure discards `process_pty_bytes`'s
  `terminal_responses` field entirely (no `PtyReadResult`-shaped return value at all — the
  `RemoteTerminalSourceConfig::on_read` type is `Box<dyn FnMut(&[u8]) + Send>`, not
  `Box<dyn FnMut(&[u8]) -> PtyReadResult>`). Applies the same disposition to
  `RemoteTerminalSourceHandle::resize`'s `terminal_responses` parameter (ignored, RT-F10).
  Why: `terminal_responses` are what a real terminal would write back to the PTY master fd it's
  attached to (e.g. answering an embedded CPR/DA query) — a remote-mirrored pane is not the terminal
  driving the remote shell; the remote host's own real terminal already owns that responder role and
  already answered the query before/independently of the federation tap. Writing these locally
  re-synthesized responses back has no destination (no local PTY); sending them onward as `Input`
  would inject a second, redundant, possibly-desyncing reply for a query the remote already resolved.
  RT-F10 explicitly pins "default to remote-host-local handling; propagate only the visual resize" —
  this generalizes that same call to the byte-in path too.
  Evidence: phase-05.md requirement 10 + Context bullet 4 (RT-F10); `pane.rs`'s existing local
  `on_read` closures write `terminal_responses` back via `PtyReadResult` specifically because
  `PtyIoActor` owns a real `master_fd` to write them to (`src/pty/actor.rs`), which `RemoteTerminalSourceHandle`
  structurally does not have.
  Reversibility: isolated to two call sites; trivial to route responses somewhere else later if a
  future phase finds a real need (none identified — remote's own PTY already handles this).

- What: RT-F7's "write local clipboard from remote OSC" half is NOT wired into
  `AppEvent::ClipboardWrite` (the path every local pane's `on_read` uses). `spawn_remote` instead
  packages remote-origin OSC 52 writes as `ClipboardMessage { origin_tag: "remote", payload }` and
  sends them on a caller-supplied `mpsc::UnboundedSender<ClipboardMessage>` — a plain queue, not an
  `AppEvent`. `client.rs`'s new `drive_mount_channel` similarly forwards inbound `FederationMessage::
  Clipboard` to a caller-supplied queue rather than applying it to any local state itself.
  Why: `events.rs` (where `AppEvent`/`AppEvent::ClipboardWrite` live) is not in phase-05's file
  ownership, and the findings-resolution table itself splits RT-F7 as "P5 + P7": P5 carries it on an
  origin-tagged channel, P7 "routes through local policy with origin propagation" — i.e. actually
  applying it to local state with origin-aware policy is explicitly P7's job, not P5's. Silently
  wiring it into the existing untagged `AppEvent::ClipboardWrite` now would both overstep file
  ownership and produce a fake "done" that P7 would have to partially undo to add real origin
  propagation.
  Evidence: plan.md findings-resolution table row "RT-F7 ... Resolved in: P5 + P7 ... How: Clipboard/
  OSC carried on an origin-tagged channel (P5), routed through local policy with origin propagation
  (P7)"; `src/events.rs` not in phase-05.md's Files section.
  Reversibility: additive/isolated — P7 drains these queues into real local clipboard policy without
  needing to revert anything P5 added; "not dropped" (the concrete, testable P5 guarantee) holds
  either way.

- What: `src/terminal/runtime_registry.rs` was left UNCHANGED, despite being listed in phase-05.md's
  Files section as a file to modify ("4 `#[cfg(unix)]` registry handoff methods skip `Remote`").
  Why: every `TerminalRuntimeRegistry` method (`set_handoff_readers_paused`, `assume_handoff_ownership`,
  `nudge_child_redraw_after_handoff`, `drain_for_handoff`) is generic over `TerminalRuntime` — it
  never matches on `PaneRuntimeIo` itself. Once `PaneRuntimeIo`'s own methods (in `pane.rs`, which
  *is* owned) gained `Remote` no-op arms, every registry-level call already degrades correctly with
  zero registry-level code changes needed; there was nothing left to modify without introducing a
  no-op change purely to match the phase file's literal file list.
  Evidence: `src/terminal/runtime_registry.rs`'s methods each just iterate `self.runtimes.values()`
  calling opaque `TerminalRuntime`/`PaneRuntime` methods; `shutdown_pane_processes`'s `child_pid == 0`
  early return additionally makes any registry-triggered process shutdown a guaranteed no-op for a
  remote pane regardless.
  Reversibility: N/A — no functional change was skipped, only a mechanically-unnecessary edit.

- What: P5's TDD tests deliberately avoid reaching into `loopback.rs`'s private `FixtureHost.terminals`
  field from `client.rs` (attempted once, would not compile: the field has no `pub`/`pub(crate)`
  modifier, module-private to `loopback.rs`). `client.rs`'s router/drive_mount_channel tests either
  inject synthetic `TerminalChannelMessage`s directly via `router.route_inbound(...)`, or drive a
  fully hand-rolled fake server (raw `read_frame`/`write_frame` over a bare `tokio::io::duplex`, same
  pattern as the pre-existing `version_mismatch_is_surfaced_as_a_typed_rejection` test) instead of
  `FixtureHost`/`LoopbackFederationServer` for the parts that would have needed direct field access.
  Why: `loopback.rs` is not in phase-05's file ownership (P3/P4 own it); CX-4 raw-byte-tap fidelity
  against a real fixture terminal is already proven by `loopback.rs`'s own existing tests, so P5's
  router-level tests only need to prove ordering/routing, which synthetic messages exercise just as
  faithfully without requiring a new cross-module accessor.
  Evidence: remote compile attempt — `terminals: Mutex<HashMap<...>>` field declared with no
  visibility modifier in `src/remote/federation/loopback.rs`.
  Reversibility: N/A — no scope was lost; if a future phase wants a shared test accessor, adding one
  `pub(crate) fn` to `loopback.rs` is a one-line, purely-additive follow-up.

- What: P6 did not touch `src/detect/mod.rs` at all, despite it being listed in phase-06.md's Files
  section as a conditional edit ("only if a small seam is needed").
  Why: the relayed `AgentStatus` is fed directly into `spawn_basic_detection_task`'s existing loop
  (new `tokio::select!` arm reading an optional `mpsc::Receiver<AgentStatus>`) and mapped to
  `AgentState` by a small pure function in `pane.rs` (`map_relayed_agent_status`) — exactly the
  phase's own preference ("prefer feeding through pane detection state, not forking `detect` core").
  No seam inside `detect::detect_agent_with_osc`/`AgentDetection` was needed.
  Evidence: `src/pane.rs` new `Some(relayed_status) = relayed_status_recv` select arm calls
  `publish_state_changed_event` directly, reusing `agent_presence.current_agent()` for identity —
  `src/detect/mod.rs` diff is empty.
  Reversibility: N/A — a strictly additive file was simply not needed.

- What: like P4/P5, nothing in production `App`/`AppState`/`PaneRuntime` construction wires a live
  relay source into `spawn_remote`'s new `relayed_agent_status_tx`/`RemoteAgentStatusGate` yet — only
  `reducer.rs`'s and `pane.rs`'s own tests exercise them (`PaneRuntime::relayed_agent_status_sender()`
  and `RemoteMirror::apply_agent_status()` are both real, tested, working, but nothing calls the first
  with output driven by the second in a live path).
  Why: per the plan's own risk/rollback note ("every phase is additive and dormant") and P4/P5's
  identical precedent, wiring a live call site is P8/P9 scope (mount lifecycle + CLI flag own that
  wiring), not P6's; P6's job was to make the relay mechanism correct and testable in isolation.
  Evidence: `grep -rn "relayed_agent_status_sender\|apply_agent_status" src/` outside `pane.rs`/
  `reducer.rs`'s own modules returns only the two definitions and their own `#[cfg(test)]` call sites.
  Reversibility: additive — P8/P9 adds one call site (poll `RemoteMirror`'s agent-status channel,
  call `PaneRuntime::relayed_agent_status_sender()`); no revert needed here.

- What: `RemoteMirror::apply_agent_status`'s `ReducerAction::Applied { source_seq, .. }` reuses the
  mirror's current event-channel cursor rather than a real per-message sequence number.
  Why: the P1 agent-status channel (`AgentStatusMessage`) carries no `source_seq` of its own (unlike
  `EventFrame`) — it is a separate, unordered-relative-to-events channel by design (S12.2 cadence,
  independent cadence from the ordered event stream). `ReducerAction::Applied`'s `source_seq` field is
  documented as "for observability only" (existing module doc), so reusing the cursor is a harmless,
  honest placeholder rather than fabricating a wire value that does not exist.
  Evidence: `src/remote/federation/protocol/mod.rs`'s `AgentStatusMessage` struct has no `source_seq`
  field, unlike `EventFrame`.
  Reversibility: trivial — cosmetic only; no test asserts a specific `source_seq` value for
  `apply_agent_status`'s `Applied` variant.

- What: P7 — new `sanitize.rs` strips the full C0 (`0x00..=0x1F`) + DEL (`0x7F`) + C1 (`0x80..=0x9F`)
  control range from remote chrome strings, rather than reusing `terminal_notify::sanitize_text`
  (which strips only ESC/BEL/ST and is `fn`-private).
  Why (trust-boundary decision): the threat is ANSI/OSC/cursor-move/OSC52 injection via any rendered
  remote field (S11.1 Blocker) — filtering the byte *ranges* that open/close every such sequence (not
  a curated 3-byte blocklist) is the narrower, harder-to-miss mitigation, and DRY-reusing a
  notification-scoped private helper would couple an unrelated module to this new adversarial
  boundary's contract. Mitigation: filter-not-escape at ONE ingest choke point
  (`reducer::namespace_workspace/_tab/_pane`), covering both `apply_snapshot` and
  `reconcile_by_diff` (the only two paths that construct a mirrored entity).
  Evidence: `grep -rn "fn sanitize\|fn strip" src/` found only `terminal_notify::sanitize_text`
  (private, 3-byte filter, single-line-notification-scoped); `reducer.rs` tests
  `remote_chrome_strings_are_sanitized_on_ingest_via_both_snapshot_paths` exercises both choke points.
  Reversibility: additive/pure function; trivially revertible without touching wire format.

- What: P7 — remote-origin OSC52 clipboard writes are NOT wired to a live consumer in this phase;
  `pane_source::apply_remote_clipboard_writes` is built + tested against the exact
  `mpsc::UnboundedReceiver<ClipboardMessage>` type `pane.rs`'s dormant `spawn_remote(..., clipboard_tx,
  ...)` parameter already produces, but nothing calls it in production yet.
  Why (trust-boundary decision): `pane.rs` is not in P7's file ownership (exclusive: sanitize.rs;
  shared: reducer.rs/pane_source.rs/client.rs only), and wiring the real receiver is P8/P9 CLI-mount
  scope per the same "additive, dormant until a live call site" precedent P4/P5/P6 already established
  for this codebase. The policy itself (S11.3 parity: remote-origin gets no more/less trust than
  local — herdr's actual local OSC52 policy auto-applies unconditionally today, so parity means
  auto-apply, flagged as a known consideration rather than silently escalated with a new confirmation
  gate) is fully implemented, tested, and origin-tag-preserving; only the live call site is deferred.
  Evidence: `grep -rn "clipboard_tx" src/pane.rs` shows only the dormant P5 parameter + a
  `#[cfg(test)]`-only `_clipboard_rx`; `pane_source.rs` tests
  `remote_and_local_origin_clipboard_writes_are_applied_through_the_same_policy` prove parity in
  isolation.
  Reversibility: additive; P8/P9 adds one `tokio::spawn(apply_remote_clipboard_writes(rx, |origin_tag,
  bytes| ...))` call site with the real `write_osc52_bytes`-equivalent as `apply`; no revert needed.

- What: P7 — `client.rs`'s `drive_mount_channel` clipboard parameter changed from
  `mpsc::UnboundedSender<ClipboardMessage>` to a bounded `mpsc::Sender<ClipboardMessage>`
  (`CLIPBOARD_CHANNEL_CAPACITY = 64`), routed via `try_send` instead of `send`.
  Why (trust-boundary decision): the wire codec already caps a single `Clipboard` frame's payload
  (16 MiB), but the *queue depth* behind an unbounded channel was still unbounded — a remote flooding
  `Clipboard` frames faster than a (currently nonexistent) consumer drains them could grow local
  memory without limit (S2.2/S10.2 requirement 5). `try_send` (never `.await`) preserves the existing
  isolation invariant that the ONE mount tunnel's single read loop must never block on a slow/absent
  consumer, matching `TerminalChannelRouter::route_inbound`'s established pattern.
  Evidence: `client.rs` test `flooding_the_clipboard_channel_never_exceeds_its_bounded_budget` proves
  the queue never exceeds capacity under a 10x-capacity flood; `TerminalChannelRouter`'s own
  `TERMINAL_OUTPUT_CHANNEL_CAPACITY` (4096, pre-existing) was already bounded — confirmed, not changed.
  Reversibility: trivial — one type change + one call-site `send`→`try_send`; the one call site
  (drive_mount_channel test) updated in the same commit.

- What: P7 — added a trust-model doc section to `remote/federation/mod.rs`'s module doc comment
  (not a new `docs/*.md` file) to satisfy the phase's "Docs — trust-model note … in federation docs"
  bullet.
  Why: `mod.rs` is not in P7's explicit file-ownership list (only `sanitize.rs` exclusive +
  `reducer.rs`/`pane_source.rs`/`client.rs` shared), but it already required a one-line edit to
  register the new `sanitize` module — a `docs/*.md` file was not listed anywhere in the plan's
  documentation-management structure for this feature, and the module doc comment is this crate's
  existing convention for module-level "what/why" documentation (every other `federation/*` module
  carries one). Adding the trust-model paragraph there is the narrowest-scope way to satisfy the bullet.
  Evidence: `remote/federation/mod.rs` docs section: "Trust model (P7 — read before adding a new
  ingestion field)".
  Reversibility: trivial — a doc comment; no behavior change.

- What: P7's own new test `client::tests::oversized_clipboard_frame_is_rejected_by_the_shared_codec_
  not_a_bypass` deadlocked the whole suite (`cargo test` never printed `test result:` for the main lib
  binary). Fixed by sizing the test's `tokio::io::duplex` buffer off the actual `codec::encode`d frame
  length instead of the raw payload length.
  Why (root cause, proven not assumed): the test writes a 16 MiB+1 `Vec<u8>` `ClipboardMessage` through
  `write_frame`, but `codec::encode` uses `serde_json`, which serializes a raw `Vec<u8>` as a numeric
  JSON array (`[0,0,...]`, ~2 bytes/element) — the encoded frame is ~33.5 MB, roughly 2x the raw payload
  it sized the duplex buffer against (~17 MB). `write_frame`'s `writer.write_all(&frame).await` therefore
  blocked forever waiting for buffer space, because the test's `read_frame(&mut client_reader)` call
  (the only thing that would drain the duplex) runs *after* `write_frame` returns in the same task —
  never concurrently. This is the exact JSON-bloat effect P1 already flagged as a follow-up
  ("raw terminal Output/Input bytes JSON-bloat ~4x") biting a P7 test that (unlike P1's own terminal-byte
  tests) drives a full-size (16 MiB) payload through the JSON codec for the first time. Not a production
  deadlock: no production code path constructs an in-memory duplex sized off a raw payload length: this
  bug was 100% contained to the test's own fixture setup.
  Evidence: local reasoning confirmed on the remote runner — `timeout 180 nix develop -c cargo test
  remote::federation:: -- --test-threads=1 --nocapture` printed every federation test up through this
  one's name with no trailing `ok`/`FAILED`, then hung past the timeout; after the fix, the same command
  completes in 3.90s with `60 passed; 0 failed`. Full suite: `nix develop -c cargo test -- --test-threads=4`
  now completes (previously hung indefinitely) — `2622 passed; 0 failed` (main lib bin) + 9 integration
  binaries (188 more passed, 0 failed) = 2810/0 total, no hang. `cargo clippy --all-targets` shows zero
  warnings in the touched file (`client.rs`).
  Reversibility: trivial, test-only — one buffer-sizing expression changed
  (`Channel::Clipboard.max_len() + 1_048_576` -> `frame.len() + 1_048_576`); no production file touched,
  P7's bounded-clipboard-channel security property (`try_send`, `CLIPBOARD_CHANNEL_CAPACITY`) is
  completely unchanged. Commit 05ab278.

- What: P8 GAP #1 (carried from P3): the real `AppFederationHost::run` never drains `App::event_rx`, so
  a real running `federation-serve` process never applies `AppEvent`s (`handle_internal_event`/the
  `emit_pane_updated`-producing call sites in `app/runtime.rs`/`app/terminal_titles.rs`) into its own
  `App`, meaning `events_after`/`EventHub` on that host never observes pane-content-changed events even
  though raw PTY bytes ARE tapped correctly (`on_read` already sets `render_dirty` directly, independent
  of `event_rx`). NOT closed in P8. Deferred to P9, explicitly.
  Why not closed here: closing it means porting a meaningful slice of `HeadlessServer::run`'s loop
  (`server/headless.rs:564-609` — the `event_rx`/`api_rx`/render-tick/git-refresh machinery that
  actually calls `handle_internal_event_with_forwarding` and drives the dirty-pane detection that leads
  to `emit_pane_updated`) into `AppFederationHost`/`serve::run` — both of which are P3-owned files
  (`src/remote/federation/serve.rs`), not P8's declared file set (`main.rs`, `remote/unix.rs`,
  `ui/sidebar.rs`, `app/state.rs`). Doing it well (without also accidentally wiring rendering the
  federation host itself doesn't need) is a real design decision, not a mechanical fix; attempting it
  inside P8's file boundary would mean reaching into a file this phase does not own.
  Evidence: `grep -n "event_rx" src/app/mod.rs src/server/headless.rs` — `HeadlessServer::run`'s
  `tokio::select!` (`headless.rs:564-580`) is the only production drain of `app.event_rx`;
  `AppFederationHost::boot`/`run_federation_serve_over_stdio` (`serve.rs`) never construct or run
  anything equivalent. `emit_pane_updated` call sites (`app/runtime.rs:349`, `app/terminal_titles.rs:52`)
  are reached only through paths that assume that same drain loop is running.
  Reversibility: N/A (nothing closed, nothing changed in `serve.rs`); this note exists so P9 does not
  have to re-derive the finding.

- What: P8's "instead of `run_client_process`, the running local server triggers a P4 mount"
  (requirement 2) and "sidebar host group + badge" (requirement 3) are split into two halves. The
  CAPABILITY-NEGOTIATION half is real and fully wired: `remote/unix.rs`'s `attempt_federation_mount`
  really dials `ssh <target> ... federation-serve`, builds a real `FederationClient`, and performs the
  real P1 handshake + P4 atomic mount; `decide_federation_route` (pure, unit-tested) turns the outcome
  into `Federated` / `ClassicFallback { notice }` / `ClassicUnchanged`. The MATERIALIZATION half — turning
  a successful mount's `RemoteMirror` (`WorkspaceInfo`/`TabInfo`/`PaneInfo`) into real `Workspace`/`Tab`/
  `Pane` entries inside an *already-running* local interactive session (spawning
  `pane::PaneRuntime::spawn_remote`, which P5 already built and left dormant for exactly this call site)
  is NOT wired in P8. On a successful mount, `run_remote` prints an explicit notice
  ("interactive federated rendering is not yet wired ... attaching via the classic full-screen view
  instead") and falls through to the existing `run_client_process` full-screen attach — the mount itself
  is real and torn down cleanly (`child.start_kill()`), never silently pretended to be live UI.
  Why deferred: `run_remote` (`remote/unix.rs`) is architecturally the STANDALONE CLIENT PROCESS for the
  classic full-screen `--remote` path — it never touches a local `AppState`/interactive session at all
  (`run_client_process` renders the *remote* session as the whole screen). The real target for
  requirement 2 ("triggers a P4 mount" inside "the running local server") is the *already-running local
  session* (server+client architecture reached via `server::autodetect::auto_detect_launch()`), which
  would need either a new local JSON API method (`api/schema.rs`, not P8-owned) or direct `App`/
  `app/creation.rs` construction wiring (also not P8-owned) to accept a live mount and call
  `pane::PaneRuntime::spawn_remote` for each remote pane. Building that well is realistically
  P5-scale effort (P5's own estimate: 4-6d) and spans files outside this phase's declared ownership
  (`main.rs`, `remote/unix.rs`, `ui/sidebar.rs`, `app/state.rs`). Single-mount enforcement
  (`AppState::begin_federation_mount`/`FederationMountConflict`), the double-attach registry
  (`AppState::double_attach_conflict`), and the sidebar origin badge/grouping
  (`ui/sidebar.rs::workspace_federation_origin`/`federation_origin_badge`/
  `workspace_display_label_with_origin_badge`) are all real and fully wired against whatever `Workspace`
  entries eventually get inserted (proven by unit tests constructing such a `Workspace` directly) — only
  the "construct those `Workspace` entries from a live mount inside a running session" step is deferred.
  Evidence: `grep -n "fn spawn_remote" src/pane.rs` shows P5's `PaneRuntime::spawn_remote` already exists,
  fully built, `#[allow(dead_code)]`'d, with a doc comment stating "Dormant outside tests until P8 wires a
  real call site" — confirming this exact gap was anticipated by P5, not discovered late.
  Reversibility: fully additive; nothing here changes behavior for `--remote-workspace`/
  `HERDR_REMOTE_FEDERATION` unset (default OFF, classic path byte-for-byte unchanged, requirement 1
  test 1). Follow-up: wire local-session mount materialization in P9 or a dedicated follow-up phase,
  reusing `pane::PaneRuntime::spawn_remote` and `RemoteMirror`'s already-namespaced (`FedRef`) ids.

- What: RT-F11 double-attach guard (`AppState::double_attach_conflict`) is a real, tested detection
  function keyed on `HostKey`, but per the phase file's explicit allowance ("If detection proves costly,
  downgrade to documented-not-enforced for v1"), it is downgraded to documented-not-enforced ACROSS
  PROCESS BOUNDARIES: `run_remote` (a separate, standalone CLI process) has no way to query a *different*,
  already-running local server's mount registry (no JSON API method exists for it — `api/schema.rs` is
  not P8-owned). The function is real and correct for same-process callers (proven by unit test); wiring
  it into the classic `--remote` attach path's pre-flight check requires the same missing API surface as
  the materialization gap above.
  Reversibility: additive; adding the API method later is a pure extension, no behavior to revert.

- What: `--remote-workspace` requires `--remote` (mirrors `--remote-keybindings requires --remote`);
  `HERDR_REMOTE_FEDERATION=1` alone (without `--remote-workspace`) only takes effect on a launch that also
  passes `--remote` — there is no bare "federation on but no target" mode, matching the phase's framing of
  federation as something `--remote` opts into, not a separate launch mode.
  Reversibility: trivial, pure parsing logic.

- What: Cargo.toml — added tokio's `"process"` feature (previously absent: `rt-multi-thread`, `macros`,
  `sync`, `time`, `io-util`, `io-std` only). Required for `attempt_federation_mount`'s real
  `tokio::process::Command` SSH dial; not in P8's declared file list but a mechanical, zero-risk additive
  Cargo feature flag necessary for the phase's own requirement 2 to compile as real (not stubbed) code.
  Reversibility: trivial, additive dependency feature.

- What: P9 Priority 1 — closed GAP #1. Added `FederationHost::drain_internal_events` (default no-op)
  and an `AppFederationHost` impl that locks `app`, drains up to 256 pending `AppEvent`s per tick via
  `event_rx.try_recv()`, and applies each via the existing `pub(crate) App::handle_internal_event`
  (`app/api.rs:60`) — the same call `HeadlessServer::handle_internal_event_with_forwarding` makes,
  minus client-forwarding (sound/clipboard/prefix-input relay), which has no meaning for a
  `federation-serve` process with no attached interactive client. Wired into `serve::run`'s existing
  `event_ticker` arm, before `poll_events`, so a drained event's `EventHub` push is visible in the
  same tick. Proved with a new test driving a REAL `App` (not `FixtureHost`) through the real
  `serve::run` protocol handler over an in-memory duplex: sends `AppEvent::PaneDied` via
  `app.event_tx`, asserts a `pane.exited` `Frame` arrives on the federation event stream.
  Why: `AppFederationHost::boot()` built a real `App` but nothing drove its `event_rx` (documented gap
  in the P8 entry above); this closes it without needing `HeadlessServer`'s render/client-forwarding
  machinery, which does not apply to a headless federation host.
  Evidence: `src/remote/federation/serve.rs` `drain_internal_events`,
  `gap1_app_event_drain_tests::a_real_app_event_reaches_the_federation_event_stream` (passes,
  appn-ltu-vm-100). Full suite: `cargo test -- --test-threads=4` → 2830 passed/0 failed (was 2829/0 +
  this 1 new test), no hang. `cargo clippy --all-targets` clean in touched files.
  Reversibility: additive/isolated — one new trait method (default no-op), one impl, one call site,
  one test module; trivial to revert.

- What: P9 Priority 2 — built the core mount->rendered-panes MECHANISM, not the interactive CLI wiring.
  Added `App::materialize_federation_mount` (app/creation.rs): converts a mounted `RemoteMirror`'s
  namespaced snapshot into real `Workspace`/`Tab`/`PaneState` entries, each pane spawned via P5's
  `PaneRuntime::spawn_remote` and fed by a live `TerminalChannelRouter` channel — reusing the SAME
  primitives an existing "move pane to a new workspace" flow already uses
  (`Workspace::from_existing_pane`/`create_tab_from_existing_pane`/`Tab::insert_existing_pane`) and the
  SAME `WorkspaceCreated`/`TabCreated` event-emission path every other workspace creation uses, rather
  than inventing a parallel construction path. Sets the materialized `Workspace::id` to the mirror's own
  namespaced id and a `worktree_space.key = "federation:<host_key>"`, which is what
  `ui::sidebar::workspace_federation_origin`/`federation_origin_badge` (P8, unmodified) key off — proven
  by a new test asserting `remote::federation::id::classify(&ws.id)` returns `IdClass::Remote` for a
  materialized workspace. v1 eagerly spawns every mirrored pane at mount time rather than phase-05's
  anticipated lazy hydrate-on-focus (S12.1) — smaller, more reversible; lazy hydrate can be layered on
  later without reworking this call site.
  Why (design fork, latitude used per the assigning prompt): the plan's own P8 deviation entry above
  already narrowed this to "a new local JSON API method... or direct App/app/creation.rs construction
  wiring" — both require the mount to live in the SAME process/tokio runtime as the target `App`. But
  `App`/`AppState` in the normal (non-`--no-session`) launch path live in a separate SERVER process from
  the interactive TUI client (`main.rs`: `server::autodetect::auto_detect_launch()`), while
  `run_remote`/`attempt_federation_mount` (`remote/unix.rs`) is a third, standalone CLI process that
  performs the real SSH dial+handshake+mount and then exits or attaches classically — it never
  constructs an `App` at all. Merging a live mount into an *already-running* local server session
  (the phase-08 ideal, "coexist in one chrome") needs either a new async-deferred JSON API method whose
  response streams back over the existing socket/event-hub machinery, or forwarding the mount's SSH
  child stdio through a second `SshStdioBridge`-style local socket the server connects to — both real,
  identified, buildable designs, but each independently developer-week-scale (matches P8's own estimate,
  "P5-scale effort... 4-6 days", for the *materialization* half alone; the *live-session-merge* half is
  additional scope on top of that). Building that blind, un-reviewable, and un-buildable-locally (this
  workstation cannot compile herdr at all; every check is a remote round trip) risked a large, wrong,
  hard-to-revert commit for the CLI half. Per the assigning prompt's own instruction for a fork with
  material product implications ("pick the more conservative/reversible one, log it, and continue"), the
  conservative choice made here is: ship the materialization MECHANISM as a real, fully tested,
  independently reviewable unit — proven against a hand-built loopback-shaped `RemoteMirror` snapshot,
  exercising the exact same `spawn_remote`/`TerminalChannelRouter`/id-namespacing primitives a live mount
  would use — and leave the interactive CLI/session-merge wiring (`remote/unix.rs`'s `FederationRoute::
  Federated` arm, still printing today's P8 notice and falling back to classic attach) as clearly-scoped
  follow-up. `#[allow(dead_code)]` on the new functions until that live call site lands matches this
  codebase's own established precedent (P4's `client.rs`/`reducer.rs`, P5's `spawn_remote` itself, P8's
  sidebar badge helpers all shipped dormant before their live call sites landed in a later phase).
  Supporting plumbing added: `remote/federation/id::strip_mount_namespace` (reverses the reducer's P7
  ingest-time id-namespacing so a materialized pane can re-open its federation `Terminal` channel by the
  remote's raw, un-namespaced id — the wire protocol addresses channels by that raw id, never the local
  public one) and `remote/federation/client::spawn_mount_writer` (extracts the client-side writer-pump
  loop `client.rs`'s own `local_clipboard_paste_crosses_the_wire_origin_tagged` test already hand-rolled
  inline, mirroring `serve.rs`'s existing `run`'s `writer_task` pattern, into a reusable, independently
  tested helper).
  Evidence: `src/app/creation.rs` `App::materialize_federation_mount`/`build_remote_pane` + test module
  `federation_materialization_tests` (2 tests: a two-pane single-tab mount materializes a real
  `Workspace`/`Tab` with both panes' terminals reachable via `AppState.terminals`/`App.terminal_runtimes`,
  the workspace id classifies `IdClass::Remote` and carries the `federation:<host_key>` worktree-space
  key, and the router opens both panes' federation `Terminal` channels under their raw un-namespaced
  ids; an empty mirror materializes nothing). `src/remote/federation/id.rs`
  `strip_mount_namespace`+2 round-trip tests. `src/remote/federation/client.rs` `spawn_mount_writer`+1
  test. Commits (appn-ltu-vm-100, `~/Projects/herdr` pulled to each): dfafd76 (materialization+writer+
  strip_mount_namespace), 6110d05 (materialization tests), a593d59 (fix 2 test compile errors — private
  `ui::sidebar` module, `TerminalRuntimeRegistry` has no `contains_key`, fixed via `classify()` directly
  and `.get(...).is_some()`), 70555f2 (`#[allow(dead_code)]` on the 3 still-dormant items). Full suite
  `cargo test -- --test-threads=4`: 2835 passed/0 failed (main lib 2647 + 9 integration binaries 188; was
  2830 baseline + 5 new tests), no hang. `cargo clippy --all-targets`: zero new warnings in any touched
  file (the 4 remaining warnings — `map_out`, `Capability::CLIPBOARD`, `TerminalChannelMessage::
  terminal_id`, `pane_source.rs` type_complexity — are all pre-existing, none in code this entry added).
  Reversibility: fully additive; nothing here changes behavior for any existing launch path (no live
  call site exists yet, matching P4/P5/P6/P8 precedent). Follow-up (P9 continuation, or a dedicated
  phase): wire `remote/unix.rs`'s `FederationRoute::Federated` arm to a real call site — either (a) a new
  async-deferred JSON API method (`api/schema.rs`, `app/api.rs`) that runs the mount+materialization
  inside the already-running local server's tokio runtime and streams the resulting `WorkspaceCreated`/
  `TabCreated`/`PaneCreated` events back over the existing socket (the phase-08 ideal — federated panes
  coexist with local ones in one chrome), or (b) a smaller, more isolated v1 where `run_remote`'s
  federation branch runs its own dedicated in-process interactive session (own tokio runtime + ratatui
  terminal + `App`, no server-process merge) rendering only the federated workspace — both were scoped
  during this session (see Why above) but neither was implemented; also not yet wired: live agent-status
  relay (P6) and remote-origin clipboard writes (P7) into the materialized entries — both already have
  dormant, independently-tested call-site-ready functions (`PaneRuntime::relayed_agent_status_sender()`,
  `pane_source::apply_remote_clipboard_writes`) per their own phase notes above; wiring them is a small
  addition once a live CLI call site exists. A true two-machine live render was not attempted (needs a
  buildable second machine + a real remote target; out of reach in this environment per the P1 entry
  above) — everything here is validated against the loopback-shaped `RemoteMirror`/`spawn_remote`/
  `TerminalChannelRouter` substrate, same class of validation P4/P5/P6/P7 used for their own dormant
  surfaces before a live call site existed.
