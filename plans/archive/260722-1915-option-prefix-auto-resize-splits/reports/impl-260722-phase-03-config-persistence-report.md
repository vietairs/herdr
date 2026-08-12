# Phase 3 impl report — config persistence for auto-resize toggle

## Files changed

- `src/config/model.rs`: added `pub auto_resize_splits: bool` to `UiConfig` (after
  `show_agent_labels_on_pane_borders`), doc comment per spec, `Default` impl sets `false`.
- `src/app/config_io.rs`: added
  `pub(super) fn save_auto_resize_splits(&mut self, enabled: bool)`, mirrors
  `save_agent_border_labels` exactly: `update_config_file("auto-resize splits", ...)` +
  `upsert_section_bool(content, "ui", "auto_resize_splits", enabled)` +
  `apply_config_from_disk(false)` on success.
- `src/app/mod.rs`:
  - replaced placeholder `auto_resize_splits: false` (was :681) with
    `auto_resize_splits: config.ui.auto_resize_splits` in the `AppState` construction literal.
  - added `self.state.auto_resize_splits = config.ui.auto_resize_splits;` in
    `apply_live_config`'s `[ui]` sync block, next to `pane_borders`/`pane_gaps`.

## Tests added

- `src/config/model.rs`: `default_config_has_auto_resize_splits_off`,
  `toml_round_trip_reads_auto_resize_splits_true`.
- `src/app/mod.rs` (`tests` mod): `save_auto_resize_splits_writes_ui_section_and_reloads_state`
  (asserts written file contains `[ui]` + `auto_resize_splits = true`, preserves unrelated
  `onboarding = false` key, and `app.state.auto_resize_splits` flips via the reload path — not
  mutated directly), `save_auto_resize_splits_false_after_true_clears_it` (toggle round-trip).
  No dedicated test module exists in `config_io.rs` itself (precedent `save_agent_border_labels`
  has none either) — round-trip tests live in `app/mod.rs`'s existing `config_env_lock`/
  `temp_config_path` harness, same as `save_agent_panel_sort`/`save_pane_history_persistence`.

## Validation

`ZIG=~/.local/zig-0.15.2/zig cargo test auto_resize -- --test-threads=4` → all 5 new tests pass
(plus pre-existing `state.rs::app_state_default_has_auto_resize_splits_off` from phase 2, which
this phase's `config.ui.auto_resize_splits` wiring now actually feeds).

Full suite: `ZIG=~/.local/zig-0.15.2/zig cargo test -- --test-threads=4` → 2981 passed, 2 failed:
`app::api::plugins::tests::manifest_action_invoke_injects_plugin_paths` and
`workspace::tests::generated_workspace_ids_are_short_base32_handles`. Neither touches my owned
files; both pass in isolation on this branch and pass on baseline (`git stash` + rerun) too —
confirmed pre-existing parallel-run flakes (shared global config path / probabilistic short-ID
collision under `--test-threads=4`), not caused by this change.

`cargo check` (no `--tests`): compiles clean, only pre-existing dead-code warnings from
unfinished phases 1/4/5 (`layout.rs` balance fns, federation `map_out`/`CLIPBOARD`) plus one new
expected one: `save_auto_resize_splits` itself is unused outside tests until phase 5 wires menu
dispatch — same shape as any not-yet-dispatched save fn, not a regression.

## Deviations

None from the phase spec.

## Unresolved questions

None.

Status: DONE
Summary: `UiConfig.auto_resize_splits` (default false) added with save/reload wiring mirroring
`save_agent_border_labels`; all required tests pass; two unrelated pre-existing flaky tests seen
under full-suite parallel run, confirmed not caused by this change.
