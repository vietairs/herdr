# Code Review: multi-remote federated workspace launch (uncommitted diff)

Scope: 21 changed files, ~1400 insertions, feat/remote-workspace-federation. Reviewed src/remote/unix.rs, src/remote/federation/session.rs, src/app/api/workspaces.rs, src/app/state.rs, src/events.rs, src/server/autodetect.rs, src/main.rs coexistence branch.

## Findings

### High

1. **`--remote-keybindings server` is silently dropped in the Coexistence (federated) launch path.**
   `mount_remote_request` (src/remote/unix.rs:122-132) hardcodes `remote_keybindings: false` and ignores `remote.keybindings` entirely. `WorkspaceMountRemoteParams.remote_keybindings` (src/api/schema/workspaces.rs:31) is then never read anywhere server-side (`handle_workspace_mount_remote` in src/app/api/workspaces.rs never touches `params.remote_keybindings`). Net effect: a user running `herdr --remote host --remote-workspace --remote-keybindings server` gets local-keybindings behavior with no error/warning — a silent contract violation of an explicit user flag. Either wire the field through to whatever governs remote-pane keybinding behavior, or remove the field and reject `--remote-keybindings` combined with `--remote-workspace` explicitly (fail loud, not silent).

### Medium

2. **Stale `#[allow(dead_code)]` on now-live code paths.** `AppState::begin_federation_mount`, `end_federation_mount`, `double_attach_conflict`, and `remote_mirrors` (src/app/state.rs:1494, 1506, 1526, 1542, 1560) are all marked `#[allow(dead_code)]` but are now called from `src/app/api/workspaces.rs` (`handle_federation_mount_ready`, `handle_workspace_mount_remote`). Not a functional bug, but stale suppressions mask future genuine dead-code regressions in this area (project rule: "`#[allow]` only with a comment explaining why" — the comments now explain a historical reason, not the current one). Low cost to clean up; flag for follow-up, not blocking.

3. **Writer-task join is not observed on the `materialize_federation_mount` failure path.** In `handle_federation_mount_ready` (src/app/api/workspaces.rs:161-175), if `materialize_federation_mount` errors, `out_tx`/`writer_handle` (from `spawn_mount_writer`, created a few lines above) are dropped un-awaited rather than explicitly torn down like the success path's teardown block does (drop out_tx, bounded-await writer_handle, then drop tunnel_guard). Functionally the writer task still exits because its channel closes and `tunnel_guard`'s `Drop` still kills the ssh child on scope exit, so there's no resource leak — but the asymmetry with the documented teardown-order comment elsewhere in this file is worth a one-line fix (explicit drop + bounded join) for consistency, not correctness.

## Verified as correct / not re-flagging

- `remote_mirrors: HashMap<HostKey, RemoteMirror>` correctly rejects duplicate `HostKey` at both `begin_federation_mount` (state-level) and the pre-spawn check in `handle_workspace_mount_remote` (isolated per-target failure event, doesn't touch sibling mounts) — matches Phase B requirement 4/5.
- `is_local_target`/`remote_ssh_targets` filtering happens client-side in `auto_detect_launch_with_mount` (src/server/autodetect.rs:328-340) before the JSON API request is built — `localhost` never reaches the SSH dial path. Correct per Phase B requirement 2.
- `--remote-workspace` is consumed (not pushed to `cleaned`) by `extract_remote_args`, so it never reaches `main.rs`'s `known_flags` allow-list check — no false "unknown option" rejection despite the flag being absent from that list. Confirmed by reading the control flow, not assumed.
- N-target concurrency: each target is an independent `tokio::spawn` in `handle_workspace_mount_remote` (src/app/api/workspaces.rs:97-120); no per-target serial await chain, so failure/success of one target cannot block or corrupt another's event delivery.
- Teardown ordering in the success path of `handle_federation_mount_ready` and in `run_federated_session` correctly bounds the writer drain (`FEDERATION_TEARDOWN_DRAIN_TIMEOUT`/2s) before unconditionally dropping `tunnel_guard` (SIGKILL via `start_kill`), so a half-open remote peer cannot hang teardown.
- SSH targets flow into `Command`/`tokio::process::Command` via `.arg(target)` (argv, not shell string) in both `attempt_federation_mount` and `dial_federation` — no shell injection surface from the target string itself. `validate_remote_target` also rejects targets starting with `-` (prevents SSH option injection via a target arg). The one place a value is shell-interpolated (`remote_federation_serve_command`'s `--session`) uses `shell_quote`.
- `Method::WorkspaceMountRemote` and its request/response schema are additive JSON-API-only surface; `PROTOCOL_VERSION` (binary wire protocol) correctly untouched — confirmed no diff to `src/protocol/wire.rs`.
- No `unwrap()` introduced in the reviewed production code paths (tests use `.unwrap()`/`.expect()` as usual, which is accepted for tests).
- Accepted documented gaps (missing focus-barrier/teardown/batch-budget tests, mount-time-snapshot `remote_mirrors`, no teardown on natural drive-task end, Windows parity unverified) not re-flagged per instructions.

## Unresolved Questions

- Is dropping `--remote-keybindings server` in the coexistence path an intentional v1 scope cut (like the documented `double_attach_conflict` cross-process gap), or an oversight? The other deliberate cuts in this diff are explicitly commented as such; this one has no such comment, which reads as unintentional.
