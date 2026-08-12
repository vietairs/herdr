# Phase 2 implementation — federation multi-workspace create

Worktree: `/Users/hvnguyen/Projects/herdr/.claude/worktrees/federation-multi-tab-workspace`
Branch: `feat/federation-multi-tab-workspace`, on top of Phase 1 (`591b979a`). Not committed.

## Status

Status: DONE_WITH_CONCERNS — code + tests green, clippy/fmt clean, but never
exercised against a live remote host, and one deviation from the brief (2c) was
taken deliberately to avoid double-materialization. Also fixed a latent
group-close bug the feature made reachable (see Incidental fix).

## FEDERATION_PROTOCOL_VERSION decision

Bumped **5 -> 6**.

- Latest release tag: `v0.8.0-hvn.2` (`git tag --list | tail -5`).
- `git show v0.8.0-hvn.2:src/remote/federation/protocol/mod.rs` — the released
  value is `5`. Source on this branch before the change was also `5`, i.e.
  **not already greater than the released value**, so CLAUDE.md's rule requires
  the bump rather than an in-place amendment.
- Category: two new top-level `FederationMessage` variants a v5 peer cannot
  decode — the same case as the `Fault` 1->2, `SplitPaneRequest` 2->3 and
  `ClosePaneRequest` 3->4 bumps, not an additive field. `codec::decode`
  hard-rejects a version mismatch from the header and `negotiate()` rejects the
  handshake, so there is no degrade path an unbumped addition could lean on.
  Rationale recorded in the constant's own doc comment.
- Hardcoded expectations: none. Every reference (`grep FEDERATION_PROTOCOL_VERSION`,
  25 hits across `server/federation_accept.rs`, `remote/federation/{serve,
  loopback,client}.rs`, `protocol/{mod,codec}.rs`) is symbolic — tests use
  `FEDERATION_PROTOCOL_VERSION ± 1`, never the literal `5`. `crate::protocol::
  wire::PROTOCOL_VERSION` (19) is a different protocol and was **not** touched.

## Files changed

| File | Change |
|---|---|
| `src/remote/federation/protocol/mod.rs` | `FEDERATION_PROTOCOL_VERSION` 5→6 + rationale. New `WorkspaceCreateRequest { request_id, label: Option<String> }` and `WorkspaceCreateResponse::{Created { request_id, workspace_id, tab_id, pane_id, terminal_id }, Failed { request_id, reason } }`, shaped exactly like `SplitPaneRequest`/`Response`. Both added to `FederationMessage` and to `channel()` → `Channel::Control`. New codec round-trip test. |
| `src/remote/federation/protocol/codec.rs` | New test: a frame stamped with the *previous* version is rejected as `VersionSkew` before the payload is touched. |
| `src/server/federation_actor.rs` | New `FederationCommand::CreateWorkspace { label, reply }` + `Debug` arm + `dispatch` arm servicing it through the serving host's own `Method::WorkspaceCreate` (no parallel creation path), unpacking `ResponseResult::WorkspaceCreated { workspace, tab, root_pane }` into the four raw ids. `cwd: None` (client paths are meaningless remotely), `focus: false` (never steal the serving host's focus). Failure replies `code: message`, same shape `ClosePane` uses. New test. |
| `src/server/federation_accept.rs` | `handle_workspace_create_request` — blocking round-trip mirroring `handle_split_pane_request` exactly; gone/dropped actor replies `Failed`, never drops the request. Reader-loop match arm. No rate limit / count cap (parity with the uncapped split/close handlers, per the recorded decision). Two new tests. |
| `src/remote/federation/reducer.rs` | `reconcile_workspaces` now returns `(Vec<WorkspaceInfo>, Vec<String>)`; `ReconcileDiff` gains `created_workspaces` + `removed_workspace_ids`, mirroring Phase 1's tab fields. |
| `src/events.rs` | New `#[cfg(unix)]` `AppEvent::FederationResyncWorkspaceCreated { origin, workspace_id, label }`, `FederationResyncWorkspaceRemoved { origin, workspace_id }`, `FederationWorkspaceCreateFailed { request_id, reason, origin }`. |
| `src/remote/federation/client.rs` | `drive_mount_channel` emits workspace-created **before** the tab loop and workspace-removed **after** the tab-closed loop. `WorkspaceCreateRequest` inbound → debug-ignore (client→server only). `WorkspaceCreateResponse::Created` → log + request a resync; `Failed` → `FederationWorkspaceCreateFailed`. `is_structural_event_kind` gains `WorkspaceCreated | WorkspaceClosed`. |
| `src/app/mod.rs` | New `App::remote_resync_workspace_index: HashMap<String, creation::RemoteWorkspaceRef>` + init. |
| `src/app/creation.rs` | New `RemoteWorkspaceRef { origin, label }`. `handle_federation_resync_workspace_created` / `_removed`, `handle_federation_workspace_create_failed`, `materialize_resync_workspace_from_pane`, `purge_remote_resync_workspace_index_for_workspaces`, `close_single_workspace_at`. `handle_federation_resync_pane_created` routes an unknown workspace id into the new materializer. `handle_federation_resync_tab_created` now also accepts a tab whose workspace is announced-but-not-yet-materialized. 6 new tests. |
| `src/app/api.rs` | Dispatch arms for the three new events. |
| `src/app/actions.rs` | Three `=> Vec::new()` exhaustiveness arms. |
| `src/app/api/workspaces.rs` | `handle_workspace_create` routes to `dispatch_remote_workspace_create` when the creation-source workspace is federation-owned and no explicit `cwd` was given. New `next_remote_workspace_create_request_id()`. Workspace-index purge added at both unmount/close sites that already purge the tab index. |

## Key-dispatch path (2d) — which one, and the proof

Wired at the **JSON-API handler** `App::handle_workspace_create`
(`src/app/api/workspaces.rs`), not at a key handler. Traced end to end:

```
route_client_events_from            (src/app/mod.rs:1905)          <- LIVE path
  -> handle_non_terminal_key_headless (src/app/mod.rs:2035)
    -> handle_navigate_key            (src/app/input/navigate.rs:127)
      -> execute_tui_navigate_action  (:183, NavigateAction::NewWorkspace at :191)
        -> begin_tui_workspace_create (src/app/creation.rs:111)
          -> runtime_workspace_create (src/app/runtime_mutations.rs:32)
            -> Method::WorkspaceCreate -> App::handle_workspace_create
```

The legacy monolithic `App::handle_key` (`src/app/input/mod.rs:81`) reaches
`handle_navigate_key` too (`:111`), and so do the mouse path
(`src/app/input/mod.rs:431`), `App::run`'s `request_new_workspace` drain
(`src/app/mod.rs:1131`), and the rename-modal named create
(`src/app/input/modal.rs:1065`). **All five converge on
`runtime_workspace_create` → `Method::WorkspaceCreate`.** Gating at the API
handler is therefore downstream of every dispatch path, live or dead, which is
strictly stronger than picking one — and it is the same place
`dispatch_remote_pane_split` gates the analogous remote split
(`src/app/api/panes.rs:120-200`). The end-to-end test
`workspace_create_inside_a_federated_workspace_goes_out_over_the_mount` drives
`handle_api_request_after_internal_events_drained` with
`Method::WorkspaceCreate` and asserts the `WorkspaceCreateRequest` frame really
left the mount.

**Fence:** routing only applies when `params.cwd.is_none()`. An explicit path is
a deliberate local-directory choice and the remote host's filesystem is a
different namespace; covered by
`workspace_create_with_an_explicit_cwd_stays_local_inside_a_federated_workspace`.
Response is `remote_workspace_create_pending` (fire-and-forget, same
sync/async constraint and same acknowledgment shape as `remote_split_pending`);
a dead link answers `remote_workspace_create_unsupported` and **never** falls
through to a silent local workspace.

## Deviation from the brief (2c)

**`WorkspaceCreateResponse::Created` does not materialize inline.** The brief
said to handle it "the same way `SplitPaneResponse::Created` is handled". It is
handled in the same place and with the same origin/ctx discipline, but instead
of building the workspace from the response it logs the ids and issues a
`SnapshotRequest`.

Reason: the serving host's `workspace.create` also emits `workspace.created` /
`tab.created` / `pane.created` on its event stream, which — now that
`is_structural_event_kind` covers `WorkspaceCreated` — triggers a resync whose
diff carries the same workspace, tab and pane. Materializing from the response
*as well* would create the workspace twice unless the mirror were
pre-registered for a workspace, a tab **and** a pane (the split path only needs
one `register_split_pane`). One materialization path is the correct design;
requesting the resync immediately makes it prompt rather than dependent on
event-frame timing, and it is idempotent (a snapshot whose entities the mirror
already holds diffs to nothing). `AppEvent::FederationResyncWorkspaceCreated`
exists and is used — emitted from `diff.created_workspaces`, which is the path
that also covers out-of-band remote creation.

Consequence: the `request_id` correlation is used only for the failure path and
for logging; there is no `pending_remote_workspace_creates` map, because
nothing local needs reversing when the remote refuses.

## Staged materialization (mirrors Phase 1's tab design)

A local `Workspace` needs a `Tab`, which needs a pane, so an empty workspace is
not representable. Emission order in `drive_mount_channel` is
**workspaces-created → tabs-created → panes-created → panes-removed →
tabs-closed → workspaces-removed**. The workspace and tab events only record
identity/label in `remote_resync_workspace_index` /
`remote_resync_tab_index`; the first pane event calls
`materialize_resync_workspace_from_pane`, which reuses exactly what
`materialize_federation_mount` uses (`Workspace::from_existing_pane`, the
mirror's namespaced id as `Workspace::id`, the `federation:<host_key>`
`worktree_space` membership, `emit_workspace_open_events`) and registers the new
tab/pane in both Phase 1's `remote_resync_tab_index` and the existing
`remote_resync_pane_index`.

Two origin fences: `handle_federation_resync_workspace_created` rejects an id
not namespaced under the reporting mount (`id::classify`), and
`materialize_resync_workspace_from_pane` re-checks the pane event's origin
against the mount that announced the workspace. Materialized workspaces keep
using Phase 1's `workspace_matches_federation_origin`.

## Incidental fix (in scope, not requested)

`AppState::close_selected_workspace` closes **every** workspace sharing the
selected one's `worktree_space` key (`close_indices_for`). Every workspace one
federation mount materializes shares that mount's single
`federation:<host_key>` key — so with N remote workspaces, retiring one (or its
last pane) would have taken all N down. Caught by
`resync_workspace_removed_prunes_the_workspace_and_its_index_entries`
(0 workspaces left instead of 1).

Fixed with `App::close_single_workspace_at(ws_idx)`, which detaches the doomed
workspace from its space (the membership dies with it either way) so
`close_indices_for` falls back to the single index. Applied at all three
federation single-workspace teardown sites:
`handle_federation_close_pane_ready`, `handle_federation_resync_pane_removed`,
`handle_federation_resync_workspace_removed`. `handle_federation_mount_ended`'s
deliberate whole-group teardown is untouched.

Latent before this phase (one mount reported one workspace in practice), live
now.

## Tests added (11)

`src/remote/federation/protocol/mod.rs`
- `workspace_create_request_response_roundtrip_through_the_wire_codec` — request
  (labelled + unlabelled), `Created`, `Failed`, all through the shared codec on
  `Channel::Control`.

`src/remote/federation/protocol/codec.rs`
- `decode_rejects_a_peer_on_the_previous_federation_protocol_version` —
  `VersionSkew { local: 6, remote: 5 }` from the header alone.

`src/server/federation_accept.rs`
- `reader_loop_routes_a_workspace_create_request_and_replies_created`
- `reader_loop_replies_failed_when_workspace_create_cannot_be_serviced` —
  dropped reply channel answers `Failed`, no panic.

`src/server/federation_actor.rs`
- `create_workspace_creates_a_real_workspace_and_replies_with_its_ids` — real
  `App` gains a workspace, four non-empty ids, the label hint lands, and
  `state.active` is unchanged (no focus theft).

`src/app/creation.rs` (`federation_materialization_tests`)
- `resync_workspace_created_materializes_a_second_federated_workspace` —
  announcement alone creates nothing; the pane event builds a second workspace
  with the namespaced id, `classify(&ws.id) == Remote(host_key)`, the
  `federation:<host>` `worktree_space`, both labels, and index entries;
  `Workspace::assert_invariants_for_test()` + `AppState::assert_invariants_for_test()`.
- `resync_workspace_removed_prunes_the_workspace_and_its_index_entries` — the
  other workspace survives; workspace/tab/pane index entries all pruned;
  invariants asserted.
- `resync_workspace_events_from_the_wrong_origin_are_dropped` — a foreign mount
  can neither announce under another mount's namespace nor close its workspace.
- `resync_workspace_handlers_leave_adversarial_identity_state_intact` — driven
  from `AppState::test_with_adversarial_identity_state()`; ghost announce,
  ghost removal and a non-namespaced id are all inert.
- `workspace_create_inside_a_federated_workspace_goes_out_over_the_mount`
- `workspace_create_with_an_explicit_cwd_stays_local_inside_a_federated_workspace`

## Verification (verbatim)

`just` / `cargo nextest` unavailable, as stated in the brief.
`export ZIG=$HOME/.local/zig-0.15.2/zig`

`cargo test --no-fail-fast -- --test-threads=4`:

```
test result: ok. 3410 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 35.01s
test result: ok. 11 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 22.91s
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 14 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 12.93s
test result: ok. 9 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 8.18s
test result: ok. 11 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 22.73s
---- live_handoff_keeps_unmanaged_agent_name_bound_to_saved_session stdout ----
thread 'live_handoff_keeps_unmanaged_agent_name_bound_to_saved_session' panicked at tests/live_handoff.rs:1306:9:
test result: FAILED. 19 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 20.83s
test result: ok. 11 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 28.97s
test result: ok. 15 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 9.05s
```

Totals: **3500 passed, 1 failed**. Bin target 3399 (Phase 1) → 3410 = the 11
new tests. The single failure is the pre-existing live-PTY agent-detection test
`live_handoff_keeps_unmanaged_agent_name_bound_to_saved_session`, already
identified as not-mine in the Phase 1 report and empirically confirmed there
against the clean base. No federation involvement.

`cargo fmt --check` → clean.
`cargo clippy --all-targets` → exit 0, **0 warnings, 0 errors**. Baseline held.

Flake note: two *different* pairs of tests failed on two earlier full runs
(`pane_graphics_stream::inactive_owner_cancels_idle_stream_and_dispatches_close`
+ `plugins::manifest_action_invoke_injects_plugin_paths`, then
`pane::pane_terminal_identity_*`). All four pass in isolation
(`--test-threads=1`); they are env-var/timing races under parallelism, unrelated
to this change.

## Not verified

- **No live federation run.** Nothing here was exercised against a real remote
  herdr host. The full loop — TUI new-workspace → `WorkspaceCreateRequest` over
  SSH → serving-host `workspace.create` → `workspace.created` event → resync →
  local materialization — is verified only by unit tests and compilation.
- **No cross-version live check.** A v5 peer meeting a v6 peer is asserted at
  the codec layer only; the handshake reject path was not run against two real
  binaries.
- **Windows.** `cargo clippy --all-targets` ran on macOS only. The new
  `AppEvent` variants and creation handlers are `#[cfg(unix)]`;
  `dispatch_remote_workspace_create`, `RemoteWorkspaceRef` and the
  `remote_resync_workspace_index` field are ungated, matching the existing
  `dispatch_remote_pane_split` / `RemoteTabRef` precedent, but Windows was not
  compiled.
- **Split geometry / focus.** A remotely created workspace materializes
  unfocused on the client (the resync path never focuses what it creates), so
  the user's "new workspace" press does not switch to the new workspace. Same
  as the pre-existing resync behaviour; see question 2.

## Unresolved questions

1. **Focus after a remote create.** A local "new workspace" focuses the result;
   the remote one does not (the workspace arrives asynchronously through
   resync, and nothing carries "the user asked for this one"). Wiring it would
   need the pending `request_id` to survive to materialization — i.e. the
   `pending_remote_workspace_creates` map this phase deliberately avoided.
   Product call.
2. **`focus: false` on the serving host.** Chosen so a remote request never
   yanks the serving host user's own focus. If the serving host is headless
   this is inert; if a human is using it, this is the safe default — but it is
   a policy choice, not a repo fact.
3. **`cwd.is_none()` as the local/remote fence.** Predictable, but it means
   "new workspace here" from the rename modal (which always supplies a cwd)
   stays local even inside a federated workspace. Acceptable? An explicit
   `remote: bool` on `WorkspaceCreateParams` would be the alternative, and that
   is a public JSON-API contract change I did not make unilaterally.
4. **No cap on remotely created workspaces.** Parity with the uncapped
   `SplitPaneRequest`/`ClosePaneRequest` handlers, per the recorded decision. A
   peer that cleared the handshake can create unbounded workspaces on the
   serving host.
5. Phase 1's open questions (remote `TabRenamed` propagation; last-tab removal
   assumption) are unchanged.
6. Live-test ownership: who runs a 2–5 workspace create against a real host
   (e.g. appn-ltu-vm-105) before this lands? **Both** ends must be rebuilt —
   the protocol bump makes a v5 server reject a v6 client outright.
