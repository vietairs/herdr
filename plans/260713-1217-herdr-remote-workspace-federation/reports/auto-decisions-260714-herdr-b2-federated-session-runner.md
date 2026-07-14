# Auto-Decisions — cortex `--auto` run, herdr federation b2

**RISK: HIGH — ran unattended.** R7 route (SSH transport + session/persistence model, high blast
radius). User passed `--auto` on `/hvn:cortex continue` → accepted unattended risk; all stop-and-ask
gates skipped, decisions auto-adjudicated with conservative bias and logged here.

Plan dir: `plans/260713-1217-herdr-remote-workspace-federation/`
Branch: `feat/remote-workspace-federation` (draft PR #1). Build/test remote-only on gpu-ml
(appn-ltu-vm-100); macOS 27 cannot build locally.

---

### D0 — Execution engine: controller-driven remote build, NOT a fire-and-forget Workflow
- **What:** Under `--auto` cortex prescribes ONE dynamic Workflow. I am instead driving the
  rsync→gpu-ml→`nix develop -c cargo …` loop from the controller (background jobs), using subagents
  only for stall-free local drafting/scouting.
- **Why:** The prior b1 agent CRASHED by stalling 600s on exactly this multi-minute remote build
  (watchdog + API ConnectionRefused). A Workflow/subagent that shells the remote nix build would hit
  the same watchdog. The controller can background the build and poll without a watchdog.
- **Risk:** Low — same verification rigor (build+test+clippy green before commit); only the
  orchestration surface changes.
- **Alternatives rejected:** pure Workflow engine (stall-prone on remote build).
- **Reversibility:** N/A (process choice).

### D1 — b2 scope this turn: ship b2.1 (persistence policy) only, then checkpoint
- **What:** b2 is multi-week. This turn advances it by its first dormant brick, b2.1
  `SessionPersistencePolicy::Disabled`, verified green + committed. b2.2/b2.3/b3 remain for resume.
- **Why:** Faithful to the plan's brick-by-brick "BUILD NOW" (v5 codex SOUND-WITH-CHANGES). Trying to
  land all of b2 in one unattended session is not feasible on the remote-only loop.
- **Risk:** Low — b2.1 is dormant (no federated App constructed until b2.3/b3); classic path unchanged.
- **Reversibility:** HIGH — dormant, additive.

### D2 — Additive policy, NOT replace-no_session (diverged from scout Option A)
- **What:** `SessionPersistencePolicy` is an independent immutable field OR-guarding the persistence
  write sites, alongside `no_session`; `no_session` is not retired.
- **Why:** `no_session` also gates update-check + plugin-registry + detach_exits and is live-mutated
  (headless.rs:1193). Replacing it would drag unrelated concerns into a "persistence" policy and
  break the immutability requirement. Additive keeps classic byte-for-byte and preserves the live flip.
- **Risk:** Low — default Enabled = zero classic behavior change (proven: full suite green + existing
  save-scheduling test still passes).
- **Alternatives rejected:** scout Option A (swap the bool) — scope creep + immutability conflict.
- **Reversibility:** HIGH.

### D3 — b2.1 gates WRITE/CLEAR sites only; restore deferred to b2.3
- **What:** b2.1 policy-gates the 5 write/clear paths, not the restore branch.
- **Why:** The spec's C3 clobber risk is the WRITES; restore is a read that cannot mutate the saved
  snapshot. Restore is at construction (before `self`), so honoring Disabled there needs a federated
  constructor (`App::new_federated`) — that lands in b2.3 with the rest of the construction wiring.
- **Risk:** Low — worst case a future federated App shows stale local data until b2.3; no clobber.
- **Reversibility:** HIGH.

### RESULT — b2.1 shipped green
7dc71ec. gpu-ml: build OK, full suite EXIT_0 (all-zero-failed), clippy 0-new, 2 new tests pass.
b2.2/b2.3/b3 remain — checkpointed, pipeline resumable.

### D4 — Continued to b2.2 (bonus), then CHECKPOINT before b2.3 keystone
- **What:** After b2.1, continued autonomously and shipped b2.2 (mutation allowlist) green. Then
  STOPPED the --auto run before b2.3 rather than attempting the integration keystone unattended.
- **Why:** b2.3 wires everything LIVE (b2.1 policy + b2.2 allowlist + b1 tunnel + supervision +
  teardown across many app/ files) — the highest-blast-radius brick. --auto skips ASK gates but does
  not obligate rushing the keystone late in a long session; a deliberate checkpoint before the
  riskiest integration is the conservative call ("insurance proportional to risk"). All prior bricks
  are dormant + verified, so the branch is safe to leave here.
- **Risk:** None added — nothing new wired; checkpoint only.
- **Reversibility:** N/A.

### RESULT — b2.2 shipped green
dd3cf57. gpu-ml: build OK (exhaustive match compiles), full suite EXIT_0 all-zero-failed (2684),
clippy 0-new, 2 tests pass. b2 bricks remaining: b2.3 (keystone) → b3 → R7 tail.

---

## RESUME — cortex `--auto` continue: b2.3 keystone → b3 → R7 tail

### D5 — b2.3 authored by a local-only Opus implementer; controller owns the remote build/verify loop
- **What:** The entire keystone (4 pieces: `App::new_federated`+`federated_mode`+`ensure_default_workspace`
  no-op / two-funnel mutation allowlist guard + `federated_forbidden_response` / process-global
  `FEDERATED_SESSION_ACTIVE` PTY backstop / `run_federated_session` in new `session.rs`) was drafted by a
  background Opus general-purpose agent doing LOCAL Write/Edit only — no build. The controller (main loop)
  owns rsync→gpu-ml→build/test and would relay errors back via SendMessage.
- **Why:** Subagents cannot call `AskUserQuestion`, so the auto-mode classifier's DENY on a shared-host
  remote write (seen earlier this run) would silently STALL a subagent that shells ssh. Keeping all remote
  writes in the controller (which the user already authorized) avoids that deadlock; the implementer stays
  stall-free on pure local edits.
- **Risk:** Low — same verification rigor applies at the controller (build+test+clippy green before commit).
- **Reversibility:** N/A (process choice).

### D6 — Defer runner/guard INTEGRATION tests to post-b3 (when the live path can be exercised)
- **What:** b2.3's plan line named "then headless TESTS" as a follow-up piece. Shipping b2.3 with NO new
  tests this brick; the security-relevant allowlist (`federated_session_allows`) already has its exhaustive
  81-variant test from b2.2, and `federated_forbidden_response` is a trivial serializer.
- **Why:** The whole `session` module is DORMANT — nothing constructs a federated `App` or drives the
  mount channel until b3 flips `run_remote`. A meaningful integration test needs a live tunnel + remote
  server, which only exists once the path is activated. Writing an isolated App-construction unit test now
  would fight the construction helpers for marginal coverage the b3 live attach will supersede. Matches the
  "additive, wired at the flip point" precedent (b0/b1/P9).
- **Risk:** Low-Medium — the guard's WIRING is compiler-enforced (both funnels call it; exhaustive match has
  no wildcard); only the runtime rejection is unverified until b3. Conservative bias: revisit at the R7
  code-review/ship-gate — if either flags the coverage gap on a HIGH-risk surface, add the test before merge.
- **Reversibility:** HIGH — tests are additive; nothing shipped depends on their absence.

### RESULT — b2.3 keystone shipped green
gpu-ml: `cargo build` EXIT_0 (only the fixed `mut mirror` + 2 pre-existing dead-code warnings), full suite
EXIT_0 all-zero-failed (live_handoff/multi_client/server_headless + all bins), clippy 0-new. Dormant additive
code; classic path byte-for-byte unchanged. Committed 65d4388. NEXT = b3 (run_remote Federated arm live flip).

### D7 — b3 live flip: re-dial live in the Federated arm; keep the snapshot mount as the route probe
- **What:** Flipped `run_remote`'s `FederationRoute::Federated` arm from an eprintln+classic-fallthrough onto
  `run_federated_session`. On `Ok` it returns early (session ran + exited to shell, D2); on `Err` (pre-render
  dial/mount/empty failure) it eprintln's and falls through to the classic SshStdioBridge attach. `Config` +
  `config_diagnostic` loaded inside the arm (matches main.rs's `Config::load()`/`config_diagnostic_summary`),
  so the classic route stays byte-for-byte. Removed the now-stale `dormant until b3` allows on
  `run_federated_session` + `App::new_federated`.
- **Why the double dial:** the existing `attempt_federation_mount` snapshot still runs to DECIDE the route;
  `run_federated_session` then dials a FRESH live tunnel. Reusing the one-shot snapshot as a live tunnel would
  fight its consume-and-drop contract and widen `decide_federation_route`'s surface. v1 accepts one extra cheap
  snapshot dial as the viability probe; an optimization (skip snapshot when Federated) is a later cleanup.
- **Risk:** Medium — this ACTIVATES the path. Mitigated: `Err`-before-TTY always falls back to the working
  classic attach; full remote suite green (2684 + all bins, 0 failed); classic route untouched. Residual: the
  live attach itself has no automated coverage (D6) — flagged for R7 code-review/ship-gate.
- **Reversibility:** HIGH — revert one arm to restore the eprintln behavior.

### RESULT — b3 live flip shipped green
gpu-ml: build EXIT_0 (run_federated_session/new_federated no longer warn dead — confirming live wiring), full
suite EXIT_0 all-zero-failed (2684 + 22+14+76+12+9+11+18+11+15). NEXT = R7 tail (impl-notes review →
code-review ‖ codex diff → ship-gate --hard) → un-draft PR #1.

---

## R7 TAIL — reviews + blocker fixes (do NOT un-draft yet)

### D8 — codex adversarial review returned BLOCK (5 findings); fixed 4, documented 1 as v1 scope
Ran `/hvn:impl-notes review` (main loop) + `code-reviewer` agent (inherited Opus) ‖ `codex exec` adversarial
pass on the b2.3+b3 diff (fb1e00f..67c82eb). Codex verdict: **BLOCK — unsafe to land**. All 5 findings verified
against source before acting (review-audit rule: concrete file:line, not abstract). Decision: this is a genuine
BLOCK → do NOT un-draft PR #1; fix the confirmed blockers, re-verify green, re-assess (cortex never merges).

- **F1 CRITICAL — teardown deadlock** (session.rs): `writer_handle.await` ran BEFORE the ssh kill; a half-open
  peer (stopped reading) leaves the writer's `write_all`/`flush` pending forever → await never returns → ssh +
  `TerminalRestoreGuard` never drop → user stranded in alt-screen. FIXED: bound the writer drain with
  `FEDERATION_TEARDOWN_DRAIN_TIMEOUT` (2s) then drop `tunnel_guard` (kill) regardless — clean exit still drains
  promptly, fault case caps at 2s then kills (which also breaks the pending write).
- **F4 HIGH (security) — mutation-guard hole** (app/api.rs): a THIRD funnel `dispatch_deferred_api_request`
  (interactive New Worktree submit) reached `handle_deferred_worktree_api_request` → `create_dir_all` WITHOUT
  a `federated_mode` check, bypassing both guarded entrances; federated workspaces are non-linked so the UI
  admits the action. FIXED: added the same allowlist guard at `dispatch_deferred_api_request` (returns
  `federated_forbidden_response`), closing the default-forbidden hole.
- **F5 HIGH — unbounded probe timeout** (remote/unix.rs `attempt_federation_mount`): the pre-existing snapshot
  probe's `connect_and_mount().await` had no timeout → a peer that accepts SSH but never answers hangs
  `run_remote` before the federated route is reached. FIXED: wrapped in `timeout(CONNECT+MOUNT)`.
- **F2 HIGH — lease race / double-dial** (remote/unix.rs): the probe only `start_kill()`'d (no wait) so the ssh
  child + its server-side single-controller lease could outlive the call; b3's immediate live dial could be
  rejected `Busy` → spurious classic fallback. MITIGATED: added `child.wait().await` after `start_kill` so the
  snapshot tunnel is fully reaped (remote observes the close + releases the lease) before the live dial. Not
  provably eliminated (remote lease-release is async), but the failure mode is a SAFE degrade-to-classic and the
  fresh-dial handshake RTT gives ample release time; a retry-on-Busy is a possible follow-up. This also removes
  the D7 double-dial's practical harm.
- **F3 HIGH (codex) → assessed MEDIUM / v1 SCOPE — stale structural state** (session.rs/client.rs): the App is
  materialized once at mount; the mirror then moves into the drive task, whose EventFrames advance only that
  private mirror (and P4 EventFrames carry no payload, so only a remount reconciles) → remote STRUCTURAL changes
  (new tab/pane, rename) never reach the displayed App. NOT fixed this run: pane terminal OUTPUT does flow live
  (router → pane receivers); only mount-time structure is frozen. Propagating structural deltas mirror→App is
  P9.3 lifecycle scope, not the b3 flip. DOCUMENTED as a known v1 limitation (below) for the human's merge
  decision — neither silently shipped as "full live sync" nor silently expanded into a new phase under --auto.

No defect in the router receiver handoff (receivers exist before Opens are queued / drive starts), the AtomicBool
ordering, or new_federated's Disabled-persistence isolation — codex explicitly cleared these.

FIX VERIFY (partial — full suite pending code-reviewer fold): gpu-ml build EXIT_0, no new warnings, 4 fixes
compile clean. NEXT: fold code-reviewer agent findings → full suite → commit → re-assess merge-readiness.
