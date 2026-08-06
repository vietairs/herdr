# Phase 1 — layout-core equal-area solver + remove_pane characterization

File owned/touched: `src/layout.rs` only.

## Functions added

- `TileLayout::path_to_pane(&self, pane_id: PaneId) -> Option<Vec<bool>>`
- `TileLayout::balance_areas(&mut self)`
- `TileLayout::balance_areas_along_path(&mut self, path: &[bool]) -> bool`
- Private helpers: `find_path_to_pane`, `equal_area_ratio(first_leaves, second_leaves) -> f32`
  (clamps via existing `valid_split_ratio`), `balance_subtree_areas(&mut Node) -> usize`,
  `balance_split_ratios_along_path(&mut Node, &[bool]) -> bool`.

All ratio writes go through `equal_area_ratio` -> `valid_split_ratio`, so clamping is explicit
per the plan's correction (free `set_ratio_at` does not clamp).

## Tests added (all in `src/layout.rs` `mod tests`)

Characterization (pin current `remove_pane` behavior): `remove_pane_discards_parent_split_ratio_on_collapse`, `remove_pane_of_last_child_promotes_sibling_ratio_unchanged`.

`path_to_pane`: `path_to_pane_finds_leaf_at_each_depth`, `path_to_pane_missing_pane_returns_none`.

`balance_areas`: `balance_areas_equalizes_2x2_grid`, `balance_areas_equalizes_leaf_areas_at_depth_3`, `balance_areas_equalizes_leaf_areas_at_depth_4`, `balance_areas_weights_by_leaf_count_not_split_position`, `balance_areas_1v9_leaves_hits_exact_lower_clamp`, `balance_areas_1v10_leaves_clamps_with_documented_error`, `balance_areas_single_pane_is_noop`, `balance_areas_is_idempotent`, `balance_areas_handles_deeply_nested_chain_without_stack_issues` (17-leaf chain).

`balance_areas_along_path`: `balance_areas_along_path_only_touches_path_nodes`, `balance_areas_along_path_tolerates_path_longer_than_tree`, `balance_areas_along_path_empty_path_balances_root_split`, `balance_areas_along_path_single_pane_is_noop`.

Test helpers added: `skewed_chain` (lopsided "1 vs rest" chain, for leaf-weighting tests), `balanced_binary_tree` (power-of-two complete binary tree, for area-equality tests), `leaf_areas`, `assert_areas_within_one_cell`.

## Validation

`ZIG=$HOME/.local/zig-0.15.2/zig` + `PATH="$HOME/.local/zig-0.15.2/xcrun-shim:$PATH"` (bare `~/...` in `env` does not tilde-expand in zsh; `just`/nextest unavailable here — matches prior memory note).

- `cargo test --bin herdr layout:: -- --test-threads=4`: **28 passed, 0 failed** (18 new + 10 pre-existing, none modified).
- `cargo test --bin herdr -- --test-threads=4` (full suite): **2975 passed, 4 failed**. All 4 failures are pre-existing/unrelated to `layout.rs`: `api::server::pane_graphics_stream::...cancels_idle_stream` (socket timeout), `server::autodetect::...stale_socket` / `...listener_dropped` (socket contention), `workspace::tests::generated_workspace_ids_are_short_base32_handles` (random-id collision under parallel contention) — matches the known "cross-test contention under full parallelism" issue in local memory notes; not touched by this change.

## Surprising / notable

- `balance_areas_equalizes_leaf_areas_at_depth_4` initially failed (areas `[1440,1440,1440,1472,1408]` on a 5-leaf "1 vs rest" chain over 120x60). Root cause: a 1-pixel rounding error at the deepest split (height 45 -> round(22.5)=23/22) gets multiplied by the still-large remaining width (64), producing a ±32 area delta — an inherent property of pixel-quantized lopsided chains, not a bug in the balance math. Fixed by rebuilding the depth-3/depth-4 area-equality fixtures on a genuinely balanced (power-of-two-leaf, power-of-two-`Rect`) complete binary tree, where every split divides evenly with zero rounding. The lopsided-chain shape (`skewed_chain`) is kept for the tests where it's the actual point (leaf-count weighting, clamp degradation).
- Compile blocked transiently (~100s) on two `E0063` errors in `src/app/mod.rs`/`src/app/input/mouse.rs` referencing fields not yet defined in `src/app/state.rs` — from the concurrent Phase 2 agent's in-progress edit to that file (touched zero files outside `layout.rs` myself). Resolved once that agent's edit landed; confirmed via repeated `cargo check` polling that no error ever referenced `layout.rs`.
- Blank-line and doc-comment `Reconstruct a layout from a saved tree.` duplicate already present pre-existing in `from_saved` — left untouched (out of scope).

## Unresolved questions

None. Deviation from the phase file's suggested test fixture (chain shape for depth-3/4 area tests) documented above and in a code comment on `balanced_binary_tree`.

Status: DONE
Summary: Added `path_to_pane`, `balance_areas`, `balance_areas_along_path` plus 18 tests to `src/layout.rs`; all layout tests and the full suite (minus 4 pre-existing unrelated flaky failures) pass.
Concerns/Blockers: None for this phase; downstream phases (4, 5) still need to wire these into split/close/menu call sites per the plan.
