# Phase 9b — P9.2b Materialization Call-Site Wiring (Option a: JSON-API-in-server)

> SUPERSEDED 260714 — codex gpt-5.6-sol rated this design UNSOUND (cross-process model; see
> reports/codex-gpt56sol-adversarial-review-p9b-option-a-materialization.md). User pivoted to
> option (b): phase-09b-option-b-own-in-proc-session.md. Kept for history + as the (a)/coexistence
> evolution target once v1 (b) ships.


Status: PLAN (stage 4b revision, scoped to P9.2b) — decision 260714: option (a).
Build/test: remote-only, nix host `appn-ltu-vm-100` (resolves to `gpu-ml`, 125 cores);
`nix develop` → `cargo test -- --test-threads=4` + `cargo clippy --all-targets`. Edit locally →
push → pull on remote → build there. macOS workstation cannot compile (per P1 note).

## Goal

Replace the `FederationRoute::Federated` classic-attach fallback (`src/remote/unix.rs:399-419`)
with a live call that materializes the remote mount as a NEW workspace COEXISTING with local
panes in the already-running local server — the original federation goal. Reuse P9.2a
`App::materialize_federation_mount` (dormant, event-shape already correct) as the terminal step.

## Precedent to follow (verbatim from scout)

Worktree deferred dispatch is the template:
- `dispatch_deferred_api_request` (`src/app/api.rs:41-58`) → `handle_deferred_worktree_api_request`
  (`src/app/api/worktrees/deferred.rs:15-31`): builds an `mpsc::channel`, stashes `respond_to`.
- `start_api_worktree_create` (`deferred.rs:94-212`): validate sync, then `std::thread::spawn`
  blocking work, `event_tx.blocking_send(AppEvent::...Finished(... api_request: Some({id, respond_to}) ...))`.
- `handle_api_worktree_add_finished` (`deferred.rs:343-472`): runs on main App loop, mutates
  `AppState`, calls `emit_workspace_open_events` (emits WorkspaceCreated/TabCreated/PaneCreated via
  `emit_event`→`event_hub.push`), then `Self::send_api_response(respond_to, encode_success(id, ...))`.

Client side already exists: `ApiClient::local()` (`src/api/client.rs:39`) → `.request(Request{id,method})`
and `.subscribe_value(...)` (`client.rs:93`) → `EventStream` yielding WorkspaceCreated/TabCreated/PaneCreated.

## The three gaps this phase must close (not just wiring)

1. **Mirror not serializable.** `RemoteMirror` (`src/remote/federation/reducer.rs:82`, `pub(crate)`,
   no serde) cannot cross the API socket. → The new handler RE-PERFORMS the SSH dial + mount inside
   the server process (logic ≈ `attempt_federation_mount`, `unix.rs:289-351`), taking only
   `{ target, session_name, ... }` as JSON params. The CLI process no longer builds the mirror for
   the federated path.
2. **No live-tunnel owner (the crux).** `attempt_federation_mount` kills the SSH child after
   snapshot (`unix.rs:339`). But `build_remote_pane`→`TerminalRuntime::spawn_remote` (P5) need
   `router`/`out_tx`/`clipboard_tx` alive for each pane's whole lifetime. → Introduce a
   **server-owned federation-tunnel registry** (new long-lived state on `App`/`AppState`) that owns,
   per mount: the SSH child process, the `FederationClient` channel machinery, and the
   `TerminalChannelRouter`, keyed by `host_key`. Lives until the materialized workspace is closed.
3. **No tokio handle in `App`.** Match precedent: run the dial+mount on a `std::thread` with its own
   throwaway `current_thread` runtime (as `attempt_federation_mount` already does), not tokio.

## Decomposition (each slice: compiles + full suite green + additive/dormant until the final flip)

### P9.2b-1 — Server-owned federation-tunnel registry + in-server mount

VERIFIED DESIGN (scout 2, all file:line confirmed). `connect_and_mount` (`client.rs:134`) is
ONE-SHOT: returns `MountedConnection{ mirror, agreed_capabilities, reader, writer }`, spawns nothing,
no Drop. The ongoing loop is NOT running — the registry must spawn+own it. Concretely, one
`FederationTunnel` must own for the mount's whole life:
  - `child: tokio::process::Child` — NOT `start_kill()`'d (contrast `unix.rs:339`).
  - write pump: `spawn_mount_writer(writer)` → `(out_tx: UnboundedSender<FederationMessage>, JoinHandle)`.
  - read loop: a NEWLY spawned task running `drive_mount_channel(reader, &mut mirror, &mut router,
    &clipboard_tx)` — today only ever driven inline in `#[tokio::test]`, never spawned in prod.
  - `TerminalChannelRouter` (`client.rs:298`) + `RemoteMirror` — SHARED between the read loop (mutates
    both: applies Event frames to mirror, `route_inbound` to router) and materialization
    (`materialize_federation_mount` reads mirror layout + `router.open_terminal` per pane). → wrap
    `Arc<Mutex<TerminalChannelRouter>>` and `Arc<Mutex<RemoteMirror>>` (or an `Arc<Mutex<MountState>>`
    holding both) so both sides share safely across the thread boundary.
  - RUNTIME OWNERSHIP: `App` holds NO tokio handle (tokio confined to server/headless.rs + main.rs).
    These tasks must run for the SESSION life, not a blocking op — so the registry owns a DEDICATED OS
    thread running a `current_thread` (or multi_thread) tokio runtime that hosts the read+write tasks;
    Drop = signal stop + `start_kill()` child + join thread (mirror `SshStdioBridge`'s stop-flag +
    `Option<JoinHandle>` + Drop-join shape, `unix.rs:1928-1996`).
  - CLIPBOARD TYPE FIX: `materialize_federation_mount` param is `UnboundedSender<ClipboardMessage>`
    (`creation.rs:525`) but `drive_mount_channel` wants bounded `mpsc::Sender<ClipboardMessage>`
    (`client.rs:396`); no prod code wires a clipboard receiver yet. Resolve in this slice: pick ONE
    type (bounded, cap `CLIPBOARD_CHANNEL_CAPACITY`=64, `client.rs:291`) end-to-end, adjust the
    materialize signature, own the `clipboard_rx` in the tunnel (drain per P7 policy or drop until P7
    live-wire).

Files: new `src/app/federation_tunnels.rs` (`FederationTunnel` + `FederationTunnelRegistry`
keyed by `host_key`, insert/get/remove + Drop teardown); `src/app/mod.rs` (registry field on
App/AppState); refactor the dial+mount core out of `unix.rs::attempt_federation_mount` into reusable
`perform_federation_mount(target, remote_herdr, session_name, ssh_opts) -> io::Result<FederationTunnel>`
that RETURNS the live tunnel (does NOT kill child, spawns the loops) — CLI snapshot-only caller keeps a
thin wrapper that reads the mirror then drops the tunnel (Drop kills child), preserving today's
one-shot behavior byte-for-byte.
Dormant: registry has no populator on a live path yet. Tests: registry insert/get/remove + Drop
teardown (no orphan ssh child); `perform_federation_mount` returns a live tunnel whose read loop
applies an injected Event frame to the shared mirror + routes a Terminal frame to an opened pane
channel — all on the loopback duplex substrate P4-P9 tests already use (no real ssh).

### P9.2b-2 — `FederationMaterialize` API method + deferred handler
Files: `src/api/schema.rs` (new `Method::FederationMaterialize(FederationMaterializeParams{target,
session_name,...})`); `src/api/schema/response.rs` (new `ResponseResult::FederationMaterialized{
workspace, tabs, panes }` or reuse WorkspaceCreated shape); `src/app/api.rs` +
`src/app/api/federation/deferred.rs` (new, parallel to worktrees/deferred.rs): generalize
`dispatch_deferred_api_request` to also match `FederationMaterialize`; handler = std::thread mount →
`AppEvent::FederationMaterializeFinished` → on-loop completion inserts tunnel into registry, calls
`materialize_federation_mount(mirror, router, out_tx, clipboard_tx)`, emits events, answers respond_to.
Add `AppEvent::FederationMaterializeFinished` variant. Dormant: handler exists, no live caller.
Tests: handler drives full materialization on loopback (2-pane single-tab → real Workspace/Tab/Panes
reachable via AppState.terminals; IdClass::Remote; federation:<host_key> worktree key), respond_to
gets FederationMaterialized, registry holds the live tunnel.

### P9.2b-3 — Live flip: wire the CLI arm (the only behavior change)
Files: `src/remote/unix.rs` `FederationRoute::Federated` arm — replace the eprintln+classic-attach
fallback with: `ApiClient::local()` → `subscribe_value` (so events render) → `.request(FederationMaterialize{target,session_name})`
→ on success, hand off to the normal interactive/attach loop against the local session now showing
the federated workspace; on error, KEEP the honest fallback (message + classic attach). Remove the
`#[allow(dead_code)]` on the now-live P9.2a + registry surfaces.
Tests: arm issues the API call + subscribes on the federated path; error path still falls back.
Manual: two-machine live smoke (needs buildable 2nd machine + real remote target — out of reach in
this env per P1; note as the one un-automatable check).

Also wire (small, once live call site exists): P6 `PaneRuntime::relayed_agent_status_sender()` +
P7 `pane_source::apply_remote_clipboard_writes` into the materialized entries (both dormant + tested).

## Risks / rollback
- Registry Drop must guarantee SSH child teardown (no orphan ssh procs) — test Drop explicitly.
- Re-dialing inside the server duplicates the CLI's mount handshake — extract to ONE
  `perform_federation_mount` so the two callers can't drift.
- Every slice is additive+dormant until P9.2b-3; revert = drop the last commit, no behavior change to
  any existing launch path (P4/P5/P6/P7/P8 precedent).
- HIGH-risk R7: after cook → impl-notes review → code-review ‖ codex diff gate → ship-gate --hard,
  before the draft PR #1 flips out of draft.

## Open questions
- Registry keying: per `host_key` only, or per (host_key, session) to allow multiple mounts of the
  same host? Default per `host_key` (YAGNI) unless multi-session mount is a stated need.
- Does the local server's API socket require an `events.subscribe` on the SAME connection issuing
  FederationMaterialize, or can the CLI arm open a second subscription connection? Scout: event push
  is per-subscribed-connection, decoupled from the request — a second connection works.
