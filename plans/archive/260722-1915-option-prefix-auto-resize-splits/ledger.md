Task: one-shot equal-area split balance + persistent auto-rebalance toggle
on the pane context menu (local-only v1).

Shipped: balance + auto-resize toggle. Fable/high review (4 lenses +
adversarial verify): 3 findings confirmed and fixed (610d350d), 2 refuted.
H1 closed at the class level — rule moved into
`TileLayout::balance_areas_after_removal` + exhaustive shape sweep
(573552b6).

PR #4 — MERGED 2026-07-22 23:50 as `9e8ada16`.

Teardown: worktrees `pane-auto-resize-on-origin` / `pane-auto-resize-splits`
removed. Local branch `feat/pane-auto-resize-splits` kept (off 5ec2a10b,
unpushable — pre-dates the vendored-blob history problem); the
on-origin branch was merged. Remote branch kept as rollback evidence.

Archived-at-SHA: d2ce3de280b2446a3d07575795416b6976077898
