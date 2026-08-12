# Phase 1 implementation — federation multi-tab workspace

Worktree: `/Users/hvnguyen/Projects/herdr/.claude/worktrees/federation-multi-tab-workspace`
Branch: `feat/federation-multi-tab-workspace`. Not committed.

## Status

Status: DONE_WITH_CONCERNS — code + tests green, but the fix is never exercised
against a live remote host in this session, and two deviations from the brief
were forced by a local structural constraint (see Deviations).

## Files changed

| File | Change |
|---|---|
| `src/events.rs` | `FederationResyncPaneCreated` gains `tab_id: String`. New `AppEvent::FederationResyncTabCreated { origin, workspace_id, tab_id, label }` and `AppEvent::FederationResyncTabClosed { origin, tab_id }`, both `#[cfg(unix)]`. |
| `src/remote/federation/reducer.rs` | `reconcile_tabs` now returns `(Vec<TabInfo>, Vec<String>)` (created / retired-namespaced-ids). `ReconcileDiff` gains `created_tabs` + `removed_tab_ids`; `reconcile_by_diff` populates them. |
| `src/remote/federation/client.rs` | `materialize_resync_pane` copies `pane_info.tab_id` into the event. `drive_mount_channel` emits `FederationResyncTabCreated` **before** the created-pane loop and `FederationResyncTabClosed` **after** the removed-pane loop. |
| `src/app/mod.rs` | New `App::remote_resync_tab_index: HashMap<String, creation::RemoteTabRef>` + init. |
| `src/app/creation.rs` | New `RemoteTabRef { workspace_id, tab_number: Option<usize>, label: Option<String> }`. `materialize_federation_mount` indexes every mount-time tab. `handle_federation_resync_pane_created` rewritten to resolve the target tab from `tab_id`. New `handle_federation_resync_tab_created` / `handle_federation_resync_tab_removed`. New `purge_remote_resync_tab_index_for_workspaces`. New shared `workspace_matches_federation_origin` helper (the two existing inline origin fences now call it). New tests. |
| `src/app/api.rs` | Dispatch arms for the two new events next to the pane ones. |
| `src/app/api/workspaces.rs` | Tab-index purge added at both unmount/close sites that already purge the pane index. |
| `src/app/actions.rs` | Two `=> Vec::new()` match arms for the new events (exhaustiveness). |

Diff: 8 files, +756 / -52.

## Design notes

- **Stable local tab handle.** `Tab` has no id, so `RemoteTabRef` identifies the
  local tab by `Workspace::id` + `Tab::number` (the public tab number).
  `Tab::number` is stable for the tab's life and never reused
  (`next_public_tab_number` only increases), so — unlike a `Vec` index — it
  cannot silently re-point at an unrelated tab after a close. A number that no
  longer resolves degrades to the "unknown tab" branch and is overwritten.
- **Resolution order in `handle_federation_resync_pane_created`:** known tab
  number that still resolves → splice as a horizontal split inside *that* tab;
  otherwise → `create_tab_from_existing_pane` + `emit_tab_created_events`
  (the same primitives + event path mount-time materialization uses), then
  record the tab number in the index.
- **No wire change.** `PROTOCOL_VERSION` and `FEDERATION_PROTOCOL_VERSION`
  untouched; `PaneInfo.tab_id` was already on the wire. Verified nothing under
  `src/protocol/` or `src/remote/federation/protocol/` was edited.

## Deviations from the brief

1. **`is_structural_event_kind` already listed `TabCreated`/`TabClosed`**
   (`src/remote/federation/client.rs`). No change was needed; the brief assumed
   they were missing. Remote tab create/close already triggered a resync — the
   diff just had nowhere to report tabs.
2. **`FederationResyncTabCreated` does not itself create a local `Tab`.** A
   local `Tab` cannot exist without at least one pane (`Tab::from_existing_pane`
   / `create_tab_from_existing_pane` both require a `MovedPane`), so an
   "empty tab" is not representable. The handler therefore records the remote
   tab's identity and **label** in `remote_resync_tab_index` with
   `tab_number: None`; the pane event that follows in the same diff does the
   actual materialization and fills in the number. Emission order in
   `drive_mount_channel` (tabs-created → panes-created → panes-removed →
   tabs-closed) is what makes the label available in time. Net effect matches
   the acceptance criterion — a remote tab created after mount becomes a real
   local tab, correctly labelled — but the creation happens on the pane event,
   as the brief's 1a branch already required. Idempotency holds either way: a
   tab already in the index keeps its binding.
3. **Split target inside a known tab changed.** The old code split from
   `ws.focused_pane_id()` (workspace-level focus). Now that panes land in their
   real tab, the focused pane may be in a different tab, so it splits from the
   target tab's own last layout pane. This is a behaviour change for the
   known-tab case, unavoidable once the target is retargeted.
4. **Tab-removed does not close the workspace's last tab.** `Workspace` must
   always keep ≥1 tab, and "last tab closes the workspace" is already owned by
   the pane-removal path (`Workspace::close_pane` returning `true`).
   `handle_federation_resync_tab_removed` prunes the index and returns in that
   case rather than duplicating workspace teardown.
5. **Purge helper is a sibling, not a rename.** Added
   `purge_remote_resync_tab_index_for_workspaces` alongside the existing pane
   one (matching the "sibling purge helpers" pattern already documented there)
   instead of renaming the pane helper and churning its call sites/tests.

## Tests added (`src/app/creation.rs`, `federation_materialization_tests`)

Helpers: `only_tab_id`, `tab_info_for`, `pane_info_in_tab`, `two_tab_snapshot`,
`resync_pane_payload`, `mount_two_tab_mirror`.

- `multi_tab_mount_materializes_one_local_tab_per_remote_tab` — 2 remote tabs ×
  1 pane materialize as 2 local tabs (1 pane each, correct labels, both
  indexed). The multi-tab analogue of the existing 1-tab/2-split test.
- `resync_pane_created_with_a_known_tab_id_lands_in_that_tab` — pane for tab 2
  lands in tab index 1 while the active tab is 0, and does not create a tab.
- `resync_pane_created_with_an_unknown_tab_id_creates_a_new_local_tab` — the
  regression test for the report: unseen `tab_id` → third local tab, not a
  split; label from the tab-created event reaches it; index updated.
- `resync_tab_created_then_closed_round_trip_leaves_state_consistent` — asserts
  `Workspace::assert_invariants_for_test()` **and**
  `AppState::assert_invariants_for_test()`, plus that both the tab index and
  the pane index are pruned.
- `resync_tab_removed_from_the_wrong_origin_is_dropped` — origin fence.

Four pre-existing resync tests were updated to pass the new `tab_id` (sourced
from the mirror's own namespaced tab key, so they still exercise the known-tab
path unchanged).

## Verification (verbatim)

Zig: `/Users/hvnguyen/.local/zig-0.15.2/zig` (present at the expected path).
`just` / `cargo nextest` unavailable, as stated in the brief.

`cargo test -- --test-threads=4`:

```
test result: ok. 3399 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 34.82s
test result: ok. 11 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 22.85s
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 14 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 12.83s
test result: ok. 9 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 7.59s
test result: ok. 11 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 22.79s
test result: FAILED. 19 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 21.22s
```

Totals: **3463 passed, 1 failed**.

The single failure is `tests/live_handoff.rs::live_handoff_keeps_unmanaged_agent_name_bound_to_saved_session`:

```
panicked at tests/live_handoff.rs:1306:9:
agent process was not detected: {"error":{"code":"agent_not_found","message":"agent target w1:p1 not found"},"id":"test:agent:wait-for-process"}
```

**Pre-existing, not caused by this change.** Confirmed empirically: stashed all
changes (`git stash push -u`), rebuilt, reran that single test on the clean
base — same failure — then restored. It is a live-PTY agent-detection
integration test with no federation involvement.

`cargo fmt --check` → clean.
`cargo clippy --all-targets` → exit 0, no errors, no warnings. (The ~3 known
pre-existing clippy errors noted in the brief did not reproduce on this branch;
the baseline here is already clean.)

## Not verified

- **No live federation run.** The fix was not exercised against a real remote
  herdr host (e.g. appn-ltu-vm-105). Everything is unit-level with a
  `RemoteMirror` snapshot; the wire path (`drive_mount_channel` → real
  `SnapshotResponse`) is only verified by compilation and the existing
  federation client tests.
- **Mount-time path with a real multi-tab remote.** `materialize_federation_mount`
  was already correct per the root-cause report and is now additionally covered
  by a 2-tab unit test, but not by a live mount.
- **Split geometry.** As before this change, remote split geometry is not
  reproduced — panes chain horizontally within their tab. Only tab boundaries
  are now faithful.

## Unresolved questions

1. Should a remote **`TabRenamed`** propagate to the local tab's `custom_name`?
   `reconcile_tabs` already emits it to the hub, and `RemoteTabRef.label` now
   exists to hold it, but Phase 1's scope was create/close only — a rename
   currently updates mirror metadata and the sidebar but not the rendered local
   tab title.
2. `handle_federation_resync_tab_removed` intentionally leaves the last tab of
   a workspace alone. If a remote ever closes its last tab *without* also
   retiring that tab's panes in the same snapshot, the local workspace would
   survive with a stale tab. I judged that unreachable (a tab cannot exist
   without panes on the serving host either), but it is an assumption about
   server behaviour, not something enforced client-side.
3. Live-test ownership: who runs the 2-tab mount against a real host before
   this lands?
