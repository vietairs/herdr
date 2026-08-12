# Blindspot scan: forward workspace/tab close over federation

Read-only. No files edited.

## 1. Echo / feedback loop — PROVEN, #1 hazard, likelihood HIGH x damage HIGH

The pane-close template avoids the echo loop by never mutating locally until the
host confirms (`dispatch_remote_pane_close`, `src/app/api/panes.rs:345-417`):
it sends `ClosePaneRequest` and returns `remote_close_pending` — no local
`ws.close_pane` call. The real local teardown happens only in
`handle_federation_close_pane_ready` (`src/app/creation.rs:1187-1271`), and
`handle_federation_resync_pane_removed` (a host-cascade echo) is idempotent
because it looks the pane up by id first and no-ops if already gone
(`src/app/creation.rs:2111+`, confirmed via `find_pane`/`self.state.workspaces
.iter().position` returning `None`).

`handle_workspace_close` (`src/app/api/workspaces.rs:1001-1083`) and
`handle_tab_close`'s federated branch (`src/app/api/tabs.rs:279-308`) do **not**
follow this pattern — they mutate local state **immediately, optimistically**,
then emit `WorkspaceClosed`/`TabClosed` synchronously and return `Ok` before any
host round-trip could happen. If "forward on close" is bolted onto these
handlers as-is (fire a wire message alongside the existing optimistic local
close), the resulting host-side cascade comes back through the *same* resync
handlers already in the code — `handle_federation_resync_workspace_removed`
(`src/app/creation.rs:1780-1845`) and `handle_federation_resync_tab_removed`
(`src/app/creation.rs:1977-2075`) — and I confirmed **both are already
idempotent**: they look the workspace/tab up by id
(`self.state.workspaces.iter().position(...)`) and early-return if it is
already gone locally. So a straightforward "send + also close locally now"
addition will *not* double-free or panic — the resync echo becomes a silent
no-op, matching the pane precedent.

The real hazard is subtler: **`close_single_workspace_at` and
`Workspace::close_tab` are the same low-level primitives called by both the
user-initiated close handlers AND the resync (host-initiated) removal
handlers** (`handle_federation_resync_workspace_removed` calls
`close_single_workspace_at`; `handle_federation_resync_tab_removed` calls
`ws.close_tab`). If the forward-to-host wire send is implemented at that
shared low level instead of gated to the user-initiated entry points only
(mirroring `dispatch_remote_pane_close`'s placement), a host-initiated removal
arriving via resync would re-trigger the same code path and echo a redundant
`WorkspaceCloseRequest`/`TabCloseRequest` right back at the host — for
something the host already tore down. That is a live, easy-to-introduce bug
class specific to this change, not present in the pane template because pane
close has no resync-driven local-removal call site sharing code with the
user-close call site in the same way.

**Recommendation embedded as a finding, not a proposal**: whatever design is
chosen must NOT put the wire-send inside `close_single_workspace_at` /
`Workspace::close_tab` directly. It must live only in the top-level user-close
handlers, before or instead of the current optimistic local mutation — exactly
where `dispatch_remote_pane_close` lives relative to `handle_pane_close`.

## 2. Unmount vs delete — PROVEN, likelihood HIGH x damage HIGH if done wrong

Enumerated every local-removal path that is NOT "user wants to delete on host":

- **Link drop / mount ended** — `handle_federation_mount_ended`
  (`src/app/api/workspaces.rs:472-580`+). Fenced by `connection_epoch` +
  `mount_generation` (lines 480-512), then calls
  `self.state.close_selected_workspace()` (line 567) — the **whole
  worktree-space group**, i.e. every sibling workspace of that mount at once,
  not `close_single_workspace_at` per workspace. Correct today because there
  is no forwarding; a naive change must NOT wire a host-close send into this
  path — the host is (by definition, that's why the mount ended) unreachable
  or the link is already gone. A forward call here would either silently fail
  (fine) or, worse, race a reconnect's fresh `out_tx` and send a stale close
  down a new link. **This path must stay forwarding-free.**
- **`end_federation_mount`** (`src/app/state.rs:1828`+) — pure local mirror
  registry teardown, called by both the mount-ended path above and by
  `handle_workspace_close` itself (`workspaces.rs:1058`) when no sibling
  remains. Not itself a removal path but sits directly upstream of one; forward
  logic must be sequenced so it fires before this is called, using the still-live
  `out_tx`, not after.
- **8eea3a59 "retire one federated workspace without ending its mount"** — this
  is exactly the current `handle_workspace_close` code I read
  (`workspaces.rs:1001-1083`). It already distinguishes "close one workspace of
  a multi-workspace mount" (keep mount, `close_single_workspace_at`) from "close
  the mount's last workspace" (end mount, `close_selected_workspace` group-close
  for the non-federated path / `close_single_workspace_at` for the federated
  one). This is genuinely a user-delete intent both ways, so forwarding is
  correct here — but the sibling-counting logic (`siblings_remain`,
  `workspaces.rs:1053-1059`) must run BEFORE deciding whether to also end the
  mount locally after the host confirms, not naively per this-workspace-only.
- **App shutdown** — no dedicated teardown code found (searched
  `src/app/mod.rs`, `src/main.rs` for shutdown/Drop hooks; none touch
  workspaces). The process just exits; the host's own idle/keepalive or link
  read failure is what tells it the client vanished. Nothing to gate here, but
  also confirms there is no "graceful unmount" signal today distinct from a
  link drop — so a future close-forward feature must not assume shutdown will
  route through the same code as a user-driven close.
- **Mount failure (never reached "live")** — different code path entirely
  (`claim_abandoned_remote_mount`, pending-target resolution in
  `workspaces.rs:450-459`); no materialized workspace exists yet, so it is out
  of scope for close-forwarding by construction.

Net: exactly one of these five paths (8eea3a59's own case) is a genuine
"forward this" case; the other four are local-only and must stay that way.

## 3. Last-pane / last-tab cascade — PROVEN

Host-side, `FederationCommand::ClosePane` reuses `Method::PaneClose`
end-to-end (`src/server/federation_actor.rs:460-483` dispatches into
`app.handle_api_request_after_internal_events_drained`), and that handler's
cascade is the same local one at `src/app/api/panes.rs:1918-1923`: closing the
last pane closes the tab/workspace (`ws.close_pane` returning `true` triggers
`close_selected_workspace()`). A test proves this end-to-end already:
`close_pane_against_a_known_target_pane_closes_it_and_replies_ok`
(`federation_actor.rs:1172-1208`) asserts a single-pane workspace vanishes
entirely after one `ClosePane` command. So the host's real cascade semantics
are: last-pane-in-tab closes the tab, last-tab-in-workspace closes the
workspace — driven by the *same* `Method::*` handlers a hypothetical
`Method::WorkspaceClose`/`Method::TabClose` reuse would need to call.

**If the client forwards a workspace-close request for a workspace the host
has already cascade-closed** (e.g. the user closed the workspace's last pane
locally first, that got forwarded per pane-close forwarding, the host cascaded
away the whole workspace, and *then* something on the client still sends an
explicit `WorkspaceCloseRequest` for it): the host's `Method::WorkspaceClose`
handler at `workspaces.rs:1001-1007` does `parse_workspace_id` +
`self.state.workspaces.get(index).is_none()` -> `workspace_not_found` error.
This is a clean, structured error (not a panic, not a silent no-op, not a
wrong-target close) **only if the id-to-index parse fails cleanly** — see
finding 4 for why that is not fully guaranteed under id reuse, and see finding
1 for why this scenario (workspace already gone) should be made unreachable by
correct sequencing rather than relied upon to fail safely.

## 4. ID reuse / staleness — PROVEN not reusable within a session; caveat on restart

`generate_workspace_id` (`src/workspace.rs:118-127`) is a monotonic
process-global `AtomicU64` counter (`NEXT_WORKSPACE_ID`), never decremented,
never recycled on close. Within one server process's lifetime a workspace or
tab id is never reused, so "close request in flight while host reuses the id"
cannot happen against a live, unrestarted host. The only staleness vector is a
**host process restart**, which resets the counter — but that is already fenced
at the mount/handshake layer by `ServerInstanceId` (`src/remote/federation/
id.rs:64-84`, doc comment states it fences exactly this: "federation
handshake/mount fence stale traffic against it"). I did not verify that every
new-command reply path (a hypothetical `WorkspaceCloseResponse`) would
actually be fenced by `ServerInstanceId` the same way `ClosePaneResponse` is —
worth confirming during design, not assumed.

## 5. Concurrency — SUSPECTED gap, no layout-level fence found

Confirmed: `FederationCommand::SendInput`/`Resize`/`NudgeRedraw`/
`ReleaseTerminalSize` all gate on `lease.is_mounted_controller(epoch, connid)`
(`federation_actor.rs` dispatch, lines ~330-420) — but `ClosePane` does **not**
check `is_mounted_controller` at all (`federation_actor.rs:460-483` — no lease
check visible in the `ClosePane` arm, unlike the input/resize arms three cases
above it). That means **any** connected federation peer (not just the single
mounted controller) can currently close a pane via `ClosePaneRequest`
today — I could not find a lease check gating it, only that it runs through the
normal local API handler with no additional authz. If true, this is an
existing gap, not new — but a hypothetical workspace/tab close forward would
inherit the exact same missing gate unless explicitly added. Separately: the
host's own local TUI user closing the same workspace at the same moment the
client's forwarded close arrives races through the identical
`Method::WorkspaceClose`/`TabClose` handler with no additional lock beyond
normal single-threaded `App` mutation (the actor processes one
`FederationCommand` at a time via the server event loop, so no data race, but
no reconciliation either — last writer wins, and the loser gets
`workspace_not_found`, which is the same "clean error" outcome as finding 3,
same caveat applies about relying on it). I found no generation-counter/fence
for layout mutations analogous to the terminal-output generation fence
mentioned in the brief — layout ops rely entirely on synchronous single-actor
ordering, not an explicit fence value.

## 6. Multi-workspace mount — PROVEN, sibling logic already isolated per-workspace

8eea3a59 (`workspaces.rs:1001-1083`, read in full) already handles this
correctly for the local-only case: `siblings_remain` (lines 1053-1059) scans
all workspaces for another with the same `host_key` before deciding whether to
`end_federation_mount`; `close_single_workspace_at` (not the group
`close_selected_workspace`) removes exactly one. A regression test already
covers it: `closing_one_of_several_federated_workspaces_keeps_siblings_and_mount`
(`workspaces.rs:2644`). Adding forwarding on top must preserve this ordering —
specifically, the "is this the mount's last workspace" check must happen
**before** any decision to also send an unmount-style signal to the host (there
is currently no such signal; a naive implementation could conflate "close this
one workspace" with "end the mount" if it piggybacks the wrong request shape).
No test today exercises "forward-close one of several federated workspaces and
confirm the *host* also keeps the siblings" — that's a new host-side
interaction the current suite cannot catch (see finding 7).

## 7. Test blind spots

None of the following exist today (grepped `workspaces.rs`/`tabs.rs`/
`federation_actor.rs` test modules for close-forwarding-shaped names — nothing
matched beyond the pane-close template and the local-only 8eea3a59 tests):

- A `federation_actor.rs`-level test for a hypothetical
  `FederationCommand::CloseWorkspace`/`CloseTab` mirroring
  `close_pane_against_a_known_target_pane_closes_it_and_replies_ok` — proving
  the host-side cascade (workspace-close -> if last tab, cascades correctly;
  tab-close -> if last tab, becomes workspace-close) with the SAME assertions
  used for panes.
  Also missing: unauthorized-caller lease check for `ClosePane` at all (finding 5) — a
  regression test proving only `is_mounted_controller` can close is absent for
  pane close today and would be needed for any new close-forward command too.
- A client-side idempotence test that sends a resync-workspace-removed /
  resync-tab-removed event for an id **the local forwarding path is
  simultaneously waiting on a pending-close response for** — proving no double
  `WorkspaceClosed`/`TabClosed` emission and no stale-pending-map entry. The
  pane-close precedent has this exact test shape via `pending_remote_closes`
  (`app/mod.rs:155-169`) but workspace/tab close has no equivalent pending map
  yet, so there is nothing to test.
- A test proving the echo-loop finding from item 1 concretely: seed a
  federated multi-workspace mount, forward a workspace close, THEN deliver a
  `FederationResyncWorkspaceRemoved` for the same id before any pending-close
  response, and assert `close_single_workspace_at` is not invoked twice / no
  second `WorkspaceCloseRequest` is sent. This is the single most valuable new
  test and does not exist because the forwarding code does not exist yet.
- A test for finding 2's "must not forward on mount-ended/link-drop" — assert
  `handle_federation_mount_ended` never touches whatever pending-close/out_tx
  mechanism a new implementation adds.
- A test for `close_single_workspace_at` being invoked from a resync path vs a
  user-initiated path staying observably distinct (e.g. via a marker/flag param
  or by asserting no wire send happens on the resync-triggered call) — this is
  the regression test that would have caught finding 1's shared-primitive
  hazard directly.

These fit the repo's stated pattern: `AppState::test_new()`/`Workspace::
test_new()` state-level tests without PTYs, following the exact style already
used at `workspaces.rs:2644` and `federation_actor.rs:1172`.

## Ranked summary (likelihood x damage)

1. **Echo loop via shared low-level close primitives** (finding 1) — HIGH x
   HIGH. Concrete, easy to introduce, not caught by existing tests. Fix is
   architectural: forward only from the top-level user-close entry points,
   never from `close_single_workspace_at`/`Workspace::close_tab` themselves.
2. **Missing lease/authz check on close commands** (finding 5) — MEDIUM
   likelihood (existing gap, not introduced by this change) x HIGH damage (any
   federation peer, not just the mounted controller, can already close panes;
   a new close-forward command would inherit this unless explicitly gated).
   PROVEN as a gap in `ClosePane`'s dispatch arm; unverified whether it's
   exploitable given other connection-level gating I did not trace exhaustively.
3. **Forwarding fired from link-drop/mount-ended path** (finding 2) — MEDIUM x
   HIGH. Straightforward to avoid once named, but the existing code has many
   call sites that "look like" a close and only one of them is a real delete
   intent.
4. **Optimistic local close diverging from host's actual cascade outcome**
   (finding 3) — LOW-MEDIUM x MEDIUM. Resolves to a clean `workspace_not_found`/
   `tab_not_found` error today rather than corruption, but only if id parsing
   stays well-behaved (finding 4) and only if the client doesn't misinterpret
   that error as something worse.
5. **ID reuse** (finding 4) — LOW x LOW under normal operation (monotonic
   counter, already fenced by `ServerInstanceId` at the mount layer); the open
   item is whether a new command's response path reuses that same fence.

## Unresolved questions

- Does `ClosePaneRequest`'s server-side dispatch actually enforce
  `is_mounted_controller` somewhere upstream of `federation_actor.rs`'s
  `dispatch_command` (e.g. at the accept-loop / connection layer in
  `federation_accept.rs`) that I did not trace far enough to find? If so,
  finding 5's severity drops; if not, it's a pre-existing gap independent of
  this feature.
- Is there a design intent for what "close a mirrored workspace" should even
  mean when host-cascade semantics disagree with client-side state at the
  moment of the request (finding 3) — surface the host's authoritative error
  to the user, or treat it as already-succeeded (matching the pane-close
  precedent's "already gone, treat as idempotent success" choice)?

Status: DONE
Summary: The dominant hazard is that `close_single_workspace_at`/`Workspace::close_tab` are shared primitives called by both user-initiated close handlers and the host-cascade resync-removed handlers, so wiring the host-forward send into that shared layer (rather than only the top-level user-close entry points, as the pane-close template does) would echo a close request back at the host for removals the host itself just reported. A secondary, pre-existing gap is that `ClosePane`'s federation dispatch has no `is_mounted_controller` lease check unlike sibling input/resize commands, a gap any new close-forward command would inherit.
