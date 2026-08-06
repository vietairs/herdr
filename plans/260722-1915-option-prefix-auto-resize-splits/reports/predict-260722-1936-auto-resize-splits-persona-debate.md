# Persona debate: "Balance splits" + "Auto-resize splits" toggle

Read-only review, worktree `herdr-worktrees/pane-auto-resize-splits`. No source edits made.

## 1. Systems/architecture

- **HIGH** — runtime/client guardrail: toggle state is a session fact once persisted, must be
  server-side/API, not TUI-socket-only. `handle_layout_set_split_ratio` (src/app/api/layouts.rs:218-248)
  is the right precedent: mutate -> `schedule_session_save()` -> `emit_layout_updated_event()`.
- **MED** — no `LayoutBalance*` method found in this review's grep; reusing `LayoutApply` forces the
  client to compute the full replacement tree, fighting the server-owns-mutation pattern the ratio API
  already sets.
- **LOW** — global config bool precedent for the toggle: `pane_borders`/`pane_gaps`
  (src/app/state.rs:1510-1512), config-persisted, not in `TabSnapshot`.

**Rec**: new API method mirroring `handle_layout_set_split_ratio`'s save/emit shape; don't overload
`LayoutApply`.

## 2. Distributed-systems/federation

- **HIGH** — `LayoutUpdated` absent from `is_structural_event_kind`
  (src/remote/federation/client.rs:814-829: PaneCreated|PaneClosed|PaneMoved|TabCreated|TabClosed|TabMoved
  only) — ratio changes never resync mounted clients today. Pre-existing gap, not introduced by this
  feature, but auto-resize fires `LayoutUpdated` far more often, widening practical impact.
- **MED** — coalescing already exists: `drive_mount_channel` (client.rs:490-509) tracks
  `resync_in_flight`; a burst of structural frames -> exactly one `SnapshotRequest`, per test at
  client.rs:1547-1553. So snapshot-storm risk, IF `LayoutUpdated` were added to the trigger list, is
  lower than the brief implies — contingent on `LayoutUpdated` actually reaching this relay path
  (unverified).
- **LOW** — gap degrades gracefully (stale ratios, not corruption) — safe to defer.

**Rec**: ship v1 local-only; adding `LayoutUpdated` to the trigger list is a separate PR needing its
own relay-path verification.

## 3. Performance

- **MED** — every split/close under auto-resize forces a full rebalance then `resize_tab_panes`
  (src/ui/panes.rs:166-211) loops all visible panes in the tab calling `rt.resize()` (unless
  `direct_attach_resize_locks` guards it) — O(N) PTY resizes per split, not O(1).
- **LOW** — `split_rect` (src/layout.rs:~621-640) is already recomputed from scratch every render, no
  cache to invalidate; rebalance itself is a cheap O(pane count) tree walk.
- **LOW** — key-repeat amplification is not a real risk: splits/closes are discrete user actions; the
  0.05-step manual resize (src/app/actions.rs:1764) is an unrelated code path.

**Rec**: no special batching needed (discrete, human-paced events), but flag the N-pane PTY resize
fanout so it's not assumed free.

## 4. UX

- **HIGH** — equal-ratio vs equal-area is genuinely ambiguous: BSP ratios are per-split-node
  (src/layout.rs:73-81), so "all ratios=0.5" != equal-area past depth 1 (3-pane row-then-column gives
  50/25/25 by area). No whole-tree equal-area solver exists today; would be new code.
- **MED** — spec says rebalance "after every split and pane close" tab-wide, not scoped — will
  overwrite a user's just-dragged manual ratio anywhere in the tab, feels like the app fighting the
  user. No existing subtree-scoping precedent found.
- **MED** — zoom: `resize_tab_panes`/`compute_pane_infos` special-case `tab.zoomed`/`ws.zoomed`
  (src/ui/panes.rs:~170-192, ~215-245) to resize only the focused pane full-area; rebalancing against
  a 1-pane zoomed view would corrupt ratios for after un-zoom.

**Rec**: no-op rebalance while zoomed; scope auto-resize to the affected split's ancestor chain, not
whole-tree.

## 5. Maintainability/testing

- **MED, downgraded from brief's HIGH** — ground truth claims numeric-index test assertions at
  modal.rs:1983/2029/2058. Verified inaccurate for current worktree: those lines are test-fn
  declarations; actual index resolution is `menu.items().iter().position(|item| *item == "Close pane")`
  — label-based. One genuine literal-index call found at modal.rs:1971, against the `GitWorkspace`
  menu (not `Pane`). Net: adding 2 pane-menu labels is lower test-churn risk than the brief assumes,
  provided new tests follow the `.position()` convention.
- **HIGH** — `items()` match arms key on 2 bools today (`has_manual_label`, `source_pane_id.is_some()`,
  src/app/state.rs:1266-1313, 4 arms). A 3rd bool (auto-resize-on-for-tab) doubles to 8 near-duplicate
  arms — pattern doesn't scale past 2 bools.
- **MED** — no characterization tests found pinning post-close ratio behavior (`remove_pane`,
  src/layout.rs:557-577) before adding rebalance logic on top of it.

**Rec**: refactor `items()` to a `Vec<&str>` builder (conditional pushes) before adding the 3rd bool —
avoids the 4->8 explosion.

## Consolidated

**Decisions needed:**
1. Equal ratios vs equal areas -> **equal ratios**. No equal-area solver exists; ratio sweep is one
   `set_ratio_at` pass. Document the 50/25/25 caveat in UI copy.
2. Balance scope: parent split vs whole tab root -> **whole root for one-shot "Balance splits"**
   (explicit user action, expects full reset); **ancestor-chain only for auto-resize-on-toggle** (fires
   silently, full-tree would fight manual resize per UX finding above).
3. Toggle persistence: global vs per-tab -> **global config bool**, precedent `pane_borders`/
   `pane_gaps` (state.rs:1510-1512) — avoids `SNAPSHOT_VERSION` bump for v1.
4. Fix federation gap in this PR vs local-only -> **local-only**. Gap is pre-existing, degrades
   gracefully, relay path for `LayoutUpdated` unverified.

**Riskiest thing**: the `items()` match-per-bool-combination pattern (state.rs:1266-1313) is the real
blast radius, not the layout math — a 3rd toggle bool doubles arm count, and the design already has
combinatorial pressure (manual-label x source-pane x now auto-resize). Fix the builder pattern
alongside this feature, not after.

**Minimum viable v1**: ship "Balance splits" (one-shot, whole-root, equal-ratio, new API method
mirroring `handle_layout_set_split_ratio`) alone. Defer the persistent toggle — it carries the open
UX question (rebalance scope vs manual-resize fights, zoom interaction) and the maintainability cost
(3rd menu bool). One-shot needs no new persisted state, no config bump, no `items()` explosion.

## Unresolved questions
- Does `LayoutUpdated` reach the federation wire today prior to the trigger-list check, or filtered
  earlier? Not traced.
- Exact protocol `Method` enum contents for a hypothetical balance method — not exhaustively grepped.
- Is `direct_attach_resize_locks` per-pane or per-terminal-id — affects how many of N panes in a
  rebalanced tab get a real PTY resize.

Status: DONE
Summary: 5-persona debate complete with file:line evidence; corrected one stale ground-truth claim
(context-menu tests mostly label-based via `.position()`, not numeric-index) and confirmed the
federation resync gap is pre-existing/degrades gracefully rather than newly introduced.
