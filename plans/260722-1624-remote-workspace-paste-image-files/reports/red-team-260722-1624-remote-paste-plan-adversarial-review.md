# Red-team adjudication — remote-workspace image paste plan

Three lenses (security, protocol/reliability, executability/TDD) reviewed the plan. Every defect was
re-verified against source before action. Verdicts below; fixes are already applied in place.

**Tally:** security 7 confirmed / 0 rejected. Protocol 8 confirmed / 0 rejected. Executability
11 confirmed / 2 rejected (1 declined as scope, 1 factually wrong).

---

## Confirmed — CRITICAL / HIGH

### C1. Returned-path guard stops at control bytes; shell metacharacters reach an unbracketed PTY
Verified `src/pane.rs:3076-3086` — `paste_payload` returns `text` unchanged when
`input_state().bracketed_paste` is false. `/tmp/a; rm -rf ~/.png` has no control byte and passes all
six original checks. **Fix:** phase 04 requirement 8 rewritten — ordered checks now add (5) the final
component must start with the `federation-clipboard-` prefix minted in phase 02 step 8, and (6) a
`[A-Za-z0-9._/@+-]`-plus-printable-non-ASCII allowlist over the whole string. New starred tests 11
and 12. plan.md acceptance criterion 4 updated to say control-byte rejection is insufficient and why.

### C2. Same hole at the source: the 8-step filename contract permits metacharacters
Nothing in steps 1-8 rejected `;`, `|`, `$`, space. **Fix:** phase 02 step 5b inserted — reject (not
strip) any stem byte outside the *same* allowlist, with the two-guard-agreement invariant stated on
both sides. New starred test 11 `stage_rejects_shell_metacharacters_in_original_filename`.

### C3. Stale-response guard order inverted vs. the precedent it copies
Verified `src/app/creation.rs:845-861` — `get()` → origin check → `return` → only then
`take_pending_remote_split`; and `src/app/api/panes.rs:34-40` — `request_id` is a bare process-wide
`AtomicU64`. Remove-first lets any second mount evict a legitimate pending entry.
**Fix:** phase 04 requirement 7 reordered to peek → origin → `server_instance_id` → remove, with the
reasoning inline; timeout handler explicitly keeps remove-first (locally originated, no foreign
claimant). Test added asserting a mismatched-origin response leaves the entry intact.

### C4. `original_filename` has no source in the capture path
Verified `src/platform/mod.rs:88-94` — `ClipboardImage { bytes: Vec<u8>, extension: &'static str }`;
`linux.rs:377-416` maps mime→extension with magic-byte validation, `macos.rs:628` hardcodes `"png"`.
No filename exists anywhere. **Fix:** phase 01 requirement 8 records the finding; phase 05 gets a new
step 2b synthesising `format!("image.{}", image.extension)`; the phase-02 contract is re-framed as
defence against a hostile *mounting client*. plan.md carries an explicit correction to the locked
"original filename preserved" wording — **this is the one item needing a human ack** (below).

### C5. Phase 03 could not compile: `AppEvent` has an exhaustive match outside every ownership table
Verified `src/app/actions.rs:2828-2841` — `AppState::apply_event` matches every `#[cfg(unix)]`
federation variant. `actions.rs` was in no phase's Files table. **Fix:** added to phase 03's table
plus new step 11b (three `=> Vec::new()` arms).

### C6. Staging-worker teardown would hang the connection join
Verified `output_pump` (`federation_accept.rs:741-769`) never blocks — it drains and sleeps, so it
can poll `shutdown`. A worker parked in `Receiver::recv()` cannot. `run_connection:405-428` joins
subordinates before `drop(out_tx)`. **Fix:** phase 03 step 3 now specifies
`drop(worker.tx); let _ = worker.handle.join();` with the pump joins, before `drop(out_tx)`, and the
shutdown-flag-inside-recv claim is removed from Risks.

### C7. Phase 03 step 2 targets a type that no longer exists
Verified: `AppFederationHost` appears only in doc comments (`federation_accept.rs:88`,
`headless.rs:307`). The loopback host is `FixtureHost` (`loopback.rs:33`, `capabilities()` `:139`,
default set `:53`). **Fix:** step 2 retargeted; loopback tests 3/4 would otherwise negotiate without
the capability.

### C8. `src/remote/federation/` is NOT inside a `#[cfg(unix)]` tree
Verified `src/remote.rs:4` (`pub mod federation;`, ungated — contrast `#[cfg(unix)] mod unix;` at
`:1-2`) and `src/main.rs:89`. Only `federation::session` is gated (`federation/mod.rs:74`).
`file_staging.rs` would compile for Windows, where `restrict_file_options` is a no-op stub
(`clipboard_image.rs:96-104`) — the 0600 guarantee silently vanishes. **Fix:** phase 02 requirement 8
rewritten to mandate `#[cfg(unix)] mod file_staging;` plus a gated module body, test 10 gated,
`windows-lint` re-baseline called out.

### C9. Phase 05 wires an ungated call into an ungated file
`src/app/input/mod.rs:74-86` has no cfg split; `begin_remote_clipboard_stage` is `#[cfg(unix)]`.
**Fix:** phase 05 step 1 now requires the whole intercept block be `#[cfg(unix)]`.

---

## Confirmed — MEDIUM / LOW

| # | Defect | Verification | Fix location |
|---|---|---|---|
| M1 | Backpressure reported as disk-full; `InvalidFilename` reported as "no image" | phase 03 step 3 vs phase 05 req 6 | `Busy` variant added (phase 01 req 6 + step 2); phase 03 responds `Failed{Busy}`; phase 05 req 6 gains distinct busy and rejected-file-name copy |
| M2 | Purge misses two mount-death paths | `api/workspaces.rs:320-333` (generation fence, reachable — `mount_generation` degenerate) and `:341-347` both `return` before the `:369` purge | phase 04 req 6: origin-keyed purge hoisted above both early returns; step 5 rewritten |
| M3 | `server_instance_id` inert for the case test 6 names | minted per remote *process*, arrives in `MountSnapshot` (`client.rs:190-206`) — a remount to a live remote reuses it | phase 04 test 6 renamed to the restart case; new test 6b proves `workspace_id` resolution catches the same-instance remount |
| M4 | No backpressure on the client send path | `client.rs:852` `mpsc::unbounded_channel` | in-flight cap lowered 4→2 with the ~58 MiB-per-stage arithmetic stated (phase 04 req 5, plan.md A1) |
| M5 | `Ord::max` is not `const` on stable; `max_len` is `pub const fn` (`protocol/mod.rs:331`) | — | phase 01 step 4 specifies explicit `if`/`match` |
| M6 | Wall-clock 10-15s timeout test; no paused-clock convention (`grep tokio::time::pause` → 0 hits) | — | phase 04 test 3 now drives `FederationClipboardStageTimedOut` through `handle_internal_event` |
| M7 | `TMPDIR` override unsound under nextest in-binary threads (`temp_dir()` is process-global) | `clipboard_image.rs:79` | phase 02 req 2 adds a `stage_into(dir, ..)` seam; Risks rewritten |
| M8 | Ownership not disjoint: phase 03 step 11 enumerated only 2 of 3 event variants | — | phase 03 step 11 lands all three now; phase 04's Files table rows changed to "does not touch" |
| M9 | Acceptance criterion 6 not checkable locally | plan.md Local build note | criterion 6 now names the VM as the discharge point and the local proxy's `windows-lint` gap |
| L1 | Quota measured over a dir shared with the local writer | `staging_dir()` `clipboard_image.rs:74-80`, `stage()` `:13-50` unquota'd | phase 02 req 4: sum scoped to `federation-clipboard-`-prefixed entries; sibling test assertion |
| L2 | Bridge topology reads the wrong machine's clipboard | no App-side predicate exists (`is_remote_client_process` is a client env check, `client/mod.rs:667-668`) | documented as an accepted limitation in phase 05 req 2 and plan.md Deferred — no guard invented (nothing to guard on) |
| L3 | `Channel` doc "The six federation channel classes" already stale at seven arms | `protocol/mod.rs:311` | phase 01 step 3 |
| L4 | `src/remote/mod.rs:12` does not exist | env var is `src/remote/unix.rs:29`, re-exported via `remote.rs:7`; `remote.rs:12` is the windows copy | phase 05 Context |
| L5 | Doc-comments framed as plan labels (forbidden by CLAUDE.md) | — | phase 01 step 2 and phase 03 step 11 restated as invariants |
| L6 | Step 5 anchors two insertions by line number in one file (first edit shifts the second) | `api/workspaces.rs:369`, `:650` | phase 04 step 5 anchors by sibling symbol |

Also folded in (protocol lens, non-defect): phase 03 Context now records that `serve.rs`'s
`global_max_frame` governs the **production** mounting client's read path (`client.rs` imports
`serve::read_frame`), not only loopback.

---

## Rejected

1. **"Phase 04 tests 1/2/10 assert on bytes reaching a pane runtime, and no such seam exists"**
   (exec MEDIUM) — **rejected.** The seam exists: `TerminalRuntime::test_with_channel(cols, rows) ->
   (Self, mpsc::Receiver<Bytes>)` at `src/terminal/runtime.rs:545`, already inserted into
   `App::terminal_runtimes` by existing tests at `src/app/api.rs:1449-1451`. The tests were kept as
   written; phase 04's Tests section now names the seam so the executor does not re-derive it.

2. **"Split phase 03 into 03a (remote) / 03b (client)"** (exec MEDIUM) — **declined as scope, not a
   defect.** File ownership is already disjoint, execution is strictly sequential, and the two halves
   share phase 01's types plus the capability contract that only makes sense reviewed together.
   Splitting adds a commit boundary and a partially-wired intermediate state for no verification
   gain. YAGNI.

---

## Residual risk accepted

- **A1 head-of-line blocking** unchanged; now with the memory consequence quantified (cap 2).
- **Same-process remount** on the *same* `server_instance_id` is caught only by workspace/pane
  resolution (test 6b), not by the fence. Accepted; the federation-wide fencing gap stays deferred.
- **No magic-byte validation on the remote** — "images only" is enforced client-side plus by
  extension allowlist. Recorded in plan.md Deferred.
- **Mount-inside-bridge** clipboard-host confusion, documented not guarded.
- **`windows-lint`** may already be red on this branch (`client.rs:704-712` references a
  `#[cfg(unix)]` `AppEvent` from an ungated file) — phase 02 requirement 8 now says re-baseline
  before trusting acceptance criterion 6. Not investigated further; plan-only task.

---

## Needs a human decision

1. **The locked phrase "Original filename preserved" cannot be delivered.** There is no filename in
   the OS clipboard path on either backend. The plan now synthesises `image.{extension}` and keeps
   the sanitisation contract as client-hostility defence. Confirm this reading of the lock, or
   redefine the requirement (e.g. accept that v1 names files `image.png` and drop the phrase).
2. **In-flight cap lowered 4 → 2** on memory grounds. This is a tuning decision inside a locked
   assumption (A1), not a reversal, but it is a user-visible concurrency limit — confirm.

---

## Cross-model (Codex) adversarial review — round 1

Seven findings received; **all seven confirmed at source**, none rejected. Plan files updated in
place; no Rust source touched.

### Confirmed — where each fix landed

1. **[high] Capability gate enforced only on the mounting client.** Verified:
   `federation_accept.rs:245` discards `drive_handshake`'s `AgreedCaps`
   (`if drive_handshake(..)?.is_none()`), and `drive_mount` (`:274`), `run_connection` (`:349`) and
   `reader_loop` (`:444`) take no capability argument. Fix: phase 03 requirement 6 rewritten to a
   two-sided gate; new step 2b threads `AgreedCaps` through; step 3 spawns the staging worker **only**
   when `FILE_STAGING` is agreed (structural — no worker, no response path); steps 5/6 drop an
   unnegotiated request with a log and no frame. New phase 03 test 8
   `a_host_without_the_agreed_file_staging_capability_never_emits_a_stage_frame`; plan.md acceptance
   criterion 3 and P3 row updated; manual-validation item 5b added for the real mixed-binary
   new-host/old-controller direction.

2. **[high] Stale mount-end purges a fresh same-host remount.** Verified: `MOUNT_GENERATION` is the
   constant `1` (`federation_accept.rs:66`, `serve.rs:44`); `AppEvent::FederationMountEnded`
   (`events.rs:177-182`) carries no per-connection identity; `workspaces.rs:320-336` fences on
   generation and then calls `end_federation_mount`. Fix: a locally minted `MountConnectionEpoch`
   (phase 03 requirement 7b, steps 7/7b/8), carried on the two stage events **and** on
   `FederationMountEnded` (step 11a). Phase 04 requirement 6 restructured: epoch fence **first**,
   then the existing generation fence, then an origin+epoch-keyed purge; guard step 3 now compares
   the epoch. New phase 04 test 6c
   `a_delayed_mount_ended_event_does_not_purge_or_end_a_fresh_remount`; tests 6/6b reframed onto the
   epoch. Manual-validation item 5c added.

3. **[high] Worker teardown pins the single-controller lease.** Verified: `LeaseReleaseGuard` lives
   in `drive_mount` and drops only after `run_connection` returns; `run_connection`'s teardown
   (`:405-428`) joins pumps/ticker/writer, and `writer_loop` (`:804`) blocks in `recv()` with no
   timeout. Fix: phase 03 step 3 replaces drop-then-join with a bounded teardown — the worker is
   **detached**, holds its outbound sender through a revocable
   `Arc<Mutex<Option<SyncSender<..>>>>` locked only at enqueue time, and late results are discarded.
   Step 4 adds a `StagingOp` indirection as the test seam; new test 7
   `a_blocked_staging_operation_does_not_delay_connection_teardown`. Risk bullet rewritten; the
   residual detached-thread leak recorded in plan.md Deferred.

4. **[high] Staged path rejected only after the remote write.** Verified: the staging dir resolves
   through `std::env::temp_dir()` (`clipboard_image.rs:74-80`), the existing code returns the path via
   `to_string_lossy` (`:38`), and phase 04 requirement 8.6 applies its allowlist to the whole
   returned path. Fix: new phase 02 requirement 4b (root **and** final path must be absolute,
   losslessly UTF-8, and allowlist-clean **before** any write; typed
   `ClipboardStageFailure::StagingUnavailable`, added in phase 01), new step 4
   `validate_staging_path`, reordered step 5, new tests 12/13/14; phase 04 requirement 8.6 now
   *consumes* phase 02's exported predicate instead of re-typing it. Manual-validation item 6b added.

5. **[medium] Malformed base64 had no defined failure path.** Verified: phase 01's enum had no such
   variant and phase 02 step 5 mapped only `io::Error`. Fix: `ClipboardStageFailure::InvalidPayload`
   added in phase 01 with an explicit "before any filesystem access" contract; phase 03 step 4
   decodes first and `continue`s on failure; phase 02 step 6 states decoding is not its job. New
   phase 03 test 9
   `a_malformed_base64_payload_is_reported_as_invalid_payload_without_touching_the_filesystem`;
   phase 05 requirement 6 maps it to its own toast copy.

6. **[medium] Old-peer UX contradicted the gate contract.** Verified: phase 05 step 1 said fall
   through, test 3 expected a "too old" toast on the same path. Fix: phase 05 requirement 1 now
   specifies three named branches (`FallThrough` / `Unsupported` / `Capture`) via a pure
   `remote_image_paste_decision`; `Unsupported` toasts **and consumes**. Tests 1/2/3 rewritten to
   those exact branches, including "no wire send, no PTY write" on `Unsupported`.

7. **[medium] Phase ownership and tests-first ordering not executable.** Verified: phase 05 declared
   `remote_clipboard_stage.rs` phase-04-owned yet told phase 05 to implement the delayed toast there,
   and its trigger test needed a real OS clipboard image. Fix: the delayed-toast affordance, the
   toast copy constants and the failure→copy mapping are now **phase 04's** (requirement 10, step 2,
   tests 10b/10c); phase 05 only references and asserts them. New phase 05 step 1b defines a narrow
   post-capture seam `App::handle_remote_image_paste(ws_idx, pane, image)` in phase-05-owned
   `input/mod.rs`, and new test 1b drives the success branch with a constructed `ClipboardImage` —
   no OS clipboard, no global injection.

### Rejected

None.

### Residual risk accepted

- The detached staging worker can leak one thread if a filesystem call never returns. Chosen over
  lease-pinning; watchdog deferred (plan.md Deferred).
- The epoch fence on `FederationMountEnded` changes behavior for **all** consumers of that event, not
  just this feature — a stale end-notice can no longer tear down a live remount. Strict improvement,
  but outside this feature's minimal footprint; flagged for the ship gate.
- Federation-wide fencing (`SplitPaneResponse` on the degenerate `mount_generation`) stays unfixed,
  per A2.
- Two narrow cross-phase file carve-outs now exist (`actions.rs` and the `FederationMountEnded`
  construction sites in `app/api/workspaces.rs`, both phase 03) so the tree compiles at each phase
  boundary. Regions are disjoint and phases are strictly sequential.

### Needs a human decision (round 1)

3. **A2's mechanism changed** from `server_instance_id` to a locally minted per-mount
   `MountConnectionEpoch`, because `server_instance_id` is per remote *process* and cannot see a
   remount to a still-running remote. Recorded in plan.md Assumptions as a mechanism revision inside
   A2, not a reversal — confirm that reading.
4. **The epoch fence widens `FederationMountEnded` semantics** for the split-pane path too (item 2 of
   Residual risk). Accept as a side benefit, or ask for it to be narrowed to the clipboard purge only.

## Cross-model (Codex) adversarial review — round 2

Verdict: needs-attention. 6 findings raised, **6 confirmed, 0 rejected**. All applied in the plan.

### Confirmed and fixed

1. **[high] Final staging-path validation still after the write (regression, round-1 item 4 half
   applied).** Verified: phase-02 step 5 ordered `write_all` → "final-path validation", and
   requirement 4b bullet 3 said "before `Ok` is returned". Fix: requirement 4b now mandates that
   **each candidate final path is composed and validated inside the `create_new` loop, immediately
   before the `OpenOptions` call**, with post-write validation explicitly banned; step 5 reordered;
   new starred phase-02 test 12b `a_rejected_candidate_final_path_leaves_no_file_on_disk` (test 12
   only ever covered the root). plan.md gains acceptance criterion 7 and the P2 coverage row cites
   the new test.

2. **[high] `file_staging` private to `remote::federation`.** Verified: phase-02 requirement 8 said
   `#[cfg(unix)] mod file_staging;` while phase 03 step 4 (`server::federation_accept`) and phase 04
   requirement 8.6 (`app::remote_clipboard_stage`) must name items in it; `pub` inside a private
   module is unreachable. Fix: new phase-02 requirement 9 — `#[cfg(unix)] pub(crate) mod
   file_staging;` with an explicit `pub(crate)` surface (`stage_remote_clipboard_image`,
   `stage_into`, the shared path predicate, the `federation-clipboard-` prefix constant), everything
   else private; `mod.rs` Files row updated; phase-04 requirement 8.5 now imports the prefix constant
   instead of retyping the literal.

3. **[high] Epoch contract not threaded through existing dispatch.** Verified at source:
   `src/app/api.rs:161-166` destructures `FederationMountEnded` exhaustively without `..`, and
   `app/api/workspaces.rs:313-319` takes four parameters. Also confirmed phase-04 requirement 4
   constructed `FederationClipboardStageTimedOut { request_id, origin }` without the epoch its
   phase-03 variant requires. Fix: phase 03 step 11a now owns all three broken sites — the
   `workspaces.rs` construction literals, the `api.rs` destructure/forward, and the handler
   **signature** (consumed by a placeholder `debug!` so the build stays warning-clean); phase 04 step
   5 replaces that placeholder with the fence and no longer claims the signature. Phase-03 and
   phase-04 Files tables and plan.md's carve-out paragraph updated so ownership stays disjoint.
   Phase-04 requirement 4 and step 4 now carry `connection_epoch` on construction and dispatch.

4. **[high] Returned-path rejection tests vacuous at the injection boundary.** Verified: phase-04
   tests 1/2/11 called only `sanitize_returned_remote_path` then asserted an empty pane receiver —
   necessarily empty, since no handler ran. Fix: a shared handler fixture is now mandated (real
   pending entry, `TerminalRuntime::test_with_channel` in `App::terminal_runtimes`, event driven
   through `App::handle_internal_event`), asserting no bytes on **any** runtime plus a toast, and an
   **injecting positive control**. Tests 1, 2, 11 and 12 rewritten onto it (12 promoted to starred).
   plan.md's P1 row records the requirement.

5. **[medium] Two-request cap not enforced (server) and not tested (client).** Verified: phase-03
   step 3 relied on `sync_channel(2)`, which admits two queued plus one in-progress; phase-04 test 9
   only required the fifth request to fail, which passes against the discarded cap of 4. Fix: phase
   03 step 3 now specifies an explicit **two-permit admission counter covering the in-progress
   operation**, with `sync_channel(2)` as backing only; step 5 takes a permit before `try_send`; new
   phase-03 test 10 `a_third_concurrent_stage_request_on_one_connection_is_refused_as_busy`
   (including permit return). Phase-04 test 9 renamed to
   `a_third_concurrent_stage_request_is_rejected_locally_at_the_in_flight_cap` with wire-send
   assertions on the two accepted requests. plan.md gains acceptance criterion 8.

6. **[medium] Phase-05 success test asserts state phase 04 does not store.** Verified:
   `PendingClipboardStage` (phase-04 requirement 2) holds workspace/pane/origin/epoch/payload
   length/deadline; `original_filename` lives on `ClipboardStageRequest` (phase 01). Fix: phase-05
   test 1b now asserts the filename on the outbound `ClipboardStageRequest` receiver, with an
   explicit instruction not to widen the pending struct for test convenience.

### Rejected

None.

### Regression sweep — other round-1 fixes checked for partial application

Round-1 items 1, 3, 5 and 6 are fully applied (two-sided gate: phase-03 req 6 + step 2b + test 8 +
plan criterion 3 + manual item 5b; detached worker: step 3/4 + test 7 + Deferred bullet;
`InvalidPayload`: phase-01 variant + phase-02 step 6 + phase-03 test 9 + phase-05 mapping;
three-branch UX: phase-05 req 1 + tests 1/2/3). Two were partial and are the same two the round-2
findings name: item 4 (validate-before-write applied to the root only → finding 1) and item 2
(epoch applied to the event and handlers but not to the existing dispatch/construction chain, nor to
the timeout event phase 04 constructs → finding 3). One knock-on of item 7's ownership move was also
partial — phase 05 gained test 1b but written against phase-04 state that the same fix had deliberately
kept minimal (finding 6). No further partial applications found.

### Vacuity audit — phase 04 and phase 05 tests

Beyond findings 4 and 6, the same "passes against an implementation that skips the guard" pattern was
found and fixed in four more tests:

- phase-04 test 5 (purge): now asserts entries exist before teardown and includes a **survivor**
  entry (different workspace / different epoch), so a purge-everything implementation fails.
- phase-04 test 6 (restarted remote): now uses the real handler fixture, pairs with a matching-epoch
  positive control, and asserts the pending entry **survives** the dropped response (guard steps 2-3
  peek, not remove).
- phase-04 test 7 (foreign `HostKey`): now asserts the entry is left intact and that the legitimate
  response delivered afterwards still injects.
- phase-05 tests 3 and 6: both "no wire send" assertions now require a positive control on the
  identical fixture (capability present / under-limit image), otherwise they are satisfied by a
  fixture that could never send.

Tests judged sound as written: phase-04 3, 4, 6b, 6c, 8, 10, 10b, 10c; phase-05 1, 2, 4, 5.

### Needs a human decision (round 2)

5. **Phase 03 briefly consumes the new `connection_epoch` parameter in a `debug!` line** so phase 03
   compiles warning-clean before phase 04 lands the fence. That is a deliberate two-step edit to one
   function across two phases. Confirm this over the alternative — folding the mount-ended epoch
   fence itself into phase 03, which would move a phase-04 behavior change earlier.

---

## Cross-model (Codex) adversarial review — round 3

Three new findings. Two confirmed and applied, one rejected. Three sibling sweeps run; two new
defects found and applied.

### Confirmed

1. **[high] `FallThrough` specified to discard the key.** Confirmed: `handle_key`
   (`src/app/input/mod.rs:74-117`) dispatches `match self.state.mode` at `:87`, and the intercept is
   specified to sit above it (phase-05 "Where the new gate goes"), so a `return` on the default arm
   swallows local terminal keys **and** every non-terminal mode handler (`:92-115`). Fixed in
   phase-05 requirement 1 (FallThrough now explicitly non-returning) and step 1, which now mandates
   the `if let Unsupported/Capture = … { …; return; }` shape mirroring `:75-78`/`:80-85`. Test 2 now
   drives the real `handle_key` with a live `TerminalRuntime::test_with_channel` receiver instead of
   asserting the decision value; new test 2b covers the non-terminal-mode half. plan.md gains
   acceptance criterion 9 and the P9 row cites both tests.

2. **[high] Write failure leaves a partial staged file.** Confirmed: `create_new`
   (`clipboard_image.rs:32`) is exclusive on *creation* only, and `:39` `file.write_all(data)?`
   propagates without removal — the exact shape phase 02 said to reuse. Fixed by new phase-02
   requirement 6b (all-or-clean write: remove the just-created candidate on any write/flush failure,
   `?` banned between `create_new` and the successful return, cleanup failure logged via
   `tracing::warn!`), step 5 (adds `flush` and an injectable-writer seam), new test 12c
   `a_failed_write_leaves_no_partial_file_on_disk` with a positive control. plan.md acceptance
   criterion 7 extended, P2 row updated, manual-validation item 6c added (real ENOSPC on a tiny
   tmpfs).

### Rejected

3. **[medium] Shared staging directory unreachable from the new module.** Rejected. Verification
   source: `ensure_staging_dir()` (`src/server/clipboard_image.rs:82-94`) **returns the resolved
   `PathBuf`** (`Ok(dir)` at `:93`), and it is already in phase-02's widen list (Files table row for
   `clipboard_image.rs`) and already the first call in step 5. No `staging_dir()` accessor is needed;
   the private helper's name and euid rule are never re-derived. Phase-02 requirement 7 now records
   this explicitly so the point is not re-raised.

### Sweep (a) — other "intercept returns early and swallows the normal path" sites

Audited every input/event interception the plan specifies. Clean:
- `app/api.rs` dispatch arms (phase 04 step 4): the existing arms at `:155-198` are a flat chain of
  `if let … { handle; return; }` inside a dispatcher whose entire body is that chain — `return` is
  correct there, and the new arms copy the same shape.
- phase-03 step 5 `reader_loop`'s stage arm uses `continue` inside the read loop, not an early
  function return; the following `Terminal::Input` frame is still processed (test 2 asserts it).
- phase-04 requirement 6's epoch fence and requirement 7's guard steps return deliberately, dropping
  only the event they claim.
- phase-05 step 1b `handle_remote_image_paste` always reports consumed — correct, it is only reached
  from `Capture`.
Nothing further applied.

### Sweep (b) — other partial-failure / half-updated-state paths

One new defect, applied:
- **Serving-side admission permit leaks on `try_send` failure** (phase 03 step 3). The permit was
  specified as acquired by `reader_loop` before `try_send` and released *only by the worker*, so a
  `try_send` returning `Err` (queue full, or worker gone during teardown) leaks a permit
  permanently and the connection answers `Busy` for the rest of its life with nothing staging. Fixed:
  the permit is now an RAII guard **moved into the queued request**, so the `try_send` error path
  (which returns the request, and the guard, to the caller) and a dropped queue both release it; a
  bare `fetch_add`/`fetch_sub` with the decrement in the worker loop is explicitly rejected. New
  phase-03 test 10b `a_request_refused_by_a_full_staging_queue_still_returns_its_permit`. plan.md
  acceptance criterion 8 extended.
Checked and clean: phase-04 `begin_remote_clipboard_stage` sends before registering (a failed send
leaves no pending entry); the stage-ready handler removes before resolving the pane, so a missing
pane cannot leave a half-claimed entry; phase-03's revocable outbound handle discards a late response
without partial writes; phase-02's quota runs after `cleanup_stale` and before any create.

### Sweep (c) — cross-module reachability of every call the plan specifies

One new defect, applied:
- **`file_staging` is Unix-gated but two of its three call sites are not.** Verified:
  `src/server/mod.rs:8` declares `pub(crate) mod federation_accept;` and
  `src/remote/federation/mod.rs:46` declares `pub(crate) mod serve;`, both unconditionally, and
  `serve.rs` contains no `cfg(unix)` at all — while phase-02 requirement 8 gates `file_staging` to
  Unix. Phase-03 steps 4/5 (`reader_loop` + worker) and step 6 (`serve.rs::handle_inbound`) would
  therefore be unresolved symbols under `just windows-lint` (justfile:26). Fixed by new phase-03
  requirement 3b: the `FILE_STAGING` advertisement in `federation_capabilities()` and `FixtureHost`,
  the worker, its spawn, and both request arms are `#[cfg(unix)]`; on Windows the capability is never
  advertised, never agreed, and the inbound frame takes the existing "not agreed → log and drop"
  path, so no stub is invented. plan.md acceptance criterion 6 now names phase 03 too.
- **`MountConnectionEpoch` visibility was unspecified** while the type crosses into `src/events.rs`,
  `app/api.rs`, `app/api/workspaces.rs` and `app/remote_clipboard_stage.rs` — none a descendant of
  `remote::federation::client`. Phase-03 step 7b now requires `pub(crate)` (and *no* `cfg(unix)` on
  the type itself, since `client.rs`/`reducer.rs` compile on Windows and only the `AppEvent` variants
  are gated). `federation/mod.rs:55` already declares `pub(crate) mod client;`, so the path resolves.
Checked and clean: phase-04's imports of `is_injection_safe_path` and the `federation-clipboard-`
constant are covered by phase-02 requirement 9 (`pub(crate) mod file_staging` + `pub(crate)` items),
and both consumer files are `#[cfg(unix)]`. Phase-04's `mod remote_clipboard_stage;` being private is
fine: inherent `impl App` methods resolve through the type regardless of module path, and phase-05's
copy tests live in `app::input`, a descendant of `app`, so the private module's items are visible to
them. `ClipboardStageFailure` sits under `pub mod protocol` (`federation/mod.rs:40`).

### Needs a human decision (round 3)

None. All three sweeps' fixes are mechanical consequences of already-locked decisions (A1's cap, A4's
shared staging dir, A3's ctrl+v preservation).

## Cross-model (Codex) adversarial review — round 4 (final gate)

Verdict: needs-attention, **1 medium finding**, down from 7 / 6 / 3 in rounds 1-3. Round 4 explicitly
confirms the round-3 and sweep contracts are now explicit in the plan: the `FallThrough` no-op
intercept, all-or-clean writes, RAII admission permits, unix cfg-gating, and epoch visibility.

**CONFIRMED (medium) — local PTY enqueue failure unhandled after a successful remote stage.**
Verified at source: `try_send_paste` returns `Result<(), mpsc::error::TrySendError<Bytes>>`
(`src/terminal/runtime.rs:466`, `src/pane.rs:3072`), and `TerminalRuntime::test_with_channel_capacity`
exists (`src/terminal/runtime.rs:550`) so both `Full` and `Closed` are drivable in a test.

Why it matters more than its severity suggests: this is the only boundary where the *remote* side has
already succeeded. The file exists on the remote host and the pending entry has already been claimed
and removed, so no timeout and no later response can surface the failure. Discarding the `Err` yields
a silent success — a staged image whose path no agent ever learns, holding quota until the sweep.

Fix landed: phase-04 step 11 (renumbered from a misordered step 9) now mandates matching the result,
a failure toast on both arms, and a `warn!` carrying the pane id and error kind, because the remote
artifact outlives the session and the log is its only trace. New starred test 12b covers both arms
against the shared handler fixture, paired with the existing positive control so "a toast appeared"
cannot be confused with a rejection.

**Review loop closed here.** Trajectory 7 → 6 → 3 → 1 with the final finding medium and its fix
self-contained; a fifth round was not judged worth its cost. Totals across four rounds: 17 findings
applied, 1 rejected on verified grounds (`ensure_staging_dir` already returns the resolved `PathBuf`),
plus 3 defects found by the post-round-3 sweeps that no review round named.
