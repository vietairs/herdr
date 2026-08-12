# Phase 1 — layout-core equal-area solver + remove_pane characterization

Status: pending | Owns: `src/layout.rs` (ONLY) | Depends on: none | Parallel group: A

## Context

- `enum Node { Pane(PaneId), Split { direction, ratio: f32, first: Box<Node>, second: Box<Node> } }`
  — `src/layout.rs:73-81`.
- `TileLayout { root: Node, focus: PaneId }` — `:84-87`.
- `count_panes(&Node) -> usize` already exists (`:406-411`), private, leaf-count recursion —
  reuse, don't duplicate (DRY).
- Ratio clamp is 0.1..0.9. `TileLayout::set_ratio_at` clamps (`:209-210`); the free
  `set_ratio_at()` fn does **not** (`:580-599`, direct assign) — writing ratios directly to
  `Node::Split.ratio` in new code must clamp explicitly, e.g. via `valid_split_ratio()`
  (`:549-555`, already handles non-finite -> 0.5 too — reuse it).
- `remove_pane(Node, PaneId) -> Option<Node>` (`:557-577`) collapses the tree on close.
  Verified: when one child returns `None`, the surviving child fully replaces the parent
  `Split` node (ratio and direction of the removed node are discarded, not propagated) — this
  is the mechanism that currently produces lopsided layouts after repeated closes. NO test
  today pins this exact discard behavior (H5) — must characterize BEFORE adding balance logic
  on top, so a future change to `remove_pane` can't silently break balance's assumptions.
- Existing test scaffolding to reuse: `pane()`, `sample_layout()`, `pane_rects()`, `pane_rect()`,
  `split_snapshot()` helpers already in `#[cfg(test)] mod tests` (`:642-708`).

## Requirements

1. `path_to_pane(&self, pane_id: PaneId) -> Option<Vec<bool>>` on `TileLayout` — walk from root,
   return the sequence of branch choices (`false`=first, `true`=second) at each `Split` node
   leading to the leaf holding `pane_id`. `None` if `pane_id` not present. Path format matches
   the existing `SplitBorder.path` / `set_ratio_at(path, ...)` convention (`:580-599`,
   `:601-619`) — reuse that convention, don't invent a new one.
2. `balance_areas(&mut self)` on `TileLayout` — recompute **every** split ratio in the whole
   tree via leaf-count weighting: for each `Split`, `ratio = clamp(leaves(first) as f32 /
   (leaves(first) + leaves(second)) as f32, 0.1, 0.9)`. This is D3's "whole tab root" case.
   Implement as one recursive pass returning `(subtree_leaf_count)`, setting ratios as a side
   effect (same recursion shape as `count_panes`, extended).
3. `balance_areas_along_path(&mut self, path: &[bool]) -> bool` on `TileLayout` — walk `root`
   along `path`; at each `Split` node visited (i.e. while `path` still addresses a `Split`),
   recompute **only that node's** ratio via the same leaf-weighted formula over its current two
   children subtrees, then recurse into the child indicated by the next path element. Stop
   silently (return what was set so far) if `path` runs out or addresses a `Pane` — this must
   tolerate a path that is **longer than the current tree depth at that spot**, which happens
   post-close when `remove_pane` collapsed a level out from under the path (H5 interaction).
   Returns `true` if at least one ratio changed. This is D3's "ancestor chain only" case, used
   for both post-split (call with `path_to_pane(new_id)`, computed AFTER the split) and
   post-close (call with the path captured BEFORE `remove_pane`, since the pane is gone after).
4. Degenerate/edge cases to handle explicitly (not incidentally):
   - Single-pane tree (`Node::Pane`, no splits): `balance_areas` / `balance_areas_along_path`
     are no-ops, return `false`/unchanged.
   - `path` = `[]` on a `Split` root: `balance_areas_along_path` balances just the root split.
   - `path` addressing a node that no longer exists post-collapse (H5): must not panic, must
     not silently write past the actual tree — bound the walk to `Node::Split` matches only.

## Files to modify

- `src/layout.rs` — add `path_to_pane`, `balance_areas`, `balance_areas_along_path`
  (all on `impl TileLayout`, plus private recursive helpers below the existing `// --- Tree
  operations ---` marker at `:404`). No other file touched.

## Step-by-step (TDD — tests first)

1. Add characterization tests for `remove_pane` (H5), pinning CURRENT behavior before any new
   code:
   - `remove_pane_discards_parent_split_ratio_on_collapse` — build a 3-pane tree where the
     removed pane's sibling subtree has a non-0.5 ratio; close the OTHER pane; assert the
     grandparent's ratio is unchanged (sibling subtree ratio survives untouched) — pins the
     "ancestor ratios unchanged" ground-truth claim.
   - `remove_pane_of_last_child_promotes_sibling_ratio_unchanged` — 2-pane split, close one;
     assert `pane_count() == 1` and the survivor's rect is the full area (no residual split).
2. Run tests, confirm both pass against current code (pure characterization, no impl change
   yet) — `ZIG=~/.local/zig-0.15.2/zig cargo test path_to_pane -- --test-threads=4` will fail to
   compile until step 3-4 land; run just the `remove_pane_*` tests first in isolation.
3. Add failing tests for `path_to_pane`:
   - `path_to_pane_finds_leaf_at_each_depth` — using `sample_layout()` (4-pane, mixed
     directions), assert path for pane(1) is `[]`... actually pane(1) is direct child of root
     (`first`), so path is `[false]`; pane(4) is `[true, true, true]`. Verify via existing
     `split_snapshot`/tree shape in `sample_layout()` (`:650-670`).
   - `path_to_pane_missing_pane_returns_none`.
4. Add failing tests for `balance_areas`:
   - `balance_areas_equalizes_2x2_grid` — 4 equal leaves nested (2 splits), all manual ratios
     pre-set to non-0.5 (e.g. 0.3/0.7), assert post-balance all 4 `PaneInfo.rect` areas within
     ±1 cell (rounding) on a `Rect::new(0,0,100,40)`.
   - `balance_areas_weights_by_leaf_count_not_split_position` — 3-leaf tree where one branch has
     2 leaves and the other has 1 (asymmetric BSP depth); assert the 2-leaf branch's split ratio
     is `2/3` (clamped if needed) not `0.5`.
   - `balance_areas_1v9_leaves_hits_exact_lower_clamp` — assert ratio == 0.1 exactly (1/10 = 0.1,
     representable, not degraded).
   - `balance_areas_1v10_leaves_clamps_with_documented_error` — assert ratio == 0.1 (clamped,
     NOT 1/11≈0.0909) — this IS the accepted degradation per H1; test documents it, doesn't
     "fix" it.
   - `balance_areas_single_pane_is_noop`.
5. Add failing tests for `balance_areas_along_path`:
   - `balance_areas_along_path_only_touches_path_nodes` — 4-leaf tree (2 sibling 2-leaf
     subtrees), pre-set BOTH subtrees' internal ratios to 0.3; call
     `balance_areas_along_path` with the path into ONLY one subtree; assert that subtree's ratio
     changed and the OTHER subtree's ratio is still 0.3 (D3 sibling-preservation, the core
     correctness property of the whole feature).
   - `balance_areas_along_path_tolerates_path_longer_than_tree` — build a path `[false, true,
     false]` against a tree only 1 level deep at that branch (simulating post-collapse
     shortening); assert no panic, returns without corrupting the tree.
   - `balance_areas_along_path_empty_path_balances_root_split`.
6. Implement `path_to_pane`, `balance_areas`, `balance_areas_along_path` to make all tests pass.
   Reuse `count_panes` pattern; do not duplicate leaf-counting logic — extend it or call it per
   subtree.
7. Run full `src/layout.rs` test module: `ZIG=~/.local/zig-0.15.2/zig cargo test --lib layout::
   -- --test-threads=4` (fallback if `just test-one` unavailable). Confirm zero regressions in
   existing tests (`:709-956`).

## Risks / rollback

- Risk: leaf-weighted formula recursion double-counts or under-counts on deeply nested trees
  (depth > `MAX_LAYOUT_DEPTH = 16`, `src/app/api/layouts.rs:16`) — mitigate with a depth-bounded
  test (build a 16-deep degenerate chain, assert no stack issue / correct leaf counts).
- Rollback: this phase is additive-only (3 new pub methods + private helpers), zero call sites
  yet (nothing invokes them until Phase 4). Revert = delete the added code block; zero blast
  radius on other phases if reverted before Phase 4 lands.
