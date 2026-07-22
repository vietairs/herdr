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

- 260714 b0.4 sub-brick 2c-1 GREEN+SHIPPED 2368791 — ACTOR BRIDGE LIVE END-TO-END.
  What: after handshake Accept, drive_mount acquires the lease (AcquireController) + mounts (Mount) via
  server_event_tx.blocking_send + oneshot::blocking_recv, streams MountSnapshot{this server's sid}.
  accept_federation_connections reads self.federation_lease.current_epoch() synchronously + stamps it.
  LeaseReleaseGuard (Drop→blocking_send Release) covers every connection-thread exit incl panic.
  Test: mock event loop dispatches Federation cmds against test App+lease; drive_mount over UnixStream::pair
  delivers MountSnapshot carrying the server identity. FULL suite 2677/0.
- 260714 b0.4 sub-brick 2c-2 GREEN+SHIPPED — inbound command loop.
  What: after the snapshot, run_command_loop reads inbound frames until EOF; route_terminal_message sends
  Input→SendInput / Resize→Resize to the actor (fire-and-forget; actor drops unless mounted controller).
  Open/Close/Output ignored (they drive the OUTBOUND side = 2c-3). FIX: clear the 4s handshake recv-timeout
  before the loop (else a mounted federated session idles→drops after 4s; mirrors client_transport.rs:543).
  Test: recording mock loop asserts Input+Resize frames route to the right commands w/ terminal_id/bytes.
  FULL suite 2678/0, 0 warnings.
  Reversibility: additive; the loop runs only on a real federation connection (nothing dials until b3).
  NEXT = 2c-3 (outbound): try_clone the stream → writer thread draining mpsc<FederationMessage>; on inbound
  Open → SubscribeOutput + ScrollbackReplay → emit Open{replay}+Output frames (output pump uses try_recv()
  polling per tee.rs drain_available — this tokio's broadcast::Receiver has NO blocking_recv); event ticker
  (EventsAfter→Frame/Gap) + agent ticker (AgentStatuses); first-cause supervisor (TunnelExit) over the
  reader/writer/pump threads. Then remove FederationCommand/dispatch #[allow(dead_code)] once all variants
  constructed. mount_generation = const 1 (serve.rs:38).

- 260714 b0.4 sub-brick 2c-3 GREEN+SHIPPED 393c07f — OUTBOUND COMPLETE; a mounted federation connection is
  now fully bidirectional.
  What: after the mount snapshot, drive_mount calls run_connection (the sync analogue of serve::run's async
  select!). try_clone's the stream; ONE writer thread drains a std::sync::mpsc<FederationMessage> as the sole
  serializer (every outbound producer funnels through it so no two writes race the socket). Producers: the
  reader thread (Open{replay}), one output-pump thread per opened terminal, and one event/agent ticker.
  Reader routes inbound Input/Resize (unchanged) + Open (→ SubscribeOutput + ScrollbackReplay via the actor →
  emit Open{replay}, spawn pump) + Close (stop+join that pump). Output pump polls tee::drain_available(&mut
  rx) @25ms (NOT blocking_recv — absent on this tokio's broadcast::Receiver) → Output frames. Ticker @25ms:
  EventsAfter(cursor)→Gap/Frame (resumes at the snapshot cursor so no dup/skip); every 4th tick @~100ms
  AgentStatuses diff-on-change→AgentStatus frame. All bridged to the live App via server_event_tx.blocking_send
  + oneshot::blocking_recv (legal on plain std::threads). All 9 FederationCommand variants now constructed.
  Why the topology: decision B (sync threads) locked earlier — serve.rs's shape is async, but the accept path
  yields a sync interprocess stream and this tokio's broadcast has no blocking recv, so I mirror serve.rs's
  LOGIC (poll_events gap/frame, agent diff, MOUNT_GENERATION=1) over threads, not its async mechanics. Added a
  FederationStream trait (try_clone_stream) impl'd for LocalStream (prod) + UnixStream (cfg-test) so the whole
  bidirectional driver unit-tests over a socket pair.
  TEARDOWN (the flagged deadlock risk): reader is the owning thread; writer/ticker/pumps are subordinate. On
  the reader's EOF/read-error it records FirstCauseCell(PeerClosed); a writer failure records WriterFailed +
  sets the shared shutdown flag. Teardown order is load-bearing: set shutdown → stop+join pumps → join ticker
  → DROP our out_tx → join writer. Dropping the last sender BEFORE joining the writer is what makes the
  writer's recv() disconnect; hold it and the join hangs. Verified by test writer_loop_drains_the_queue_and_
  stops_when_senders_drop.
  Evidence: cargo test --bin herdr federation 106/0 (incl 3 new: writer-drain/teardown, open→scrollback-replay,
  reused reader-input); FULL suite 2680/0 (was 2678 → +2); federation_accept.rs clippy-clean. 3 clippy warnings
  remain but ALL in untouched files (id.rs map_out, protocol CLIPBOARD, pane_source complex-type) — pre-existing
  dormant-scaffolding baseline, not caused by this file's change (surfaced under --all-targets). Fixed 3 of my
  own clippy nits inline (is_multiple_of, drop clone-on-Copy AgentStatus, test type alias).
  DECISIONS logged: (1) DEFERRED removing FederationCommand/dispatch #[allow(dead_code)] (federation_actor.rs:
  52/141) — now redundant (all variants live) but a redundant allow is a no-op not a warning, and it is out of
  this brick's file; fold into a later brick to avoid touching unaudited code. (2) BOUNDED BACKSTOP: a wedged
  peer (write-half closed but not reading/not sending EOF) can delay reader- or writer-initiated teardown
  because a blocking read/write only unblocks on peer close; I did NOT add socket shutdown(Both) (interprocess
  Stream shutdown availability unconfirmed; the well-behaved herdr peer closes both directions). Same rare
  class as the already-deferred reservation-expiry; a hard supervisor kill is P9.3 scope.
  Reversibility: additive; the bidirectional loop runs only on a real mounted federation connection and nothing
  dials the federation socket until b3's run_remote flip. Revert = drop 393c07f (restores the 2c-2 inbound-only
  run_command_loop).
  NEXT = sub-brick 3: live handoff revocation (lease.begin_revocation wired into perform_live_handoff; test
  revoke-without-deadlock + no-resurrection). Then sub-brick 4: DELETE AppFederationHost. Then b0.3-tail
  (wire-fault variant + protocol version bump + bounded egress) + b0-proxy + b1 + b2 + b3, then the R7 tail.

- 260714 b0.4 sub-brick 3 GREEN+SHIPPED 7183c24 — live handoff revocation wired.
  What: perform_live_handoff (headless.rs) calls self.federation_lease.begin_revocation() right after the
  client-disconnect/reject step (closes admission, bumps accept-epoch, frees the single-controller slot);
  rollback_handoff_before_commit — the SINGLE shared pre-commit rollback (8 call sites) — calls
  reopen_admission() so a rolled-back handoff doesn't permanently wedge federation (epoch stays bumped,
  never restored). The lease FSM (reopen_admission etc.) was already fully built+tested in abc6a35; SB3 is
  pure wiring.
  Why minimal (no active stream-close): scout confirmed federation accept threads are FULLY DETACHED (no
  registry). But (1) begin_revocation's epoch bump fences the revoked controller — its queued commands carry
  the stale epoch and are rejected (no authority resurrection, lease-tested); (2) a SUCCESSFUL handoff replaces
  this process → detached threads die with it; (3) the CLIENT path also doesn't force-close sockets (sends
  ServerShutdown + drops writer). So adding a connection registry to force-close revoked sockets is v1 hygiene,
  not correctness — DEFERRED (a typed wire-fault "you're revoked" frame is b0.3-tail; hard registry/kill is
  P9.3). "Revoke-without-deadlock" is trivially satisfied: SB3 adds NO joins/blocking.
  Evidence: FULL suite 2680/0 (unchanged — pure wiring); clippy clean (headless.rs + federation_lease.rs no
  warnings). No new test (lease no-resurrection tested; a perform_live_handoff integration test needs full
  fd/socket machinery — disproportionate for a 2-line call of tested methods).
  Reversibility: additive; revert = drop 7183c24. federation_lease #![allow(dead_code)] left in place —
  batched removal scheduled for b3 ("Remove dead_code allows", v5 plan).
  NEXT = b0.3-tail: bump FEDERATION_PROTOCOL_VERSION 1→2 + versioned wire-fault variant + Channel::Control +
  bounded egress + inbound-Fault→TunnelExit (ripples serve/client/loopback/pane_source/codec).

- 260714 b0.3-tail GREEN+SHIPPED (split into 2 green commits).
  tail-1 fa8700e — control-channel wire fault + protocol v2. What: FEDERATION_PROTOCOL_VERSION 1→2 (codec
  version == protocol version, so this gates old peers); new Channel::Control (4KiB cap) + FederationMessage::
  Fault(FaultMessage{reason: FaultReason}); FaultReason mirrors TunnelExit 1:1 with to_wire/from_wire in
  federation_fault.rs (kept the wire type in protocol, the conversion in the server module to avoid a
  protocol→server dep). Co-located server (federation_accept run_connection) best-effort try_send's a Fault
  on a non-clean first-cause before dropping out_tx; reader_loop maps an inbound Fault → TunnelExit::from_wire
  + teardown; local client (client.rs drive loop) returns new DriveOutcome::Faulted(FaultReason) (fail-fast,
  no remount — b2's supervisor consumes it). Exhaustiveness ripple was SMALL: only client.rs:403 matched
  FederationMessage exhaustively (added arm); serve.rs/pane_source use let-else, loopback has catch-alls,
  federation_accept has _other. Codec every_message_variant round-trips Fault.
  tail-2 acaf78e — bounded server egress. What: the co-located writer queue std_mpsc::channel (unbounded) →
  sync_channel(EGRESS_QUEUE_CAP=1024); one enqueue_outbound(out_tx, msg, first_cause, shutdown) helper
  (try_send; Full → first_cause.set(EgressOverflow)+shutdown+return false; Disconnected → false) threaded
  through every producer (open_terminal Open, output_pump Output, poll_events Gap/Frame, poll_agent_statuses).
  first_cause now flows as &Arc through reader_loop→handle_terminal_inbound→open_terminal (pumps clone it),
  &FirstCauseCell into ticker/polls. New test enqueue_outbound_fails_fast_on_a_full_queue (sync_channel(1)).
  Why fail-fast not backpressure: a bounded queue with BLOCKING send would stall a pump/ticker on a stuck
  peer and could deadlock teardown; try_send + overflow-as-fault matches the v1 no-reopen contract.
  Evidence: FULL suite 2681/0 (+1 test); federation_accept.rs + all touched files clippy-clean.
  DECISIONS/DEFERRALS: (1) byte-budget egress bound = DEFERRED (v1 = message-count cap only; the codec MAJ
  "byte permits before encode" is a later refinement — 1024 msgs bounds queue growth, the actual DoS). (2)
  client-input egress bounding DEFERRED to b2 (client→server input is low-volume; server→client output is the
  overflow risk, now bounded). (3) serve.rs async egress NOT bounded — it's the dying duplicate-App path that
  b0-proxy+SB4 supersede; only kept compiling (no exhaustive-match break). (4) federation_lease/federation_
  actor/federation_fault #![allow(dead_code)] still in place — batched removal at b3 per plan.
  Reversibility: additive; nothing dials federation until b3. Revert = drop acaf78e (tail-2) then fa8700e
  (tail-1, protocol change — would need client/codec reverts too).
  NEXT = b0-proxy (transparent federation-serve) → SB4 (delete AppFederationHost) → b1 → b2 (multi-week) →
  b3 → R7 tail.

- What: RESUMED b1 (tunnel keep-alive + single dial + timeouts) after prior agent stalled 600s
  mid-slice (watchdog + API ConnectionRefused). Adopted the crashed agent's uncommitted WIP verbatim
  (Cargo.toml tokio-util dep + src/remote/unix.rs: dial_federation/LiveTunnel/ChildGuard/timeout
  consts + --session dial fix + --session ordering test). Kept tokio-util despite being currently
  unused; deferred the dial-live/timeout tests to b2/b3.
  Why: WIP matched b1 spec (phase-09b-option-b b1 slice) ~90%. tokio-util is spec-directed b2 prep
  (CancellationToken) and unused crate deps do not fail cargo/clippy default, so the green-suite gate
  is unaffected. dial-live + timeout-fallback tests need a real ssh + the b2 mount driver, so they are
  hermetically untestable at b1 level — the plan already schedules them as the b3 acceptance smoke
  (plan L171 "mount live App, stream a PTY, disconnect, prove PTY alive"). Unit tests here are all
  pure (no ssh); the --session ordering test is the one genuine b1-level behavioral test.
  Evidence: b1 spec plan L119-125; unix.rs tests all #[test] pure (no ssh spawn); WIP code comments
  already mark timeouts "applied by b2". Remote sandbox gpu-ml (appn-ltu-vm-100) reachable, 125 cores.
  Reversibility: HIGH — all b1 additions are #[allow(dead_code)] dormant (nothing dials until b3);
  the only live change is remote_federation_serve_command gaining --session, guarded by its test.

- What: b1 SHIPPED 3e1a2ad on feat/remote-workspace-federation. dial_federation + LiveTunnel +
  ChildGuard + FEDERATION_CONNECT/MOUNT_TIMEOUT + --session dial fix (test) live; all dormant
  (#[allow(dead_code)]) except the --session change. Widened ChildGuard/RemoteHerdr/ManagedSshOptions
  to pub(crate) to clear 3 private-interface clippy warnings the pub(crate) tunnel API introduced.
  Why: LiveTunnel{reader,writer} halves are driven by federation::client (a different module), so the
  tunnel API is pub(crate) and its referenced types must match; prior bricks shipped clippy-clean in
  touched files, so no new warnings in unix.rs.
  Evidence: gpu-ml (appn-ltu-vm-100) nix develop: cargo build OK, cargo test exit 0 (0 failed across
  all suites), cargo clippy = 2 warnings total, both pre-existing baseline (id.rs/protocol/pane_source),
  0 in unix.rs. Cargo.lock pins tokio-util 0.7.18 (regenerated on remote, pulled back).
  Reversibility: HIGH — dormant; nothing dials the live tunnel until b3.
  NEXT: b2 (in-proc federated session runner; App::new_federated + SessionPersistencePolicy::Disabled
  + closed-allowlist + supervision + teardown) — the multi-week slice.

- What: b2 STARTED under cortex --auto. Decomposed b2 into bricks b2.1 (persistence policy) /
  b2.2 (closed-allowlist mutation guard) / b2.3 (run_federated_session orchestration + App::new_
  federated) → b3 flip. b2.1 SHIPPED 7dc71ec: immutable SessionPersistencePolicy{Enabled,Disabled}
  on App, additive OR-guard (`!no_session && !persistence.is_disabled()`) at all 5 write/clear sites
  (schedule/background/now save app/session.rs:15/61/91, exit-save mod.rs:1108, config-reload
  clear_history mod.rs:1440). Default Enabled everywhere → classic byte-for-byte unchanged.
  Why: chose ADDITIVE policy over the scout-recommended REPLACE-no_session (Option A). no_session
  also gates update-check + plugin-registry + detach_exits AND is live-mutated (headless.rs:1193);
  replacing it = scope creep + breaks "immutable". Additive policy is an independent persistence axis,
  leaves no_session's other roles + live flip untouched, forces persistence off immutably for a future
  federated App. Scoped b2.1 to the WRITE/CLEAR sites (the clobber risk the spec C3 headlines);
  restore is a READ (can't clobber the saved snapshot) → deferred to b2.3's App::new_federated.
  Evidence: gpu-ml nix: cargo build OK, full suite EXIT_0 all-zero-failed, clippy 2 warnings (both
  pre-existing baseline, 0 new), 2 new tests green (session_persistence_policy_is_disabled_reports_
  variant + disabled_persistence_policy_blocks_session_save_even_when_no_session_false). No dep added.
  Reversibility: HIGH — Disabled #[allow(dead_code)], only tests construct it; classic path unchanged.
  NEXT (b2.2): closed-allowlist mutation guard at the local-runtime-creation seam (D4/MAJ7).

- What: b2.2 SHIPPED dd3cf57: federated_session_allows closed mutation allowlist in src/api/mod.rs —
  exhaustive match over all 81 Method variants, 36 allowed (26 read-only + 6 presentation/nav + 4
  remote input), 45 forbidden, NO wildcard arm (future Method → compile error). Dormant
  (#[allow(dead_code)], only tests call it); wiring at the two dispatch entrances + spawn backstop
  deferred to b2.3. 2 tests green; full suite 2684, clippy 0-new.
  Why: exhaustive-match (not the matches! idiom of the sibling request_changes_ui) because every
  variant carries (_) so it costs nothing and the compiler now forces every present/future method to
  be explicitly bucketed — auditable both ways for a security allowlist. Forbid the 3 ambiguous
  client-display/server methods (NotificationShow/ClientWindowTitle*/ServerReload*) per D4 default-
  closed. PaneZoom/PaneResize forbidden (scout proved both mutate persisted layout, not view-state).
  Evidence: gpu-ml build OK, clippy 2 warnings (pre-existing baseline), full suite EXIT_0 all-zero-
  failed, 2 b2.2 tests pass (api::federated_allowlist_tests). Seam map:
  reports/from-root-causer-b22-mutation-guard-seam-map.md.
  Reversibility: HIGH — pure fn, uncalled, dormant.
  CHECKPOINT: stopping the --auto run before b2.3. b2.3 (run_federated_session + App::new_federated +
  eager-open + C1-safe router move + supervision select! + clipboard sinks + teardown RAII, WIRING
  b2.1 policy + b2.2 allowlist + b1 dial_federation LIVE) is the integration keystone — highest blast
  radius, multi-file, needs fresh focused context + its own sub-scouting, not a late-session one-shot.

- What: b2.3 keystone STARTED under cortex --auto. Design LOCKED after full seam-map read
  (reports/from-root-causer-b23-keystone-seam-map.md). Architecture = materialize-then-move-router
  (C1 dodge): materialize_federation_mount populates App model + router.output_senders + queues Opens
  via out_tx; THEN router+reader+mirror MOVE into ONE drive_mount_channel task; App panes already hold
  the open_terminal rx receivers → no shared Arc<Mutex>, clean ownership handoff. Eager-open (MAJ6):
  spawn drive(reader) + writer tasks BEFORE the server can reply to queued Opens (out_rx unbounded so
  queuing Opens pre-writer never blocks). Supervision: select! app.run() vs drive-task DriveOutcome
  (LinkClosed/Faulted) → tunnel wins → drop App → restore TTY (main.rs:804-822 seq) → exit (D2).
  Two clipboard channels: outbound UNBOUNDED (materialize→spawn_remote) + inbound BOUNDED
  (drive_mount_channel try_send), both dropped on teardown.
- Why: matches phase-09b-option-b plan §b2 + escapes option-a C1 deadlock (codex-verified).
- Evidence: client.rs materialize/drive/router/spawn_mount_writer signatures read; main.rs:768-831
  classic template; mod.rs new_from_handoff wrap-pattern; api.rs:834 + runtime.rs:60 dispatch funnels.
- Reversibility: fully DORMANT/additive — nothing calls run_federated_session until b3; classic path
  byte-for-byte unchanged. Reversible by deleting the new module + new_federated + guards.

- What: DECISION (conservative, reversible) — mutation guard = TWO layers. (1) Primary: closed
  allowlist federated_session_allows(&method) at BOTH API funnels (handle_api_request_after_internal_
  events_drained api.rs:834 + handle_api_request_message runtime.rs:60 incl. its Worktree deferred
  branch) → clean ErrorResponse to client. (2) Backstop: process-global AtomicBool
  FEDERATED_SESSION_ACTIVE (precedent: kitty_graphics global flag mod.rs:394) set by
  run_federated_session, checked at the ONE local-spawn choke spawn_with_portable_pty
  (pty/backend/unix.rs:12) → Err if set. Catches any non-API local-pane path (keybindings) the API
  allowlist can't see.
- Why: plan wants default-FORBIDDEN + "authoritative local-runtime-creation seam"; the spawn choke is
  the single point every local pane passes through (seam map L96). Global atomic avoids high-churn
  threading of federated-mode through the whole spawn call graph; safe because exactly one App/process
  and v1 = no local coexistence.
- Evidence: seam map §4; spawn_with_portable_pty at pty/backend/unix.rs:12; kitty_graphics precedent.
- Reversibility: additive; global flag default false = classic unaffected. Remove flag+check to revert.

- What: b2.3 SHIPPED 65d4388 + b3 live flip SHIPPED 67c82eb (both gpu-ml green). Then R7 tail reviews
  (codex adversarial ‖ code-reviewer agent) returned BLOCK — 5 findings, all verified vs source; fixed 4,
  documented 1 as v1 scope. (F1 CRIT) teardown deadlock: writer_handle.await ran BEFORE the ssh kill →
  a half-open peer (stopped reading) hangs write_all forever → user stranded in alt-screen. Fixed: bound
  the writer drain (FEDERATION_TEARDOWN_DRAIN_TIMEOUT=2s) then drop tunnel_guard (kill) regardless.
  (F4 CRIT/HIGH, both reviewers) federated view-only guard hole: interactive New Worktree (keyboard) reaches
  handle_deferred_worktree_api_request → git worktree add + create_dir_all WITHOUT a federated_mode check,
  bypassing both API-dispatch guards (the runtime.rs api_rx guard is on a DEAD path in v1 — no live API
  socket). Fixed: guard at the DEEPEST single choke (handle_deferred_worktree_api_request) covering keyboard
  + api_rx + future callers; reverted the shallower dispatch_deferred_api_request guard as redundant; added
  regression test federated_session_rejects_deferred_worktree_mutations. (F5 HIGH) unbounded snapshot-probe
  mount → wrapped attempt_federation_mount's connect_and_mount in timeout(CONNECT+MOUNT). (F2 HIGH) lease
  race: probe start_kill()'d without waiting → live re-dial could be rejected Busy → spurious classic
  fallback; mitigated with child.wait() so the tunnel is reaped (server releases the lease) before the live
  dial. (F3 → v1 SCOPE, not fixed) remote STRUCTURAL changes (tab/pane/rename post-mount) don't reach the
  displayed App — pane byte-output flows live, only mount-time structure is frozen; propagation is P9.3
  lifecycle scope. Also fixed a b2.2-introduced clippy warning (moved federated_allowlist_tests to end of
  api/mod.rs — items-after-test-module).
- Why: HIGH-risk R7 surface + b3 ACTIVATES the path; adversarial reviews are the gate. F1 is a hard hang
  (user must kill -9); F4 is a trust-boundary breach (local git/fs mutation from a view-only remote session).
  Deepest-choke guard = self-protecting handler, robust to future callers (both reviewers' recommendation).
- Evidence: gpu-ml build EXIT_0, full suite 2685/0 (+1 guard test, was 2684), clippy 3 pre-existing baseline
  only (map_out/CLIPBOARD/pane_source complex-type). Reviews: codex 5-finding BLOCK; code-reviewer
  DONE_WITH_CONCERNS. auto-decisions D8.
- Reversibility: all fixes additive/isolated; F3 documented for P9.3. Live-attach still has no automated
  coverage (manual two-machine smoke, D6) — the ONE gate before a human un-drafts + merges PR #1.

## 2026-07-16 16:50 — VM100 orca update + serve enable (ops task)

**What:** Updated VM100 Orca AppImage 1.4.141 -> 1.4.143; killed an orphaned `orca serve` tree holding the Electron single-instance profile lock; enabled `orca-xvfb`; restarted `orca-serve`. Proven reachable end-to-end from Mac via saved env `APPN-LTU-VM-100`.
**Why:** Plan report assumed serve merely needed enabling. False: it was already `enabled` but exited on every start with "Another Orca instance is already running for this userData profile". The orphan (PPID=1, from an interactive debug one-liner) squatted the lock while NOT serving (`runtimeState: stale_bootstrap`).
**Evidence:** `runtimeId` seen locally on VM100 == `runtimeId` returned remotely via pairing env -> remote traffic lands on the new 1.4.143 process. `nc 131.172.248.161:45511` succeeds from Mac.
**Reversibility:** Full rollback = `/opt/orca/orca-linux.AppImage.bak-1.4.141` (kept on VM); `sudo systemctl disable orca-xvfb` reverts the enable.

**Plan corrections (report was wrong on 4 points):**
1. Download URL `github.com/orca-sh/orca` is fabricated -> 404. Real feed: `stablyai/orca` (from local `Orca.app/Contents/Resources/app-update.yml`). Asset `orca-linux.AppImage`, verified via sha512 in `latest-linux.yml`.
2. `orca-serve` was already enabled; blocker was the profile lock, not the unit.
3. `orca-xvfb` was `active` but `disabled` -> would NOT survive reboot despite `orca-serve` depending on it. Now enabled.
4. `--port 6768` in the unit is a no-op: ws-transport binds `0.0.0.0:45511`. Saved env already points at 45511, which is why the existing pairing code keeps working.

## 2026-07-16 17:00 — VM105 orca update (ops task, continuation)

**What:** Updated VM105 Orca 1.4.141 -> 1.4.143. Unlike VM100, `orca-serve` was already healthy (active/enabled, genuinely serving on :33155, no lock-squat). Downloaded+verified on Mac (gh authed), scp'd to VM105 (no gh there), re-verified sha512 post-transfer, clean stop/swap/start.
**Why:** VM105 has no `gh` CLI and the release repo (stablyai/orca) is private -> can't curl it directly on VM105. Reused the already-verified binary from the Mac instead of installing new tooling on the VM (YAGNI).
**Evidence:** sha512 matched pre- and post-scp. `runtimeId` returned via `orca status --environment APPN-LTU-VM-105` == local VM105 runtimeId. `orca repo list --environment APPN-LTU-VM-105` returned real repos (herdr, APPNltu_smartForm) — stronger proof than VM100's empty list. Pairing banner (unlike VM100) DID appear directly in journalctl -- runbook's "banner never reaches journal" claim doesn't hold universally, softened in report.
**Reversibility:** `/opt/orca/orca-linux.AppImage.bak-1.4.141` kept on VM105 for rollback.

**Note:** VM105 has no `orca-xvfb` unit at all (unlike VM100) — its `orca-serve` unit has no DISPLAY env var and doesn't need one; left untouched, don't invent a unit that isn't broken.

## 2026-07-21 13:48 — interactive live smoke found a new bug: pane shows local shell, not remote stream

**What:** Ran the D9/D10 manual smoke for real (vm100 -> vm105, `--remote-workspace --session fedsmoke`). Sidebar
correctly showed the federated workspace (`131.172.248.163#fedsmoke`), but the selected pane showed a live,
keystroke-responsive `hvnguyen@bio-1-ubuntu:~$` (vm100's own shell), not vm105's mirrored stream. Root-caused via
hvn:hvn-root-causer (report: reports/from-root-causer-federated-pane-shows-local-shell-instead-of-remote-stream.md).
Both a-priori hypotheses (local pty spawned during materialize; guard bypassed via keybinding) were DISCONFIRMED by
source: `PaneRuntime::spawn_remote` never touches `spawn_with_portable_pty` (pane.rs:1796-1871), and every
mutation path shares one `federated_mode`-gated dispatch (api.rs:851). Strongest remaining hypothesis: D2's own
documented "fail-fast: exit to shell" (session.rs:7-9,333-346) fired silently after a correct first render — no
`DriveOutcome` branch prints anything to the alt-screen, `run_remote`'s `Federated` arm returns `Ok(())` with no
message (unix.rs:513-514), so control silently returns to vm100's real login shell in the same tty.
**Why:** This is the FIRST live interactive exercise of `materialize_federation_mount`'s render path (prior
verification was headless/no-TTY per auto-decisions D10). Not yet confirmed which `DriveOutcome` (or panic) ended
the session — needs vm100's herdr log.
**Evidence:** root-causer's file:line citations (session.rs, client.rs, creation.rs, pane.rs, api.rs, unix.rs);
screenshot showing live `^C` echoes in the "wrong" pane, consistent with a real restored shell, not stale buffer.
**Reversibility:** investigation-only, no code changed. Next: pull vm100's log at
`~/.config/herdr/sessions/fedsmoke/herdr.log` (or wherever XDG_CONFIG_HOME points) to identify the exact
`DriveOutcome`/panic, then decide fix scope. Ship-gate CONDITIONAL PASS downgraded — a real functional gap, not
just an unmet-in-env test.

## 2026-07-21 13:56 — CORRECTION: "wrong pane" was a false alarm (hostname confusion); real finding is a silent-logging gap + an unexplained live exit

**What:** Got direct SSH access to vm100 (`ssh appn-ltu-vm-100`). Confirmed vm100's own hostname is `gpu-ml`, NOT
`bio-1-ubuntu`. Queried vm105 THROUGH the live fedsmoke process's own ssh ControlMaster socket
(`-F /tmp/herdr-ssh-831358-0/config -S .../ctl 131.172.248.163 hostname`) and got `bio-1-ubuntu` back — i.e.
vm105's REAL hostname is `bio-1-ubuntu`. The pane the user saw was NOT a local vm100 shell; it was genuinely
vm105's remote content, correctly mirrored. The 13:48 root-cause finding (both prior hypotheses disconfirmed,
D2 silent-exit theorized) is superseded for the "wrong content" framing — there was no wrong content.
Separately confirmed: no `~/.config/herdr/sessions/fedsmoke/` directory was ever created on vm100 (only the old
`fedtest` from Jul14 exists) -&gt; `init_file_logging` (logging.rs:12-20) silently returns when
`RotatingFileMakeWriter::new` can't create the log file in a non-existent dir -&gt; **zero log output for the
entire fedsmoke run**, on either the render question or anything else.
While gathering this evidence (between confirming the mux socket worked and the next check ~1 command later),
the whole session tore down: `herdr-fed` on vm100 (pid 831358) AND `federation-serve` on vm105 (pid 1075781)
both disappeared. `journalctl --since "10 min ago"` on vm100 shows no OOM/kill entry for that pid, so it was not
killed by the kernel. Cannot rule out that my own diagnostics (`stty -F /dev/pts/1 -a`, or reusing the process's
own ssh ControlMaster socket for an extra hostname query) disturbed the live session, though both are normally
safe/read-only operations and ssh multiplexing is designed to support extra sessions over one master
non-disruptively. Equally plausible: an unrelated timeout/fault fired around the ~20min mark on its own (matches
D2 fail-fast design) — there is no log to distinguish these.
**Why:** Correcting the record before it misdirects a fix — the original bug report was a hostname-identity
mistake, not a rendering/routing bug. The REAL open items are (1) the missing session-log-dir bug (independently
real, blocks all future debugging of federated runs) and (2) confirming disconnect-survival (F1) and
New-Worktree-rejection (F4) from the actual manual smoke checklist, neither of which got exercised before the
session ended.
**Evidence:** `ssh appn-ltu-vm-100 hostname` -&gt; `gpu-ml`; `ssh -F /tmp/herdr-ssh-831358-0/config -S ctl
131.172.248.163 hostname` (via the fedsmoke process's own tunnel) -&gt; `bio-1-ubuntu`; `ps aux` before/after
showing both endpoint processes vanish; `find ~/.config/herdr/sessions` showing only `fedtest`, never `fedsmoke`.
**Reversibility:** no code changed, read-only SSH investigation only. Next: re-run the smoke test with the
session directory pre-created (or `HERDR_LOG=debug`) so a future exit actually leaves a log trail, then complete
the disconnect-survival and New-Worktree-rejection checks the runbook still requires.

## 2026-07-21 14:02 — D9 manual smoke test executed end-to-end via SSH (vm100->vm105); ALL 4 checks PASS

**What:** Ran a fresh dial (`--session fedsmoke2`, tmux-wrapped so output could be captured non-interactively)
and drove it directly over SSH: (1) mount/render — sidebar + pane matched the earlier screenshot exactly,
confirming that finding was correctly resolved as a false alarm, not a regression; (2) live-stream proof — typed
`echo REMOTE_LIVE_CHECK_958938; hostname` into the federated pane, got both back including `bio-1-ubuntu`,
proving genuine live round-trip to vm105, not a cached/static buffer; (3) New Worktree guard (F4) — prefix+G
opened the dialog, submitting "create and open" produced the visible message "this operation is not permitted on
a federated remote workspace", `git -C ~/Projects/herdr worktree list` on vm100 unchanged (still just the main
checkout), `~/.herdr/worktrees/` never created; (4) disconnect-survival (F1) — killed the ssh ControlMaster
(pid 954474) to sever the tunnel; herdr-fed (pid 954464) fully exited within ~3s (confirmed via `ps`, plus its
wrapping tmux server auto-closed because its one window's command had exited — i.e. a clean return, not a hang);
on vm105 the backing bash (pid 1119962, child of the session's own `herdr server` 1119946) was STILL RUNNING
after disconnect — the remote PTY survived exactly as required.
Separately reconfirmed the missing-log bug is NOT simply "directory doesn't exist": pre-created
`~/.config/herdr/sessions/fedsmoke2/` AND set `HERDR_LOG=debug` before launch, and still zero `herdr.log` was
written anywhere (not in the session dir, not at `~/.config/herdr/herdr.log`). The federated `--remote
--remote-workspace` startup path does not appear to call `init_file_logging` at all (or something before it
short-circuits), independent of directory existence. Filed as a real but non-blocking gap (observability only —
every acceptance check above was verified by direct behavior, not logs).
**Why:** User asked me to run the smoke test myself given direct SSH access was available. This is the exact D9
manual-smoke gate ship-gate's CONDITIONAL PASS was waiting on.
**Evidence:** command transcripts captured via `tmux capture-pane` at each step (dialog rejection text, live
echo/hostname round-trip, process tables before/after disconnect on both hosts).
**Reversibility:** test-only; cleaned up `/tmp/fedsmoke2-stderr.log` and the empty session dirs on vm100 after.
No code changed. PR #1 is not un-drafted by me — that handover decision goes back to the user with this evidence.

## 2026-07-21 14:04 — PR #1 un-drafted (user-confirmed)

**What:** User explicitly said "yes" to un-drafting after reviewing the D9 smoke evidence. Ran `gh pr ready 1`.
PR #1 is now "ready for review" (isDraft: false).
**Why:** All human-only ship-gate acceptance criteria empirically satisfied (13a); this was purely the
human sign-off cortex/I cannot make unilaterally.
**Evidence:** `gh pr view 1 --json isDraft` -&gt; `false`.
**Reversibility:** `gh pr ready 1 --undo` re-drafts it. Merge itself still not performed — stays with the user.

## 260721 15:42 -- Phase B (multi-target CLI + concurrent mounts + HashMap mirror) implementation (implementer)
**What:** Generalized Phase A's local+1-remote coexistence to local+N-remote.
Files touched: `src/remote/unix.rs`, `src/remote.rs` (Windows parity),
`src/api/schema/workspaces.rs`, `src/api/schema/response.rs`,
`src/app/api/workspaces.rs`, `src/app/state.rs`, `src/app/mod.rs`,
`src/main.rs`, `src/server/autodetect.rs`, `src/ui/sidebar.rs`,
`docs/next/api/herdr-api.schema.json` (regenerated artifact).

**Deviations from the phase file:**
1. Concurrent-dial location: the phase file assumed the driver lives in
   `src/api/server.rs`. In the actual (already-corrected) Phase A
   architecture the driver lives in `src/app/api/workspaces.rs::handle_
   workspace_mount_remote` -- that is the file generalized, not
   `server.rs` (which only maps the method name, unchanged).
2. No `FuturesUnordered`: `handle_workspace_mount_remote` is fire-and-
   forget -- it never awaits the mounts it starts (success/failure arrives
   later via `AppEvent::FederationMountReady`/`Failed`). Looping
   `tokio::spawn` once per target is already fully concurrent: each task
   runs independently with its own internal ~25s dial budget
   (`FEDERATION_CONNECT_TIMEOUT + FEDERATION_MOUNT_TIMEOUT` in
   `session.rs::dial_and_mount`, unchanged). Test 7 (mock 3x20s dials, fake
   clock) was not written -- there is no seam to mock the internal SSH dial;
   documenting the gap rather than fabricating a weak test.
3. Duplicate `HostKey` in a batch is rejected before any dial, via
   `AppState::double_attach_conflict` checked against `session_name` +
   target before spawning, immediately emitting a per-host
   `FederationMountFailed`.
4. One-request-N-targets chosen over N-requests (implementer's choice per
   requirement 9): `WorkspaceMountRemoteParams.target: String` -> `targets:
   Vec<String>`; `WorkspaceMountRemoteRequested.target: String` -> `targets:
   Vec<String>`.
5. `localhost` filtering implemented as `is_local_target`/`remote_ssh_
   targets` pure functions in `unix.rs`. Exact-string match only, no
   `127.0.0.1`/hostname canonicalization (documented v1 limitation, per
   predict's explicit allowance).
6. `--remote` space-form grammar: a single `--remote` greedily consumes
   bare tokens after its first required value as additional targets,
   stopping at the next `--`-prefixed flag or `--` separator. The old
   "--remote can only be specified once" error was removed entirely (not
   just for the space form), satisfying the inverted duplicate-values test
   for both space and equals forms. A malformed value starting with a
   single `-` still errors via `validate_remote_target`.
7. Classic single-target dispatch (`run_remote`) reads
   `remote.target.first()` only; extra targets on a classic (non-federated)
   `--remote` invocation are silently ignored rather than rejected at parse
   time -- minor documented gap, not enforced since not requested.
8. Windows parity test (phase test 10) not added as a runnable test: the
   `#[cfg(windows)]` code cannot be built/executed on this macOS sandbox and
   there is no shared cross-platform parsing function. The Windows grammar
   was mirrored by hand to match unix.rs's `is_flag_like`/multi-value loop;
   not compiler-verified this pass.
9. `HashMap<HostKey, RemoteMirror>` teardown on natural drive-task end is
   still missing (pre-existing Phase A gap, not newly introduced) --
   `remote_mirrors` entries are only removed on materialize failure, not
   when a live tunnel closes normally. Needs a new `AppEvent` variant to
   hand `&mut App` back after the spawned task ends; out of this phase's
   locked requirements.

**Validation:**
- `cargo check --bin herdr` (ZIG=~/.local/zig-0.15.2/zig; sandbox default
  zig 0.16.0 is incompatible with vendored libghostty-vt's pinned 0.15.2):
  clean.
- `cargo test --bin herdr -- --test-threads=4`: 2681 passed, 0 failed (up
  from pre-Phase-B baseline of 2673; net +8 new/inverted tests across
  unix.rs, state.rs, workspaces.rs, sidebar.rs; just/nextest unavailable in
  this sandbox).
- `cargo clippy --bin herdr --all-targets -- -D warnings`: 3 remaining
  errors, all verified pre-existing via `git stash`/re-run (dead-code
  `map_out`, dead-code `Capability::CLIPBOARD`, `type_complexity` in
  `pane_source.rs`) -- zero new clippy findings from this phase.
- `docs/next/api/herdr-api.schema.json` regenerated via
  `HERDR_UPDATE_API_SCHEMA=1 cargo test generated_protocol_schema_artifact_
  is_current` to match the `targets: Vec<String>` shape change.
- Manual two-machine smoke not run (out-of-env for this sandbox), same as
  Phase A.

## Remote split end-to-end: server-side real dispatch + client fire-and-forget bridge (260722)

**What:** Production `federation_accept.rs` now performs a REAL split against
the live `App` when it receives `SplitPaneRequest` (item 1, fully working):
`FederationCommand::SplitPane` (new variant, `server::federation_actor`)
dispatches through the exact same `Method::PaneSplit` JSON-API handler the
local TUI/CLI split action calls, then replies `SplitPaneResponse::Created`/
`Failed` on the connection's existing outbound queue. `handle_pane_split`
(`app/api/panes.rs`) now, for a remote-federated workspace with a live mount,
looks up the target pane's own `PaneRuntime` (`remote_terminal_id()`/
`remote_out_tx()`, new accessors on `PaneRuntime`/`TerminalRuntime`/
`RemoteTerminalSourceHandle`) and sends the real `SplitPaneRequest` over that
pane's own mount tunnel -- no new mount registry needed, since every
remote-backed `PaneRuntime` already privately holds a clone of its mount's
out-tx.

**Why (design choice):** `handle_pane_split` is a synchronous JSON-API
handler invoked inline with `App`'s own tick; it cannot `.await` the
`SplitPaneResponse` a live mount's async drive task
(`client::drive_mount_channel`) will eventually read, and blocking on a
`oneshot::Receiver` there would stall the same tokio worker the response
needs to arrive on (deadlock risk on a single-worker runtime). The smallest
correct design given that hard sync/async boundary: send the request
fire-and-forget, and reply `remote_split_pending` (not a fabricated
`PaneInfo`) instead of the old `remote_split_unsupported`. `client.rs`'s
`drive_mount_channel` now logs `SplitPaneResponse::Created`/`Failed` instead
of silently dropping it.

**Evidence:** `handle_pane_split`'s remote branch and `client.rs`'s response
handling only have `mirror`/`router`/`hub` in scope, never `&mut App` --
confirmed by reading `app/api/workspaces.rs::handle_federation_mount_ready`
(the only call site that owns both `out_tx` and `&mut App` simultaneously,
briefly, before both are moved into separately-spawned tasks) and
`client::drive_mount_channel`'s signature.

**Reversibility:** fully additive; the old refusal path is kept as the
exact fallback when the target pane has no live `remote_out_tx` (mount down
or not actually remote-backed).

**Remaining gap (not closed, needs files outside this fix's ownership):**
the new pane the remote host creates does not yet materialize into a real
local `Tab`/`PaneRuntime` in the requesting client's TUI -- `client.rs`'s
`drive_mount_channel` only owns `mirror`/`router`/`hub`, never `&mut App`,
so it cannot itself build the local pane. Closing this needs, in one
coordinated follow-up (three files, none owned by this fix):
1. `app/api/workspaces.rs`: thread a channel (or the existing `event_tx`)
   into `drive_mount_channel`'s call site so a `SplitPaneResponse` can reach
   `App`.
2. `events.rs`/`app/mod.rs`: a new `AppEvent` variant (e.g.
   `FederationSplitPaneReady { host_key, new_pane_id, new_terminal_id,
   ws_idx, direction, ratio, focus }`) and its handler, calling
   `App::build_remote_pane` (already exists, `app/creation.rs`) and
   inserting into the target `Tab`'s layout via the local mirrored
   `ws_idx`/tab, the same way `split_focused_with_runtime` does for a local
   split but with a remote-backed runtime.
3. `app/api/panes.rs`: correlate the fire-and-forget `request_id` this fix
   already mints (`next_remote_split_request_id`) if the eventual design
   needs per-request context beyond "which pane initiated the request" (may
   not be necessary if the response's `new_pane_id`/`new_terminal_id` alone
   are enough, given the remote host and client already share the same
   `ws_idx`/tab topology from mount time).

**Deviations from strict file ownership (logged per orchestration-protocol):**
this fix's assigned owned-file list was `src/server/federation_accept.rs,
src/remote/federation/*, src/app/api/panes.rs, src/app/creation.rs,
src/workspace.rs, src/workspace/tab.rs, src/pane.rs`. Two structurally
unavoidable small edits landed just outside that list:
- `src/server/federation_actor.rs` (new `FederationCommand::SplitPane`
  variant + dispatch arm) -- `federation_accept.rs`'s only path to the live
  `App` is this actor's existing command channel; no existing variant can
  perform a split, and item 1 (this fix's primary ask) is impossible without
  it.
- `src/terminal/runtime.rs` (two 3-line delegation methods,
  `remote_terminal_id`/`remote_out_tx`, mirroring the file's own existing
  `relayed_agent_status_sender` delegation precedent) -- `TerminalRuntime`
  is the newtype `App` actually stores (`terminal_runtimes: HashMap<String,
  TerminalRuntime>`), not the inner `PaneRuntime` this fix's owned
  `src/pane.rs` added the real accessors to.
Both are mechanical, narrowly-scoped (no new logic invented, only exposing
existing private state), and consistent with the "smallest faithful
deviation" guidance when a phase file's file list turns out to structurally
exclude something the assigned goal cannot be met without.

**Validation:**
- `cargo check --bin herdr` (ZIG=~/.local/zig-0.15.2/zig): clean, only the 2
  pre-existing dead-code warnings (`map_out`, `Capability::CLIPBOARD`).
- `cargo clippy --bin herdr --no-deps`: same 2 pre-existing warnings, no new
  ones.
- `cargo test --bin herdr federation -- --test-threads=4`: 124 passed, 0
  failed (includes 2 new `federation_actor::SplitPane` dispatch tests, 1 new
  `federation_accept` reader-loop routing test).
- `cargo test --bin herdr -- --test-threads=4` (full suite): **2706 passed,
  0 failed, 0 ignored** (up from the pre-existing 2702 baseline; net +4 new
  tests: 2 in `federation_actor`, 1 in `federation_accept`, 1 in `pane.rs`'s
  `remote_spawn_tests`).
- The pre-existing `pane_split_in_a_federated_workspace_is_refused_not_
  misfiled_locally` regression test (`app/api/panes.rs`) still passes
  unchanged -- a workspace classified remote but whose pane has no live
  `remote_out_tx` (this test's setup) still gets the exact same
  `remote_split_unsupported` refusal as before.

**Remote VM binaries:** need redeploy once this patch (or its follow-up)
lands -- unlike the prior fix in this chain, this one DOES change real
production wire behavior: `federation_accept.rs` (the co-located production
serve-side handler, not a test-only path) now actually performs splits for
any peer that sends `SplitPaneRequest`, so the served-host binary must be
rebuilt/redeployed for the new dispatch to take effect. The requesting
client's binary also needs redeploy for `handle_pane_split`'s new
`remote_split_pending` branch to replace `remote_split_unsupported`.

## Remote split materialization: mount drive task builds the runtime, App splices it into layout (260722)

**What:** Closed the "remote split end-to-end" chain's last gap. On
`SplitPaneResponse::Created`, `client::drive_mount_channel` (the mount's own
drive task, which already owns `TerminalChannelRouter`/the mount's `out_tx`)
opens the new pane's `Terminal` channel and spawns a real `TerminalRuntime`
via `TerminalRuntime::spawn_remote` itself, then hands the finished
`(PaneId, TerminalId, TerminalState, TerminalRuntime, PaneState)` bundle to
`App` over a new `AppEvent::FederationSplitPaneReady`. `App::
handle_federation_split_pane_ready` (`app/creation.rs`) pops a
`PendingRemoteSplit` (keyed by `request_id`, stashed by `dispatch_remote_pane_
split` at mint time) and calls the mirrored target pane's own
`Tab::insert_existing_pane` — the same primitive `materialize_federation_
mount` uses for mount-time split panes — placing the new pane in the correct
tab, then registers the runtime/terminal and emits `PaneCreated`/layout-
updated events exactly like a local split. `SplitPaneResponse::Failed` now
also surfaces via `AppEvent::FederationSplitPaneFailed`, toast-notified the
same way `handle_federation_mount_failed` does, instead of only a log line.

**Why (design fork):** the drive task, not `App`, owns the mount's
`TerminalChannelRouter`/`out_tx` — the only handles `router.open_terminal`/
`TerminalRuntime::spawn_remote` need — so the runtime MUST be built inside
`drive_mount_channel`, not after handing a bare response back to `App`. What
`App` alone owns is the workspace/tab layout and the original request's
context (target pane, direction, ratio, focus), which the wire response does
not carry at all (`SplitPaneResponse::Created` has only `request_id`/
`new_pane_id`/`new_terminal_id`) — hence the new `PendingRemoteSplit` map on
`App` (keyed by `request_id`, populated at `dispatch_remote_pane_split` time)
bridges the two. `rows`/`cols`/`scrollback_limit_bytes`/`host_terminal_theme`
for the new runtime are captured ONCE at mount time into a new `client::
SplitMaterializationContext` (passed as `Option<&_>` into `drive_mount_
channel`, `None` for every caller with no live session to materialize into)
rather than re-queried per split — the same "v1 simplification" precedent
`materialize_federation_mount` already uses for mount-time panes.
**Evidence:** `src/remote/federation/client.rs` `SplitMaterializationContext`,
`drive_mount_channel`'s `FederationMessage::SplitPaneResponse` arm; `src/app/
creation.rs::handle_federation_split_pane_ready`/`_failed`/
`PendingRemoteSplit`; new test `remote::federation::client::tests::drive_
mount_channel_materializes_a_runtime_on_split_pane_created` (asserts the
`AppEvent::FederationSplitPaneReady` actually arrives with the request's real
`request_id`, driven end-to-end over the same hand-rolled fake server pattern
this file's other `drive_mount_channel` tests already use).
**Reversibility:** additive — `split_materialization: Option<&_>` defaults to
`None` (log-only, prior behavior) for every existing caller
(`remote/federation/session.rs`'s classic full-screen `--remote` path; every
pre-existing `client.rs` test). Only `app/api/workspaces.rs`'s real
server-owned mount call site passes `Some(...)`.

**Deviations from strict file ownership (logged per orchestration-protocol):**
this fix's assigned owned-file list was `app/api/workspaces.rs`, `app/
events.rs` (or `app/mod.rs`), `app/creation.rs`, `remote/federation/client.rs`,
`remote/federation/reducer.rs`, `workspace.rs`, `workspace/tab.rs`, `pane.rs`.
Four small, structurally-required edits landed just outside that list, none
inventing new logic:
1. `src/app/api/panes.rs::dispatch_remote_pane_split` — one call to the new
   `App::register_pending_remote_split` right after minting `request_id`.
   Without this, nothing would ever populate the correlation map this fix's
   response handler reads; this is the ONE place that owns both the local
   layout context and the moment a `request_id` is minted.
2. `src/app/api.rs::handle_internal_event` — two new early-return arms for
   `AppEvent::FederationSplitPaneReady`/`FederationSplitPaneFailed`, mirroring
   the 3 existing `FederationMount*` arms in the same function. Mechanically
   required: this is the ONE dispatch point every `AppEvent` variant must be
   routed from.
3. `src/app/actions.rs` — two `Vec::new()` arms in `AppState`'s exhaustive
   `AppEvent` match (same pattern as the existing `FederationMount*` arms
   immediately above them), required only because Rust's exhaustiveness check
   forces every `AppEvent` variant to appear in every match over the enum.
4. `src/remote/federation/session.rs` — the classic full-screen `--remote`
   client process's own `drive_mount_channel` call site needed the same 3 new
   positional args (`out_tx`/`outbound_clip_tx`/`None`) to keep compiling;
   passes `None` (no live server-owned session to materialize into from a
   standalone client process), so its behavior is unchanged (still logs only).
   Required cloning `out_tx`/`outbound_clip_tx` before the task's `async move`
   block instead of moving them, since the outer scope still needs to `drop
   (out_tx)` for its own (pre-existing, unrelated) teardown sequence.

All four are minimal, mirror an existing precedent in the same file, and
invent no new logic — consistent with prior fixes in this chain's own
"smallest faithful deviation" disposition.

**Validation:**
- `ZIG=~/.local/zig-0.15.2/zig cargo check --bin herdr`: clean, only the 2
  pre-existing dead-code warnings (`map_out`, `Capability::CLIPBOARD`).
- `cargo clippy --bin herdr --no-deps`: same 2 pre-existing warnings, no new.
- `cargo test --bin herdr federation -- --test-threads=4`: **125 passed, 0
  failed** (up from the pre-existing 124 baseline; +1 new test).
- `cargo test --bin herdr -- --test-threads=4` (full suite): **2707 passed,
  0 failed, 0 ignored** (up from the pre-existing 2706 baseline; net +1).

**Remaining gap (v1 scope, logged for a future pass):** split-response
materialization always creates a horizontal chain split via `insert_existing_
pane` with the request's own direction/ratio against the target pane — it
does not attempt to reconcile against any subsequent layout drift on the
remote side (e.g. the remote's own layout differs if two splits race). Same
"v1 does not reproduce exact remote split geometry" caveat `materialize_
federation_mount` already carries for mount-time panes. Remote VM binaries
need redeploy for the server-side half of this chain (already noted in the
prior entry); this fix's changes are entirely client-side (the requesting
herdr process), so only that side needs redeploy for local materialization to
take effect — the serve-side dispatch itself is unchanged by this fix.

- What: Fixed the code-review Critical finding (stale-index splice/leak in
  `pending_remote_splits`) by re-keying `PendingRemoteSplit` on `Workspace::id`
  (stable string) instead of `ws_idx: usize`, and purging every entry whose
  `workspace_id` is in a mount's `closing_ids` set inside
  `handle_federation_mount_ended` (new `App::purge_pending_remote_splits_
  for_workspaces`).
  Why: `ws_idx` is a raw `Vec` index; workspace close/reorder can make it
  point at an unrelated (possibly local) workspace by the time a delayed
  `SplitPaneResponse` arrives, and `handle_federation_mount_ended` never
  touched the map at all, so every disconnected-mid-flight split leaked
  forever. `ws.id` never gets reused across a process's lifetime
  (`Workspace::test_new`/creation always allocates a fresh base32 id; see
  `workspace::tests::reserving_restored_workspace_ids_prevents_reuse`), so
  keying on it removes the reuse hazard entirely rather than just narrowing it.
  Evidence: new regression test
  `app::api::workspaces::tests::federation_mount_ended_purges_pending_remote_
  splits_for_its_workspaces` (src/app/api/workspaces.rs) — registers a pending
  split against a federated workspace, ends its mount, asserts the entry is
  gone, then feeds a late `FederationSplitPaneReady` for that request_id and
  asserts no pane lands anywhere (including the local workspace now sitting at
  the old index). Full suite green: `cargo test --bin herdr -- --test-
  threads=4` → 2708 passed, 0 failed (net +1 over the 2707 baseline above).
  `cargo clippy --bin herdr --tests`: only the 3 pre-existing warnings
  (`map_out`, `Capability::CLIPBOARD`, a pane_source.rs type-complexity lint),
  no new ones.
  Reversibility: trivial — struct field rename + one new purge call site; no
  wire/protocol change, no persisted-state shape change.

- What: Did not add separate mount-generation fencing to `pending_remote_
  splits` for the review's Major #2 (register/send-race amplifier).
  Why: `dispatch_remote_pane_split` (registers) and `handle_federation_mount_
  ended` (would purge) both run as `&mut App` methods off the same single
  `handle_internal_event` dispatch (`src/app/api.rs:64`); `AppEvent`s are
  processed one at a time, so there is no concurrent-mutation window on
  `App` state for these two handlers to race in. The only real risk was the
  ordering/leak this fix already closes (register can still land for a
  request whose mount already ended, but the stable-id purge plus the
  existing "workspace no longer exists" / "unknown request" guards in
  `handle_federation_split_pane_ready` already make that a safe no-op, not
  a mis-splice).
  Evidence: `src/app/api.rs:64` `handle_internal_event(&mut self, ev: AppEvent)`
  is the sole call site for both `handle_federation_mount_ended` and
  `handle_federation_split_pane_ready`.
  Reversibility: n/a (no code change); revisit if `App` event dispatch ever
  becomes concurrent.

- What: Fixed a CRITICAL finding — creating a tab while focused on a
  federated remote workspace spawned a LOCAL shell stamped with a
  remote-looking (`r:`-namespaced) tab/pane id, same class of bug as the
  earlier `remote_split_unsupported` fix for pane split. Two refusal points,
  both following that existing pattern (`crate::remote::federation::
  id::classify` on the target workspace's public id):
  1. `src/app/api/tabs.rs::handle_tab_create` — refuses with error code
     `remote_tab_unsupported` / message "remote workspace: new tab not
     supported yet" before calling `Workspace::create_tab`. This is the
     single choke point: `TabCreateParams` API requests, `runtime_tab_create`
     (used by the keyboard path, the `+` mouse click, and the rename-dialog
     confirm flow) all funnel into `handle_tab_create` via `dispatch_api_
     request`/`dispatch_runtime_mutation`.
  2. `src/app/input/navigate.rs::execute_tui_navigate_action` (`NavigateAction
     ::NewTab` arm) — added an earlier, TUI-visible refusal so the keyboard
     path shows a `ToastKind::NeedsAttention` toast ("remote workspace" /
     "new tab not supported yet") and skips opening the rename dialog,
     instead of silently discarding the JSON error string that `runtime_tab_
     create`'s return value would otherwise drop on the floor. The mouse-`+`
     and rename-dialog-confirm triggers (owned by `mouse.rs`/`modal.rs`,
     out of this task's file scope) still get the safety net from fix #1 —
     no local shell is spawned — but do not yet show a toast for this
     specific refusal.
  Files: `src/app/api/tabs.rs`, `src/app/input/navigate.rs`.
  Tests added: `app::api::tabs::tests::
  api_tab_create_in_a_federated_workspace_is_refused_not_misfiled_locally`,
  `app::input::navigate::tests::
  tui_new_tab_keybind_in_a_federated_workspace_is_refused_with_toast`.
  Validation: `cargo test --bin herdr -- --test-threads=4` → 2710 passed,
  0 failed (net +3 over the 2707 baseline: the two new tests above plus
  gaining `pane_split_in_a_federated_workspace_is_refused_not_misfiled_
  locally` which was already present from the earlier split fix).
  Reversibility: trivial — two independent early-return refusal checks, no
  wire/protocol change, no persisted-state shape change.

- Closing a federated remote workspace via `workspace.close` now actively
  tears down its federation mount instead of only removing UI/runtime state
  (MAJOR review finding: SSH link stayed alive invisibly, remount reported
  "already live", and `pending_remote_splits` for the closing workspace
  leaked because the purge only ran from `handle_federation_mount_ended`,
  which never fires for a locally initiated close).
  - `AppState` gained `mount_drive_tasks: HashMap<HostKey, JoinHandle<()>>`,
    populated in `handle_federation_mount_ready` right after the drive task
    is spawned. `end_federation_mount` now removes and `.abort()`s the
    matching handle unconditionally (a no-op if the task already finished),
    so both the reactive (`handle_federation_mount_ended`) and the new
    proactive (workspace close) teardown paths converge on one place.
    Aborting mid-drive drops the task's owned `out_tx`/`tunnel_guard`
    in-place, which is the same drop-order teardown the graceful path
    performs deliberately, just triggered by cancellation instead of EOF.
  - `handle_workspace_close` resolves the closing workspace's `HostKey` via
    a new `federation_host_key_for_workspace` helper (matches its
    `worktree_space` key `federation:<host_key>` against the live
    `remote_mirrors` registry), purges `pending_remote_splits` for the
    whole `close_indices_for` group, then calls `end_federation_mount`
    *before* `close_selected_workspace` removes the workspaces — mirroring
    `handle_federation_mount_ended`'s purge-before-remove ordering.
  - No partial-mount case to handle: federation workspaces always set
    `is_linked_worktree: false` on their shared `worktree_space`, so
    `close_indices_for`/`close_selected_workspace` already close the
    mount's entire workspace group in one shot — ending the mount in
    `handle_workspace_close` is always the "last workspace" case, never a
    premature teardown of a mount another still-open workspace needs.
  - Files: `src/app/api/workspaces.rs`, `src/app/state.rs`, `src/app/mod.rs`
    (second `AppState` constructor needed the new field too).
  - Test added:
    `app::api::workspaces::tests::
    closing_a_federated_workspace_ends_its_mount_and_purges_pending_splits`.
  - Validation: `cargo test --bin herdr -- --test-threads=4` → 2711 passed,
    0 failed.
  - client.rs was not touched — no wire/protocol change was needed; the
    fix is entirely App/state-layer bookkeeping around the already-existing
    drive task and mount registry.

- What: Fixed three code-review findings in the remote-split (protocol v3
  `SplitPane`) path: (A) client/server id-space mismatch, (C) non-root
  remote panes bypassing public pane numbering. Finding (B)'s cross-mount
  response-spoofing/reorder-before-spawn ask is only partially fixed —
  see the refutation below.
  - (A) `dispatch_remote_pane_split` (`app/api/panes.rs`) sends
    `runtime.remote_terminal_id()` (raw `term_…`), but the server's
    `Method::PaneSplit` handler (`handle_pane_split`) only accepted public
    `w…:p…` pane ids (`App::parse_pane_id`) — every real remote split
    replied `pane_not_found`. Fixed by resolving `target_pane_id` through
    the already-implemented (previously dead-code, `#[allow(dead_code)]`,
    staged for #00f) `App::resolve_terminal_target` first, which tries a
    raw terminal id, then falls back to `parse_pane_id` for ordinary
    public-id callers — server now owns the terminal-id → pane mapping, no
    client-side change needed. Left the `#[allow(dead_code)]` attributes in
    `src/app/terminal_targets.rs` untouched (that file is outside this
    fix's owned-files list) since they are harmless once the function
    becomes reachable.
  - (C) `materialize_federation_mount`'s split-chain loop and
    `handle_federation_split_pane_ready` (`app/creation.rs`) both called
    `Tab::insert_existing_pane` directly for non-root remote panes,
    bypassing `Workspace::public_pane_numbers` — those panes rendered but
    were unreachable through the public pane-id API (list/focus/close).
    Switched both call sites to `Workspace::insert_moved_pane_into_tab`
    (the existing wrapper mount-time root panes and local splits already
    use), which registers the public number as part of the same call.
  - Evidence: new regression tests, all green under
    `ZIG=... cargo test --bin herdr -- --test-threads=4` (2713 passed, 0
    failed): `server::federation_actor::tests::
    split_pane_resolves_a_raw_terminal_id_the_same_as_a_public_pane_id`
    (A); `app::creation::federation_materialization_tests::
    split_materialization_assigns_a_public_pane_number_to_the_new_pane`
    plus an added `Workspace::assert_invariants_for_test()` +
    `public_pane_number` assertion inside
    `successful_mount_materializes_into_rendered_workspace_tab_and_two_panes`
    (C).
  - Files: `src/app/api/panes.rs`, `src/app/creation.rs`,
    `src/remote/federation/client.rs` (Err-arm cleanup only, see below),
    `src/server/federation_actor.rs` (test only).
  - Reversibility: both are small, localized call-site changes (swap one
    resolver / one insertion helper) — a one-line revert each, no
    persisted-state or wire-protocol shape change.

- What: Finding (B) ("split responses are not origin-bound and
  materialization happens before validation") is only partially fixed —
  refuting the full fix as infeasible inside this task's owned-files
  boundary, with evidence, rather than silently doing less than asked.
  Fixed: on a local runtime-spawn failure for a `SplitPaneResponse::
  Created` (`client::drive_mount_channel`), the drive task now also sends
  `AppEvent::FederationSplitPaneFailed` so `App::
  handle_federation_split_pane_failed` drops the matching
  `pending_remote_splits` entry — before this it only logged a warning and
  left the entry orphaned in the map forever (finding's third ask).
  Not fixed: binding a pending split request to its originating mount and
  rejecting a `SplitPaneResponse`/`Failed` from a different mount, and
  reordering pending-map validation before channel-open/runtime-spawn.
  - Why not fixed: the only way to give `App`'s synchronous
    `pending_remote_splits` map (or a mount-identity token) to the async,
    per-mount `client::drive_mount_channel` task without `&mut App` is to
    thread new state through `SplitMaterializationContext`, which is a
    struct literal constructed at the mount's own setup site in
    `src/app/api/workspaces.rs` (`handle_federation_mount_ready`) — a file
    this task's prompt explicitly listed as owned by another agent
    ("Do NOT touch ... src/app/api/workspaces.rs"). The struct's Rust
    field-literal syntax means adding any required field (a mount/host-key
    token, or a shared `Arc<Mutex<pending map>>`) does not compile without
    editing that exact call site (and its two test literals of
    `PendingRemoteSplit`/`FederationSplitPaneReady` in the same file, also
    forbidden). I built and then reverted a working
    `UnboundedSender::same_channel`-based origin-binding implementation on
    `FederationSplitPaneReady`/`PendingRemoteSplit` for exactly this
    reason (it compiled everywhere in my owned files, but broke
    `src/app/api/workspaces.rs`'s existing test literals at ~line 1471 and
    ~line 1531). `FederationSplitPaneFailed`'s dispatch site
    (`src/app/api.rs`'s pattern match) is similarly outside the owned-files
    list and would need a matching edit for an `origin` field there too.
  - Practical exposure this leaves open: a second concurrently-mounted
    (malicious or buggy) remote host that predicts/observes the
    process-global `SplitPaneRequest::request_id` counter can send a
    `SplitPaneResponse`/`Failed` that `handle_federation_split_pane_ready`/
    `_failed` will accept as if it came from the mount the request was
    actually sent to, since neither the pending entry nor the event carry
    mount identity. Recommend: whichever agent owns
    `src/app/api/workspaces.rs`/`src/app/api.rs` adds a `HostKey`/
    `UnboundedSender` field to `SplitMaterializationContext`,
    `PendingRemoteSplit`, and `FederationSplitPaneReady`/
    `FederationSplitPaneFailed`, then compares origins in both handlers
    (the exact shape I built and reverted is recoverable from this
    session's diff history if useful as a starting point).
  - Evidence: `cargo build --bin herdr` failure trace when the reverted
    approach was attempted, pointing at
    `src/app/api/workspaces.rs:1471` and `:1531` missing-field errors (not
    preserved verbatim, but reproducible by re-adding an `origin` field to
    either struct).
  - Reversibility: the shipped partial fix (spawn-failure cleanup event)
    is additive and independently revertible; the refuted portion requires
    no rollback since nothing was left half-applied in owned files.

- What: Completed finding (B)'s remaining ask — cross-mount split-response
  origin binding — now that `src/app/api/workspaces.rs`/`src/app/api.rs`
  were free to edit. Implemented the exact shape the prior agent
  recommended (and had built/reverted): threaded `HostKey` through
  `PendingRemoteSplit`, `SplitMaterializationContext`,
  `FederationSplitPaneReady`, and `FederationSplitPaneFailed`.
  - `PendingRemoteSplit` (`app/creation.rs`) gained `origin: HostKey`, set
    at mint time in `dispatch_remote_pane_split` (`app/api/panes.rs`) from
    a new call to `App::federation_host_key_for_workspace` (widened from
    private to `pub(crate)` in `app/api/workspaces.rs` — it already existed
    there for `handle_workspace_close`'s mount teardown, no new lookup
    logic needed). If a workspace somehow has a live mount out-tx but no
    resolvable `HostKey` (should not happen in practice — same registry
    both paths read), the split now hard-refuses with
    `remote_split_unsupported` rather than minting an un-originable pending
    entry.
  - `SplitMaterializationContext` (`remote/federation/client.rs`) gained
    `origin: HostKey`, populated at mount-setup time in
    `handle_federation_mount_ready` (`app/api/workspaces.rs`) from the same
    `host_key` local already used for the mount's registry entry — one
    extra field on an existing struct literal, no new plumbing.
    `drive_mount_channel` stamps `ctx.origin.clone()` onto every
    `FederationSplitPaneReady`/`FederationSplitPaneFailed` it emits (both
    the `Created`-success path and the local-runtime-spawn-failure path).
  - `handle_federation_split_pane_ready`/`handle_federation_split_pane_failed`
    (`app/creation.rs`) now peek the pending map by `request_id` *before*
    calling `take_pending_remote_split`; if a pending entry exists and its
    `origin` doesn't match the incoming event's `origin`, they
    `tracing::warn!` and return without removing the entry or touching any
    workspace/pane state — the real response (if it ever arrives from the
    correct mount) can still land later. If no pending entry exists at all
    (already-purged/unknown request id), behavior is unchanged from
    before (drop with the existing "unknown/stale" warning).
  - `AppEvent::FederationSplitPaneFailed` gained an `origin: HostKey` field;
    its match arm in `app/api.rs` and `handle_federation_split_pane_failed`'s
    signature were updated to carry it through.
  - Files touched: `src/app/api/workspaces.rs` (visibility widen +
    literal field), `src/app/api.rs` (match-arm field), `src/app/api/
    panes.rs` (origin lookup + refusal + field), `src/app/creation.rs`
    (struct field, both handlers' origin checks, signature), `src/events.rs`
    (two struct/variant field additions), `src/remote/federation/client.rs`
    (context field + two emit sites + one test-fixture literal).
  - The "unknown request ids dropped before channel-open/runtime-spawn"
    ordering ask in the same finding is not separately addressed: `client
    .rs`'s `drive_mount_channel` has no access to `App`'s
    `pending_remote_splits` map (by design — it is `&mut App`-owned,
    unreachable from the async per-mount drive task without a shared-
    mutable-state redesign, e.g. `Arc<Mutex<..>>`, which is out of scope
    for this fix). Origin binding closes the actual security gap (a
    predicted/observed `request_id` from mount B can no longer splice a
    pane into a workspace whose pending entry's `origin` is mount A) even
    though the local runtime spawn for mount B's (rejected) response still
    happens before `App` gets to validate it — that spawn is wasted work
    on a mismatch, not a splice risk, since `handle_federation_split_pane_
    ready` now refuses to insert it anywhere.
  - Test added: `app::creation::federation_materialization_tests::
    split_pane_response_from_a_different_mount_than_the_request_is_ignored`
    — registers a pending split with a `real-host` origin, then delivers a
    fully-materialized `FederationSplitPaneReady` tagged with an
    `evil-host` origin for the same `request_id`; asserts the pending
    entry is still present and no pane landed in the workspace.
  - Validation: `ZIG=... cargo test --bin herdr -- --test-threads=4` →
    2714 passed, 0 failed (net +1 over the 2713 baseline).
    `cargo clippy --bin herdr --tests`: only the same 3 pre-existing
    baseline warnings (`map_out`, `Capability::CLIPBOARD`, `pane_source.rs`
    type-complexity), no new ones.
  - Reversibility: additive field threading through five existing structs/
    enum variants plus two guard checks in two existing handlers; no wire/
    protocol change (the `HostKey` never crosses the wire, it is local-
    process bookkeeping only), no persisted-state shape change.

## Remote agent panes missing from the sidebar — agent identity never relayed (260722 follow-up)

- Root cause (full evidence in `plans/260722-1240-remote-agents-sidebar-
  still-missing/reports/debug-260722-1240-remote-agents-sidebar-root-cause-
  report.md`): `AgentStatusMessage` carried only `status`, never identity,
  and the client's only identity-setting path (`pane.rs`'s process probe)
  is gated on `pid > 0`, which is always `0` for a remote-mirrored pane. So
  `TerminalState::is_agent_terminal()` never went true and
  `collect_agent_infos()` silently dropped the pane.
- Fix, both ends:
  - `AgentStatusMessage` (`src/remote/federation/protocol/mod.rs`) gained
    an additive `agent: Option<String>` field
    (`#[serde(default, skip_serializing_if)]`). No `FEDERATION_PROTOCOL_
    VERSION` bump — the handshake already rejects any version skew before
    a channel frame is ever decoded (v3, already bumped past the last
    released tag by 6bbc829), so both peers that can reach this type
    always agree on the field's presence; the serde default only keeps
    hand-built old-shaped test frames decodable.
  - Serve side: `FederationCommand::AgentStatuses`'s reply
    (`src/server/federation_actor.rs`) now carries `AgentInfo.agent`
    alongside status. `poll_agent_statuses`
    (`src/server/federation_accept.rs`) diffs on `(status, agent)` instead
    of `status` alone, so an identity that resolves *after* the first
    status poll (e.g. screen-text detection catching up) still reaches the
    client instead of being permanently stuck at `agent: None`.
  - Client side: the wire router (`TerminalChannelRouter` in
    `src/remote/federation/client.rs`) and the per-pane relay channel
    (`PaneRuntime::relayed_agent_status_sender`, `src/pane.rs`) now carry a
    new `pane::RelayedAgentStatus { status, agent: Option<String> }`
    instead of a bare `AgentStatus`. `spawn_basic_detection_task`'s
    relayed-status branch parses the label
    (`crate::detect::parse_agent_label`) and calls
    `agent_presence.observe_process_probe(Some(identified))` directly —
    bypassing the `pid > 0`-gated process probe entirely, since a remote
    pane never has one. The branch now publishes `StateChanged` on an
    identity change alone (not just a status transition), since identity
    is the only thing `AppEvent::StateChanged`'s handler
    (`app/actions.rs`) writes into `TerminalState::detected_agent`.
  - Deviation from the minimal fix shape in the debug report: identity is
    only ever *set*, never cleared, from the relay (`relayed_status.agent
    == None` is a no-op, not a reset). The report didn't need to resolve
    this edge case (agent process exiting mid-mount) and clearing would
    need a second relay-side "agent disappeared" contract that doesn't
    exist yet; logged here rather than invented.
- Tests added: `codec::tests::agent_status_frame_without_agent_field_
  decodes_with_none` (old-shaped frame decodes with `agent: None`),
  `codec`'s roundtrip fixture now carries `agent: Some("claude")`,
  `client::tests::drive_mount_channel_relays_agent_status_to_the_
  registered_pane_sink` now asserts both `status` and `agent` on the
  relayed value, `pane::remote_spawn_tests::relayed_agent_status_updates_
  detection_state_without_any_local_probe` now sends identity and asserts
  the published `StateChanged` event carries `agent: Some(Agent::Claude)`
  for a pane whose `child_pid` never leaves `0`.
- Validation: `ZIG=... cargo test --bin herdr -- --test-threads=4` → 2715
  passed, 0 failed (net +1 over the 2714 baseline). `cargo clippy` could
  not be run this session — the vendored `libghostty-vt` `ReleaseFast`
  build fails in this environment independent of this change (Zig
  0.15.2's bundled libcxx vs. the macOS SDK; `cargo build`/`cargo test`
  dev-profile builds are unaffected and compile clean with zero warnings
  from the touched files). `cargo fmt` run; only the files this fix
  touched were reformatted.
- Files touched: `src/remote/federation/protocol/mod.rs`,
  `src/remote/federation/protocol/codec.rs` (+ test fixtures),
  `src/remote/federation/reducer.rs` (test fixtures only),
  `src/remote/federation/serve.rs` (dead-code-path construction site kept
  compiling), `src/server/federation_actor.rs`,
  `src/server/federation_accept.rs`, `src/remote/federation/client.rs`,
  `src/pane.rs`, `src/terminal/runtime.rs` (wrapper return type only).

## Code-review remediation: stale relayed identity now clears (MAJOR + 2 MINOR)

- What: `src/pane.rs`'s relayed-status branch (previously logged above as
  "identity is only ever set, never cleared") now clears a previously
  relayed identity when the remote reports `agent: None` **and**
  `status` is `Idle`/`Done`. It routes through the existing
  `agent_presence.observe_process_probe(None)` debounce (same path the
  local process-probe loop already uses to clear a vanished agent,
  `AGENT_MISS_CONFIRMATION_ATTEMPTS = 6`) rather than clearing on a
  single frame. A `Working`/`Blocked` status paired with `agent: None`
  is left untouched (not even debounce-counted) — that shape is what an
  *old* peer that structurally never populates the `agent` field would
  send, and treating it as a clear signal would wipe a real identity for
  a remote agent that's still actively running. Also fixed the stale "P6:
  dormant... nothing drives it yet" comment at the relay channel's
  construction site (it's been actively driven since commit `6bbc829`)
  and added a comment on `serve.rs`'s hardcoded `agent: None` marking it
  fixture-only (`FixtureHost`) so a real `FederationHost` impl doesn't
  copy it.
- Why: code review (`plans/260722-1240-remote-agents-sidebar-still-
  missing/reports/code-review-260722-remote-agent-identity-relay-
  report.md`) flagged this as the one MAJOR gap — a remote agent exiting
  left the mirrored pane permanently `is_agent_terminal() == true`,
  showing a stale sidebar entry indefinitely.
- Evidence: traced the actual server-side signal this depends on
  (`src/app/agents.rs::agent_info` filters on `is_agent_terminal()`
  before a terminal ever reaches `AgentList`/the federation status poll,
  so a fully-exited agent terminal with no `launch_argv` disappears from
  `agent_statuses()` entirely with no explicit frame — but a terminal
  spawned via `herdr agent start` keeps `launch_argv` set and stays an
  agent terminal with `agent_status: Idle/Done` and `agent: None` once
  screen-text can no longer identify a specific agent; that's the real,
  reachable "agent gone" signal this fix keys on). New tests: `pane::
  remote_spawn_tests::relayed_idle_none_clears_identity_only_after_
  debounce` (drives the full sequence — Working+Some establishes
  identity, Working+None is a no-op, then 6 Idle+None frames are needed
  before the clear publishes) and `app::actions::tests::
  state_changed_agent_gone_clears_is_agent_terminal` (proves the
  resulting `StateChanged{agent: None, state: Idle}` event flips
  `TerminalState::is_agent_terminal()` to `false`, which is exactly what
  `agent_info`/`collect_agent_infos` filter on for the sidebar/API).
  `ZIG=/Users/hvnguyen/.local/zig-0.15.2/zig cargo test --bin herdr --
  --test-threads=4` → 2717 passed, 0 failed (net +2 over the 2715
  baseline this session started from). `cargo fmt` run clean.
- Reversibility: fully reversible — isolated to the added `else if`
  branch in `pane.rs`'s relayed-status match arm, the two comment
  updates, and the two new tests; no wire/protocol shape change.

- 2026-07-22 fix (post-mount pane mirroring, plans/260722-1327-post-mount-
  pane-mirroring): panes/tabs created or closed on the SERVING side after
  a client mounted it were never mirrored to that client.
- Root cause (two gaps, both closed):
  1. `agent.start` (`src/app/agents.rs::start_agent`) never pushed
     `PaneCreated`/`WorkspaceCreated` onto the shared `EventHub` (every
     other creation path — local split, worktree open — already does).
     A pane spawned this way was invisible to anything that only
     observes the hub, including a mounted federation client.
  2. Federation `Event` channel frames carry only `{source_seq, kind}` —
     no entity id/payload (`reducer.rs`'s longstanding module-doc
     limitation) — and nothing downstream turned a structural frame into
     a mirror mutation outside the initial mount/`Gap`/`Reset` remount
     path.
- Fix (shape 1 from the root-cause report, resync-on-structural-event):
  - `src/app/agents.rs`: `start_agent` now emits `WorkspaceCreated`
    (new-workspace path, via the existing `emit_workspace_open_events`)
    or `PaneCreated` + `LayoutUpdated` (split-into-existing-tab path,
    matching `app/api/panes.rs`'s own split handler) after a successful
    spawn.
  - `src/remote/federation/protocol/mod.rs`: additive
    `SnapshotRequest`/`SnapshotResponse` `FederationMessage` variants —
    NOT a `FEDERATION_PROTOCOL_VERSION` bump, since v3 has never shipped
    in a release (no tagged version has a `federation_accept.rs`/
    `client.rs` production call site at all), so there is no deployed
    peer that could observe the addition as a skew.
  - `src/server/federation_actor.rs`: new `FederationCommand::Snapshot`
    (read-only, no lease touch) + a `current_snapshot` helper extracted
    out of `Mount`'s existing snapshot-production code so both commands
    build the same way.
  - `src/server/federation_accept.rs`: the reader loop answers an
    inbound `SnapshotRequest` with a `SnapshotResponse`, mirroring the
    existing `SplitPaneRequest`/`Response` handling shape exactly
    (`handle_snapshot_request`). Threaded `server_instance_id` into
    `run_connection`/`reader_loop` (previously only known to the
    handshake step) so the response can be tagged correctly.
  - `src/remote/federation/client.rs`: `drive_mount_channel` now tracks
    one in-flight resync request; on an `Applied` structural `EventKind`
    (`PaneCreated`/`PaneClosed`/`PaneMoved`/`TabCreated`/`TabClosed`/
    `TabMoved`) it sends a `SnapshotRequest` if none is already pending
    (coalescing a burst into exactly one), and on the matching
    `SnapshotResponse` calls the existing (previously production-dead)
    `RemoteMirror::reconcile_by_diff` — the same resync primitive the
    `Gap`/`Reset` path already had but nothing ever drove in production.
- Deviation / known residual gap (logged per the review-audit rule
  rather than silently cut): `reconcile_by_diff` updates the
  `RemoteMirror`'s own metadata (`workspaces()`/`tabs()`/`panes()`) and
  pushes `PaneCreated`/`PaneClosed`/`TabCreated`/`TabClosed` onto the
  local `EventHub` correctly, but this fix does **not** go the next
  step of splicing a newly-resynced remote pane into the already-live
  mounting `App`'s real `Tab`/`PaneRuntime` layout (spawning a
  `TerminalRuntime::spawn_remote`, registering it with
  `TerminalChannelRouter`, calling
  `Workspace::insert_moved_pane_into_tab`) or tearing down a closed
  one's local runtime. That materialization step needs `&mut App`
  inside the mount's drive task, which — like the existing
  `SplitPaneResponse::Created` path — would require a new
  `AppEvent`/handler round-trip plus a stable remote-pane-id ->
  local-`PaneId` reverse index this mirror-level fix does not yet
  carry. Full scope was judged too large for one mid-tier implementer
  pass; a full unmount/remount already shows the correct state (via
  `materialize_federation_mount`), so this is a real but bounded
  regression (a live-mounted session's sidebar/state metadata is
  correct post-resync; the sidebar's live TUI pane layout for that
  already-open mount is not auto-spliced until a remount). Follow-up
  scope: an `AppEvent::FederationResyncApplied` carrying the built
  runtime(s) for added panes + the namespaced ids to remove, handled
  next to `handle_federation_split_pane_ready` in
  `src/app/creation.rs`, is the natural next step.
- Verified unresolved question from the root-cause report: yes,
  `agent.start`-spawned panes did NOT push `PaneCreated` into the
  server's local `EventHub` prior to this fix (see gap #1 above) — this
  was the earlier, necessary-but-not-sufficient half of the bug; gap #2
  (no resync mechanism at all) was the other half.
- Evidence: `ZIG=/Users/hvnguyen/.local/zig-0.15.2/zig cargo test --bin
  herdr -- --test-threads=4` → 2723 passed, 0 failed (net +6 over the
  2717 baseline this session started from: two `agent.start` event
  tests, one wire codec roundtrip test, one server actor `Snapshot`
  test, one `federation_accept` reader-loop `SnapshotRequest` test, one
  client-side burst-coalescing + resync-updates-the-mirror test).
  `cargo fmt --check` clean. `cargo clippy` was not run — it fails
  locally on the pre-existing vendored libghostty ReleaseFast/Zig build
  issue unrelated to this change (see `herdr local build: Zig 0.15.2
  workaround` memory note); this is an environment limitation, not a
  new regression from this fix.
- Reversibility: fully reversible — additive wire variants (no version
  bump), an additive server command + one new reader-loop match arm, an
  additive client-side resync request/apply path gated on structural
  `EventKind`s only, and the two new `agent.start` event pushes. No
  existing message shape, JSON API contract, or persisted state format
  changed.

- 2026-07-22 fix, part 2 (post-mount pane mirroring, plans/260722-1327):
  closes the residual gap the part-1 fix above logged — a resync diff
  updated `RemoteMirror`'s own metadata and the sidebar-facing local
  `EventHub`, but never spliced a newly-revealed remote pane into (or
  tore a removed one out of) the already-live mounted `App`'s real
  `Tab`/`PaneRuntime` layout.
- What:
  - `src/remote/federation/reducer.rs`: `RemoteMirror::reconcile_by_diff`
    now returns a `ReconcileDiff { created_panes, removed_pane_ids }`
    (namespaced/public ids and `PaneInfo`s) alongside its existing hub
    pushes — `reconcile_panes` already computed this internally, it just
    never surfaced it to a caller.
  - `src/remote/federation/client.rs`: `drive_mount_channel`'s
    `SnapshotResponse` handling reads that diff. For each created pane
    (when `split_materialization` is `Some`, same convention as
    `SplitPaneResponse::Created`) it spawns a real local
    `TerminalRuntime` (new `materialize_resync_pane` helper, mirrors the
    `SplitPaneResponse::Created` arm) and sends `AppEvent::
    FederationResyncPaneCreated`; for each removed id it sends
    `AppEvent::FederationResyncPaneRemoved`.
  - `src/events.rs`: new `AppEvent::FederationResyncPaneCreated(Box<..>)`
    / `FederationResyncPaneRemoved { origin, pane_id }` variants
    (`#[cfg(unix)]`, matching every other federation event).
  - `src/app/creation.rs`: `App::handle_federation_resync_pane_created`
    finds the target workspace by its namespaced id, checks the mount's
    `HostKey` against the workspace's `federation:<host_key>`
    `worktree_space` (same origin-binding discipline
    `handle_federation_split_pane_ready` uses), and splices the pane into
    the workspace's **active tab** via `insert_moved_pane_into_tab` (same
    primitive mount-time materialization and split-materialization use,
    so the pane gets a `public_pane_numbers` entry). `App::
    handle_federation_resync_pane_removed` looks the local `PaneId` up in
    a new reverse index, checks the same origin binding, then tears it
    down via `Workspace::close_pane` + the same terminal-runtime-shutdown
    sequence `panes.rs::close_pane` uses — deliberately skipping that
    handler's interactive close-confirmation gate (e.g. "closing this
    pane would close a worktree group"): the remote already made this
    decision, there is nothing left to confirm locally.
  - `src/app/mod.rs`: new `App::remote_resync_pane_index: HashMap<String,
    PaneId>` field (remote namespaced pane id -> local `PaneId`) — no
    existing map served this; `RemoteMirror::panes()`'s own namespaced
    `pane_id` values are not, in general, the local
    `<workspace_id>:p<N>` public form (local pane numbers are assigned
    independently by `insert_moved_pane_into_tab`/
    `create_tab_from_existing_pane` in materialization order, not
    parsed from the remote's own numbering), so a dedicated reverse
    index is the correct primitive rather than trying to derive the
    local `PaneId` from the string itself.
  - `src/app/actions.rs` (not in this task's owned-files list, touched
    for compilation only): two mechanical no-op arms added to
    `AppState::handle_app_event`'s exhaustive `match AppEvent` — matches
    the pre-existing pattern already used for every other Federation*
    event there (`App::handle_internal_event` intercepts and returns
    before this fallback ever runs); required for the crate to compile
    once the two new `AppEvent` variants exist.
- Deviation (logged, conservative-minimal per the task's own explicit
  allowance): a resync diff only reports created/removed *panes*, not a
  tab-level diff (the wire's `SnapshotResponse` carries a full
  snapshot, but `ReconcileDiff` deliberately mirrors only what
  `reconcile_panes` already tracked rather than also diffing
  `TabCreated`/`TabClosed` — that would need matching each new pane to
  the *specific* new/existing remote tab it belongs to, which the
  mirror's tab ids don't map onto a stable local tab index the way
  workspace ids map onto `Workspace::id`). `handle_federation_resync_
  pane_created` always targets the mounted workspace's current
  `active_tab` rather than attempting to reconstruct the remote's tab
  placement. Accepted rather than building a full tab-diff pass, per
  the task's own guidance to keep new tabs in the active tab and log
  the compromise.
- Evidence: `ZIG=/Users/hvnguyen/.local/zig-0.15.2/zig cargo test --bin
  herdr -- --test-threads=4` → 2728 passed, 0 failed (net +5 over the
  2723 baseline this session started from: two `client.rs` tests
  proving a resync diff materializes a runtime / emits a removal event,
  three `creation.rs` App-level tests proving splice-with-public-pane-
  number, wrong-origin-drop, and teardown-on-removal). `cargo fmt` run
  clean.
- Reversibility: fully reversible — `reconcile_by_diff`'s new return
  value is additive (existing callers ignore it), the two new
  `AppEvent` variants and their handlers are additive and gated on a
  diff that previously silently updated metadata only, and the reverse-
  index field is new state with no persisted/wire format change.

## Code review remediation (post-mount pane mirroring, 260722)

Remediated all findings from
`plans/260722-1327-post-mount-pane-mirroring/reports/code-review-260722-post-mount-pane-mirroring-report.md`.

- C1 (CRITICAL — split-created pane double-materialized on the next
  resync): `SplitPaneResponse::Created`'s arm (`src/remote/federation/
  client.rs`) never registered the split-created pane into
  `RemoteMirror::panes`, so the server's own `PaneCreated` hub event for
  that same split (delivered on a separate channel, ordering vs. the
  `SplitPaneResponse` not guaranteed) could trigger a resync whose
  snapshot re-reported the pane; `reconcile_panes`'s `None` arm then
  classified it as newly created and `materialize_resync_pane` spawned
  a SECOND `TerminalRuntime`/`PaneId` for the same remote terminal.
  Fixed by adding `RemoteMirror::register_split_pane(remote_pane_id,
  remote_terminal_id) -> String` (`src/remote/federation/reducer.rs`):
  namespaces both ids and inserts a placeholder `PaneInfo` under the
  namespaced pane id via `entry().or_insert_with` — deliberately does
  NOT push onto `hub` (the caller already drives its own
  `AppEvent::FederationSplitPaneReady`/`PaneCreated`, so pushing here
  would double-emit on the local sidebar-facing bus). Placeholder field
  values beyond `pane_id`/`terminal_id` don't matter: `reconcile_panes`
  compares by full equality and, on a mismatch, emits `PaneUpdated`
  (never a duplicate `PaneCreated`) once a real resync snapshot brings
  the pane's true metadata in. Called from `SplitPaneResponse::Created`
  in `client.rs` right after the runtime spawns successfully, before
  the `AppEvent::FederationSplitPaneReady` send.
  - Also closes the M1 (MAJOR) compounding gap: the returned namespaced
    pane id is threaded through a new `FederationSplitPaneReady::
    remote_pane_id: String` field (`src/events.rs`) into
    `App::handle_federation_split_pane_ready`
    (`src/app/creation.rs`), which now inserts it into
    `remote_resync_pane_index` exactly like a resync-created pane
    already does — previously a split-created pane was NEVER in that
    reverse index at all (a pre-existing, separate gap from C1, noted
    explicitly in the existing `handle_federation_resync_pane_removed`
    comment), so a later remote-initiated close of that same pane could
    never be torn down via the resync-removal path. Now it can.
  - Regression test added:
    `a_split_created_pane_is_not_double_materialized_by_a_later_resync`
    (`src/remote/federation/client.rs`) — drives split-then-resync in
    the exact server-side wire order the finding describes (split
    response, then the structural event frame, then a `SnapshotResponse`
    re-reporting the same pane id), and asserts (a) exactly one
    `AppEvent` reaches the caller (no second `FederationResyncPaneCreated`),
    (b) `mirror.panes().len() == 1` after the resync, and (c) the split's
    `FederationSplitPaneReady.remote_pane_id` matches the namespaced form
    `register_split_pane` produced.
- N1 (MINOR — stale `#[allow(dead_code)]`): removed the attribute and
  its "dormant until b0.4" comment from `current_snapshot`
  (`src/server/federation_actor.rs`) — verified it has two live call
  sites in the same `dispatch` (`Mount` and `Snapshot`), not dead.
- N2 (MINOR — no rate limiting on `SnapshotRequest`): no code change.
  The report itself concludes this is "consistent with the codebase's
  existing trust model, not a new hole — noting for awareness only"
  (same posture as the pre-existing `EventsAfter` precedent, handshake
  already gates who can open a mount); adding throttling here would be
  scope the review did not actually ask for.
- Deviation: none — every fix matches the report's own stated fix
  direction (register at split-materialization time in both the mirror
  and the reverse index, "whichever layer owns each structure").
- Evidence: `ZIG=/Users/hvnguyen/.local/zig-0.15.2/zig cargo test --bin
  herdr -- --test-threads=4` → 2728 passed, 1 failed
  (`server::autodetect::tests::is_server_listening_returns_false_for_stale_socket`,
  confirmed pre-existing/flaky — passes in isolation, unrelated to this
  diff, no federation/mirror/pane code touched). Narrower
  `cargo test --bin herdr federation` → 140 passed, 0 failed (includes
  the new regression test). `cargo fmt` run clean; `cargo build --bin
  herdr` clean (two pre-existing unrelated warnings: `map_out`,
  `Capability::CLIPBOARD`, neither touched by this change).
- Reversibility: fully reversible — `register_split_pane` and the new
  `remote_pane_id` field are additive; removing the stale `allow` on
  `current_snapshot` has no behavioral effect (the function was already
  compiling and running from two live call sites).
