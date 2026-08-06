# Verify: mount-recents-perf worktree

Worktree: /Users/hvnguyen/Projects/herdr/.claude/worktrees/mount-recents-perf

## 1. cargo fmt --check
FAILED initially — 2 unformatted blocks in `src/app/remote_mount.rs` (lines ~539, ~560, test fixtures).
Ran `cargo fmt` (formatting-only fix, allowed). Re-ran `cargo fmt --check` → clean.

## 2. cargo test --bin herdr -- --test-threads=4
3179 passed; 3 failed; finished in 50.94s.

Failures:
- `api::server::pane_graphics_stream::tests::inactive_owner_cancels_idle_stream_and_dispatches_close` — Timeout waiting for dispatched close (known flaky).
- `pane::tests::pane_terminal_identity_allows_explicit_override` — assertion mismatch `"xterm-256color\ntruecolor\n"` vs `"vt100\n24bit\n"` (env-var race, not in the pre-listed known-flaky set but same class).
- `server::headless::tests::terminal_observe_rejects_later_attach_upgrade` — `AddrInUse` binding test listener (known flaky).

## 3. Serial re-run (--test-threads=1) of the 3 failures
All 3 passed cleanly:
```
test api::server::pane_graphics_stream::tests::inactive_owner_cancels_idle_stream_and_dispatches_close ... ok
test pane::tests::pane_terminal_identity_allows_explicit_override ... ok
test server::headless::tests::terminal_observe_rejects_later_attach_upgrade ... ok
```
Confirms all three are parallelism-induced (port/global-env races), not regressions from this branch's changes.

## 4. Working tree
```
M src/app/api/workspaces.rs
M src/app/config_io.rs
M src/app/input/mouse.rs
M src/app/mod.rs
M src/app/remote_mount.rs
M src/app/state.rs
M src/config/model.rs
M src/ui.rs
M src/ui/remote_mount.rs
```
```
 src/app/api/workspaces.rs |   8 +++
 src/app/config_io.rs      |  27 ++++++++
 src/app/input/mouse.rs    |  14 +++-
 src/app/mod.rs            |  58 +++++++++++++++-
 src/app/remote_mount.rs   | 110 +++++++++++++++++++++++++++++-
 src/app/state.rs          | 110 +++++++++++++++++++++++++++++-
 src/config/model.rs       |   6 ++
 src/ui.rs                 |   2 +-
 src/ui/remote_mount.rs    | 169 +++++++++++++++++++++++++++++++++++++++++-----
 9 files changed, 483 insertions(+), 21 deletions(-)
```
(cargo fmt fix to `src/app/remote_mount.rs` test fixtures is included in this diff/status — no code-behavior change.)

## Verdict
DONE. Suite green modulo known-flaky-under-parallelism tests, confirmed passing serially. Not committed (verifier may not commit).

## Unresolved questions
- `pane::tests::pane_terminal_identity_allows_explicit_override` was not in the pre-listed known-flaky set; recommend adding it (env var race, same class as other flakes) if it recurs.

## Fix round

- `cargo fmt --check`: clean, no changes needed.
- Full `cargo test --bin herdr -- --test-threads=4`: 3186 passed, 1 failed (`api::server::pane_graphics_stream::tests::inactive_owner_cancels_idle_stream_and_dispatches_close`, Timeout panic) — this is on the known parallel-flake list.
- Serial re-run (`--test-threads=1`) of the failing test plus all known-flaky tests (plugins::manifest_action_invoke, workspace generated_workspace_ids, pane_terminal_identity_allows_explicit_override, server::headless/autodetect, and the pane_graphics_stream test): all 139 passed, 0 failed.
- Diff stat: 10 files changed, 736 insertions(+), 20 deletions(-) across app/api/workspaces.rs, app/config_io.rs, app/input/mouse.rs, app/mod.rs, app/remote_mount.rs, app/state.rs, config/model.rs, ui.rs, ui/remote_mount.rs, docs/next config-reference.json.

Status: DONE (green modulo known serial-passing flake).
