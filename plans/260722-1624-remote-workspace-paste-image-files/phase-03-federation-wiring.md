# Phase 03 — federation wiring (inbound dispatch, off-thread staging, capability gate)

## Context (verified against source)

### Production inbound path (predict correction C1 — confirmed)

- `src/remote/federation/serve.rs:412-415` — `handle_inbound` carries
  `#[allow(dead_code)]` with the comment "only reachable through `run()`, kept for `loopback.rs`'s
  tests". `MOUNT_GENERATION` at `serve.rs:43-44` has the same note. **Not the production site.**
- `src/server/federation_accept.rs:444-503` — `reader_loop`, one blocking `std::thread` per mount;
  the `SplitPaneRequest` arm is at `:478-480`, `SnapshotRequest` at `:481-489`.
- `src/server/federation_accept.rs:511-555` — `handle_split_pane_request`: `blocking_send` +
  `rx.blocking_recv()` **inline on the reader thread**, no timeout. This is the shape to *not* copy
  for a multi-MiB write (predict HIGH-2).
- `src/server/federation_accept.rs:705-724` — the `OutputPump` spawn pattern (`std::thread::spawn`,
  cloned `out_tx`, own stop flag). `:741-769` `output_pump`. `:777-792` `enqueue_outbound` —
  bounded `try_send`, fails fast to `EgressOverflow`. This is the separation to mirror.
- `src/server/federation_accept.rs:90-97` — `federation_capabilities()`, the co-located host's
  advertised set (`SCROLLBACK_REPLAY`, `AGENT_STATUS`).
- `src/server/federation_accept.rs:100-103` and `src/remote/federation/serve.rs:150-153` — both
  `global_max_frame()` hardcode `Channel::Clipboard.max_len()` as "the largest cap". **Both must
  become `Channel::largest_max_len()`** (phase 01 requirement 4) or every stage frame is rejected
  before decode.

### Client side

- `src/remote/federation/client.rs:98-103` — `MountedConnection { mirror, agreed_capabilities,
  reader, writer }`; `:165-190` handshake + `required_capabilities` check; `:208-220` mount
  construction. `agreed_capabilities` is returned but **dropped on the floor** by
  `session.rs:225-235` (`DialAndMountOutcome` does not carry it) — this is the missing plumbing
  predict CRITICAL-3 names.
- `src/remote/federation/reducer.rs:91-105` — `struct RemoteMirror` and `:107-117` `new()`. This is
  where `agreed_capabilities` lands (predict CRITICAL-3); `AppState.remote_mirrors`
  (`app/state.rs:1595-1598`) then carries it to every send site. Capabilities are pure data, so
  `AppState` stays pure.
- `src/remote/federation/session.rs:152-159` — `local_capabilities()` advertised by the mounting
  client; `:167-173` `DialAndMountOutcome`; `:180-235` `dial_and_mount`.
- `src/remote/federation/client.rs:470-479` — `drive_mount_channel` signature;
  `:621-731` the `SplitPaneResponse` arm, including the `AppEvent::FederationSplitPaneFailed` emit
  at `:704-712` and `:715-727`. `:324-340` `SplitMaterializationContext` carries `origin: HostKey`.
- `src/events.rs:191` / `:203-207` — `AppEvent::FederationSplitPaneReady` /
  `FederationSplitPaneFailed`, both `#[cfg(unix)]`.
- `src/remote/federation/loopback.rs:232-256` — `LoopbackFederationServer::spawn`, the in-memory
  duplex e2e harness (1 MiB duplex buffer — note for the large-frame test).

## Requirements

1. Inbound `ClipboardStageRequest` dispatched from `reader_loop`, **never staged inline on it**.
2. Both `global_max_frame()` call sites use `Channel::largest_max_len()`.
3. `FILE_STAGING` advertised by both the co-located host (`federation_accept::federation_capabilities`)
   and the mounting client (`session::local_capabilities`), and additionally by
   `serve.rs`'s host so the loopback e2e test negotiates it.
3b. **Serving-side advertisement and handling are `#[cfg(unix)]`; the modules that host them are
   not.** Verified: `src/server/mod.rs:8` declares `pub(crate) mod federation_accept;` and
   `src/remote/federation/mod.rs:46` declares `pub(crate) mod serve;`, both **unconditionally**, and
   `serve.rs` contains no `cfg(unix)` at all — while phase 02 requirement 8 gates `file_staging` to
   Unix. An ungated call into `file_staging` from either file is an unresolved symbol under
   `just windows-lint` (justfile:26). Therefore, on the serving side: gate the `FILE_STAGING` entry in
   `federation_capabilities()` and in `FixtureHost`'s default set with `#[cfg(unix)]`, and gate the
   staging worker, its spawn, and both `ClipboardStageRequest` arms (steps 3-6) with `#[cfg(unix)]`.
   On non-Unix the capability is therefore never advertised, never agreed, and the inbound arm is the
   existing "capability not agreed → log and drop the frame" path of requirement 6 — no new
   behaviour, no stub. The mounting side (`session::local_capabilities`, `client.rs`) needs no gate:
   it touches no filesystem.
4. `agreed_capabilities` stored on `RemoteMirror` at mount time and reachable from every send site.
   Not `required` — a peer without it must still mount.
5. `ClipboardStageResponse` handled in `drive_mount_channel`, converted to two new `#[cfg(unix)]`
   `AppEvent`s carrying `request_id`, the mount's `origin: HostKey`, **and** the mount's
   **connection epoch** (requirement 7b — the locally minted per-mount fence).
6. **The capability gate is enforced on BOTH sides, independently.** Verified: `drive_handshake`
   returns `io::Result<Option<AgreedCaps>>` (`federation_accept.rs:1090-1094`) but the caller
   discards it — `if drive_handshake(..)?.is_none() { return }` at `:245` — and neither `drive_mount`
   (`:274`), `run_connection` (`:349`) nor `reader_loop` (`:444`) takes an `AgreedCaps`. So today the
   serving side has no way to know what was negotiated, and request/response ordering is **not** an
   enforcement boundary: a newer host would happily emit `ClipboardStageResponse` to an older
   controller, whose `read_frame` fails on the unknown externally-tagged variant
   (`CodecError::Malformed` → `read_frame` Err → the whole mount dies). Therefore:
   - carry the negotiated `AgreedCaps` from `drive_handshake` through `drive_mount` →
     `run_connection` → `reader_loop` → the staging worker;
   - the serving side **drops** an inbound `ClipboardStageRequest` (log, no response frame at all)
     when `FILE_STAGING` is not in the agreed set — an unnegotiated request is a protocol violation
     by the peer, not a user-visible failure;
   - the serving side **must not construct either `ClipboardStage*` frame** unless `FILE_STAGING`
     was agreed. Make that structural: the staging worker is only spawned when the capability is
     agreed, and the response path only exists behind the worker;
   - the mounting side keeps its own gated send helper (step 10) so an ungated send can never leave
     an older-peer mount either.
7b. **Per-mount connection epoch (A2, replacing `server_instance_id`).** `mount_generation` is
   degenerate (always 1: `federation_accept.rs:66`, `serve.rs:44`) and `server_instance_id` is minted
   per remote *server process*, so it cannot distinguish a fresh mount from a stale one against the
   same still-running remote. Mint a `MountConnectionEpoch(u64)` locally from a process-global
   `AtomicU64` on each successful `connect_and_mount`, store it on `RemoteMirror`, and carry it on
   every event that must be fenced: the two stage-response events **and** the existing
   `AppEvent::FederationMountEnded`. This is a local workaround inside this feature, not a
   federation-wide fencing refactor (see plan.md A2).

## Deviation from the phase skeleton (deliberate)

The skeleton calls for "a `FederationCommand` variant in `federation_actor.rs`". **Not doing that.**
Staging is pure filesystem work: it needs no `App`, no `FederationLease`, no session state — unlike
`SplitPane`, which exists precisely to reach the live `App`. Routing a multi-MiB payload through
`ServerEvent` into the single-threaded server event loop would put the allocation and the dispatch
on the hot loop for zero benefit, and `dispatch()` (`federation_actor.rs:174+`) is a synchronous
`&mut App` function that would then have to `spawn_blocking` anyway.

Instead: a **per-connection staging worker thread**, spawned alongside `reader_loop` and fed by a
bounded `std::sync::mpsc::sync_channel`, owning a cloned `out_tx`. This is exactly the separation
`OutputPump` (`federation_accept.rs:705-724,741`) already uses and exactly what predict HIGH-2
prescribes ("reader enqueues only; write on a bounded worker … mirroring `output_pump`'s
separation"). `federation_actor.rs` is untouched.

## Files

| Action | File | Owner |
|---|---|---|
| modify | `src/server/federation_accept.rs` | phase 03 (exclusive) |
| modify | `src/remote/federation/serve.rs` | phase 03 (exclusive) |
| modify | `src/remote/federation/client.rs` | phase 03 (exclusive) |
| modify | `src/remote/federation/reducer.rs` | phase 03 (exclusive) |
| modify | `src/remote/federation/session.rs` | phase 03 (exclusive) |
| modify | `src/events.rs` | phase 03 (exclusive) |
| modify | `src/app/actions.rs` | phase 03 (exclusive) — three `=> Vec::new()` arms only |
| modify | `src/remote/federation/loopback.rs` | phase 03 (exclusive) — e2e test only |
| modify | `src/app/api/workspaces.rs` | phase 03 — **only** the `AppEvent::FederationMountEnded` construction sites (production + tests) plus the `handle_federation_mount_ended` **signature** and its doc comment, to accept the new `connection_epoch` (step 11a). The handler **body** logic — the epoch fence and the purges — is phase 04's. |
| modify | `src/app/api.rs` | phase 03 — **only** the `AppEvent::FederationMountEnded` destructure at `:161-168` and its forwarding call (step 11a). The three new `ClipboardStage*` dispatch arms are phase 04's. |

`federation_actor.rs` is **not** modified (see Deviation). The rest of `app/**` is phase 04/05;
`actions.rs` is listed here because `AppState::apply_event`'s exhaustive match makes it a
compile-time dependency of the `events.rs` edit, and phase 03 owns `events.rs`.

## Implementation steps

### Remote (serving) side

1. `federation_accept.rs`: `global_max_frame()` → `Channel::largest_max_len()`. Same in
   `serve.rs:150-153`.
2. `federation_accept.rs`: add `Capability::FILE_STAGING` to `federation_capabilities()` (`:90-97`).
   **`AppFederationHost` no longer exists** — verified, it survives only in doc comments
   (`federation_accept.rs:88`, `headless.rs:307` "Replaces `AppFederationHost`"). The loopback host
   is `FixtureHost` (`loopback.rs:33`, `capabilities()` at `:139`, default set built at `:53`); add
   `FILE_STAGING` to that default so tests 3 and 4 negotiate it. `FixtureHost` is phase-03-owned via
   the `loopback.rs` row.
2b. `federation_accept.rs`: thread the negotiated capabilities (requirement 6). `drive_handshake`'s
   `AgreedCaps` becomes a binding, passed to `drive_mount`, `run_connection` and `reader_loop`.
   Nothing else about the handshake changes.
3. `federation_accept.rs`: add a `StagingWorker` spawned once per connection next to the reader
   thread — **only when `FILE_STAGING` is in the agreed set** — admitting at most **two requests in
   the system at once, counting the one the worker is actively staging**. A bare
   `sync_channel(2)` does **not** express this: once the worker has `recv()`d a request, two more fit
   in the queue, so three stages are live and the ~58 MiB-per-stage memory budget the locked cap of 2
   was derived from (plan.md A1) is exceeded by a buggy or hostile controller. Implement the bound as
   an explicit two-permit admission counter — an `Arc<AtomicUsize>` (or a semaphore) acquired by
   `reader_loop` **before** `try_send` and released only after the worker has enqueued or discarded
   that request's response — over a `sync_channel(2)`. The counter, not the channel depth,
   is the contract; the channel then never fills in practice.
   **The permit must be released on every path that does not hand the request to the worker.** It is
   acquired before `try_send`, so a `try_send` returning `Err` (full, or the worker gone after
   teardown) would otherwise leak a permit permanently: the counter never returns to zero and that
   connection answers `Busy` for the rest of its life while nothing is actually staging. Represent
   the permit as an RAII guard **moved into the queued request** and dropped by the worker after the
   response is enqueued or discarded, so the `try_send` error path (which hands the request, and
   therefore the guard, straight back) and a dropped queue both release it automatically. A bare
   `fetch_add`/`fetch_sub` pair whose decrement lives only in the worker loop is the leaky form and
   is not acceptable. On a refused admission, respond
   `Failed { Busy }` (**not** `WriteFailed` — phase 05 maps
   `WriteFailed` to a disk-space toast, and transient backpressure is not a disk failure) and log;
   never block the reader.
   **Teardown must be bounded and must never gate the controller lease.** The lease is released by
   `LeaseReleaseGuard` in `drive_mount` (`:290` neighbourhood), which drops only after
   `run_connection` returns; so any unbounded `join()` inside `run_connection`'s teardown
   (`:405-428`) pins the remote's single-controller slot for as long as the worker is stuck, and
   every subsequent mount to that host is refused. Dropping the queue `SyncSender` only wakes a
   worker parked in `recv()`; it cannot interrupt `cleanup_stale`, base64 decoding, or `write_all`.
   Therefore:
   - the worker is **detached — never joined**;
   - it does not own a plain `out_tx` clone (that would hang the writer, whose `recv()` has no
     timeout, `:804`). It holds an `Arc<Mutex<Option<SyncSender<FederationMessage>>>>` handle it
     locks **only** for the moment of enqueueing a response, never across filesystem work;
   - teardown, placed with the pump joins at `:414-417`: set `shutdown`, drop the request queue
     sender, then set the shared handle to `None`. That drops the worker's only outbound sender
     synchronously, so `drop(out_tx)` at `:427` still disconnects the writer and `writer.join()`
     stays bounded;
   - a worker that finishes late finds `None` and discards its result. Log at `debug!`; a discarded
     late response is correct, not an error.
4. The worker loop, in this order: **decode base64 first** — on failure respond
   `Failed { InvalidPayload }` and `continue`, with **no filesystem call at all** (a malformed but
   frame-legal payload must never be reported as a disk failure and must never create a partial
   file) — then `file_staging::stage_remote_clipboard_image(&original_filename, &bytes)` → build
   `ClipboardStageResponse::Staged{path}` or `::Failed{failure}` → enqueue through the revocable
   handle. Exit on the shared `shutdown` flag or a disconnected queue, like `output_pump`.
   The staging call is reached through a small `StagingOp` indirection (a `fn(&str, &[u8]) ->
   Result<PathBuf, ClipboardStageFailure>` field on the worker, defaulting to the real function) so
   test 7 can inject a deliberately blocked operation. No trait, no dyn dispatch beyond that.
5. `reader_loop`: add an `Ok(Some(FederationMessage::ClipboardStageRequest(request)))` arm that
   checks the agreed set, then takes an admission permit (step 3) and only `try_send`s onto the
   worker queue and immediately continues; a refused permit enqueues `Failed { Busy }` and continues.
   No filesystem call, no `blocking_recv`. With the capability absent there is no worker and no queue:
   log and drop the frame.
6. `serve.rs::handle_inbound`: add the same arm, with the **same** capability check and the same
   base64-decode-before-filesystem ordering, calling the **same** `file_staging` function inline.
   This path is test-only (it keeps `#[allow(dead_code)]`) and exists so the loopback e2e test can
   exercise the real staging module end to end; a comment must say so and point at `reader_loop` as
   production.

### Client (mounting) side

7. `reducer.rs`: add `agreed_capabilities: BTreeSet<Capability>` and
   `connection_epoch: MountConnectionEpoch` to `RemoteMirror`, with
   `set_agreed_capabilities` / `supports(&Capability) -> bool` and a `connection_epoch()` accessor.
   `RemoteMirror::new` defaults the set empty and the epoch to a sentinel `0` (keeps every existing
   test construction compiling). Both are pure data, so `AppState` stays pure.
7b. `client.rs`: define `pub(crate) struct MountConnectionEpoch(u64)` — **`pub(crate)`, with
   `pub(crate)` accessors/constructors, is required, not stylistic**: the type appears in
   `src/events.rs` (`AppEvent` fields), `reducer.rs`, `app/api.rs`, `app/api/workspaces.rs` and
   `app/remote_clipboard_stage.rs`, none of which is a descendant of `remote::federation::client`.
   `federation/mod.rs:55` already declares `pub(crate) mod client;` unconditionally, so the path
   resolves once the type itself is `pub(crate)`; the type must **not** be `#[cfg(unix)]`-gated,
   because `reducer.rs` and `client.rs` are compiled on Windows (only the `AppEvent` variants
   carrying it are Unix-gated). Derive `Copy`, `Clone`, `Debug`, `PartialEq`, `Eq`. Mint it from a
   process-global `AtomicU64`
   (`fetch_add`) exactly once per successful `connect_and_mount`. Doc-comment the invariant directly:
   `mount_generation` is a constant and `server_instance_id` is per remote *process*, so neither can
   distinguish a fresh mount from a superseded one against the same running remote; this counter is
   minted locally per mount and therefore can.
8. `client.rs::connect_and_mount` (`:208-220`): call `mirror.set_agreed_capabilities(...)` and stamp
   the minted epoch before returning, so both travel with the mirror through `DialAndMountOutcome` →
   `handle_federation_mount_ready` → `AppState.remote_mirrors` with no further plumbing. The drive
   task captures the same epoch so every event it emits carries it.
9. `session.rs::local_capabilities()` (`:152-159`): advertise `FILE_STAGING`.
10. `client.rs`: add `send_clipboard_stage_request(mirror, out_tx, request) -> Result<(),
    StageSendError>` — **the single gated send site**. It returns `Err(CapabilityNotAgreed)` without
    touching `out_tx` when `!mirror.supports(FILE_STAGING)`. Doc-comment that an ungated send kills
    the whole mount against an older peer (`serve.rs:184-185` → `?` → `drive_mount_channel` exits).
11. `events.rs`: add **all three** `#[cfg(unix)]` variants now (phase 04 needs the third and does
    not own this file):
    - `FederationClipboardStageReady { request_id, remote_path: String, origin: HostKey,
      connection_epoch: MountConnectionEpoch }`
    - `FederationClipboardStageFailed { request_id, failure: ClipboardStageFailure, origin,
      connection_epoch }`
    - `FederationClipboardStageTimedOut { request_id, origin, connection_epoch }`
    Doc-comment the `connection_epoch` field with the invariant, not a plan label: `mount_generation`
    is a constant and `server_instance_id` is per remote process, so the locally minted per-mount
    epoch is the only value that distinguishes this mount from a superseded one against the same
    running remote.
11a. `events.rs`: add the same `connection_epoch: MountConnectionEpoch` field to the **existing**
    `AppEvent::FederationMountEnded` (`events.rs:177-182`), whose current fields are
    `host_key`/`generation`/`target`/`reason`. Without it a delayed end-notice from a superseded
    connection passes the generation fence (`generation` is always 1) and tears down a fresh mount to
    the same host. Update its doc comment to state that the epoch, not the generation, is what
    actually fences it today. **Adding a field to this variant breaks three existing sites, and phase
    03 owns all three** — without them the tree does not compile at the end of this phase:
    - `app/api/workspaces.rs`: the `AppEvent::FederationMountEnded { .. }` construction literal near
      `:255` and the matching literal in that file's tests;
    - `app/api.rs:161-168`: the dispatch destructures the variant **exhaustively, without `..`**
      (verified in source), so it must gain the `connection_epoch` binding and forward it;
    - `app/api/workspaces.rs:313-319`: `handle_federation_mount_ended`'s signature therefore gains a
      `connection_epoch: MountConnectionEpoch` parameter. Phase 03 changes the **signature and doc
      comment only** and consumes the value in a single `tracing::debug!` line so the build stays
      warning-clean; phase 04 step 5 replaces that line with the real epoch fence. The handler's
      teardown logic and the purge calls remain phase 04's.
    `app/actions.rs:2833` matches with `{ .. }` and needs no change for this field.
11b. `app/actions.rs`: add the three matching `#[cfg(unix)] … => Vec::new()` arms alongside
    `:2835-2841`, reusing the existing comment block. Without them `AppState::apply_event`'s
    exhaustive match fails to compile.
12. `client.rs::drive_mount_channel`: add the `FederationMessage::ClipboardStageResponse` arm,
    emitting those events. Source `origin` from the existing context and `connection_epoch` from the
    epoch this drive task was created with (step 7b). **Do not sanitise the path here** — the reject-don't-strip
    contract belongs to phase 04, at the injection boundary, where it can be tested against
    `try_send_paste` directly.

## Tests — TESTS FIRST

Starred (predict section F) — write before the implementation step named:

1. ★`stage_request_is_not_sent_when_capability_not_agreed` (before step 10) — a `RemoteMirror` with
   an empty agreed set; `send_clipboard_stage_request` returns `Err(CapabilityNotAgreed)` and the
   `out_tx` receiver is empty. This is the guard for P3.

Non-starred, this phase:

2. `reader_loop_hands_a_clipboard_stage_request_to_the_staging_worker_without_blocking` (before
   step 5) — drive `reader_loop` with a stage request followed by a `Terminal::Input` frame; assert
   the input frame is processed while the staging queue still holds the request (a stalled worker
   must not stall the reader). Model on
   `reader_loop_routes_a_split_pane_request_and_replies_created` (`federation_accept.rs:1346`).
3. `clipboard_stage_request_end_to_end_through_loopback_server` (predict F) — via
   `LoopbackFederationServer::spawn` (`loopback.rs:232`): send a small real PNG, receive
   `Staged{path}`, assert the file exists on disk with the expected sanitised name, then delete it.
4. `a_large_clipboard_frame_does_not_starve_terminal_output_delivery_forever` (predict F) —
   **documents A1**, does not fix it. Interleave a large stage frame with terminal output over the
   loopback duplex and assert the output still arrives, after the stage frame completes. Name the
   accepted stall in the test's own comment. Note `loopback.rs:246-249`'s duplex buffer is 1 MiB, so
   size the payload to exercise multi-read framing without needing 16 MiB.
5. `agreed_capabilities_survive_the_mount_into_the_mirror` (before step 8) — after
   `connect_and_mount` against a host advertising `FILE_STAGING`, `mirror.supports(FILE_STAGING)` is
   true; against one that does not, the mount still succeeds and `supports` is false.
6. `clipboard_stage_response_emits_an_app_event_carrying_origin_and_connection_epoch` (before
   step 12) — model on
   `drive_mount_channel_materializes_a_runtime_on_split_pane_created` (`client.rs:1696`).
7. `a_blocked_staging_operation_does_not_delay_connection_teardown` (before step 3) — inject a
   `StagingOp` that parks on a channel the test controls, send one stage request, then tear the
   connection down. Assert `run_connection` returns (and therefore `LeaseReleaseGuard` drops) while
   the operation is still parked, and that releasing it afterwards writes nothing to the socket.
   This is the controller-lease guard: without it a stalled filesystem call strands the old
   controller and blocks every subsequent mount to that host.
8. ★`a_host_without_the_agreed_file_staging_capability_never_emits_a_stage_frame` (before step 3) —
   drive `reader_loop` with `AgreedCaps` **not** containing `FILE_STAGING` and feed it a
   `ClipboardStageRequest`. Assert: no worker was spawned, the outbound queue stays empty (not even
   a `Failed` frame), the connection stays up, and a following `Terminal::Input` frame is still
   processed. This is the mixed-direction proof that a newer host cannot kill an older controller's
   mount — the serving half of P3.
10. `a_third_concurrent_stage_request_on_one_connection_is_refused_as_busy` (before step 3) — with an
   injected `StagingOp` parked on a test-controlled channel, send three stage requests. Assert the
   first two are admitted (both reach the op) and the **third** produces exactly one
   `Failed { Busy }` frame and never reaches the op — i.e. the bound counts the in-progress
   operation, not just the queue. Then release the parked op and assert a subsequent fourth request
   is admitted again (the permit is returned). A `sync_channel(2)`-only implementation admits the
   third and fails this test.
10b. `a_request_refused_by_a_full_staging_queue_still_returns_its_permit` — the permit-leak guard for
   step 3. Force the `try_send` error path (a worker queue whose receiver has been dropped, or a
   queue capacity of 0), send one request, assert it answers `Failed { Busy }`, then restore a live
   worker and assert two further requests are still admitted. An implementation that decrements only
   inside the worker loop leaks the permit and refuses them.
9. `a_malformed_base64_payload_is_reported_as_invalid_payload_without_touching_the_filesystem`
   (before step 4) — a request whose `payload_base64` is frame-legal but undecodable produces
   `Failed { InvalidPayload }`, creates no file in the staging dir, and does not end the connection.

## Risks and rollback

- **Risk (A1, accepted):** the reader still blocks on `read_exact` for the whole frame body
  (`federation_accept.rs:129-133`), so a large paste on a slow link freezes every pane on that mount
  for the transfer. Test 4 documents it. Not fixed in v1.
- **Risk:** the staging worker outlives the connection and writes after teardown. Accepted and
  bounded by design: the worker is detached, its outbound handle is revoked at teardown, and a late
  result is discarded (step 3). A detached worker thread can survive a stuck filesystem call, but it
  no longer holds the controller lease, the writer, or any socket handle — which is the property
  test 7 asserts. Joining it instead would reintroduce the lease-pinning bug.
- **Risk:** `serve.rs`'s duplicate handler drifts from `reader_loop`'s. Both call the same
  `file_staging` function; only the dispatch differs, and the comment in step 6 names production.
- **Risk:** raising `global_max_frame` raises the maximum single read-side allocation from 16 MiB to
  24 MiB per connection. Accepted — it is bounded, per-connection, and the frame is rejected by the
  channel-specific re-check immediately after decode
  (`federation_accept.rs:140-146`).
- **Rollback:** phases 01-03 together are still inert from the user's perspective (no capture
  trigger until 05). Reverting 03 alone leaves 01/02 unused but harmless.
