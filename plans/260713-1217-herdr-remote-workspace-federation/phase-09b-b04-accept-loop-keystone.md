# b0.4 keystone — server-owned federation listener + accept loop + handoff integration

Execution spec synthesized from 3 parallel seam scouts (260714). Wires the six shipped b0 primitives
(identity dd7335c, actor seam ee3804a, lease abc6a35, fault b27ff1d, socket-path 6dbea3d, typed-unlink
1f733f9) into a live co-located federation listener. Build/test remote-only (nix host gpu-ml).

## The crux (design decision to make FIRST)
The existing `serve::run<R,W,H>(host: Arc<H>, reader, writer)` (serve.rs:182) drives a federation
connection, but its `FederationHost` trait methods are SYNC and `AppFederationHost` backs them with a
`Mutex<App>`. For co-location the host must instead reach the LIVE App via the actor seam
(`ServerEvent::Federation(FederationCommand)` → `server_event_tx` → oneshot reply). But the reply is
ASYNC (oneshot) and `serve::run`'s select! loop is async — a sync trait method that blocks on a oneshot
inside the async loop stalls the runtime (codex v5 MAJ3).

**Two viable resolutions (decide before coding the accept handler):**
- (A) A NEW async serve path for the co-located case that `await`s the oneshot directly (does NOT go
  through the sync `FederationHost` trait). Cleanest; duplicates some of serve::run's loop shape.
- (B) A `ServerBackedFederationHost` whose sync trait methods `blocking_recv()` the oneshot — only
  legal if each such call runs on a dedicated blocking thread (not the async reader), i.e. the explicit
  reader/serializer/poller thread topology (codex MAJ5), never inside an async task.
Recommendation: (A) — a purpose-built async connection driver that owns the reader + an exclusive
serializer + the tickers, and bridges each request to the server loop via `server_event_tx` + oneshot.
This also naturally hosts the connection supervisor (first-cause + shutdown(Both), b0.3).

## Accept pattern to MIRROR (client path)
- `accept_pending_client_connections(&LocalListener, &mut next_id, &Arc<AtomicBool>, &mpsc::Sender<ServerEvent>)`
  (client_accept.rs:12-51): nonblocking `listener.accept()` (19) → WouldBlock breaks (42); mint id
  (21-22); `stream.set_nonblocking(true)` (24); clone should_quit + tx (29-30);
  `std::thread::spawn` (31-40) → `handle_client_handshake(stream, id, &tx, &should_quit)` (32-37).
- `handle_client_handshake` (client_transport.rs:429): `set_nonblocking(false)` for the handshake (437);
  `read_message`/`write_message` Hello/Welcome (447,535); build `ClientWriter` (551-555); spawn writer
  thread (558-562); `server_event_tx.blocking_send(ServerEvent::ClientConnected{...,writer})` (565-575);
  enter `client_read_loop` on the same thread (578).
- Main loop calls `self.accept_client_connections()?` (headless.rs:517) each tick →
  `accept_pending_client_connections(&self.client_listener, &mut self.next_client_id, &self.should_quit,
  &self.server_event_tx)` (headless.rs:1385-1395).
- Listener plumbing: `bind_local_listener(path)` (ipc.rs:48) → `LocalListener =
  interprocess::local_socket::Listener` (ipc.rs:10); `listener.set_nonblocking(ListenerNonblockingMode::
  Accept)` (headless.rs:29,383).

## Federation wire handshake the accept handler drives (SERVER side)
Mirror `serve::run` (serve.rs:182-287) but backed by the live App:
1. `read_frame` first msg → `FederationMessage::Handshake` (188-193).
2. Build local `Handshake { federation_protocol_version: FEDERATION_PROTOCOL_VERSION, capabilities:
   host.capabilities(), server_instance_id }` (195-199) — use HeadlessServer's
   `federation_server_instance_id` (shipped dd7335c), NOT a per-connection id.
3. `negotiate(&local, &remote)` (negotiate.rs:20) → `AgreedCaps` or `RejectReason::Version`; on reject
   send `HandshakeResponse::Reject` and return (201-211).
4. Send `HandshakeResponse::Accept { agreed_capabilities }` (212-218).
5. **Lease admission (b0.2):** before mounting, `lease.try_acquire(epoch, connid)` via an actor command;
   `Busy`/`StaleEpoch`/`Closed` → reject the connection (typed Busy). Only `Accepted` proceeds.
6. `Mount` via the actor (`FederationCommand::Mount`) → `(snapshot, cursor)`; promote lease to Mounted;
   write `FederationMessage::MountSnapshot { server_instance_id, snapshot, cursor }` (220-231).
7. select! loop: inbound `read_frame` → route Terminal/control to `FederationCommand::SendInput/Resize`
   (authorize via `lease.is_mounted_controller`); event ticker → `FederationCommand::EventsAfter` →
   emit Event frames; agent ticker → `FederationCommand::AgentStatuses`. Output pane bytes:
   `FederationCommand::SubscribeOutput` returns a `broadcast::Receiver<Bytes>` the exclusive serializer
   pumps. (serve.rs:233-279 shape.)
8. EOF/fault → first-cause (b0.3) → `lease.release(epoch, connid)` (compare-and-clear) + supervisor
   shutdown(Both). (serve.rs:281-287.)
- Codec: `[u32 LE version][u32 LE len][serde_json payload]`; version mismatch → `CodecError::VersionSkew`
  (codec.rs:51,79,86-91). `FederationMessage::channel().max_len()` selects the cap.

## Socket lifecycle wiring (every site — miss none)
New HeadlessServer fields (BOTH constructors — `new()` headless.rs:367 AND the struct literal
headless.rs:4220): `federation_listener: LocalListener`, `federation_socket_path: PathBuf`,
`federation_socket_identity: SocketFileIdentity` (all `#[cfg(unix)]`).
- **Bind** in each constructor, mirroring the client recipe (headless.rs:373-383):
  `let fed_path = federation_socket_path(&client_path);` (shipped 6dbea3d) → `prepare_socket_path` →
  `bind_local_listener` → `restrict_socket_permissions` → `socket_file_identity` →
  `set_nonblocking(Accept)`.
- **Accept**: add `accept_pending_federation_connections(...)` mirroring client_accept.rs; call it from
  the main tick next to `accept_client_connections()` (headless.rs:517). (This is the sub-brick AFTER
  the bind+lifecycle sub-brick.)
- **Handoff** (perform_live_handoff): reject pending federation accepts next to client (963);
  `lease.begin_revocation()` + close accepted federation streams + cancel pending replies; **unlink the
  federation socket between send_fds (1065) and wait_ready (1081)**, i.e. alongside client/api unlink at
  1075-1080, using `remove_socket_file_if_owned_typed` (shipped 1f733f9) to record what was removed;
  restore on the rollback paths (1083-1092, 1100-1108); the replacement server (new() at 4115 / literal
  4220) binds a FRESH federation socket + starts with NO controller (lease default) + rotated
  ServerInstanceId (already done). NOT added to the SCM_RIGHTS manifest (handoff.rs:32-42 = panes only).
- **Normal shutdown/Drop**: add federation unlink to `cleanup_sockets()` (headless.rs:3835-3848; called
  by complete_shutdown:3829 and Drop:3859).
- **Session stop-wait** (session.rs:236-245): add the federation socket path to the wait list so `server
  stop` waits for it to close too.

## Sub-brick order (each: compiles + suite green on nix)
1. **bind + lifecycle** (no accept yet): add fields + bind in both constructors + cleanup_sockets + Drop
   + handoff unlink/rollback + stop-wait. Test: server binds the federation socket + removes it on
   shutdown + handoff unlink is typed. LIVE behavior change (every server now binds the socket) but
   nothing accepts it yet.
2. **accept + async connection driver** (the crux, decision A): accept loop mints connid + registers at
   `lease.current_epoch()`; the async driver does handshake → lease acquire → mount → select! loop
   bridging to the live App via server_event_tx + oneshot; exclusive serializer; first-cause supervisor.
3. **handoff revocation live**: wire `lease.begin_revocation()` + stream close + reply-cancel into
   perform_live_handoff; test revoke-without-deadlock + no-resurrection.
4. **delete AppFederationHost**: once federation-serve is the transparent proxy (b0-proxy), remove the
   duplicate-App boot (serve.rs:470-501) + `run_federation_serve_over_stdio`'s AppFederationHost use.

## Risks
- Sub-brick 1 changes live server startup (binds a new socket) — must nail every cleanup site or leak
  socket files / break handoff. The map above lists all sites.
- Sub-brick 2 is the async-bridge crux — get the sync-vs-async boundary right (decision A) or the render
  loop stalls.
- Full `cargo test` (not scoped) after sub-brick 1 and 3, since they touch the live handoff path.
