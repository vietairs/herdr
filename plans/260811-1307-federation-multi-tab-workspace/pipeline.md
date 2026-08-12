# Federation: multi-tab + multi-workspace remote mounts

Task: (1) Fix remote workspace rendering N remote tabs as 1 local tab with N split panes.
(2) Extend federation so multiple remote workspaces can be opened from the same mount.

Worktree: `.claude/worktrees/federation-multi-tab-workspace` (branch `feat/federation-multi-tab-workspace`)

## Evidence

- Root cause: `plans/reports/root-cause-260811-federation-tab-collapse.md`
- Design scout: `plans/reports/scout-260811-multi-remote-workspace.md`

Confirmed root cause (client-side only, no wire change needed for the fix):
`PaneInfo.tab_id` exists on the wire, but `materialize_resync_pane`
(`src/remote/federation/client.rs:993-1002`) never copies it into
`FederationResyncPaneCreated` (`src/events.rs:346-365`, no `tab_id` field), so
`handle_federation_resync_pane_created` (`src/app/creation.rs:1316-1338`) always
splices the pane into `ws.active_tab` as a horizontal split. Mount-time
materialization (`materialize_federation_mount`) is correct; the collapse only
hits panes discovered by post-mount resync.

Secondary gap: no client handler for `EventKind::TabCreated`/`TabClosed`;
`ReconcileDiff` (`src/remote/federation/reducer.rs:397-399`) has no tab fields,
and `is_structural_event_kind` (`client.rs:1028-1039`) ignores `WorkspaceCreated`.

## Phases

- [ ] 1. Tab identity through resync — fix the collapse, add tab created/closed resync
- [ ] 2. Remote workspace/tab creation over the federation wire (FEDERATION_PROTOCOL_VERSION 5 -> 6)
- [ ] 3. Review + test + ship

## Constraints

- CLAUDE.md runtime/client boundary: shared runtime facts go in server state and
  the JSON API/event path; neutral server/API names, no UI-surface names.
- No `unwrap()` in production code. `tracing` for logging.
- Platform code stays in `src/platform/`.
- Build: `ZIG=~/.local/zig-0.15.2/zig cargo test -- --test-threads=4` (no just/nextest locally).
