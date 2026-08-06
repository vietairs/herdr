# Fix: H1 / M5 / M2 / M3 code-review findings

Worktree: `herdr-worktrees/pane-auto-resize-splits`. Files touched: `src/app/api/panes.rs`,
`src/app/api/layouts.rs`, `src/app/input/modal.rs`, `docs/next/website/src/content/docs/socket-api.mdx`.

## FIX 1 — H1 (blocking): stale-path over-descent into promoted sibling

Root cause confirmed exactly as reported: `path_to_pane` is captured PRE-close; `remove_pane`
collapses the closing pane's parent and promotes the sibling into its slot; replaying the full
path afterward descends one node too far and rewrites the promoted sibling's ratio.

**New fixture** `app_with_split_sibling_of_closing_pane` (panes.rs) builds
`root H(0.7){ Pane1, V(0.6){ Pane2, H(0.25){ Pane3, Pane4 } } }` — sibling of the closing pane
is a `Split`, not a leaf, so over-descent lands on a real node (existing fixture's sibling was
always a leaf, masking the bug).

**Failing test BEFORE fix** (`pane_close_auto_rebalance_preserves_manual_ratio_in_split_sibling_subtree`,
truncation temporarily reverted):
```
thread '...' panicked at src/app/api/panes.rs:4328:9:
manual ratio in the unrelated promoted-sibling subtree must survive the parent collapse: 0.5
test result: FAILED. 0 passed; 1 failed
```

**Passing AFTER fix**:
```
test app::api::panes::tests::pane_close_auto_rebalance_touches_only_collapsed_ancestor_chain ... ok
test app::api::panes::tests::pane_close_auto_rebalance_preserves_manual_ratio_in_split_sibling_subtree ... ok
test app::api::panes::tests::pane_close_auto_rebalance_is_noop_while_tab_zoomed ... ok
test result: ok. 3 passed; 0 failed
```

**Placement: hook site (`panes.rs`), not `balance_areas_along_path`.** The -2 truncation is
specific to the close-hook's semantics (path captured pre-collapse, replayed post-collapse). The
split hook's path (`path_to_pane(new_pane.pane_id)`) addresses a live, uncollapsed tree and must
NOT be truncated. Putting the truncation inside the general function would silently corrupt that
caller (and any future correct caller) instead of only fixing the one caller whose contract is
violated. Fix: `let ancestor_path = &path[..path.len().saturating_sub(2)];` before
`tab.layout.balance_areas_along_path(ancestor_path)`, with a comment explaining the L/L-1/L-2
depth reasoning. `layout.rs` untouched.

## FIX 2 — M5: plan-decision labels stripped

Removed `(D3)`, `(D6)`, `(D4/D9)`, `(D8)`, `(D6/H3)` from `panes.rs` (4 sites: split-hook zoom
comment, split-hook rebalance comment, close-hook rebalance comment) and `layouts.rs` (2 sites:
`handle_layout_balance` doc comment, zoom no-op comment). Explanations kept, labels dropped.
`git diff master | grep -n "(D[0-9]"` on the whole diff now returns nothing.

## FIX 3 — M2: false doc claim corrected

`socket-api.mdx` claimed `layout.balance` gets a federation guard ("Mounted federated (remote)
workspaces do not receive or participate in balance"). No such guard exists. Replaced with an
accurate statement: ratio-only changes (`layout.balance`, `layout.set_split_ratio`, auto-resize)
don't trigger federation resync per the event-kind gate in `client.rs:824-829`, so a mounted
remote mirror can show stale ratios until an unrelated structural event forces a resync. No code
touched; `src/remote/federation/**` untouched.

## FIX 4 — M3: dispatcher drift

`apply_context_menu_action` (free fn, `AppState`-only) has no access to `App`/config/event_hub
so it structurally cannot call `save_auto_resize_splits` or emit `LayoutUpdated` — delegating to
`_via_api` isn't a same-signature swap. Verified its only callers are `handle_context_menu_key`
(already `#[cfg(test)]`) and this module's `#[cfg(test)] mod tests` (grep confirmed, no other
call sites). Gated the free fn itself `#[cfg(test)]` with a comment stating production always
uses `_via_api`. `cargo test context_menu` (25 tests) still compiles and passes, confirming no
non-test caller exists.

## Verification

- `cargo test pane_close_auto_rebalance` / `cargo test layout` (64 tests) / `cargo test
  context_menu` (25 tests): all pass.
- `cargo fmt --check` on the 3 touched Rust files: clean after `cargo fmt` reformatted one
  rustfmt-mandated line wrap in the new test.
- Full suite (`ZIG=~/.local/zig-0.15.2/zig cargo test -- --test-threads=4`), run twice:
  run 1: 2983 passed / 19 failed; run 2: 2999 passed / 3 failed. The larger run's extra 16
  failures are `PoisonError` cascades from a shared mutex in `src/integration/env.rs:159`
  poisoned by `manifest_action_invoke_injects_plugin_paths` panicking (temp-dir path mismatch) —
  pure test-environment contention under `--test-threads=4`, not a regression. Both runs'
  survive-every-run failures (`manifest_action_invoke_injects_plugin_paths`,
  `generated_workspace_ids_are_short_base32_handles`, `server::autodetect::...stale_socket`)
  match the documented flaky baseline. No failure in either run touches `panes.rs`, `layouts.rs`,
  `modal.rs`, or `layout.rs`.

## Deviations

None from the assigned scope. M1/M4 left untouched per instructions.

## Unresolved questions

None.

Status: DONE
Summary: H1 reproduced with a new split-sibling fixture, fixed by truncating the close-hook's
replayed path by 2 at the call site; M5/M2/M3 fixed as specified; narrow and full suites clean
apart from pre-existing documented flaky tests.
