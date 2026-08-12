# Code review — balance splits + auto-resize toggle (uncommitted diff)

Scope: `git diff master` on `feat/pane-auto-resize-splits` (16 files, +1683/-56).
Method: static trace only. **Tests NOT executed** (no `just`/`nextest`; full cargo+zig build
not run in this session). All findings below are code-trace based.

## HIGH — CONFIRMED

### H1. Close hook rebalances the *promoted sibling subtree*, violating D3
`src/app/api/panes.rs:1776-1788` + `src/layout.rs:104-126`

The path to the closing pane is captured pre-close, but `remove_pane` collapses the parent
split and **promotes the sibling in its place**. Replaying the full stale path on the shortened
tree therefore descends one level into the promoted sibling and rewrites its ratio.
`balance_split_ratios_along_path` balances the node it lands on *before* consuming the path
element, so tolerating an overlong path is not enough — it actively mutates an out-of-chain node.

Failure scenario:
```
root H(0.7): first = Pane(1)
             second = V(0.6): first = Pane(2)
                              second = H(0.25): Pane(3), Pane(4)   <- user's manual 0.25
```
Toggle ON, close Pane(2). path_to_pane(2) = `[true,false]`. After collapse `second` is the
promoted `H(0.25)` subtree. Walk: balance root (ok, ancestor) → descend `true` → lands on the
promoted `H` → sets ratio 0.5. The user's manual 0.25 in an unrelated subtree is destroyed.

`src/layout.rs:209` (`remove_pane_discards_parent_split_ratio_on_collapse`) explicitly pins
"sibling subtree's ratio must survive the parent collapse unchanged" — the close hook then
breaks that invariant one call later.

Test gap: `pane_close_auto_rebalance_touches_only_collapsed_ancestor_chain`
(`panes.rs:4107`) only closes a pane whose sibling is a **leaf**, so the descent stops at
`Node::Pane` and the bug is invisible. The fixture cannot reach the failing shape.

Fix direction: after a collapse, the surviving ancestors are the prefixes of length
`0..=len-2`; pass `&path[..path.len().saturating_sub(2)]`, or split the "balance this node"
and "descend" steps so the last path element is not entered. Existing close tests stay green
under the truncation (both close a pane at depth 2 → `[]` → root-only, same expected values).
Add a fixture whose closing pane's sibling is a `Split` with a distinctive manual ratio.

## MEDIUM — CONFIRMED

### M1. Auto-rebalance silently overrides an explicit `pane.split { ratio }`
`src/app/api/panes.rs:176-186`. The new split node is itself on the ancestor chain of the new
pane, so with the toggle ON `balance_areas_along_path` overwrites the caller-supplied ratio
with the leaf-weighted value. `pane.split {"ratio":0.8}` returns success and yields 0.5.
An explicitly requested value should win over an implicit global preference (or the hook should
skip the newly created split when `params.ratio.is_some()`). No test covers `ratio: Some(_)`
with the toggle on.

### M2. `layout.balance` has no federation guard, contradicting the shipped doc
`src/app/api/layouts.rs:12-53`; doc claim at `docs/next/website/src/content/docs/socket-api.mdx`
("Mounted federated (remote) workspaces do not receive or participate in balance: it only ever
rebalances locally-owned tabs"). `resolve_layout_export_target` resolves any tab index; unlike
`handle_pane_split` (`panes.rs:80-84`) there is no
`federation::id::classify(...) == IdClass::Remote` check. Right-clicking a pane in a mounted
remote workspace → "Balance splits" mutates the local mirror's ratios, desyncing it from the
authoritative remote layout until the next resync. Either add the classify guard (returning an
error/no-op) or correct the doc sentence — as written the documentation is false.

### M3. The two context-menu dispatchers have already drifted
`src/app/input/modal.rs:889-198` (free fn) vs `:1313-1331` (via_api). For the same two new items:
- toggle: free fn does `state.auto_resize_splits = !…` with **no config persistence**; via_api
  calls `save_auto_resize_splits` (persist + reload).
- balance: free fn calls `tab.layout.balance_areas()` + `mark_session_dirty()` directly, emitting
  **no `LayoutUpdated` event**; via_api dispatches `Method::LayoutBalance`, which emits the event
  and schedules a session save.

Production today only reaches `apply_context_menu_action_via_api`
(`input/mod.rs:363`, `modal.rs:1145`); the free fn is reachable only from
`handle_context_menu_key`, which is used solely by tests. So this is not a live bug — but the
free fn is `pub(super)`/`pub(crate)`, not `#[cfg(test)]`, and its tests assert the divergent
behavior as correct. Any future re-route makes the toggle stop persisting silently. Either gate
the free-fn path behind `#[cfg(test)]` or make it delegate.

Direction check (both paths): label shows current state, action computes
`!state.auto_resize_splits` (not derived from the label), so clicking ": Off" turns it ON.
Correct in both, and the stale-`auto_resize_enabled`-snapshot case is harmless. Toggling does
not rebalance — asserted at `modal.rs:2286`.

### M4. Toggle is server-side runtime behavior with no API surface
`AppState.auto_resize_splits` changes the observable result of `pane.split`/`pane.close` for
**every** JSON-API client, but is only settable/readable through the TUI context menu, and
`socket-api.mdx` documents `layout.balance` without mentioning that split/close may now rewrite
ancestor ratios. This is the exact shape the CLAUDE.md runtime/client guardrail warns about
("new shared behavior that only works through the private TUI client socket"). At minimum
document the split/close side effect in `socket-api.mdx`.

### M5. Plan decision labels embedded in production comments
`src/app/api/panes.rs:96-100 ("D6"), :164-174 ("D3", "D6", "D4/D9"), :1723-1733 ("D3", "D6",
"D4/D9")`; `src/app/api/layouts.rs:9-11 ("D3", "D8"), :29-34 ("D6/H3")`. Violates
`~/.claude/rules/review-audit-self-decision.md` "Stable Code Artifacts" (no plan IDs / finding
codes in code comments). The comments are otherwise good — just drop the `(Dn)` tokens.

## LOW

- **L1** `balance_areas_along_path` returns `bool` that is discarded at both production hook
  sites (`panes.rs:186`, `:1786`); it exists only for tests. Either use it (skip the session
  save / event when nothing changed) or drop it. YAGNI.
- **L2** `ContextMenuState::items()` now allocates a `Vec` per call, and it is called per frame
  in `src/ui/menus.rs:299` plus twice per mouse move in `input/mouse.rs:1217,1223,1242`.
  Negligible in absolute terms; noted only because the `&'static [&'static str]` → `Vec` change
  was avoidable (a `match` returning one of N static slices plus one dynamic arm would do).
- **L3** Pane menu grew from 5–7 to 7–9 items. `mouse.rs:1223` clamps menu height to the screen;
  on a short terminal the tail items ("Close pane") may be clipped and unreachable by mouse.
  Pre-existing mechanism, newly more likely. Worth a manual check on a 12-row terminal.

## Verified-good (asked for explicitly)

- **Zoom guard is complete on all four entry points**: menu→API (`layouts.rs:35`), direct menu
  free fn (`modal.rs:895`), split hook (`panes.rs:104-110`, captured pre-split — the
  `split_focused_with_runtime` clear is correctly anticipated, and the tab located by
  `find_tab_index_for_pane(target_pane_id)` is the same tab as the later `target_tab_idx`),
  close hook (`panes.rs:1734-1746`, checked pre-close). No unguarded path found.
- **Equal-area math**: `equal_area_ratio` = `leaves_first / total` routed through
  `valid_split_ratio` (`layout.rs:580`), which clamps 0.1..=0.9 and maps non-finite → 0.5.
  The "free `set_ratio_at` doesn't clamp" hazard is handled — both new writers go through
  `equal_area_ratio`. Idempotent by construction (leaf counts are shape-invariant); recursion
  depth bounded by `MAX_LAYOUT_DEPTH`. No integer math in the solver; `f32::EPSILON` absolute
  comparison is fine at 0.1–0.9 magnitudes. `total` cannot be 0 (leaf counts ≥ 1).
- **Toggle OFF path**: both hooks are gated on `self.state.auto_resize_splits` before any tree
  read or write; nothing else in the diff changes split/close behavior. Default `false` in
  `UiConfig::default`, `AppState::test_new`, and the `App` constructor. Byte-identical, and
  `pane_split_without_auto_resize_leaves_existing_ratios_untouched` /
  `pane_close_without_auto_resize_leaves_root_ratio_untouched` prove it.
- **Power-of-two fixture swap is legitimate**, not a weakened test: on a lopsided chain the
  1-cell integer rounding at a leaf is multiplied by the full opposite dimension, so exact cell
  area equality is genuinely unrepresentable. The lopsided fixture is retained for the
  leaf-weighting and clamp tests, which are the ones that would actually catch a math bug.
  Caveat: at depth 3–4 the power-of-two fixture only exercises the 1:1 case (every ratio → 0.5),
  so **no test asserts approximate area equality on an unbalanced-leaf tree deeper than 2**.
  Consider a 3-leaf tree in a large `Rect` with a percentage tolerance.
- 1-vs-10 clamp degradation is tested (`balance_areas_1v10_leaves_clamps_with_documented_error`)
  and commented at `equal_area_ratio`. Federation gap comment + `socket-api.mdx` note both exist
  (see M2 for the accuracy problem). No new `unwrap()` in production code; no new platform code.

## Unresolved questions

1. Should an explicit `pane.split { ratio }` beat the auto-resize toggle (M1)? Product call.
2. M2: guard `layout.balance` against mounted remote workspaces, or soften the doc sentence?
3. `implementation-notes.md` records 4 (then 2, differing) full-suite failures as "pre-existing,
   UNVERIFIED BY CONTROLLER". Still unverified — I did not run the suite. Needs a clean-master
   baseline before ship.
4. Was `HERDR_UPDATE_API_SCHEMA=1 cargo test generated_protocol_schema_artifact_is_current`
   actually re-run? The schema JSON looks hand-consistent but I could not execute the check.
