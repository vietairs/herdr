# b2.3 keystone seam map — run_federated_session + App::new_federated

Scope: DORMANT wiring of b2.1 (SessionPersistencePolicy::Disabled) + b2.2 (federated_session_allows)
+ b1 (dial_federation/LiveTunnel/ChildGuard). Nothing calls it until b3. READ-ONLY scout; anchors =
current tree on feat/remote-workspace-federation. Grammar sacrificed for concision.

## 1. CLASSIC template to mirror — main.rs
- Enclosing fn: `run_app_session` body inside `rt.block_on(async { ... })` at **main.rs:773-828**
  (tokio multi-thread rt built main.rs:768-771; `rt.shutdown_timeout(100ms)` main.rs:831).
- Terminal-mode entry: **main.rs:774-792** — `ratatui::init()` (774); clear host mouse reporting (775);
  mouse capture on/off per config (776-780); `EnableBracketedPaste, EnableFocusChange` (781);
  color-scheme reports (782); `push_keyboard_enhancement_flags` (783); optional modifyOtherKeys (788-792).
- App construct + run: **main.rs:794-801** — `app::App::new(config, true /*no_session*/, config_diagnostic,
  api_rx, event_hub)`; `app.run(&mut terminal).await` (801).
- TTY restore path: **main.rs:804-822** — reset modifyOtherKeys (804-808); clear kitty graphics (810-812);
  `pop_keyboard_enhancement_flags` (813); `DisableFocusChange/BracketedPaste/MouseCapture` (814-819);
  clear host mouse reporting (820); color-scheme off (821); `ratatui::restore()` (822).
- Exit path: `drop(app)` **main.rs:825** BEFORE rt shutdown (comment: drop workspaces/panes first), then
  return `result`. rt killed at 831.
- KEY for b2.3: the restore block 804-822 must run on the tunnel-wins select! branch too (RAII armed
  terminal guard) — main.rs restores unconditionally on any `run()` return; federated must restore on
  tunnel-exit AND app-exit. Mirror the drop-before-rt-shutdown order.

## 2. App constructor + persistence gates — src/app/mod.rs
- `App::new` signature: **mod.rs:386-391** `(config: &Config, no_session: bool, config_diagnostic:
  Option<String>, api_rx: UnboundedReceiver<ApiRequestMessage>, event_hub: EventHub) -> Self`.
  Sets `no_session` (mod.rs:770) + `persistence: SessionPersistencePolicy::Enabled` (mod.rs:771).
- `SessionPersistencePolicy` enum: **mod.rs:218-227** (Enabled/Disabled); `is_disabled()` **mod.rs:229-234**.
  Field on App: `persistence` **mod.rs:106** (immutable contract). `no_session` field mod.rs:103.
- RESTORE gate (b2.3 must cover — a READ, deferred from b2.1): **mod.rs:405-410** `... = if no_session {
  (Vec::new(), ...) } else { restore... }`. To keep classic snapshot untouched, new_federated must take the
  no_session=true branch (empty) — restore is disabled by construction, never touches disk.
- SAVE/CLEAR gates already b2.1-gated with `!no_session && !persistence.is_disabled()`:
  exit-save **mod.rs:1143-1145** (`save_session_now()` in run() teardown); config-reload clear_history
  **mod.rs:1475-1476**; plus session.rs schedule/background/now saves (session.rs:15/61/91 per b2.1 note).
  materialize's `schedule_session_save()` **creation.rs:683** is NOT yet gated by persistence (see §3 RISK).
- `new_from_handoff` (mod.rs:786) — a SECOND constructor precedent; sets `app.no_session=false` (810). Shows
  the "construct-then-adjust" pattern; b2.3's `new_federated` should instead set fields at construction
  (immutable Disabled). Recommend: `App::new_federated(...)` = call `App::new(config, true, ...)` then
  overwrite `persistence = Disabled` before any use (mirrors handoff's post-construct field set), OR add a
  private core ctor. Simplest DRY: wrap new() + set persistence=Disabled (no_session already true → restore
  empty, all save/clear gates already fire off).
- `App::run` (the pinned future for the supervision select!): **mod.rs:907** `pub async fn run(&mut self,
  terminal: &mut DefaultTerminal) -> io::Result<()>`.
- `materialize_federation_mount`: **creation.rs:521-527** (see §3). `ensure_default_workspace` (federated
  no-op target): **mod.rs:1167**; called at runtime.rs:81/88, headless.rs:589/3219. For federated, must NOT
  auto-create a local default workspace — spec wants a no-op; the guard/mode must suppress it.

## 3. Open emission + save-schedule + clipboard — src/app/creation.rs
- `materialize_federation_mount(&mut self, mirror: &RemoteMirror, router: &mut TerminalChannelRouter,
  out_tx: &UnboundedSender<FederationMessage>, clipboard_tx: &UnboundedSender<ClipboardMessage>)
  -> io::Result<Vec<usize>>`: **creation.rs:521-527** (`#[allow(dead_code)]` 520).
- Open emission per pane: `build_remote_pane` **creation.rs:700-743**; opens channel
  `router.open_terminal(raw_id, mount.mount_generation, out_tx)` **creation.rs:713** → drives an OUTBOUND
  `Terminal::Open` per pane (client.rs:326). Event emission: `emit_tab_created_events` (creation.rs:676),
  `emit_workspace_open_events` **creation.rs:682**.
- SAVE-SCHEDULE that persistence MUST gate (C3 clobber risk): **creation.rs:681-684** loop —
  `emit_workspace_open_events(*ws_idx); self.schedule_session_save();`. `schedule_session_save` is NOT yet
  wrapped by the persistence policy inside materialize. RISK: with a Disabled-policy App, this line still
  calls schedule → session.rs write path. VERIFY session.rs:15 `schedule_session_save` honors
  `!persistence.is_disabled()` (b2.1 note says session.rs:15/61/91 gated). If session.rs already gates
  internally, creation.rs:683 is safe as-is; if not, b2.3 must gate here. **UNCONFIRMED — read
  session.rs:15 to confirm the schedule path early-returns under Disabled.**
- Clipboard sinks (two distinct, both must be dropped on teardown):
  (a) OUTBOUND local→remote UNBOUNDED: `clipboard_tx: UnboundedSender<ClipboardMessage>` threaded through
  materialize **creation.rs:526** → build_remote_pane 710 → `spawn_remote(...clipboard_tx.clone()...)`
  creation.rs:727. Created by run_federated_session (template: test harness `unbounded_channel()`
  creation.rs:879).
  (b) INBOUND remote→local BOUNDED: `clipboard_tx: &mpsc::Sender<ClipboardMessage>` in drive_mount_channel,
  `try_send` **client.rs:426** (see §5).
- Drive-loop template (eager-open ordering evidence): test `successful_mount_materializes...`
  **creation.rs:870-931** shows the exact object graph run_federated_session assembles: RemoteMirror
  seeded, `TerminalChannelRouter::new()` (877), `out_tx/out_rx` unbounded (878), clipboard channel (879),
  `materialize_federation_mount(&mirror,&mut router,&out_tx,&clipboard_tx)` (881-883). Empty-mirror →
  creates nothing (creation.rs:934-948) — matches spec's NON-EMPTY-required abort→classic-fallback.

## 4. Mutation allowlist + dispatch entrances — src/api + src/app
- b2.2 allowlist fn: `federated_session_allows(method: &Method) -> bool` **src/api/mod.rs:84**
  (`#[allow(dead_code)]`, exhaustive 81-variant match, no wildcard; 36 allow / 45 forbid). Tests
  api/mod.rs:179-206.
- Two dispatch entrances to gate (per notes: "api.rs:829 sync + the deferred entrance"):
  1. SYNC: `App::handle_api_request(&mut self, request) -> String` **src/app/api.rs:829** (drains
     internal events NON-forwarding at api.rs:830).
  2. DEFERRED/forwarding: `handle_api_request_message` **src/app/runtime.rs:60** (drains WITH forwarding,
     `drain_all_internal_events()` runtime.rs:77); also `handle_api_request_after_internal_events_drained`
     call sites mod.rs:2174/2210/2242/2278/2311.
  b2.3 gates BOTH: in federated mode, reject any `!federated_session_allows(&request.method)` before it
  reaches a mutating handler. (Spec: "default-FORBIDDEN".)
- Low-level runtime-creation backstop (defense-in-depth): authoritative local-PTY spawn =
  `spawn_with_portable_pty` **src/pane.rs:2182** (called from pane.rs; backend `spawn_with_portable_pty`
  src/pty/backend/unix.rs:12). The spec's stale line refs (navigate.rs:769/880/935, api.rs:450,
  agent_resume.rs:204, panes.rs:777) map onto the CURRENT tree as: local-input navigation
  `src/app/input/navigate.rs`; pane API mutators `src/app/api/panes.rs`; agent resume
  `src/app/agent_resume.rs` (+ `src/agent_resume.rs`). NOTE: the numeric anchors in the spec do NOT match
  the current file layout (api/ was split; navigate/panes/agent_resume moved under src/app/**). **The
  authoritative single choke = the local-spawn permit before pane.rs:2182** (one guard covers every
  creator per notes L904). Recommend the closed-allowlist at the two dispatch entrances (§4.1/4.2) as
  primary; the pane.rs:2182 spawn permit as the last-resort backstop.

## 5. Federation client drive objects — src/remote/federation/client.rs
- `MountedConnection<R,W>{ mirror, agreed_capabilities, reader, writer }` **client.rs:95-100** — output of
  `connect_and_mount` **client.rs:135-220** (handshake+atomic mount; unbounded reads at read_frame 162/187
  → b2 must wrap in FEDERATION_CONNECT/MOUNT_TIMEOUT, see §6).
- `FederationClient::new(host_key, local_caps, required_caps)` **client.rs:116-129**.
- ONE drive task (C1-safe: move router+reader+mirror+clipboard_tx together):
  `drive_mount_channel(reader: &mut R, mirror: &mut RemoteMirror, generation: u64, hub: &EventHub,
  router: &mut TerminalChannelRouter, clipboard_tx: &mpsc::Sender<ClipboardMessage>) -> Result<DriveOutcome>`
  **client.rs:395-402**. Loop client.rs:404-...; routes Event/Terminal/Clipboard/Fault.
- Router: `TerminalChannelRouter` **client.rs:303-306**; `open_terminal` **client.rs:318-332** (emits
  outbound Open 326); `route_inbound` **client.rs:346-371** (try_send, never await, S2.2 isolation).
- BOUNDED clipboard sink (inbound): `clipboard_tx.try_send(clip_msg)` **client.rs:426**.
- Fault→exit: `FederationMessage::Fault(fault) => return Ok(DriveOutcome::Faulted(fault.reason))`
  **client.rs:428-432**. `DriveOutcome` enum **client.rs:225-238** (LinkClosed/Faulted/ResyncRequired).
- Generation fence (spec "release-mode fence client.rs:341" is stale): the real fence =
  `mirror.apply_event_message(&event_msg, generation)` **client.rs:410** → `fence(&self.mount, generation)`
  in reducer **reducer.rs:233** (and :149), returns FenceResult::RejectStale for a superseded mount. A task
  left running post-reconnect can never mutate a newer mirror (client.rs:243-245 doc). generation set at
  connect_and_mount `next_generation.fetch_add` **client.rs:205**.

## 6. b1 surfaces — src/remote/unix.rs
- `dial_federation(target: &str, remote_herdr: &RemoteHerdr, session_name: &str,
  ssh_options: Option<&ManagedSshOptions>) -> Result<LiveTunnel, FederationMountFailure>`
  **unix.rs:383-414** (`#[allow(dead_code)]` 382). Spawns ssh `-T`, keeps child alive (NO start_kill),
  returns piped stdin/stdout. `--session <name>` ordering via `remote_federation_serve_command`
  unix.rs:191-194 (CRIT4 fix).
- `LiveTunnel{ guard: ChildGuard, reader: ChildStdout, writer: ChildStdin }` **unix.rs:371-375**.
- `ChildGuard(tokio::process::Child)` **unix.rs:360**; `Drop` → `start_kill()` **unix.rs:362-366** (RAII kill).
- Timeouts (b2 applies): `FEDERATION_CONNECT_TIMEOUT=10s` **unix.rs:421**, `FEDERATION_MOUNT_TIMEOUT=15s`
  **unix.rs:423**. Wrap connect_and_mount's unbounded reads (client.rs:162/187) in tokio::time::timeout.
- NOTE: spec says "retained TunnelTasks/TunnelExit" — no `TunnelTasks` struct exists (LiveTunnel is the
  retained handle; ssh child + reader/writer). `TunnelExit` is the server-side fault enum
  **src/server/federation_fault.rs:32** (client side surfaces it as `DriveOutcome::Faulted(FaultReason)`
  client.rs:231). Supervision select! pins app.run() vs the drive task's DriveOutcome (LinkClosed/Faulted)
  — tunnel wins → end scope → drop App → restore TTY + exit (D2).

## 7. b3 flip point — src/remote/unix.rs
- `run_remote(remote: RemoteLaunch) -> io::Result<()>` **unix.rs:425**.
- Federation decision: `federation_requested_now` **unix.rs:456-457**; today `attempt_federation_mount`
  (one-shot snapshot dial) **unix.rs:458-465**; `decide_federation_route` **unix.rs:466-469**.
- THE FLIP: `match &route { FederationRoute::Federated => {...} ...}` **unix.rs:470-496**. The
  `FederationRoute::Federated` arm **unix.rs:471-491** currently ONLY eprintln's "interactive federated
  rendering is not yet wired ... attaching via the classic full-screen view instead" (483-489) then FALLS
  THROUGH to classic attach (SshStdioBridge::start unix.rs:498-504 + run_client_process 506). b3 replaces
  this arm: on federation requested → `dial_federation` + `run_federated_session(...)`; pre-run Err →
  classic fallback (the existing 498-506 path); post-start success → return/exit (D2, do NOT fall through
  to classic attach). Remove `#[allow(dead_code)]` allows across b1/b2 objects at b3.

## Recommended implementation order (b2.3, dormant)
1. `App::new_federated` (mod.rs) — wrap `App::new(config, /*no_session*/ true, ...)`, then set immutable
   `persistence = SessionPersistencePolicy::Disabled`; make ensure_default_workspace a no-op in federated
   mode. VERIFY restore takes the empty branch (mod.rs:405-410) + all 5 save/clear gates fire off.
2. Persistence-safety FIRST TEST: pre-seed a classic snapshot, run a stubbed federated App lifecycle
   (materialize→drop), assert snapshot byte-for-byte unchanged (covers creation.rs:683 schedule + exit-save
   mod.rs:1143). Fix creation.rs:683 gating iff session.rs:15 doesn't already early-return under Disabled.
3. Mutation guard wiring: gate `federated_session_allows` (api/mod.rs:84) at BOTH dispatch entrances
   (app/api.rs:829, app/runtime.rs:60) + spawn backstop before pane.rs:2182; test every mutation family
   default-closed.
4. `run_federated_session(...)` skeleton (remote/unix.rs or a new remote/federation module): dial (b1) →
   connect_and_mount under timeouts (client.rs:135 + unix.rs:421/423) → REQUIRE non-empty mirror (else
   abort→classic). EAGER-OPEN (MAJ6): start the drive task (drive_mount_channel client.rs:395) BEFORE / or
   split from issuing Opens (materialize creation.rs:713 Opens) so scrollback replay can't overflow a
   down reader.
5. C1-safe assembly: spawn writer → materialize (eager model) → move router+reader+mirror+clipboard into
   ONE drive task. Two clipboard channels (outbound unbounded creation.rs:879-pattern + inbound bounded
   client.rs:401/426), both dropped on teardown.
6. Supervision select!: pin `app.run()` (mod.rs:907) vs drive-task DriveOutcome; any Faulted/LinkClosed →
   end scope → drop App → restore TTY (main.rs:804-822 sequence) → exit. RAII: armed terminal-restore guard
   + ChildGuard (unix.rs:360). Teardown order: end-App-scope → drop App → cancel/await reader → drop
   senders → await/abort writer → kill/wait child → restore; drain ssh stderr; restore-on-panic.
7. Tests (headless): materialize→cancel no orphan; empty-mount abort; persistence-unchanged; eager-open
   bounded (many panes, max replay+live); every mutation family forbidden; Faulted→restore+exit.
8. b3 (separate brick): flip unix.rs:471 Federated arm to run_federated_session; remove dead_code allows.

## Unresolved / to verify before coding
- Q1: Does `schedule_session_save` (session.rs:15) already early-return under Disabled? If yes, creation.rs:683
  needs no change; if no, gate it (only remaining C3 write not confirmed gated). **Read session.rs:15-105.**
- Q2: `App::new_federated` construction shape — wrap-and-override (like new_from_handoff mod.rs:786-810) vs a
  shared private core ctor. Wrap is lower-churn; confirm no field reads happen between new() and the
  persistence override.
- Q3: Federated `ensure_default_workspace` no-op mechanism — a mode flag on App vs guarding each of the 4
  call sites (runtime.rs:81/88, headless.rs:589/3219). A single federated-mode bool read is DRYest.
- Q4: Where run_federated_session lives (remote/unix.rs owns run_remote, but the drive loop is async on the
  main rt) — confirm it can `rt.block_on` like main.rs:773 or reuse run_remote's context.
- Q5: spec's numeric anchors (navigate.rs:769 etc., client.rs:341, TunnelTasks) are stale vs current tree —
  confirm the guard choke set with a fresh grep at implementation time (mapped above).
