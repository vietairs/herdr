# Fix — post-mount pane mirroring (resync-on-structural-event)

## What changed, file by file

- `src/app/agents.rs` — `start_agent` now pushes `WorkspaceCreated` (new
  workspace) or `PaneCreated`+`LayoutUpdated` (split into an existing tab)
  after a successful spawn. Confirmed root-cause gap #1: `agent.start` never
  emitted these events at all (every other creation path already did).
- `src/remote/federation/protocol/mod.rs` — additive `SnapshotRequest`
  (empty struct) / `SnapshotResponse(MountSnapshot)` `FederationMessage`
  variants + channel routing (`Control`/`Mount`) + codec roundtrip test.
  `FEDERATION_PROTOCOL_VERSION` left at 3 (documented why: v3 has never
  shipped in a release).
- `src/server/federation_actor.rs` — new `FederationCommand::Snapshot`
  (read-only, no lease interaction); extracted `current_snapshot(app)` out
  of the existing `Mount` handler so both share the exact same
  `Method::SessionSnapshot` construction. One new unit test.
- `src/server/federation_accept.rs` — reader loop answers an inbound
  `SnapshotRequest` with a `SnapshotResponse` (`handle_snapshot_request`,
  mirrors the existing `handle_split_pane_request` shape). Threaded
  `server_instance_id` into `run_connection`/`reader_loop` (needed to tag
  the response). One new reader-loop test; updated two existing test call
  sites for the new parameter.
- `src/remote/federation/client.rs` — `drive_mount_channel` tracks one
  in-flight resync request; on an `Applied` structural `EventKind`
  (`PaneCreated`/`PaneClosed`/`PaneMoved`/`TabCreated`/`TabClosed`/
  `TabMoved`) sends a `SnapshotRequest` if none is pending (coalesces a
  burst into one); on the matching `SnapshotResponse` calls the existing
  (previously production-dead) `RemoteMirror::reconcile_by_diff`. New
  `is_structural_event_kind` helper. One new burst/coalesce/reconcile test.
- `plans/260713-1217-.../implementation-notes.md` — decisions + deviation
  logged.

## Validation

`ZIG=/Users/hvnguyen/.local/zig-0.15.2/zig cargo test --bin herdr --
--test-threads=4` → **2723 passed, 0 failed** (baseline 2717, net +6: two
`agent.start` event tests, one codec roundtrip, one server `Snapshot`
actor test, one `federation_accept` `SnapshotRequest` reader-loop test,
one client burst-coalescing + mirror-update test).

`cargo fmt --check` → clean.

`cargo clippy` → **not run**; fails locally on the pre-existing vendored
libghostty ReleaseFast/Zig `INFINITY` build error, unrelated to this
change (matches the known local-build limitation, not a new regression).

## Deviation (logged, not silently cut)

`reconcile_by_diff` correctly updates `RemoteMirror`'s own metadata
(`workspaces()`/`tabs()`/`panes()`) and pushes
`PaneCreated`/`PaneClosed`/`TabCreated`/`TabClosed` onto the local
`EventHub`. This fix does **not** additionally splice a newly-resynced
remote pane into the already-live mounting `App`'s real
`Tab`/`PaneRuntime` layout (spawn `TerminalRuntime::spawn_remote`,
register with `TerminalChannelRouter`, `Workspace::
insert_moved_pane_into_tab`), or tear down a closed one's local runtime.
That step needs `&mut App` inside the mount's drive task and — like the
existing `SplitPaneResponse::Created` path already does — would need a
new `AppEvent`/handler round-trip plus a stable remote-pane-id ->
local-`PaneId` reverse index this mirror-level fix does not carry.
Judged too large for one mid-tier pass (would roughly double this
change's size across `app/creation.rs`/`app/api/workspaces.rs`/
`events.rs`). A full unmount/remount already shows the correct state via
`materialize_federation_mount`. Net effect: post-fix, a live-mounted
session's mirror/sidebar-facing metadata is correct after a resync; the
already-open mount's live TUI pane layout is not auto-spliced until a
remount. Follow-up: `AppEvent::FederationResyncApplied` next to
`handle_federation_split_pane_ready` (`src/app/creation.rs`).

## Unresolved questions

- None blocking. The report's own open question (does `agent.start` push
  `PaneCreated`?) is now answered: no, prior to this fix — fixed here.

## Part 2 — live-mount layout splicing (this pass)

Closes the residual gap this report's own "Deviation" section logged: a
resync diff now actually splices into (or tears out of) the already-open
mount's live `Tab`/`PaneRuntime` layout, not just the mirror/sidebar
metadata.

- `src/remote/federation/reducer.rs` — `RemoteMirror::reconcile_by_diff`
  now returns `ReconcileDiff { created_panes, removed_pane_ids }`
  (`reconcile_panes` already computed this internally).
- `src/remote/federation/client.rs` — `drive_mount_channel`'s
  `SnapshotResponse` handling reads that diff: spawns a real local
  `TerminalRuntime` per created pane (new `materialize_resync_pane`
  helper, mirrors the existing `SplitPaneResponse::Created` arm) and
  emits `AppEvent::FederationResyncPaneCreated`/`FederationResyncPaneRemoved`.
- `src/events.rs` — the two new `#[cfg(unix)]` `AppEvent` variants.
- `src/app/creation.rs` — `handle_federation_resync_pane_created` splices
  the new pane into the mounted workspace's active tab (origin-checked
  against the workspace's `federation:<host_key>` binding, same
  discipline as `handle_federation_split_pane_ready`);
  `handle_federation_resync_pane_removed` tears the local pane/runtime
  down via a new reverse index (`App::remote_resync_pane_index`) —
  needed because the mirror's own namespaced pane ids are not, in
  general, the local `<workspace_id>:p<N>` public form.
- `src/app/mod.rs` — the new `remote_resync_pane_index` field.
- `src/app/actions.rs` — two mechanical no-op match arms (not in this
  task's owned files, touched only because `AppState::handle_app_event`'s
  match over `AppEvent` is exhaustive and would not compile otherwise;
  matches the pre-existing no-op-arm pattern already used for every other
  Federation* event there).

Deviation (conservative-minimal, explicitly sanctioned by the task): a
resync diff only reports created/removed panes, not a tab-level diff, so
`handle_federation_resync_pane_created` always targets the mounted
workspace's current active tab rather than reconstructing the remote's
exact tab placement.

Validation: `ZIG=/Users/hvnguyen/.local/zig-0.15.2/zig cargo test --bin
herdr -- --test-threads=4` → 2728 passed, 0 failed (net +5 over the 2723
baseline this pass started from). `cargo fmt` run clean. `cargo clippy`
not run (same pre-existing local libghostty ReleaseFast/Zig build issue
as part 1, unrelated to this change).
