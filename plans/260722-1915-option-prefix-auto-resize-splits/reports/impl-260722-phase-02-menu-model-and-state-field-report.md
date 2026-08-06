# Phase 2 implementation report — menu model + AppState toggle field

File touched: `src/app/state.rs` only (worktree: `herdr-worktrees/pane-auto-resize-splits`).

## What changed

1. `ContextMenuState::items()` return type: `&'static [&'static str]` -> `Vec<&'static str>`.
   Non-`Pane` arms unchanged besides `.to_vec()` on the array literal. The 4 `Pane` arms
   collapsed into ONE arm that builds a `Vec` via conditional pushes, fixed order:
   `"Rename pane"`, `[if has_manual_label] "Clear pane name"`,
   `[if source_pane_id.is_some()] "Swap with focused pane"`, `"Split right"`, `"Split down"`,
   `"Zoom"`, `"Balance splits"`, `[auto_resize_enabled ? "...: On" : "...: Off"]`,
   `"Close pane"`.
2. `ContextMenuKind::Pane` gained `auto_resize_enabled: bool` (doc comment: snapshot of
   `AppState.auto_resize_splits` at menu-open time).
3. `AppState` gained `pub auto_resize_splits: bool` next to `pane_borders`/`pane_gaps`
   (`:1510-1513` area), doc comment added. Initialized `false` in `AppState::test_new()`
   (`:1996` area). Note: there is no `impl Default for AppState` in this file — the only
   full-literal constructor here is `test_new()`; production construction lives in
   `src/app/mod.rs:567` (Phase 3's file, not touched).

## Tests added (in `src/app/state.rs` `mod tests`, after
`parent_worktree_context_menu_uses_repo_actions`)

- `pane_context_menu` — private test helper builder (`has_manual_label`, `source_pane_id`,
  `auto_resize_enabled`) to avoid literal duplication across the 3 new pane tests.
- `pane_context_menu_includes_balance_and_toggle_off_items`
- `pane_context_menu_toggle_on_shows_on_label`
- `pane_context_menu_all_4_label_combinations_unchanged_besides_new_items` (loops all 4
  `(has_manual_label, has_source)` combos, asserts old items still present in relative order,
  plus `"Balance splits"` present)
- `non_pane_menu_kinds_items_unchanged` (Workspace/Tab/GitWorkspace)
- `app_state_default_has_auto_resize_splits_off`

All use `.position(|item| *item == "...")` per the file's existing convention.

## Validation

`ZIG=~/.local/zig-0.15.2/zig cargo check --tests` from worktree root (no `just`/nextest here,
per plan). `cargo fmt -- --check src/app/state.rs` also run and clean after one fix.

Result: **5 compile errors, none in `src/app/state.rs`** — confirms this file is internally
correct. Could not run `cargo test` (single-binary crate, no separate `[lib]` target — the
whole crate must compile). This is the expected state per the plan's phase-dependency graph
(Phase 3/5 depend on Phase 2 and haven't landed yet).

## Call sites needing attention (owned by other phases — NOT edited here)

`ContextMenuKind::Pane { .. }` struct-literal constructions missing `auto_resize_enabled`
(match patterns using `..` are unaffected and compile fine — e.g. `state.rs:2366`,
`mouse.rs:2294`):

- `src/app/input/mouse.rs:1096` — production right-click construction site (Phase 5 wires the
  real toggle value here per plan).
- `src/app/input/mouse.rs:2843` — test construction.
- `src/app/input/modal.rs:2003` — test construction.
- `src/app/input/modal.rs:2068` — test construction.

`AppState { .. }` struct-literal construction missing `auto_resize_splits`:

- `src/app/mod.rs:567` — production constructor (Phase 3's file; full explicit literal, no
  `..Default::default()` spread).

The 11 `.items()` call sites in `ui/menus.rs`, `modal.rs`, `mouse.rs` all compile unaffected
via `Deref<Target=[&str]>` — confirmed no errors attributed to any of them.

## Deviations

- Phase file step 4 suggested this phase should also patch the 4 `ContextMenuKind::Pane`
  construction sites in `mouse.rs`/`modal.rs` with `auto_resize_enabled: false` placeholders.
  Per the explicit spawn-prompt boundary ("YOU OWN EXACTLY ONE FILE... Do NOT edit any other
  source file... report BLOCKED"), those files were left untouched; listed above instead for
  the owning phase.
- Fixed one `cargo fmt` formatting nit inside my new test code (line-wrap of a
  `.position().unwrap_or_else()` chain) — mechanical, no behavior change.

## Unresolved questions

None for this file's scope. Cross-phase note: Phase 3 (`app/mod.rs`) and Phase 5
(`mouse.rs`/`modal.rs`) must land the 5 listed field additions before the crate compiles and
the new tests here can actually execute.

Status: DONE_WITH_CONCERNS
Summary: `src/app/state.rs` fully implements the items() Vec refactor, the new `auto_resize_enabled`/`auto_resize_splits` fields, and all 5 required tests; file compiles clean in isolation but the whole-crate build (and thus `cargo test`) stays red until Phases 3/5 add the field to their 5 owned construction sites, as expected by the plan's dependency graph.
Concerns/Blockers: Cannot run `cargo test` to green until Phase 3 (`app/mod.rs:567`) and Phase 5 (`mouse.rs:1096,2843`, `modal.rs:2003,2068`) land their field additions — this is structural, not a defect in this file.
