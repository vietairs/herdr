# Review fixes — federation multi-tab/multi-workspace

Worktree `/Users/hvnguyen/Projects/herdr/.claude/worktrees/federation-multi-tab-workspace`,
branch `feat/federation-multi-tab-workspace`, base HEAD `870a4bfd`. Uncommitted (as instructed).

15 files, +1115/-56. `FEDERATION_PROTOCOL_VERSION` untouched at **6** — no wire-format change.
`src/protocol/wire.rs::PROTOCOL_VERSION` untouched.

---

## 1. CRITICAL — peer-controlled workspace label (C1 + M2)

**Changed** `src/server/federation_accept.rs::handle_workspace_create_request`: the peer's
`label` is now passed through `sanitize::sanitize_remote_string_opt` then
`protocol::clamp_workspace_label` on the first line of the handler, before the
`FederationCommand` is built — the same ingress-choke-point discipline `file_staging.rs:344`
uses for an inbound peer filename. Nothing downstream was touched, so the local user's own
`workspace.create` keeps taking labels verbatim.

**Bound** (M2): new `protocol::MAX_WORKSPACE_LABEL_CHARS = 128` + `clamp_workspace_label`,
applied on **both** sides — at accept ingress and in `dispatch_remote_workspace_create`
before framing. No pre-existing local workspace-label limit exists to inherit (grepped; the
nearest sibling is `app/agent_view.rs`'s unrelated 32-char `MAX_LABEL_CHARS`), so the
constant is explicit and documented against the 4 KiB `Channel::Control` ceiling.

**Answering the report's open question on M2 severity, empirically:** an oversized frame
**faults the whole link**. Writing a 4212-byte control frame made `reader_loop` return
`Err("federation frame size 4212 exceeds its channel's cap 4096")` — observed while
building the test below, which is why the sender clamps too, not only the receiver.

Test: `server::federation_accept::tests::reader_loop_neutralizes_a_peer_supplied_workspace_label_before_the_app_sees_it`
— drives a real `WorkspaceCreateRequest` frame carrying OSC 52 + `ESC[2J` + conceal SGR +
512 filler chars through `reader_loop`, and asserts on the `FederationCommand` the actor
receives (the last point still inside the host process) that no ESC/BEL/C0 byte survives,
that the visible text is preserved verbatim, and that the label is clamped to 128 chars.

## 2. HIGH — redirect inert under `prompt_new_workspace_name` (C2 / correctness #2)

**Root of the problem:** the name dialog asks for a *name*, never a directory, yet its
confirm sent the locally-resolved cwd — which the redirect (correctly) reads as "the caller
chose a local directory".

**How I distinguished auto-seeded from user-specified, and why.** I did *not* add a field to
`WorkspaceCreateParams` (that would put a TUI-internal fact on the public JSON API). Instead:

- `src/app/input/modal.rs::save_rename_modal_via_api` now decides at confirm time: if the
  pinned creation source is federation-owned it sends `cwd: None` (the dialog's captured
  path is a *serving-host* path and is not a user choice); otherwise it still sends the
  captured cwd. That preserves the existing tested guarantee that the dialog pins its
  directory against changes made while the user types
  (`new_workspace_key_opens_prefilled_prompt_and_preserves_captured_cwd`, which a blanket
  `cwd: None` broke).
- `AppState::pending_workspace_create_source_workspace` (new, TUI-local, `Workspace::id`)
  pins the creation source when the dialog opens; `App::workspace_creation_source()` prefers
  it. Without this, confirming re-resolved the source under `Mode::RenameWorkspace` and fell
  through to `active` — so a create started from a sidebar-*selected* remote workspace while
  a different workspace was active would have resolved the wrong source. Pinned by id, not
  index, because a resync can add/remove workspaces while the dialog is open.

The API-level gate stays `params.cwd.is_none()`, so an explicit `--cwd` from the CLI/JSON API
still creates locally.

**Label reachability:** the dialog's typed name now flows through as
`WorkspaceCreateRequest::label`, so the field is reachable from the TUI for the first time.

Tests:
- `named_workspace_create_inside_a_federated_workspace_goes_out_over_the_mount` (the
  regression test for the dead feature): `prompt_new_workspace_name = true`, mirrored
  workspace focused → a `WorkspaceCreateRequest` carrying the typed name reaches the mount
  and no local workspace is created.
- `named_workspace_create_outside_a_mount_still_creates_locally` (the fence).
- Corrected the false doc comment on
  `workspace_create_inside_a_federated_workspace_goes_out_over_the_mount`, which claimed to
  cover the rename-modal path.

## 3. HIGH — workspace lost when a resync is in flight (correctness #1)

`src/remote/federation/client.rs::drive_mount_channel`: added `resync_dirty: bool` beside
`resync_in_flight`. A structural event (`:586` region) or a `WorkspaceCreateResponse::Created`
(`:796` region) arriving during an outstanding snapshot now sets `dirty` instead of being
dropped; `SnapshotResponse` re-issues exactly one request when `dirty` is set (via
`std::mem::take`) instead of just clearing the flag. No queue.

Test: `remote::federation::client::tests::a_workspace_created_while_a_resync_is_in_flight_still_materializes`
— structural event → request #1 outstanding → `Created` arrives → the answer to #1 is a
*stale* (empty) snapshot → the client must re-request, and the workspace reaches the mirror.
The load-bearing assertion is `snapshot_requests == 2` (it was 1 before the fix); the harness
has no writer pump, so the script cannot gate its recovering snapshot on receiving the
request, and that is stated in the test.

Also updated the pre-existing
`a_burst_of_structural_frames_coalesces_into_one_snapshot_request_...` expectation from 1 to
2 with the reasoning: coalescing now means "one request in flight at a time, plus one
follow-up per burst", not "one request per burst". That extra snapshot is the price of not
losing a trigger, and it is idempotent (diffs to nothing).

## 4. MEDIUM — index purge before the origin fence (M1 / correctness #3)

- `handle_federation_resync_workspace_removed`: the `remove` moved below both fences. Added
  the previously-missing case: when the workspace is only *announced* (not materialized), the
  pending entry is retracted only by the mount that announced it.
- `handle_federation_resync_tab_removed`: reads with `get` first, removes only after the
  origin fence passes. When the tab's workspace is gone locally the entry is still pruned,
  but only if the id's namespace belongs to the reporting origin.
- Bonus (correctness #6, in the same lines): the entry is now also kept when the
  `tabs.len() <= 1` guard declines to close the last tab, so the still-live local tab keeps
  resolving instead of having later panes build a duplicate beside it.

Tests (strengthened, per the note that the old ones asserted only the workspace/tab):
- `resync_tab_removed_from_the_wrong_origin_is_dropped_and_keeps_the_index` — asserts the
  index entry is byte-for-byte unchanged after a refused removal.
- `resync_workspace_events_from_the_wrong_origin_are_dropped` — now also asserts a foreign
  removal does not evict the real mount's *pending announcement*, and does not purge the
  materialized workspace's tab-index entries.

## 5. MEDIUM — `tab.close` tearing down a whole mount (correctness #4)

`src/app/api/tabs.rs::handle_tab_close`: added the federation branch that `tab.create`
(`remote_tab_unsupported`) and `pane.close` already have. Closing the **last** tab of a
mirrored workspace now purges that workspace's federation bookkeeping and calls
`close_single_workspace_at` (the branch's own single teardown), emitting the same
`TabClosed` + `WorkspaceClosed` pair. The mount and its other mirrored workspaces stay
alive, and the wrong-for-federation `confirm_implicit_worktree_group_close` message is
skipped.

Supporting change: `close_single_workspace_at` is now ungated `pub(crate)` (the `tab.close`
caller is ungated, same rationale as `dispatch_remote_workspace_create`), and the six purge
helpers are grouped behind `purge_federation_state_for_workspaces`, which has a `#[cfg(unix)]`
body and a `#[cfg(not(unix))]` no-op so the ungated caller carries no `#[cfg]`.

Test: `closing_a_mirrored_workspaces_last_tab_leaves_the_mounts_other_workspaces_alive`.

**Not fixed (out of scope, flagged):** a *non-last* mirrored tab close is still not forwarded
to the remote — the local tab vanishes while the remote keeps running it. That needs either a
refusal or a new `TabCloseRequest` (protocol 7); both are behavior decisions beyond this
fix list.

## 6. Contract + hygiene

**H1 — honest response.** `workspace.create` targeting a mount now returns a **success**:
new `ResponseResult::WorkspaceCreateRequested { origin }`. I chose this over
`ResponseResult::Ok {}` because the closest precedent is exact:
`WorkspaceMountRemoteRequested` already answers "the async remote operation was requested,
not completed" and its doc comment says so. `Ok {}` would have been honest but would have
thrown away the "not done yet" signal.

**LOUD: this is a public JSON API contract change.** It is purely additive (one new
`ResponseResult` variant), but `herdr workspace create` inside a federated workspace now
exits 0 instead of 1, and API clients see `result` where they saw
`error.code == "remote_workspace_create_pending"`. That code no longer exists.
`docs/next/api/herdr-api.schema.json` was regenerated with
`HERDR_UPDATE_API_SCHEMA=1`. I did **not** touch `api/panes.rs`'s sibling
`remote_split_pending` / `remote_close_pending` — same smell, but outside this fix list.

**M3 — no host paths to the peer.** `federation_actor.rs`'s `CreateWorkspace` arm now replies
`"{code}: workspace could not be created on the remote host"` and logs the real API message
(usually a PTY-spawn/cwd `io::Error` naming a shell or home path) through `tracing::warn!` on
the serving host only. The machine-readable `code` is preserved, so the client's
classification format is unchanged.

## 7. UX — a client-requested workspace arrives focused

Correlation is by workspace id, not by a per-origin counter, so a race with an out-of-band
remote create cannot mis-attribute focus:

- `dispatch_remote_workspace_create` records `request_id` in
  `App::pending_remote_workspace_create_focus` **only when `params.focus` is true** (the same
  flag the local path passes to `create_workspace_with_launch_env`).
- The drive task, on `WorkspaceCreateResponse::Created`, emits the new
  `AppEvent::FederationWorkspaceCreateAccepted { request_id, origin, workspace_id }` with the
  id already namespaced via `id::map_in`. Only this client sends `WorkspaceCreateRequest` on
  a link, so a `Created` response always answers a local request; a workspace the remote user
  made only ever arrives as a structural event and produces no such event.
- `handle_federation_workspace_create_accepted` fences the id against the origin, redeems the
  request-id claim, and either focuses immediately (already materialized) or parks the id in
  `pending_remote_workspace_focus`.
- `materialize_resync_workspace_from_pane` focuses on arrival if the id is claimed.
- Claims are dropped on `WorkspaceCreateFailed` and purged with the workspace on teardown.

Test: `a_client_requested_remote_workspace_takes_focus_and_a_remote_originated_one_does_not`
— both halves in one test, plus `AppState::assert_invariants_for_test()`.

---

## Disagreements / deviations

None on the substance of the seven items. Two deliberate deviations in *method*:

1. **C2's suggested fix** was "skip the local-cwd prefill and pass `cwd: None` when the source
   is federated". Passing `cwd: None` unconditionally broke a real tested guarantee (the
   dialog pins its directory against a config change made while the user types), so the
   `cwd: None` is applied only on the federated branch, and the source is pinned by id.
2. **The burst-coalescing test's expectation changed from 1 to 2.** That is a deliberate,
   documented relaxation, not an accidental regression: preserving "exactly one request per
   burst" is precisely what strands the workspace.

Also fixed opportunistically, because they were inside lines I was already rewriting:
correctness findings **#6** (orphaned tab index entry on the last-tab skip) and the pending-
announcement half of **#3**. Correctness findings **#5, #7, #8, #9** are untouched — outside
the fix list, all latent/leak-class.

## Verification (verbatim)

```
$ cargo fmt --check
FMT CLEAN            (no output from fmt itself)

$ cargo clippy --all-targets
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.09s
                     (zero warnings, zero errors)

$ cargo test --bin herdr -- --test-threads=2
test result: ok. 3416 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 44.14s
```

At `--test-threads=4` the run was `3414 passed; 2 failed`, both being the flagged
load-sensitive flakes —
`api::server::pane_graphics_stream::tests::inactive_owner_cancels_idle_stream_and_dispatches_close`
and `server::headless::tests::split_default_background_response_updates_theme_without_forwarding_tail`
(the latter is a third instance of the same class, not one of the two named in the brief).
Both pass on re-run in isolation and in the `--test-threads=2` full run above.
Baseline was 3410; +6 net tests (7 added, 1 renamed).

## Unresolved questions

- Should the sibling `remote_split_pending` / `remote_close_pending` (`api/panes.rs:323`,
  `:413`) get the same success-ack treatment now that `workspace.create` diverged from them?
  They are now the only `*_pending`-as-error codes left.
- A non-last mirrored tab close still diverges silently from the remote (see item 5). Refuse
  it like `tab.create`, or add a `TabCloseRequest` at protocol 7?
- The new `WorkspaceCreateRequested` result is unreleased-behavior; does it warrant a
  `docs/next` entry, or does this branch's docs pass cover the whole feature at once?
```
