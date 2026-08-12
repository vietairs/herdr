# Federation multi-tab / multi-workspace — correctness & state-integrity review

Branch: `feat/federation-multi-tab-workspace` (`591b979a`, `870a4bfd`) vs `master`
Worktree: `/Users/hvnguyen/Projects/herdr/.claude/worktrees/federation-multi-tab-workspace`
Lens: correctness and state integrity. Read-only; nothing edited.

Build/test evidence run in the worktree:
`cargo test --bin herdr federation -- --test-threads=4` → **221 passed, 0 failed** (compiles clean).

## Scope

12 files, +2220/-62. Non-test surface reviewed line by line:
`src/app/creation.rs` (mount-time indexing, 5 new resync handlers, `close_single_workspace_at`),
`src/app/api/workspaces.rs` (create redirect + purges), `src/app/api.rs`, `src/app/mod.rs`,
`src/events.rs`, `src/remote/federation/{client.rs, reducer.rs, protocol/mod.rs}`,
`src/server/{federation_accept.rs, federation_actor.rs}`.

---

## CONFIRMED findings

### 1. HIGH — a created remote workspace can silently never materialize (resync coalescing drops its only two triggers)

`src/remote/federation/client.rs:576`, `:586`, `:603`, `:796`

`resync_in_flight` is a plain bool with no "dirty" companion. A structural event or a
`WorkspaceCreateResponse::Created` arriving while a snapshot request is outstanding is
**dropped, not deferred**; `SnapshotResponse` just clears the flag (`:603`) and never
re-requests.

Failure sequence (all local to one drive task):
1. Some unrelated structural event → client sends `SnapshotRequest` (`resync_in_flight = true`).
2. Serving host builds that snapshot at T2.
3. Host creates the workspace at T3 > T2 (either its own user, or servicing this client's
   `WorkspaceCreateRequest`).
4. `WorkspaceCreateResponse::Created` arrives → `:796` sees `resync_in_flight == true` → no request.
5. Host's `workspace.created` event frame arrives → `:586` sees the same flag → no request.
6. The T2 snapshot lands, `resync_in_flight = false`, and its diff contains nothing.

The workspace exists on the host and in no diff the client will ever see. Nothing retries.
The user already got `remote_workspace_create_pending` back (an "ack"), so there is no error
either — the action just does nothing until some unrelated later structural event happens to
fire. This is pre-existing coalescing logic, but Phase 2 makes it load-bearing for a
user-initiated action and adds a second suppressed trigger at `:796`.

Fix: `resync_in_flight: bool` + `resync_dirty: bool`; set `dirty` instead of dropping, and on
`SnapshotResponse` re-issue when `dirty`.

### 2. HIGH — the whole workspace-create redirect is inert when `prompt_new_workspace_name` is on

`src/app/api/workspaces.rs:620` (`if params.cwd.is_none()`) vs
`src/app/creation.rs:111-120` and `src/app/input/modal.rs:1059-1070`

The redirect only fires when `cwd` is `None`. `begin_tui_workspace_create` short-circuits into
`open_new_workspace_dialog` when `state.prompt_new_workspace_name` is set, and the dialog's
confirm path (`save_rename_modal_via_api`) always sends `cwd: Some(...)`. So for every user with
that config on, "new workspace" inside a mounted remote workspace still creates a **local**
workspace — seeded from `focused_pane_cwd_in_workspace(<federated ws>)`, i.e. a path that only
exists on the remote host.

The added test's doc comment explicitly claims this path is covered
(`creation.rs:3629-3636`: "the rename-modal named create ... all via `runtime_workspace_create`").
It is not covered, and the claim is false — that call site never satisfies `cwd.is_none()`.
No test exercises `prompt_new_workspace_name = true`.

### 3. MEDIUM — index entries are purged *before* the origin fence (foreign-origin event corrupts another mount's index)

`src/app/creation.rs:1673` (workspace) and `src/app/creation.rs:1852` (tab)

Both removal handlers mutate the index on the first line and only afterwards check the origin:

```rust
// creation.rs:1852
let Some(tab_ref) = self.remote_resync_tab_index.remove(&tab_id) else { return; };
... position(|ws| ws.id == tab_ref.workspace_id) ...
if !self.workspace_matches_federation_origin(ws_idx, &origin) { warn; return; }   // too late
```

With two mounts live (the branch's whole premise), a second host that emits a removal for an id
it does not own — buggy or hostile — is correctly refused *for the tab/workspace* but has already
deleted the legitimate index entry. Consequences:

- Tab entry wiped → the next resync pane for that still-live remote tab hits the "unknown tab"
  branch and creates a **duplicate local tab** next to the real one.
- Pending workspace announcement wiped (`:1673`) → the pane event that was supposed to
  materialize that workspace hits `materialize_resync_workspace_from_pane`'s
  "neither materialized nor announced" warn at `creation.rs:1517` and is **dropped permanently**
  (see finding 5 — there is no repair path).

The two wrong-origin tests (`resync_tab_removed_from_the_wrong_origin_is_dropped`,
`resync_workspace_events_from_the_wrong_origin_are_dropped`) only assert the workspace/tab
survives; neither asserts the index survives, which is why the defect passes CI.

Fix: move both `remove` calls below the origin check (read with `get` first).

### 4. MEDIUM — `tab.close` on a mirrored workspace still group-closes the entire mount

`src/app/api/tabs.rs:245` (`handle_tab_close`), group close at `:266-276`

The branch's `close_single_workspace_at` (`creation.rs:1608`) is correctly used at the three
federation handlers it touches (`:1218`, `:1705`, `:1997`), and I verified no non-federation
caller was switched to single-close — `close_indices_for`'s worktree-group semantics are intact.
But the audit missed a reachable site:

- `pane.close` is safe: `api/panes.rs:1846-1851` routes a federated workspace to
  `dispatch_remote_pane_close` before reaching the group close at `:1923`.
- `actions.rs:2047`/`:2081` are inside `#[cfg(test)]` fns — not production.
- **`tab.close` has no federation guard at all.** Contrast `tab.create`, which explicitly refuses
  with `remote_tab_unsupported` (`api/tabs.rs:75-84`). Closing the *last* tab of one mirrored
  workspace runs `self.state.close_selected_workspace()` (`:276`) → `close_indices_for` matches
  every workspace sharing `federation:<host_key>` → **all mirrored workspaces of that mount
  disappear locally** while the mount, its drive task and its mirror stay alive. Only
  `confirm_implicit_worktree_group_close` stands in the way, and only when `confirm_close` is on;
  its message ("closing this tab would close a worktree group") is also wrong for federation.
- Secondary: a non-last mirrored-tab close is never forwarded to the remote, so the local tab and
  its panes vanish while the remote keeps running them — permanent divergence (no repair path,
  same reason as finding 5).

### 5. MEDIUM — a mirror-known workspace that failed to materialize can never be repaired

`src/app/creation.rs:1517` + `src/remote/federation/reducer.rs:478-523`

`materialize_resync_workspace_from_pane` hard-requires an entry in
`remote_resync_workspace_index`, and that map is only ever populated from
`ReconcileDiff::created_workspaces`, which `reconcile_workspaces` emits **only the first time**
the mirror sees the id. Once the mirror holds a workspace, no future diff re-announces it.

Reachable triggers:
- mount-time skip: `creation.rs:596` (`tabs.is_empty()`) and `:617` (tab with no panes) `continue`
  without recording anything;
- any dropped/failed pane event for a newly-announced workspace, including finding 3's wipe.

After that, every pane the remote adds to that workspace is warned and dropped forever. Consider
falling back to `mirror.workspaces()` for the label/origin instead of requiring the pending entry.

### 6. LOW/MEDIUM — last-tab skip leaves an orphaned local tab with a wiped index entry

`src/app/creation.rs:1852` (remove) + `:1895` (`if tabs.len() <= 1 { return; }`)

The `tabs.len() <= 1` guard correctly protects the workspace invariant, but the entry was already
removed at `:1852`. If the host retires a tab **without** retiring its panes in the same snapshot
(so the pane-removal loop did not collapse it), the local tab survives showing panes whose remote
counterparts are gone, and the index no longer knows about it. Only reachable from a host that
reports panes belonging to a tab absent from `snapshot.tabs`; a well-behaved host does not. I
found no path where a `Workspace` ends with zero tabs or a `Tab` with zero panes — `close_tab`
refuses the last tab and `create_tab_from_existing_pane`/`from_existing_pane` always seed a pane.

### 7. LOW — tab resolution ignores `RemoteTabRef::workspace_id`

`src/app/creation.rs:1381-1390`

```rust
let known_tab = self.remote_resync_tab_index.get(&tab_id).and_then(|r| r.tab_number);
let existing_tab_idx = known_tab.and_then(|number|
    self.state.workspaces[ws_idx].tabs.iter().position(|tab| tab.number == number));
```

`tab_ref.workspace_id` is never compared against the pane's `workspace_id`. `Tab::number` is only
unique *within* a workspace, so an entry recorded under workspace A resolves against workspace B's
tab numbering and the pane is spliced into an unrelated tab. Not reachable through the current API
(`tab.move` only reorders within a workspace; `reconcile_tabs:555-566` emits `TabRenamed`, not
`created`, when a `TabInfo` changes, so a hypothetical cross-workspace move would leave exactly
this stale entry). A buggy/hostile host re-announcing a tab id under a different workspace reaches
it today. `handle_federation_resync_tab_created:1829` has the mirror-image gap — `or_insert_with`
never corrects an existing entry's `workspace_id`. One-line fix: filter the lookup on
`tab_ref.workspace_id == workspace_id`.

### 8. LOW — `remote_resync_tab_index` grows unbounded for the mount's lifetime

No caller prunes the tab index when `Workspace::close_pane` collapses a local tab (the
`handle_federation_resync_pane_removed` path, `creation.rs:1953+`). Entries accumulate with dead
`tab_number`s. Behaviorally benign — `Tab::number` is monotonic and never reused, so a stale entry
resolves to `None` and degrades to the documented "unknown tab" branch, never to a wrong live tab.
Memory only.

### 9. LOW — tab teardown prunes only the pane index

`src/app/creation.rs:1912-1916` purges `remote_resync_pane_index` for the closed tab's panes but
not `pending_remote_splits`, `pending_remote_closes`, or `remote_image_paste_pane_state` (those
helpers are workspace-scoped). `PaneId::alloc` never reuses ids, so a stale pending entry resolves
to nothing rather than to a wrong pane — leak, not misroute.

---

## Categories checked and found clean

- **Panics.** No new `unwrap`/`expect`/raw index reachable from remote input. Every
  `self.state.workspaces[ws_idx]` in the new handlers follows a `position()` in the same borrow
  with no intervening removal. `creation.rs:688`'s `expect("set immediately above on first tab")`
  is pre-existing context, not added here. `federation_accept.rs:678-722` and
  `federation_actor.rs:499-551` use `unwrap_or_else`/`ok()` throughout and always reply.
- **Cross-mount key collisions.** Impossible: all three indexes are keyed on the mirror's
  namespaced `r:<host_key>:…` ids (`reducer.rs::namespace_*`), and the mount-time inserts
  (`creation.rs:706`) use `mirror.tabs()` keys, not raw host ids.
- **Stale-entry misresolution after close/reorder.** The choice of `Workspace::id` +
  `Tab::number` (never reused, `next_public_tab_number` monotonic) over `Vec` indices is right —
  reorders and closes degrade a stale entry to "unknown", never to a wrong live target. Verified
  `Workspace::id` is a minted id, not positional, so id reuse across remote close/create is not a
  concern.
- **Double-creation on `WorkspaceCreateResponse::Created`.** The claim holds:
  `client.rs:775-800` materializes nothing and only requests a snapshot;
  `handle_federation_resync_workspace_created:1645` early-returns when a `Workspace` with that id
  already exists; the pane handler checks `position(|ws| ws.id == workspace_id)` before choosing
  the create branch. I found no double-create path. The *lost-update* half of that question is
  real — finding 1.
- **Diff ordering.** created-workspaces → created-tabs → created-panes → removed-panes →
  removed-tabs → removed-workspaces (`client.rs:621-704`) is correct for a snapshot that both adds
  and removes. A pane arriving for a tab the same diff retires materializes a tab that the
  trailing tab-removed event then closes; the only exception is finding 6.
- **Unmount purge coverage.** Both `end_federation_mount` sites (`api/workspaces.rs:559-562`,
  `:1021-1025`) now purge the tab and workspace indexes alongside the pane index. Note (pre-existing,
  unchanged): the TUI's own close paths (`input/navigate.rs:1646`, `input/modal.rs:753`, `:837`)
  call `AppState::close_selected_workspace` directly, bypassing `end_federation_mount` and all
  purges — out of scope for this branch but adjacent to finding 4.
- **Protocol.** 5→6 bump is required (two new top-level variants) and documented against the
  released tags; both variants routed to `Channel::Control`; the skew rejection is tested with a
  `const` floor assert.
- **Serving host.** `CreateWorkspace` reuses `Method::WorkspaceCreate` with `focus: false`, never
  steals host focus (asserted), and replies `Failed` on a gone/dropped actor rather than dropping
  the peer's request (asserted).

## Test quality

Substantive, not tautological. The new tests assert real placement (`find_tab_index_for_pane`),
labels, index contents, `public_pane_number` reachability, and run
`Workspace::assert_invariants_for_test` / `AppState::assert_invariants_for_test`, including the
adversarial-identity case CLAUDE.md requires for identity work. Gaps worth closing:

1. No two-concurrent-mount test (findings 3 and 7 both live there).
2. No test that a wrong-origin removal leaves the **index** intact (finding 3).
3. No test for the resync coalescing race (finding 1).
4. No test for `prompt_new_workspace_name = true` (finding 2); the existing test's doc comment
   asserts coverage that does not exist.
5. No test closing a mirrored tab/last-tab locally (finding 4).

## Recommended actions (priority order)

1. Add a `resync_dirty` companion to `resync_in_flight` and re-request on `SnapshotResponse`
   (finding 1) — otherwise the headline feature fails silently under normal load.
2. Route the named-create dialog path through the federation redirect, or gate the redirect on
   "cwd was not user-supplied" rather than `cwd.is_none()`; fix the false test doc comment
   (finding 2).
3. Move both index `remove` calls below their origin checks and add an index-survival assertion to
   the wrong-origin tests (finding 3).
4. Give `tab.close` the same federation treatment `tab.create` and `pane.close` have — refuse, or
   route over the wire, and never group-close a mount from a tab close (finding 4).
5. Fall back to `mirror.workspaces()` when the pending announcement is missing (finding 5).
6. One-line `workspace_id` filter on the tab lookup, and correct `workspace_id` on an existing
   entry in `handle_federation_resync_tab_created` (finding 7).
7. Optional: prune the tab index when a pane removal collapses a tab (finding 8).

## Unresolved questions

- Is `prompt_new_workspace_name` expected to be on for the target users? That decides whether
  finding 2 is a blocker or a follow-up.
- Is closing a mirrored workspace's last tab meant to unmount the whole host, or to close just
  that mirrored workspace? Finding 4's fix depends on the intended gesture.
- Should a mirrored tab close be forwarded to the remote (a `TabCloseRequest`, protocol 7), or
  stay refused like `tab.create` does today?

Status: DONE
Summary: Traced every insert/purge site of the three resync indexes, both close paths, the
create/response ordering, and the new wire surface; the branch compiles and its 221 federation
tests pass, but nine real defects remain — two HIGH (a silent lost-workspace race in the resync
coalescing, and the create redirect being inert under `prompt_new_workspace_name`), three MEDIUM
(purge-before-authorization, `tab.close` still group-closing a whole mount, and unrepairable
workspaces), and four LOW.
CONFIRMED findings: 9 (2 HIGH, 3 MEDIUM, 1 LOW/MEDIUM, 3 LOW). SUSPECTED-only findings: 0.
