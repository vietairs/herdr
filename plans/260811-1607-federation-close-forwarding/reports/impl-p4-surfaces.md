# P4 surfaces: CLI, TUI, docs — impl report

Worktree: `/Users/hvnguyen/Projects/herdr/.claude/worktrees/federation-multi-tab-workspace`

## 1. CLI

- `src/cli/runtime.rs`: added `workspace_close_remote` / `tab_close_remote`, dispatching `Method::WorkspaceCloseRemote` / `Method::TabCloseRemote` with ids `cli:workspace:close_remote` / `cli:tab:close_remote`.
- `src/cli/workspace.rs`: `close-remote` subcommand + `workspace_close_remote` fn (same arg-count validation/usage shape as `close`), help text distinguishes the two.
- `src/cli/tab.rs`: same for `tab close-remote`.

## 2. TUI

- `src/app/state.rs`: added `pub federated: bool` to `ContextMenuState` (not to `ContextMenuKind`). `items()` appends `"Close on host"` after Close/Close group for `Workspace`, `GitWorkspace` (all 4 arms), and `Tab` when `federated`. Pane variant untouched. Added `federated: false`/`true` to every existing construction site (all in `state.rs`, `mouse.rs`, `mod.rs`, `modal.rs` — none in panes.rs/workspaces.rs/tabs.rs).
- `src/app/input/mouse.rs`: the two real construction sites (workspace/git-workspace menu, tab menu) now compute `federated` via `classify(&ws.id)` (self is `AppState` here, so I read `self.workspaces[idx].id` directly rather than calling `App::public_workspace_id`, which isn't reachable from that impl block). The pane-menu construction site passes `federated: false` (out of scope).
- `src/app/input/modal.rs`: `apply_context_menu_action_via_api` (the real, non-test dispatch path) gained two arms: `(Workspace|GitWorkspace, "Close on host")` and `(Tab, "Close on host")`, both calling the new remote-close wrappers directly and `leave_modal` — neither goes through `open_confirm_close`. Left the `#[cfg(test)]`-only `apply_context_menu_action` (explicitly documented as diverging from production, no API access) untouched.
- `src/app/input/navigate.rs`: added `close_workspace_idx_remote_via_api` / `close_tab_idx_remote_via_api`, calling the new `runtime_*_close_remote` and parsing the JSON response via `surface_remote_close_response`: `remote_close_pending` → informational toast (`raise_remote_close_pending_toast`, new, mirrors `App::raise_remote_close_failed_toast`'s `toast_config.delivery` match but with `ToastKind::Finished`); any other error code → reuses `App::raise_remote_close_failed_toast` (now `pub(crate)`) under `#[cfg(unix)]` (that function is itself `#[cfg(unix)]`-gated in creation.rs and I was told not to touch that file beyond the visibility widening); success envelope → no toast.
- `src/app/runtime_mutations.rs`: added `runtime_workspace_close_remote` / `runtime_tab_close_remote`.
- `src/app/creation.rs`: **only** change is `fn raise_remote_close_failed_toast` → `pub(crate) fn raise_remote_close_failed_toast` (one line). Everything else in that file's diff predates this session (the already-landed close-forwarding backend).

Note on cross-platform: `runtime_*_close_remote` and the underlying `handle_workspace_close_remote`/`handle_tab_close_remote` handlers are not `#[cfg(unix)]`-gated (they short-circuit synchronously to `remote_close_unsupported` off a live-mount lookup, no network I/O), so the CLI/TUI wiring compiles and behaves sanely cross-platform. `raise_remote_close_failed_toast` itself is `#[cfg(unix)]` only (pre-existing), so the failure-toast call in `surface_remote_close_response` is gated the same way; the pending-toast branch uses the same `toast_config.delivery` match verbatim-in-spirit but with `ToastKind::Finished` and is not `cfg`-gated (the match body itself — `local_terminal_notifications`, `terminal_notify::show_notification`, `platform::show_desktop_notification` — is cross-platform elsewhere in the codebase, e.g. `remote_clipboard_stage.rs`).

## 3. Docs (docs/next only)

- `docs/next/website/src/content/docs/cli-reference.mdx`: added `herdr workspace close-remote` / `herdr tab close-remote` to the command blocks plus one paragraph each explaining the close vs close-remote distinction and the `remote_close_pending`/`remote_close_unsupported` outcomes.
- `docs/next/website/src/content/docs/socket-api.mdx`: added `workspace.close_remote` / `tab.close_remote` to the raw-methods table, plus one paragraph (next to the existing `workspace.move_block` paragraph) documenting the same distinction and the two error codes.
- Did not touch `website/src/content/docs/` (stable), root README/CHANGELOG, or the `ja`/`zh-cn` doc trees (no existing convention in this session's scope to update translations for in-flight docs/next changes).
- Note: `docs/next/api/herdr-api.schema.json` already had a 34-line diff before I started (pre-existing background work, not mine).

## Tests added

- `src/app/state.rs`: `federated_workspace_and_git_workspace_context_menus_add_close_on_host`, `non_federated_workspace_context_menu_omits_close_on_host`, `federated_tab_context_menu_adds_close_on_host`, `non_federated_tab_context_menu_omits_close_on_host`.
- `src/app/input/modal.rs`: `context_menu_close_workspace_on_host_via_api_skips_confirm_dialog`, `context_menu_close_tab_on_host_via_api_skips_confirm_dialog` — assert `apply_context_menu_action_via_api` never enters `Mode::ConfirmClose` for "Close on host" (even with `confirm_close = true` / last-tab-closes-workspace) and never removes the local mirror (workspace/tab count unchanged — no live mount means the dispatch synchronously refuses with `remote_close_unsupported`, but the no-confirm-dialog / no-local-removal contract holds regardless of that outcome).
- CLI: skipped per instructions — `src/cli/workspace.rs` / `src/cli/tab.rs` have no existing arg-parsing test harness to extend.

## Verification

- `ZIG=~/.local/zig-0.15.2/zig cargo check --bin herdr` — clean.
- `ZIG=~/.local/zig-0.15.2/zig cargo test --bin herdr -- --test-threads=1` — 3449 passed, 0 failed (one unrelated pre-existing timing-flaky test, `api::server::pane_graphics_stream::tests::inactive_owner_cancels_idle_stream_and_dispatches_close`, failed once under full-suite timing pressure and passed cleanly both in isolation and on a clean full rerun — not touched by this change).
- `cargo fmt --check` — clean.
- `ZIG=~/.local/zig-0.15.2/zig cargo clippy --bin herdr` — no warnings or errors.

## Unresolved questions

- None blocking. One judgment call worth flagging: the pending-toast helper (`raise_remote_close_pending_toast`) duplicates `raise_remote_close_failed_toast`'s delivery-match shape (with a different `ToastKind`) rather than extending the existing function with a `ToastKind` parameter, because the hard constraint only permitted a visibility change to `src/app/creation.rs`. If a shared parameterized helper is preferred later, `raise_remote_close_failed_toast` would need an explicit scope decision to touch creation.rs beyond visibility.

Status: DONE
Summary: CLI `close-remote` subcommands, TUI "Close on host" context-menu item + toast surfacing, and docs/next updates are all implemented and tested; full check/test/fmt/clippy gates pass.
Concerns/Blockers: none.
