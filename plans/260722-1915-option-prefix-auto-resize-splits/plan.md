---
title: "Balance splits + auto-resize toggle (pane context menu)"
description: "One-shot equal-area split balance + persistent auto-rebalance toggle, local-only v1"
status: pending
priority: P2
effort: 10h
branch: feat/pane-auto-resize-splits
tags: [layout, context-menu, config, api]
created: 2026-07-22
---

# Balance splits + auto-resize toggle

Scope: Unit B only (auto-resize splits). Unit A (option+b prefix) is a separate, unrelated
config change tracked in `pipeline.md` in this same dir — not part of this plan.

Work context: `/Users/hvnguyen/Projects/herdr-worktrees/pane-auto-resize-splits`
(branch `feat/pane-auto-resize-splits`). All file:line citations below verified against this
worktree on 2026-07-22.

## Corrections to caller-supplied ground truth (verified; full detail in phase files)

- Ratio clamp only happens at call sites (`layout.rs:209-210`, `:549-555`), NOT inside the free
  `set_ratio_at()` fn (`:580-599`) — balance code must clamp explicitly. See phase 1.
- `.items()` has **11** call sites, not 9 (see phase 2) — most unaffected by the `Vec` return
  type change (`Deref`), all must be checked to compile.
- Manual keyboard resize is production-routed through `handle_pane_resize`
  (`src/app/api/panes.rs`, via `Method::PaneResize`), never through split/close code —
  structurally confirms H4. `AppState::resize_pane`/`split_pane`/`close_pane` are
  `#[cfg(test)]`-only stubs; production split/close chokepoints are `handle_pane_split`
  (`src/app/api/panes.rs:40-161`) and `close_pane`/`handle_pane_close` (`:1663-1733`) — these
  are the correct auto-rebalance hook points (see phase 4).
- Config-bool precedent is two-layer (`UiConfig` field + mirrored `AppState` field), but
  `pane_borders`/`pane_gaps` have **no in-app toggle** — real toggle-and-persist precedent is
  `save_agent_border_labels()` (`src/app/config_io.rs:82-93`). See phase 3. `AppState` is
  confirmed server-side (API handlers mutate `self.state.*` directly) — satisfies the
  runtime/client guardrail with no new protocol surface for the toggle itself.
- `Tab.zoomed: bool` (`src/workspace/tab.rs:48`) is NOT on `TileLayout` — zoom no-op (D6) is a
  caller-side check (phase 4), not inside `layout.rs`.
- New JSON-RPC method requires regenerating `docs/next/api/herdr-api.schema.json` via
  `HERDR_UPDATE_API_SCHEMA=1 cargo test generated_protocol_schema_artifact_is_current`
  (test at `src/api/schema/tests.rs:156-169`) — see phase 4.

## Test command (this machine)

Try first: `just test` / `just check`. **Known to be unavailable here** (no `just`/`nextest`).
Fallback: `ZIG=~/.local/zig-0.15.2/zig cargo test -- --test-threads=4` (from worktree root).

## Phases

| # | File | Owns | Depends on | Parallel group |
|---|------|------|-----------|-----------------|
| 1 | [phase-01-layout-core-solver.md](phase-01-layout-core-solver.md) | `src/layout.rs` | none | A |
| 2 | [phase-02-menu-model-and-config-field.md](phase-02-menu-model-and-config-field.md) | `src/app/state.rs` | none | A |
| 3 | [phase-03-config-persistence.md](phase-03-config-persistence.md) | `src/config/model.rs`, `src/app/config_io.rs`, `src/app/mod.rs` | Phase 2 | B |
| 4 | [phase-04-server-api-balance.md](phase-04-server-api-balance.md) | `src/api/schema.rs`, `src/api/schema/panes.rs`, `src/api/schema/response.rs`, `src/app/api/layouts.rs`, `src/app/api/panes.rs`, `docs/next/**` | Phase 1, Phase 2 | B |
| 5 | [phase-05-menu-dispatch-and-tests.md](phase-05-menu-dispatch-and-tests.md) | `src/app/input/modal.rs`, `src/app/input/mouse.rs` | Phase 2, 3, 4 | C |

Group A phases (1, 2) touch disjoint files — run parallel. Group B (3, 4) touch disjoint files
and both only depend on group A — run parallel after A lands. Group C (5) is solo, serialized
after B (it wires everything together; touches the only two files nothing else owns).

Deviation from caller's suggested shape: merged "items() refactor" and "config bool field" into
one phase (both are `src/app/state.rs` edits — splitting them would violate the disjoint-file
rule for parallel phases). Split "server API" into its own phase separate from "config bool"
since they touch entirely disjoint files and have no ordering dependency on each other.

## Acceptance criteria (behavioral, verifiable)

1. Right-click a pane in a tab with ≥2 panes → menu shows "Balance splits" and
   "Auto-resize splits: Off" (or "On"), verified via `ContextMenuState::items()` unit test.
2. Selecting "Balance splits" on a tab with unequal manual ratios equalizes **areas**
   (leaf-weighted), verified by asserting `PaneInfo.rect` areas within ±1 cell of each other
   post-balance, for tab depths 2-4.
3. Selecting "Balance splits" while any pane in the tab is zoomed is a no-op (ratios and
   `layout_description` unchanged), per D6/H3.
4. With toggle ON: `pane.split` immediately followed by `layout.export` shows only the
   split/close pane's ancestor-chain ratios changed; sibling subtrees' manual ratios untouched
   (D3).
5. With toggle ON: closing a pane rebalances the collapsed ancestor chain; other branches'
   ratios untouched. Regression-proof via Phase 1 characterization tests pinned first.
6. With toggle OFF (default): split/close behavior is byte-identical to pre-feature behavior
   (existing tests in `layout.rs`/`panes.rs` keep passing unmodified).
7. Toggle state round-trips: flip via menu → restart process (fresh config load) → toggle state
   preserved. `[ui]` section in `~/.config/herdr/config.toml` (or `HERDR_CONFIG_PATH`) contains
   `auto_resize_splits = true/false`.
8. `docs/next/api/herdr-api.schema.json` regenerated and matches `generated_protocol_schema_artifact_is_current`.
9. 1-vs-9-leaf split balances to exactly ratio 0.1 (representable); 1-vs-10-leaf clamps to 0.1
   with a passing test documenting the non-exact degradation (H1).
10. `ZIG=~/.local/zig-0.15.2/zig cargo test -- --test-threads=4` green, or `just test` if
    available.

## Out of scope / explicit non-goals

- `src/remote/federation/**` untouched (D4). Known gap: mounted remote workspaces do not
  auto-rebalance or receive balance actions in v1 — must be documented in code comment at the
  hook site in Phase 4 and in `docs/next` (server-authority note), not fixed here.
- No `SNAPSHOT_VERSION` bump (D5) — this is a `[ui]` config key, not session-snapshot schema.
- No new keybind for balance/toggle (menu-only, per feature spec).

## Unresolved questions

1. Exact toggle menu label/copy: "Auto-resize splits: On/Off" vs a checkmark prefix (✓) like
   other toggles may use — no existing toggle-style item in `ContextMenuState::items()` today
   (Collapse/Expand precedent swaps label text entirely, doesn't prefix a checkmark). Phase 5
   implementer should pick the pattern consistent with `Collapse`/`Expand` (full label swap) —
   proposed default, confirm if it matters.
2. Should `Method::LayoutBalance` reuse `LayoutExportParams` verbatim (DRY, same
   `tab_id`/`pane_id` shape) or get its own params struct? Recommend reuse (Phase 4), flag if
   API-schema conventions elsewhere disagree.
3. Federation gap doc: which `docs/next` page is the right home (socket-api.mdx vs a dedicated
   federation limitations note)? Defaulting to a code comment + one line in socket-api.mdx;
   confirm if a stronger doc surface is wanted.
