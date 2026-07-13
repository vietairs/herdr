# Phase 01 — Federation protocol + id-fencing (shared types, both ends) — ROOT A

**Goal:** define the purpose-built federation protocol as shared Rust types compiled into BOTH ends, plus the
id-namespacing + mount-generation fencing primitives. Pure library, no I/O, no live path. **Depends on:**
nothing. **Blocks:** P3, P4, P5. **Shippable:** yes (dormant lib). **ROOT — parallel with P2.**

## Context
- Verified: no server-instance epoch on `SessionSnapshot` (only `version`+`protocol`, `src/api/schema/session.rs:9`).
- Verified: EventHub single-source, global `next_sequence`, 512 cap (`src/api/event_hub.rs:8,13`); per-sub serial
  `last_sequence` cursors (`src/api/subscriptions.rs:54,317`). Federation must NOT depend on this ring — the
  reducer owns its own resumable cursor.
- Verified: id generators are per-process `AtomicU64` monotonic (`src/workspace.rs:73-79`); `TerminalId` =
  `term_{micros:x}{ctr:x}` (`src/terminal/id.rs:10-22`); `PaneId(u32)` internal (`src/layout.rs:11-19`).
- Verified: existing framed codec in `src/protocol/wire.rs:20-29` (u32 LE length prefix, `MAX_FRAME_SIZE=2MB`,
  `MAX_GRAPHICS_FRAME_SIZE=32MB`) — reuse the framing *style* (DRY), but federation has its OWN version counter,
  independent of `PROTOCOL_VERSION=16`.
- Red-team F1/F2/F3, codex #1/#2. Scenario S3.1/S4.1/S8.1.

## Requirements
1. **Protocol message types** (serde, one module), covering all six channel classes from `plan.md`:
   - `Handshake { federation_protocol_version: u32, capabilities: BTreeSet<Capability>, server_instance_id }`
     and its response (accept/reject-with-reason).
   - `MountSnapshot { server_instance_id, snapshot: SessionSnapshot, cursor: EventCursor }` — the atomic mount.
   - `EventFrame { source_seq: u64, kind: EventKind }` + control markers `Gap { from, to }`, `Reset`.
   - `TerminalChannel` messages: `Open { terminal_id, mount_generation, replay: ScrollbackReplay }`,
     `Output { terminal_id, mount_generation, bytes }`, `Input`, `Resize`, `Close`, tagged with
     `{terminal_id, mount_generation}` on every variant.
   - `AgentStatus { terminal_id, mount_generation, status }`.
   - `Clipboard { origin_tag, payload }` (origin-tagged; consumed by P5/P7).
2. **Framed codec** — a single `encode`/`decode` with a per-channel `max_len` cap; decode errors are typed
   (`FrameTooLarge`, `Malformed`, `VersionSkew`). No hand-rolled per-call-site decode anywhere downstream.
3. **Version + capability negotiation logic** (pure): `negotiate(local, remote) -> Result<AgreedCaps, RejectReason>`;
   skew ⇒ typed reject (feeds P8 fallback). Additive: unknown capabilities are ignored, not fatal.
4. **id-fencing primitives** (`id.rs`): `HostKey` (stable per `user@ip`+session discriminator), `FedRef`
   newtype = `HostKey + server_instance_id + mount_generation + remote_id`; `map_in(remote_id, mount) -> FedRef`,
   `map_out(FedRef) -> remote_id`, `classify(id) -> Local | Remote(HostKey)`. Total, reversible, stable.
5. **Generation fencing**: `Mount { host_key, server_instance_id, mount_generation }`;
   `fence(mount, inbound_generation) -> Accept | RejectStale`. Every inbound protocol message carries the
   generation; a message whose generation ≠ live mount generation is rejected (never routed).
6. **Public-API contract:** non-federating users see byte-for-byte unchanged ids. Namespaced ids appear only
   under a live mount. Prefix cascades (no double-prefix `r:h:r:h:w1`).

## Files
- **Create** `src/remote/federation/protocol/mod.rs` — message enums + `Capability` + version const.
- **Create** `src/remote/federation/protocol/codec.rs` — framed encode/decode + per-channel caps + typed errors.
- **Create** `src/remote/federation/protocol/negotiate.rs` — version/capability negotiation (pure).
- **Create** `src/remote/federation/id.rs` — `HostKey`, `FedRef`, `Mount`, `map_in`/`map_out`/`classify`/`fence`.
- **Create** `src/remote/federation/mod.rs` — `pub mod protocol; pub mod id;`.
- **Modify** `src/remote/mod.rs` (or `src/remote.rs` root — verify which is the module root; `src/remote.rs`
  re-exports unix) — add `pub mod federation;`. **This is the only existing file P1 touches** (one line).

## TDD test plan (tests FIRST)
Run `cargo test -p herdr federation::protocol federation::id`:
1. **Codec round-trip** — every message variant encode→decode is identity; fuzz-style over generated payloads.
2. **Frame-cap enforcement** — a payload > channel `max_len` decodes to `FrameTooLarge`, never allocates the
   oversize buffer.
3. **Version skew** — `negotiate` with mismatched `federation_protocol_version` ⇒ typed `RejectReason::Version`;
   an added future capability is ignored gracefully (additive proof).
4. **id round-trip + no-collision** — `map_in` then `map_out` is identity; local `w1` and remote `w1` under two
   different `HostKey`s produce three distinct `FedRef`s; prefix cascades without double-prefix.
5. **Generation fencing** — `fence` accepts the live generation, rejects a stale one; a `TerminalChannel::Output`
   tagged with an old `mount_generation` is rejected.
6. **Atomic mount consistency** — `MountSnapshot.cursor` is the cursor the snapshot is consistent with (a
   subsequent `EventFrame.source_seq` == cursor+1 is the first applied event; a gap is detectable).

## Implementation steps
1. Write failing tests (1-6).
2. Implement message enums + `Capability` + version const; implement the framed codec with per-channel caps.
3. Implement negotiation + id/fence primitives.
4. Wire `pub mod federation;`; `cargo build` + `cargo test` + `cargo clippy`. No call sites yet (P3/P4 consume).

## Risks + rollback
- **Risk:** a channel forgets the `{terminal_id, mount_generation}` tag ⇒ fencing gap. Mitigation: tag is a
  required field on the shared struct, enforced by the type system; test 5. **Risk:** codec cap bypass.
  Mitigation: single codec, no downstream decode; test 2. **Rollback:** delete the module + the one-line
  `pub mod`; zero behavior change.

## File ownership
Exclusive: `src/remote/federation/protocol/*`, `src/remote/federation/id.rs`, `src/remote/federation/mod.rs`.
Shared (1 line, coordinate): `src/remote/mod.rs`/`src/remote.rs` module root. No overlap with P2.
