# Root cause: federated remote workspace's 2 tabs render as 1 tab + 2 split panes

Date: 2026-08-11. Read-only investigation, no code changed.

## Symptom
A herdr server on appn-ltu-vm-105 has a workspace with 2 `Tab`s. Mounted via
federation from a Mac herdr client, the mounted workspace materializes as
ONE local `Tab` containing 2 split `PaneState`s instead of 2 `Tab`s.

## Confirmed root cause

The **post-mount resync pane-created path** structurally cannot create a new
tab — it drops the remote pane's `tab_id` before the local materializer ever
sees it, and always splices the new pane as a horizontal split into whichever
tab is currently active locally.

### Evidence chain

**1. The wire data has the tab identity.** `PaneInfo` (part of every
`SessionSnapshot`/`MountSnapshot`) carries `tab_id`:

```
src/api/schema/panes.rs:402    pub tab_id: String,
```

**2. It is discarded at the one construction site for resync-created panes.**
`materialize_resync_pane` receives the full `PaneInfo` (with `.tab_id`) but
only copies `workspace_id` and `pane_id` into the event it hands back to
`App`:

```
src/remote/federation/client.rs:993-1002
            let ready = crate::events::FederationResyncPaneCreated {
                origin: ctx.origin.clone(),
                workspace_id: pane_info.workspace_id,
                pane_id: pane_info.pane_id,
                local_pane_id: pane_id,
                terminal_id,
                terminal,
                runtime,
                pane_state,
            };
```

`pane_info.tab_id` is never read here. The event type itself has no field to
carry it:

```
src/events.rs:346-365
pub struct FederationResyncPaneCreated {
    pub origin: crate::remote::federation::id::HostKey,
    pub workspace_id: String,
    pub pane_id: String,
    pub local_pane_id: crate::layout::PaneId,
    pub terminal_id: crate::terminal::TerminalId,
    pub terminal: crate::terminal::TerminalState,
    pub runtime: crate::terminal::TerminalRuntime,
    pub pane_state: crate::pane::PaneState,
}
```
(no `tab_id` member at all)

**3. The handler falls back to "whatever tab is active" and splits.**
`App::handle_federation_resync_pane_created`, with no tab identity available,
uses the workspace's *currently active* local tab and inserts the new pane as
a **horizontal split** next to the focused pane:

```
src/app/creation.rs:1316-1338
        let ws = &mut self.state.workspaces[ws_idx];
        let tab_idx = ws.active_tab;
        let Some(target_pane_id) = ws.focused_pane_id() else { ... };
        ...
        let moved = crate::workspace::MovedPane { pane_id: local_pane_id, pane_state };
        if ws
            .insert_moved_pane_into_tab(
                tab_idx,
                target_pane_id,
                moved,
                ratatui::layout::Direction::Horizontal,
                0.5,
            )
            .is_err()
        { ... }
```

There is no branch anywhere in this handler that creates a new `Tab`
(`create_tab_from_existing_pane`) — it can only ever splice into an existing
tab. So any pane that is discovered through this path — rather than through
the initial `MountSnapshot` handled by `materialize_federation_mount` — loses
its tab boundary and shows up as a split in whichever tab happens to be
active, exactly matching "2 tabs become 1 tab with 2 split panes."

**4. `reconcile_by_diff`/`materialize_resync_pane` is a live, reachable code
path, not dead code.** It fires whenever the client re-fetches a snapshot
after a `Gap`/`Reset` on the event channel (module docs, `reducer.rs:1-24`)
and after every initial mount (`client.rs:555-630`, `reconcile_by_diff` at
`client.rs:611`, `materialize_resync_pane` call at `client.rs:623`). A brand
new mount over an SSH tunnel is a plausible place for an early `Gap`/`Reset`
(first few frames racing the initial `MountSnapshot` fetch), or for a pane
that was created on the remote a moment after the `MountSnapshot` was
captured but before the live event stream caught up — either lands here.

## Hypotheses evaluated (from the task)

**H1 — server only ever reports one tab per workspace.** REFUTED.
`session_snapshot()` builds `tabs` by iterating **every** `tab_idx` of
**every** workspace unconditionally:

```
src/app/api/session.rs:33-43
        for (ws_idx, ws) in self.state.workspaces.iter().enumerate() {
            workspaces.push(self.workspace_info(ws_idx));
            for tab_idx in 0..ws.tabs.len() {
                if let Some(tab) = self.tab_info(ws_idx, tab_idx) { tabs.push(tab); }
                ...
            }
        }
```
This is the same, non-federation-specific code path used for the regular
`session.snapshot` JSON API and for the federation `mount()`/`current_snapshot`
call (`src/server/federation_actor.rs:285,502-516`) — no per-mount filtering
exists anywhere between `App::session_snapshot()` and the wire
`MountSnapshot` (`src/server/federation_accept.rs:317`,
`src/remote/federation/protocol/mod.rs:159-163`).

**H2 — snapshot has 2 tabs but `PaneInfo.tab_id` isn't namespaced
consistently with `TabInfo.tab_id`, so the client-side filter drops the
second tab.** REFUTED for the initial-mount path.
- Server side: `tab_info()` and `pane_info()` both derive `tab_id` from the
  *same* function `App::public_tab_id(ws_idx, tab_idx)` for the *same*
  `(ws_idx, tab_idx)` pair (`src/app/creation.rs:328-350` for `tab_info`,
  `:433-482` for `pane_info`, both calling `src/app/ids.rs:19-25`). A pane's
  `tab_idx` comes from `ws.find_tab_index_for_pane(pane_id)`
  (`src/workspace.rs:1239-1243`), which is keyed off the globally-unique,
  process-lifetime-monotonic `PaneId` counter
  (`src/layout.rs:14,16-19`) — no collision path found for a live,
  non-restarted server.
- Client side: `namespace_tab`/`namespace_pane` both call the same `map_in`
  on the tab id (`src/remote/federation/reducer.rs:424-430,440-464`), so a
  matching raw `tab_id` always namespaces to the same public id.
- The materializer's own filter (`pane.tab_id == tab_info.tab_id`,
  `src/app/creation.rs:604-610`) is therefore consistent for panes that
  arrive via the initial `MountSnapshot`/`apply_snapshot` path.
- H2's *mechanism* (tab_id carried by pane but dropped/mismatched) **is**
  what happens, but only on the resync path (see confirmed root cause above),
  not via a namespacing bug in the initial-mount path.

**H3 — snapshot/materialization correct, but a later reconcile collapses
tabs.** CONFIRMED (this is the root cause, restated): `reconcile_by_diff`
(`src/remote/federation/reducer.rs:373-390`) itself does not collapse
anything — `reconcile_tabs` (`reducer.rs:517-567`) correctly diffs tabs by
namespaced id and pushes `TabCreated`/`TabRenamed`/`TabClosed` hub events.
The collapse happens one layer up, in how the *client* consumes
`reconcile_panes`'s `created_panes` output (`materialize_resync_pane` +
`handle_federation_resync_pane_created`, evidence above) — it never
consults `reconcile_tabs`'s output or the pane's own `tab_id` at all.

**H4 — client's `apply_snapshot`/reducer loop drops tabs.** REFUTED.
`apply_snapshot` (`reducer.rs:252-270`) unconditionally inserts every
`workspace`/`tab`/`pane` from the incoming `SessionSnapshot` into the
mirror's maps, keyed by namespaced id — no filtering, no `continue`, no
early return. `materialize_federation_mount`'s own tab loop
(`src/app/creation.rs:585-753`) correctly calls
`create_tab_from_existing_pane` for every `tab_idx != 0`
(`creation.rs:652-659,696`) and emits `TabCreated` for it
(`creation.rs:751-753`). This path, exercised by the existing test
`successful_mount_materializes_into_rendered_workspace_tab_and_two_panes`
(`creation.rs:1629-1707`), is logically correct for tabs present in the
*initial* `MountSnapshot`.

## Secondary gap explicitly confirmed (per task's "also check")

There is **no client-side handler that turns a post-mount
`EventKind::TabCreated` resync event into a materialized local `Tab`.**
`reconcile_tabs` only pushes the event onto `EventHub`
(`reducer.rs:543-549`); grepping every federation resync consumer in
`src/app/` (`src/app/api.rs:228,234`, `src/app/creation.rs:1274,1375`) shows
only `handle_federation_resync_pane_created` / `handle_federation_resync_pane_removed`
exist — no `handle_federation_resync_tab_created`/`_removed` equivalent.
This is the same underlying gap as the main root cause (tab identity is
tracked in the mirror/hub metadata layer but never wired into the real
`Workspace`/`Tab` layout the TUI renders), just observed from the "remote
creates a brand new tab after I'm already mounted" angle instead of the
"pane discovered via resync" angle.

## Minimal fix location(s)

1. `src/events.rs:346-365` — add a `tab_id: String` field to
   `FederationResyncPaneCreated` (the namespaced/public tab id, same form
   `RemoteMirror::tabs()` keys on).
2. `src/remote/federation/client.rs:993-1002` — populate it from
   `pane_info.tab_id` (already in scope, currently discarded) when
   constructing the event.
3. `src/app/creation.rs:1274-1365` (`handle_federation_resync_pane_created`)
   — resolve the *actual* target tab from `tab_id` instead of hardcoding
   `ws.active_tab`:
   - if a local tab already exists for that `tab_id` (needs a lookup,
     analogous to how `materialize_federation_mount` finds tabs by id —
     likely requires tracking a `tab_id -> local tab_idx` map similar to
     `remote_resync_pane_index` for panes), splice the pane into *that*
     tab's layout instead of the active one;
   - if no local tab exists yet for that `tab_id` (this is the "remote
     created a new tab after mount" case — the secondary gap), create one
     via `Workspace::create_tab_from_existing_pane` and emit
     `TabCreated`/`PaneCreated`, mirroring what
     `materialize_federation_mount` already does for tab_idx != 0
     (`creation.rs:652-659,751-753`).
4. Symmetric handling should be added for tab removal
   (`EventKind::TabClosed` from `reconcile_tabs`, `reducer.rs:557-566`) so a
   remote tab closed after mount also removes the local tab, not just its
   panes — currently there is no consumer for that event either.

No change is needed to `session_snapshot()`, `tab_info()`, `pane_info()`,
`apply_snapshot()`, or `materialize_federation_mount()` — all verified
correct for the tabs-present-at-initial-mount case.

## Protocol wire version

**No `src/protocol/wire.rs::PROTOCOL_VERSION` bump needed.** The wire
already carries everything required: `PaneInfo.tab_id`
(`src/api/schema/panes.rs:402`) is already part of `SessionSnapshot`/
`MountSnapshot` and already reaches the client via `reconcile_panes`'s
`created_panes: Vec<PaneInfo>` (`reducer.rs:569-...`, `ReconcileDiff`
struct at end of reducer.rs). The bug is purely that a *local, in-process*
event struct (`FederationResyncPaneCreated`) drops a field that was already
available — not a wire/protocol gap.

## Unresolved / what would confirm this beyond static analysis

- I could not obtain a live wire capture from appn-ltu-vm-105 to prove
  *which* trigger (an early Gap/Reset racing the initial mount, vs. the
  second tab being created moments after the `MountSnapshot` fetch, vs. some
  other resync trigger) actually fired for this specific report. The code
  path is confirmed structurally incapable of preserving tab boundaries for
  ANY pane it handles, regardless of trigger, so pinpointing the trigger is
  not required to confirm the defect, but would confirm it is *the* cause
  for this specific incident rather than a latent, not-yet-hit defect.
- No existing test exercises a 2-tab mount scenario at all (only
  `successful_mount_materializes_into_rendered_workspace_tab_and_two_panes`,
  which is deliberately 1 tab + 2 split panes,
  `src/app/creation.rs:1608-1621,1629`) and no test exercises
  `materialize_resync_pane`/`handle_federation_resync_pane_created` with a
  pane whose `tab_id` differs from the workspace's active tab — confirming
  the gap would need a new test asserting a resync-discovered pane for a
  *different* tab creates a new tab rather than splitting the active one.
- Whether `reconcile_by_diff` legitimately ran early in this mount's
  lifetime (i.e., whether a Gap/Reset happened right after connect) is
  unconfirmed; would need `HERDR_LOG=herdr=debug` on the client for a fresh
  repro mount against appn-ltu-vm-105, watching for `GapDetected`/
  `ResetRequired` log lines shortly after `MountSnapshot`.

Status: DONE
Summary: Confirmed root cause is a client-side gap, not a server/protocol bug — `materialize_resync_pane` (src/remote/federation/client.rs:993-1002) discards `PaneInfo.tab_id` when building `FederationResyncPaneCreated` (src/events.rs:346-365, no tab_id field), so `handle_federation_resync_pane_created` (src/app/creation.rs:1316-1338) always splices any resync-discovered remote pane into the workspace's currently-active local tab as a horizontal split instead of its real remote tab; the initial-mount path (`materialize_federation_mount`, `session_snapshot`, `apply_snapshot`) was verified correct by direct code reading and existing tests, and H1/H2/H4 are refuted with citations above.
Concerns: Root cause proven via static evidence chain, not a live repro against appn-ltu-vm-105 (no VM access in this session); trigger timing (which resync fired) is unconfirmed. Fix also needs symmetric tab-closed handling and a tab_id->local-tab-index tracking structure, which is real added complexity beyond a one-line field add.
