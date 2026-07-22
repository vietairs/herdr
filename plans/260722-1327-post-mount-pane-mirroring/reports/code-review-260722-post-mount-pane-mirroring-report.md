# Code Review: post-mount pane mirroring (uncommitted diff)

Verdict: **REQUEST_CHANGES**

## Critical

### C1 — Split-created pane double-materialized on the next resync (`src/remote/federation/client.rs`, `src/app/creation.rs`)
`SplitPaneResponse::Created` (existing pre-diff arm, `client.rs:621-701`) materializes a
local `TerminalRuntime`/`PaneId` for a remote split and drives
`AppEvent::FederationSplitPaneReady` → `App::handle_federation_split_pane_ready`
(`creation.rs:803`). **Neither path ever inserts the pane into `RemoteMirror::panes`**
(the mirror), and neither registers it in the new `remote_resync_pane_index`.

Meanwhile, the server-side split (`FederationCommand::SplitPane`) performs a real
`Method::PaneSplit` against the live `App`, which pushes a `PaneCreated` event onto the
shared `EventHub` exactly like any other structural mutation. That event is relayed to
the mounted client as an `Event(Frame)` with `kind: PaneCreated`
(`is_structural_event_kind` → true), which now (this diff) triggers a coalesced
`SnapshotRequest`/`SnapshotResponse` resync. The resync snapshot includes the new pane,
and because it was never recorded in `mirror.panes`, `reconcile_panes`
(`reducer.rs:482-524`, `None` arm) classifies it as **created** and returns it in
`ReconcileDiff::created_panes`. `drive_mount_channel`'s `SnapshotResponse` handler then
calls `materialize_resync_pane` unconditionally for every entry in `created_panes`
(`client.rs:69-80`), producing a **second** `TerminalRuntime`/`PaneId`/
`AppEvent::FederationResyncPaneCreated` for the same remote terminal, spliced into the
tab a second time via `insert_moved_pane_into_tab`.

Failure scenario: user on the mounting side splits a remote pane (`SplitPaneRequest`).
Ordering between the `SplitPaneResponse` control-channel reply and the `Event(Frame)`
carrying the same split's `PaneCreated` is not guaranteed (different channel classes
multiplexed on one connection). Either ordering ends with two live panes/runtimes for
one remote terminal — the tab shows a duplicate, one runtime is orphaned (its output
pump keeps running, never torn down, no `AppEvent` closes it), and `terminal_targets`/
`pane_info` lookups become ambiguous.

No test exercises the split → resync interaction; only split-alone and resync-alone
paths are covered (`client.rs` tests around line 1400 and 1784; `creation.rs` tests
after line 1570).

Fix direction: have `SplitPaneResponse::Created` (and/or
`handle_federation_split_pane_ready`) register the pane into `mirror.panes` (or into
`remote_resync_pane_index`) so `reconcile_panes` — or `materialize_resync_pane` — can
skip an id it already knows, mirroring the dedup the mount-time path gets for free via
`apply_snapshot`.

## Major

### M1 — resync-triggered pane teardown skips close-confirmation and pane-close side effects that a mounted App expects
`handle_federation_resync_pane_removed` (`creation.rs`) calls `ws.close_pane` directly,
bypassing the interactive `pane.close` gate intentionally (documented, acceptable per
the origin-decided-already reasoning) — but it does **not** guard against
`local_pane_id` already having been torn down by another path (e.g. a user-initiated
local close of the same federated pane racing the resync). `find_pane` returning
`None` is handled (early return), so this is likely fine; flagging only because C1's
duplicate-pane scenario compounds here: tearing down one of two duplicate `PaneId`s for
the same remote pane leaves the orphan behind with no reverse-index entry to ever
remove it.

## Minor

### N1 — stale `#[allow(dead_code)]` on `current_snapshot` (`src/server/federation_actor.rs:~325`)
The comment says "dormant until b0.4 wires the federation listener," but
`current_snapshot` is called from **two** live match arms in the same `dispatch` (both
`Mount` and the new `Snapshot`), not dead. Harmless today (allow on a used fn is not a
build error) but actively misleading — a future change removing one call site and
"cleaning up" the comment could hide that the other is still live. Drop the attribute
and the stale note.

### N2 — `SnapshotRequest` handling has no rate limiting
`reader_loop`'s `Ok(Some(FederationMessage::SnapshotRequest(_)))` arm
(`federation_accept.rs`) services every request unconditionally with a blocking
`SessionSnapshot` round-trip through the live `App`. `is_structural_event_kind`-driven
coalescing on the *client* side is the only backpressure; a client is
self-throttled, but nothing stops a non-conforming/compromised peer from spamming
`SnapshotRequest` and forcing repeated full-session serialization on the server actor
thread. Given the existing `EventsAfter`-precedent posture (no lease check needed,
handshake already gates who can open a mount), this is consistent with the codebase's
existing trust model, not a new hole — noting for awareness only.

## Verified as sound
- Resync-loop self-triggering (lens 1): `reconcile_by_diff` only pushes to the
  **local** client-side `EventHub` (sidebar-facing), never re-enters
  `drive_mount_channel`'s `FederationMessage::Event` match arm, so no local
  amplification loop. Coalescing (`resync_in_flight`) is correctly gated by inbound
  wire events only, reset on `SnapshotResponse` receipt; a burst of N structural frames
  produces exactly one `SnapshotRequest` (proven by test, `client.rs:1400+`).
- Origin binding (lens 2): both `handle_federation_resync_pane_created` and
  `_removed` check `worktree_space().key == "federation:{origin}"` before touching a
  workspace, with regression tests for the spoofed-origin case
  (`creation.rs::resync_pane_created_from_the_wrong_origin_is_dropped`).
  `remote_resync_pane_index` entries are inserted on create and removed on
  `_removed` (`.remove(&pane_id)`), including the should-close-workspace branch.
- Wire compat (lens 5): `FEDERATION_PROTOCOL_VERSION` deliberately NOT bumped, with a
  documented rationale that v3 has never shipped in a release — verified against
  `federation_accept.rs`/`client.rs` git history is plausible but not independently
  confirmed from tags in this pass; if v3 *has* shipped to any preview build, an old
  peer hitting the new `SnapshotRequest`/`SnapshotResponse` variants falls into each
  side's already-defensive "ignore unknown control frame"/"other" arms — not a hard
  decode error either way (checked `reader_loop`'s `Ok(Some(_other))` and `client.rs`'s
  catch-all `continue` arms).
- No production `unwrap()` introduced; `tracing::warn!`/`debug!` used consistently;
  new `#[cfg(unix)]` gating on `AppEvent` variants matches the existing
  `FederationSplitPaneReady` precedent (pre-existing, not introduced by this diff, that
  `client.rs`/`materialize_resync_pane` itself compiles unconditionally on all
  platforms yet references these `cfg(unix)` `AppEvent` variants and
  `TerminalRuntime::spawn_remote`/`pane::PaneRuntime::spawn_remote` — this is an
  existing pattern shared with the pre-existing `SplitPaneResponse::Created` arm, not
  new risk from this diff).

## Unresolved Questions
- Confirm empirically (not just via the code comment) whether
  `FEDERATION_PROTOCOL_VERSION = 3` has shipped in any tagged/preview release; the
  "no deployed peer" no-bump rationale depends on that being true.
- Does `pane_info(ws_idx, pane_id)` in `agents.rs`'s new `agent.start` event-emission
  path ever run before the pane's terminal is registered in `state.terminals` (i.e.
  could `pane_info` return `None` and silently drop the `PaneCreated` push)? Not
  traced in this pass — worth a follow-up read of `spawn_agent_workspace`/pane
  creation ordering.

Status: DONE
Summary: REQUEST_CHANGES — 1 CRITICAL (split + resync race double-materializes a remote pane, no dedup by terminal/pane id across the two materialization paths), 1 MAJOR (compounding teardown gap), 2 MINOR (stale dead_code comment, no snapshot-request rate limit). Resync-loop safety, origin binding, and wire-compat lenses check out.
