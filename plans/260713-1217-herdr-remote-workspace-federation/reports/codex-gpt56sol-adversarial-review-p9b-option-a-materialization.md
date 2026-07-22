# Codex gpt-5.6-sol adversarial review — P9.2b option (a) materialization call-site design

Reviewer: codex-cli 0.144.1, model `gpt-5.6-sol`, reasoning effort `xhigh`, sandbox read-only,
workdir `~/Projects/herdr`. Read-only; no files changed, no build/tests run. Date 260714.
Target: `phase-09b-materialization-call-site-option-a.md` (verified against actual source).

## Verdict: UNSOUND

Option (a) remains viable, but this design cannot be implemented safely as written.

## Findings

1. **[CRITICAL] Shared `Arc<Mutex>` state deadlocks materialization.** `drive_mount_channel` holds
   `&mut RemoteMirror` and `&mut TerminalChannelRouter` throughout its indefinite `read_frame().await`
   loop (client.rs:390, 399); materialization then needs that router (creation.rs:521). App loop can
   never acquire those locks. `tokio::Mutex` does not fix the ownership problem. Fix: read task
   exclusively owns `RemoteMirror`; expose a cloneable router handle whose internal sender map is
   locked only for individual insert/lookup/remove, never across an await.

2. **[CRITICAL] Generation fencing absent + incompatible with reconnect.** Every terminal frame
   carries `mount_generation` (protocol/mod.rs:112), but `route_inbound` ignores it and keys only by
   raw terminal ID (client.rs:299, 341). A reused client increments generation on reconnect
   (client.rs:204) while every new `federation-serve` accepts only generation `1` (serve.rs:33, 370).
   Reuse → server-rejected frames; recreating client resets fencing. Fix: negotiate client-selected
   generation at mount, echo on all channels; validate terminal + agent-status generations before
   routing; key registrations by `(generation, terminal_id)`; cancel old drivers before activating
   new generation. Resolve that `FedRef` stores epoch but public IDs omit it (id.rs:63).

3. **[CRITICAL] `{target, session_name}` is not a durable server-side SSH recipe.** CLI resolves the
   actual remote executable, may install it, owns managed SSH control/config paths (unix.rs:367, 915);
   those private values are destroyed when CLI ownership ends (unix.rs:530, 678, 688). Detached server
   has null stdio → password, host-key, install confirmation cannot work (autodetect.rs:211); the API
   request has no timeout (client.rs:51, server.rs:718) → can hang instead of falling back. Named
   sessions mislabeled: command explicitly omits `--session` (unix.rs:186). Fix: define either a
   server-owned noninteractive SSH policy with durable config + typed timeout, or an explicit
   CLI→server control-master/config ownership transfer. Pass selected remote exe + session correctly.

4. **[CRITICAL] The deferred API method would not reach production dispatch.**
   `dispatch_deferred_api_request` is an internal helper recognizing only worktrees (api.rs:41). Real
   headless + monolithic paths independently hardcode those two deferred methods (headless.rs:2891,
   runtime.rs:72). Plan's file list omits both, plus exhaustive method naming + synchronous fallback
   matches (server.rs:307, api.rs:916). Fix: one generic deferred dispatcher used by both production
   paths; add explicit synchronous rejection, UI-change classification, event handling, schema tests,
   method naming.

5. **[CRITICAL] Proposed teardown can hang the server + leave silently frozen panes.** Writer exits
   only after all `out_tx` clones disappear (client.rs:434); every materialized pane + forwarding task
   retain clones (creation.rs:716, pane_source.rs:117). Reader has no shutdown branch → `start_kill()`
   + `join()` can block indefinitely. `SshStdioBridge` is not an adequate precedent (50ms poll loop,
   owns no child; unix.rs:1950). Remote pane readers intentionally exit without `PaneDied`/disconnected
   state (pane_source.rs:106). Fix: a supervisor must `select!` on shutdown/read outcome, abort writer,
   kill + `wait()` child, notify App of `LinkClosed`/`ResyncRequired`, only then terminate its runtime
   thread. **Do not make P9.2b live before at least the P9 disconnected-state FSM exists.**

6. **[MAJOR] Slice 3 double-mounts; extraction cannot preserve one-shot.** `run_remote` already
   completes the CLI mount before computing `FederationRoute` (unix.rs:381); replacing only the
   `Federated` arm triggers a second server mount. Making the old wrapper build a full live tunnel then
   drop it also starts tasks + consumes frames before teardown, unlike today's immediate kill
   (unix.rs:338). Fix: extract `dial_and_mount -> (ChildGuard, MountedConnection)`; snapshot wrapper
   kills immediately without starting pumps; live constructor consumes connection + starts supervisor.
   Replace the whole federation routing block so the API result — not a preliminary CLI mount — selects
   fallback.

7. **[MAJOR] Materialization is non-transactional; claimed event shape is false.** Mutates
   runtime/state incrementally, can fail after earlier panes installed (creation.rs:572, 639); failed
   split insertion silently ignored after runtime registered (creation.rs:662). Additional-tab events
   occur before `WorkspaceCreated`, split panes get no `PaneCreated`, and the function already emits
   events (creation.rs:675, 681). A single `WorkspaceCreated` response cannot represent the multiple
   workspaces materialization returns (response.rs:57). Fix: prepare all panes off-state, atomically
   commit AppState/runtimes/router regs/registry entry/correctly-ordered events, else roll back. Plural
   result DTO.

8. **[MAJOR] Registry ownership contradicts v1; lacks request/close fencing.** V1 explicitly permits
   ONE mount, enforced by `AppState::begin_federation_mount` (state.rs:1516); proposed host-key map
   lets concurrent requests dial before either inserts. `HostKey` already includes the session
   discriminator → the "keying" open question is invalid (id.rs:17). A mount can create multiple
   workspaces; plan doesn't define whether closing one or the last tears it down. Fix: reserve the
   single mount slot synchronously with an operation ID, reject stale completions, track exact set of
   materialized workspace IDs until last closes. Runtime registry belongs on `App`, not pure-data
   `AppState` (state.rs:1314). App-only field = modest constructor ripple; AppState field violates
   architecture + touches production + test literals.

9. **[MAJOR] Clipboard / agent-status "small wiring" is underspecified.** Bounded driver sender is
   wire ingress (client.rs:390); unbounded materialization sender is OSC52 output from the local mirror
   emulator (pane.rs:1844). DIFFERENT flows → choosing one bounded channel "end-to-end" is not
   automatically correct. Converting the emulator requires changes through `PaneRuntime`,
   `TerminalRuntime`, the P7 consumer, tests (terminal/runtime.rs:148, pane_source.rs:206). Meanwhile
   `drive_mount_channel` discards every `AgentStatus` frame (client.rs:423). Fix: separate bounded
   ingress queues feeding one App clipboard-policy sink, define overflow semantics, register a
   `(generation, remote_terminal_id) → relayed_agent_status_sender` mapping in the router.

10. **[MAJOR] The live event loop does not make workspace topology live.** `EventFrame` carries only
    sequence + kind (protocol/mod.rs:86); applying one merely advances the mirror cursor
    (reducer.rs:223). Only snapshot reconciliation changes mirror entities, and nothing projects later
    mirror changes into materialized AppState. Planned "Event frame applies" test could pass while
    remote workspace/tab/pane changes remain invisible. Fix: extend protocol with typed deltas, or
    trigger snapshot reconciliation for relevant events + transactionally project the diff into
    AppState.

11. **[MAJOR] `subscribe_value` neither renders nor ensures a local server.** Remote launch returns
    before normal local auto-detect startup (main.rs:692); `ApiClient::local()` only connects to a
    socket. Subscriptions use separate connections; subscribing first closes the narrow event race, but
    subscriptions begin at sequence zero + replay retained history (subscriptions.rs:115). More
    importantly, rendering happens through the thin-client socket, NOT the JSON event stream
    (autodetect.rs:291). Fix: ensure/start the local server first, issue the typed materialization
    request, then explicitly attach the local thin client + skip `SshStdioBridge`/classic remote
    attach. Omit the event subscription unless it has a real correlated consumer.

## Required ownership redesign (replaces the plan's Arc<Mutex> router+mirror)

The proposed `Arc<Mutex<RemoteMirror>> + Arc<Mutex<TerminalChannelRouter>>` is NOT the right call.
Use this split:
- Tunnel supervisor exclusively owns SSH child, reader, writer task, live `RemoteMirror`.
- App owns a `FederationTunnelHandle`, workspace ownership metadata, pure presentation state.
- Materialization receives an immutable `MaterializationSnapshot` cloned from the atomic mount — not
  the live mirror.
- `TerminalRouterHandle` = internally shared map keyed by `(generation, terminal_id)`. `open_terminal`
  inserts synchronously before queuing `Open`; inbound routing locks only for lookup/`try_send`, never
  across `.await`.
- Each router entry also carries the agent-status sink. Queue overflow → explicit pane desync/reopen,
  not silent terminal-state corruption.
- Tunnel outcomes + mirror deltas return to App via events/channels.
- Shutdown first closes pane runtimes/router entries, then signals the supervisor, which aborts tasks,
  kills + reaps SSH, reports completion.
Note: the same logical router map IS mandatory (`open_terminal` inserts pane sender client.rs:313,
`route_inbound` retrieves client.rs:341); the same `RemoteMirror` INSTANCE is not required for initial
materialization.

## Verified correct (design assumptions that hold)

- `connect_and_mount` is genuinely one-shot, returns untouched reader/writer halves (client.rs:91).
- `RemoteMirror` crate-private + non-serializable (reducer.rs:80); server-owned transport reasonable.
- Worktree deferred-completion pattern is a valid main-loop mutation precedent.
- Clipboard type mismatch is real.
- Dedicated runtime thread is viable but OPTIONAL: production App paths already run inside tokio
  runtimes (headless.rs:3952). If retained, its runtime must stay alive around the full supervisor.

## Unresolved questions (need product/architecture decisions)

- Is P9.2b-3 allowed to become user-visible before the required P9 disconnected/reconnect behavior
  (P9.3 FSM)?
- Which durable SSH authentication / control-master ownership model is accepted (server-owned
  noninteractive policy vs CLI→server ownership transfer)?
- Does one mount materialize every remote workspace, and does teardown occur only after the last
  materialized workspace closes?
