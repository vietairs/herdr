# Scout report — pane layout / geometry model

Stage 2 (blindspot), scout 1 of 3. Read-only. Worktree:
`/Users/hvnguyen/Projects/herdr-worktrees/pane-auto-resize-splits`

## Findings

1. **Layout representation** — binary space partition (BSP) tree.
   `enum Node` at `src/layout.rs:73-81`:
   - `Node::Pane(PaneId)` — leaf
   - `Node::Split { direction, ratio, first, second }` — internal
   Held by `TileLayout { root: Node, focus: PaneId }` at `src/layout.rs:84-87`.

2. **Sizes** — NOT stored. Only `ratio: f32` per split node (`src/layout.rs:76`).
   Rects recomputed every render from the tree via `split_rect()` (`src/layout.rs:621-640`).
   No size cache. Ratios clamped 0.1–0.9.

3. **On split** — default ratio `0.5`. `TileLayout::split_focused()` (`src/layout.rs:126-128`)
   -> `split_focused_with_ratio(0.5)` -> `split_at()` (`src/layout.rs:520-547`).

4. **On close** — tree collapse. `remove_pane()` (`src/layout.rs:557-577`): parent adopts the
   surviving sibling. **Ratios of ancestor splits are left unchanged** — this is the source of
   the lopsided-layout feel that motivates the feature.

5. **Manual resize mode** — `Mode::Resize` -> `tab.layout.resize_focused(direction, delta, area)`
   (`src/app/actions.rs:1764`) -> finds nearest split in direction -> `set_ratio_at()`
   (`src/layout.rs:580-599`). Mutates ratio only; does NOT itself trigger PTY resize.

6. **PTY propagation** — automatic at render. `resize_tab_panes()` (`src/ui/panes.rs:166-211`)
   walks panes, computes rects, calls `rt.resize(height, width, cell_px_w, cell_px_h)`.
   Also `compute_pane_infos()` (`src/ui/panes.rs:215-250`) when `resize_panes=true`.
   Path: `compute_tab_surface()` -> `resize_tab_surface()` -> `resize_tab_panes()`.

7. **Render purity** — `compute_view()` (`src/ui.rs:116-119`) DOES mutate (calls
   `compute_view_internal()`, which may call `resize_tab_panes()`). So a balance action
   mutates ratios in state; `compute_view()` + render propagate to PTYs. No manual PTY
   resize call needed in the action itself.

## Implication for the plan

Manual "balance splits" is **much cheaper than assumed at classification time**: it is a
recursive walk setting every `Node::Split.ratio` to `0.5` within the target subtree
(or the whole tab root). No new geometry math, no PTY plumbing, no cache invalidation —
existing render path handles propagation.

Persistent auto-resize toggle = same walk, re-applied after split/close. The cost is in
WHERE the flag lives (server vs client) and menu-label statefulness, not in layout math.

Note: equal RATIOS != equal AREAS for unbalanced trees. A 3-pane tree
`Split(A, Split(B, C))` at all-0.5 gives A=50%, B=25%, C=25%. True equal-area balancing
requires weighting by leaf count per subtree. **Which one the user wants is an open
question for the plan gate.**

## Unresolved questions

- Balance scope: focused pane's immediate parent split, or entire tab root?
- Equal ratios (simple, tmux `select-layout even-*`-ish) vs equal areas (leaf-count weighted)?

Status: DONE
Summary: BSP tree with per-split f32 ratios recomputed each frame; balance = recursive
ratio reset, propagation already handled by the render path.
Concerns: equal-ratio vs equal-area semantics is a real product decision, not an
implementation detail.
