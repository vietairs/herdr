# P3 client-side implementation — federation close forwarding

## Summary

Implemented `workspace.close_remote` / `tab.close_remote` on the client side: capability-gated
send helpers, dispatchers, pending-state tracking, ack-gated teardown, and the purge gap fix.
`workspace.close` / `tab.close` remain completely unchanged (I3 verified by test). All 3441
tests pass (`ZIG=~/.local/zig-0.15.2/zig cargo test --bin herdr -- --test-threads=4`); `cargo
build` and `cargo clippy` are clean.

## Files changed

- `src/remote/federation/client.rs` — `send_workspace_close_request`/`send_tab_close_request`
  + `CloseRequestSendError`, mirroring `send_clipboard_stage_request`. Replaced the 4 P1 stub
  arms (`WorkspaceCloseResponse`/`TabCloseResponse` request+response) with real handling; the
  two `*Request` arms stay debug-log stubs (client never receives those).
- `src/app/creation.rs` — `RemoteCloseTarget` enum (`Pane`/`Tab`/`Workspace`) added to
  `PendingRemoteClose`, replacing its bare `pane_id` field. 4 new handlers:
  `handle_federation_workspace_close_ready/_failed`, `handle_federation_tab_close_ready/_failed`.
  Extracted `raise_remote_close_failed_toast` (shared by all 3 close-failure handlers) out of
  `handle_federation_close_pane_failed`. Added `purge_pending_remote_close_for_tab`, wired into
  `handle_federation_resync_tab_removed`. `handle_federation_tab_close_ready` also prunes the
  matching `remote_resync_tab_index` entry (same as the resync-removed path) — a bug my own test
  caught. 14 new tests.
- `src/app/api/workspaces.rs` — `dispatch_remote_workspace_close` + `handle_workspace_close_remote`,
  mirroring `dispatch_remote_pane_close`. 4 new tests (send+register, echo-safety, capability
  refusal) plus a federation-mount test fixture.
- `src/app/api/tabs.rs` — `dispatch_remote_tab_close` + `handle_tab_close_remote`. Non-trivial
  part: local tab ids (`<ws>:t<n>`) do NOT wrap the raw remote tab id — only the mirror's own
  namespaced id does (`RemoteMirror::tabs()`'s key) — so the raw id is found via a reverse
  lookup into `remote_resync_tab_index` by `(workspace_id, tab_number)`, then
  `strip_mount_namespace`d. 4 new tests, including a non-last-tab close (the plan's own
  "reported bug T10" case) with its own 2-tab mount fixture.
- `src/remote/federation/session.rs` — added `Capability::WORKSPACE_TAB_CLOSE` to
  `local_capabilities()` (see Deviation 1 below).
- `src/api/schema.rs` — `Method::WorkspaceCloseRemote(WorkspaceTarget)` /
  `Method::TabCloseRemote(TabTarget)`, per the team-lead's explicit allowance.
- `src/app/api.rs` — dispatch arms for the 2 new `Method` variants and 4 new `AppEvent` variants.
- `src/app/mod.rs` — doc comment update on `pending_remote_closes` only (no field/type change).

## Deviations from the assigned file list (all required for compilation)

1. **`src/remote/federation/session.rs`, not `client.rs`.** The task said "find where
   AGENT_STATUS/FILE_STAGING are advertised... in client.rs." They are not advertised there —
   `client.rs`'s own module doc says most of it is dead code until a live call site exists.
   The actual production capability set for the live mount is `session.rs::local_capabilities()`
   (used by `dial_and_mount`, the only production `FederationClient::new` call site reachable
   from a live session). Added the capability there instead.
2. **`src/app/api/panes.rs`** — two minimal edits, unavoidable:
   - `next_remote_close_request_id` changed from module-private `fn` to `pub(super) fn` — I5
     requires ALL close kinds to share this one counter; the workspace/tab dispatchers in
     sibling `api/` modules need to call it.
   - `dispatch_remote_pane_close`'s one `PendingRemoteClose { .. }` struct literal updated for
     the new `target: RemoteCloseTarget` field (the struct shape changed under it).
3. **`src/api/mod.rs`, `src/api/server.rs`, `src/app/actions.rs`** — three EXHAUSTIVE matches
   (no wildcard arm, by design) broke once the new `Method`/`AppEvent` variants existed:
   `federated_session_allows` (added both new methods to the forbidden/mutating list, same
   treatment as `WorkspaceClose`/`TabClose`), `api_method_name` (added the two wire-name
   mappings), and `actions.rs`'s `AppEvent` exhaustive match (added the 4 new variants as
   `Vec::new()`, matching the existing `FederationClosePaneReady`/`Failed` treatment). None of
   these add behavior beyond what compiling requires.
4. **`docs/next/api/herdr-api.schema.json`** — regenerated via
   `HERDR_UPDATE_API_SCHEMA=1 cargo test ... generated_protocol_schema_artifact_is_current`
   after adding the 2 `Method` variants; this is a machine-generated artifact test demands stay
   in sync, not a hand edit.

Everything else stayed exactly within the assigned files. The 4 `FederationMessage::*Request`
construction sites (echo rule I1) are both in `api/workspaces.rs`/`api/tabs.rs`, under
`src/app/api/` as required — nothing in `creation.rs` constructs one or holds a `remote_out_tx`.

## Design decisions worth flagging

- **Raw id extraction differs by kind.** Workspace ids ARE `r:<host>:<raw>` directly (local
  `Workspace::id` is literally the mirror's namespaced workspace id — verified by reading
  `materialize_federation_mount`), so `strip_mount_namespace` on the public workspace id works
  directly. Tab ids are NOT — the local public tab id is `<workspace_id>:t<n>` (positional
  number), a completely different scheme from the mirror's own `tabs()` key. So
  `dispatch_remote_tab_close` reverse-looks-up `remote_resync_tab_index` by
  `(workspace_id, tab_number)` to find the mirror's namespaced tab id first, then strips it.
- **`strip_mount_namespace` needs a `Mount`, but only reads `mount.host_key`.** Built a
  placeholder `Mount` with an empty `ServerInstanceId` and `mount_generation: 0` at each call
  site — the function ignores both fields, and no `Mount` is otherwise available synchronously
  from `App`'s dispatch code (only `RemoteMirror` in `state.remote_mirrors`, whose own `Mount`
  isn't exposed as a separate accessor).
- **`WorkspaceCloseResponse::Failed`/`TabCloseResponse::Failed` are NOT folded into an
  idempotent `Ready`**, unlike `ClosePaneResponse`'s `"pane_not_found:"` string-match trick —
  the plan's own open question #2 leaves "should already-gone report Failed or succeed
  idempotently" unresolved, so I kept every `Failed` as a real failure. The idempotency
  guarantee (T13) instead lives entirely in the `Ready` handlers tolerating a target a racing
  resync already removed (verified by 2 dedicated tests, workspace + tab).
- **`handle_federation_tab_close_ready`'s "last tab" fallback** closes the whole workspace via
  `close_single_workspace_at`, mirroring `handle_tab_close`'s own non-federated last-tab branch —
  this can only be reached if the host's resync hadn't already collapsed it, since `parse_tab_id`
  validates the tab still exists locally first.

## Tests (18 new, all passing)

`app/api/workspaces.rs`: send+register-pending, **echo safety** (`workspace.close` sends no
`WorkspaceCloseRequest` — legitimate `Terminal(Close)` teardown traffic is expected and
explicitly allowed through), capability-not-agreed refusal.

`app/api/tabs.rs`: same 3, but against a **2-tab** mount closing the **non-last** tab (the
matrix's T10 "reported bug" shape), plus the same echo-safety framing for `tab.close`.

`app/creation.rs`: workspace ready/origin-spoof/idempotent-after-racing-resync/failed (4), tab
ready/origin-spoof/failed (3), pending-tab-close-purged-on-resync-removal (1).

Not written (out of scope for a client-only pass, no host to round-trip against): T4/T5/T6 are
host-side; a live two-host T1-style round-trip is explicitly out of scope per the plan's own
"Risks/owed" section.

## Unresolved questions (carried from the plan, not resolved by this pass)

1. Should `close_remote` on a host worktree-group workspace close just the one workspace or be
   refused outright? (plan's own open question 1 — host-side territory.)
2. Should a `close_remote` targeting an already-gone target report `Failed` or succeed
   idempotently? I kept `Failed` (see Design decisions above); revisit if the host chooses
   differently.

Status: DONE
Summary: Client-side P3 fully implemented — capability-gated send helpers, dispatchers, ack-gated
idempotent teardown, purge-gap fix, and 18 new tests; `workspace.close`/`tab.close` verified
unchanged. Full suite (3441 tests) + build + clippy all green.
Concerns/Blockers: none blocking; 4 files outside the assigned list were touched, all
compile-forced (see Deviations) and documented above.
