# Code review: federation 3-fix diff (agent-status relay, resize gate, remote split)

Scope: uncommitted working-tree diff, 20 src files + 2 plan notes. Read-only review, no cargo build run (hook blocks `target`/`build` paths; `cargo check` also has no lib target configured for this invocation).

## Critical

1. **Stale/leaked `pending_remote_splits` entries can splice a remote pane into the wrong workspace, and are never cleaned up on link close.**
   `src/app/mod.rs:155` adds `pending_remote_splits: HashMap<u64, PendingRemoteSplit>` where `PendingRemoteSplit` (`src/app/creation.rs:426-431`) stores `ws_idx: usize` — a raw Vec index, not a stable workspace id.
   `src/app/api/workspaces.rs:306-341` (`handle_federation_mount_ended`) removes the mirror and closes the mount's own workspaces via `close_selected_workspace`, but never touches `pending_remote_splits`. The same function's own comment at line 354 acknowledges *"indices shift once the closing workspaces are removed"* for a different variable, but this exact hazard is not applied to `pending_remote_splits`.
   Failure scenario: user splits a pane in federated workspace A (`request_id=N` registered with `ws_idx=2`), then the mount link drops before the response arrives (or the response is merely slow) and the user closes workspace(s) before index 2, or a differently-ordered workspace later occupies index 2. When the (stale, or simply late) `SplitPaneResponse::Created` for N eventually arrives — or a lingering registration from an entirely different session — `handle_federation_split_pane_ready` (`src/app/creation.rs:328`) does `self.state.workspaces.get_mut(pending.ws_idx)` and, finding a workspace at that index, silently splices the remote-materialized pane into whatever unrelated (possibly local, non-federated) workspace now lives there. This is a data-integrity/trust-boundary bug: a remote host's pane can land in a local workspace's tab layout that the request never targeted.
   Even absent index reuse, every split whose mount disconnects before a response (or whose response is dropped by `first_cause`/actor shutdown paths) leaks its `HashMap` entry for the life of the process — unbounded growth is bounded only by how many splits a session issues, but it's a real, unbounded-by-any-cap resource never reclaimed.
   Fix: key `PendingRemoteSplit` by workspace/mount identity (e.g. `HostKey` + a stable pane/tab id, not `usize`), and purge every `pending_remote_splits` entry belonging to a host in `handle_federation_mount_ended` (mirroring how `remote_mirrors`/workspaces are torn down there).

## Major

2. **`dispatch_remote_pane_split` request/registration race is not atomic and errors leave state inconsistent on the failure path — but only partially guarded.**
   `src/app/api/panes.rs:122-157`: `request_id` is minted, the frame is sent via `out_tx.send(...)`, and only *after* a successful send is `register_pending_remote_split` called. If `out_tx.send` succeeds but the mount's reader/writer task tears down concurrently (`out_tx` closing races with `drive_mount_channel` racing to read a response), the registration can still be reached after the mount already ended, so it is once again never cleaned up (compounds Critical #1). This is a race amplifier for the same underlying gap, not a new independent bug, but worth flagging: there is no generation/host fencing on `pending_remote_splits` entries at all (contrast with `handle_federation_mount_ended`'s `generation` fencing for `remote_mirrors`).

3. **Fire-and-forget split silently produces two different local outcomes for the same tab, both surfaced only via error codes/toasts, not a normal success response.** Not a bug per se, but `remote_split_pending` (`src/app/api/panes.rs:159-164`) is returned via `encode_error`, meaning any caller (including tests, and any external API consumer) sees this as a hard error rather than an accepted/pending state, even though the split will actually happen. This is a --pragmatic protocol choice per the doc comment, but callers that treat any `error` field as fatal (e.g. scripting against the JSON API) will misreport a successful-in-flight split as a failure. Consider a distinct non-error "accepted" response shape if any automation depends on this API long-term (flagging for awareness, not blocking).

## Minor / Verified-OK

- Resize gate (`src/pane.rs:2711-2734`, `src/pane/terminal.rs:135-154`): `is_remote()` correctly threads through `PaneRuntime::resize` → `PaneTerminal::resize` → `GhosttyPaneTerminal::resize`; all local call sites (tests, and the only other call site) pass `false`, and only `PaneRuntimeIo::Remote` panes gate off the replay-recovery block. Confirmed both the classic-session and federated-mount code paths run in the same `PaneRuntime` and reach the same `PaneRuntimeIo` enum, so both share this gate correctly. No regression to local pane resize behavior found.
- AgentStatus relay id-space (`src/remote/federation/client.rs:71-75`, `src/app/creation.rs:271-273`): `route_agent_status`/`register_agent_status_sender` are consistently keyed by the same raw (un-namespaced) `terminal_id` `output_senders`/`open_terminal` already use — verified no `r:`-prefixed id ever enters this map. `forget()` (`client.rs:83-85`) correctly removes both maps together, so per-pane sender registrations don't outlive `Close`.
- Protocol version bump 2→3 (`src/remote/federation/protocol/mod.rs:24-29`) is justified (new wire variants) and handshake enforces an exact-version match (`client.rs` `version_mismatch_is_surfaced_as_a_typed_rejection` test, `serve.rs:229`), so a v2 peer is refused the whole mount up front rather than partially misunderstanding a v3-only frame — no partial-compat gap.
- Cross-agent seam (production vs. `FixtureHost` loopback split dispatch, `serve.rs:405-437` vs `loopback.rs` new `split_pane`, and `federation_accept.rs`/`federation_actor.rs` production path): both implement the same `FederationHost::split_pane` contract; production path reuses the real `Method::PaneSplit` handler (good — no logic duplication) while the fixture mints a synthetic sibling id for test purposes only. No divergence risk since callers only see the trait interface.
- `handle_pane_split`'s new remote-workspace guard (`src/app/api/panes.rs:36-58`) derives `ws_idx` from the actual resolved `target_pane_id` before classifying, not from `params.workspace_id`, so it can't be bypassed by a caller passing a mismatched `workspace_id`.
- No new `unwrap()` in reachable production code; the only new `.unwrap()`s are inside `#[cfg(test)]` test bodies or `loopback.rs`'s `FixtureHost` (pre-existing pattern in that same file, not introduced by this diff — `self.terminals.lock().unwrap()` matches surrounding fixture code).
- `#[cfg(unix)]` gating is consistent for all new `AppEvent` variants and their handlers (`events.rs:175-221`, `creation.rs:307,387`, `api.rs:170-180`, `actions.rs:2731-2735`).
- Tracing used throughout (`tracing::warn!`/`debug!`/`info!`), no `println!` added.

## Recommended actions

1. Fix Critical #1 before landing: key `PendingRemoteSplit` by a stable identity (workspace id string, not `usize`) and purge entries for a host's splits in `handle_federation_mount_ended`.
2. Consider fencing `pending_remote_splits` entries by mount generation (same pattern `remote_mirrors`/`FederationMountEnded` already use) to close the race in Major #2.
3. Optional: revisit the `remote_split_pending` error-shaped success response if any script/automation is expected to consume `Method::PaneSplit` against federated workspaces.

## Unresolved questions

- Is `ws_idx` reuse after workspace close actually reachable in practice (e.g. does `close_selected_workspace` ever get called between a split dispatch and its response under real user action), or is this purely a leak with no live misrouting path today? Worth a targeted repro/test before deciding severity finality.

Status: DONE
Verdict: REQUEST_CHANGES
Counts: Critical 1, Major 2, Minor/Verified-OK 8
