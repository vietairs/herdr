# Group B — Rust app/ conflict resolution (upstream v0.8.0 merge)

Worktree: `/Users/hvnguyen/Projects/herdr/.claude/worktrees/merge-upstream-v0.8.0`
Scope: 7 files, all conflicts integrated (both sides kept), no side dropped outright.

## src/app/mod.rs (4 hunks)

1. **`TerminalInputTarget` area (~line 251).** Fork added `SessionPersistencePolicy`
   enum (federation: forces classic session persistence off for an in-proc
   federated display). Upstream added `TerminalInputContext` enum +
   `InputSourceId` type alias + `LOCAL_INPUT_SOURCE` const (multi-source input
   routing refactor). Unrelated, non-overlapping. Resolved: kept both, fork's
   block first then upstream's block.

2. **`App::new` UI field init (~line 739).** Fork added `auto_resize_splits`
   field, upstream added `pane_scrollbars` field, both reading from
   `config.ui.*`. Confirmed both fields are used independently elsewhere in the
   file (tests at `save_auto_resize_splits_*` for fork's, and the
   `self.state.pane_scrollbars = ...` reload site for upstream's). Resolved:
   kept both lines.

3. **Config-reload site (~line 1614).** Same pair (`auto_resize_splits` /
   `pane_scrollbars`) but in the live-reload path (`self.state.foo = ...`).
   Same reasoning — kept both lines.

4. **Test module (~line 3745-3919).** Fork added three new tests
   (`record_successful_remote_mount_target_persists_most_recent_first`,
   `..._escapes_quotes_and_backslashes`, `..._skips_the_write_when_nothing_changed`)
   plus two auto-resize-splits save tests, all ahead of upstream's unchanged
   `reload_config_keeps_current_state_on_invalid_toml`. No overlap — resolved
   by keeping fork's new tests, then upstream's existing test immediately after.

## src/app/config_io.rs (1 hunk)

Fork added four new `App` methods: `save_auto_resize_splits`,
`save_pane_history_persistence`, `save_switch_ascii_input_source_in_prefix`,
and `save_recent_remote_mount_targets` (the recent-mount-targets persistence —
serializes via `toml::Value::from(targets.to_vec()).to_string()`, never
hand-quoted, written under `[ui] recent_remote_mount_targets`). Upstream's
side of the conflict was empty (just the unchanged `save_agent_panel_sort`
that follows). Resolved: kept the fork's whole block verbatim, immediately
followed by upstream's unchanged `save_agent_panel_sort`. Verified the
`toml::Value` serialization call site is intact and the config key name
(`recent_remote_mount_targets`) survived — matches the persisted-recents
feature this merge must not silently drop.

## src/app/runtime_mutations.rs (1 hunk)

Import list conflict: fork imports `WorkspaceMountRemoteParams`, upstream
imports `WorkspaceMoveBlockParams` (unrelated new upstream API: pane/tab block
move). Grep confirmed both types are used later in the same file (line 47 uses
`WorkspaceMountRemoteParams`, line 71 uses `WorkspaceMoveBlockParams`).
Resolved: merged import list to include both.

## src/app/api/workspaces.rs (1 hunk)

Same pattern as runtime_mutations.rs — import list conflict between
`WorkspaceMountRemoteParams` (fork) and `WorkspaceMoveBlockParams` (upstream).
Both used extensively later in the file (mount-remote handler + several
tests; move-block handler + test). Resolved: merged import list.

## src/app/input/mod.rs and src/app/input/copy_mode.rs (1 hunk each, same shape)

Both hunks are functionally identical: after a copy/selection action, dispatch
a pending clipboard write. Fork inlines the send with
`ClipboardWrite { content, origin: None }` (origin is the federation
attribution field — `None` for local, `Some(host_key)` for a mirrored remote
pane). Upstream refactored this into a shared helper,
`self.dispatch_pending_clipboard_write()`, defined in the new
`src/app/input/clipboard.rs`.

**Reconciliation:** `src/app/input/clipboard.rs` is NOT in my file ownership
(owned by another agent/group) and its currently-checked-out content still
calls `ClipboardWrite { content }` — no `origin` field. `src/events.rs` is
already resolved (not conflicted, `M` in git status) and its `ClipboardWrite`
variant now requires `origin: Option<String>` (the fork's federation
attribution field). That means upstream's shared helper as it stands would
fail to compile once `events.rs`'s struct shape is honored.

Within my two files I resolved by keeping the fork's inline version verbatim
(which already supplies `origin: None` correctly) rather than calling the
upstream helper, since I cannot fix `clipboard.rs` from this file set.

**Flag for whoever owns `src/app/input/clipboard.rs`:** `dispatch_pending_clipboard_write()`
needs an `origin: Option<String>` parameter (or a `None`-defaulting variant)
to compile against the now-federation-aware `ClipboardWrite` event, and once
fixed, `mod.rs`/`copy_mode.rs` could go back to calling the shared helper
instead of duplicating the inline send — that would be a nice follow-up
simplification but is out of scope for this merge pass and not required for
correctness (both files currently compile-correct given events.rs's shape,
assuming clipboard.rs gets fixed by its owner).

## src/app/input/mouse.rs (1 hunk)

Two independent new `#[tokio::test]` async tests landed adjacent to each
other: fork's `pane_right_click_menu_reflects_live_auto_resize_state`
(exercises the new auto-resize-splits context-menu label) and upstream's
`normal_right_click_keeps_focus_and_exposes_swap_for_reporting_pane` (exercises
right-click-keeps-focus + swap-with-focused-pane menu item for a
mouse-reporting pane). No behavioral overlap. Resolved: kept both tests back
to back.

## Confidence / not fully verified

- All resolutions in this batch are additive (both sides kept, no logic
  dropped) except the clipboard dispatch flag above, which is a cross-file
  compile dependency outside my ownership boundary — not something I can fix
  here but must be watched by whoever finishes `src/app/input/clipboard.rs`.
- Did not run `cargo check` — most of the tree (Cargo.toml, Cargo.lock,
  CHANGELOG.md, several docs, and other UU files) is still mid-conflict from
  parallel groups, so a build attempt would fail for reasons unrelated to
  these 7 files. A full build should happen after all groups finish per the
  task instructions.
