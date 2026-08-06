# Implementation notes — federation pane-close sync

Append-only. Each entry: What / Why / Evidence / Reversibility.

## Entry 1 — reverse-index key is the mirror's namespaced id, not the raw snapshot pane id

**What**: Phase 5/6 tests originally assumed `remote_resync_pane_index` keys
equal the raw `PaneInfo.pane_id` strings used to build a test snapshot (e.g.
literal `"p1"`). Fixed by looking the key up by value (`local_id ==
root_pane_id`) instead of hardcoding the string.

**Why**: `RemoteMirror::apply_snapshot` runs every ingested pane through
`namespace_pane`, which rewrites `pane_id`/`terminal_id`/etc. via
`map_in(..., mount).to_public_id()` before it ever reaches
`materialize_federation_mount`. The index key `build_remote_pane`'s call
sites insert under is this already-namespaced id, not the snapshot's raw
string.

**Evidence**: `src/remote/federation/reducer.rs:440-445` (`namespace_pane`);
first test run failed with `left: None, right: Some(PaneId(511))` when
asserting `remote_resync_pane_index.get("p1")`.

**Reversibility**: Test-only; no production code depends on the literal
string. Fully reversible by reverting the two affected tests in
`src/app/creation.rs`.

## Entry 2 — pre-existing test assertion updated for Gap B's new indexing behavior

**What**: `resync_pane_created_from_the_wrong_origin_is_dropped`
(`src/app/creation.rs`, pre-existing) asserted
`remote_resync_pane_index.is_empty()` after a rejected resync-created pane.
Changed to `!contains_key(&remote_pane_id)` (the specific rejected id).

**Why**: Before the Gap B fix, `materialize_federation_mount` never indexed
mount-time panes, so the index legitimately started empty in that test. After
the fix it now holds entries for the fixture's own mount-time panes before
the rejected-origin call ever runs, so `is_empty()` is no longer the right
assertion — the test's actual intent (the rejected pane must not be added)
is unchanged and is what the updated assertion checks.

**Evidence**: test failed post-fix with `assertion failed:
app.remote_resync_pane_index.is_empty()`; confirmed by reading
`materialize_federation_mount`'s new insert call sites (`creation.rs` Phase 5
edit).

**Reversibility**: Test-only, single assertion line; trivially revertable.

## Entry 3 — federation_materialization_tests fixtures must call `begin_federation_mount`

**What**: New Phase 3 test (`dispatch_remote_pane_close_sends_a_request_and_registers_pending`,
`src/app/api/panes.rs`) initially failed with `remote_close_unsupported`
("no registered federation mount") even with a real materialized mount.
Fixed by calling `app.state.begin_federation_mount(mirror)` after
`materialize_federation_mount` in the test helper.

**Why**: `materialize_federation_mount` only builds workspace/tab/pane state
and wires terminal runtimes; it never registers the mirror in
`AppState::remote_mirrors`. `App::federation_host_key_for_workspace` (which
`dispatch_remote_pane_close` needs to fence the eventual response's origin)
reads that registry, not the mirror the test constructed locally. Existing
`creation.rs` tests never hit this path because they call
`handle_federation_split_pane_ready`/`handle_federation_resync_pane_*`
directly with an explicit `HostKey`, bypassing `federation_host_key_for_workspace`
entirely.

**Evidence**: first run of the new test returned `error.error.code ==
"remote_close_unsupported"` instead of `"remote_close_pending"`; fixed by
adding the `begin_federation_mount` call, confirmed by rerun passing.

**Reversibility**: Test-only, additive one-line fixture change.

## Entry 4 — "pane not found" folding heuristic in `ClosePaneResponse::Failed`

**What**: `client.rs`'s `drive_mount_channel` folds a `ClosePaneResponse::
Failed` whose `reason` contains the substring `"not found"` into an
idempotent `FederationClosePaneReady` instead of `FederationClosePaneFailed`
(Predict risk 3 — retry/duplicate-click safety), per plan Phase 4 step 5.

**Why**: The server's `pane_not_found` helper
(`src/app/api/panes.rs::pane_not_found`) always formats its message as
`"pane {pane_id} not found"`; matching on that reliable substring (not an
exact string, since the pane id is interpolated) is the smallest correct
signal without adding a new structured error-code field to
`ClosePaneResponse::Failed`'s wire shape (which the plan did not call for
and would need a version-relevant protocol change).

**Why not a structured error code instead**: would touch the wire protocol
shape for a one-string distinction; substring matching on a message this
codebase already treats as stable phrasing (`pane_not_found` is not
documented as machine-parsed elsewhere, but the coupling is narrow, local to
this one match arm, and easy to replace if `pane_not_found`'s wording ever
changes) was judged the minimal-diff choice consistent with the plan's own
framing ("Predict risk 3 mitigation").

**Evidence**: `src/app/api/panes.rs:1993-1995` (`pane_not_found`); wired at
`src/remote/federation/client.rs`'s `ClosePaneResponse::Failed` arm.

**Reversibility**: Fully additive/local to one match arm; reverting drops
back to always surfacing `ClosePaneFailed`, no data-shape change involved.

## Entry 5 — build/test environment: `xcrun-shim` on `PATH` required in addition to `ZIG`

**What**: `ZIG=$HOME/.local/zig-0.15.2/zig cargo check` alone failed with
`zig build for vendored libghostty-vt failed` (missing libSystem symbols)
until `PATH="$HOME/.local/zig-0.15.2/xcrun-shim:$PATH"` was also exported.

**Why**: Zig 0.15.2 cannot link against the current macOS 27 SDK on this
machine; the xcrun shim points it at an older SDK. Matches memory
`herdr-local-build-zig.md`'s documented workaround, which the task's stated
constraint ("export ZIG") omitted.

**Evidence**: first `cargo check --bin herdr` failed with `undefined symbol:
__availability_version_check` and a long list of missing libSystem symbols;
resolved immediately after adding the shim to `PATH`.

**Reversibility**: N/A — local environment setup, not a code change.

## Entry 6 — pre-merge review: unmount leaked one `remote_resync_pane_index` entry per pane

**What**: Added `App::purge_remote_resync_pane_index_for_workspaces` (src/app/creation.rs), mirroring the existing sibling purge helpers, and called it from the locally-initiated unmount path in `handle_workspace_close` (src/app/api/workspaces.rs) alongside the split/close/clipboard-stage purges.

**Why**: Since every mount-time pane is now indexed in `remote_resync_pane_index` (Gap B fix, Entry 4-adjacent), the unmount path purged `pending_remote_splits`/`_closes`/`_clipboard_stages` for closing workspaces but never this index — one stale `remote_pane_id -> local PaneId` entry leaked per pane on every federated unmount, forever. Unlike the sibling maps, entries here carry no `workspace_id`, so membership is resolved by walking the still-live workspaces' `Tab::layout.pane_ids()` before removal, not by a stored field.

**Evidence**: new test `unmount_purges_remote_resync_pane_index_for_closing_workspace_only` (src/app/creation.rs) builds a real mount via `materialize_federation_mount`, inserts a manual entry standing in for a different still-live workspace's pane, calls the purge, and asserts the closing workspace's entries are gone while the other survives. `ZIG=~/.local/zig-0.15.2/zig cargo test --bin herdr -- resync --test-threads=4` — 12 passed, 0 failed (includes this test plus all pre-existing `remote_resync_pane_index` tests).

**Reversibility**: Fully additive — one new helper function and one new call site; reverting drops back to the leak with no data-shape change.
