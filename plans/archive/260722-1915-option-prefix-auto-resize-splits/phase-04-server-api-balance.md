# Phase 4 — server API one-shot balance + auto-rebalance hooks

Status: pending | Owns: `src/api/schema.rs`, `src/api/schema/panes.rs`,
`src/api/schema/response.rs`, `src/app/api/layouts.rs`, `src/app/api/panes.rs`,
`docs/next/api/herdr-api.schema.json` (generated), `docs/next/website/src/content/docs/socket-api.mdx`
Depends on: Phase 1 (`balance_areas`/`balance_areas_along_path`/`path_to_pane`), Phase 2
(`AppState.auto_resize_splits` field) | Parallel group: B

## Context

- Precedent handler to mirror: `handle_layout_set_split_ratio` (`src/app/api/layouts.rs:
  218-249`) — validates, resolves target via `resolve_layout_export_target` (`:251+`, accepts
  `LayoutExportParams { tab_id, pane_id }`), mutates via `tab.layout.set_ratio_at(...)`,
  `self.schedule_session_save()` (`:243`), `self.emit_layout_updated_event(ws_idx, tab_idx)`
  (`:247`), `encode_success(id, ResponseResult::LayoutSplitRatioSet { layout })` (`:248`).
- `Method` enum, `layout.*` variants at `src/api/schema.rs:142-147`
  (`#[serde(rename = "layout.set_split_ratio")] LayoutSetSplitRatio(LayoutSetSplitRatioParams),`
  is the last one — insert new variant directly after).
- `LayoutExportParams { tab_id: Option<String>, pane_id: Option<String> }`
  (`src/api/schema/panes.rs:113-119`) — shape matches exactly what the one-shot balance action
  needs (D3: whole tab root, target by tab or pane). Reuse this struct verbatim for the new
  method's params (DRY; flagged as unresolved question #2 in `plan.md` if reviewers disagree).
- `ResponseResult` variants incl. `LayoutApply`, `LayoutSplitRatioSet` — `src/api/schema/
  response.rs:152,155` — add a new variant here, response shape `{ layout: LayoutDescription }`
  matching the existing two.
- Zoom guard (D6/H3): `Tab.zoomed: bool` (`src/workspace/tab.rs:48`), NOT on `TileLayout` — must
  check `tab.zoomed` (production tabs) at the call site before invoking any balance fn; skip
  (no-op, still return success) when zoomed.
- Split hook point: `handle_pane_split` (`src/app/api/panes.rs:40-161`). Local-spawn result
  lands at `:134` (`let (target_tab_idx, new_pane) = ...`). Remote-federation early return is
  `:80-85` (`dispatch_remote_pane_split`) — strictly BEFORE the hook insertion point, so the
  hook only ever runs on the local path (D4 satisfied structurally, not by an extra check).
  Insert the auto-rebalance call after `:138` (state fully updated: pane inserted, focus/tab
  switched) and before `:152` (`self.schedule_session_save()`), so a rebalanced ratio is what
  gets persisted.
- Close hook point: `close_pane` (`src/app/api/panes.rs:1671-1733`). The path to the target pane
  must be captured via `path_to_pane` BEFORE `ws.close_pane(pane_id)` runs (`:1695`) — after
  that call the pane and its former ancestor chain no longer exist in the tree in the same
  shape. Only rebalance in the `else` branch (`:1716-1729`, pane closed without closing the
  whole workspace) — the `should_close_workspace` branch (`:1698-1715`) tears down the
  workspace, no layout left to rebalance.
- Gate: both hooks only run `if self.state.auto_resize_splits { ... }`.

## Requirements

1. New `Method::LayoutBalance(LayoutExportParams)`, `#[serde(rename = "layout.balance")]`, in
   `src/api/schema.rs` after `LayoutSetSplitRatio`.
2. New `ResponseResult::LayoutBalance { layout: LayoutDescription }` in
   `src/api/schema/response.rs`.
3. New `App::handle_layout_balance(&mut self, id: String, params: LayoutExportParams) -> String`
   in `src/app/api/layouts.rs`, mirroring `handle_layout_set_split_ratio` structurally: resolve
   target via `resolve_layout_export_target`, if `tab.zoomed` return success with unchanged
   layout (D6 no-op, NOT an error — zoom is a normal transient state, not a failure), else call
   `tab.layout.balance_areas()`, `schedule_session_save`, `emit_layout_updated_event`,
   `encode_success`.
4. Register `Method::LayoutBalance` dispatch in `src/app/api.rs` next to
   `Method::LayoutSetSplitRatio` (`:1099-1100` area — this file is a thin dispatch match, single
   line addition, listed here since it's required for the method to work even though it's not
   in this phase's primary file-ownership list; if touching it creates a conflict with another
   in-flight phase, coordinate — it's a 1-line, non-conflicting addition in practice).
5. Auto-rebalance hook in `handle_pane_split`: after new pane exists (`:138`), if
   `self.state.auto_resize_splits` and `!tab.zoomed`, compute
   `tab.layout.path_to_pane(new_pane.pane_id)` and call
   `tab.layout.balance_areas_along_path(&path)`.
6. Auto-rebalance hook in `close_pane`'s local (non-workspace-closing) branch: capture
   `path_to_pane(pane_id)` BEFORE `ws.close_pane(pane_id)` if `self.state.auto_resize_splits`
   and pre-close `!tab.zoomed`; after successful close, call `balance_areas_along_path` with the
   captured path on the (now-collapsed) tree.
7. Federation gap doc comment at both hook sites: one line noting mounted remote workspaces
   don't participate in v1 auto-rebalance (D4), pointing at the same local-path guarantee
   already provided by the remote early-return / `src/remote/federation/` boundary.
8. Regenerate `docs/next/api/herdr-api.schema.json`:
   `HERDR_UPDATE_API_SCHEMA=1 ZIG=~/.local/zig-0.15.2/zig cargo test
   generated_protocol_schema_artifact_is_current -- --test-threads=4` (test at
   `src/api/schema/tests.rs:156-169`). Add a `layout.balance` entry to
   `docs/next/website/src/content/docs/socket-api.mdx`, mirroring the existing
   `layout.set_split_ratio` entry's format.

## Files to modify

`src/api/schema.rs`, `src/api/schema/panes.rs` (only if a dedicated params struct is chosen over
reusing `LayoutExportParams` — prefer NOT touching this file, see Requirement 1), `src/api/
schema/response.rs`, `src/app/api/layouts.rs`, `src/app/api/panes.rs`, `src/app/api.rs`
(1-line dispatch registration), `docs/next/api/herdr-api.schema.json`, `docs/next/website/src/
content/docs/socket-api.mdx`.

## Step-by-step (TDD)

1. Add failing test for `handle_layout_balance` in `src/app/api/layouts.rs` test mod: build a
   4-pane tab with manual unequal ratios, call the handler, assert `layout_description`'s ratios
   match the leaf-weighted expectation; add a zoomed-tab variant asserting no-op.
2. Add failing tests for the split hook in `src/app/api/panes.rs` test mod: with
   `auto_resize_splits = true`, split a pane, assert only the new pane's ancestor chain changed
   (reuse Phase 1's sibling-preservation property at the integration level); with
   `auto_resize_splits = false`, assert byte-identical ratios to current behavior (regression
   guard, run FIRST against current code before the hook exists — should trivially pass, keep
   it as a permanent regression test).
3. Add failing tests for the close hook: same on/off pairing, plus a test asserting the
   `should_close_workspace` branch never calls balance (workspace-closing path untouched).
4. Add failing test for the zoom no-op on BOTH split and close hooks (H3): zoom a tab, toggle on,
   split/close, assert layout ratios are what they'd be WITHOUT the toggle (confirms the guard,
   not just "doesn't crash").
5. Implement Requirements 1-7. Run
   `ZIG=~/.local/zig-0.15.2/zig cargo test --lib app::api:: -- --test-threads=4`.
6. Regenerate schema artifact (Requirement 8), commit the diff, update the mdx.
7. Full suite: `ZIG=~/.local/zig-0.15.2/zig cargo test -- --test-threads=4`.

## Risks / rollback

- Risk (High, mitigated): forgetting the zoom guard on the close hook silently corrupts ratios
  seen post-unzoom (H3) — mitigated by step 4's explicit test, required before merge.
- Risk: `path_to_pane` captured pre-close references a path that Phase 1's
  `balance_areas_along_path` must tolerate being "too long" post-collapse — already covered by
  Phase 1's own test (`balance_areas_along_path_tolerates_path_longer_than_tree`); this phase's
  integration test re-proves it at the handler level, not re-deriving the math.
- Rollback: revert this file set; Phases 1-3 remain inert (no caller invokes their new code),
  zero behavior change for existing users.
