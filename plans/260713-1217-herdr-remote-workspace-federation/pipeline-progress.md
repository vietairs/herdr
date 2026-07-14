- [x] 1. /hvn:blindspot --deep — done
- [x] 2. /ck:brainstorm --html — done
- [x] GATE — user chose Tier-2 true federation
- [x] 3. /ck:scenario — done — reports/scenario-federation-edge-cases.md
- [x] 3b. arch probe TerminalRuntime seam — done — MEDIUM seam
- [x] 4. /ck:plan --tdd v1 — done (SUPERSEDED — unbuildable per adversarial gates) — plan.md + phase-01..08
- [x] 5. red-team plan — done — reports/redteam-federation-plan-review.md (F1-F8; F1/F2 blockers)
- [x] 6. codex adversarial plan review — done — 2-protocol conflation + missing remote-side bridge (Critical 1-3)
- [x] PLAN-GATE — user chose: build full two-server system, our herdr on both ends (remote = headless federation server)
- [ ] 4b. /ck:plan REVISION (fold findings + both-ends federation protocol) — in progress
- [ ] 7. /hvn:impl-notes init — pending
- [~] 8. /ck:cook — UNBLOCKED (nix on appn-ltu-vm-100/gpu-ml; `nix develop` builds+tests)
    - [x] P1 federation protocol + id-fencing — VALIDATED 16/16 tests green (commit d619c20; codec bincode→serde_json fix)
    - [x] P2 TerminalSource seam — VALIDATED full suite green (2569+integration, 0 failed; clippy clean; commits 4e43ffd, 50fc3ef)
    - [x] P3 federation-serve remote server — VALIDATED suite green (2768/2768; clippy clean; commit 077c627). CARRIED GAP #1: AppFederationHost does not drain real App AppEvent channel → live relay not end-to-end vs real App; loopback substrate (P4-P9 consume) complete+green; MUST close before P8 default-flip
    - [x] P4 federation client + replica reducer — VALIDATED suite green (2778/2778; no new clippy; commit 86453b4). CARRIED GAP #2: P1 EventFrame wire format has NO entity payload → per-event EventHub::push infeasible; impl = full-snapshot reconcile-by-diff on mount/gap-reset only (no incremental streaming). Affects P6 (event relay) + P8. Needs design review before P6/P9.
    - [x] P5 remote-backed panes (raw byte tee → TerminalSource) — VALIDATED suite green (2793/2793; no new clippy; commit 8929dca). Additive/dormant until P8. RT-F7 clipboard local-policy + agent-status suppression deferred to P7/P6 as planned.
    - [x] P6 agent-status relay — VALIDATED suite green (2801/0; clippy clean vs baseline; commit bc91b43). Relay + remote-probe-suppression tested in isolation; live call site is P8/P9 scope.
    - [x] P7 security hardening (untrusted remote data) — VALIDATED suite green (2810/0; clippy clean; commits 4636fe9 impl + 05ab278 deadlock-fix). sanitize.rs OSC/ANSI strip at reducer ingest, bounded clipboard channel (try_send), RT-F7 clipboard policy parity, adversarial tests. FIX: oversized_clipboard_frame test sized duplex off raw payload not JSON-encoded (2x) frame → write_frame blocked; test-only fix, security intact.
    - [x] P8 --remote federated CLI path + sidebar host labeling + capability fallback — VALIDATED suite green (2829/0; no new clippy; commit c146630). Flag/negotiation/real SSH dial+mount/fallback/single-mount-guard/unspoofable RT-F8 badge all live+tested. TWO PLAN-GAP DEFERRALS (both need files outside any phase's ownership): (a) GAP #1 real App AppEvent drain in serve.rs → no live events from real host; (b) MATERIALIZATION: mount RemoteMirror → rendered Workspace/Tab/Pane via app/creation.rs+spawn_remote → currently mounts then FALLS BACK to classic attach, no visible remote panes. Feature not end-to-end until (a)+(b) wired. Manual two-machine smoke still needed for real SSH path.
    - [~] P9 lifecycle — EXPANDED per user decision, split into 3 priorities:
        - [x] P9.1 GAP #1 event drain (serve.rs drains real App AppEvent→federation stream) — VALIDATED 2830/0; commits 8d43034/3f60b92/31458b3
        - [x] P9.2a materialization MECHANISM (App::materialize_federation_mount + build_remote_pane via spawn_remote, reuses existing creation/event primitives) — VALIDATED 2835/0; commits dfafd76/6110d05/a593d59/70555f2/41f07ef. Tested against loopback.
        - [~] P9.2b materialization CALL-SITE wiring — DECISION MADE 260714: option (a) JSON-API-in-server.
          Rejected (b) own-in-proc-session (partial-goal, attach-like). DESIGN VERIFIED + LOCKED (2 deep
          seam scouts): plan = phase-09b-materialization-call-site-option-a.md, 3 additive/dormant slices:
            P9.2b-1 server-owned FederationTunnel registry (dedicated tokio-runtime thread; owns un-killed
                    ssh child + spawn_mount_writer pump + newly-spawned drive_mount_channel read-loop +
                    Arc<Mutex> shared router/mirror; Drop teardown per SshStdioBridge; clipboard type fix)
                    + extract perform_federation_mount from unix.rs::attempt_federation_mount — NOT STARTED
            P9.2b-2 FederationMaterialize Method + ResponseResult + deferred handler (worktree-deferred
                    precedent app/api.rs:41) — NOT STARTED
            P9.2b-3 live flip: remote/unix.rs FederationRoute::Federated → ApiClient::local() subscribe+call
                    (replaces classic-attach fallback; error path keeps fallback) — NOT STARTED
          Key findings logged to implementation-notes.md (connect_and_mount one-shot; App has no tokio
          handle; RemoteMirror not serde → re-mount in server). Build/test remote-only (nix host
          appn-ltu-vm-100/gpu-ml).
          REVIEW-FIRST (codex gpt-5.6-sol xhigh, "review first"): VERDICT = UNSOUND (viable direction,
          not buildable as written) — 5 CRITICAL + 6 MAJOR. Full report:
          reports/codex-gpt56sol-adversarial-review-p9b-option-a-materialization.md. Core Arc<Mutex>
          router/mirror design DEADLOCKS (C1); + generation fencing (C2), durable server-side SSH
          recipe (C3, needs product decision), generic deferred dispatcher for both prod paths (C4),
          supervisor teardown + P9.3 FSM as PREREQUISITE for live (C5). phase-09b plan = UNSOUND-
          NEEDS-REVISION (superseded).
          PIVOT 260714: user chose OPTION (b) own-in-proc-session (avoids cross-process CRITICALs
          C3/C4/M11; renders federated workspace alone, no local coexistence = narrower v1). (a) file
          marked SUPERSEDED. (b) plan written: phase-09b-option-b-own-in-proc-session.md (precedent
          main.rs:766-843; crux = restructure attempt_federation_mount to keep tunnel alive onto the
          app.run() runtime; C1 dodged via materialize-then-move-router; C2/M7/M9/M10 scoped as v1
          non-goals; 3 slices b1/b2/b3). Feasibility scouted DONE.
          codex gpt-5.6-sol RE-REVIEW of (b) = UNSOUND-but-TRACTABLE (4 CRIT + 5 MAJ + 1 MIN; report
          reports/codex-gpt56sol-adversarial-review-p9b-option-b-inproc-session.md). (b) ARCHITECTURE
          CONFIRMED (escapes (a) C1-deadlock/C4/M11); remaining = bounded correctness checklist. CRUX
          finding CRIT1 = federation-serve boots a DUPLICATE App not the live remote session (pre-
          existing GAP #1; a REMOTE-SIDE decision proxy-vs-durable). DECISIONS 260714: D1 remote=PROXY-live
          (co-locate in HeadlessServer — proxy-live has NO existing wire seam; raw PTY bytes reachable
          only in-process, scout-backed), D2 post-start-fail=EXIT to shell. (b) plan REVISED to v2
          (phase-09b-option-b-own-in-proc-session.md) folding both + all 10 findings + remote scout:
          4 slices b0 REMOTE co-location (biggest, needs sub-scout) / b1 tunnel keep-alive+single-dial
          / b2 session runner (federated mode, supervision, close/gap/overflow, clipboard sinks,
          teardown) / b3 live flip + --session fix. Scope now TWO-SIDED (remote+local), multi-week.
          codex gpt-5.6-sol RE-REVIEW of v2 = UNSOUND (direction VINDICATED, design not yet
          executable; 2 CRIT + 6 MAJ; report reports/codex-gpt56sol-adversarial-review-p9b-option-b
          -v2-colocation.md). TWO KILLERS CLEARED: CRIT2 supervision (outer select! around app.run())
          IS feasible (app !Send irrelevant when block_on-driven inline); co-location DOES satisfy
          disconnects-never-kills (HeadlessServer owns PTYs, federation = broadcast subscriber not
          ownership). REMAINING v2 BLOCKERS: (C1) no legal live-App sharing — need a bounded async
          FederationCommand ACTOR SEAM driven from the headless loop, NOT direct event_rx drain (App
          !Send, &mut self across whole loop, steals events + bypasses forwarding handler); (C2)
          federation socket needs first-class server-owned lifecycle (handoff/rollback/readiness) +
          DELETE the duplicate-App legacy-boot fallback (reverses D1). 6 MAJ: proxy uses wrong fn
          (ensure_remote_server_RUNNING not _ready) + needs timeouts; close/lag/overflow = FAIL-FAST
          typed fatal outcome (no reopen protocol exists); federated mode misses 6 creation bypasses
          → need SessionKind::Federated central policy; single-controller lease undefined; teardown
          RAII; D2 vs acceptance-criterion-4 conflict (defer AC4 to P9.3). Slices: b0 BLOCKED (actor
          seam+socket lifecycle) / b1 conditionally buildable / b2 blocked / b3 not ready. Start b0.
          260714 user chose v3-design-pass-then-re-review. v3 WRITTEN
          (phase-09b-option-b-own-in-proc-session.md): 3 decisions adopted from codex defaults (D2
          intermediate + AC4→P9.3; lazy-start via ensure_remote_server_RUNNING, legacy boot deleted;
          single-controller v1). CRIT1 UNBLOCKED by seam scout — the actor seam ALREADY EXISTS: extend
          ServerEvent with Federation(cmd,oneshot), arms in handle_server_event (headless.rs:2473, the
          sole &mut self dispatch) call live-App accessors, no lock/no theft. b0 restructured: b0.1
          actor variants / b0.2 server-owned federation socket + handoff integration / b0.3 fail-fast
          TunnelFault signal / b0.4 delete duplicate-App; b0-proxy thin stdio proxy; b1 single-dial+
          timeouts+RAII; b2 SessionKind::Federated central policy (all 6 bypasses)+supervision+fail-fast
          consume+teardown; b3 flip. v3 codex gpt-5.6-sol re-review: 1st launch BLOCKED by
          workspace spend cap; RETRY SUCCEEDED. VERDICT = UNSOUND but CRIT1 RESOLVED — codex explicitly:
          "the existing actor seam truly resolves the original live-App borrow/lock problem"; co-location
          architecture VALIDATED. Report reports/codex-gpt56sol-adversarial-review-p9b-option-b-v3-actor
          -seam.md (2 CRIT + 5 MAJ + 1 MIN, all concrete correctness checklist, NOT dead-ends). New CRITs:
          (C1) handoff can't safely revoke actor-blocked controllers → need connid-tagged commands +
          Free/Reserved/Mounted FSM + cancellation/completion + bounded non-actor-joining wait; (C2)
          typed fail-fast can't route through the SAME bounded data queue → need SEPARATE control/
          cancellation lane + versioned wire fault + retained TunnelTasks→TunnelExit + bounded queues
          both dirs. 5 MAJ: adapter needs async-RPC/explicit-thread topology (one thread can't block-read
          AND pump output); federation socket needs FULL public-socket lifecycle not just handoff (don't
          SCM_RIGHTS-transfer it, fresh listener + no controller on replacement); admission needs actor
          AcquireController-before-Accept + per-controller opened-terminals; central policy misses MORE
          bypasses (custom cmds/overlays/respawn/agent-resume/pane.move) + must be construction-time
          (App::new_federated, restoration disabled, NOT post-construction SessionKind); socket naming
          must inherit override precedence. Buildability (codex): b0.1 actor prototype BUILDABLE after
          connid/admission + forwarding-aware sequence; b0.2/b0.3/b2 blocked; b1 plausible. 4th UNSOUND
          but findings converging to bounded engineering. 260714 user chose v4-fold+one-more-review.
          v4 WRITTEN: folds all 7 v3 findings + resolves v3's 4 open Qs with conservative defaults (D4:
          forbidden-mutation BROAD; fault=local-typed-TunnelExit + remote-reason-best-effort; terminal
          Close=whole-mount-fatal v1; unix-only v1). b0 restructured b0.1 actor seam+forwarding-aware+
          thread-topology / b0.2 lease FSM (connid Free/Reserved/Mounted)+handoff revocation-without-
          actor-join / b0.3 fault-control-lane+versioned-wire-fault+TunnelExit / b0.4 full socket
          lifecycle+delete-dup-App; b2 App::new_federated construction-time + exhaustive mutation guard.
          v4 codex gpt-5.6-sol review DONE = UNSOUND (5th consecutive). Report
          reports/codex-gpt56sol-adversarial-review-p9b-option-b-v4-lease-fault-persistence.md (4 CRIT +
          5 MAJ + 2 MIN). Architecture STILL validated ("D1 co-location does not need reversal") but v4
          introduced 2 NEW CRITs by specifying more: (C3) App::new_federated can CLOBBER the classic
          local session (no_session controls persistence too, not just restore) → need
          SessionPersistencePolicy::Disabled; (C4) deleting AppFederationHost removes the only
          ServerInstanceId owner → handshake/mount identity + AC3 restart-fencing lost → HeadlessServer
          must own+rotate it. C1 lease revocation still NOT linearizable (stale queued Acquire/Mount
          resurrect authority post-rollback → need accept_epoch on every command + close-admission-
          before-revoke); C2 fault still can't guarantee EOF when the sole writer blocks mid-frame →
          need per-conn supervisor with independent shutdown(Both). 5 MAJ: contradictory task graph
          (need 1 supervisor+1 serializer+poller); eager-open vs bounded-queue startup overflow; D4
          mutation policy needs CLOSED ALLOWLIST not creator-list; socket unlink outcomes untyped +
          path-length/collision unsafe; proxy can't be transparent AND handshake (client owns handshake).
          Codex: "NOT small implementation-time folds ... a focused v5 design round is warranted."
          META: 5 UNSOUND, architecture validated since v2, but each round specifying more reveals more
          correctness surface — CRIT count went 2→2→4. This is a genuine multi-week live-daemon systems
          change. NEXT = (user decision) v5 targeted fold+review (codex-recommended, but grind continues)
          OR pause/checkpoint (design record preserved, PR #1 stands) OR build b0.1-minus-gaps blind
          (codex says not buildable as written — higher risk). NOT building until user chooses.
          260714 user chose v5-fold+one-more-review. v5 WRITTEN (folds all 11 v4 findings; adopts D5:
          ServerInstanceId fresh-per-boot+rotated, federation-serve transparent-only; MIN11 closed — AC4
          P9.2b exception recorded plan.md:143-146; D4 now CLOSED ALLOWLIST). b0.1 identity+supervisor /
          b0.2 linearized lease (accept_epoch on every cmd) / b0.3 EOF-safe fault supervisor / b0.4 typed
          unlink+hash-safe socket / b2 SessionPersistencePolicy::Disabled+eager-open-ordering+allowlist.
          v5 codex review DONE = SOUND-WITH-CHANGES, **BUILD NOW** (codex verbatim: "No CRITICAL
          architecture blocker remains ... small enough to fold into b0.2/b0.3/b2 without a sixth design
          round ... Unresolved questions: none requiring a product or architecture decision"). Report
          reports/codex-gpt56sol-adversarial-review-p9b-option-b-v5-build-now.md. All 5 load-bearing
          claims YES (linearization/EOF/persistence/allowlist/no-races). 0 CRIT; 5 MAJ + 3 MIN =
          fold-into-slice-tests constraints: per-iteration actor drain BUDGET (else handoff starves);
          byte permits BEFORE frame encode (chunk replay/output); ONE exhaustive Method classifier at
          BOTH sync+deferred dispatch entrances; local-spawn PERMIT before spawn_with_portable_pty +
          reject detached custom cmds; gate clear_history() (app/mod.rs:1432) on persistence policy;
          monotonic epoch never-restored-on-rollback; partial-header-EOF precision; eager-open split.
          Build order (codex-endorsed): b0.1 (buildable+DORMANT, no listener until b0.4) → b0.2 → b0.3 →
          b0.4 → b0-proxy → b1 → b2 → b3; land protocol-version bump with first wire-shape change.
          DESIGN CONVERGED after 5 rounds. 260714 user greenlit BUILD b0.1 (commit on PR-1 branch).
          BUILDING STARTED — remote loop PROVEN (edit→rsync→ssh appn-ltu-vm-100 nix develop cargo test→
          commit→push). Design record committed 0ddea08. b0.1 FIRST BRICK GREEN+SHIPPED dd7335c:
          ServerInstanceId::fresh() + HeadlessServer per-boot identity + replacement rotation (v5 C4
          foundation); 8 id tests pass, 2640 unaffected, clean compile. Dormant (no listener until b0.4).
          b0 PURE-PRIMITIVE FOUNDATION SHIPPED (4 green bricks on PR-1, all dormant #[allow(dead_code)],
          production unchanged): dd7335c per-boot federation identity + replacement rotation / ee3804a
          actor seam (ServerEvent::Federation + FederationCommand + dispatch on live App via forwarding-
          aware handle_api_request_after_internal_events_drained) / abc6a35 single-controller lease FSM
          (accept_epoch linearization, compare-and-clear release, resurrection-hole test) / b27ff1d typed
          TunnelExit + FirstCauseCell (first-fault-wins). Remote loop humming (rsync→nix cargo test→push).
          b0–b0.4 PRIMITIVES ALL SHIPPED (6 green bricks on PR-1, dormant, production unchanged): identity
          dd7335c / actor seam ee3804a / lease FSM abc6a35 / fault b27ff1d / socket-path 6dbea3d / typed-
          unlink 1f733f9. ~28 new unit tests, all green; every brick compiled the full binary; remote loop
          humming.
          REMAINING = live-I/O INTEGRATION (the keystone + tails, need fresh focused context):
          (1) b0.4 KEYSTONE — IN PROGRESS (spec: phase-09b-b04-accept-loop-keystone.md, from 3 parallel
              scouts). CRUX RESOLVED 260714 = decision B (SYNC thread topology, NOT the old decision-A
              async lean) — codex is pure + LocalStream try_clones into halves + the client transport
              already drives connections this exact way; recorded b049b15.
              SUB-BRICK 1 DONE+SHIPPED d0f166f (hvn-implementer): federation socket BOUND + full lifecycle;
              first LIVE change; FULL SUITE 2669/0.
              SUB-BRICK 2a DONE+SHIPPED d58064d: lease↔actor integration — dispatch(&mut App, &mut
              FederationLease, cmd); FederationCommand gains AcquireController/Mount{epoch,connid}/Release +
              mounted-controller authz on SendInput/Resize; HeadlessServer owns federation_lease. Dormant.
              6 actor tests; FULL suite 2673/0.
              SUB-BRICK 2b DONE+SHIPPED a1eb97f — FIRST FEDERATION ACCEPT: new server::federation_accept
              (accept loop + sync framing + drive_handshake); HeadlessServer mints next_federation_id +
              accept_federation_connections() each tick (handoff drains). Closes after handshake (mount is
              2c). 3 handshake tests; FULL suite 2676/0, 0 warnings.
              NEXT sub-brick 2c (THE BIG ONE): after Accept → AcquireController→Mount via server_event_tx +
              oneshot::blocking_recv → MountSnapshot; then command loop (reader thread → SendInput/Resize;
              writer thread draining mpsc<FederationMessage>; output-pump threads on broadcast::Receiver::
              blocking_recv w/ mount-gen fencing; event/agent ticker) + first-cause supervisor + guaranteed
              lease Release on every EOF/fault exit. Suggest splitting 2c-1 (mount-on-accept + Release) /
              2c-2 (reader command loop + writer) / 2c-3 (output pumps + tickers + supervisor).
              Sub-brick 3: live handoff revocation (lease.begin_revocation wired into perform_live_handoff).
              Sub-brick 4: DELETE AppFederationHost.
          (2) b0.3-tail: wire-fault FederationMessage variant + Channel::Control + PROTOCOL VERSION bump 1→2
              + bounded egress + inbound-Fault→TunnelExit (ripples into serve/client/loopback/pane_source/codec).
          (3) b0-proxy transparent stdio; b1 tunnel keep-alive (remote/unix.rs); b2 App::new_federated +
              SessionPersistencePolicy::Disabled + closed-allowlist (many app/ files) + eager-open + teardown;
              b3 run_remote flip.
          Then R7 tail (impl-notes review → code-review‖codex diff → ship-gate --hard) before un-drafting PR #1.
        - [ ] P9.3 lifecycle FSM (reconnect/re-fence/cold-resume/warm-handoff exclusion/shutdown-never-kills) — pending; depends on P9.2b real-session wiring
- [ ] 9. /hvn:impl-notes review — pending
- [ ] 10. /ck:code-review ‖ 11. /codex:adversarial-review <diff> — pending
- [ ] 12. /hvn:ship-gate --hard — pending
