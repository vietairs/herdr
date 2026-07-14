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

---

- What: P9.2b decision = option (a) JSON-API-in-server (user, 260714). Scouted the two seams
  (API dispatch + federation-client tunnel lifecycle) before any code.
  Why: the two-line "(a)" description hid genuine new architecture; needed the real shapes to plan
  slice 1 correctly rather than blind-refactor the SSH-child lifetime.
  Evidence: (1) Worktree deferred dispatch is a near-exact precedent — `dispatch_deferred_api_request`
  (app/api.rs:41) → std::thread + `AppEvent::...Finished{api_request:Some({id,respond_to})}` →
  on-loop completion mutates AppState + `emit_workspace_open_events` (WorkspaceCreated/TabCreated/
  PaneCreated via emit_event→event_hub) + `send_api_response(respond_to,...)`; `materialize_federation_mount`
  event shape already matches (zero mismatch). (2) `RemoteMirror` (reducer.rs:82) is pub(crate), no
  serde → mount must be RE-PERFORMED inside the server, not passed over the socket. (3)
  `connect_and_mount` (client.rs:134) is ONE-SHOT (returns MountedConnection{mirror,agreed,reader,
  writer}, spawns nothing); ongoing loop = caller's job via `drive_mount_channel` (read, never spawned
  in prod — only in #[tokio::test]) + `spawn_mount_writer` (write, returns out_tx+JoinHandle). (4) App
  has NO tokio handle → keep-alive tunnel needs a DEDICATED runtime thread for the session life; Drop
  teardown mirrors SshStdioBridge (unix.rs:1928). (5) router + mirror SHARED between read-loop (mutates)
  and materialize (reads/opens) → Arc<Mutex<..>>. (6) clipboard type mismatch: materialize wants
  UnboundedSender, drive_mount_channel wants bounded Sender — resolve to bounded end-to-end.
  Reversibility: planning only, no code yet. Plan locked in phase-09b-materialization-call-site-option-a.md
  (3 additive/dormant slices: registry+perform_federation_mount → FederationMaterialize API+deferred
  handler → CLI-arm live flip). Build/test remote-only (nix host). Client-side call already exists:
  ApiClient::local() (api/client.rs:39) .request + .subscribe_value.

---

- What: P9.2b option (a) design REVIEWED by codex gpt-5.6-sol (xhigh, read-only) before any code.
  Verdict UNSOUND — viable direction, but not buildable as written. 5 CRITICAL + 6 MAJOR.
  Why: user asked "review first"; R7 /codex:adversarial-review <plan> gate. Vindicated — caught
  a deadlock in the core concurrency design + scope-expanding gaps before multi-day remote build.
  Evidence (full report: reports/codex-gpt56sol-adversarial-review-p9b-option-a-materialization.md):
  C1 Arc<Mutex> router/mirror DEADLOCKS (drive_mount_channel holds &mut across await forever; App
     never locks) → redesign: read-task owns mirror; cloneable router handle, per-op lock never
     across await. C2 generation fencing absent + reconnect-incompatible (route_inbound ignores
     mount_generation; reused client bumps gen but serve accepts only gen 1) → key by
     (generation,terminal_id). C3 {target,session_name} not a durable server-side SSH recipe
     (CLI owns managed ssh control/config + install + interactive stdio; detached server null stdio;
     no request timeout → hang not fallback) → needs an SSH ownership MODEL decision. C4 deferred
     dispatch won't reach prod (headless.rs:2891 + runtime.rs:72 hardcode the 2 worktree methods;
     plan omitted both) → one generic deferred dispatcher. C5 teardown hangs + frozen panes (writer
     waits all out_tx clones; reader no shutdown branch) → supervisor select!+kill+wait+LinkClosed;
     DO NOT go live before P9.3 disconnected-state FSM. M6 slice3 double-mounts (unix.rs:381 mounts
     before route) → dial_and_mount->(ChildGuard,MountedConnection), API result selects fallback.
     M7 materialization non-transactional + single WorkspaceCreated can't represent multi-workspace →
     plural DTO + atomic commit/rollback. M8 registry contradicts v1 single-mount
     (AppState::begin_federation_mount state.rs:1516); HostKey already has session (my keying Q was
     invalid); registry belongs on App not AppState. M9 clipboard/agent-status are DIFFERENT flows
     (bounded wire ingress vs unbounded OSC52 emulator out); AgentStatus frames discarded. M10 event
     loop doesn't make topology live (EventFrame = seq+kind only; nothing projects mirror deltas into
     AppState). M11 subscribe_value doesn't render/ensure-server (render is thin-client socket, not
     JSON stream) → ensure/start local server, request, attach thin client, skip classic attach.
     Verified-correct: connect_and_mount one-shot; RemoteMirror non-serde; worktree precedent valid;
     clipboard mismatch real; dedicated runtime thread viable-but-OPTIONAL (App paths already in tokio).
  Reversibility: review-only, no code. phase-09b plan now marked UNSOUND-NEEDS-REVISION; must fold
  the 11 findings + ownership redesign + 3 product decisions before cook. P9.3 FSM is now a
  PREREQUISITE for P9.2b-3 going live (was sequenced after).

---

- What: P9.2b pivoted option (a)→(b) own-in-proc-session (user, 260714, after codex UNSOUND on (a)).
  Scouted (b) feasibility; wrote phase-09b-option-b-own-in-proc-session.md.
  Why: codex CRITICALs C3/C4/M11 were all artifacts of the cross-process server model; (b) runs
  federation in the foreground --remote process (owns SSH dial + real TTY), so those don't arise.
  Evidence: (b) construction precedent = main.rs:766-843 (--no-session block: manual multi_thread
  Builder + ratatui::init() + App::new + app.run()); materialize_federation_mount proven callable on
  a fresh App pre-loop (its own #[tokio::test] creation.rs:870). CRUX = attempt_federation_mount
  kills the ssh child right after handshake (unix.rs ~339) → must restructure so tunnel + read/write
  tasks survive onto the app.run() runtime. C1 deadlock DODGED in v1: materialize synchronously
  (eager pane spawn), THEN move router into the read-loop task (sole owner) — no Arc<Mutex>. Residual
  findings scoped for v1: C2 gen-fencing (single mount=gen1, no reconnect, benign), M7 (dedicated
  session → mid-materialize fail aborts cleanly + classic fallback), M9 (bytes-only; agent-status/
  clipboard = P6/P7), M10 (renders as-of-mount, no live topology projection — headline v1 limit),
  C5 (single-proc: CancellationToken + select!, kill+wait child, rt.shutdown_timeout). 3 slices:
  b1 tunnel keep-alive restructure → b2 in-proc session runner+teardown → b3 live flip (single-dial,
  no double-mount per codex M6).
  Reversibility: planning only, no code. NEXT = codex sol re-review of the (b) plan before cook.

---

- What: codex gpt-5.6-sol re-reviewed the (b) plan → UNSOUND-but-TRACTABLE (4 CRIT + 5 MAJ + 1 MIN).
  Report: reports/codex-gpt56sol-adversarial-review-p9b-option-b-inproc-session.md.
  Why: user "review first". (b) architecture CONFIRMED sound — escapes (a)'s C1 deadlock (validated),
  C4 dispatch, M11 render; remaining defects are a bounded correctness checklist, not a dead-end.
  Evidence: CRIT1 = federation-serve boots a DUPLICATE persistence App (serve.rs:476 restores fresh
  shells) not the live remote session; EOF kills its children → violates "disconnects never kills"
  (plan.md:141). = pre-existing CARRIED GAP #1 surfacing as a blocker; ORTHOGONAL to (a)/(b) — a
  REMOTE-SIDE decision (proxy live daemon vs durable federation-serve runtime). CRIT2 tunnel-failure
  invisible (await only app.run(); no completion branch app/mod.rs:1060) → supervise App+reader+writer
  +child. CRIT3 post-start classic fallback UNSAFE (App::run detached stdin thread, no cancel
  raw_input.rs:441; fallback child inherits stdin) → v1 default = exit-to-shell on post-start failure,
  classic fallback only pre-run. CRIT4 named session omits --session (unix.rs:186) → default-session
  data under named identity → exec herdr --session <name> federation-serve (easy). MAJ5 dedicated App
  needs explicit federated MODE (activate ws, terminal mode, disable local creation) else App::run
  makes a local workspace. MAJ6 route API needs already-successful mount → single live dial, match
  result. MAJ7 M10 close/gap/overflow are CORRECTNESS bugs not doc non-goals (remote close sends no
  Terminal Close serve.rs:417; gap stops demux; overflow drops VT bytes) → need close-propagation +
  fail-fast gap + overflow-marks-desync. MAJ8 clipboard type (unbounded vs bounded) → two discard
  sinks. MAJ9 teardown needs RAII guards + kill-on-drop; CancellationToken NOT in deps (Cargo.toml:38,
  tokio only). MIN10 add release-mode gen equality check not just a comment.
  Reversibility: review-only, no code. (b) plan = REVISE (fold 10 findings) after 2 user decisions:
  #1 remote proxy-vs-durable (the crux); #2 post-start-failure exit-vs-fallback (default: exit).

---

- What: (b) plan revised to v2 folding 2 user decisions + 10 codex findings + remote-side scout.
  D1 remote=PROXY-live (co-locate in HeadlessServer, scout-backed default), D2 post-start-fail=exit.
  Why: proxy-live has NO existing wire seam — federation needs raw PTY bytes (subscribe_output
  serve.rs:76,582) reachable ONLY in-process via TerminalRuntime::subscribe_output_bytes(); JSON API
  socket is RPC-only, client socket is rendered-frames-only. So proxy = co-locate federation inside
  the live server (reuse App accessors, zero new wire protocol) > new streaming RPC (more surface).
  Evidence: AppFederationHost::boot (serve.rs:476) = fresh persistence-restored App (restore.rs:64
  "each pane gets a fresh shell") = duplicate; federation-serve is a SEPARATE process from the live
  herdr server (unix.rs:186 exec herdr federation-serve, never touches HeadlessServer). Fix =
  HeadlessServer opens a federation unix-socket listener backed by live App; federation-serve becomes
  a thin stdio<->socket proxy. Plan now 4 slices: b0 REMOTE co-location (biggest/riskiest, needs its
  own sub-scout) → b1 local tunnel keep-alive+single-dial+RAII (add tokio-util for CancellationToken,
  not a dep yet Cargo.toml:38) → b2 local session runner (federated MODE disabling local ws creation,
  CRIT2 supervision, MAJ7 close/gap/overflow correctness, MAJ8 two clipboard sinks, C5 teardown) → b3
  live flip + CRIT4 --session fix. Scope = TWO-SIDED (remote+local), multi-week, remote-build-only.
  Reversibility: planning only, no code. NEXT = (user) final codex re-review of v2 OR start b0.

- 260714 v2 codex gpt-5.6-sol re-review = UNSOUND (direction vindicated, design not executable).
  What: 3rd consecutive UNSOUND verdict on the P9.2b design; report saved
  reports/codex-gpt56sol-adversarial-review-p9b-option-b-v2-colocation.md (2 CRIT + 6 MAJ).
  Why it matters: TWO potential option-(b) killers CLEARED — (1) CRIT2 supervision IS feasible
  (outer tokio::select! around a block_on-driven-inline app.run(); App !Send irrelevant when not
  spawned); (2) co-location DOES satisfy disconnects-never-kills (HeadlessServer owns PTYs
  headless.rs:192; federation output = broadcast subscriber serve.rs:582 not runtime ownership;
  EOF only aborts that conn's forwarders serve.rs:281). So D1 co-location is the correct arch.
  Evidence of remaining blockers: (C1) b0 has NO legal live-App sharing model — HeadlessServer owns
  App by value + holds &mut self across its whole select! loop (headless.rs:430,564), App is !Send
  (serve.rs:46), direct event_rx drain steals events + bypasses forwarding handler (headless.rs:1836)
  → FIX = bounded async FederationCommand ACTOR SEAM (reply channels) driven FROM the headless loop,
  mount atomic drain→snapshot→cursor, live drain_internal_events becomes no-op. (C2) federation
  socket needs first-class server-owned lifecycle (session-scoped path, owner-only, accept-shutdown,
  unlink-before-handoff, rollback, replacement-readiness) + DELETE duplicate-App legacy-boot (reverses
  D1). MAJ: proxy must use ensure_remote_server_RUNNING (unix.rs:464) not _ready (returns ok for
  NotRunning unix.rs:1275) + typed timeouts (connect_and_mount unbounded reads client.rs:147,186);
  close/lag/overflow = FAIL-FAST typed fatal outcome for THIS milestone (no reopen/resnapshot protocol
  exists — open_terminal one-shot client.rs:313); federated mode misses 6 creation bypasses
  (navigate/modal/panes/layouts/agents/plugins) → SessionKind::Federated central forbidden-mutation
  policy at normal+deferred dispatch; single-controller lease undefined (serve.rs:33,392); teardown
  RAII (ChildGuard + armed terminal guard, drain SSH stderr); D2 exit-to-shell CONFLICTS with parent
  acceptance-criterion-4 (plan.md:147 is the real disconnects-never-kills line, not :141) → defer AC4
  to P9.3 or revise. Slices: b0 BLOCKED (actor seam + socket lifecycle) / b1 conditionally buildable
  (single dial sound) / b2 blocked / b3 not ready. Start b0: headless command/response seam + socket
  lifecycle; first test = mount live App, stream PTY, disconnect, prove PTY alive.
  Reversibility: planning only, no code. NEXT = (user decision) v3 design pass for the actor seam +
  socket lifecycle + resolve 3 unresolved (AC4 defer? lazy-start vs clean fallback? single-controller
  vs observers?) BEFORE any code — NOT building; review-first stance held, review did not pass.

- 260714 v3 written; v3 codex re-review BLOCKED by spend cap.
  What: user chose v3-design-pass-then-re-review. v3 written to phase-09b-option-b-own-in-proc-session.md.
  Then launched v3 codex gpt-5.6-sol review — it exited 0 but produced NO review: "ERROR: You hit your
  spend cap set by the owner of your workspace." Codex/OpenAI workspace spend cap exhausted.
  Why it matters: the v3 design is grounded by a HeadlessServer seam scout — CRIT1 is structurally
  UNBLOCKED: the actor seam ALREADY EXISTS (server_event_rx mpsc + handle_server_event headless.rs:2473
  = sole &mut self dispatch; classic clients already use it). v3 extends ServerEvent with
  Federation(cmd,oneshot); b0 = b0.1 actor variants / b0.2 server-owned federation socket + handoff
  integration / b0.3 fail-fast TunnelFault / b0.4 delete duplicate-App; b0-proxy stdio proxy; b1
  single-dial+timeouts+RAII; b2 SessionKind::Federated central policy (all 6 bypasses)+supervision+
  fail-fast+teardown; b3 flip. 3 decisions adopted from codex defaults (D2 intermediate+AC4→P9.3;
  lazy-start via ensure_remote_server_RUNNING+legacy-boot-deleted; single-controller v1).
  Evidence: bg task bdw81opn2 output = spend-cap error, not a verdict.
  Reversibility: planning only, no code. NEXT = (user) raise codex spend cap → re-run v3 review, OR
  authorize a Claude Fable/Opus adversarial pass as a stopgap, OR hold. Review-first stance HELD — not
  building until v3 gets an independent adversarial pass.

- 260714 v3 codex re-review SUCCEEDED (retry) = UNSOUND but CRIT1 RESOLVED, architecture VALIDATED.
  What: user said "try codex again, else fall back to fable 5 max". 1st codex launch hit spend cap;
  retry ran clean. Report reports/codex-gpt56sol-adversarial-review-p9b-option-b-v3-actor-seam.md.
  Why it matters: codex EXPLICITLY confirms the deepest unknown is closed — "the existing actor seam
  truly resolves the original live-App borrow/lock problem"; broadcast::Receiver<Bytes> back through
  the oneshot is ownership-sound (runtime.rs:494 by value, no App borrow, drop = only that subscriber);
  ordinary request mid-drain does NOT deadlock. Co-location is viable. 4th UNSOUND but the findings are
  now a bounded correctness checklist, not architecture dead-ends.
  Evidence of remaining blockers: (C1-new) handoff runs synchronously INSIDE the actor (headless.rs:2805)
  → can't join a controller thread waiting on that actor without deadlock; unlink doesn't close streams
  (ipc.rs:246); rollback re-enables dispatch (headless.rs:1161) → need connid + Free/Reserved/Mounted
  FSM + cancellation + bounded non-actor join + per-command authz. (C2-new) fault can't go through the
  full DATA queue (serve.rs:233 unbounded, once full the fault is stuck) → SEPARATE first-fault-wins
  control lane + versioned wire fault (protocol/mod.rs:215 has none) + retained TunnelTasks→typed
  TunnelExit + bounded both dirs. MAJ: sync trait from async controller (serve.rs:46,177) + one thread
  can't block-read AND pump output → async RPC adapter or explicit thread topology; correct API seq =
  forwarding-aware drain → handle_api_request_after_internal_events_drained (headless.rs:2848) NOT
  generic App::handle_api_request (api.rs:829 non-forwarding drain); federation socket needs FULL
  lifecycle (replacement-readiness headless.rs:4110, rollback :1132, Drop :3798, stop-wait session.rs:236)
  + do NOT SCM_RIGHTS-transfer (handoff.rs:33 only pane runtimes), fresh listener + no controller on
  replacement; AcquireController-before-Accept (serve.rs:201 sends Accept before mount) + per-controller
  opened-terminals; central policy misses navigate.rs:769/880/935, api.rs:450 respawn, agent_resume.rs:204,
  panes.rs:777 pane.move + MUST be construction-time App::new_federated (app/mod.rs:353 restores runtimes,
  :1132 default shell) NOT post-construction SessionKind; socket naming inherit socket_paths.rs:14
  override precedence. Buildability: b0.1 actor prototype BUILDABLE after connid/admission + forwarding
  sequence; b0.2/b0.3/b2 blocked; b1 plausible.
  Reversibility: planning only, no code. NEXT = (user decision) v4 fold+re-review OR build b0.1 (codex
  says buildable) letting code+tests surface the rest OR pause. Review-first held — not building until
  user chooses.

- 260714 v4 written + v4 codex review launched.
  What: user chose v4-fold+one-more-review. v4 folds all 7 v3 findings; resolves v3's 4 open questions
  with conservative defaults (D4). b0 now 4 sub-slices: b0.1 actor seam (connid cmds + oneshot +
  forwarding-aware drain→handle_api_request_after_internal_events_drained headless.rs:2848, never await
  AppEvent before reply, reader-thread + separate writer pumping broadcast::Receiver, read-only polls
  return false) / b0.2 lease FSM Free|Reserved(connid)|Mounted(connid) + AcquireController-before-Accept
  + handoff revoke+close-streams+cancel-replies+bounded-wait-WITHOUT-actor-join (the C1 deadlock fix) +
  compare-and-clear release / b0.3 versioned wire fault on SEPARATE control lane + bounded queues both
  dirs + retained TunnelTasks→TunnelExit / b0.4 full socket lifecycle (construction/accept-cancel/
  shutdown/Drop/stop-wait/replacement-readiness/rollback, NOT SCM_RIGHTS-transferred, *-federation.sock
  via socket_paths.rs:14 precedence) + delete AppFederationHost::boot. b2 = App::new_federated
  construction-time (restoration disabled) + exhaustive mutation classifier + low-level PTY guard
  (adds navigate.rs:769/880/935, api.rs:450, agent_resume.rs:204, panes.rs:777).
  Why: user wants one more adversarial pass before any code (strict review-first held despite CRIT1
  resolved). Defaults chosen conservatively; review is the safety net.
  Evidence: bg task bs7g55vtc running codex gpt-5.6-sol xhigh.
  Reversibility: planning only, no code. NEXT = await v4 verdict → if SOUND/SOUND-WITH-CHANGES start
  b0.1 on nix host; if UNSOUND fold again. Not building until verdict.

- 260714 v4 codex review = UNSOUND (5th consecutive); architecture still validated; escalating decision.
  What: v4 review DONE (bg bs7g55vtc, clean run). 4 CRIT + 5 MAJ + 2 MIN. Report saved
  reports/codex-gpt56sol-adversarial-review-p9b-option-b-v4-lease-fault-persistence.md.
  Why it matters (META, honest): architecture VALIDATED since v2 ("D1 co-location does not need
  reversal") and CRIT1 resolved in v3 — but the CRIT count went 2(v2)→2(v3)→4(v4). It rose because v4
  SPECIFIED more (delete AppFederationHost, App::new_federated), which exposed 2 genuinely-new ownership
  CRITs: C3 App::new_federated can CLOBBER the user's classic session (no_session controls persistence
  not just restore, app/mod.rs:366-405,1107-1110; materialization schedules a save creation.rs:681-684)
  → need SessionPersistencePolicy::Disabled; C4 deleting AppFederationHost removes the sole
  ServerInstanceId owner (serve.rs:470-500) → handshake/mount identity + AC3 restart-fencing lost →
  HeadlessServer owns+rotates it. Prior CRITs refined not closed: C1 revocation still non-linearizable
  (accept spawns before actor registration client_accept.rs:12-40 → stale queued Acquire/Mount resurrect
  authority after rollback → accept_epoch on every command + close-admission-before-revoke); C2 sole
  writer blocks mid-frame in write_frame().await (serve.rs:168-175) so a fault can't reach the wire →
  per-conn supervisor with independent shutdown(Both). 5 MAJ (task-graph contradiction, eager-open vs
  bounded-queue startup overflow, D4 needs CLOSED ALLOWLIST, untyped socket unlink + path-length unsafe
  ipc.rs:246/unix.rs:2197, proxy transparent-vs-handshake). Codex: "NOT small implementation-time folds
  ... a focused v5 design round is warranted."
  Evidence: 5 codex reports now in reports/ (a, b-v1, b-v2, b-v3, b-v4).
  Reversibility: planning only, no code. NEXT = user decision — the review loop keeps finding real
  correctness surface each pass; this is a genuine multi-week live-daemon systems change. Options: v5
  targeted fold+review (codex-recommended) / pause+checkpoint (design record preserved, PR #1 stands) /
  build b0.1-minus-gaps blind (codex: not buildable as written — higher risk). Not building until chosen.

- 260714 v5 written + v5 codex review launched (with a BUILD-NOW-vs-ROUND calibration ask).
  What: user chose v5-fold+one-more-review. v5 folds all 11 v4 findings + adopts D5 (ServerInstanceId
  fresh-per-boot+rotated; federation-serve transparent transport only). Closed MIN11 by editing
  plan.md:143-146 to record the P9.2b AC4 exit-to-shell exception (reconnect→P9.3 final acceptance). D4
  upgraded to a CLOSED ALLOWLIST (allow only read-only+navigation+remote-input+remote-terminal-resize;
  all else forbidden; pane.resize disambiguated). b0.1 = actor seam + HeadlessServer-owned ServerInstanceId
  + ONE connection supervisor (reader+poller+forwarders+single serializer) + MIN10 drain-causality; b0.2 =
  {accept_epoch,connid} on EVERY command + close-admission-before-revoke + register-before-enqueue
  (linearization); b0.3 = per-conn supervisor first-cause + independent shutdown(Both) forcing EOF + bump
  FEDERATION_PROTOCOL_VERSION; b0.4 = typed unlink outcomes + unlink bitmap + hash-safe *-federation.sock;
  b2 = SessionPersistencePolicy::Disabled (all write paths) + read-before-Open ordering + closed allowlist.
  Why: 5th round; prompt explicitly asks codex to give BUILD NOW vs ONE MORE ROUND + separate genuine
  build-blockers from implementer-resolvable details, to avoid an open-ended review loop.
  Evidence: bg task bo2yqp9s6 running codex gpt-5.6-sol xhigh; clean launch (no cap).
  Reversibility: planning only, no code (+ 1 plan.md doc edit for the AC4 exception). NEXT = await v5
  verdict + its build-now recommendation.

- 260714 v5 codex review = SOUND-WITH-CHANGES → BUILD NOW. DESIGN CONVERGED after 5 rounds.
  What: codex verbatim "BUILD NOW. No further design round." 0 CRIT; 5 MAJ + 3 MIN, all compiler/test-
  guided constraints to fold into slice tests (NOT design blockers). Report saved
  reports/codex-gpt56sol-adversarial-review-p9b-option-b-v5-build-now.md. All 5 load-bearing claims YES.
  Why it matters: the review-first insurance is PAID IN FULL — option (a) dead-end killed pre-code,
  option (b) architecture validated + hardened across v2→v5 (CRIT count 2→2→4→0). Constraints to enforce
  in tests: per-iteration actor-drain budget (handoff-starvation, headless.rs:1384); byte permits before
  encode + chunk replay/output (tee.rs:27, pane.rs:1041, protocol/mod.rs:203); ONE exhaustive Method
  classifier at BOTH api.rs:829 sync + api.rs:41 deferred entrances (+ runtime.rs:60 worktree intercept);
  local-spawn permit before pane.rs:2266 spawn_with_portable_pty + reject detached custom cmds
  navigate.rs:769/845; gate app/mod.rs:1432 clear_history() on SessionPersistencePolicy; monotonic epoch
  never restored on rollback; partial-header EOF vs clean EOF (serve.rs:129); eager-open split
  (client.rs:308). Build order: b0.1 (buildable+DORMANT, no listener until b0.4) → b0.2 → b0.3 → b0.4 →
  b0-proxy → b1 → b2 → b3; protocol-version bump lands with first wire-shape change.
  Evidence: 5 codex reports in reports/ (a, b-v1..v5). bg task bo2yqp9s6 exit 0.
  Reversibility: planning only, no code yet. NEXT = start b0.1 on nix host gpu-ml — but this is the
  USER's own herdr checkout (branch feat/remote-workspace-federation, draft PR #1); MUST confirm go +
  branch/commit approach before writing code + the remote build loop (edit→push→ssh pull→cargo test).

- 260714 BUILD STARTED. User greenlit b0.1 + "commit directly on PR-1 branch". Design record committed
  0ddea08 + pushed. b0.1 FIRST BRICK (C4 foundation): ServerInstanceId::fresh() constructor added to
  remote/federation/id.rs (mints <pid>-<nanos>-<seq>, DRY over serve.rs's private fresh_server_instance_id
  which stays until AppFederationHost deletion in b0.4); HeadlessServer now owns
  federation_server_instance_id (fresh per boot) + a #[cfg(unix)] accessor; both dormant (no listener
  until b0.4). Test: fresh()-distinct-within-process in id.rs.
  Build-loop DECISION: macOS can't compile herdr; remote host SSH alias = appn-ltu-vm-100 (hostname
  gpu-ml, 125 cores, nix at /nix/var/nix/profiles/default/bin/nix), checkout at
  /home/hvnguyen/Projects/herdr. To avoid pushing UNVERIFIED commits to the shared PR-1 branch, loop =
  edit locally → rsync changed files to remote → nix develop -c cargo test there → only commit+push to
  origin once GREEN. (Remote git hard-reset was classifier-blocked + unnecessary: remote HEAD 41f07ef
  has identical CODE to origin/feat, newer commits docs-only.)
  Why: first brick proves the remote build loop end-to-end on the smallest safe dormant change before
  the larger b0.1 actor-seam work.
  Reversibility: 2-file additive change, uncommitted until remote build green.

- 260714 b0.1 first brick GREEN + SHIPPED (dd7335c, pushed origin/feat).
  What: ServerInstanceId::fresh() + HeadlessServer per-boot identity ownership + replacement rotation.
  Remote build proved the loop: cargo test --bin herdr remote::federation::id → 8 passed (new test
  incl.), 2640 filtered, compiles clean. Binary crate (no lib target) → must use --bin herdr, not --lib.
  SURPRISE: TWO HeadlessServer initializers — ::new (headless.rs:391) AND the replacement-server
  construction (headless.rs:4220, post-live-handoff); both need the field. The 4220 site fresh()es a NEW
  id = exactly v5's rotation-on-replacement requirement (free correctness win).
  Pre-existing unrelated warning left alone (scope): TerminalChannelMessage::terminal_id dead at
  protocol/mod.rs:158 (P4-era dormant accessor, not my file). My change added zero new warnings.
  Reversibility: shipped; revert = drop dd7335c.
  NEXT b0.1 brick (larger): ServerEvent::Federation(cmd, {accept_epoch,connid}, oneshot) variant +
  FederationCommand/Reply enums + handle_server_event arms (forwarding-aware drain →
  handle_api_request_after_internal_events_drained headless.rs:2848; never await AppEvent pre-reply) +
  connection-supervisor skeleton. Still dormant (no listener until b0.4).

- 260714 b0.1 actor seam GREEN + SHIPPED (ee3804a).
  What: new src/server/federation_actor.rs — FederationCommand enum (Mount/EventsAfter/SubscribeOutput/
  ScrollbackReplay/SendInput/Resize/AgentStatuses, each value-producer carries a oneshot reply) +
  dispatch(&mut App, cmd) mirroring AppFederationHost onto the LIVE App via
  handle_api_request_after_internal_events_drained (NOT handle_api_request — that one drain_all_internal_
  events NON-forwarding, api.rs:830; the _after_ variant assumes the loop already drained WITH forwarding
  via its event_rx arm). ServerEvent::Federation variant + handle_server_event arm (returns false = MIN10
  render-inert). 4 unit tests green (2648 unaffected).
  SURPRISES: (1) ServerEvent derives Debug → the variant forced a manual Debug impl for FederationCommand
  (channels aren't Debug) — prints variant+plain fields only. (2) oneshot::Receiver::try_recv needs &mut.
  (3) EventCursor has no PartialEq/Debug → mount test asserts delivery not equality. Dormant via
  #[allow(dead_code)] on enum+variant (matches id::map_out precedent).
  Reversibility: shipped; revert = drop ee3804a. b0.1 = DONE (identity + actor seam); the connection
  supervisor topology folds into b0.3 fault-lane / b0.4 socket where the stream exists.
  NEXT: b0.2 controller-lease FSM as a PURE testable primitive (Free/Reserved(epoch,connid)/Mounted;
  monotonic epoch never-restored-on-rollback; close-admission→increment-epoch→revoke; compare-and-clear
  release) — standalone module + unit tests, dormant; wired into HeadlessServer/handoff in a later brick.

- 260714 b0.2 lease FSM GREEN + SHIPPED (abc6a35).
  What: src/server/federation_lease.rs — pure FederationLease state machine (Free/Reserved/Mounted keyed
  by (accept_epoch, connid)). try_acquire (single-controller→Busy; StaleEpoch/Closed before Busy check),
  try_mount (holder-only promotion), is_mounted_controller (authz), release (compare-and-clear so late EOF
  can't drop a newer lease), begin_revocation (close admission→++epoch→free, returns revoked connid),
  reopen_admission (epoch never restored). 8 tests incl. the exact codex resurrection-hole case
  (post-rollback stale acquire/mount inert). 2652 unaffected. Pure, no I/O, #![allow(dead_code)] dormant.
  Reversibility: shipped; revert = drop abc6a35. Wiring into HeadlessServer + perform_live_handoff is a
  later brick (needs the socket/accept context, b0.4).
  NEXT: b0.3 fault primitives — TunnelExit typed outcome enum + a first-cause cell (record FIRST cause,
  ignore subsequent — codex "secondary EOF must not overwrite the initiating cause"), pure+tested+dormant;
  the versioned wire fault message + bounded-egress rewire come with the socket/stream (b0.3 tail / b0.4).

- 260714 b0.3 fault primitives GREEN + SHIPPED (b27ff1d). CHECKPOINT: b0 pure-primitive foundation done.
  What: src/server/federation_fault.rs — TunnelExit typed outcome enum (PeerClosed/WriterFailed/
  ChildExited/TaskPanicked/ServerTerminalClosed/Lagged/EgressOverflow/LocalQueueOverflow/
  GenerationMismatch/EventGap; is_clean()) + FirstCauseCell (Mutex<Option>, set-if-empty first-wins,
  Send+Sync for Arc-sharing across reader/writer/child tasks). 4 tests incl. concurrent single-winner.
  4 CODE BRICKS SHIPPED this session, all green+pushed to PR-1: dd7335c identity / ee3804a actor seam /
  abc6a35 lease FSM / b27ff1d fault primitives. = ALL of b0's clean, dormant, PURE primitives.
  Why checkpoint here: remaining work is NO LONGER dormant primitives — it's live I/O WIRING:
  (b0.3-tail) versioned wire-fault frame + protocol version bump + bounded-egress rewire (touches
  protocol/mod.rs+codec+serve.rs); (b0.4) server-owned unix socket + actor-polled accept + WIRE the
  lease+actor+first-cause into a real listener + integrate perform_live_handoff (headless.rs:900-1108)
  revoke/close/unlink/rollback/readiness + delete AppFederationHost; (b0-proxy) transparent stdio proxy;
  (b1) tunnel keep-alive+single-dial+timeouts in remote/unix.rs; (b2) App::new_federated +
  SessionPersistencePolicy::Disabled + closed-allowlist mutation guard (touches many app/ files) +
  eager-open ordering + supervision/teardown; (b3) run_remote flip. These are bigger multi-file bricks
  needing careful reads of unfamiliar seams (handoff FSM, App::new, API dispatch chokes) — a natural
  boundary for a fresh focused effort rather than the tail of a long session.
  Reversibility: all 4 bricks additive+dormant behind #[allow(dead_code)]; production behavior unchanged
  (no federation listener exposed). Revert any = drop its commit.
  NEXT (b0.4, the wiring keystone): server-owned federation unix socket + accept loop that mints connids,
  registers each connection at the lease's current epoch, drives FederationCommands through
  server_event_tx, and integrates handoff revocation. This is where the dormant primitives light up.

- 260714 b0.4 sub-primitives GREEN + SHIPPED: socket-path (6dbea3d) + typed-unlink (1f733f9).
  TYPED-UNLINK BUILD SURPRISE: first NotOwner test relied on remove+recreate giving a new inode, but the
  fs REUSED the inode (dev+ino identical) → identity matched → Removed. Fixed by fabricating a mismatching
  SocketFileIdentity{dev:MAX,ino:MAX} instead of trusting inode non-reuse.
  What: (a) socket_paths::federation_socket_path(classic) — sibling of the classic client socket (inherits
  session/env override precedence for free), name embeds a DefaultHasher(fixed-seed, deterministic per
  binary) hash of the full classic path → INJECTIVE (kills x vs x.sock collision) + readable stem truncated
  to a SUN_PATH_BUDGET=100 budget. 4 tests. (b) ipc::remove_socket_file_if_owned_typed → SocketUnlink
  {Removed|Absent|NotOwner}; existing remove_socket_file_if_owned refactored to delegate (DRY, behavior
  unchanged for its 5 callers). Lets handoff rollback restore ONLY actually-removed sockets (MAJ8). 1 unix
  test (removed/absent/not-owner).
  Why: these are the last clean isolable b0.4 primitives; the accept-loop/handoff INTEGRATION is the next
  (big) brick.
  Reversibility: additive+dormant; revert = drop commits.
  NOW 6 code bricks shipped this session (identity, actor seam, lease FSM, fault, socket-path, typed-unlink)
  + design/progress docs. Remaining b0.4 = the actual listener/accept-loop + wire lease+actor+first-cause+
  socket-path+typed-unlink together + perform_live_handoff integration + delete AppFederationHost. Then
  b0-proxy, b1, b2, b3, R7 tail.

- 260714 b0.4 KEYSTONE started — WITH AGENTS. 3 parallel hvn-scout maps (client-accept pattern / federation
  wire handshake / socket-lifecycle sites) → synthesized into phase-09b-b04-accept-loop-keystone.md
  (execution spec + the sync-trait-vs-async-oneshot crux + decision A recommendation). Then delegated
  sub-brick 1 to hvn-implementer (owned headless.rs+session.rs; additive-only; couldn't build).
- 260714 b0.4 sub-brick 1 GREEN + SHIPPED (d0f166f) — FIRST LIVE (non-dormant) change.
  What: HeadlessServer now BINDS a server-owned federation unix socket (sibling of client socket via
  federation_socket_path) + full lifecycle: both constructors, handoff unlink (typed) between send_fds
  and wait_ready, rollback rebind in restore_public_sockets_after_failed_handoff, cleanup_sockets/Drop,
  session.rs stop-wait. Nothing accepts yet. Unix-only (D4). FULL SUITE 2669/0, 0 warnings.
  Process note: implementer's edits mirrored the client recipe faithfully at every mapped site; I
  reviewed the handoff/rollback diff + ran the FULL suite (not scoped) since it touches live handoff.
  GOTCHA: chained rsync of 2 files to a single-file dest silently errored → remote headless.rs was STALE
  → first build tested old code. Fixed by per-file rsync + md5 checksum verify. LESSON: verify remote
  checksum after rsync when it matters.
  Reversibility: additive; revert = drop d0f166f (but it changes live startup — every server binds the
  socket now). NEXT = b0.4 sub-brick 2 (THE CRUX): accept loop + async connection driver (decision A:
  purpose-built async driver awaiting oneshot, NOT the sync FederationHost trait). Needs: federation_lease
  field on HeadlessServer + lease ops (AcquireController/ReleaseController) added to FederationCommand
  serviced against self.federation_lease + connid/accept_epoch threading + exclusive serializer +
  first-cause supervisor. Large careful async brick — fresh-context boundary. Full spec in the keystone md.

- 260714 b0.4 sub-brick 2 SPLIT into 2a/2b/2c to keep each remote round-trip cheap + each commit green:
  2a lease/actor integration (dormant) / 2b accept loop + handshake-only conn thread / 2c the async
  connection driver (mount → select! loop, exclusive serializer, first-cause supervisor).
- 260714 b0.4 sub-brick 2a GREEN — lease↔actor integration (dormant).
  What: `dispatch` now takes `&mut FederationLease` alongside `&mut App` (one servicing home, DRY), so
  admission (AcquireController), lease-gated Mount{epoch,connid}→Option<(snapshot,cursor)>, Release, and
  mounted-controller authorization on SendInput/Resize are ALL linearized at the single event-loop
  dispatch point. HeadlessServer owns a `federation_lease` field (both constructors + fresh-on-handoff-
  replacement). 6 new actor tests: full acquire→mount→authz→release + busy + stale-mount-inert.
  Why: the lease can't be serviced inside dispatch(&mut App,..) — it lives on HeadlessServer; threading
  it through dispatch (vs splitting lease logic into the headless arm) keeps all federation-command
  servicing in one unit, unit-testable against a fresh lease without building a whole HeadlessServer.
  Evidence: `cargo test --bin herdr federation` 99/0; FULL suite 2673/0.
  GOTCHA: full suite surfaced a pre-existing latent warning `TerminalChannelMessage::terminal_id() never
  used` (protocol/mod.rs:158) — NOT caused by 2a (my diff never touches that file/callers; incremental
  compile hadn't re-surfaced it at sub-brick 1). It's a dormant tag accessor symmetric with the *used*
  mount_generation(); the b0.3-tail/2c wire router will consume it. Annotated #[allow(dead_code)] per the
  local dormant-federation-scaffolding convention (id::map_out etc.) rather than deleting churn.
  Reversibility: additive+dormant (field read only in the never-reached Federation arm); revert = drop commit.
  NEXT = 2b accept loop + handshake-only connection thread (socket finally accepts).

- 260714 b0.4 CRUX RESOLVED — I/O topology = decision B (SYNC thread topology), reversing the keystone's
  earlier decision-A lean. Evidence: codec::encode/decode are PURE (byte-slice, no Read/Write coupling —
  codec.rs:6-8); LocalStream is sync Read+Write with try_clone() into independent halves; the classic
  client transport ALREADY drives a connection this exact way (handle_client_handshake → try_clone →
  client_writer_loop thread + client_read_loop, bridging via server_event_tx.blocking_send —
  client_transport.rs:429-579). So a federation connection = reader thread (blocking sync-framed reads →
  FederationCommand via blocking_send + oneshot::blocking_recv) + writer thread (drains mpsc<Federation
  Message>, blocking sync-framed writes) + output-pump threads (broadcast::Receiver::blocking_recv →
  writer mpsc) + ticker. NO per-connection tokio runtime, NO async-over-interprocess bridge (decision A
  would need fragile raw-fd→tokio::net::UnixStream). Codebase-idiomatic; sidesteps A's fragility.
  Why it matters: this was THE flagged crux risk of the whole feature ("get the sync-vs-async boundary
  right or the render loop stalls"). Recorded in the keystone spec's crux section.
  Reversibility: design decision, not code yet; revert = re-open the A-vs-B choice.

- 260714 b0.4 sub-brick 2b GREEN + FIRST FEDERATION ACCEPT — the socket now accepts.
  What: new src/server/federation_accept.rs (unix-only): accept loop (mirrors accept_pending_client_
  connections) spawns one std::thread per accepted connection; sync framing helpers read_frame_blocking/
  write_frame_blocking (sync twins of serve::read_frame/write_frame, over codec's pure encode/decode);
  drive_handshake<S: Read+Write> reads the peer Handshake, negotiates against this server's identity
  (federation_server_instance_id) + capabilities {SCROLLBACK_REPLAY, AGENT_STATUS}, replies Accept/Reject.
  handle_federation_handshake sets blocking + a 4s recv timeout then delegates; 2b CLOSES after handshake
  (mount loop is 2c). HeadlessServer gains next_federation_id counter (both constructors) +
  accept_federation_connections() (unix real / windows no-op) called each tick next to accept_client_
  connections(); handoff drains pending federation peers (reject_pending_federation_connections). Removed
  federation_listener's now-inaccurate #[allow(dead_code)].
  Why sync/thread topology (not the keystone's old decision-A async): see the crux-resolution note above.
  drive_handshake is stream-generic so it unit-tests over UnixStream::pair() — 3 tests: accept-compatible,
  reject-version-mismatch (payload version field, independent of the frame header's codec version),
  drop-non-handshake-opener.
  Evidence: cargo test --bin herdr federation 102/0; FULL suite 2676/0, 0 warnings (live tick change).
  Reversibility: additive; the accept loop runs in production now but nothing DIALS the federation socket
  until b3's run_remote flip, and a connection is closed right after handshake (no mount) — so no live
  federated session yet. Revert = drop the commit.
  NEXT = 2c: after Accept, drive mount (FederationCommand::AcquireController→Mount via server_event_tx +
  oneshot::blocking_recv) + the command loop (reader thread → SendInput/Resize; writer thread draining an
  mpsc<FederationMessage>; output-pump threads on broadcast::Receiver::blocking_recv; event/agent ticker)
  + first-cause supervisor + lease Release on EOF. This is the big one — wires the dormant actor commands.

- 260714 b0.4 2c DESIGN scouted WITH 2 PARALLEL AGENTS (hvn-root-causer: lease lifecycle; hvn-scout:
  TerminalRuntime output/mount-gen API). Two decisions locked:
  (D-accept-epoch) The accept loop reads self.federation_lease.current_epoch() SYNCHRONOUSLY in
  accept_federation_connections(&mut self) — same single-threaded event-loop context that owns the lease,
  so no RegisterConnection actor round-trip; epoch threaded into the spawned connection thread + carried on
  every command. Matches federation_lease.rs:91-93 doc + v5-endorsed ordering (register {epoch,connid} THEN
  spawn). AcquireController takes epoch as INPUT, never returns it.
  (D-dead-reserved) THE HOLE: a connection that wins AcquireController (slot Reserved) then dies before
  Mount with NO handoff → slot stuck Held forever (begin_revocation only runs on live-handoff, not on a
  connection dying; codex v4/v5 flagged it + slated an unspecified "reservation expiry"). FIX for 2c-1 = a
  RAII LeaseReleaseGuard: once acquired, a guard whose Drop blocking_sends Release covers EVERY observable
  thread exit incl. panic (stack unwinds → Drop runs). release() is compare-and-clear so a race vs a newer
  lease is inert. Reservation-expiry stays DEFERRED as the wedged-thread backstop (rare; bounded by the 4s
  handshake timeout + bounded reads). blocking_send legal in Drop (plain std::thread, no tokio runtime;
  errors instantly if the loop/receiver is gone → never hangs).
  CORRECTION from scout: this tokio's broadcast::Receiver<Bytes> has NO blocking_recv — the 2c-3 output
  pump must use try_recv() polling (tee.rs:27-42 drain_available coalesce), NOT blocking_recv. mount_
  generation is a fixed const=1 (serve.rs:38). subscribe_output_bytes()->broadcast::Receiver<Bytes>
  (runtime.rs:497). ServerEvent channel is bounded mpsc::channel(64).
  Reversibility: decisions, not code; revert = re-open. Guard is additive.
