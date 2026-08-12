# Phase 01 — Server-side target validation for `workspace.mount_remote`

Status: pending · Depends on: none · Owns: `src/app/api/workspaces.rs`, `src/remote/unix.rs`

## Why

`handle_workspace_mount_remote` (`src/app/api/workspaces.rs:53-126`) only trims and drops empty
targets (lines 59-65). It never calls `validate_remote_target` (`src/remote/unix.rs:285-293`),
which is private and reachable only from `extract_remote_args` (`:212,218,224`). The target then
reaches `command.arg("-T").arg(target)` (`src/remote/unix.rs:417-419`, `:546-548`, `:1044-1046`).
`ssh` parses a leading-`-` argv element as an **option**, so
`-oProxyCommand=<cmd>` is arbitrary command execution reachable by anything that can open
`herdr.sock`. The discovery report's "SAFE — mechanism prevents injection" verdict is correct for
the *shell* but wrong for ssh's own option parsing, and it assumed the CLI validator was on this
path. It is not. **Trust the source.**

Also: `localhost` is filtered on the CLI path only (`is_local_target` / `remote_ssh_targets`,
`src/remote/unix.rs:160-173`); the API happily dials it.

## Data flow

`Method::WorkspaceMountRemote(params)` → `src/app/api.rs:1025` → handler →
**[new] trim → reject empty → validate each target → reject `localhost`** → per-target
`double_attach_conflict` (unchanged, async failure event) → `tokio::spawn` dial →
`encode_success(ResponseResult::WorkspaceMountRemoteRequested { targets })`.

New rejections are **synchronous** `encode_error(id, "invalid_request", …)` returned before any
spawn, so the caller (dialog or CLI) sees them immediately.

## Tests first

Write these before touching the handler.

In `src/remote/unix.rs` `#[cfg(test)] mod tests` (near the existing
`extract_remote_args_rejects_option_like_target`, `:3071`):

- `validate_remote_target_rejects_empty_and_option_like_values` — direct unit on the now
  `pub(crate)` fn: `""` → Err, `"-oProxyCommand=x"` → Err, `"-"` → Err,
  `"alice@host:22"` → Ok.

In `src/app/api/workspaces.rs` `#[cfg(test)] mod tests` (reuse the existing harness shape from
`duplicate_host_key_target_is_isolated_and_named_in_failure_event`, `:1270`:
`App::new(&Config::default(), true, None, api_rx, EventHub::default())`, `#[cfg(unix)]`
`#[tokio::test]`):

- `mount_remote_rejects_option_like_target_without_spawning_a_dial` — targets
  `["good-host", "-oProxyCommand=touch /tmp/pwn"]` → `ErrorResponse` with code
  `invalid_request`, message naming the offending target; assert
  `tokio::time::timeout(200ms, app.event_rx.recv())` yields nothing (no dial, no failure event)
  and `app.state.remote_mirrors.is_empty()`.
- `mount_remote_rejects_localhost_target` — targets `["localhost"]` → `invalid_request`; same
  no-event assertion.
- `mount_remote_rejects_blank_only_targets` — targets `["  ", ""]` → `invalid_request`
  (covers the existing empty path with an explicit test).
- `mount_remote_accepts_plain_and_user_at_host_targets` — use a **pre-mounted host** so the
  handler returns a success ack *without* spawning a real ssh dial: seed
  `app.state.begin_federation_mount(mirror)` for `already-mounted-host` exactly as
  `:1270-1300` does, then send `["  already-mounted-host  "]`; assert `SuccessResponse` and that
  the acked `targets` are trimmed. **Never** send a dialable target in a test — it would spawn a
  real ssh child.

Run:

```bash
ZIG=$HOME/.local/zig-0.15.2/zig cargo test app::api::workspaces:: -- --test-threads=4
ZIG=$HOME/.local/zig-0.15.2/zig cargo test remote::unix::tests::validate_remote_target -- --test-threads=4
```

## Implementation steps

1. `src/remote/unix.rs:285` — change `fn validate_remote_target` to
   `pub(crate) fn validate_remote_target`. Do **not** touch the `#[cfg(windows)]` twin at
   `src/remote.rs:136` (the non-unix handler already returns `unsupported_platform`).
2. `src/app/api/workspaces.rs`, inside `#[cfg(unix)] handle_workspace_mount_remote`, right after
   the existing empty-list check (`:66-72`), add a validation loop over `&targets`:
   - `crate::remote::validate_remote_target(target)` → on `Err`, return
     `encode_error(id, "invalid_request", &format!("invalid remote target {target:?}: {err}"))`.
   - `crate::remote::is_local_target(target)` → return `encode_error(id, "invalid_request",
     "workspace.mount_remote requires a remote target; \"localhost\" is not a remote host")`.
   - `tracing::warn!(%target, "rejected workspace.mount_remote target")` on each rejection.
3. Keep the doc comment on the handler accurate — extend it with one sentence stating that
   targets are validated before any spawn and that `localhost` is rejected here to match
   `remote_ssh_targets`' CLI-side filtering.
4. Do not change `handle_federation_mount_failed` (`:271-303`) — its `_ => {}` arm is deliberate
   and covered by `mount_failure_system_delivery_is_noop_when_local_notifications_disabled`
   (`:1232`).

## Do not

- Do not add or change any field in `WorkspaceMountRemoteParams` (schema is asserted by
  `src/api/schema/tests.rs` and `tests/cli/surface.rs`, and regenerating
  `docs/next/api/herdr-api.schema.json` is a merge-conflict magnet).
- Do not bump `PROTOCOL_VERSION` or `FEDERATION_PROTOCOL_VERSION`.
- Do not canonicalize targets (`HostKey` string equality is a deliberate server fact).
- No `unwrap()` in production code.

## Acceptance

- All four handler tests plus the unit test pass.
- `auto_detect_launch_with_mount` (`src/server/autodetect.rs:328`) still works: it filters
  `localhost` before calling and the CLI already validated targets, so no caller regresses.
- Behavior change is additive-restrictive only for previously-unreachable-by-design inputs.

## Rollback

Delete the validation loop and revert the visibility change. No state, no wire, no migration.
