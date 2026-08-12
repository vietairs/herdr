# Phase 02 — TerminalSource trait + construction-level transport factory — ROOT B

**Goal:** extract the transport-general I/O surface (`write_user_input`/`resize`/`shutdown`) behind a
`TerminalSource` trait AND introduce a **construction/teardown transport factory** so P5 can add a socket-backed
source whose lifecycle policy differs from a local child. **Zero behavior change** for the local PTY path.
**Depends on:** nothing (parallel with P1). **Blocks:** P5. **Shippable:** yes (refactor only). **ROOT.**

## Context
- Arch-probe (KEY): `reports/arch-probe-terminalruntime-source-seam.md`, verdict MEDIUM.
- Verified: `TerminalRuntime` = newtype over `PaneRuntime` (`src/terminal/runtime.rs:15`); only variant
  indirection is `enum PaneRuntimeIo { Actor(PtyIoActorHandle), #[cfg(test)] TestChannel {..} }` (`src/pane.rs:923`).
- Verified — **codex #5, lifecycle is baked in at construction:** the `on_read` closure that calls
  `terminal.process_pty_bytes(...)` is built inside the `spawn_*` path and captured at `PtyIoActor::spawn` time
  (`src/pane.rs:1722`, `:1880`); the local child + `master_fd` are prerequisites of every `spawn_*`
  (`src/pane.rs:1798-1924`). So the seam is NOT just write/resize/shutdown — it must also cover **who constructs
  the byte source and how it tears down**. A remote source must never spawn/kill a local child and never emit a
  local `PaneDied` on reader-exit.
- Verified: 15 `TerminalRuntime::spawn*` call sites (arch-probe §2, enumerated). Handoff ops (`begin_handoff`,
  `duplicate_for_handoff`, `foreground_process_group_id`, `rollback_handoff`, `release_after_commit`,
  `nudge_child_redraw_after_handoff`) are local-PTY-only — MUST NOT enter the trait (arch-probe §3/§4/§6).
- Registry handoff methods are 4 `#[cfg(unix)]` fns (`src/terminal/runtime_registry.rs:46,53,64,71`).

## Requirements
1. `trait TerminalSource: Send` with exactly: `write_user_input(&self, Bytes)`, `try_write_user_input(...)`,
   `resize(rows,cols,cell_w_px,cell_h_px, terminal_responses: Vec<Bytes>)`, `shutdown(&self)`. Nothing PTY-specific.
2. `PtyIoActorHandle` implements `TerminalSource` (signatures already match — arch-probe §4). No signature drift.
3. **Transport factory / lifecycle policy (codex #5):** introduce a construction seam — a
   `TerminalTransport` concept (or a `spawn`-side factory param) that owns: (a) how the byte source is created,
   (b) the `on_read` sink wiring, (c) teardown/`on_reader_exit` policy. The existing local path is refactored to
   route through it with a `LocalChild` policy that behaves EXACTLY as today (spawn child + `master_fd`, emit
   `PaneDied` on exit). This phase adds ONLY the `LocalChild` policy; P5 adds `Remote`.
4. **No behavior change:** byte-in (`on_read`→`process_pty_bytes`) wiring identical for local panes; the 15
   existing `spawn_*` constructors keep their public signatures (factory is internal). P5 adds an additive
   `spawn_remote`, not now.
5. `PaneRuntimeIo` gains no new production variant yet (P5 adds `Remote`); document that non-PTY variants no-op
   the 4 `#[cfg(unix)]` registry handoff methods exactly like the `#[cfg(test)] TestChannel` arms already do
   (`src/pane.rs:936-1034`) — that pattern is the template.

## Files
- **Create** `src/terminal/source.rs` — `trait TerminalSource` + `impl` for `PtyIoActorHandle` + the
  `TerminalTransport`/lifecycle-policy seam type(s) with the `LocalChild` policy.
- **Modify** `src/terminal/runtime.rs` — route `send_bytes`/`try_send_bytes`/`resize`/`shutdown` through
  `TerminalSource` where it removes duplication; public `TerminalRuntime` API unchanged.
- **Modify** `src/pane.rs` — `PaneRuntimeIo::Actor` arms call trait methods; route the `spawn_*` construction +
  `on_read`/`on_reader_exit` wiring through the `LocalChild` policy (no observable change).
- **Modify** `src/terminal/mod.rs` — `mod source; pub use source::TerminalSource;`.

## TDD test plan (tests FIRST)
Run `cargo test` (full existing suite = the "no behavior change" oracle) + new unit tests:
1. **Regression gate (must stay green):** existing PTY/pane/terminal tests incl. `TestChannel` users + handoff
   tests under `#[cfg(unix)]`. Zero edits to these.
2. New `source.rs` tests: a mock `TerminalSource` records `write_user_input`/`resize`/`shutdown`; assert
   `TerminalRuntime` delegation forwards args verbatim (rows/cols/px/`terminal_responses`/Bytes).
3. **Lifecycle policy:** a `LocalChild` policy fixture spawns→exits and emits `PaneDied` exactly as today; a
   stub `Remote`-shaped policy (test-only) does NOT emit `PaneDied` on drop — locks the P5 contract at the seam.
4. **Handoff exclusion (type-level):** a test/doc-test that the trait object cannot call `begin_handoff`
   (documents the exclusion).
5. `cargo clippy` clean; Windows cfg block + `#[cfg(unix)]` handoff registry methods untouched/green.

## Implementation steps
1. Add the trait + `PtyIoActorHandle` impl; run full suite green with the impl unused.
2. Introduce the `TerminalTransport`/`LocalChild` factory; route local `spawn_*` construction + `on_read` wiring
   through it; run suite after each change (oracle must stay green).
3. Add the lifecycle-policy contract test (3). Verify Windows cfg + handoff registry untouched.

## Risks + rollback
- **Risk (highest blast radius):** accidental behavior change across 15 sites / handoff path / `PaneDied`
  emission. Mitigation: pure refactor, no new production variant, full existing suite gates every step; handoff
  ops explicitly off the trait; lifecycle contract pinned by test 3. **Rollback:** revert commit — trait +
  factory are additive, no external contract touched.

## File ownership
Exclusive: `src/terminal/source.rs`, `src/terminal/runtime.rs`, `src/terminal/mod.rs`, `src/pane.rs` (source/io
regions), `src/pty/` (if the impl lands next to the handle). No overlap with P1's `src/remote/federation/*`.
