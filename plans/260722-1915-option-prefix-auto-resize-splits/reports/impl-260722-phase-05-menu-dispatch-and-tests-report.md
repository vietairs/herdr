# Phase 5 implementation report — menu dispatch and tests

## Files touched (owned)

- `src/app/input/modal.rs`
- `src/app/input/mouse.rs`

## What was implemented

### `src/app/input/modal.rs`

- `apply_context_menu_action_via_api` (production): added two match arms after the "Close pane"
  arm (order doesn't affect match semantics):
  - `Some("Balance splits")`: resolves `public_tab_id(ws_idx, tab_idx)`, calls
    `self.dispatch_runtime_mutation("tui.layout.balance", Method::LayoutBalance(LayoutExportParams
    { tab_id: Some(tab_id), pane_id: None }))`, sets `state.mode = Mode::Terminal`.
  - `Some("Auto-resize splits: On" | "Auto-resize splits: Off")`: calls
    `self.save_auto_resize_splits(!self.state.auto_resize_splits)`, then `leave_modal(&mut
    self.state)` — matches the Collapse/Expand toggle control-flow precedent.
- `apply_context_menu_action` (`#[cfg(test)]`-only, `&mut AppState`): mirrored both arms without
  API plumbing — balance calls `tab.layout.balance_areas()` guarded by `!tab.zoomed`, followed by
  `state.mark_session_dirty()`; toggle flips `state.auto_resize_splits` directly (no persistence
  possible without `App`, matches this fn's existing no-persistence pattern for other arms).
- Test helpers added: `ratio_at(node, path)` (recursive path-based ratio reader, mirrors
  `layout::set_ratio_at`'s `false`=first/`true`=second convention),
  `app_with_unequal_three_leaf_tab()` (3-leaf fixture, ported from the equivalent fixture in
  `src/app/api/layouts.rs` tests), `pane_menu`/`pane_menu_with_auto_resize` (menu-literal builder).
- 6 new tests: `context_menu_balance_splits_equalizes_tab_via_api`,
  `context_menu_balance_splits_noop_when_zoomed_via_api`,
  `context_menu_toggle_auto_resize_flips_state_and_persists_via_api` (uses the existing
  `config_env_lock`/`temp_config_path` harness, round-trips both directions On<->Off and asserts
  the written `[ui]` section + `state.auto_resize_splits`), `context_menu_balance_splits_equalizes_tab`,
  `context_menu_balance_splits_noop_when_zoomed`, `context_menu_toggle_auto_resize_flips_state`
  (test-only-fn parity for the first three).

### `src/app/input/mouse.rs`

- `mouse.rs:1102` (`auto_resize_enabled: self.auto_resize_splits`) already present from the
  controller's out-of-phase patch (2026-07-22 20:05 note) — verified correct, no change needed.
- Added 1 test: `pane_right_click_menu_reflects_live_auto_resize_state` — sets
  `app.state.auto_resize_splits = true` before the right-click, asserts the constructed
  `ContextMenuKind::Pane.auto_resize_enabled == true` and `items()` contains
  `"Auto-resize splits: On"`.

## Balance-splits routing: shared, not duplicated

Routed through the exact same production path as `handle_layout_balance`
(`src/app/api/layouts.rs`) via the pre-existing `dispatch_runtime_mutation` +
`Method::LayoutBalance` machinery in `src/app/runtime_mutations.rs`/`src/app/api.rs` (both already
landed by Phase 4). This is the **same idiom** every other menu arm in this file already uses for
API-shaped actions (`zoom_focused_pane_via_api`, `set_split_ratio_via_api`, `runtime_pane_swap`,
etc.) — no new wrapper function, no visibility change, no file outside `modal.rs`/`mouse.rs`
touched. The phase file's speculative "split into a private fn" concern did not apply:
`handle_layout_balance` is `pub(super)` in `app::api` (not reachable from `app::input`), but the
`Method::LayoutBalance` variant + `dispatch_runtime_mutation` (`pub(crate)`) were already wired by
Phase 4, so no visibility widening or restructuring was needed — a straight `dispatch_runtime_mutation`
call is the idiomatic, zero-duplication path.

## Deviations

None. Zero files touched outside `modal.rs`/`mouse.rs`; no visibility changes to
`layouts.rs`/`runtime_mutations.rs` were required (contrary to the phase file's speculation that
this might be needed).

## Validation

Narrow (`cargo test context_menu`): 25 passed, 0 failed.
Narrow (`cargo test auto_resize`): 10 passed, 0 failed (includes both new tests + Phase 1-4 tests).
Narrow (`cargo test pane_right_click`): 2 passed, 0 failed.
`cargo fmt -- --check src/app/input/modal.rs src/app/input/mouse.rs`: clean after one `cargo fmt`
pass (3 auto-formatting fixups to my own new code, no behavior change).

Full suite (`ZIG=~/.local/zig-0.15.2/zig cargo test -- --test-threads=4`), run twice:
- Run 1: 2998 passed, 3 failed (`app::api::plugins::tests::manifest_action_invoke_injects_plugin_paths`,
  `server::headless::tests::delayed_active_tab_unfocused_agent_notification_forwards_after_deadline`
  [`AddrInUse`], `workspace::tests::generated_workspace_ids_are_short_base32_handles`).
- Run 2: 2997 passed, 4 failed (different set: `manifest_action_invoke_injects_plugin_paths`,
  `server::autodetect::tests::is_server_listening_returns_false_for_stale_socket`,
  `server::headless::tests::clipboard_write_failed_foreground_send_does_not_show_feedback`
  [`AddrInUse`], `generated_workspace_ids_are_short_base32_handles`).

Failing set differs run-to-run and none reference `layout.rs`/`modal.rs`/`mouse.rs`/`panes.rs`/
context-menu code — consistent with the documented pre-existing parallel-contention flake baseline
(socket `AddrInUse`, workspace-id-length randomness, plugin-path tmpdir race). Not touched/fixed
per instructions.

## Acceptance criteria check

- Both menu items work end to end from a right-click: covered by
  `pane_right_click_menu_reflects_live_auto_resize_state` (menu construction) +
  `context_menu_balance_splits_equalizes_tab_via_api` / toggle test (dispatch).
- Toggle direction correct, persists across reload: `context_menu_toggle_auto_resize_flips_state_and_persists_via_api`
  round-trips both directions against a real config file.
- Zoom no-op holds for menu path: `context_menu_balance_splits_noop_when_zoomed_via_api` +
  test-only-fn twin.
- Toggling does not rebalance immediately: asserted inline in the persistence test
  (`ratio_at(root, &[])` unchanged at 0.2 immediately after the toggle-on dispatch).
- Crate compiles, new tests green, no pre-existing test weakened/deleted: confirmed above.

## Unresolved questions

None.
