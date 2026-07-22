# Phase 05 — Remote-backed panes (raw byte channel → TerminalSource)

**Goal:** a non-PTY `TerminalSource` whose bytes arrive over the P1 raw output channel and feed the SAME
`on_read`/`process_pty_bytes` closure; input/resize/close go out as protocol messages. Hydrates a real local
ghostty `Pane` for a focused remote pane, with scrollback-on-hydrate and origin-tagged clipboard. **Depends on:**
P2 (seam), P1 (protocol/ids), P4 (client). **Blocks:** P6. **Shippable:** yes (behind flag; no CLI switch till P8).

## Context
- Verified: byte-in is closure-shaped (`on_read: Box<dyn FnMut(&[u8]) -> PtyReadResult>`, `src/pane.rs:1722`),
  `process_pty_bytes` wants only `&[u8]` (`src/pane/terminal.rs:183`); `Pane::render` source-agnostic
  (`src/pane.rs:2493`). A remote source calls the SAME closure with network bytes — no new emulation.
- Verified (codex #5): remote lifecycle must differ — never spawn/kill a local child, never emit local `PaneDied`
  on channel close. P2's transport factory `Remote` policy is where this lives.
- Verified: 4 `#[cfg(unix)]` registry handoff methods no-op non-PTY variants like the `#[cfg(test)] TestChannel`
  arms (`src/pane.rs:936-1034`) — template for the `Remote` arm.
- Verified: resize signature already plain data (`rows,cols,cell_w,cell_h, terminal_responses: Vec<Bytes>`,
  arch-probe §4) — serializes cleanly. RT-F10 open Q resolved here.
- Scenario S2.1 (no local echo), S2.2 (isolation Blocker), S8.3 (paste atomicity), S12.1/S12.3 (lazy hydrate /
  multiplex). RT-F6 (scrollback), RT-F7 (clipboard).

## Requirements
1. **`RemoteTerminalSource`** implements P2's `TerminalSource`: `write_user_input`/`resize`/`shutdown` serialize
   to P1 `TerminalChannel` `Input`/`Resize`/`Close` over the P4 mount, tagged `{terminal_id, mount_generation}`
   (`map_out` to the remote id before the wire).
2. **Byte-in:** a dedicated task reads `Output{bytes}` from the channel and invokes the existing `on_read`
   closure → `process_pty_bytes` → local ghostty grid. No new terminal emulation.
3. **Lifecycle (codex #5):** constructed via P2's `Remote` transport policy — never creates a local child, never
   emits a local `PaneDied` on drop/close; a remote close surfaces as a disconnected/exited pane state (P9), not
   a local process death.
4. **`PaneRuntimeIo::Remote(RemoteTerminalSource)`** production variant; the 4 `#[cfg(unix)]` registry handoff
   methods no-op it (template = `TestChannel` arms). Additive `TerminalRuntime::spawn_remote(..)` — existing 15
   `spawn_*` sites untouched.
5. **Scrollback-on-hydrate (RT-F6):** on hydrate, first apply the channel's replayed history bytes, then live
   bytes — a freshly focused remote pane shows real content immediately, scrollback works. Bounded by
   `advanced.scrollback_limit_bytes` + P7 caps.
6. **Isolation (S2.2 Blocker):** each remote pane's I/O runs on its own task; a flooding remote pane must NOT
   stall the local render loop or other panes. Backpressure propagates to the remote (channel bounded).
7. **No local echo (S2.1):** dumb relay, never predicted. **Paste (S8.3):** bracketed paste sent as one atomic
   `Input` message.
8. **Clipboard/OSC (RT-F7):** the origin-tagged `Clipboard` channel is plumbed here (write local clipboard from
   remote OSC, send local paste incl. image to the remote as `Input`/`Clipboard`) — origin tag preserved for P7
   policy. Not dropped.
9. **Lazy hydrate (S12.1) + multiplex (S12.3):** instantiate `RemoteTerminalSource` + ghostty grid only when a
   remote pane becomes visible/focused; snapshot-only panes stay metadata (P4). All channels ride the ONE mount
   tunnel (no SSH-connection-per-pane).
10. **Resize `terminal_responses` (RT-F10):** default to remote-host-local handling; propagate only the visual
    resize. Pinned + tested here.

## Files
- **Create** `src/remote/federation/pane_source.rs` — `RemoteTerminalSource` (read task, channel I/O, clipboard).
- **Modify** `src/pane.rs` — `PaneRuntimeIo::Remote` variant + no-op handoff arms + `Remote` lifecycle wiring
  (coordinate with P2 regions; P2 lands the factory, P5 adds the variant).
- **Modify** `src/terminal/runtime.rs` — additive `spawn_remote` constructor (via P2 `Remote` transport policy).
- **Modify** `src/terminal/runtime_registry.rs` — 4 handoff methods skip `Remote` (like test arms).
- **Modify** `src/remote/federation/client.rs` — open per-pane channels on focus; route input/output/clipboard.

## TDD test plan (tests FIRST) — uses P3 loopback
Run `cargo test -p herdr federation::pane_source pane:: terminal::`:
1. **Byte-in feeds the same emulator (CX-4):** inject a byte stream via the loopback `Output` channel → the
   ghostty grid equals what a local PTY produces for the same bytes (reuse `process_pty_bytes` fixtures).
2. **Input/resize/close serialize outward:** a loopback capture asserts `write_user_input`/`resize`/`shutdown`
   produce `TerminalChannel` messages with `map_out`-stripped `terminal_id` + correct `mount_generation`.
3. **No local echo (S2.1):** typing emits no grid change until relayed output returns.
4. **Isolation (S2.2):** a flooding channel does not block a second pane's task (concurrent-progress deadline).
5. **Lifecycle (codex #5):** dropping/closing a `Remote` source emits NO local `PaneDied` and spawns/kills no
   local child; handoff methods called with a `Remote` present → no panic, PTY panes still handled.
6. **Scrollback hydrate (RT-F6):** hydrate applies replayed history first, then live bytes; grid shows history;
   replay capped.
7. **Paste atomicity (S8.3):** bracketed paste relayed as one `Input`, not per-byte.
8. **Clipboard round-trip (RT-F7):** remote OSC clipboard reaches local clipboard with origin tag; local paste
   (incl. image) reaches the remote channel.
9. **Full existing PTY suite green** (P2 oracle re-run — local panes unaffected by the new variant).

## Implementation steps
1. Write failing tests (1-9) against the loopback (no real SSH).
2. Implement `RemoteTerminalSource` + read task via P2's `Remote` transport policy; wire `on_read`.
3. Add `PaneRuntimeIo::Remote` + `spawn_remote` + registry no-op arms + lifecycle contract.
4. Add focus-triggered lazy hydrate + scrollback replay + clipboard channel over the P4 mount. Full suite green.

## Risks + rollback
- **Risk:** a bad remote source blocks the shared render loop (S2.2). Mitigation: per-pane task + bounded buffers
  + test 4. **Risk (codex #5):** remote close kills/mourns a phantom local process. Mitigation: `Remote`
  lifecycle policy + test 5. **Rollback:** revert; the `Remote` variant is unreachable without a P8 mount trigger.

## File ownership
Exclusive: `src/remote/federation/pane_source.rs`. Shared with P2 (sequence after P2): `src/pane.rs`,
`src/terminal/runtime.rs`, `src/terminal/runtime_registry.rs`. Shared with P4: `client.rs` (P4 owns mount, P5
adds channel routing — coordinate).
