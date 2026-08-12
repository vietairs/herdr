---
title: "TUI affordance for mounting a remote workspace"
description: "Add a global-menu dialog that collects SSH targets and calls the existing workspace.mount_remote API, plus the missing server-side target validation."
status: pending
priority: P2
effort: 6h
branch: feat/tui-add-remote-workspace
tags: [tui, federation, remote, dialog, security]
created: 2026-07-22
---

# TUI: add a remote workspace

## Problem

`workspace.mount_remote` exists end-to-end server-side (`src/api/schema/workspaces.rs:27-32`,
handler `src/app/api/workspaces.rs:53-126`, dial `src/remote/unix.rs`), but no TUI and no CLI
surface calls it. Only `herdr --remote <target>` at launch
(`src/server/autodetect.rs:328-336`) or hand-written newline-JSON on `herdr.sock` reaches it. A
user already inside a running local herdr cannot mount a remote workspace at all.

## Solution shape (thin collector, no new subsystem)

- **No** schema change, **no** new socket message, **no** new event, **no** protocol bump. The
  mount request is already a shared runtime fact; only the collector (dialog + validation
  feedback) is new client code.
- New global-menu entry `mount remote workspace` → new `Mode::MountRemoteWorkspace` dialog
  (single text input, whitespace-separated targets) → `Method::WorkspaceMountRemote { targets,
  remote_keybindings: false }` → synchronous ack closes the dialog; a synchronous error stays
  inline in the dialog.
- Server-side hole closed: `handle_workspace_mount_remote` never validated targets, so a
  leading-`-` target reached `ssh -T <target>` as an **option**.

## Phases

| # | Phase | Status | Owns | Depends on |
|---|-------|--------|------|-----------|
| 01 | [Server-side target validation](phase-01-server-target-validation.md) | pending | `src/app/api/workspaces.rs`, `src/remote/unix.rs` | — |
| 02 | [TUI collector dialog + wiring](phase-02-tui-collector-dialog.md) | pending | `src/app/remote_mount.rs` (new), `src/ui/remote_mount.rs` (new), `src/app/state.rs`, `src/app/mod.rs`, `src/app/input/{mod,modal,mouse,sidebar}.rs`, `src/app/runtime_mutations.rs`, `src/ui.rs` | — (compiles against phase 01's public shape only via the API method, which already exists) |
| 03 | [Docs](phase-03-docs.md) | pending | `docs/next/website/src/content/docs/persistence-remote.mdx`, `docs/next/website/src/content/docs/keyboard.mdx`, `docs/next/CHANGELOG.md` | 01, 02 (describe shipped behavior) |

Phases 01 and 02 have disjoint file sets and can run in parallel. Phase 02 is a single phase on
purpose: adding a `Mode` variant breaks the exhaustive `match state.mode` in `src/ui.rs:435-458`,
`src/app/input/mod.rs:90-114`, `src/app/mod.rs:1770-1800`, so any split would not compile.

## MUST-ADDRESS risk resolution

| Risk | Phase | Resolution |
|---|---|---|
| 1. Handler never calls `validate_remote_target` (`src/app/api/workspaces.rs:59-65` vs `src/remote/unix.rs:285-293`) — leading-`-` target becomes an ssh option at `src/remote/unix.rs:417` | **01** | Make `validate_remote_target` `pub(crate)`; call it per target in the handler **before** any `tokio::spawn`; reject the whole request with `invalid_request` naming the offending target. |
| 2. `apply_global_menu_action(state: &mut AppState, …)` (`src/app/input/modal.rs:129`) cannot dispatch API | **02** | Menu sets `state.request_open_remote_mount_dialog`; mouse sets `state.request_submit_remote_mount`; both drained in `App::compute_view_and_handle_pending_state` (`src/app/mod.rs:1049-1096`), same as `request_new_linked_worktree` / `request_submit_worktree_create`. Key path calls the `App` method directly (mirrors `handle_worktree_create_key`, `src/app/worktrees.rs:237-264`). |
| 3. Failure feedback must not rely on the toast — `handle_federation_mount_failed`'s `_ => {}` arm (`src/app/api/workspaces.rs:299`) drops the notice when delivery is `Terminal`/`System` and `local_terminal_notifications` is false | **01 + 02** | (a) 02: dialog-local `error: Option<String>` rendered inline (mirrors `WorktreeCreateState.error`, `src/ui/dialogs.rs:293-302`). (b) 01: every *synchronously detectable* failure (empty, blank, option-like, `localhost`) becomes an immediate `invalid_request` instead of a fire-and-forget event, so the common cases land in the inline error. Async dial failure still uses the existing notification path — **unchanged on purpose**, see Accepted limitations. |
| 4. `remote_keybindings` is dead in the handler (`src/api/schema/workspaces.rs:31` never read by `src/app/api/workspaces.rs:53-126`) | **02** | No UI for it; always send `false`. Asserted by a test on the params builder. |
| 5. Non-unix: handler returns `unsupported_platform` (`src/app/api/workspaces.rs:28-39`) | **02** | `#[cfg(unix)]` on the `GlobalMenuAction::MountRemoteWorkspace` variant, its `global_menu_actions` push, its `global_menu_labels` push, and its `apply_global_menu_action` arm. `GlobalMenuAction` is matched only inside `src/app/input/modal.rs`, so the gate is contained. `Mode` variant itself stays cross-platform (unreachable on Windows). |
| 6. `localhost` semantics diverge — CLI filters it (`src/remote/unix.rs:160-173`), API does not | **01** | Reject `localhost` server-side with `invalid_request` (shared fact → server side). Non-breaking: the only other caller, `auto_detect_launch_with_mount` (`src/server/autodetect.rs:330`), already filters it via `remote_ssh_targets`. |

## Accepted limitations (WATCH, not fixed here)

- **No cancel, unbounded pre-dial.** `prepare_remote_herdr` + `ensure_remote_server_ready` run in
  `spawn_blocking` with no timeout (`src/remote/unix.rs:~700`); only the dial is bounded (25s).
  The dialog closes on the immediate ack and shows no progress state — a hung target is silent.
  Documented in phase 03.
- **Async mount failure can still be invisible** under `toast.delivery = terminal|system` with
  local terminal notifications off. Changing that `_ => {}` arm would reverse the deliberate,
  tested behavior at `src/app/api/workspaces.rs:1232-1249`; out of scope. Documented in phase 03
  troubleshooting (check logs).
- **`HostKey` dedup is raw-string** (`src/app/state.rs:1672-1677`): `host`, `alice@host`,
  `alice@host:22` are three keys. Do not canonicalize client-side (server fact).
- **No `BatchMode=yes` on the dial paths** (contrast the probe at `src/remote/unix.rs:1185`);
  `stderr(Stdio::inherit())` at `src/remote/unix.rs:422` means an ssh prompt can land on the
  TUI's terminal. Not changed — it would break password-auth users. Documented as
  "key-based auth required".

## Acceptance criteria

1. From a running local herdr, the global menu shows `mount remote workspace` (unix only); it
   opens a modal matching existing dialog language (shell, header, single input, two action
   buttons, esc/enter, mouse-clickable buttons).
2. Enter or the primary button sends exactly one `workspace.mount_remote` with the
   whitespace-split target list and `remote_keybindings: false`.
3. A synchronous error (empty input, option-like token, `localhost`, unsupported platform) leaves
   the dialog open with the message rendered inline; nothing is dialed.
4. `handle_workspace_mount_remote` rejects option-like and `localhost` targets before spawning
   any task, with a test proving no dial task is spawned.
5. `Mode::MountRemoteWorkspace.wants_ascii_input()` is `false` (test-asserted).
6. `global_menu_actions().len() == global_menu_labels().len()` for every badge state
   (test-asserted).
7. No change to `src/api/schema/**`, `docs/next/api/herdr-api.schema.json`,
   `src/protocol/wire.rs`, or `FEDERATION_PROTOCOL_VERSION`.
8. `ZIG=$HOME/.local/zig-0.15.2/zig cargo test app::api::workspaces remote_mount -- --test-threads=4`
   passes; the 3 pre-existing clippy errors remain the only clippy failures.

## Rollback

Each phase is revertible alone. Phase 01 revert = drop the validation block (restores the
pre-existing hole). Phase 02 revert = drop 2 new files + the one-line additions in 7 hot files;
no persisted state, no wire change, so no migration. Phase 03 is docs-only.

## Build

```bash
export ZIG=$HOME/.local/zig-0.15.2/zig
ZIG=$HOME/.local/zig-0.15.2/zig cargo test <filter> -- --test-threads=4
```

`just` and `cargo-nextest` are not installed on this machine. Prefer `cargo check` while
iterating; builds are slow (vendored C/Zig).
