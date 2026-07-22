# Auto-decisions — tui-add-remote-workspace (`--auto` run, 2026-07-22 19:13)

Every gate `--auto` skipped, auto-adjudicated with conservative bias. Append-only.

## 1. Risk tier: medium (not high)
- **What:** Classified R5 (medium) instead of R7 (high), despite the change feeding an `ssh` exec.
- **Why:** Signals conflicted — additive UI surface (low) vs user-supplied string reaching an ssh
  argv (high). Deciding evidence: the target string, its parsing, and the ssh dial ALREADY exist
  and are exercised by the CLI/socket path (`src/remote/unix.rs:125`); this feature adds a
  collector, not a new trust boundary. Not "unsure" → no tier-up required.
- **Risk:** If target validation turns out to be absent today, the dialog widens exposure from
  "user typed JSON at a socket" to "user typed into a dialog" — a small delta, but real.
- **Mitigation:** scout #3 is dedicated to the target trust boundary; `/hvn-security-scan` appended
  as a mandatory pre-ship stage; plan is required to make validation an explicit tested concern.
- **Alternatives rejected:** full R7 (blindspot --deep + brainstorm + red-team + 2 codex gates) —
  disproportionate for a thin collector over a frozen, already-shipped API contract.
- **Reversibility:** high — the security-scan stage can escalate the route mid-run.

## 2. Route-card confirm: skipped
- **What:** Executed the route without asking for approval.
- **Why:** `--auto` passed explicitly by the user.
- **Risk:** low — route card was printed before execution; the user can interrupt.
- **Reversibility:** high — worktree-isolated, nothing committed.

## 3. Plan-validation direction confirm: skipped (auto-adjudicated)
- **What:** No human stop between plan validation and implementation.
- **Why:** `--auto`. Compensated by an INDEPENDENT adversarial validator (separate opus agent, did
  not write the plan) whose REQUIRED fixes are injected into every implementer's prompt.
- **Risk:** a wrong architectural direction reaches code before a human sees it.
- **Mitigation:** validator explicitly checks the runtime/client boundary guardrail, protocol
  stability, scope creep, and whether tests would actually fail pre-implementation.
- **Reversibility:** high — worktree branch, no commits.

## 4. Plan dir placed in the worktree, not the main checkout
- **What:** `plans/260722-1913-tui-add-remote-workspace/` lives in the worktree.
- **Why:** matches prior federation pipelines in this fork; keeps plan + reports committed with the
  feature branch rather than stranded on master.
- **Reversibility:** trivial (`git mv`).

## 5. Brainstorm / red-team / codex adversarial gates: skipped
- **What:** No 3-proposal design artifact, no red-team, no cross-model gate.
- **Why:** R5 route row. Scope is concrete (thin collector), the backend contract
  (`WorkspaceMountRemoteParams`) already exists and is protocol-frozen at v3 — there is no design
  space to debate.
- **Risk:** low. The UX shape (which dialog template) is decided by the scout's "closest analogue"
  finding rather than by a design debate.
- **Reversibility:** high.

## 6. Ship / PR stage deferred out of the workflow
- **What:** Workflow stops after verification; `/hvn-ship` + `/hvn-review-pr` handled afterward.
- **Why:** opening a PR is outward-facing. `--auto` authorizes the pipeline, but the PR must target
  **this fork's** master — the external-contributor guardrail forbids any upstream issue or PR
  (acting account `vietairs`, not `ogulcancelik`).
- **Risk:** a mis-targeted PR would push fork work at upstream.
- **Reversibility:** medium — a wrongly-opened upstream PR is visible publicly before it can be
  closed. Hence the deferral.

## 7. Post-verification: three reviewer questions auto-adjudicated (20:15)

Verification returned DONE_WITH_CONCERNS from all three verifiers and raised three genuine
open questions. All reversible → adjudicated conservatively, not escalated.

### 7a. `RemoteMountState::submitting` — DELETE, not wire
- **What:** Reviewer asked whether the dead flag should be wired (dialog stays open showing
  "mounting…" until `FederationMountReady`) or deleted. Chose delete.
- **Why:** Wiring it is NEW behavior on a branch that is supposed to be a thin collector; deleting
  dead code that claims a behavior it does not have is the smaller, honest change. Mount progress
  and failure already surface through the existing `FederationMountFailed` notice path.
- **Risk:** UX is slightly worse — the dialog closes on ack and a failing mount reports ~25 s later
  as a background notice, with no in-dialog spinner.
- **Alternatives rejected:** wire the flag (adds an in-progress state machine + its own tests, out
  of scope for a collector); leave it dead (ships code asserting a behavior it lacks).
- **Reversibility:** high — wiring it later is additive.

### 7b. `localhost` rejection — UX affordance, not a security guard; server made authoritative
- **What:** Client (`eq_ignore_ascii_case`) and server (exact `==`) disagreed on `localhost`, so
  `LOCALHOST` was blocked in the dialog but accepted over the socket. Removed the client-side
  leading-`-` and localhost checks; server is now the single source of truth.
- **Why:** The security scan established this is not a privilege boundary — a same-UID caller can
  exec `ssh` directly, so `localhost` rejection is a UX affordance. Duplicating it client-side
  bought nothing and created a two-layer disagreement. Bonus: the client check was what made the
  `ErrorResponse` branch unreachable, so removing it fixes the untested-branch defect in the same
  move rather than adding a contrived test fixture.
- **Risk:** a bad target now costs one socket round-trip before the user sees the error, instead of
  being caught locally. Negligible — the server rejects synchronously before any spawn.
- **Alternatives rejected:** tighten `is_local_target` server-side to cover `127.0.0.1`/`::1`/case
  — that changes CLI-facing behavior (`--remote`) beyond this feature's scope; keep both layers and
  align them — duplicate validation that must be kept in sync forever.
- **Reversibility:** high.

### 7c. Documented target format — switch to `ssh://user@host:port`
- **What:** Docs claimed `user@host[:port]`, which ssh cannot accept as a bare `host:2222`.
  Corrected to the `ssh://` URI form already used at line 44 of the same file.
- **Why:** The target is passed verbatim as one ssh argv element; there is no `:port` parsing and
  no `-p` handling anywhere in the tree. Documenting a form that fails produces "mount silently
  does nothing" reports.
- **Alternatives rejected:** implement `host:port` → `-p` translation in the handler — real scope
  growth (new parsing on the trust boundary) on a docs-correctness finding.
- **Reversibility:** high (docs only).

## Outstanding for the human
- Whether this feature should ALSO get a CLI subcommand (`herdr workspace mount-remote`) — the same
  gap exists on the CLI surface. Deliberately OUT of scope for this run (TUI only, per the request
  wording "inside a local herdr").
- Whether the dialog should prefill from a saved remote-target list (scout #2 item 5). Left to the
  plan; conservative default is no persistence in v1.
