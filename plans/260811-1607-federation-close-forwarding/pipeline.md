# Federation: forward mirrored workspace/tab close to the host

Task: Closing a mirrored workspace, or a non-last mirrored tab, removes it
locally only. The host keeps it. Add the missing wire message(s) so federation
close is bidirectional/real-time, matching the existing pane-close behavior.

Task source: free text (`/hvn:cortex ... --auto --advise`), 2026-08-11 16:07.

Parent plan: `plans/260811-1307-federation-multi-tab-workspace/` (phases 1-2
shipped on this branch: tab identity through resync, remote workspace creation).
This run is the close-direction counterpart of phase 2.

Worktree: `.claude/worktrees/federation-multi-tab-workspace`
Branch: `feat/federation-multi-tab-workspace` (REUSED, not created)

## Route card

```
Complexity: hard -> hard (3 scouts, ~6:30) -- multi-surface protocol change;
            two working reference patterns narrow unknowns, not blast radius.
Risk: HIGH -- versioned public wire contract between two herdr instances; grants
      a mounted client a NEW destructive capability over the host (closing a
      workspace kills its running processes). No per-request scoping beyond the
      mount lease (src/server/federation_lease.rs:148).
Familiarity: HIGH -- parent plan + root-cause report; 4 commits of this feature
      already on branch; ClosePaneRequest and WorkspaceCreateRequest are
      in-tree templates.
Scope: feature -- ~11 files.
Payoff: HIGH -- local/host state silently diverges, defeating the mount.
Route: R7 (HIGH risk), --auto, --advise
Advise: 4 gates via kongming (--auto substitution)
```

Change set (11, all `change`, none `add`):

- `src/remote/federation/protocol/mod.rs` -- new request/response variants + payloads + `channel()`
- `src/remote/federation/protocol/codec.rs` -- roundtrip tests
- `src/remote/federation/client.rs` -- response -> `AppEvent`
- `src/events.rs` -- new `AppEvent` variants
- `src/app/api.rs` -- event dispatch
- `src/app/mod.rs` -- pending-request maps
- `src/app/creation.rs` -- pending structs + response handlers + purge helpers
- `src/app/api/workspaces.rs` -- `handle_workspace_close` forwarding
- `src/app/api/tabs.rs` -- `handle_tab_close` forwarding
- `src/server/federation_accept.rs` -- inbound request handlers
- `src/server/federation_actor.rs` -- `FederationCommand` variants + dispatch

## Evidence (from the pre-route scout fan-out)

Confirmed gap. `handle_workspace_close` (`src/app/api/workspaces.rs:1001-1080`)
and `handle_tab_close` (`src/app/api/tabs.rs:247-360`) ARE federation-aware, but
only for LOCAL cleanup: they purge pending remote state and call
`end_federation_mount` when no siblings remain. Neither ever sends a message to
the host.

Working reference pattern (pane close, already bidirectional):

- Predicate: `src/app/api/panes.rs:1846-1850` -- `id::classify(&public_workspace_id) == IdClass::Remote(_)`
- Send: `src/app/api/panes.rs:345-417` `dispatch_remote_pane_close` -> `ClosePaneRequest`
- Correlate: `pending_remote_closes: HashMap<u64, PendingRemoteClose>` (`src/app/mod.rs:155-169`);
  ids from an `AtomicU64` (`src/app/api/panes.rs:44-47`)
- Response: `src/remote/federation/client.rs:1003-1047` -> `AppEvent::FederationClosePaneReady`
  -> `src/app/api.rs:300-303` -> `src/app/creation.rs:1187-1271` (validates origin, then
  tears the local mirror down)

Host side: `src/server/federation_accept.rs` dispatch loop (lines 471-578, match
arms 522-566) turns each request into a `FederationCommand`, dispatched in
`src/server/federation_actor.rs:287-564`, which reuses the same JSON-API handlers
the local TUI uses (`Method::PaneClose`, `Method::WorkspaceCreate`, ...).

Authorization today: `FederationLease::is_mounted_controller`
(`src/server/federation_lease.rs:148`) gates mutating commands to the single
mounted controller. There is NO per-workspace scoping -- a mounted connection may
act on any workspace the host exposes.

Protocol version: source `FEDERATION_PROTOCOL_VERSION = 6`
(`src/remote/federation/protocol/mod.rs:65`) vs `5` in the latest release tag
`v0.8.0-hvn.2`. Per CLAUDE.md, source is already ahead -- the new variants ride
v6, NO second bump. Same for `PROTOCOL_VERSION` (source 20 vs released 19).
Codec rejects any version skew outright (`codec.rs:94`, `CodecError::VersionSkew`)
-- there is no graceful unknown-variant path, which is why the single bump must
cover every variant added before the next release.

## Design fork for the brainstorm gate

1. Two new pairs: `WorkspaceCloseRequest/Response` + `TabCloseRequest/Response`.
2. One generic `ContainerCloseRequest/Response` with a `kind` enum (workspace/tab/pane),
   optionally subsuming the existing `ClosePaneRequest`.
3. No new message: client closes mirrored panes individually via the existing
   `ClosePaneRequest` and relies on the host's last-pane-closes-the-workspace rule
   to cascade.

## Constraints

- CLAUDE.md runtime/client boundary: shared runtime facts belong to server state
  and the JSON API/event path; neutral server/API names, no UI-surface names.
- No `unwrap()` in production code. `tracing` for logging.
- Platform code stays in `src/platform/`; compile-gate OS-specific code.
- Do NOT bump `FEDERATION_PROTOCOL_VERSION` or `PROTOCOL_VERSION` (already ahead
  of the latest release).
- Build/test locally: `ZIG=~/.local/zig-0.15.2/zig cargo test -- --test-threads=4`
  (no `just`/`nextest` on this machine). 3 pre-existing clippy errors are baseline.
- Lowercase conventional commits, no AI co-author lines.

## Acceptance criteria

- Closing a mirrored workspace on the client closes the same workspace on the host.
- Closing a non-last mirrored tab on the client closes that tab on the host.
- Closing the LAST mirrored tab keeps today's semantics (workspace close path).
- An unmount/link-close must NOT be interpreted as a close-everything request --
  retiring a mount leaves the host's workspaces alone.
- Host refuses a close targeting a workspace it never exposed to this mount.
- Local teardown stays correlated to the host's response (mirrors pane-close),
  so a rejected close does not desync.
- No protocol version bump. Round-trip codec tests for every new variant.
- Full test suite green.
