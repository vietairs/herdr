# Phase 4 — server API balance + auto-rebalance hooks — implementation report

Worktree: `/Users/hvnguyen/Projects/herdr-worktrees/pane-auto-resize-splits`

## Implemented

- `src/api/schema.rs`: `Method::LayoutBalance(LayoutExportParams)`, `#[serde(rename = "layout.balance")]`,
  inserted after `LayoutSetSplitRatio` (D8: reused params verbatim).
- `src/api/schema/response.rs`: `ResponseResult::LayoutBalance { layout: LayoutDescription }`.
- `src/app/api/layouts.rs`: `App::handle_layout_balance(&mut self, id: String, params: LayoutExportParams) -> String`.
  Resolves target via `resolve_layout_export_target`; if `tab.zoomed`, returns success with the
  layout unchanged (D6, not an error); else calls `tab.layout.balance_areas()`,
  `schedule_session_save()`, `emit_layout_updated_event()`, `encode_success`.
- `src/app/api/panes.rs`:
  - `handle_pane_split` (hook ~line 152-176, before `schedule_session_save`): if
    `auto_resize_splits && !target_was_zoomed`, computes `tab.layout.path_to_pane(new_pane.pane_id)`
    and calls `balance_areas_along_path`. **Deviation from phase spec**: zoom must be captured
    *before* the split call (new `target_was_zoomed` local, ~line 98), not read from `tab.zoomed`
    after — `split_focused_with_runtime` (`src/workspace/tab.rs`) unconditionally sets
    `self.zoomed = false` as a side effect of every split, so a post-split read always observes
    `false` and the zoom guard would silently never fire. Caught by the
    `pane_split_auto_rebalance_is_noop_while_tab_zoomed` test (red before the fix: root ratio
    rebalanced to 0.6 despite the tab being zoomed).
  - `close_pane` (hook ~line 1697-1712 capture, ~1735-1746 apply): path + pre-close `tab.zoomed`
    captured via `layout_update_target_after_pane_removal`'s `(ws_idx, tab_idx)` before
    `ws.close_pane`; rebalance applied only in the non-workspace-closing `else` branch, before
    `schedule_session_save`.
  - `handle_pane_resize` untouched — no hook added (confirmed by test).
- Dispatch registration `src/app/api.rs:1101` (1 line) — required for the method to route at all.
- **Deviation (outside owned file list, logged per instructions)**: this single-binary crate has
  no `[lib]` target, so exhaustive `Method` matches elsewhere block ALL test compilation until
  patched (same class of issue implementation-notes.md already documents for phase 2→5).
  Added minimal match arms for `Method::LayoutBalance`:
  - `src/api/server.rs:397` — `"layout.balance"` metric/log name (mirrors `LayoutSetSplitRatio`).
  - `src/api/mod.rs:188` — added to the forbidden bucket in `federated_session_allows` (mutates
    persisted layout state, same bucket as `LayoutSetSplitRatio`/`PaneResize`).
  Both are mechanical, non-conflicting, one-line/one-arm additions; no design decision made.
- `docs/next/api/herdr-api.schema.json` regenerated via
  `HERDR_UPDATE_API_SCHEMA=1 ZIG=~/.local/zig-0.15.2/zig cargo test generated_protocol_schema_artifact_is_current`
  (32-line additive diff, test green).
- `docs/next/website/src/content/docs/socket-api.mdx`: added `layout.balance` to the method table
  and a paragraph + example mirroring `layout.set_split_ratio`'s format, including the D6 zoom
  no-op and D4/D9 federation-gap notes.

## Tests added

`src/app/api/layouts.rs::tests`: `layout_balance_equalizes_leaf_weighted_ratios`,
`layout_balance_is_noop_while_tab_zoomed`,
`layout_balance_emits_layout_updated_event_and_schedules_session_save`.

`src/app/api/panes.rs::tests`: `pane_split_auto_rebalance_touches_only_ancestor_chain`,
`pane_split_without_auto_resize_leaves_existing_ratios_untouched`,
`pane_split_auto_rebalance_is_noop_while_tab_zoomed`,
`pane_close_auto_rebalance_touches_only_collapsed_ancestor_chain`,
`pane_close_without_auto_resize_leaves_root_ratio_untouched`,
`pane_close_auto_rebalance_is_noop_while_tab_zoomed`,
`pane_close_that_closes_the_workspace_never_rebalances`,
`pane_resize_never_rebalances_regardless_of_toggle`.

All built on a shared 4-leaf fixture (`(root|a2)` vs sibling `(b|b2)`, each split given a
distinguishable manual ratio) so sibling-subtree preservation is asserted by exact value, not
just "didn't crash".

## Verification

- `ZIG=~/.local/zig-0.15.2/zig cargo check --tests` → clean, 0 errors.
- `cargo test app::api:: -- --test-threads=4`: 216 passed, 1 failed
  (`app::api::plugins::tests::manifest_action_invoke_injects_plugin_paths` — unrelated area,
  flaky under contention, see below).
- `HERDR_UPDATE_API_SCHEMA=1 ... cargo test generated_protocol_schema_artifact_is_current` → ok.
- Full suite `ZIG=~/.local/zig-0.15.2/zig cargo test -- --test-threads=4`: **2992 passed, 2
  failed**. Failing test names differ run-to-run: seen
  `api::server::pane_graphics_stream::tests::inactive_owner_cancels_idle_stream_and_dispatches_close`
  (timeout), `workspace::tests::generated_workspace_ids_are_short_base32_handles` (id collision),
  and `app::api::plugins::tests::manifest_action_invoke_injects_plugin_paths` across repeated
  runs — none touch layout/panes/schema/response code, matches the
  implementation-notes.md-documented pre-existing socket-timeout/random-id-collision contention
  pattern (that entry cites 4/2975; this run is 2/2992 after my added tests — controller should
  reconcile exact baseline count against a clean tree, but no failure names implicate this
  phase's files).
- `cargo test --no-fail-fast`: all 9 integration test binaries (api_ping, client_mode, cross_area,
  detach_reattach, live_handoff, multi_client, server_headless, etc.) pass 100%.
- `cargo fmt --check` on all touched files → clean after running `cargo fmt` once on owned files.

## PROTOCOL_VERSION decision: no bump

`src/protocol/wire.rs::PROTOCOL_VERSION = 17`, confirmed identical at `v0.7.5` (latest released
tag: `git show v0.7.5:src/protocol/wire.rs | grep PROTOCOL_VERSION` → `17`). Per its own doc
comment ("Bumped when wire format changes incompatibly"), this constant governs the internal
client↔server frame/handshake wire protocol (`RenderEncoding`, handshake structs) in
`src/protocol/wire.rs` — a distinct layer from the JSON-RPC `Method` enum in `src/api/schema.rs`
(the external socket API). This change touches only `src/api/schema.rs`;
`src/protocol/wire.rs` is untouched by this phase, and git history shows no PROTOCOL_VERSION
bump correlates with ordinary new `Method` variant additions (dozens exist unbumped). No bump
required.

## Unresolved questions

None blocking. Note for controller: a large (518-line) uncommitted diff in `src/layout.rs`
(Phase 1's own file, not touched by any of my Edit/Write calls) was observed mid-session with a
timestamp coincident with an unrelated concurrent process in the shared worktree — flagging for
awareness only, not something this report's changes caused or should be blamed for.

Status: DONE
Summary: `layout.balance` (one-shot, whole-tab-root) and ancestor-chain auto-rebalance hooks on
`pane.split`/`pane.close` are implemented, zoom-guarded, sibling-subtree-safe, and toggle-off
byte-identical to pre-feature behavior; schema artifact regenerated and green; PROTOCOL_VERSION
bump not required (evidence above).
Concerns/Blockers: Two files outside the strict owned list (`src/api/server.rs`, `src/api/mod.rs`)
needed one-line/one-arm additions purely to keep the single-binary crate compiling for the new
exhaustive `Method` variant — same class of unavoidable cross-file coupling
implementation-notes.md already documents for other phases; flagging per protocol rather than
silently absorbing.
