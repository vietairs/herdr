# Plan — federation pane-close sync (Gap A + Gap B), TDD

Worktree: `../herdr-worktrees/federation-pane-close-sync`, branch `fix/federation-pane-close-sync`.
Build: `ZIG=$HOME/.local/zig-0.15.2/zig cargo test -- --test-threads=4` (no just/nextest here). 3 pre-existing clippy errors are baseline.

## Phase 0 — Version-bump decision (resolved, not deferred)

**Verified via git evidence, overriding the blindspot/predict reports' "likely no bump" tentative framing:**
`FEDERATION_PROTOCOL_VERSION = 3` (bumped 2→3 at commit `89f40780`/`6bbc829f`, 2026-07-22) **is an ancestor of tag `v0.7.5-hvn.1`** (`git merge-base --is-ancestor 89f40780 v0.7.5-hvn.1` → true). Per memory `herdr-dev-instance-federation-ops`, that exact v3 build was deployed to the local Mac AND both VMs and live-verified end-to-end. So the `SnapshotRequest`/`SnapshotResponse` no-bump precedent (justified by "v3 never shipped in a tagged release with a production call site") **does not transfer** — v3 has shipped and is running on real remote hosts today.
`codec.rs::decode` hard-rejects on ANY `remote_version != FEDERATION_PROTOCOL_VERSION` before even touching the payload, and `negotiate()` hard-rejects the whole handshake on version mismatch — there is no per-message graceful degrade path. A new `FederationMessage::ClosePaneRequest` variant added to an unbumped v3 would handshake fine against an old-v3 VM (versions match) but then fail to `serde_json` deserialize the new variant into the old peer's enum → `CodecError::Malformed` → link fault, on the FIRST close attempted against a not-yet-redeployed peer.
**Decision: bump `FEDERATION_PROTOCOL_VERSION` 3 → 4**, matching the `Fault` 1→2 precedent (new top-level variant, not an additive field). Update the doc comment at `protocol/mod.rs:23-36` to record this reasoning and correct the stale "v3 never shipped" claim for future readers. No test writes code for this phase; it's a constant + doc edit, verified by the existing `FEDERATION_PROTOCOL_VERSION` roundtrip tests still passing under the new value.

## Phase 1 — Protocol wire types (TDD)

Files: `src/remote/federation/protocol/mod.rs`, `codec.rs` (test module only).

1. **Test first**: `close_pane_request_response_roundtrip_through_the_wire_codec` in `protocol/mod.rs`, mirroring `split_pane_request_response_roundtrip_through_the_wire_codec` (mod.rs:551-587) — encode/decode a `ClosePaneRequest{request_id, target_pane_id}` and both `ClosePaneResponse::{Closed{request_id}, Failed{request_id, reason}}` variants, assert roundtrip equality.
2. **Test first**: extend `every_message_variant()` (codec.rs test module) to include the two new variants so the existing "every variant fits its channel cap" style tests exercise them for free.
3. Implement `ClosePaneRequest { request_id: u64, target_pane_id: String }` and `ClosePaneResponse { Closed { request_id: u64 }, Failed { request_id: u64, reason: String } }`, add both to `FederationMessage`, route both to `Channel::Control` (same as Split), matching the doc-comment shape at protocol/mod.rs:264-270 (bare-u64 pairing, no RPC framework).
4. Run tests; both new tests plus the full `protocol::` test module must pass.

## Phase 2 — Serving-host production wiring (TDD)

Files: `src/server/federation_actor.rs`, `src/server/federation_accept.rs`.

1. **Test first** in `federation_actor.rs`: `close_pane_against_a_known_target_pane_closes_it_and_replies_ok`, mirroring `split_pane_against_a_known_target_pane_creates_a_real_pane_and_replies_ok` (federation_actor.rs:878) — construct an `App` with a real pane, send `FederationCommand::ClosePane{target_pane_id, reply}`, assert the reply is `Ok(())` and the pane is actually gone from `App` state.
2. **Test first**: `close_pane_against_an_unknown_target_pane_replies_with_a_reason` mirroring federation_actor.rs:960.
3. Implement `FederationCommand::ClosePane { target_pane_id: String, reply: oneshot::Sender<Result<(), String>> }` (federation_actor.rs:147-153 pattern) and its dispatch arm (federation_actor.rs:376-421 pattern): route through `app.handle_api_request_after_internal_events_drained(Request{method: Method::PaneClose(PaneTarget{pane_id: target_pane_id}), ..})`.
   **Concrete gap found and must be fixed here**: `close_pane`/`handle_pane_close` (`app/api/panes.rs:1706`) resolves its target via `self.parse_pane_id(&target.pane_id)` ONLY — it has no raw-terminal-id fallback. `parse_pane_id` (`app/ids.rs:106`) only understands `p_<ws>_<n>`, `<ws>:p<n>`, `<ws>-<n>`, or an alias — never a bare federation terminal id string. `handle_pane_split` avoids this by calling `resolve_terminal_target` first (panes.rs:40-48) specifically because federation's `SplitPane` command passes a raw terminal id. **`close_pane` needs the identical reordering**: try `resolve_terminal_target(&target.pane_id)` first, fall back to `parse_pane_id`, exactly mirroring split's resolution order — otherwise `FederationCommand::ClosePane`'s raw `target_pane_id` will never resolve and every remote close will spuriously fail with `pane_not_found`.
4. **Test first** in `federation_accept.rs`: `reader_loop_routes_a_close_pane_request_and_replies_closed`, mirroring `reader_loop_routes_a_split_pane_request_and_replies_created` (federation_accept.rs:1712).
5. Implement `handle_close_pane_request` (mirrors `handle_split_pane_request`, federation_accept.rs:568-616) and its `reader_loop` match arm (mirrors line 539).
6. Run `federation_actor::` and `federation_accept::` test modules.

## Phase 3 — Mounting-client dispatch (TDD, AppState-level)

Files: `src/app/api/panes.rs`, `src/app/creation.rs`, `src/app/mod.rs`.

1. **Test first** using `AppState::test_new()`/`App` test harness (follow the existing pattern at `creation.rs:1440-1600` for split): construct an `App` with a federation-mounted remote workspace fixture, call `close_pane` (or `handle_pane_close`) against a remote-namespaced pane id, assert:
   - it does NOT touch local `AppState` (no `ws.close_pane` call on the mount's local mirror pane),
   - it sends exactly one `FederationMessage::ClosePaneRequest` on the mount's `out_tx`,
   - it registers a pending-close entry keyed by the minted `request_id`,
   - it returns the same `*_pending`-style ack `dispatch_remote_pane_split` returns (`remote_close_pending`), not a fabricated success.
2. **Test first**: closing a remote pane whose mount has no live `out_tx` (disconnected) returns `remote_close_unsupported` and touches nothing — mirrors the existing `remote_split_unsupported` test.
3. Implement `dispatch_remote_pane_close` in `panes.rs`, copying `dispatch_remote_pane_split`'s exact shape (panes.rs:217-320): resolve `(raw_target_pane_id, out_tx)` via `ws.terminal_id(pane_id)` → `terminal_runtimes` → `runtime.remote_terminal_id()`/`remote_out_tx()`; resolve `origin` via `federation_host_key_for_workspace`; mint via a new `next_remote_close_request_id()` counter (same `AtomicU64` pattern, panes.rs:34-37); send `ClosePaneRequest`; register a `PendingRemoteClose{workspace_id, pane_id, origin}` (mirrors `PendingRemoteSplit`, creation.rs:1201-1210) in a new `pending_remote_closes: HashMap<u64, PendingRemoteClose>` on `App` (mirrors `pending_remote_splits`, app/mod.rs:161).
4. Insert the remote-classification guard into `close_pane` (panes.rs:1706), mirroring the guard at panes.rs:80-85 for split — **before** the `close_pane_would_close_workspace` confirmation check, since that check is about the *local mount's own* tab count and should not gate a request whose actual close decision belongs to the serving host (see Phase 4 note on the confirmation question).
5. Run `App`/`panes.rs` unit tests.

## Phase 4 — Client-side response + resync interaction (TDD)

Files: `src/app/creation.rs`, `src/remote/federation/client.rs`.

1. **Test first**, mirroring `creation.rs:1505` (`handle_federation_split_pane_ready`) and the origin-mismatch test at `creation.rs:1534`: `handle_federation_close_pane_ready` tears the local pane down (reuses the same `ws.close_pane`/event-emission shape as `handle_federation_resync_pane_removed`, creation.rs:1115-1180) when origin matches; a response tagged with a different mount's `HostKey` is dropped and logged, never applied.
2. **Test first**: `handle_federation_close_pane_failed` surfaces an error (toast/log) and clears the `pending_remote_closes` entry without touching layout.
3. **Test first — idempotency/double-signal regression** (Predict risk 3): construct a pane that is BOTH registered in `pending_remote_closes` (in-flight `ClosePaneRequest`) AND, before the response arrives, gets torn down via the resync path (`handle_federation_resync_pane_removed`) for the same pane id. Assert: no panic, no double-emit of `PaneClosed`, and when the `ClosePaneResponse::Closed` arrives afterward it is a no-op (pane already gone) rather than an error.
4. Implement both handlers, wired from `client.rs`'s response dispatch (same site that currently handles `SplitPaneResponse` — locate its `AppEvent` translation and add `FederationClosePaneReady`/`Failed` analogs to `events.rs`/`api.rs:203`'s dispatch, mirroring the split wiring exactly).
5. Treat "target not found" from the server (`ClosePaneResponse::Failed{reason: "pane not found"}` or equivalent) as an idempotent success on the client (already-closed), not a surfaced error — required for retry/duplicate-click safety (Predict risk 3 mitigation).
6. Run `creation::`/`client::` test modules.

## Phase 5 — Gap B: fix the reverse-index blind spot (TDD, regression-first)

Files: `src/app/creation.rs` (`build_remote_pane` call sites at lines ~599 and ~667 inside `materialize_federation_mount`).

**Root cause, confirmed by direct code read**: `build_remote_pane` (creation.rs:736) never inserts into `self.remote_resync_pane_index`. Only `materialize_resync_pane` (creation.rs:1091) and the split-created path (creation.rs:920) do. So `handle_federation_resync_pane_removed` (creation.rs:1120) silently no-ops (`return`) for ANY pane that was present at initial mount time — the overwhelming majority of panes a user sees — because its reverse-index lookup misses. This is the exact, previously-undiagnosed cause of "closing an original mount-time pane on the serving host doesn't tear down live on the client; only a fresh remount does."

1. **Test first — pins the regression exactly**: in `creation.rs`'s materialization test area (near 1440-1850), build a federation mount with 2+ mount-time panes (going through the real `materialize_federation_mount`/`build_remote_pane` path, not a hand-built fixture that skips it), then call `handle_federation_resync_pane_removed(origin, <mount_time_pane's_raw_id>)` and assert the local pane IS torn down (currently fails — this is the regression test).
2. **Test first**: a second case for a resync/split-created pane (already covered by existing tests at creation.rs:1762/1843) must still pass unchanged — no regression in the already-working path.
3. Implement: after each `build_remote_pane` call in `materialize_federation_mount` (both the tab-root call ~599 and the split-sibling call ~667), add `self.remote_resync_pane_index.insert(pane_info.pane_id.clone(), root_pane_id)` / `self.remote_resync_pane_index.insert(pane_info.pane_id.clone(), split_pane_id)` — making the index a complete reverse-map of every locally-materialized remote pane, not just post-mount-created ones. (Chosen over the alternative — a fallback origin-scoped scan lookup in the removal handler — because no such lookup function exists today and this is strictly smaller: one insert per existing call site, no new data structure.)
4. Run the full `creation::` test module; the regression test from step 1 must now pass.

## Phase 6 — End-to-end loopback + server test (TDD)

Files: `src/remote/federation/loopback.rs`, `src/remote/federation/serve.rs` (test-only `handle_inbound`).

1. **Test first**: `close_pane_request_for_a_known_pane_yields_a_closed_response`, mirroring `split_pane_request_for_a_known_pane_yields_a_created_response` (loopback.rs:614).
2. **Test first**: `close_pane_request_for_an_unknown_pane_yields_a_failed_response`, mirroring loopback.rs:661.
3. Implement: add a `close_pane` method to the `FederationHost` trait (serve.rs:72, alongside `split_pane` at line 105) and its `FixtureHost` impl (loopback.rs:141/213), plus the `handle_inbound` match arm (serve.rs:429, alongside the `SplitPaneRequest` arm at line 438) for the test-only loopback path.
4. Run `loopback::` module; run full `cargo test -- --test-threads=4` for the whole crate.

## Phase 7 — Docs, notes, rollback

- Update `plans/260724-1536-federation-pane-close-sync/` implementation notes (not a stable-docs surface; no `docs/next` change needed — this is an internal protocol/server behavior fix, not new user-facing config/command).
- No CHANGELOG/README touch per project convention (normal fix work).
- Update memory `herdr-federation-no-pane-close-wire-message` (now resolved) and `herdr-remote-pane-repaint-gap`/others only if this plan's scope intersects them — it does not.
- **Rollback**: every phase is additive (new enum variants, new `FederationCommand`, new handlers, one new index-populating call in an existing loop). Nothing here changes an existing wire variant's shape or an existing test's expected behavior except the Phase 5 regression fix, which only adds entries to a `HashMap` that was previously under-populated — reverting is a straight `git revert` per phase commit with no data-migration concern (nothing here is persisted to `session.json`; federation-materialized workspaces are already excluded from snapshot capture).

## Acceptance criteria

- All new tests above pass; no existing test regresses.
- `FEDERATION_PROTOCOL_VERSION` is 4, with an updated, evidence-correct doc comment.
- Closing a remote pane from a mounting client's TUI reaches the serving host and the serving host's pane is actually gone (Phase 2/3 loopback-verified).
- Closing a pane on the serving host via its own CLI (`herdr pane close`) — for BOTH a mount-time pane and a post-mount pane — tears down live on an already-mounted client with no remount required (Phase 5 regression test is the proof).
- `just check`-equivalent (`cargo test -- --test-threads=4` + fmt/clippy modulo the 3 known baseline errors) is green.

---

## Self-validation against blindspot + predict reports

| Report claim | This plan's disposition | Contradiction? |
|---|---|---|
| Blindspot: task's `actions.rs:1933` file:line is wrong; real path is `panes.rs:1706` | Confirmed independently by direct read; plan targets `panes.rs:1706`/`app/mod.rs`/`creation.rs` | No — plan corrects the same way |
| Blindspot: mirror `dispatch_remote_pane_split` pattern for close | Phase 3 does exactly this | No |
| Blindspot: version bump "likely no bump… but verify" | **Resolved, not deferred**: git-evidence shows v3 IS deployed on live VMs → bump 3→4 (Phase 0) | Resolves the report's flagged open question; not a contradiction, an upgrade from "likely" to "confirmed" |
| Predict: "Gap B may already be fixed — verify live before writing any Gap-B code" | Verified by reading code (not live pane test, due to no live VM access in this pass): the mirror mechanism exists but **the reverse-index is provably incomplete** for mount-time panes (Phase 5) — Gap B is real, just narrower than the task's original framing (a bug in an existing mechanism, not an absent one) | No contradiction — sharpens both reports' hedge into a concrete diff |
| Predict: check whether `close_pane`'s target resolution needs `resolve_terminal_target`-style handling | **New finding this pass**, not in either prior report: `close_pane` currently has ZERO raw-terminal-id fallback (unlike split), so routing `FederationCommand::ClosePane`'s raw id through it as-is will always 404 unless fixed (Phase 2 step 3) | Adds detail neither report surfaced; no contradiction |
| Predict risk 3 (double-close race) | Phase 4 step 3 writes the regression test first, per the report's own mitigation shape | No |
| Predict risk 4 (identity via `parse_pane_id`, not raw index) | Honored via `resolve_terminal_target` (which itself falls back to validated resolution, never a raw index) — Phase 2 | No |
| Predict risk 5 (focus/worktree-confirmation fallout) | Phase 3 step 4 explicitly defers the worktree-group confirmation decision to "server owns it," matching the predict report's own Maintainer-persona conclusion | No |
| Unresolved Q (blindspot/predict): "has v3 shipped in a tagged release?" | **Answered**: yes — `git merge-base --is-ancestor` confirms `89f40780` (the 2→3 bump) is an ancestor of `v0.7.5-hvn.1`, and memory confirms live VM deployment at that version | Resolves the single most consequential open question both reports flagged |

**Verdict: no contradictions found.** Both prior reports' hedges are resolved with concrete evidence in this pass (version-bump decision, Gap B's precise scope, and one new finding — `close_pane`'s missing raw-id resolution — that neither prior report caught).

## Remaining unresolved question

- Whether pane-number allocation (`next_public_pane_number`) ever resets/recycles within a workspace's lifetime (predict report's risk 4 softening question) — not re-verified this pass; low severity given `resolve_terminal_target`/`parse_pane_id` never trust a raw index directly regardless.
