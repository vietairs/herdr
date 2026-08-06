# Phase 5 — menu construction + dispatch wiring

Status: pending | Owns: `src/app/input/modal.rs`, `src/app/input/mouse.rs`
Depends on: Phase 2 (item labels + `ContextMenuKind::Pane.auto_resize_enabled` field), Phase 3
(`save_auto_resize_splits`), Phase 4 (`Method::LayoutBalance` dispatch via `runtime_pane_*`-style
call, or direct handler call) | Parallel group: C (solo, serialized last)

## Context

- Menu construction (right-click pane): `src/app/input/mouse.rs:1075-1109`, the
  `ContextMenuState { kind: ContextMenuKind::Pane { ws_idx, tab_idx, pane_id, source_pane_id,
  has_manual_label }, .. }` literal is at `:1095-1106`. Must add
  `auto_resize_enabled: self.auto_resize_splits` to this literal (reads the live `AppState`
  field, `self` here is `&AppState` per the surrounding `impl` — confirm receiver type before
  editing, the method is on `AppState` not `App`).
- Dispatch: TWO parallel functions handle the same match, keep both in sync (existing pattern,
  not a smell introduced by this phase — pre-existing dual test/prod dispatcher):
  - `apply_context_menu_action` (`modal.rs:693-894`, `#[cfg(test)]`-only, takes `&mut AppState`
    + `&mut TerminalRuntimeRegistry` directly — no async/API access, used by unit tests only).
  - `apply_context_menu_action_via_api` (`modal.rs:1127+`, `impl App`, production path, calls
    through `self.` which has API-handler access).
  - Existing `"Split right"`/`"Split down"`/`"Close pane"` arms (`:830-891` in the test fn,
    mirrored in the `_via_api` fn) show the established pattern: set `state.selected`/
    `state.active`, `switch_tab`, `focus_pane_in_workspace`, then call the mutating fn, then set
    `state.mode = Mode::Terminal`.
- "Balance splits" and the toggle are less "focus a pane and act" and more "act on the tab as a
  whole" (D3: one-shot = whole tab root) — still needs `ws_idx`/`tab_idx` from the menu's
  `ContextMenuKind::Pane` to target the right tab, but does NOT need to change focus/switch tabs
  first (unlike split/close, balance doesn't create/destroy the focused pane).
- Production "Balance splits" call: since `handle_layout_balance` (Phase 4) is a JSON-API-style
  handler taking `(id: String, params: LayoutExportParams)` and returning an encoded JSON
  string, but the menu dispatch context doesn't have a live request `id` — check how other
  in-app-triggered-but-API-shaped actions call handlers without a real request id (e.g. grep
  `handle_layout_apply\(` or similar internal call patterns in `modal.rs`/`navigate.rs` for a
  precedent of calling an `id`-taking handler with a synthetic id like `String::new()` or a
  fixed sentinel) — if no precedent exists, use `"internal".to_string()` as the id (response is
  discarded, only the `AppState` mutation matters) and ignore/log the returned JSON on error via
  existing toast infra (`state.config_diagnostic`-style, or whatever error-surfacing pattern
  `"Split right"`'s arm uses when `split_pane` can fail — none currently surfaces split
  failures to a toast per the read arm, `:823-838`, so match that silence-on-success precedent,
  don't over-engineer error UI beyond what siblings do).
- Toggle click calls `self.save_auto_resize_splits(!self.state.auto_resize_splits)` directly
  (Phase 3's fn) — no JSON-API involvement, matches the Settings-screen precedent
  (`input/settings.rs:48`).

## Requirements

1. `mouse.rs:1095-1106` — add `auto_resize_enabled: self.auto_resize_splits` to the
   `ContextMenuKind::Pane` construction.
2. `modal.rs` `apply_context_menu_action_via_api` (production): add two new match arms after the
   existing `"Zoom"` arm (`:855-870` today):
   - `Some("Balance splits")`: resolve `ws_idx`/`tab_idx` from `ContextMenuKind::Pane`, call
     `self.handle_layout_balance("internal".into(), LayoutExportParams { tab_id: Some(<public
     tab id for ws_idx,tab_idx>), pane_id: None })` (or equivalent direct-fn-call bypassing full
     JSON-string encode/decode if a lower-level non-string-returning variant is cleaner — prefer
     whatever avoids re-parsing JSON just fired by ourselves; if `handle_layout_balance` can't be
     cleanly called this way, split its body into a private `fn balance_tab_layout(&mut self,
     ws_idx, tab_idx) -> bool` that the JSON handler wraps, and have this arm call the private fn
     directly — cleaner, avoid stringly-typed self-calls, DRY over the JSON boundary). Then
     `state.mode = Mode::Terminal`.
   - `Some(label) if label.starts_with("Auto-resize splits:")`: call
     `self.save_auto_resize_splits(!self.state.auto_resize_splits)`, `leave_modal(&mut
     self.state)` (matches the `Collapse`/`Expand` toggle precedent's control flow at
     `:713-733`, which also calls `leave_modal` rather than `Mode::Terminal` — toggles close the
     menu without necessarily returning to terminal focus if triggered from elsewhere; confirm
     against the Collapse/Expand precedent exactly).
3. Mirror both arms in the `#[cfg(test)]` `apply_context_menu_action` (`modal.rs:693-894`) using
   `&mut AppState` directly (no API handler available there — call `TileLayout::balance_areas`/
   `balance_areas_along_path` directly on `state.workspaces[ws_idx].tabs[tab_idx].layout`,
   mirroring how the JSON handler works but without the JSON plumbing, consistent with how this
   test-only fn already reimplements `close_pane`-adjacent logic inline elsewhere in the file).

## Files to modify

`src/app/input/mouse.rs`, `src/app/input/modal.rs`. Optionally `src/app/api/layouts.rs` if
Requirement 2's private-fn split is chosen (coordinate with Phase 4 — if Phase 4 already landed,
this is a small follow-up edit to an already-owned-by-nobody file at this point, acceptable).

## Step-by-step (TDD)

1. Add failing tests in `modal.rs` test mod:
   - `context_menu_balance_splits_equalizes_tab_via_api` — build unequal-ratio tab, open pane
     context menu, dispatch `"Balance splits"` via `apply_context_menu_action_via_api`, assert
     ratios equalized (leaf-weighted) via `layout_description`/`pane_rects`-style assertion.
   - `context_menu_toggle_auto_resize_flips_state_and_persists` — dispatch the toggle label
     (whichever of On/Off is showing), assert `app.state.auto_resize_splits` flipped AND (if a
     temp config path fixture is available, per Phase 3's test harness) the written config file
     reflects it.
   - `context_menu_balance_splits_noop_when_zoomed` — zoom the tab, dispatch, assert unchanged
     (mirrors Phase 4's handler-level test at the dispatch-integration level).
   - Mirror all three against the `#[cfg(test)]` `apply_context_menu_action` fn too (test-only
     dispatcher parity, matches existing test coverage style for other actions in this file).
2. Add failing test in `mouse.rs` test mod: right-click a pane with toggle currently ON, assert
   the constructed `ContextMenuState`'s `items()` contains `"Auto-resize splits: On"` (proves
   the live field is read correctly at menu-open time, not just at dispatch time).
3. Implement Requirements 1-3. Run
   `ZIG=~/.local/zig-0.15.2/zig cargo test --lib app::input:: -- --test-threads=4`.
4. Full suite + acceptance criteria pass: `ZIG=~/.local/zig-0.15.2/zig cargo test
   -- --test-threads=4` (or `just check` if it becomes available). Manually verify plan.md's
   10 acceptance criteria against test names added across all 5 phases — every criterion should
   map to at least one named test; if not, add the missing test here (this phase is the
   integration seam, the right place to catch coverage gaps).

## Risks / rollback

- Risk: `label.starts_with("Auto-resize splits:")` string-matching the toggle item is brittle if
  Phase 2's exact label text changes later — acceptable given `Collapse`/`Expand` already does
  full-string exact match per state (not prefix), so consider matching both exact strings
  (`"Auto-resize splits: On"` / `"Auto-resize splits: Off"`) instead of `starts_with`, consistent
  with the `Collapse | Expand` OR-pattern precedent (`modal.rs:713-718`) — prefer this over
  prefix matching (tighter, matches codebase style). Corrected guidance: use
  `Some("Auto-resize splits: On" | "Auto-resize splits: Off")` OR-pattern, not `starts_with`.
- Rollback: revert this file set; Phases 1-4 remain inert/unreachable (no UI path invokes them),
  zero user-visible change if reverted alone.
