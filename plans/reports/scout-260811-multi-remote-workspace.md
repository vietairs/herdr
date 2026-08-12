# Scout: multiple remote workspaces per federation mount

Read-only design scout. No code edited.

## 1. Mount identity

Uniqueness key today is `HostKey = "{user@ip}#{session_discriminator}"` — `src/remote/federation/id.rs:34-40`. `session_discriminator` is the LOCAL herdr session name (`crate::session::active_name()`, defaults to `DEFAULT_SESSION_NAME = "default"` when unset — `src/session/mod.rs:11,96-99`), not anything derived from the target host. It is read once per `handle_workspace_mount_remote` call: `src/app/api/workspaces.rs:101-102`.

Rejection is a `remote_mirrors: HashMap<HostKey, RemoteMirror>` membership check on the **local App/daemon's own in-memory state**, not on the target string alone and not truly "per session" in a way that helps here:
- `AppState::double_attach_conflict` — `src/app/state.rs:1842-1847` — `self.remote_mirrors.contains_key(host_key)`.
- `AppState::begin_federation_mount` — `src/app/state.rs:1805-1815` — rejects only a literal duplicate `HostKey`.
- Checked pre-dial per target in the mount handler: `src/app/api/workspaces.rs:104-122`.

Because `session_name` is fixed for the lifetime of one running App/daemon process (it's whatever local session that daemon belongs to — different local sessions are different daemon processes with their own socket, per `src/session/mod.rs` session model), in practice **within one running daemon `session_discriminator` never varies between two mount attempts**, so the "already mounted" rejection is effectively per-host (`user@ip`) per daemon-process, full stop. Two mounts to the same host target string can only "coexist" by running from two different local herdr sessions (two separate daemon processes, each with its own `remote_mirrors` map) — not from the same App/session, and not via any second `workspace.mount_remote` call in the same session. This matches the comment at `src/app/state.rs:1831-1841`, which explicitly says the discriminator exists to prevent a re-attach-from-a-different-session collision, and that cross-process detection ("classic `--remote` attach vs. an already-federated host") is documented-not-enforced for v1 — a different concern from same-App same-host re-mount.

**Answer: today's rejection is effectively per-host (`user@ip`) within one running App/daemon.** The `session_name` component of `HostKey` does not create a usable path to a second mount of the same host from the same client session.

## 2. What the mount already materializes

Confirmed: `materialize_federation_mount` (`src/app/creation.rs:570-763`) already fans a single mount out into **one local `Workspace` per remote workspace** reported by the mirror, not just one workspace per mount:

- Iterates `mirror.workspaces().values()`, sorted by `ws_info.number` — `src/app/creation.rs:582-583`.
- For each remote workspace, iterates its tabs (`mirror.tabs()` filtered by `workspace_id`) and creates a local `Workspace` on the first tab (`Workspace::from_existing_pane`, `src/app/creation.rs:660-688`), then additional tabs via `create_tab_from_existing_pane` (`src/app/creation.rs:652-659`).
- Each created local `Workspace.id` is stamped with the mirror's own namespaced `ws_info.workspace_id` (`r:<host_key>:...` prefix) so the sidebar's federation-origin grouping recognizes it — `src/app/creation.rs:670-679` and comment there citing `ui::sidebar::workspace_federation_origin`.
- Returns `created_ws_idxs: Vec<usize>`, one entry per remote workspace materialized — `src/app/creation.rs:585, 684-688, 757-762`.
- `emit_workspace_open_events`/`schedule_session_save` fire once per created workspace — `src/app/creation.rs:757-760`.

**So the "N remote workspaces from one mount" half of the feature request is already built and live** (wired from `handle_federation_mount_ready`, `src/app/api/workspaces.rs:249-254`) — **conditioned entirely on the remote host actually reporting >1 workspace in its `MountSnapshot`/mirror at mount time.** The crux is confirmed: the real gap is not "can the client render N workspaces from one mount" (yes), it's "can the client cause the remote to have >1 workspace, or learn about one created after mount" (see Q3/Q5).

## 3. Client -> remote mutation channel

`FederationMessage` enum, `src/remote/federation/protocol/mod.rs:537-558`:

```
Handshake, HandshakeResponse, MountSnapshot, Event, Terminal, AgentStatus,
Clipboard, Fault, SplitPaneRequest, SplitPaneResponse, ClosePaneRequest,
ClosePaneResponse, SnapshotRequest, SnapshotResponse, ClipboardStageRequest,
ClipboardStageResponse
```

Client->server (mutation-intent) variants actually serviced by the serving host's `run_connection` read loop (`src/server/federation_accept.rs:540-549`): `SplitPaneRequest`, `ClosePaneRequest`, `ClipboardStageRequest`, `SnapshotRequest`. No `WorkspaceCreateRequest`, `TabCreateRequest`, or any structural-create-above-pane-level request exists anywhere in the enum or the connection loop.

**Confirmed gap: split-pane exists, workspace/tab-create does not.** `SplitPaneRequest`'s shape is the direct template to copy:

```rust
// src/remote/federation/protocol/mod.rs:301-310
pub struct SplitPaneRequest {
    pub request_id: u64,
    pub target_pane_id: String,   // un-namespaced remote pane id
    pub direction: SplitDirection,
    pub ratio: Option<f32>,
    pub focus: bool,
}
// src/remote/federation/protocol/mod.rs:327-337
pub enum SplitPaneResponse {
    Created { request_id: u64, new_pane_id: String, new_terminal_id: String },
    Failed  { request_id: u64, reason: String },
}
```

Server-side servicing pattern (`handle_split_pane_request`, `src/server/federation_accept.rs:578-623`): decode request -> build a `oneshot` reply channel -> `server_event_tx.blocking_send(ServerEvent::Federation(FederationCommand::SplitPane { .. , reply }))` -> `rx.blocking_recv()` -> encode `SplitPaneResponse` -> `enqueue_outbound`. A `WorkspaceCreateRequest`/`TabCreateRequest` would follow the identical shape, routed through a new `FederationCommand::CreateWorkspace`/`CreateTab` variant into the serving host's own `App` (same actor that already owns pane split/close), and the client side would consume `WorkspaceCreateResponse`/`TabCreateResponse` in `drive_mount_channel` (`src/remote/federation/client.rs:555` area) exactly where `SplitPaneResponse::Created` is handled today (`src/remote/federation/client.rs:712-821`) to materialize a new local `Workspace`/`Tab` via the same helpers `materialize_federation_mount` already uses.

`ClosePaneRequest`/`ClosePaneResponse` (`src/remote/federation/protocol/mod.rs:339-362`) is a second, simpler precedent for the same request/response/`FederationCommand` shape.

## 4. Mutation allowlist

`federated_session_allows` — `src/api/mod.rs:100-209` — is a closed, exhaustive allowlist of JSON-API `Method`s a **federated-mode `App`** may execute locally. It forbids `WorkspaceCreate`, `TabCreate`, `PaneSplit`, etc. (`src/api/mod.rs:156-198`).

**This allowlist gates a different, currently-dormant code path than the one the feature request targets.** It is only consulted when `App.federated_mode == true` (`src/app/mod.rs:116,949`; enforced at `src/app/runtime.rs:70` and `src/app/api.rs:1065`), and `federated_mode` is set **only** by `App::new_federated` (`src/app/mod.rs:939-949`), whose only call site is `run_federated_session` (`src/remote/federation/session.rs:308`) — the **older, single-mount, in-proc "classic `--remote --federated`" CLI path** (`src/remote/unix.rs:618-665`), explicitly documented as dormant/opt-in ("DORMANT... nothing calls `run_federated_session` until b3 flips", `src/remote/federation/session.rs:1-27`) and gated behind an env var / config opt-in (`federation_requested`, `src/remote/unix.rs:618-621`).

The **newer daemon-owned multi-workspace mount path** the feature request is about (`handle_workspace_mount_remote`/`handle_federation_mount_ready`, `src/app/api/workspaces.rs`) runs `materialize_federation_mount` on the local session's own regular `App` (`federated_mode` stays `false` — it is the user's normal local daemon, `src/app/mod.rs:907`), so `federated_session_allows` never applies to it. The mount dialog's own JSON API call (`workspace.mount_remote`) is itself in the FORBIDDEN half of the allowlist (`Method::WorkspaceMountRemote(_)` at `src/api/mod.rs:157`), which only makes sense if the allowlist's purpose is "what a *federated-mode view-only session* may do to *its own local state*" — irrelevant to whether a client can ask a *remote* host to create a workspace/tab over the wire, since that would be a new `FederationMessage` variant serviced by `src/server/federation_accept.rs`, not a JSON API `Method` at all.

**Answer: creating a new tab/workspace in a mounted remote workspace via a new federation wire command would NOT be blocked by this allowlist** — it is a same-process JSON-API gate on a different (dormant) federated single-session mode, not a check on the federation wire protocol.

## 5. Resync path

`grep handle_federation_resync` finds exactly two handlers, both pane-level:

- `handle_federation_resync_pane_created` — `src/app/creation.rs:1274`
- `handle_federation_resync_pane_removed` — `src/app/creation.rs:1375`

Driven from `AppEvent::FederationResyncPaneCreated`/`FederationResyncPaneRemoved`, emitted only out of `SnapshotResponse` handling in `drive_mount_channel` (`src/remote/federation/client.rs:600-643`), which itself only reacts to `mirror.reconcile_by_diff`'s `ReconcileDiff { created_panes: Vec<PaneInfo>, removed_pane_ids: Vec<String> }` (`src/remote/federation/reducer.rs:397-399`, produced at `:375-388`). The diff struct carries **no** `created_tabs`/`created_workspaces`/`removed_tab_ids`/`removed_workspace_ids` fields at all — tab/workspace changes are invisible to the resync-materialization path even though the mirror's own reducer independently tracks `EventKind::WorkspaceCreated`/`TabCreated` for its own bookkeeping (`src/remote/federation/reducer.rs:492-493,546-547`).

Separately, `is_structural_event_kind` (`src/remote/federation/client.rs:1028-1039`) — which decides whether a plain `EventFrame` from the remote must trigger a `SnapshotRequest` resync at all — matches `PaneCreated | PaneClosed | PaneMoved | TabCreated | TabClosed | TabMoved` but **not** `WorkspaceCreated`/`WorkspaceClosed`. So a new workspace appearing on the remote host doesn't even trigger a resync fetch; a new *tab* does trigger a resync fetch (because `TabCreated` is in the match), but the fetched snapshot's diff is thrown away for anything above pane granularity.

**Missing resync handlers: `handle_federation_resync_tab_created`, `handle_federation_resync_tab_removed`, `handle_federation_resync_workspace_created`, `handle_federation_resync_workspace_removed` — none exist.** Only pane add/remove is wired end to end (protocol -> diff -> event -> App handler).

## 6. Protocol version

- `src/protocol/wire.rs:16` — client/server JSON-API wire `PROTOCOL_VERSION: u32 = 19`.
- `src/remote/federation/protocol/mod.rs:56` — federation wire `FEDERATION_PROTOCOL_VERSION: u32 = 5`.
- Latest release tags (`git tag --list | sort -V | tail -5`): `v0.7.5-hvn.4`, `v0.7.5-hvn.5`, `v0.7.5-hvn.6`, `v0.8.0-hvn.1`, `v0.8.0-hvn.2` (HEAD `79c2dc22` is on `master`, unreleased since `v0.8.0-hvn.2`).
- At `v0.8.0-hvn.2` (`git show v0.8.0-hvn.2:src/protocol/wire.rs`), `PROTOCOL_VERSION` is already `19`; `FEDERATION_PROTOCOL_VERSION` is already `5` at that tag too.

**Both are equal to the last release, not already greater.** Per `CLAUDE.md`'s rule ("Bump it only if the current source protocol is not already greater than the latest released protocol"), any change to `FederationMessage` (new request/response variants for workspace/tab create, or new resync diff fields carried over the wire) that changes wire compatibility must bump `FEDERATION_PROTOCOL_VERSION` to `6` as part of that change — it has not been bumped yet on `master`, so this is a live obligation for any implementation, not already satisfied.

## 7. UI surface

`parse_mount_targets` (`src/app/remote_mount.rs:25-31`) splits the dialog's free-text input on whitespace into a `Vec<String>` of **distinct target strings** — e.g. `"vm105 vm106"` mounts two different hosts in one submission (`src/app/remote_mount.rs:233-238` test). It does not, and structurally cannot, express "mount vm105 twice" or "open N workspaces from vm105": the same target string typed twice would race against the same `HostKey` and the second copy would very likely lose to `double_attach_conflict` (`src/app/api/workspaces.rs:104-122`) once the first dial's mount lands — there is no dedup-before-spawn for identical strings within one submission, but there is nothing that makes a second identical-target mount succeed, since `HostKey` is host-only within one daemon (see Q1).

**"Open 2-5 remote workspaces from vm105" cannot mean "repeated mounts" today, and should not be built that way.** Recommendation: it should mean **one mount + N remote-side workspace-create commands** (or, more precisely, "mount once; then either the remote already reports N workspaces so materialization fans them out automatically (Q2), or the client sends a new `WorkspaceCreateRequest` over the same tunnel to grow the remote host's own workspace count, and a new resync handler materializes each result locally"). Reasoning: `HostKey` identity, the tunnel/mirror/mount-drive-task registry (`AppState.remote_mirrors`, `AppState.mount_drive_tasks`), and the runtime/client boundary guardrail in `CLAUDE.md` (shared runtime facts belong in server state, not duplicated per-connection) all model "one mount = one live tunnel + one mirror to one remote *server process*" as the unit of identity — workspace count on the remote side is data *within* that mount, not a reason to open a second physical connection to the same host. Re-mounting per desired workspace would also re-run the full SSH dial/handshake/snapshot cost and double the tunnel/process overhead for no protocol reason.

## Smallest viable design

Respecting the runtime/client boundary guardrail (shared runtime facts -> server state / neutral names, not TUI-only), the minimum change set is:

1. **Federation wire protocol** (`src/remote/federation/protocol/mod.rs`): add `WorkspaceCreateRequest { request_id, label: Option<String> }` / `WorkspaceCreateResponse::{Created { request_id, new_workspace_id, new_tab_id, new_pane_id, new_terminal_id }, Failed { request_id, reason } }`, following `SplitPaneRequest`/`Response`'s exact shape (Q3). Bump `FEDERATION_PROTOCOL_VERSION` to `6` (Q6) since this is a wire-incompatible addition.
2. **Serving host** (`src/server/federation_accept.rs` + a `FederationCommand::CreateWorkspace` variant in whatever owns `FederationCommand::SplitPane`/`ClosePane`): service the new request the same way `handle_split_pane_request` does — blocking round-trip into the serving `App`'s own `workspace.create`-equivalent internal call, reply with the new ids.
3. **Mounting client** (`src/remote/federation/client.rs`): handle `WorkspaceCreateResponse::Created` in `drive_mount_channel` the same way `SplitPaneResponse::Created` is handled (`:712-821`) — build the local `Workspace`/`Tab`/pane triple via the same helpers `materialize_federation_mount`/`build_remote_pane` already use, emit `AppEvent::FederationResyncWorkspaceCreated` (new), tag `Workspace.id` with the mirror's namespaced id exactly as `:670-679` does.
4. **Reducer diff** (`src/remote/federation/reducer.rs`): extend `ReconcileDiff` with `created_workspaces`/`created_tabs`/`removed_workspace_ids`/`removed_tab_ids` so a `SnapshotResponse` resync (already fetched for `TabCreated`, and would need `WorkspaceCreated` added to `is_structural_event_kind`, Q5) can materialize workspace/tab structural changes the same way pane changes already are — this closes the "remote creates a workspace out of band" half of the feature, independent of whether the *client* ever asks for it directly.
5. **New App-level resync handlers** (`src/app/creation.rs`, next to the two pane ones): `handle_federation_resync_tab_created/removed`, `handle_federation_resync_workspace_created/removed`.
6. **Client-triggered path (optional, for an explicit "new remote workspace" UI action)**: a `WorkspaceCreateParams`-shaped local intent that, when the target workspace is federation-owned (`Workspace::id` has the `r:<host_key>:` prefix), routes to step 1's wire request instead of local `Workspace::from_existing_pane` — this is TUI/App glue, not new shared server state, since the remote-create *result* still lands through the same server-state/materialization path as everything else.
7. **Do not touch** `HostKey`/`double_attach_conflict`/the mount dialog's target parsing (Q1, Q7) — those are correctly scoped to "one tunnel per host"; the multi-workspace feature is entirely inside the mount, not a second mount.

## Unresolved / unverified

- Whether `WorkspaceCreateResponse::Created`'s reply should also carry the label/cwd the client requested, or whether the serving host's own `workspace.create` defaults are acceptable for v1 — not decided by any evidence found; a product/UX call, not a repo fact.
- Whether the serving host should cap the number of remotely-creatable workspaces per federation connection (abuse/resource concern) — no existing precedent found in `src/server/federation_accept.rs`'s other request handlers (`SplitPaneRequest`/`ClosePaneRequest` have no rate limit either), so likely out of scope for parity, but flagged for the eventual plan.
- Exact `FederationCommand` enum name/location that `handle_split_pane_request` sends into (only confirmed by name at the call site, `src/server/federation_accept.rs:595`) was not read in full; the actor/module that owns it should be re-scouted before implementation.
