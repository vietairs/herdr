# Fix: federation 3-fix code-review findings remediation

Source review: `plans/260721-2353-federation-agents-sidebar-remote-detection/reports/code-review-260722-0055-federation-three-fix-diff-report.md`

## Critical #1 — `pending_remote_splits` keyed by raw `ws_idx`, never purged

**Fixed.** Changes:

- `src/app/creation.rs`: `PendingRemoteSplit.ws_idx: usize` → `workspace_id: String`
  (stable `Workspace::id`, never reused across a process's lifetime — see
  `workspace::tests::reserving_restored_workspace_ids_prevents_reuse`).
  `handle_federation_split_pane_ready` now resolves the live `ws_idx` by
  looking up `ws.id == pending.workspace_id` at splice time instead of
  trusting a stashed index; if the workspace is gone it hits the existing
  "workspace no longer exists" guard (safe no-op).
  Added `App::purge_pending_remote_splits_for_workspaces(&HashSet<String>)`.
- `src/app/api/panes.rs` (`dispatch_remote_pane_split`): registers
  `PendingRemoteSplit { workspace_id: self.public_workspace_id(ws_idx), .. }`
  instead of the raw index.
- `src/app/api/workspaces.rs` (`handle_federation_mount_ended`): calls
  `self.purge_pending_remote_splits_for_workspaces(&closing_ids)` before
  `close_selected_workspace()` runs — `closing_ids` (the set of workspace ids
  about to close) was already computed there for the identity-preserving
  focus-restore logic, so this reuses it rather than adding new bookkeeping.

Verification: new regression test
`app::api::workspaces::tests::federation_mount_ended_purges_pending_remote_splits_for_its_workspaces`
(src/app/api/workspaces.rs). It registers a pending split against a
federation-materialized workspace, calls `handle_federation_mount_ended`,
asserts the entry is purged, then builds a real `TerminalRuntime` via
`TerminalRuntime::spawn_remote` and feeds a late `FederationSplitPaneReady`
for that same `request_id` into `handle_federation_split_pane_ready` —
asserts the workspace count is unchanged and the local workspace now sitting
at the old index never receives the pane. `cargo test --bin herdr
federation_mount_ended_purges_pending_remote_splits -- --test-threads=4` →
1 passed.

## Major #2 — register/send race can leave a stale registration after mount teardown

**Verified closed by the same fix; no separate generation-fencing added.**
`dispatch_remote_pane_split` (the register site) and
`handle_federation_mount_ended` (the would-be purge site) both run as
`&mut App` methods invoked exclusively from the single sequential
`App::handle_internal_event` dispatcher (`src/app/api.rs:64`) — `AppEvent`s
are processed one at a time, so there is no concurrent-mutation window
between these two handlers on the same `App`. The only real hazard was the
leak/mis-splice this fix already closes (a registration landing for a
request whose mount already ended); the stable-id purge plus the existing
"unknown/stale request" and "workspace no longer exists" guards in
`handle_federation_split_pane_ready` make a late arrival a safe no-op, not a
mis-splice. Logged in implementation-notes.md; revisit only if `App` event
dispatch ever becomes concurrent (it currently isn't).

## Major #3 — `remote_split_pending` returned as an error-shaped success

**Not changed (deliberately, per review's own framing as "flagging for
awareness, not blocking").** This is a protocol-shape/API-contract decision
(whether federated split acknowledgement should be a distinct non-error
response), not a bug with a clear fix inside this diff's owned files, and
changing the wire/JSON-API response shape is a bigger-than-"cheap" change
that risks widening scope beyond the review's fix set. Flagging for the
user/planner if any external automation is expected to consume
`Method::PaneSplit` against federated workspaces long-term.

## Minor findings

All 8 minor/verified-OK items in the review were already confirmed correct
by the reviewer (resize gate, AgentStatus relay id-space, protocol version
bump, cross-agent seam, remote-workspace guard, no new `unwrap()`, `#[cfg(unix)]`
gating, tracing usage) — no action needed.

## Validation

- `cargo build --bin herdr`: clean (only 2 pre-existing dead-code warnings
  unrelated to this diff: `map_out`, `Capability::CLIPBOARD`).
- `cargo test --bin herdr -- --test-threads=4` (full suite): **2708 passed, 0
  failed, 0 ignored** (net +1 over the pre-existing 2707 baseline).
- `cargo clippy --bin herdr --tests`: same 2 pre-existing warnings plus 1
  pre-existing `type_complexity` warning in `pane_source.rs` (unrelated to
  this diff) — no new warnings.

Notes appended to
`plans/260713-1217-herdr-remote-workspace-federation/implementation-notes.md`
(What/Why/Evidence/Reversibility, 2 entries: the Critical fix, and why Major
#2 needed no separate fencing).

## Unresolved questions

- None blocking. Major #3 (error-shaped pending response) is left as a
  product/protocol decision for the user/planner, not resolved here.

Status: DONE
