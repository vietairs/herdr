# Phase 02 — TUI collector dialog + wiring

Status: pending · Depends on: none (file-disjoint from 01) · Owns:

- new: `src/app/remote_mount.rs`, `src/ui/remote_mount.rs`
- edit: `src/app/state.rs`, `src/app/mod.rs`, `src/app/runtime_mutations.rs`,
  `src/app/input/mod.rs`, `src/app/input/modal.rs`, `src/app/input/mouse.rs`,
  `src/app/input/sidebar.rs`, `src/ui.rs`

Single phase on purpose: adding a `Mode` variant breaks the exhaustive `match state.mode` at
`src/ui.rs:435-458`, `src/app/input/mod.rs:90-114`, `src/app/mod.rs:1770-1800`, so a split would
not compile.

## Template

Copy the **new linked worktree** shape, not the open-existing one — there is no saved-target list
to browse (`RemoteConfig` has exactly one field, `manage_ssh_config`,
`src/config/model.rs:865-877`), so the searchable-list machinery has nothing to list.

- render: `render_new_linked_worktree_overlay` (`src/ui/dialogs.rs:232-324`)
- rects: `new_linked_worktree_button_rects` (`src/ui/dialogs.rs:134-151`),
  `new_linked_worktree_inner_rect` (`:~115-132`)
- key handling: `handle_worktree_create_key` (`src/app/worktrees.rs:237-264`)
- submit + inline error: `submit_worktree_open_via_api` (`src/app/worktrees.rs:711-743`) — parses
  the response, closes on `SuccessResponse`, writes `error` on `ErrorResponse`.
- mouse: `src/app/input/mouse.rs:252-282`
- helpers available to a new `ui::` submodule: `render_modal_shell`, `render_modal_header`,
  `render_action_button`, `ActionButtonSpec`, `action_button_row_rects`, `panel_contrast_fg`
  (`src/ui/widgets.rs:32-190`), `centered_popup_rect`, `super::dim_background` (`src/ui.rs:552`).

## Data flow

1. Global menu (`GlobalMenuAction::MountRemoteWorkspace`, `#[cfg(unix)]`) →
   `apply_global_menu_action` sets `state.request_open_remote_mount_dialog = true` +
   `leave_modal(state)`. It **cannot** dispatch: the fn takes `&mut AppState`
   (`src/app/input/modal.rs:129`), while dispatch needs `&mut App`
   (`src/app/runtime_mutations.rs:12`).
2. `App::compute_view_and_handle_pending_state` (`src/app/mod.rs:1049-1096`) drains the flag →
   `self.open_remote_mount_dialog()` → `state.remote_mount = Some(RemoteMountState::default())`,
   `state.name_input.clear()`, `state.mode = Mode::MountRemoteWorkspace`.
3. Keys → `Mode::MountRemoteWorkspace => self.handle_remote_mount_key(key_event)` (App method, so
   Enter can submit directly). Mouse → `mouse.rs` sets
   `state.request_submit_remote_mount = true` (AppState has no `&mut App`), drained in the same
   `mod.rs` block.
4. `submit_remote_mount_via_api()` → parse input → on parse error, write
   `remote_mount.error` and return (no request) → else
   `runtime_workspace_mount_remote("tui.workspace.mount_remote", WorkspaceMountRemoteParams {
   targets, remote_keybindings: false })`.
5. `SuccessResponse` → close dialog (`state.remote_mount = None`, `leave_modal`).
   `ErrorResponse` → keep open, `remote_mount.error = Some(err.error.message)`.
6. Async outcome is **not** awaited: `FederationMountReady` materializes workspaces and
   `FederationMountFailed` uses the existing notification path — unchanged.

## Tests first

### `src/app/remote_mount.rs` — `#[cfg(test)] mod tests` (AppState/App, no PTYs)

Pure parser (no App needed, uses `AppState::test_new()` only where state is touched):

- `parse_mount_targets_splits_on_whitespace_and_trims` — `"  host-a   alice@host-b:22 "` →
  `["host-a", "alice@host-b:22"]`.
- `parse_mount_targets_rejects_option_like_token` — `"host-a -oProxyCommand=x"` → Err naming the
  token (client-side echo of phase 01's server rule; the server is still authoritative).
- `parse_mount_targets_rejects_blank_input` — `"   "` → Err.
- `parse_mount_targets_rejects_localhost` — `"localhost"` → Err (matches phase 01).

Dialog lifecycle on `AppState::test_new()`:

- `open_remote_mount_dialog_sets_mode_and_clears_input`
- `close_remote_mount_dialog_returns_to_terminal_when_a_workspace_is_active`
- `close_remote_mount_dialog_returns_to_navigate_when_no_workspace`
- `remote_mount_key_char_and_backspace_edit_the_input`
- `remote_mount_esc_closes_the_dialog`
- `remote_mount_mode_keeps_the_ime` — assert `!Mode::MountRemoteWorkspace.wants_ascii_input()`
  (the allowlist at `src/app/state.rs:825-840`); also add the variant to the "keeps the IME" loop
  in the existing test at `src/app/mod.rs:1997-2012`.
- `submit_remote_mount_with_invalid_input_keeps_dialog_open_and_sets_inline_error` — asserts no
  API request is made.
- `mount_remote_params_always_send_remote_keybindings_false` — on the params builder; the field
  is never read by the handler (`src/api/schema/workspaces.rs:31` vs
  `src/app/api/workspaces.rs:53-126`), so it gets no UI.

API-touching (`#[cfg(unix)] #[tokio::test]`, harness copied from
`src/app/api/workspaces.rs:1270-1300`):

- `submit_remote_mount_closes_dialog_on_success_ack` — seed a pre-mounted host with
  `state.begin_federation_mount(mirror)` and submit that same target so the handler acks
  **without spawning a real ssh dial**; assert `state.remote_mount.is_none()` and mode left the
  dialog. **Never** submit a dialable target in a test.
- `submit_remote_mount_keeps_dialog_open_with_error_from_server` — submit `"localhost"` (phase 01
  rejects it server-side, or the client parser does; either way assert the dialog stays open with
  a non-empty `error`). If phase 01 has not landed yet, assert against the client-side rejection
  and re-check after integration.

### `src/app/input/modal.rs` — `#[cfg(test)] mod tests`

- `global_menu_actions_and_labels_stay_index_aligned` — `global_menu_actions(&state).len() ==
  state.global_menu_labels().len()` for all three badge states (no badge,
  `update_available.is_some()`, `latest_release_notes_available`). These two lists are parallel
  and index-addressed by `src/ui/menus.rs:215-250` and `src/ui/mobile.rs:200,469,731`; nothing
  currently enforces alignment.
- `global_menu_mount_remote_action_requests_the_dialog` — `#[cfg(unix)]`;
  `apply_global_menu_action(&mut state, GlobalMenuAction::MountRemoteWorkspace)` sets
  `state.request_open_remote_mount_dialog` and leaves the menu.

### `src/ui/remote_mount.rs` — `#[cfg(test)] mod tests`

- `remote_mount_button_rects_are_disjoint_and_within_inner` — pure geometry, no `Frame`.
- `remote_mount_inner_rect_is_none_for_a_tiny_screen`.

### Commands

```bash
ZIG=$HOME/.local/zig-0.15.2/zig cargo test remote_mount -- --test-threads=4
ZIG=$HOME/.local/zig-0.15.2/zig cargo test app::input::modal:: -- --test-threads=4
ZIG=$HOME/.local/zig-0.15.2/zig cargo test wants_ascii_input -- --test-threads=4
```

## Implementation steps

1. `src/app/state.rs`
   - `Mode::MountRemoteWorkspace` variant (add to the enum at `:792-813`); **omit** it from
     `wants_ascii_input`'s allowlist (`:825-840`) — free-text dialog.
   - `pub struct RemoteMountState { pub error: Option<String>, pub submitting: bool }`
     (`#[derive(Debug, Clone, Default, PartialEq, Eq)]`). The typed input reuses the existing
     `name_input` field, exactly as `NewLinkedWorktree` does.
   - Fields near `:1423-1442`: `pub remote_mount: Option<RemoteMountState>`,
     `pub request_open_remote_mount_dialog: bool`, `pub request_submit_remote_mount: bool`;
     initialize all three in the constructor block at `:1900-1925`.
   - Names are neutral (no sidebar/row/card/widget).
2. `src/app/remote_mount.rs` (new, `mod remote_mount;` in `src/app/mod.rs`)
   - `pub(crate) fn parse_mount_targets(input: &str) -> Result<Vec<String>, String>` —
     `split_whitespace`, reject empty result, reject any token starting with `-`, reject
     `localhost`. Client-side echo only; the server re-validates (phase 01).
   - `impl App`: `open_remote_mount_dialog`, `close_remote_mount_dialog`,
     `handle_remote_mount_key` (Esc/Enter/Backspace/Char, mirroring
     `src/app/worktrees.rs:237-264`), `insert_remote_mount_text` (for paste),
     `submit_remote_mount_via_api`.
3. `src/ui/remote_mount.rs` (new, `mod remote_mount;` + `use` in `src/ui.rs:8-30`)
   - `remote_mount_inner_rect(area) -> Option<Rect>`, `remote_mount_button_rects(inner) ->
     (Rect, Rect)` (labels `mount` / `cancel`, hints `↵` / `esc`),
     `render_remote_mount_overlay(app: &AppState, frame, area)` — header `mount remote workspace`,
     label ` target`, one input row echoing `app.name_input`, a hint row
     (`user@host, space-separated for several`), then `mounting…` or the inline error, then the
     buttons. Render is pure — no state mutation.
4. `src/ui.rs` — `mod remote_mount;`, re-export the two rect fns `pub(crate)` (mouse.rs needs
   them, same as `new_linked_worktree_*`), and add
   `Mode::MountRemoteWorkspace => render_remote_mount_overlay(app, frame, frame.area())` to the
   dispatch at `:435-458`.
5. `src/app/runtime_mutations.rs` — `runtime_workspace_mount_remote(&mut self, id, params)`
   next to `runtime_workspace_create` (`:31-37`); import `WorkspaceMountRemoteParams`.
6. `src/app/input/modal.rs` — `#[cfg(unix)] MountRemoteWorkspace` on `GlobalMenuAction`
   (`:75-82`), push it in `global_menu_actions` after `ReloadConfig` (`:84-95`), and add the
   `apply_global_menu_action` arm (`:129-142`) setting the request flag + `leave_modal`.
7. `src/app/input/sidebar.rs:199-208` — push `"mount remote"` at the **same index**
   (after `"reload config"`), `#[cfg(unix)]`.
8. `src/app/input/mod.rs` — `Mode::MountRemoteWorkspace => self.handle_remote_mount_key(key_event)`
   at `:100`; add to the paste routing `match` at `:143-160` via `insert_remote_mount_text`; check
   `:608` (`Mode::RenameWorkspace | … | Mode::NewLinkedWorktree` text-input group) and add the
   variant there if it governs text-input behavior the dialog needs.
9. `src/app/mod.rs` — key dispatch arm at `:1770-1800`; two drain blocks in
   `compute_view_and_handle_pending_state` (`:1049-1096`) following the
   `request_submit_worktree_create` pattern, each setting `needs_render = true`; add the variant
   to the IME test loop at `:1997-2012`.
10. `src/app/input/mouse.rs` — add `Mode::MountRemoteWorkspace` to the left-click-only guard at
    `:212-218`; add a hit-test block mirroring `:252-282`: Confirm → `request_submit_remote_mount
    = true`; Cancel (unless `submitting`) → clear `remote_mount`, clear `name_input`,
    `leave_modal(self)`.

## Do not

- No new API method, params field, event, or socket message. No protocol bump.
- No checkbox/toggle for `remote_keybindings`.
- No new dependency, no new abstraction over the existing modal helpers.
- No `unwrap()` in production code; `tracing` for logging.
- Do not mutate state inside `render_*`.

## Acceptance

- All named tests pass; `cargo check` clean for unix and (spot-check) `--target
  x86_64-pc-windows-msvc` is not required, but the `#[cfg(unix)]` gates must leave no unused
  imports on non-unix.
- Menu entry absent on non-unix; dialog reachable via mouse click and keyboard from the global
  menu.
- Dialog matches existing modal language (shell/header/buttons/esc/enter).

## Rollback

Delete the two new files and revert the ~10 small edits. No persisted state, no wire change.
