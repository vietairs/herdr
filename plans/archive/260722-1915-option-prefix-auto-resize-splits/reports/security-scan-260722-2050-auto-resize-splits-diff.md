# Security scan: pane-auto-resize-splits diff (vs master)

Worktree: `/Users/hvnguyen/Projects/herdr-worktrees/pane-auto-resize-splits`
Diff: `git diff master` (16 files, +1683/-56). Read-only scan, no edits made.

## Result: no real findings

Checked all 5 categories from the task. Nothing rises to an actionable
security/correctness issue. Details below per category.

### 1. Secrets/credentials
None. `docs/next/api/herdr-api.schema.json` diff (+32/-0) is pure schema
scaffolding for the new `layout.balance` method (request variant + response
variant), `docs/next/website/src/content/docs/socket-api.mdx` is doc prose.
No literal tokens/keys/paths added anywhere in the diff.

### 2. Config-file write safety (`save_auto_resize_splits`)
`src/app/config_io.rs:95-101` (new fn) calls the **pre-existing**
`update_config_file` helper (`src/app/config_io.rs:4-34`, unchanged by this
diff) exactly the way `save_theme`/`save_sound`/`save_agent_border_labels`
already do. `upsert_section_bool` (in `src/config/`) is also untouched.

`update_config_file` itself does non-atomic `std::fs::write` (no tempfile+
rename, no symlink check, no permission hardening) — but this is the
project's existing settings-write pattern, reused verbatim, not a new
surface introduced here. No path traversal (path comes from
`crate::config::config_path()`, not from API input), no TOCTOU beyond what
every other settings toggle already has. Out of scope as "highest-value
target" claim doesn't hold: **the diff adds zero new lines to the actual
write path**, only a new caller of it.

### 3. `layout.balance` JSON-RPC method
`src/app/api/layouts.rs:251-293` (`handle_layout_balance`). Reuses
`resolve_layout_export_target` (unmodified, shared with `layout.export`/
`layout.apply`) for target resolution — same validated tab_id/pane_id
parsing and ownership checks as siblings. All lookups use `.get()`/
`and_then`/`let-else` returning `layout_not_found` on miss; no `.unwrap()`.
Zoomed tabs short-circuit to a no-op success (documented, intentional).
Registered in `federated_session_allows` (`src/api/mod.rs:188`) alongside
the other layout methods it mirrors — consistent, not a new privilege leak.

### 4. `unwrap`/`expect`/`panic!`/indexing/casts on API-input paths
None found in production code added by this diff.
- `src/layout.rs` new fns (`path_to_pane`, `balance_areas`,
  `balance_areas_along_path`, `find_path_to_pane`,
  `balance_split_ratios_along_path`, `equal_area_ratio`) — pure recursion/
  arithmetic, no indexing, no unwrap.
- `src/app/api/panes.rs` new hooks in `handle_pane_split`/pane-close path —
  all `if let Some(...)`, no unwrap.
- `src/app/input/modal.rs` — the one `panic!("path does not resolve to a
  split")` (line 1432) is in a `#[cfg(test)]` test helper (`ratio_at`,
  confirmed at `modal.rs:1373` `mod tests {`), not production code.
- `equal_area_ratio` (`src/layout.rs`) clamps via existing
  `valid_split_ratio` (0.1..=0.9) — no float cast panics, documented
  accepted-degradation for extreme leaf-count skew.

### 5. DoS via unbounded recursion in `balance_areas`/`balance_areas_along_path`/`path_to_pane`
Recursion depth in all three is bounded by the *actual* current layout
tree depth, same as pre-existing recursive fns in the same file
(`remove_pane`, `count_panes`, `split_rect`-adjacent tree walks). This diff
does not add a new way to grow tree depth — `handle_pane_split`
(`src/app/api/panes.rs:40`) has no depth/pane-count cap, but that gap is
**pre-existing** (confirmed: `MAX_LAYOUT_DEPTH` in
`src/app/api/layouts.rs:16` is only enforced on layout *import*
validation at layouts.rs:580/600, not on interactive/API pane splits).
The new balance functions inherit whatever depth an attacker could already
reach via repeated `pane.split` calls before this change; they don't widen
that surface. Not a new finding, but worth flagging separately if the team
wants a split-depth cap — that's pre-existing scope, not introduced here.

## Unresolved questions
- None blocking. One pre-existing gap noted (no depth/pane-count limit on
  interactive `pane.split`) is out of this diff's scope per task framing,
  but the team may want to track it separately since `layout.balance`'s
  recursion depth now depends on it.

Status: DONE
Summary: No secrets, no unsafe config-write regression (new toggle reuses the existing non-atomic write helper verbatim), no unwrap/panic/indexing on API-input paths, and `layout.balance` + the split/close auto-rebalance hooks validate targets and recurse only as deep as the tree already legally gets — the one recursion-depth question traces to a pre-existing lack of a pane-split depth cap, not to code this diff added.
