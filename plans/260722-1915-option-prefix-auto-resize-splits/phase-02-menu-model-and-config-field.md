# Phase 2 — context menu model refactor + AppState toggle field

Status: pending | Owns: `src/app/state.rs` (ONLY) | Depends on: none | Parallel group: A

## Context

- `ContextMenuState::items(&self) -> &'static [&'static str]` (`:1229-1315`) — match on
  `self.kind`, 4 arms for `ContextMenuKind::Pane` keyed on `(has_manual_label,
  source_pane_id.is_some())` (`:1266-1313`), each a static slice literal ending
  `"Zoom", "Close pane"`.
- `ContextMenuKind::Pane { ws_idx, tab_idx, pane_id, source_pane_id: Option<PaneId>,
  has_manual_label: bool }` (`:1211-1217`).
- H2 (largest blast radius): adding a 3rd bool (`auto_resize_enabled`, snapshot of the global
  toggle at menu-open time — needed because `items()` takes only `&self`, no `AppState` access)
  doubles 4 arms to 8 if kept as static match arms. **Refactor to imperative `Vec` builder**:
  push base items conditionally, keep labels `&'static str` (return type becomes
  `Vec<&'static str>`).
- All 11 call sites of `.items()` (verified 2026-07-22, corrects caller's "9"): `state.rs:2849,
  2869,2889` (existing tests, unaffected — different `ContextMenuKind` variants), `ui/menus.rs:
  300` (iterates, unaffected), `modal.rs:699,914,1114,1128,2015,2046,2080` (`.get()`/`.iter()`/
  `.len()`, unaffected — `Vec<&str>` derefs to `&[&str]`), `mouse.rs:1216,1222`
  (`context_menu_rect`, unaffected, same reason). **None require edits** — `Vec<&'static str>`
  return type is a drop-in replacement via `Deref<Target = [&'static str]>`. Do not touch those
  files in this phase; Phase 5 owns the ONE real behavior change (constructing
  `ContextMenuKind::Pane` with the new field at the mouse right-click site).
- Config-bool precedent (corrected): `pane_borders`/`pane_gaps` fields on `AppState`
  (`:1510-1512`, defaults `:1995-1997`) are the FIELD SHAPE to copy (plain `bool`, doc comment,
  default value in the `Default` impl block). The toggle-and-persist BEHAVIOR precedent lives in
  Phase 3 (`config_io.rs`), not this file — this phase only declares the field.

## Requirements

1. Add `pub auto_resize_splits: bool` field to `AppState` next to `pane_borders`/`pane_gaps`
   (`:1510-1512` area), with doc comment matching the style there. Default `false` (opt-in,
   matches `pane_gaps` default of `false` not `pane_borders`' `true` — this is a new behavior,
   default off is the safer choice; not user-locked, flag if wrong in review).
2. Add default `auto_resize_splits: false,` to the `Default` impl block (`:1995-1997` area).
3. Add `zoomed_snapshot`... no — add `auto_resize_enabled: bool` field to
   `ContextMenuKind::Pane` (`:1211-1217`), doc comment: "snapshot of `AppState.auto_resize_splits`
   at menu-open time, used to render the toggle label."
4. Refactor `ContextMenuState::items()` return type `&'static [&'static str]` ->
   `Vec<&'static str>`. Rewrite the 4 `ContextMenuKind::Pane` match arms
   (`:1266-1313`) as ONE arm matching `ContextMenuKind::Pane { has_manual_label,
   source_pane_id, auto_resize_enabled, .. }`, building the `Vec` via conditional pushes in a
   fixed order: `"Rename pane"`, `if has_manual_label { "Clear pane name" }`,
   `if source_pane_id.is_some() { "Swap with focused pane" }`, `"Split right"`, `"Split down"`,
   `"Zoom"`, `"Balance splits"`,
   `if auto_resize_enabled { "Auto-resize splits: On" } else { "Auto-resize splits: Off" }`,
   `"Close pane"`. Non-`Pane` arms keep returning `.to_vec()` of their existing static slices
   (minimal diff, `&["Rename", "Close"].to_vec()` etc.) — do not restructure arms that don't
   need it (YAGNI).

## Files to modify

- `src/app/state.rs` ONLY.

## Step-by-step (TDD)

1. Add failing tests (in `src/app/state.rs`'s existing `#[cfg(test)] mod tests`, near
   `:2834-2895`):
   - `pane_context_menu_includes_balance_and_toggle_off_items` — construct
     `ContextMenuKind::Pane { auto_resize_enabled: false, has_manual_label: false,
     source_pane_id: None, .. }`, assert `items()` contains `"Balance splits"` and
     `"Auto-resize splits: Off"` in that relative order, and does NOT contain
     `"Auto-resize splits: On"`.
   - `pane_context_menu_toggle_on_shows_on_label` — same with `auto_resize_enabled: true`,
     assert `"Auto-resize splits: On"` present, `"...Off"` absent.
   - `pane_context_menu_all_4_label_combinations_unchanged_besides_new_items` — parametrize the
     existing 4 `(has_manual_label, source_pane_id)` combinations, assert the pre-existing 5-7
     item labels (`Rename pane`, `Clear pane name`, `Swap with focused pane`, `Split right`,
     `Split down`, `Zoom`, `Close pane`) are all still present in the same relative order —
     regression guard for the arm-collapse refactor.
   - `non_pane_menu_kinds_items_unchanged` — spot-check `Workspace`/`Tab`/`GitWorkspace` variants
     still return their exact original slices (guards against accidental `.to_vec()` mistakes).
   - `app_state_default_has_auto_resize_splits_off`.
2. Run `ZIG=~/.local/zig-0.15.2/zig cargo test --lib state:: -- --test-threads=4` (or
   `just test-one` if available) — expect compile failure (`auto_resize_enabled` field doesn't
   exist yet), then implement fields per Requirements 1-3.
3. Implement `items()` refactor (Requirement 4). Re-run tests until green.
4. Full-crate build check (`cargo check` via
   `ZIG=~/.local/zig-0.15.2/zig cargo check`) to catch any downstream `ContextMenuKind::Pane {
   ... }` struct-literal construction site that now fails to compile because it's missing
   `auto_resize_enabled` — grep first: `grep -rn "ContextMenuKind::Pane {" src/` to enumerate
   every construction site before touching anything (expect: `mouse.rs:1096` construction site,
   the 3 test sites in `state.rs` itself already updated in step 1, plus `modal.rs:2003,2067`
   test constructions found during verification — list all, fix each to pass
   `auto_resize_enabled: false` as a safe placeholder; Phase 5 wires the REAL value at
   `mouse.rs:1096`).

## Risks / rollback

- Risk: missed a `ContextMenuKind::Pane { .. }` construction site outside `mouse.rs`/`state.rs`
  test mod — mitigated by the full-crate `cargo check` in step 4 (compiler enforces
  exhaustiveness, can't silently miss one).
- Rollback: revert this file only; Phase 4/5 haven't landed yet if this is reverted early, so no
  cross-phase breakage. If Phase 5 already landed (depends on this), reverting requires
  reverting Phase 5 first (documented dependency, not a hidden risk).
