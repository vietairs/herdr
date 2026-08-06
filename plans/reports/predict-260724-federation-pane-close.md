# Predict debate: federation pane-close sync (Gap A + Gap B)

Read-only analysis. No code changed. Scope: add `ClosePaneRequest`/`ClosePaneResponse`
(client->server, mirroring `SplitPaneRequest`/`SplitPaneResponse`) for Gap A, and make
server-side pane closes propagate live to mounted clients for Gap B.

## Evidence gathered before the debate

- `src/remote/federation/protocol/mod.rs`: `FEDERATION_PROTOCOL_VERSION = 3`. Doc comment
  says v3 never shipped in a release and `SnapshotRequest`/`SnapshotResponse` were added to
  it WITHOUT a bump on that basis. `SplitPaneRequest`/`SplitPaneResponse` use a bare-`u64`
  `request_id` pairing on `Channel::Control`, no pending-request dedup map visible in
  `client.rs` beyond the response match itself.
- **Gap B looks already-mitigated, not open.** `src/app/api/panes.rs::close_pane` (the
  server-owned CLI/API close path) already calls `self.emit_event(EventKind::PaneClosed)`.
  `src/remote/federation/client.rs`'s drive loop treats `PaneClosed` as a *structural* event
  kind, triggers a coalesced `SnapshotRequest`/`SnapshotResponse` round trip, diffs it via
  `reconcile_by_diff`, and for every `removed_pane_ids` entry fires
  `AppEvent::FederationResyncPaneRemoved` to tear the pane out of the mounted client's live
  layout. This is exactly commit `b5cb8ce8` ("mirror server-side pane changes to mounted
  clients", 2026-07-22). The task brief's claim that "only a fresh MountSnapshot syncs it"
  may be stale relative to that commit — **verify live before treating Gap B as unfixed**;
  the real remaining work may be Gap A only, plus tightening the resync round-trip latency.
- Pane identity: `App::parse_pane_id` resolves `"<ws>:p<number>"` (workspace-scoped,
  numbered) or raw `p_<ws>_<raw>` forms; `RawSessionSnapshot` carries a monotonic
  `next_public_pane_number` counter, meaning close-then-reuse-the-same-number-for-a-new-pane
  is not the default allocation behavior (lowers identity-reuse severity vs. a naive scheme,
  but not independently confirmed to never wrap/reset).
- Federation-materialized workspaces are explicitly filtered out of `session.json` capture
  (`is_federation_materialized` in `src/persist/snapshot.rs`) — the mounting client never
  persists remote panes to disk, so "stale session.json resurrects a closed remote pane" is
  not a real risk on the client side. The equivalent risk is on the **serving** side: local
  close already goes through the normal `schedule_session_save()` debounce, same as any
  local pane close — no new persistence surface is added by this fix.

## 5-persona debate

**Architect** — Reuse of the `SplitPaneRequest` bare-`u64` pattern is the right shape (no
new RPC framework). But `ClosePaneRequest` should reuse the *existing* `PaneClosed` emission
path server-side rather than inventing a second event source: on success, the server should
emit `EventKind::PaneClosed` (already wired to the mirror/resync path) AND answer the control
request — do not let the `ClosePaneResponse` be the only signal, or non-federation-aware code
paths (other future observers of the event hub) miss the close.

**Security** — `target_pane_id` is attacker/peer-controlled input on the wire, same as
`SplitPaneRequest`'s. `close_pane` must resolve it through the same `parse_pane_id` validation
already used for the JSON API (workspace-scoped, existence-checked) — never trust it as a
direct index into `layout::PaneId`. No new privilege boundary is crossed (a mounting client
already gets to command splits on the serving host); closing is not more dangerous than
splitting, but a malformed/forged id must fail closed (`ClosePaneResponse::Failed`), not panic.

**Protocol-compat** — Two concerns. (1) If this repo has already *shipped* a v3 federation
build to any real mounted host (check the fork's release tags / VM relay usage before relying
on "v3 never released" — the doc comment's stated justification for skipping a bump was
specifically "no deployed peer exists"; if that's no longer true on this fork, a live peer on
v3-without-`ClosePaneRequest` will hit an undecodable enum variant from a newer peer and the
link will fault, not gracefully degrade). (2) Interaction between the new client->server
`ClosePaneRequest` and the existing resync-driven teardown: a server-side close from either
path fires the same `PaneClosed` event, which the drive loop already coalesces
(`resync_in_flight` guard) — but the two "removal" signals (an explicit `ClosePaneResponse`
and a `FederationResyncPaneRemoved` from the SAME close) can each reach the mounted client's
layout-mutation code. That code must be idempotent against removing an already-removed pane.

**UX** — Symmetry with split matters: `SplitPaneResponse::Created` carries `focus: bool` so
the caller can request focus move. `ClosePaneResponse` needs no such field but the *client*
must still pick a sensible new focus locally the instant it sends the request (optimistic
local removal), then reconcile against whatever the server's authoritative event/resync
confirms — echoing the existing local `close_pane`'s "closing pane can also close the
workspace" cascade (`close_pane_would_close_workspace` / `confirm_implicit_worktree_group_close`)
correctly for a *remote* target, including the confirm-dialog case, is easy to under-scope.
Also: the already-known "ratio-only changes don't trigger federation resync" gap
(`client.rs:824-829` comment) means a close's sibling-rebalance (`balance_areas_after_removal`)
on the serving host will not reliably re-arrive at the mounted client — expect stale split
ratios after a remote close, same as the existing documented split-ratio staleness, not a
new regression.

**Maintainer** — Bump `FEDERATION_PROTOCOL_VERSION` only if the "no deployed v3 peer" premise
is still true; state the decision explicitly in the PR rather than silently following the
`SnapshotRequest` precedent, since the fork's release history differs from upstream. Keep the
change additive to `FederationMessage`/`Channel::Control`; do not touch existing variants.

## Top 5 concrete risks + mitigations

1. **Gap B premise may be stale.** `b5cb8ce8` already wires `PaneClosed` -> resync ->
   `FederationResyncPaneRemoved` teardown on the mount. *Mitigate*: live-test a server-side
   `herdr pane close` against an already-mounted client BEFORE writing any Gap-B code; if it
   already tears down, scope the fix to Gap A only and just tighten resync latency/dedup.
2. **v3 may already be deployed on this fork**, invalidating the "no bump needed" doc-comment
   rationale. *Mitigate*: check fork release tags / any live remote host running federation
   before deciding not to bump `FEDERATION_PROTOCOL_VERSION`.
3. **Double-close race**: explicit `ClosePaneResponse` and resync-driven
   `FederationResyncPaneRemoved` can both fire for the same close. *Mitigate*: make the
   mount-side pane-removal handler idempotent (no-op / logged, not panic, on an unknown
   pane_id), and treat "not found" from the server as a successful (already-closed) response,
   not an error, for idempotency against retries/duplicate clicks.
4. **Pane-identity resolution must go through `parse_pane_id`**, not a raw index, or a stale/
   forged `target_pane_id` from a lagging peer could target the wrong (recycled-slot) pane.
   *Mitigate*: reuse the exact validation `close_pane` already does for the JSON API; reject
   unresolved ids with `ClosePaneResponse::Failed`.
5. **Focus/layout fallout on remote close**: the confirm-dialog cascade
   (`close_pane_would_close_workspace`, worktree-group confirmation) and sibling-ratio
   rebalance exist for local closes but have no proven equivalent replay on the mount's local
   layout copy. *Mitigate*: explicitly decide (and test) what a remote close that would also
   close a workspace does for a mounted client — likely: server-side confirmation still
   applies (server owns the state), client just receives the eventual `WorkspaceClosed`/
   `PaneClosed` pair, same as `close_pane`'s existing `should_close_workspace` branch already
   emits.

## Unresolved questions

- Has this fork already shipped a federation v3 build (server or client) to any real,
  currently-running remote host? Determines the version-bump decision in risk 2.
- Is pane-number allocation (`next_public_pane_number`) ever reset or recycled within a
  workspace's lifetime (e.g. across a save/restore round trip), which would sharpen or soften
  risk 4?
