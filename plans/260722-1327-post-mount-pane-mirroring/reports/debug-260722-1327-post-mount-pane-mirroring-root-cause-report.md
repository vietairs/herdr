# Debug — post-mount server-side pane creation not mirrored to federation client

Symptom: pane created on serving side (vm105 `agent.start` → w3:p5) after mount never appears on Mac client. Mount-time panes + Mac-initiated splits mirror fine.

## Proven cause (source-confirmed, high confidence)

Federation Event channel frames carry only `{source_seq, kind}` — no entity id/payload — and the client drops them after cursor bookkeeping.

Evidence chain:
1. Server emits: `src/server/federation_accept.rs:789-831` `poll_events` forwards local event log entries as `FederationMessage::Event(EventChannelMessage::Frame(EventFrame{source_seq, kind}))` (816-828). `EventKind::PaneCreated` real kind (`src/api/schema/events.rs:238`).
2. Client drops: `src/remote/federation/reducer.rs:11-23` module doc admits frames cannot become entity-level events; mirroring only via `RemoteMirror::reconcile_by_diff` off a fresh `MountSnapshot` (initial mount or Gap/Reset remount). `reducer.rs:238-268` in-order Frame → advance cursor → `ReducerAction::Applied`. Both consumers discard: `client.rs:286-288` and `client.rs:488-491` `Applied => continue`.
3. Nothing downstream compensates: `src/app/api/workspaces.rs:216-249` only handles LinkClosed/Faulted/ResyncRequired/Err.
4. `reconcile_by_diff` (`reducer.rs:279-287` → `reconcile_panes`/`reconcile_tabs` 407-503) DOES push PaneCreated/PaneClosed/TabCreated/TabClosed — but only invoked from `apply_snapshot` (mount, `client.rs:215`) and Gap/Reset remount. No steady-state call.

Why Mac→remote splits work: separate payload-carrying RPC `SplitPaneRequest`/`SplitPaneResponse::Created` (`client.rs:554-609`) keyed by client-generated request_id — machinery not reusable as-is for server-initiated panes (no request to correlate). Reusable unit is the `build_remote_pane`-style runtime construction (`materialize_federation_mount`, `handle_federation_split_pane_ready` in `src/app/creation.rs`), not the pending-request wrapper.

Q4: pane close + tab create/close post-mount — same gap, identical path (no kind-specific branching in `apply_event_message`).

## Fix shape (ranked)

1. RECOMMENDED — resync-on-structural-event: on `ReducerAction::Applied` with structural kind (PaneCreated/PaneClosed/TabCreated/TabClosed), request a fresh snapshot over the link (new lightweight in-band request/response — wire lacks one today per reducer.rs:20-23) and run `reconcile_by_diff` exactly like the Gap/Reset path. Reuses all materialization machinery. Additive message within protocol v3 (unreleased).
2. Extend EventFrame with entity payloads — proper long-term fix, protocol change, out of scope now.

## Unresolved

- Whether `agent.start`-spawned panes push `PaneCreated` into the server's local EventHub at all (vs only pane.split) — verify during fix (grep event push call sites at server pane-creation paths); if absent, that is a second, earlier gap to close in the same change.
