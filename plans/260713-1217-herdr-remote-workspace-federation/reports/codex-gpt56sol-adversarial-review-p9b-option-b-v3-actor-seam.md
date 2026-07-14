# Codex gpt-5.6-sol adversarial review — P9.2b option (b) v3 (actor seam)

Reviewer: codex-cli, `gpt-5.6-sol`, xhigh, read-only, workdir `~/Projects/herdr`. Date 260714.
Target: `phase-09b-option-b-own-in-proc-session.md` (v3). Verified against source. No files changed.
(First launch hit the workspace spend cap; this is the successful retry.)

## Verdict: UNSOUND — but CRIT1 RESOLVED, architecture validated

Codex EXPLICITLY confirms the deepest unknown is closed: **"Yes: the existing actor seam truly
resolves the original live-`App` borrow/lock problem."** `HeadlessServer` owns `App` by value
(headless.rs:192); the main loop dispatches ServerEvents sequentially via `handle_server_event`
(headless.rs:565,2473) — the single ingress for thread-originated work. An ordinary federation request
mid-drain does NOT deadlock. Returning `broadcast::Receiver<Bytes>` through the oneshot is
ownership-sound (runtime.rs:494 returns by value, no App borrow; drop disconnects only that subscriber,
not the PTY). Co-location is viable. Remaining = a concrete correctness checklist (2 CRIT + 5 MAJ +
1 MIN), NOT architectural dead-ends.

Deadlock appears ONLY if (a) the actor awaits an AppEvent before replying, or (b) handoff synchronously
joins a connection thread waiting on this actor — both are finding-1 concerns, both avoidable.

## Findings

1. **[CRITICAL] Handoff cannot safely revoke actor-blocked controllers.** Federation threads wait on
   actor replies (phase:86); handoff runs synchronously INSIDE that same actor (headless.rs:2805);
   queued server events can't resume until it returns (headless.rs:1387); unlinking a socket does NOT
   close accepted streams (ipc.rs:246); rollback re-enables dispatch (headless.rs:1161). "Terminate
   controllers" has no cancellation handle / stream shutdown / completion barrier / stale-command
   authz. Synchronously joining a thread waiting on this actor DEADLOCKS; not joining lets revoked
   input/resize run after rollback; a late EOF can release a newer lease. **Fix:** every command
   carries `connid`; `Free | Reserved(connid) | Mounted(connid)` state + per-conn cancellation/
   completion; handoff revokes authority, closes accepted streams, cancels pending replies, waits
   boundedly WITHOUT actor-dependent joins; authorize every mutating command against the active connid;
   release = compare-and-clear.

2. **[CRITICAL] Typed fail-fast is NOT deliverable through the proposed bounded queue.** No fault
   message in the protocol (protocol/mod.rs:215); server egress unbounded (serve.rs:233); lag silently
   resumed (serve.rs:424,440); client writer errors vanish into `JoinHandle<()>` (client.rs:444); local
   demux overflow ignored (client.rs:341). Once the DATA queue is full (or the writer blocks), the
   overflow fault CANNOT be sent through that same queue → b2 has no guaranteed signal to win its
   supervisor `select!`. Client→server egress also unbounded. **Fix:** versioned wire fault + a SEPARATE
   first-fault-wins control/cancellation lane + ONE exclusive writer + bounded queues BOTH directions +
   byte-and-message budgets + retained `TunnelTasks` whose reader/writer/child/panic results all produce
   a typed `TunnelExit`. Exact remote overflow reason may be best-effort; EOF stays mandatory.

3. **[MAJOR] Actor seam valid, but the adapter is underspecified and can violate event forwarding.**
   Federation hosting is a SYNC trait used from an async controller (serve.rs:46,177); Headless requires
   ALL internal events through its forwarding handler (headless.rs:1836); correct API sequence =
   forwarding-aware drain → `handle_api_request_after_internal_events_drained` (headless.rs:2848);
   generic `App::handle_api_request` does its OWN non-forwarding drain (api.rs:829). "Call the same
   json_request accessor" is UNSAFE if literal; one blocking thread can't both block on protocol reads
   AND pump broadcast output. **Fix:** async actor RPC adapter OR explicit controller/writer/forwarder
   thread topology; inside the actor, mount/API = forwarding-aware drain → after-drained request →
   cursor, WITHOUT awaiting an AppEvent before replying.

4. **[MAJOR] The second socket is not covered by the FULL public-socket lifecycle.** Replacement
   waiting checks only API/client sockets (headless.rs:4110); rollback restores only those (:1132);
   shutdown/Drop likewise (:3798); session stop waits only API/client (session.rs:236). Adding steps
   only to `perform_live_handoff` leaves normal shutdown / replacement startup / stop completion /
   pre-vs-post-unlink rollback inconsistent. **Fix:** enumerate federation-socket ownership across
   construction, accept-cancellation, post-unlink restoration, replacement readiness, cleanup, Drop,
   stop-wait; preserve original listener on pre-unlink failure, rebind only after actual unlink.
   NOTE: do NOT SCM_RIGHTS-transfer the federation listener/streams (manifest transfers only pane
   runtimes, handoff.rs:33); replacement binds a FRESH listener, starts with NO controller.
   `wait_owned_ack` stays a pane-runtime ack, not a federation-socket ack.

5. **[MAJOR] Admission & Open ordering lack actor-level state transitions.** Protocol can reject only
   version mismatch (protocol/mod.rs:53); server sends `Accept` BEFORE mounting (serve.rs:201);
   proposed actor commands have no acquire/release (phase:70). Simultaneous handshakes can't reserve
   the lease atomically; discovering occupancy at Mount is too late for handshake rejection;
   `Option<connid>` doesn't track opened terminal IDs. **Fix:** actor-mediated `AcquireController(connid)`
   BEFORE `Accept`, typed `Busy`, atomic promotion on successful Mount, RAII reservation release,
   per-controller opened-terminal state enforcing Open before input/resize.

6. **[MAJOR] Central federated-session policy misses production bypasses.** Navigation mostly converges
   through runtime API calls (navigate.rs:179) so API classifiers ARE useful chokes — BUT direct local
   spawns remain in custom commands + scrollback overlays (navigate.rs:769,880,935); auto shell respawn
   bypasses API dispatch (api.rs:450); pending agent resume spawns directly (agent_resume.rs:204);
   `pane.move` creates topology without a PTY (panes.rs:777). v3's 6-site inventory is NOT exhaustive.
   Also setting `SessionKind` AFTER construction is too late — `App::new` can restore local runtimes
   (app/mod.rs:353), run loop can create a default shell (app/mod.rs:1132). **Fix:** immutable
   `App::new_federated` with restoration DISABLED; exhaustive normal/deferred API policy classifier;
   federated no-op for `ensure_default_workspace`; low-level local-PTY creation guard covering custom
   commands, overlays, respawn, agent resume. Keep remote materialization on its distinct `spawn_remote`
   path.

7. **[MAJOR] Federation socket naming doesn't inherit override semantics.** Socket selection includes
   explicit session + env/legacy overrides (socket_paths.rs:14). Deriving the federation socket only
   from the data dir can collide when default-session daemons use different `HERDR_SOCKET_PATH`.
   `ensure_remote_server_running` verifies CLASSIC-daemon readiness, not federation-socket readiness /
   protocol compat. **Fix:** derive `*-federation.sock` from the effective classic socket with the SAME
   precedence, then independently connect/handshake with bounded retries + typed version failure.

8. **[MINOR] Polling actor requests can trigger unnecessary renders / unfair draining.** Every handled
   ServerEvent returning true requests a render (headless.rs:583); server-event drain runs until empty
   (headless.rs:1387). 25ms event + 100ms status polling → continuous full renders; always-fed actor
   queue delays other loop work. **Fix:** read-only polling commands return false; actor drain uses a
   bounded per-iteration budget.

## CRIT1 determination (codex verbatim intent)
YES — the existing actor seam truly resolves the live-`App` borrow/lock problem. `handle_server_event`
is not literally the ONLY place the loop mutates App, but it IS the single ingress for thread-originated
ServerEvent work, which is sufficient. Ownership of the returned broadcast receiver across the oneshot
is sound.

## Buildability (codex)
- **b0.1 actor command/reply prototype:** BUILDABLE after adding connection identity/admission + locking
  the forwarding-aware API sequence.
- **b0.2 socket/handoff:** blocked by cancellation, stale-authority, lifecycle, path rules.
- **b0.3 fail-fast:** blocked by the missing wire/control lane + aggregate task supervisor.
- **b1 proxy:** independently plausible after federation readiness/version behavior specified.
- **b2 own-in-proc session:** blocked by incomplete mutation coverage + construction-time mode.
→ v3 not buildable slice-by-slice AS WRITTEN, though the co-location architecture is viable.

## What v3 got right (verified)
- Found the correct live-`App` authority seam; eliminated duplicate-App boot.
- Forwarding-aware atomic mount is the right ordering.
- Per-subscriber broadcast lag can terminate only the federation connection, not the PTY/other subs.
- `ensure_remote_server_running` + `--session` before the subcommand are correct.
- Server-owned socket/runtime satisfies "client disconnect never kills the remote federation server."
- D2 documented as P9.2b intermediate with AC4 deferred to P9.3 — but parent global AC4 is still
  unqualified at plan.md:141, so the acceptance ledger must record the phase exception too.

## Unresolved questions (need decision for a v4)
- Is "forbidden mutation" limited to local PTY/topology creation, or ALL shared ws/tab/pane mutation?
- Is exact client-visible `EgressOverflow` mandatory when the writer is congested, or is typed/local
  `PeerClosed` acceptable?
- Does an authoritative terminal Close terminate the WHOLE v1 mount, or only that pane?
- Is the initial federation listener Unix-only, or must b0 include the Windows accept-thread lifecycle?
