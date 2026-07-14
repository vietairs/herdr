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
