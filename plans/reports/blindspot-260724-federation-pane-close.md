# Blindspot scout: federation pane-close sync (Gap A + Gap B)

## Build/test setup
- `ZIG=$HOME/.local/zig-0.15.2/zig` — file exists (per memory `herdr-local-build-zig.md`). No `just`/nextest here; use `cargo test -- --test-threads=4`.
- `FEDERATION_PROTOCOL_VERSION` currently `3` (protocol/mod.rs:37), unreleased on this fork. A closed-enum addition (`ClosePaneRequest`/`Response`) an old peer cannot decode requires a bump PER the doc-comment rule at protocol/mod.rs:23-36 UNLESS this fork can prove no tagged release ever shipped v3 with a production call site (same argument used for `SnapshotRequest`). Verify via `plans/260713-1217-herdr-remote-workspace-federation/implementation-notes.md` before deciding; likely no bump needed but must be justified in writing like the `SnapshotRequest` precedent, not assumed.

## Gap A — client→server (confirmed, exact location)

**Correction to task framing**: `src/app/actions.rs:1933 pub fn close_pane` is `#[cfg(test)]`-only (line 1931 `#[cfg(test)]`) — it is NOT the production close path. The real production path is:

- `NavigateAction::ClosePane` (app/input/navigate.rs:385) → `close_focused_pane_via_api_requires_confirmation` (navigate.rs:581) → `runtime_pane_close` (app/runtime_mutations.rs:108) → `dispatch_runtime_mutation(Method::PaneClose(...))` → `App::handle_pane_close` (app/api/panes.rs:1698) → **`App::close_pane`** (app/api/panes.rs:1706-1811).
- CLI `herdr pane close <id>` (cli/spec.rs:566) dispatches the same `Method::PaneClose` over the client socket into the same `handle_pane_close`/`close_pane`.

`close_pane` (panes.rs:1706) has **no remote-mount check at all** — it goes straight to `ws.close_pane(pane_id)` on local `AppState`, unconditionally. Compare with the sibling `handle_pane_split` (panes.rs:40-85), which explicitly classifies the target workspace before doing anything local:

```rust
// panes.rs:80-85
if matches!(
    crate::remote::federation::id::classify(&self.public_workspace_id(ws_idx)),
    crate::remote::federation::id::IdClass::Remote(_)
) {
    return self.dispatch_remote_pane_split(id, ws_idx, target_pane_id, &params);
}
```

`dispatch_remote_pane_split` (panes.rs:217-320ish) is the exact pattern to mirror for close:
1. Resolve the live mount's raw remote pane id + `out_tx` via `ws.terminal_id(pane_id)` → `terminal_runtimes` → `runtime.remote_terminal_id()` / `runtime.remote_out_tx()` (panes.rs:224-235).
2. Resolve `origin` via `federation_host_key_for_workspace(ws_idx)` (panes.rs:251) — needed to validate the eventual response's mount, same convention `handle_federation_resync_pane_removed` already uses (creation.rs:1132-1144).
3. Mint a `request_id` via a bare `AtomicU64` counter (`next_remote_split_request_id`, panes.rs:34-37) — a `next_remote_close_request_id` sibling is the natural analog. `SplitPaneRequest`'s own doc comment (protocol/mod.rs:264-270) explicitly designed the bare-u64 pairing to be reused rather than building an RPC framework — `ClosePaneRequest`/`ClosePaneResponse` should follow the identical shape.
4. Send `FederationMessage::SplitPaneRequest` equivalent, fire-and-forget (panes.rs:269-286) — same constraint applies to close: this JSON-API handler is synchronous and cannot await the response.
5. Register a `PendingRemoteSplit`-equivalent (`app/creation.rs`'s `PendingRemoteSplit`) so the eventual `ClosePaneResponse` can be spliced back — for close this is simpler: on `Created`/success, tear the local pane down (same effect as `handle_federation_resync_pane_removed`); on `Failed`, surface an error toast.

On the **serving host** side, mirror `serve.rs:438-453` and `federation_accept.rs:539,568-620` (the existing `SplitPaneRequest` inbound handler) — a new arm dispatching `ClosePaneRequest` into the live `App`'s own `handle_pane_close`, answering `ClosePaneResponse::{Closed,Failed}` on the shared outbound queue.

**Confirmation dialog interaction risk**: `close_pane` (panes.rs:1715-1723) can refuse with `confirmation_required` when closing would close a worktree group. `dispatch_remote_pane_close` must decide whether that confirmation applies on the *serving* host (its own worktree grouping) or should never apply for a request arriving over the wire (like `handle_federation_resync_pane_removed`'s comment at creation.rs:1110-1113 explicitly says: "unlike the interactive `pane.close` API path, this never asks for close confirmation... the remote already made this decision"). A `ClosePaneRequest` handled on the serving host, however, runs through the *server's own* `handle_pane_close`, which *will* apply its own confirmation logic and can return `Failed` for a reason the mounting client's UI never surfaces distinctly from "pane not found" unless `ClosePaneResponse::Failed` carries a `reason: String` (mirror `SplitPaneResponse::Failed`, protocol/mod.rs:304-307).

## Gap B — server→client (root cause found; NOT what the task premise assumes)

The task assumes server-side pane closes never propagate live to a mounted client. **That mechanism largely exists already** (b5cb8ce8, "mirror server-side pane changes to mounted clients"):

1. Server's own `close_pane` emits `EventKind::PaneClosed` into its `EventHub` (panes.rs:1798-1804 for the ordinary case, 1765-1771 for the workspace-closing case).
2. `poll_events` (federation_accept.rs:1218-1260) forwards **every** `EventKind` frame off that hub as `FederationMessage::Event(EventChannelMessage::Frame(...))` to the mounted client — generic, not close-specific.
3. Client's `reducer.apply_event_message` (reducer.rs:327-358) applies the frame for cursor bookkeeping and returns `ReducerAction::Applied{kind,..}`.
4. `client.rs:575-580`'s `is_structural_event_kind` (client.rs:945-956) matches `PaneClosed` (among `PaneCreated`/`PaneMoved`/`TabCreated`/`TabClosed`) and triggers a `SnapshotRequest` (coalesced — a burst of structural frames produces exactly one, per the test at client.rs:1549).
5. Serving host answers `SnapshotResponse` (federation_accept.rs's `SnapshotRequest` handler, ~line 621+).
6. Client's `reconcile_by_diff` (reducer.rs:375+) diffs the fresh snapshot against the mirror's own namespaced maps and returns `removed_pane_ids`.
7. `client.rs:624-632` sends `AppEvent::FederationResyncPaneRemoved{origin, pane_id}` for each.
8. `app/api.rs:203` dispatches that event to **`App::handle_federation_resync_pane_removed`** (creation.rs:1115-1180+), which tears the local `Tab`/`PaneRuntime` down for real.

**The actual bug**, confirmed by reading `handle_federation_resync_pane_removed` (creation.rs:1120):

```rust
let Some(local_pane_id) = self.remote_resync_pane_index.remove(&pane_id) else {
    // Not every removed remote pane necessarily went through the
    // resync-created path (e.g. it may have been materialized at
    // mount time, ...) — nothing to reverse-index here means
    // nothing this handler owns to tear down.
    return;
};
```

`remote_resync_pane_index` (app/mod.rs:181, `HashMap<String, PaneId>`) is populated in exactly two places:
- `materialize_resync_pane` (creation.rs:1091) — panes discovered via a *later* resync.
- The remote-split-created path (creation.rs:920, comment explicitly: "so a later resync-driven removal of it can find and tear it down").

It is **never populated by `build_remote_pane`** (creation.rs:736), the function that materializes panes present **at initial mount time** — i.e. the overwhelming majority of panes a user actually sees on a freshly mounted remote workspace. So: closing one of those original mount-time panes on the serving host (e.g. via its own `herdr pane close` CLI, matching the memory note's documented workaround) **does** reach the client's reducer, **does** trigger the resync round-trip, **does** compute a correct `removed_pane_ids` diff — and then the removal handler silently no-ops and returns at line 1126 because that pane id was never reverse-indexed. Only panes created *after* mount (via remote split or a later resync) get torn down live. This matches the task's real-world symptom (only fresh mount syncs it) even though the mechanism looks complete on a shallow read.

**Fix shape for Gap B**: either (a) populate `remote_resync_pane_index` for every pane at `build_remote_pane` time too (make it a reverse index of "every locally-materialized remote pane", not just "resync/split-created" ones), or (b) fall back in `handle_federation_resync_pane_removed` to `self.find_remote_pane_by_raw_id(origin, &pane_id)` (an origin-scoped lookup across all live panes) when the reverse-index misses. Option (a) is the smaller, more consistent fix — but check `apply_snapshot`'s initial-mount call site (reducer.rs:250) and whichever caller drives `build_remote_pane` to see if the raw remote pane id is available there at the same point `local_pane_id` is minted.

## Protocol version rule

`FEDERATION_PROTOCOL_VERSION` doc-comment (protocol/mod.rs:23-36) requires a bump only when a genuinely deployed peer could observe the new variant as a decode failure. Precedent: `SnapshotRequest`/`SnapshotResponse` shipped WITHOUT a bump because "no production call site existed in any tagged version." Before deciding on `ClosePaneRequest`/`ClosePaneResponse`, check whether v3 (current) has shipped in any tagged release with a live `SplitPaneRequest` call site — if yes, a new `FederationMessage` variant added to the same enum is still an additive `serde` variant only if the wire codec tags by name (check `codec.rs`); if it's positionally tagged/adjacently-tagged in a way an old peer's deserializer would hard-fail on an unknown variant (which `serde`'s `#[serde(tag=...)]`-less internally-tagged enum typically does), the same reasoning that gated `Fault` (v1→v2 bump, protocol/mod.rs:25-26) likely applies to `ClosePaneRequest` too, since it's a wholly new top-level `FederationMessage` variant, not an additive field. Read `codec.rs` before concluding no bump is needed — do not copy the `SnapshotRequest` precedent without checking whether v3 has actually shipped.

## Existing tests for protocol messages (patterns to extend)
- `protocol/mod.rs:550-587` — `split_pane_request_response_roundtrip_through_the_wire_codec` — direct template for a `close_pane_request_response_roundtrip_through_the_wire_codec` test.
- `protocol/mod.rs:592-613` — snapshot request/response roundtrip, same shape.
- `loopback.rs:609-696` — full loopback client↔server exercise of `SplitPaneRequest`→`SplitPaneResponse::{Created,Failed}`; do the same for close.
- `federation_accept.rs:1737-1783` — server-side handling test of an inbound `SplitPaneRequest`.
- `client.rs:1815-2120ish` — client-side materialization tests for `SplitPaneResponse::Created` (register in `remote_resync_pane_index`, splice into layout) — the close-analog tests should specifically assert tear-down for BOTH a resync-index-registered pane AND (once Gap B is fixed) a mount-time-materialized pane, to pin the exact bug found above as a regression test.

## Hidden risks / unresolved questions

1. **Pane identity mapping (local↔remote) for close targets**: `close_pane`'s remote dispatch needs the same raw-remote-id resolution `dispatch_remote_pane_split` uses (`runtime.remote_terminal_id()`), not the namespaced `r:<host>:...` public id. Get this wrong and `ClosePaneRequest.target_pane_id` will not match anything on the serving host.
2. **Focus handling after close**: local `close_pane` reassigns focus via `layout_update_target_after_pane_removal` + `emit_layout_updated_event` (panes.rs:1805-1807) and updates `previous_pane_focus`/`current_pane_focus_target` elsewhere. A fire-and-forget remote close means the mounting client's UI cannot immediately show the post-close focus state — it must wait for the `ClosePaneResponse` (or the mirrored `PaneClosed` event) before reassigning focus, exactly like the split path shows an intermediate "pending" state (`remote_split_pending` ack, panes.rs:207-216 doc comment) rather than fabricating a result. Decide the UX (spinner/dim vs. optimistic removal) before implementing.
3. **Snapshot reconcile race**: if a `ClosePaneRequest` is in flight from client A at the same moment the serving host's OWN structural-event mirroring fires (Gap B path) for the same pane (e.g. because it was already closing for some other reason), the client could receive both a `ClosePaneResponse::Closed` (direct RPC answer) and a `FederationResyncPaneRemoved` (via the generic mirror) for the same pane. `remote_resync_pane_index.remove` is idempotent-safe (returns `None` on a second call and simply returns), so this looks race-safe by construction, but confirm the direct-response handler ALSO removes the pane from `remote_resync_pane_index` if it was present (to avoid a stale reverse-index entry after a request-response close), the same way `handle_federation_resync_pane_removed` does.
4. **`close_pane`'s worktree-group confirmation** (panes.rs:1715-1723) fires BEFORE the remote-vs-local classification would need to happen (order matters if the classification is inserted the same way `handle_pane_split` does it) — decide whether a remote-targeted close should even evaluate the LOCAL mounting client's `close_pane_would_close_workspace`, since the relevant "would this close a workspace" question is about the *client's own* mounted workspace tab count, which is legitimately a client-side concern independent of the remote host's own tab structure. This is unlike split, which has no such confirmation gate.
5. **`origin_matches` guard for close responses**: `handle_federation_resync_pane_removed` guards on `worktree_space().key == "federation:<origin>"` (creation.rs:1132-1144) before tearing down — any new direct `ClosePaneResponse` handler needs the identical origin check (same rationale: reject a response tagged with a DIFFERENT mount's `HostKey`, matching `App::handle_federation_split_pane_ready`/`_failed`'s existing origin check for split).
6. Whether v3 has actually shipped in a tagged release on this fork (needed to decide the version-bump question above) — not verified in this pass; check `plans/260713-1217-herdr-remote-workspace-federation/implementation-notes.md` and this fork's release tags (`herdr-fork-release-convention` memory: `v<ver>-hvn.N`).
