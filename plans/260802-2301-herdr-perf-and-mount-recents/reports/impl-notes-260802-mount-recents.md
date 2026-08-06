# impl notes: mount-remote-workspace recents

What: Put `recent_remote_mount_targets: Vec<String>` under `UiConfig` (`[ui]` in config.toml), not `RemoteConfig`.
Why: `RemoteConfig` (src/config/model.rs:867) is Deserialize-only shared connection policy; recents is TUI presentation/convenience state per CLAUDE.md's runtime/client boundary, and `[ui]` already holds the other client-local prefs (theme, sound, agent_panel_sort) saved the same way.
Evidence: src/app/config_io.rs's save_theme/save_agent_panel_sort precedent; scout report's persistence_pattern section.
Reversibility: easy — rename the config key/section if a future non-TUI client needs recents synced server-side instead.

What: `RemoteMountState` lost its `#[derive(Default)]` and gained a manual `impl Default`.
Why: added a `recents_list: MenuListState` field, and `MenuListState` itself has no `Default` impl anywhere in the codebase (every existing use is `MenuListState::new(0)`); adding `#[derive(Default)]` to `MenuListState` would be a wider blast-radius change than this feature needs.
Evidence: `grep MenuListState::new` showed zero `Default::default()` uses of it.
Reversibility: easy — could add `impl Default for MenuListState` later if another caller wants it.

What: `select_recent_remote_mount_target` lives on `AppState` (state.rs), not `App` (remote_mount.rs).
Why: the mouse click handler for `Mode::MountRemoteWorkspace` (src/app/input/mouse.rs) is `impl AppState`, not `impl App` — filling `name_input` from a click is pure state, so putting the method on `AppState` lets the mouse handler call it directly instead of adding a `request_*` flag that would need draining in both `src/app/mod.rs` and `src/server/headless.rs` (the exact trap flagged in the task).
Evidence: read mouse.rs:69 `impl AppState`, and the existing mount/cancel button click arms in the same block already flip `request_submit_remote_mount`/clear `remote_mount` directly on `self` (AppState), confirming button/list interaction in this dialog is handled at the AppState layer.
Reversibility: easy — pure function move, no signature change visible to other modules.

What: dialog popup height only grows when `recent_remote_mount_targets` is non-empty (0 extra rows, 1 heading + up to 5 rows).
Why: requirement said "only shown when recents_list items are non-empty so the dialog stays compact" (scout sketch item 5); reused the same `Layout::vertical` row array (now 7 elements) for both `render_remote_mount_overlay` and the new `remote_mount_recent_at` hit-test so the two can never disagree on row math.
Evidence: `remote_mount_popup_grows_only_when_recents_are_present` test (src/ui/remote_mount.rs).
Reversibility: easy — purely additive layout constant.

What: mouse-move hover-follow for the recents list was NOT implemented (only click-to-select and Up/Down keys).
Why: `Mode::MountRemoteWorkspace`'s mouse dispatch (src/app/input/mouse.rs:212-221) already early-returns `None` for every `MouseEventKind` except `Down(Left)` for this mode; wiring hover-follow would mean loosening that guard for this one mode, a larger and riskier change than the task's time budget covers. Requirement's "mouse-first: clickable rows" is satisfied by click; keyboard Up/Down covers keyboard navigation.
Evidence: same guard block, unchanged in this diff.
Reversibility: easy, additive — a follow-up can add a `Moved` branch scoped to `Mode::MountRemoteWorkspace` that calls `recents_list.hover(...)` without touching this diff's behavior.

What: config-array serialization uses simple `"{target}"` quote-wrapping, not full TOML escaping.
Why: matches this file's own existing precedent (`save_theme`'s `format!("\"{name}\"")`, `save_toast_delivery`'s literal `"\"off\""` etc. — none of `config_io.rs`'s writers do general TOML string escaping); targets are `user@host[:port]` strings that structurally cannot contain `"` or `\`.
Evidence: read every `save_*` fn in src/app/config_io.rs before writing `save_recent_remote_mount_targets`.
Reversibility: easy — swap in `toml::Value`-based serialization later if a differently-shaped value ever needs it.

Unresolved questions:
- None blocking. Optional follow-up: mouse hover-follow highlighting for the recents list (see note above).

---

## Adversarial-review fixes (260803)

Source: `plans/260802-2301-herdr-perf-and-mount-recents/reports/review-260802-mount-recents.md`.
Worktree `.claude/worktrees/mount-recents-perf`, branch `feat/mount-recents-and-perf`, uncommitted.

### B1 — TOML escaping of persisted targets (blocker)

What: `save_recent_remote_mount_targets` (`src/app/config_io.rs`) now serializes the list with `toml::Value::from(targets.to_vec()).to_string()` instead of hand-quoting each element with `format!("\"{target}\"")`. The doc comment claiming targets can never contain `"` is gone.
Why: reverses the earlier note above ("simple quote-wrapping matches this file's precedent") — that reasoning was wrong. `save_theme`'s name comes from an enumerated UI list; a mount target is arbitrary user text. `DOMAIN\user@host` is a legal OpenSSH destination and an ssh_config `Host` alias may contain a quote. One unescaped `\` or `"` makes `config.toml` unparsable, and `Config::load` answers a top-level parse error by returning `Config::default()` for the *whole* file — silently resetting theme, keybinds, sidebar, `[remote]`, `[experimental]` on the next start, with no self-healing (`update_config_file` is line-based and never parses).
Evidence: new test `record_successful_remote_mount_target_escapes_quotes_and_backslashes` (`src/app/mod.rs`) records `we"ird@host` and `DOMAIN\user@host`, then re-parses the written file as a full `Config` and asserts both targets round-trip byte-for-byte and an unrelated `[ui] accent` key survives. `toml` 0.8 is already a direct dependency; no new dep.
Reversibility: easy — one expression; the on-disk format is unchanged for targets that need no escaping.

### M1 — recents rows clamped to the layout rect (major)

What: new shared helper `remote_mount_visible_recents_rows(list_rect, recents_count)` (`src/ui/remote_mount.rs`) derives the row budget from `list_rect.height.saturating_sub(1)`, and both the render loop and `remote_mount_recent_at` use it instead of `remote_mount_recents_rows(recents_count)`.
Why: `centered_popup_rect` clamps the popup to `area.height - 2`, so on a short terminal ratatui shrinks the recents `Length` row while both loops kept assuming the full-size list. At 12 terminal rows with 5 recents, recent #3 painted onto the mount/cancel button row and — because `remote_mount_recent_at` is consulted *before* `remote_mount_button_rects` in `src/app/input/mouse.rs` — a click on "mount" selected a recent instead of submitting, making the dialog's primary action unreachable by mouse.
Evidence: new test `recents_never_overlap_the_buttons_at_a_clamped_popup_height` sweeps 12/13/14/16/40 terminal rows with 5 recents and asserts (a) the last drawn row stays inside `list_rect`, (b) neither button row hit-tests as a recent, (c) the last visible row is still clickable. Verified non-vacuous: reverting just the hit-test to the state-derived count reproduces the review's exact failure — `left: Some(2)` at 12 rows.
Reversibility: easy — one helper, two call sites.

### M2 — real "nothing selected" state (major)

What: `RemoteMountState.recents_list: MenuListState` became `recents_highlighted: Option<usize>`, with `highlight_next_recent`/`highlight_prev_recent` on `RemoteMountState`. `None` is the open state; first `Down` → index 0; `Up` from `None` → index 0 (matches the sibling menus' saturating `move_prev` and the pre-existing `up_key_clamps_at_the_first_recent` expectation). Render compares `recents_highlighted == Some(idx)`. The manual `impl Default` added earlier is now a `#[derive(Default)]` (all fields default-able again), which also clears a clippy `derivable_impls` error.
Why: `MenuListState.highlighted: usize` cannot express "nothing picked yet", so the top recent rendered with the accent background while `name_input` was empty (Enter on that visibly-selected row errors "enter at least one target"), and `move_next` from 0 skipped index 0 entirely. Reusing `MenuListState` here was the wrong primitive; `Option<usize>` is the smallest fix and leaves every sibling menu untouched.
Evidence: FIXED the test that codified the defect — `down_key_navigates_recents_and_fills_the_input` (asserted first `Down` → index 1) is replaced by `down_key_selects_the_most_recent_target_first` (index 0, then 1, then clamps). New `a_freshly_opened_dialog_highlights_nothing`. `up_key_clamps_at_the_first_recent` and `up_down_keys_are_a_noop_with_no_recents` unchanged and still pass.
Reversibility: easy — field type change local to the dialog; no wire/API surface.

### M3 — persist without a full config reload (major)

What: `save_recent_remote_mount_targets` no longer calls `apply_config_from_disk(false)`. `record_successful_remote_mount_target` (`src/app/remote_mount.rs`) builds the candidate list, returns early when it equals the current one, and only then assigns + persists.
Why: the in-memory list is already updated before persisting, so the reload bought nothing while dragging in unrelated live side effects on *every* successful mount: `apply_live_config` resets `agent_panel_scroll = 0` (the sidebar jumps to the top mid-work), re-clamps `sidebar_width`, clears selection when `copy_on_select = false`, and sets `config_reloaded_from_disk`, which `src/server/headless.rs` consumes on the next client input to run a second full `reload_server_config` including `apply_keybindings`. Re-mounting the target already at index 0 paid the whole read/format/write/re-parse cycle to rewrite byte-identical content.
Evidence: new test `record_successful_remote_mount_target_skips_the_write_when_nothing_changed` (`src/app/mod.rs`) resets the file after the first record and asserts a repeat record of the same target leaves the key absent. `record_successful_remote_mount_target_persists_most_recent_first` still passes, confirming the in-memory list stays live without the reload.
Reversibility: easy — re-adding the reload call is one line, but it should not come back.

### Federation resync-index leak on remote-initiated teardown (separate concern)

What: `handle_federation_mount_ended` (`src/app/api/workspaces.rs`) now calls `purge_remote_resync_pane_index_for_workspaces(&closing_ids)` alongside the existing splits/closes purges.
Why: the purge (added by commit `db671fbc`) had exactly one production call site — the locally-initiated close path at `handle_workspace_close`. A remote-initiated teardown (LinkClosed / Faulted) removed the workspaces without it, leaking one `remote_resync_pane_index` entry per mount-time pane and leaving `remote_pane_id -> dead PaneId` mappings that a later remount to the same host could route a resync pane-removal at.
Evidence: new test `federation_mount_ended_purges_remote_resync_pane_index_for_its_workspaces` builds a real mount through `handle_federation_mount_ready`, seeds one entry for an unrelated still-live workspace, drives the mount-ended teardown, and asserts only the unrelated entry survives. Verified non-vacuous: with the purge call commented out the test fails with the leaked `"r:remote-host#s1:p1"` entry still present.
Reversibility: easy — mirrors the existing workspace-close call site exactly.

### Minors

What: `REMOTE_MOUNT_RECENTS_MAX_ROWS` (`src/ui/remote_mount.rs`) is now `= crate::app::state::RECENT_REMOTE_MOUNT_TARGETS_CAP` instead of a hardcoded `5` that only *commented* that it tracked the state cap.
Why: raising the state cap to 10 would have left the popup showing 5 rows while Up/Down navigated to index 9 — highlight invisible, input filled from an off-screen entry.
Reversibility: trivial.

What: the `recents_highlighted` doc comment no longer claims mouse-hover support.
Why: only click is wired; `MouseEventKind::Moved` has no arm for `Mode::MountRemoteWorkspace`. Hover itself stays unimplemented (explicitly out of scope) — the comment now matches the code.
Reversibility: trivial.

What: added `ui.recent_remote_mount_targets` to `docs/next/website/src/data/config-reference.json`.
Why: `scripts/config_reference_check.py` (run by `just release-docs-check`) flagged it as present in `src/config` but missing from the reference, blocking release-docs finalization.
Evidence: the check no longer lists the key; the 6 remaining entries (`remote.accept_clipboard_writes`, `ui.auto_resize_splits`, `ui.prompt_new_workspace_name`, `ui.sidebar.agents.row_gap`, `ui.sidebar.spaces.row_gap`, `ui.sidebar_start_collapsed`) are pre-existing on this fork and out of scope.
Reversibility: trivial, docs-only.

### Explicitly not done (out of scope, noted)

- **m3 — ssh identities in `config.toml` rather than `state_dir()`, and no clear/disable affordance.** A product decision (where history lives, whether recording is opt-out) with a user-visible config-key migration attached; not a review fix.
- **m2 — actual mouse-hover support for the recents list.** Would require loosening the `Mode::MountRemoteWorkspace` mouse guard, which currently early-returns for every kind except `Down(Left)`. The doc comment was corrected instead.
- **m7 — freezing recents navigation while `submitting`.** Behavior question (does a pick during "mounting…" queue or discard), not a defect with a single correct answer.
- **m6 — the unrelated `assert_eq!` reflow at `src/app/mod.rs:2351`.** Diff noise only; left alone rather than adding another unrelated hunk.

### Validation

- `cargo fmt` clean.
- `cargo check --bin herdr` and `cargo check --tests`: no new warnings.
- `cargo clippy --all-targets -- -D warnings`: 13 errors, all pre-existing (`cli.rs` ×3, `workspace.rs`, `api/client.rs` ×3, `federation/id.rs`, `protocol/mod.rs`, `pane/osc.rs`, `api/workspaces.rs` test module ×2, `input/mouse.rs` test module, `pane_source.rs`). Two errors this work *introduced* (`derivable_impls` on `RemoteMountState`, `int_plus_one` in the new geometry test) were fixed before finishing; none remain inside the diff.
- `cargo test --bin herdr -- --test-threads=4`: 3187 pass. The only failures are the known parallel flakes (`AddrInUse` in `server::headless`, `pane_graphics_stream inactive_owner`, `plugins manifest_action_invoke`), all green when re-run with `--test-threads=1`.
- `python3 scripts/config_reference_check.py`: this change's key resolved; 6 pre-existing fork gaps remain.

Unresolved questions:
- Review's Q2/Q3 stand: should recents live in `state_dir()` rather than `config.toml`, and is there meant to be a "clear recents" affordance? Both are the m3 product decision above.
