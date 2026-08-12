# Ship-gate — balance splits + auto-resize toggle

Branch `feat/pane-auto-resize-splits`, 1778 insertions / 56 deletions, 16 files.
45 new test fns. Suite: 3000 passed / 2 failed (both confirmed flakes). `cargo fmt --check` clean.

## Plan vs shipped — all 10 acceptance criteria

| # | Criterion | Verdict |
|---|---|---|
| 1 | Menu shows "Balance splits" + "Auto-resize splits: On/Off" | PASS |
| 2 | Balance equalizes leaf AREAS, depths 2-4, +/-1 cell | PASS (see gap G1) |
| 3 | Balance while zoomed = no-op | PASS — reviewer confirmed all 4 entry points |
| 4 | Toggle ON + split -> ancestor chain only | PASS |
| 5 | Toggle ON + close -> ancestor chain only | PASS **only after H1 fix** |
| 6 | Toggle OFF byte-identical to pre-feature | PASS — reviewer confirmed |
| 7 | Toggle round-trips config + survives reload | PASS |
| 8 | API schema artifact regenerated | PASS |
| 9 | 1-vs-9 = exactly 0.1; 1-vs-10 clamps, documented | PASS |
| 10 | Suite green | PASS (2 pre-existing flakes, verified) |

Locked decisions D1-D9 all honored. D2 (equal AREAS) was a user override of predict's
recommendation and was delivered as specified.

## Predicted vs actual (predict report, 5 personas)

| Prediction | Outcome |
|---|---|
| "Riskiest thing = `items()` combinatorial blast radius, not the layout math" | **CORRECT.** 4->8 arm explosion was real; Vec-builder refactor avoided it. |
| Test churn MED not HIGH (label-based, not index-based) | **CORRECT** — no index churn materialized. |
| Zoom rebalance would corrupt ratios | **CORRECT, and worse than predicted** — TDD found `split_focused_with_runtime` CLEARS `zoomed`, so a post-split check silently never fires. |
| Equal-area needs new solver code | **CORRECT** — ~518 lines in layout.rs. |
| Federation gap degrades gracefully, defer | **ACCEPTED** — local-only v1, doc corrected (M2). |
| Snapshot-storm risk lower than briefed (coalescing exists) | **N/A** — LayoutUpdated never added to the federation trigger list. |
| **MVP rec: ship one-shot only, defer the toggle** | **USER OVERRODE — and predict was arguably right.** The single HIGH bug (H1) lived exclusively in the toggle's close hook. One-shot-only would not have contained it. Recorded honestly, not as a reason to undo the decision. |

## Findings disposition

- **H1 HIGH** — close hook replayed a pre-close path on the post-collapse tree, destroying a
  manual ratio in an unrelated subtree. **FIXED**, bug proven by revert (failed 0.5) then
  restore (passed). New split-sibling fixture added; old fixture structurally could not catch it.
- **M5** plan labels in production comments — FIXED (5 blocks).
- **M2** false federation claim in socket-api.mdx — FIXED (doc now accurate).
- **M3** dispatcher drift — FIXED (free fn gated `#[cfg(test)]` after caller audit).
- **M1** auto-rebalance overrides explicit `pane.split {ratio}` — **ACCEPTED, not fixed**
  (opt-in toggle; user deliberately enabled it).
- **M4** toggle not API-exposed — **ACCEPTED, not fixed** (locked D5: config-only).
- Security scan: **no findings.** No secrets; config write path reused verbatim; no
  panic/unwrap reachable from socket input. Pre-existing `pane.split` depth cap gap noted, not
  introduced here.

## Independently verified by controller (not taken on report)

- Flaky baseline: both named tests PASS serially (`--test-threads=1`); failing set differs
  every run; failure shapes are AddrInUse / random-ID / mutex-poison. **Confirmed pre-existing.**
- Zero NEW production `unwrap()`. The one in `src/layout.rs:171` is identical on master.
- H1 traced independently in source before authorizing the fix.
- PROTOCOL_VERSION correctly not bumped (v0.7.5 already ships 17; wire.rs untouched).

## Known gaps shipping as-is

- **G1** No depth>=3 area assertion on an UNBALANCED-leaf tree. Exact cell-area equality is
  unrepresentable there (integer rounding), so coverage uses power-of-two fixtures. The
  leaf-weighting and clamp cases DO use lopsided fixtures. Legitimate, but the gap is real.
- **G2** Mounted remote clients show stale ratios after a balance (federation resync omits
  LayoutUpdated). Locked local-only v1; now documented truthfully.
- **G3** `pane.split` has no depth/pane-count cap (pre-existing).

## SHIP BLOCKER — not a code issue

`feat/pane-auto-resize-splits` **cannot be pushed**. Master (5ec2a10b, the v0.7.5 upstream
merge) carries 385MB + 193MB + 193MB vendored static libs; GitHub's hard limit is 100MB/file.
Last-pushed commit b5cb8ce8 is verified clean; remote HEAD still sits there. Pre-existing,
unrelated to this work. No PR is possible without resolving fork history first.

## Verdict

Code is **PASS** on every agreed criterion, with one HIGH found and fixed under proof and two
mediums consciously accepted. Delivery stops at a local commit, not a PR, due to the push
blocker above.

## Unresolved questions

1. Push blocker: land locally / rebase onto clean b5cb8ce8 / strip history (filter-repo or LFS)?
2. Close G1 with a tolerance-based area assertion on an unbalanced tree, or accept as-is?
3. Revisit M1 (explicit split ratio being overridden) if the toggle ever becomes API-controllable?
