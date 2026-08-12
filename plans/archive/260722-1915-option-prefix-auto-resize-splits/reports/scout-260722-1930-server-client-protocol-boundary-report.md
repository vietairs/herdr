# Scout report — server/client protocol boundary for pane ops

Stage 2 (blindspot), scout 2 of 3. Read-only. Worktree:
`/Users/hvnguyen/Projects/herdr-worktrees/pane-auto-resize-splits`

## Findings

1. **PROTOCOL_VERSION = 17** (`src/protocol/wire.rs:16`). Bump rule quoted from `CLAUDE.md:185`:
   compare against the latest RELEASED tag; bump only if current source protocol is not
   already greater than the released one; update hardcoded protocol expectations + manual
   fixtures in tests.

2. **Pane split flow, local**: client `Method::PaneSplit` -> `src/app/api/panes.rs:45` ->
   `ws.split_pane()` (layout mutation + PTY spawn, `:121-132`) -> terminal registry insert
   (`:145-151`) -> emit `EventKind::PaneCreated` (`:154-157`) -> clients via
   `ServerMessage::Frame` (`wire.rs:599`).

   **Federated**: `src/app/api/panes.rs:182` `dispatch_remote_pane_split()` ->
   `FederationMessage::SplitPaneRequest` over mount link -> remote processes locally + emits
   `PaneCreated` -> client drive task triggers `SnapshotRequest` -> remote replies
   `SnapshotResponse(MountSnapshot)` (`src/remote/federation/protocol/mod.rs:363`) -> client
   reconciles diff into `RemoteMirror`.

3. **Events** (`src/api/schema/events.rs:192-218`): `PaneCreated`, `PaneClosed`, `PaneUpdated`,
   `PaneMoved`, `PaneFocused`, `LayoutUpdated`. Collected in `EventHub` (`src/app/mod.rs:103`).

4. **Federation mirroring (b5cb8ce8)** — resync is triggered ONLY by STRUCTURAL events:
   `PaneCreated`, `PaneClosed`, `PaneMoved`, `TabCreated`, `TabClosed`, `TabMoved`
   (`src/remote/federation/client.rs:46`). Mirrored facts: workspace/tab/pane structure,
   layout BSP tree, pane IDs, terminal state.

5. **Server-owned persistent boolean setting — NOT FOUND.** Searched
   `src/api/schema/{panes,tabs,workspaces}.rs`. Everything boolean there is ephemeral STATE
   (`focus`, `zoom_changed`, `focus_changed`, `focused`) or an intrinsic property
   (`is_linked_worktree`). **There is no precedent to copy for a persisted per-tab setting.**

6. **Session persistence** — `src/persist/snapshot.rs`, `SNAPSHOT_VERSION = 3` (`:12`).
   `TabSnapshot` (`:84-95`) holds layout (BSP), panes, zoomed, focused. A new per-tab bool
   needs: field added, `SNAPSHOT_VERSION` 3 -> 4, `#[serde(default)]` for back-compat,
   roundtrip test.

## CRITICAL implication — balance will not propagate to mounted remote clients

Balancing mutates only `Node::Split.ratio` values. A ratio change is **not** one of the
structural events in finding 4, so it will **not** trigger a federation `SnapshotRequest`.
Consequence: running "Balance splits" on a mounted remote workspace changes geometry on the
host but leaves mounted clients showing the OLD ratios until some unrelated structural event
forces a resync.

This is the same class of defect already recorded in this fork's memory as the remote pane
repaint gap (federation wire has no repaint/geometry-only request; panes go stale after
resize/split). The auto-resize toggle makes it worse, because it fires ratio changes
routinely rather than on explicit user action.

Options for the plan gate:
- (a) Scope v1 to LOCAL workspaces only; explicitly no-op or hide the items on remote panes.
- (b) Add a geometry/layout resync trigger to the federation wire (larger; touches
  PROTOCOL_VERSION and the federation protocol; overlaps the known repaint-gap work).
- (c) Reuse `LayoutUpdated` as a resync trigger in `federation/client.rs:46` — needs
  verification that `LayoutUpdated` is actually emitted on ratio-only changes (**scout did
  NOT confirm this; unverified**).

## Unresolved questions

- Is `EventKind::LayoutUpdated` emitted on ratio-only mutations (manual resize mode)? If yes,
  option (c) is cheap. If no, ratio changes are invisible to the event layer entirely.
- Does the toggle belong per-tab (needs SNAPSHOT_VERSION 4) or as a global config bool
  (no migration, no protocol change)? Global is dramatically cheaper.

Status: DONE
Summary: PROTOCOL_VERSION=17; no precedent exists for a server-owned persisted boolean;
per-tab persistence forces a snapshot migration; and ratio-only changes do not trigger
federation resync, so balancing is invisible to mounted remote clients.
Concerns: the federation propagation gap is a real correctness issue for this fork
specifically, not a theoretical one.
