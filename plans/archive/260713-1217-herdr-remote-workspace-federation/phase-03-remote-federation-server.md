# Phase 03 — Remote federation server surface (`herdr federation-serve`)

**Goal:** the REMOTE-side surface. A new headless subcommand that speaks the P1 federation protocol: emits an
atomic snapshot+cursor, an ordered event stream, per-terminal RAW byte channels tapped at the
`process_pty_bytes` source, and an agent-status stream — advertising the federation capability +
`server_instance_id` in the handshake. Ships the **in-process loopback harness** used by P4-P9 tests.
**Depends on:** P1. **Blocks:** P4. **Shippable:** yes (new subcommand; classic paths untouched).

## Context
- **This is the phase the new architecture constraint unlocks** — we control the remote binary, so we add a
  clean server surface instead of retrofitting the attach stream.
- Verified: the attach stream is NOT raw PTY bytes — it is `TerminalAnsi{ blit_encoder, seq }` rendered diffs of
  the composited grid (`src/server/render_stream.rs:16,56-75`), single-writer (`terminal_attach_owners`,
  `src/server/headless.rs:2348`). So federation MUST tap a different source.
- Verified: the ONLY raw-PTY-byte tap is the `on_read` closure feeding `process_pty_bytes`
  (`src/pane.rs:1722,1880`). Federation output channels must fork bytes at that point (a broadcast tee), NOT from
  the render path.
- Verified: scrollback replay source candidate `handoff_history_ansi()` (`src/server/headless.rs:980`,
  `runtime.handoff_history_ansi()`) — evaluate as the `Open{replay}` source.
- Verified: remote subcommand dispatch precedent — `remote-client-bridge` at `src/main.rs:472-474` →
  `run_remote_client_bridge()` (`src/remote/unix.rs:194`); server autospawn via
  `server::autodetect::spawn_server_daemon()` (`unix.rs:221-238`). Add `federation-serve` alongside.
- Verified: `SessionSnapshot` exists and is fully structured (`src/api/schema/session.rs:9`); `EventHub`
  `events_after`/`current_sequence` (`src/api/event_hub.rs:28,40`) provide the change feed to translate into the
  ordered stream with `source_seq`.

## Requirements
1. **Subcommand `herdr federation-serve`** (headless): dispatched in `main.rs`, runs against the remote's own
   in-process `AppState`/`EventHub` (reuse `spawn_server_daemon` autostart like the bridge does). Speaks P1
   protocol over stdin/stdout (so it rides `SshStdioBridge` unchanged).
2. **Handshake producer (RT-F4):** advertise `Capability::Federation` + `federation_protocol_version` (P1) +
   a fresh `server_instance_id` generated at server boot (stable for the server's lifetime).
3. **Atomic mount (codex #2):** on client mount, emit ONE `MountSnapshot { server_instance_id, snapshot, cursor }`
   where `cursor == EventHub::current_sequence()` at snapshot time (consistent pair — hold the state lock across
   snapshot+cursor read so no event slips between).
4. **Ordered event stream (codex #1):** translate `EventHub` events into `EventFrame { source_seq, .. }` with
   `source_seq` monotonic from the mount cursor; emit `Gap`/`Reset` markers if the 512-cap ring dropped events
   the client hasn't seen (detect via cursor distance) so the client can re-sync — the remote never assumes the
   client kept up.
5. **Raw terminal byte channels (codex #4 / CX raw-tap):** on `TerminalChannel::Open{terminal_id, replay}`,
   attach a **broadcast tee** at the pane's `on_read` source so federation gets the exact bytes
   `process_pty_bytes` consumes (NOT rendered frames); stream `Output{bytes}`; on open, first replay bounded
   scrollback (RT-F6) from the chosen source. Accept `Input`/`Resize`/`Close` and apply to the remote pane.
   Multiple federation consumers must coexist with the local render path (tee, not takeover) — no single-writer
   attach ownership.
6. **Agent-status stream:** relay the remote's existing `AgentInfo`/`pane.agent_status_changed`
   (already computed remotely — `src/pane.rs` detection) as `AgentStatus` messages.
7. **Clipboard/OSC channel:** forward pane clipboard/OSC events tagged with `origin_tag` (consumed by P5/P7).
8. **Bounded framing:** all outbound framing uses the P1 codec with per-channel caps. Backpressure a flooding
   pane so it cannot unbounded-buffer on the remote.
9. **In-process loopback harness (test infra):** a `LoopbackFederationServer` that runs the same protocol
   handler over an in-memory duplex instead of stdin/stdout, so P4-P9 test without SSH or a real remote.

## Files
- **Modify** `src/main.rs` — dispatch `federation-serve` (near `472-474`); keep classic subcommand exclusivity.
- **Create** `src/remote/federation/serve.rs` — the server-side protocol handler (handshake, mount, event
  translate, channel management, agent/clipboard forwarding, backpressure).
- **Create** `src/remote/federation/tee.rs` — broadcast tee attaching at the `on_read` source without perturbing
  the local render path.
- **Modify** `src/pane.rs` — expose a `subscribe_output_bytes(terminal_id) -> tee handle` at the `on_read` seam
  (additive; local path unchanged). **Coordinate with P2's `pane.rs` regions** (different function areas — P2
  owns the source/io trait wiring, P3 owns the read-tap subscription; sequence P3 after P2 lands to avoid churn).
- **Create** `src/remote/federation/loopback.rs` — `LoopbackFederationServer` test harness (also usable
  `#[cfg(test)]` from P4+).
- **Modify** `src/remote/federation/mod.rs` — `pub mod serve; pub mod tee; pub mod loopback;`.

## TDD test plan (tests FIRST)
Run `cargo test -p herdr federation::serve federation::tee federation::loopback`:
1. **Handshake advertises capability + instance id:** a mounting client reads `Capability::Federation`, a
   version, and a non-empty `server_instance_id`; a fresh boot yields a new id.
2. **Atomic snapshot+cursor:** mount returns a `SessionSnapshot` and a `cursor`; an event pushed AFTER the
   snapshot has `source_seq == cursor+1` (no gap, no double-count) — proves the consistent pair.
3. **Raw byte tap fidelity (CX-4, top risk 1):** drive a fixture pane with a known byte stream; the federation
   `Output` bytes equal the bytes `process_pty_bytes` consumed (assert against the local grid the same bytes
   produce) — proves raw, not rendered.
4. **Tee coexistence:** the local render path AND a federation channel both receive pane output; neither starves
   the other; opening/closing a channel does not disturb local rendering.
5. **Scrollback replay (RT-F6):** `Open{replay}` first emits bounded history bytes, then live bytes; replay is
   capped.
6. **Gap/reset on ring overflow (codex #1):** force > 512 events between polls; the stream emits a `Gap`/`Reset`
   the client can detect (no silent loss).
7. **Loopback harness:** `LoopbackFederationServer` completes a full handshake→mount→event→channel cycle in
   process (the P4-P9 test substrate).

## Implementation steps
1. Write failing tests (1-7) against the loopback harness.
2. Implement the server handler: handshake, atomic mount, event translate + gap detection.
3. Implement the read-tap tee + terminal channels (raw output, input/resize/close, scrollback replay).
4. Wire agent-status + clipboard forwarding; add the `main.rs` subcommand dispatch. Full suite green.

## Risks + rollback
- **Risk (top-1):** tapping the wrong source (rendered vs raw) silently double-emulates downstream. Mitigation:
  tap at `on_read` only; test 3 asserts byte-fidelity. **Risk:** the tee perturbs local rendering. Mitigation:
  broadcast (never takeover); test 4. **Risk:** ring overflow loses events silently. Mitigation: Gap/Reset;
  test 6. **Rollback:** revert; `federation-serve` is a new subcommand — classic remote paths untouched, so an
  old-or-reverted remote simply lacks the capability and the local side falls back (P8).

## File ownership
Exclusive: `src/remote/federation/serve.rs`, `tee.rs`, `loopback.rs`. Shared: `src/main.rs` (dispatch line — no
overlap with P8's federation flag branch, coordinate), `src/pane.rs` (read-tap subscription — sequence after P2).
