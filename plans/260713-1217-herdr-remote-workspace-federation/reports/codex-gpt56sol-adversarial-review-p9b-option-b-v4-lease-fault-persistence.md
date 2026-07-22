# Codex gpt-5.6-sol adversarial review — P9.2b option (b) v4

Reviewer: codex-cli, `gpt-5.6-sol`, xhigh, read-only, workdir `~/Projects/herdr`. Date 260714.
Target: `phase-09b-option-b-own-in-proc-session.md` (v4). Verified against source. No files changed.

## Verdict: UNSOUND — architecture still validated, correctness surface deeper than v4 specifies

v4 preserves the viable co-location architecture and materially improves v3, but does NOT fully resolve
either former CRITICAL, and INTRODUCES two new high-impact ownership gaps (daemon identity, session
persistence) that only became visible once v4 specified deleting AppFederationHost + `App::new_federated`.
Codex: "These are not small implementation-time folds ... A focused v5 design round is warranted; the
D1 co-location architecture itself does not need reversal." 4 CRIT + 5 MAJ + 2 MIN.

Hard answers codex gave:
1. No-actor-join avoids the handoff DEADLOCK, but revocation is NOT linearizable — stale queued
   Acquire/Mount can resurrect authority after rollback.
2. Fault design is constructible with existing broadcast/mpsc, but NOT with a separate queue alone —
   needs a connection supervisor that can interrupt a blocked writer + independently close transport.
3. Forwarding-aware sequence is CORRECT for all listed commands; none must await AppEvent. But the
   always-`false` polling return loses forwarding-drain render causality (MIN10).
4. PTY-creator inventory ~complete, but the BROAD D4 policy is not enforced: destructive/reorder/
   rename/layout methods unspecified.
5. `*-federation.sock` NOT proven collision- or path-length-safe for arbitrary overrides.
6. New gaps: identity ownership, persistence, connection task topology, eager-open ordering, proxy
   handshake ownership.

## Findings

1. **[CRITICAL] Handoff revocation has no linearization point.** v4 authorizes only mutating commands
   vs connid + protects only EOF release (phase:73-84). Handoff occupies the actor synchronously
   (headless.rs:2805-2821); queued commands resume after (headless.rs:1384-1392); Unix accept spawns
   BEFORE actor registration (client_accept.rs:12-40, client_transport.rs:552-568). An accepted conn
   can queue AcquireController BEHIND handoff; closing its stream/reply-rx does NOT remove the command
   from the actor queue. After rollback→Free, a STALE Acquire reserves a dead connid; stale Mount/
   Subscribe/poll can run. **Fix:** close admission BEFORE revocation; register every accepted stream
   before it can enqueue; attach `{accept_epoch, connid}` to EVERY command; validate every command incl.
   Acquire/read-only/Release; ignore commands whose reply-rx is canceled; direct worker-completion
   guards never actor events; add reservation expiry + handshake timeout + pending-conn cap + queued-
   stale tests per command variant.

2. **[CRITICAL] Separate fault queue still can't guarantee mandatory EOF.** Writers block inside
   `write_frame().await` (serve.rs:168-175,233-240; client.rs:444-460). When the peer stops reading,
   the sole writer is stuck mid-frame; a fault queued elsewhere can't reach the wire; canceling midway
   then writing a fault corrupts framing → neither EOF nor bounded TunnelExit guaranteed → b2 supervisor
   may never win. **Fix:** per-connection supervisor owns an atomic first-cause cell + independent
   `shutdown(Both)`/abort. On first fault: retain cause, best-effort prioritize fault frame, boundedly
   terminate writer, force transport EOF; secondary EOF must not overwrite the initiating cause. Bump
   FEDERATION_PROTOCOL_VERSION; test a non-reading peer with every budget saturated.

3. **[CRITICAL] `App::new_federated` can OVERWRITE the classic local session.** v4 says only
   "restoration disabled" (phase:135-150). Existing `no_session` controls BOTH restore (app/mod.rs:366-
   405) AND exit save (app/mod.rs:1107-1110); federation materialization SCHEDULES a session save
   (creation.rs:681-684) whose job can save/clear persistent state (session.rs:39-105). If the new
   constructor suppresses restore but not persistence, remote mirror workspaces can REPLACE/CLEAR the
   user's classic session; a later classic start restores them as local runtimes. **Fix:** immutable
   `SessionPersistencePolicy::Disabled` / `no_session=true` contract covering restore, save-scheduling,
   history, exit-save, clear. Test a pre-seeded classic snapshot is byte-for-byte unchanged after
   successful/empty/failed/faulted federated sessions.

4. **[CRITICAL] b0.1 deletes the only daemon-instance identity owner without replacing it.** Handshake
   + mount require `server_instance_id` (protocol/mod.rs:45-51,76-84; serve.rs:195-229); its production
   owner/generator is `AppFederationHost` (serve.rs:470-500) which v4 DELETES (phase:97-110); yet
   `Mount(connid)` returns only (SessionSnapshot, EventCursor) (phase:53-57). Parent AC3 requires stale
   post-restart traffic fenced (plan.md:139-140). Per-conn generation breaks identity; reuse across
   replacement breaks restart fencing. **Fix:** HeadlessServer owns ONE fresh `ServerInstanceId` per
   process boot, exposed to handshake+mount; replacement startup ROTATES it. Add sequential-controller
   stability + post-handoff rotation tests.

5. **[MAJOR] Per-connection task graph is internally contradictory + incomplete.** v4 assigns the
   reader to "write control replies," a distinct writer pumps output, then demands "one exclusive
   writer" (phase:64-67); serving also needs independent event + agent-status tickers (serve.rs:242-
   278). Two write owners interleave frames; a blocking-reader + output-writer alone leaves nobody
   polling EventsAfter/AgentStatuses → updates stop when the peer sends no inbound. **Fix:** ONE
   connection supervisor = reader + poller/coordinator + per-terminal forwarders + ONE exclusive
   serializer; only the serializer touches AsyncWrite; others enqueue bounded messages.

6. **[MAJOR] b2 eager-open ordering conflicts with the newly bounded queues.** v4 starts the writer,
   synchronously materializes+opens every pane, THEN starts the reader (phase:140-143). Materialization
   emits an Open per pane (creation.rs:689-727), today reliant on an UNBOUNDED outbound queue
   (client.rs:313-325,444-461). Once b0.3 bounds both dirs, many Opens trigger scrollback replay + live
   output while the local reader isn't running → egress overflow fatally closes a healthy mount at
   startup. **Fix:** start read/supervision BEFORE Opens, or split materialization into sync model
   creation + async bounded Open issuance. Test many panes with max replay + live output during mount.

7. **[MAJOR] D4 mutation coverage not closed.** D4 forbids all local topology mutation (phase:24-29)
   but b2 tests stay creator-focused (phase:144-165). API has rename/move/close/swap/zoom/layout/split-
   ratio (schema.rs:66-207; workspaces.rs:90-154,227-259; tabs.rs:140-270; panes.rs:388-598,1085-1136,
   1521-1588). `pane.resize` is ambiguous: API resize mutates LOCAL split geometry vs remote terminal
   resize is a separate transport command. **Fix:** CLOSED ALLOWLIST (read-only queries, presentation/
   navigation, remote input, remote terminal resize); everything else default-forbidden; low-level
   guard at the authoritative local-runtime-creation seam; test every mutation family + default-closed.

8. **[MAJOR] b0.4 can't meet its own socket transaction + naming guarantees.**
   `remove_socket_file_if_owned` returns the same `Ok(())` for removed/absent/not-owner (ipc.rs:246-
   264) → rollback can't know which sockets were relinquished. Effective classic paths can be arbitrary
   legacy overrides (socket_paths.rs:14-66); remote socket code already needs hashed shortening for
   sun_path limits (unix.rs:2197-2231) → `x` vs `x.sock` collide; `-federation` can exceed sun_path.
   Also client_accept.rs has NO cancellable Unix accept task to mirror (actor polls a nonblocking
   listener + spawns handshake threads, client_accept.rs:12-40). **Fix:** typed unlink outcomes + unlink
   bitmap; restore/wait only removed paths; derive from the WHOLE effective path with deterministic
   hash fallback; keep acceptance actor-polled so admission-closure + handoff share one authority; gate
   listener/proxy/CLI/dispatch together on Unix.

9. **[MAJOR] Proxy can't be both transparent AND independently handshaking.** v4 calls it a byte pipe
   yet gives it an independent handshake (phase:116-121). The real client consumes the SOLE handshake
   response + mount (client.rs:130-219); server emits exactly one per connection (serve.rs:188-231). If
   the proxy handshakes it steals the controller + mount; if transparent it can't report a version
   failure. **Fix:** proxy retries only the Unix-socket CONNECTION + pipes bytes; the LOCAL federation
   client exclusively owns handshake, version/busy rejection, mount consumption.

10. **[MINOR] `false` polling return discards forwarding-drain render causality.** Forwarding-aware
    handler ORs drain changes into its result (headless.rs:2848-2855,3067-3071); a polling request can
    consume a state-changing event then suppress the render. **Fix:** distinguish `command_changed=false`
    from `drain_changed`; return their OR.

11. **[MINOR] Promised AC4 exception still absent.** AC4 remains unconditional (plan.md:141-142); AC7
    (plan.md:147) is compatible. **Fix:** mark reconnect/disconnected-state as final P9.3 acceptance +
    explicitly record P9.2b's temporary exit-to-shell exception in the ledger.

## Buildability
NOT buildable slice-by-slice as written. b0.1 viable but needs daemon-identity + exclusive-writer/
poller boundary; b0.2 blocked on admission/revocation linearization + all-command epoch validation;
b0.3 blocked on transport-independent shutdown + first-cause supervision (ownership crosses into
b1/b2); b0.4 needs typed unlink outcomes + length-safe path; b0-proxy needs handshake ownership fixed;
b1 plausible; b2 blocked on persistence policy + bounded eager-open sequencing + default-closed policy.
"These are not small implementation-time folds ... A focused v5 design round is warranted; the D1
co-location architecture itself does not need reversal."

## What v4 got right (verified)
- HeadlessServer = correct sole live-App authority; dup-App deletion directionally correct.
- Forwarding-aware drain → handle_api_request_after_internal_events_drained is the correct sequence
  (headless.rs:2848-2927); listed snapshot/status/event ops are synchronous, need not await AppEvent
  (api/session.rs:7-57, api/agents.rs:12-18, event_hub.rs:28-45).
- Free→Reserved(connid)→Mounted(connid), pre-Accept reservation, Open-before-input, compare-and-clear
  EOF = right baseline.
- Explicit stream shutdown (unlink != close) correct; fresh replacement listener w/ no transferred
  controller correct.
- Restoration-disabled construction, no default shell, two-layer mutation enforcement, bounded actor
  drain, unix-only v1, whole-mount-fatal Close = sound directions once gaps fixed. Whole-mount-fatal
  Close is intentionally harsh but safe for v1 (exits local mount, leaves remote PTYs/server intact).

## Unresolved decisions for v5
- Confirm `ServerInstanceId` rotates on live-handoff replacement; recommended fresh-per-process.
- Confirm `federation-serve` is transparent transport ONLY; recommended local client owns handshake +
  mount exclusively.
