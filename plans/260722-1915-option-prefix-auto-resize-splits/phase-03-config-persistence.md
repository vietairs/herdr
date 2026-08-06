# Phase 3 — config persistence for the auto-resize toggle

Status: pending | Owns: `src/config/model.rs`, `src/app/config_io.rs`, `src/app/mod.rs`
Depends on: Phase 2 (needs `AppState.auto_resize_splits` field name to exist) | Parallel group: B

## Context

- `UiConfig` struct field block, `pane_borders`/`pane_gaps`/`show_agent_labels_on_pane_borders`
  at `src/config/model.rs:807-812`, doc-commented, defaults at `:1009-1011`
  (`pane_borders: true, pane_gaps: true, show_agent_labels_on_pane_borders: false,` — note this
  default block differs slightly from the `AppState` default block cited in the caller's ground
  truth, `pane_gaps: false` there — the two default sources currently DISAGREE for `pane_gaps`;
  not this feature's problem to fix, just don't copy the disagreement pattern).
- Real toggle-and-persist precedent: `save_agent_border_labels(&mut self, enabled: bool)`
  (`src/app/config_io.rs:82-93`) — reads `[ui]` section, calls
  `crate::config::upsert_section_bool(content, "ui", "show_agent_labels_on_pane_borders",
  enabled)` inside `self.update_config_file(...)`, then `self.apply_config_from_disk(false)` on
  success to re-sync `AppState` from the freshly-written file (single source of truth: disk,
  never mutate `AppState.auto_resize_splits` directly from the save fn — let the reload do it,
  same as the precedent).
- `update_config_file` (`config_io.rs:4-36`) — generic: reads current config file text, applies
  an `FnOnce(&str) -> String` transform, writes back, handles I/O errors via
  `self.state.config_diagnostic` + `config_diagnostic_deadline` toast. Reuse as-is, no changes
  needed to this fn.
- `upsert_section_bool(content: &str, section: &str, key: &str, value: bool) -> String`
  (`src/config/io.rs:565`) — reuse as-is.
- Sync-from-disk block: `apply_config_from_disk` (`src/app/mod.rs:1411`), the field-copy lines
  for `pane_borders`/`pane_gaps`/etc are at `:1505-1512`. Add the new field's sync line in the
  same block.

## Requirements

1. `src/config/model.rs`: add `pub auto_resize_splits: bool` field to `UiConfig` next to
   `pane_borders`/`pane_gaps` (`:807-812` area), doc comment: "Auto-rebalance split areas after
   each pane split/close. Default: false." Add default `auto_resize_splits: false,` to the
   struct's `Default` impl (`:1009-1011` area).
2. `src/app/config_io.rs`: add `pub(super) fn save_auto_resize_splits(&mut self, enabled: bool)`
   mirroring `save_agent_border_labels` exactly (same shape: `update_config_file` +
   `upsert_section_bool(content, "ui", "auto_resize_splits", enabled)` +
   `apply_config_from_disk(false)` on success).
3. `src/app/mod.rs`: add `self.state.auto_resize_splits = config.ui.auto_resize_splits;` in the
   `apply_config_from_disk` sync block (`:1505-1512` area), next to the `pane_borders`/
   `pane_gaps` lines.

## Files to modify

- `src/config/model.rs`, `src/app/config_io.rs`, `src/app/mod.rs`. No other files.

## Step-by-step (TDD)

1. Add failing test in `src/config/model.rs`'s existing test mod (near `:1240-1255`, same
   pattern as `pane_borders`/`pane_gaps` round-trip tests):
   - `default_config_has_auto_resize_splits_off` — assert `!default_config.ui.auto_resize_splits`.
   - `toml_round_trip_reads_auto_resize_splits_true` — parse a TOML fixture with
     `auto_resize_splits = true` under `[ui]`, assert `config.ui.auto_resize_splits`.
2. Implement Requirement 1. Run
   `ZIG=~/.local/zig-0.15.2/zig cargo test --lib config::model:: -- --test-threads=4`.
3. Add failing integration test in `src/app/config_io.rs` (or wherever `save_agent_border_labels`
   is tested — grep `fn save_agent_border_labels` test usage first to find the exact test
   pattern/harness, e.g. temp `HERDR_CONFIG_PATH`):
   - `save_auto_resize_splits_writes_ui_section_and_reloads_state` — call
     `app.save_auto_resize_splits(true)`, assert the written config file content contains
     `auto_resize_splits = true` under `[ui]`, and `app.state.auto_resize_splits == true` after
     the call (proves the reload-from-disk sync round-trips correctly).
   - `save_auto_resize_splits_false_after_true_clears_it` — toggle twice, assert final state.
4. Implement Requirements 2-3. Re-run tests until green.
5. Full test suite for touched files:
   `ZIG=~/.local/zig-0.15.2/zig cargo test --lib config:: app::config_io:: app::mod::
   -- --test-threads=4` (adjust module paths if `cargo test` module filtering differs — fall
   back to `cargo test -- --test-threads=4` full run if filtered run doesn't match).

## Risks / rollback

- Risk: `[ui]` TOML section doesn't exist yet in a user's config file (`update_config_file`
  reads `""` on missing file, `upsert_section_bool` must create the section) — this is already
  handled by the existing `upsert_section_bool` (used identically by `save_agent_border_labels`
  today) — no new risk, just confirm the existing fn's "add missing section" test
  (`config/io.rs:726`) still covers this shape.
- Risk: `pane_gaps` default mismatch (config.rs `true` vs AppState `false`, both cited above) is
  a pre-existing bug unrelated to this feature — do NOT fix it here (scope creep), just don't
  replicate a mismatch for `auto_resize_splits` (use `false`/`false` consistently in both
  layers).
- Rollback: revert this file set only; Phase 2's field stays declared but unused/unsynced
  (defaults to `false` from Phase 2's own `Default` impl, harmless no-op state) if this phase is
  reverted independently.
