# Phase 9b — P9.2b Materialization, OPTION (b) v5: live-daemon co-location + own-in-proc session

Status: PLAN v5 (folds codex gpt-5.6-sol v4 re-review: 4 CRIT + 5 MAJ + 2 MIN, report
reports/codex-gpt56sol-adversarial-review-p9b-option-b-v4-lease-fault-persistence.md; adopts its 2
recommended defaults). v4 superseded. (a) SUPERSEDED. Build/test remote-only (nix host gpu-ml).

## Verdict trail
- (a) UNSOUND (mutex deadlock). v1 UNSOUND-tractable. v2 UNSOUND (direction vindicated). v3 UNSOUND
  (CRIT1 RESOLVED, architecture validated). **v4 UNSOUND** — architecture still validated ("D1
  co-location does not need reversal"), but specifying more (delete AppFederationHost, App::new_federated)
  exposed 2 new ownership CRITs (identity, persistence) + refined the lease/fault CRITs. v5 folds all 11.

## Decisions locked
- **D1** co-location via the existing `ServerEvent` actor seam. Disconnect NEVER kills remote (AC7,
  plan.md:147). **AC4 phase-exception now RECORDED in plan.md:143-146** (P9.2b exits to shell; reconnect/
  disconnected-state = FINAL acceptance at P9.3) — closes v4 MIN11.
- **D2** post-start failure = EXIT to shell (INTERMEDIATE P9.2b); classic fallback pre-run only.
- **D3** lazy-start via `ensure_remote_server_running`; legacy dup-App boot DELETED; single-controller v1.
- **D4** forbidden-mutation = BROAD **CLOSED ALLOWLIST** (v4 MAJ7): ALLOW only read-only queries,
  presentation/navigation, remote input, and remote-terminal resize; EVERY other current/future API
  method default-FORBIDDEN. `pane.resize` disambiguated: local-split-geometry resize FORBIDDEN, remote-
  terminal resize (transport command) ALLOWED. Fault visibility = local typed TunnelExit; remote reason
  best-effort; EOF mandatory. Terminal Close = whole-mount fatal v1. Unix-only v1.
- **D5 (v5, adopting v4's 2 recommended defaults):**
  - `ServerInstanceId` = ONE fresh per HeadlessServer PROCESS BOOT; exposed to handshake+mount; ROTATED
    on live-handoff replacement (preserves AC3 restart-fencing, plan.md:139-140).
  - `federation-serve` = TRANSPARENT TRANSPORT ONLY (retries the unix-socket CONNECTION + pipes bytes);
    the LOCAL federation client EXCLUSIVELY owns handshake, version/busy rejection, and mount consumption.

## Seam scout grounding
`server_event_rx` mpsc + `server_event_tx` (headless.rs:231-233); main-loop `select!` arm (headless.rs:574);
`handle_server_event(&mut self,ev)` (headless.rs:2473) = single ingress for thread-originated work;
handoff runs SYNC inside the actor (headless.rs:2805-2821), queued cmds resume after (headless.rs:1384-
1392); Unix accept spawns BEFORE actor registration (client_accept.rs:12-40) — the C1 linearization crux.
Forwarding-aware seq = drain → `handle_api_request_after_internal_events_drained` (headless.rs:2848-2927),
NOT generic `App::handle_api_request` (api.rs:829). Socket recipe headless.rs:361-418.

---

## Slices (each: compiles + full suite green on remote nix host)

### b0 — REMOTE: co-locate a federation controller in the live server

**b0.1 Actor seam + connection supervisor + identity (v4 MAJ5, C4, MIN10).**
- Extend `ServerEvent` with `Federation(FederationCommand)`; every command carries `{accept_epoch,
  connid}` + a `oneshot::Sender<Reply>`. Commands: AcquireController → Accepted|Busy; Mount →
  (ServerInstanceId, SessionSnapshot, EventCursor); EventsAfter; SubscribeOutput → broadcast::Receiver;
  ScrollbackReplay; SendInput; Resize(remote-terminal); AgentStatuses; ReleaseController.
- New arms in `handle_server_event` run on LIVE `self.app`, forwarding-aware drain →
  `handle_api_request_after_internal_events_drained` (headless.rs:2848). NEVER await an AppEvent before
  replying (all listed ops are synchronous: api/session.rs:7-57, api/agents.rs:12-18, event_hub.rs:28-45).
  `drain_internal_events` = no-op.
- **Identity (C4):** `HeadlessServer` owns ONE fresh `ServerInstanceId` per process boot (replacing the
  deleted AppFederationHost owner, serve.rs:470-500), exposed to handshake + Mount; replacement startup
  ROTATES it. (protocol/mod.rs:45-51,76-84; serve.rs:195-229 handshake/mount require it.)
- **Connection supervisor (MAJ5 — fixes the contradictory task graph):** ONE supervisor per federation
  connection = reader + poller/coordinator (drives EventsAfter/AgentStatuses tickers, serve.rs:242-278) +
  per-terminal forwarders + ONE exclusive SERIALIZER. ONLY the serializer touches `AsyncWrite`; reader/
  poller/actor enqueue bounded control/data messages. (Resolves "two write owners interleave frames" AND
  "nobody polls when the peer is silent".)
- **MIN10:** polling commands return `command_changed=false` distinct from `drain_changed`; the handler
  returns their OR (don't suppress a render for a drained state-changing event, headless.rs:2848-2855,
  3067-3071).

**b0.2 Lease FSM + LINEARIZED admission/revocation (v4 C1).**
- `federation_controller: Free | Reserved{epoch,connid} | Mounted{epoch,connid}` + per-conn cancellation/
  completion + per-controller opened-terminal set.
- **Linearization (C1 core):** CLOSE ADMISSION before revocation; REGISTER every accepted stream in the
  actor BEFORE it may enqueue a command (accept spawns before registration today, client_accept.rs:12-40);
  attach `{accept_epoch, connid}` to EVERY command and validate ALL of them — incl. AcquireController,
  read-only ops, Release — against the current epoch/lease; IGNORE any command whose reply receiver is
  canceled; use direct worker-completion guards, NEVER actor events. So a stale queued Acquire/Mount
  behind a handoff can't resurrect authority after rollback→Free.
- Admission (MAJ5): AcquireController → atomic Reserve; second handshake → typed Busy; Reserve→Mount
  atomic promotion; RAII release; mutating cmds authorized vs active {epoch,connid} + require prior Open.
  Compare-and-clear release on EOF (connid+epoch compare → late EOF can't drop a newer lease).
- Add: reservation EXPIRY, handshake TIMEOUT, pending-connection CAP; queued-stale tests per command.
- **Handoff (C1 deadlock-free):** revoke authority, CLOSE accepted streams (unlink != close, ipc.rs:246),
  CANCEL pending oneshot replies, wait boundedly WITHOUT actor-dependent joins.

**b0.3 Fault-control lane + guaranteed EOF via connection supervisor (v4 C2).**
- BUMP `FEDERATION_PROTOCOL_VERSION`; add a versioned wire fault (protocol/mod.rs:215 has none).
- The per-conn supervisor (b0.1) owns an ATOMIC FIRST-CAUSE cell + an INDEPENDENT `shutdown(Both)`/abort
  handle on the transport (writers block inside `write_frame().await`, serve.rs:168-175, client.rs:444-
  460 — a fault queued elsewhere can't reach a stuck writer). On first fault: retain cause → best-effort
  prioritize the fault frame → BOUNDEDLY terminate the writer → FORCE transport EOF. Secondary EOF/cancel
  must NOT overwrite the initiating cause. BOUNDED queues both directions (serve.rs:233 unbounded today) +
  byte AND message budgets. Retained `TunnelTasks` → typed `TunnelExit` (reader/writer/child/panic) that
  b2's supervisor `select!` can win. Test a non-reading peer with every budget saturated.

**b0.4 Socket lifecycle with typed unlink + length-safe naming (v4 MAJ8).**
- First-class server-owned unix socket (D4 unix-only). **Naming:** derive `*-federation.sock` from the
  WHOLE effective classic path with a DETERMINISTIC HASH FALLBACK for sun_path limits (existing remote
  code already hash-shortens, unix.rs:2197-2231); avoid `x` vs `x.sock` collisions across all override
  precedence (socket_paths.rs:14-66). Created via prepare/bind/restrict-0600/identity.
- **Typed unlink (MAJ8):** `remove_socket_file_if_owned` returns the same Ok for removed/absent/not-owner
  (ipc.rs:246-264) → replace with TYPED unlink outcomes + an unlink BITMAP; rollback restores/waits ONLY
  actually-removed paths; preserve original listener on pre-unlink failure, rebind only after real unlink.
- **Acceptance = actor-polled** (mirror the nonblocking-listener + spawn-handshake pattern, client_accept.rs
  :12-40 — NOT a separate cancellable accept task) so admission-closure + handoff share ONE authority.
- Full lifecycle ownership: construction / accept / normal shutdown+Drop (headless.rs:3798) / session
  stop-wait (session.rs:236) / replacement readiness (headless.rs:4110) / rollback (headless.rs:1132). NOT
  SCM_RIGHTS-transferred (handoff.rs:33 only pane runtimes); replacement binds FRESH + no controller +
  rotated ServerInstanceId. `wait_owned_ack` unchanged (pane-runtime ack).
- Gate listener + proxy + CLI definition + dispatch together on `#[cfg(unix)]`.
- DELETE live use of `AppFederationHost::boot` (serve.rs:483).
Tests: live-App Mount + live-pane stream w/o new shell; disconnect leaves PTY alive (no shutdown/PaneDied/
SIGHUP/child-exit); second handshake Busy; input-before-Open rejected; stale queued Acquire post-rollback
rejected; egress saturation → TunnelExit with correct first-cause; handoff revokes+closes+cancels w/o
deadlock; socket unlinked+recreated+restored-on-rollback+cleaned-on-Drop/stop; ServerInstanceId rotates
post-handoff; classic snapshot unchanged.

### b0-proxy — REMOTE: `federation-serve` = TRANSPARENT transport only (v4 MAJ9, D5)
Retries only the unix-socket CONNECTION to `*-federation.sock` (derived path), then pipes stdin↔socket
bytes (mirror unix.rs:437). No live server → lazy-start via `ensure_remote_server_running` (D3). It does
NOT handshake/mount — the LOCAL federation client owns handshake, version/busy rejection, mount (D5;
client.rs:130-219 already consumes the sole handshake+mount, serve.rs:188-231 emits exactly one).

### b1 — LOCAL: tunnel keep-alive + single dial + timeouts
`dial_federation(...) -> LiveTunnel{child,reader,writer}` (don't kill child, contrast unix.rs:339); CLI
snapshot wrapper keeps immediate-kill byte-for-byte. SINGLE dial replaces the whole opt-in mount/route
block (unix.rs:381-397), move `MountedConnection` into b2. `--session <quoted>` before `federation-serve`
(fixes CRIT4). Typed connect/handshake/mount TIMEOUTS (unbounded reads client.rs:147,186). RAII ChildGuard;
add tokio-util CancellationToken (`rt`) to Cargo.toml:38. Tests: wrapper still kills; dial live; timeout →
clean fallback.

### b2 — LOCAL: in-proc federated session runner (v4 C3, MAJ6, MAJ7 + supervision + teardown)
`run_federated_session(...)` mirroring main.rs:766-843:
- **Persistence isolation (C3 — MUST NOT clobber the classic session):** construct via a mode with an
  immutable `SessionPersistencePolicy::Disabled` contract covering restore AND save-scheduling AND history
  AND exit-save AND clear (existing `no_session` gates restore app/mod.rs:366-405 AND exit-save :1107-1110;
  materialization schedules a save creation.rs:681-684 which can clear state session.rs:39-105). NOT merely
  "restoration disabled." Test: a pre-seeded classic snapshot is byte-for-byte unchanged after successful/
  empty/failed/faulted federated sessions.
- **Eager-open vs bounded queue (MAJ6):** START the read/supervision path BEFORE issuing Opens (or split
  materialization into sync model-creation + async bounded Open issuance) — else many Opens trigger
  scrollback replay + live output while the reader is down → b0.3 egress overflow fatally closes a healthy
  mount at startup (creation.rs:689-727 emits Open per pane; client.rs:313-325 today relies on unbounded).
  Test: many panes, max replay + live output during mount.
- C1-safe order otherwise: spawn writer → materialize (eager model) → move router+reader+mirror into ONE
  drive task. Require NON-EMPTY materialization (else abort→classic fallback); activate a workspace; enter
  terminal mode.
- **Mutation policy = CLOSED ALLOWLIST (D4/MAJ7):** allow ONLY read-only queries + presentation/navigation
  + remote input + remote-terminal resize; EVERY other API method (rename/move/close/swap/zoom/layout/
  split-ratio — schema.rs:66-207; workspaces.rs:90-154,227-259; tabs.rs:140-270; panes.rs:388-598,1085-
  1136,1521-1588) default-FORBIDDEN. Low-level guard at the authoritative local-runtime-creation seam
  (covers navigate.rs:769/880/935, api.rs:450 respawn, agent_resume.rs:204, panes.rs:777 move + the
  earlier creator set). Federated no-op `ensure_default_workspace`. Remote materialization stays on
  `spawn_remote`. Tests: every mutation FAMILY + default-closed.
- **Supervision:** outer `select!` pinned app.run() vs retained TunnelTasks/TunnelExit; tunnel wins → end
  scope → drop App → restore TTY + exit (D2). Any TunnelExit → restore+exit, no silent drop.
- Clipboard two distinct sinks (client.rs:390 bounded + creation.rs:521 unbounded), both dropped. Release-
  mode generation fence (client.rs:341). Teardown RAII: armed terminal guard + ChildGuard; order end-App-
  scope → drop App → cancel/await reader → drop senders → await/abort writer → kill/wait child → restore;
  drain SSH stderr; restore on panic.
Tests (headless): materialize→cancel no orphan; empty-mount abort; persistence-unchanged; eager-open bounded;
every mutation family forbidden; TunnelExit → restore+exit.

### b3 — LOCAL: live flip
`run_remote` federated branch → `run_federated_session`; pre-run Err → classic fallback; post-start → exit
(D2). Remove dead_code allows. Tests: federated path invoked; pre-run fallback; --session present. Manual:
two-machine smoke (out of env per P1).

## Residual v1 scoping (ENFORCED)
M9 bytes-only (sinks). M10 additions/renames/layout omitted; close/gap/overflow fail-fast. AC4 reconnect/
disconnected → P9.3 (recorded plan.md:143). No local coexistence (construction-time mode). Per-pane
reopen/Close → whole-mount-fatal v1. Windows listener → not v1.

## Slice readiness / start order
b0.1 (actor+identity+supervisor) → b0.2 (linearized lease) → b0.3 (fault+EOF) → b0.4 (socket) → b0-proxy →
b1 → b2 → b3. First acceptance test: mount live App, stream a PTY, disconnect, prove PTY alive.

## Risks / rollback
b0 touches the LIVE server + handoff/shutdown/Drop/stop — highest risk; additive behind the federation
socket. Snapshot wrapper preserves immediate-kill. b1/b2 dormant until b3. R7 tail after cook.

## Scope reality
TWO-SIDED, multi-week, remote-build-only. b0 = actor lease FSM (linearized) + fault-control lane (EOF-safe)
+ full socket lifecycle + identity ownership + persistence isolation + versioned protocol bump.

## Open questions (for v5 re-review)
- Does `{accept_epoch, connid}` on EVERY command + close-admission-before-revoke + register-before-enqueue
  fully LINEARIZE admission/revocation against the sync-in-actor handoff (headless.rs:2805) with zero
  remaining resurrect-authority path?
- Does the connection-supervisor first-cause + independent shutdown(Both) GUARANTEE EOF/TunnelExit even
  with a stuck mid-frame writer and all budgets saturated?
- Does `SessionPersistencePolicy::Disabled` cover EVERY persistence write path (restore/save/history/exit/
  clear) so a federated session can NEVER mutate the classic snapshot?
- Is the CLOSED-ALLOWLIST enforceable at a single API-dispatch choke + one local-runtime-creation seam, or
  do presentation/navigation vs mutation still interleave in a way that needs per-method classification?
