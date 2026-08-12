# Auto-decisions — terminal-dependent clipboard image paste (--auto --advise)

Run started 2026-08-07 14:57. Route R10. Every gate below was skipped by `--auto` and
auto-adjudicated with conservative bias; entries are append-only.

---

## 1. Outcome lock (ak-goal-warmup substitute, auto-locked from task text)

`ak-goal-warmup` is not installed in this skill set — outcome-lock ran inline in the main loop
per the absent-skill fallback.

- **Outcome:** a user running herdr inside Apple Terminal.app or Warp can paste a macOS clipboard
  image into a pane of a mounted remote (federated) workspace, with the same end result they get
  today in cmux (image staged on the remote, path delivered to the agent in that pane).
- **Constraints:** do not regress the cmux path; do not weaken the existing staging security gates
  (temp-dir containment, TOCTOU-safe read, extension whitelist, FILE_STAGING capability); macOS
  first, but do not break Linux builds; no new dependencies without justification.
- **Non-goals:** achieving attachment-style `[Image #N]` ingestion (prior study established no
  terminal manager achieves this, cmux included); Windows; multi-image paste.
- **Acceptance:** proven cause with an evidence chain; fix verified by tests; a stated live-test
  recipe for the terminals that cannot be exercised headlessly.
- **Assumption logged:** the user presses the same key gesture (most likely Cmd+V) in all three
  terminals. If they in fact use Ctrl+V in cmux and Cmd+V elsewhere, the cause ranking changes.
  Flagged to the debug stage as hypothesis H1 rather than silently assumed.

## 2. Route-card confirm — SKIPPED (--auto)

- **What:** proceeded on route R10 (bug, root cause unknown) without user confirmation.
- **Why:** the two entry points that could carry this feature are both present on master, so the
  symptom is not explained by any known missing change — cause must be proven before fixing.
  R2/R5 would have jumped to a fix on an unproven cause.
- **Risk:** low. R10's first real stage is read-only diagnosis; nothing is changed before evidence.
- **Alternatives rejected:** R5 (plan-first) — no design question, only an unknown cause.
  R2 (straight fix) — would have guessed at the mechanism.
- **Reversibility:** full. No source touched yet.

## 3. Worktree stage run inline rather than via `git-manager` agent

- **What:** created `.claude/worktrees/terminal-clipboard-image-paste` (branch
  `fix/terminal-clipboard-image-paste`) with a direct `git worktree add`, not a delegated spawn.
- **Why:** single mechanical git mutation; a subagent round-trip costs more than the command.
- **Risk:** none. Verified via `git worktree list`. `/.claude/` is already gitignored, so the
  worktree cannot pollute the index.
- **Reversibility:** full — `git worktree remove` + branch delete.

## 4. Asked the user ONE question despite `--auto`

- **What:** paused to ask whether the user had ever tried Ctrl+V (vs Cmd+V) in Apple Terminal/Warp.
- **Why:** not a gate — a missing *fact* about the user's own environment that no amount of source
  reading could settle, and the two branches led to entirely different deliverables (docs/
  discoverability vs. an input-path code fix). Building the wrong one would have been wasted work.
  `--auto` suppresses approval gates; it does not manufacture evidence.
- **Answer:** "Tried Ctrl+V, nothing happened" — and no toast reported.
- **Effect:** refuted the benign reading of H1. Entry A is independently broken, not merely
  undiscoverable. Investigation reopened on the Ctrl+V fall-through.
- **Risk of having asked:** one round-trip. Risk of NOT asking: a shipped docs-only "fix" for a
  real code defect, or input-path churn for a non-bug.

## 5. Assumption superseded

The outcome-lock assumption in entry 1 ("user presses the same gesture in all terminals") is now
resolved by evidence and no longer carried: the user tried both gestures.

## 7. Acceptance criteria expanded by the user (supersedes entry 1's outcome lock)

User, 15:15: *"it needs to work with ctrl+V, cmd+V, and paste from this mac clipboard to the
remote workspace. it is working when i run herdr inside the cmux app only."*

Revised outcome: Ctrl+V **and** Cmd+V both stage a mac-clipboard image to a mounted remote
workspace, in Apple Terminal and Warp, not only cmux.

**Feasibility concern raised to the user rather than silently absorbed** — Cmd+V splits:

| Case | Reaches herdr? | Verdict |
| --- | --- | --- |
| Ctrl+V, any clipboard shape | yes (decode proven) | achievable — this is the real bug |
| Cmd+V, clipboard holds a FILE (Finder copy) | yes — terminal pastes the POSIX path as text | achievable; blocked today by the deliberate `temp_dir()` gate in `src/image_path.rs:87-109` |
| Cmd+V, clipboard holds RAW IMAGE DATA (screenshot) | **no** — no text flavor, terminal emits nothing | not achievable in Terminal.app/Warp without a cmux-style helper |

The third row is a terminal limitation, not a herdr defect: the terminal consumes Cmd+V and
answers it itself. Stated plainly to the user at 15:15; sent to the debug agent to verify or
refute rather than assumed. Work proceeds on rows 1 and 2, with row 3 scoped separately if wanted.

Widening the temp-dir gate is a SECURITY decision, not a mechanical one — the agent was
instructed to propose a replacement trust boundary, never to delete the gate.

## 6. H1 downgraded from sole cause to partial cause

- **What:** the debug stage's first-pass conclusion (H1: cmux-only interception, no herdr bug) is
  retained as the explanation for *why cmux works*, but is no longer accepted as the full cause.
- **Why:** it does not account for Ctrl+V producing no effect and no toast.
- **Risk of the earlier reading:** had `--auto` accepted it and closed the run, the outcome would
  have been a "working as designed" verdict on a genuine defect.


## 7. Round-4 re-review findings — adjudicated 2026-08-07 23:1x

Review: `code-review-260807-round4.md`, verdict APPROVE_WITH_NITS, two Medium findings.

### 7a. M1 off switch was half-connected — FIXED IN CODE (not by softening docs)

- **Finding:** `should_bridge_clipboard_image_paste` (`src/client/mod.rs`) returned
  `is_remote_client` for the empty-paste shape *before* consulting the binding. So with
  `keys.remote_image_paste = ""`, a `herdr --remote` client still read its clipboard and still
  uploaded — while round 4's new doc strings asserted the binding "turns the whole feature off".
- **Two options:** soften the four doc strings, or make the code match them.
- **Chosen: make the code match.** An off switch that is documented as total must be total; the
  server-side gate was already honouring it, so the client was the odd one out. Moving the
  `None` early-return above the empty-shape check fixes it in one place — `suppress_unbridged_…`
  is only ever reached inside the bridge branch, so it inherits the gate rather than needing its
  own.
- **Test:** the existing test asserted the buggy shape (`EMPTY, true, None` → bridge). Inverted it
  and documented why. Gate proven by inspection, not by an executed mutation run: the `None`
  early-return now strictly precedes the empty-shape check, so the pre-fix code could not have
  produced the new assertion.
- **Verification after the fix:** `3394 passed / 0 failed` at `--test-threads=2`
  (the flaky `pane_graphics_stream` passed this run; same 3394 total). `cargo fmt --check` exit 0.

### 7b. M2 configured-key host-clipboard residue — NOT closed; escalated to the user

- **Finding (confirmed end-to-end by the reviewer):** on `herdr --remote`, `Ctrl+V` with no local
  image forwards `0x16`; the server reads the **host** clipboard and stages that image into the
  pane the remote operator is watching. Rounds 1-3 are what made this reachable.
- **Why not auto-closed:** the empty-paste shape could be suppressed client-side because it
  carries no payload. The configured key cannot — it also reaches non-federated panes, where it is
  an ordinary keystroke a pane app expects (readline quoted-insert, vim visual-block). Blanket
  client-side suppression would break normal input. Closing it properly needs the server to know
  the client is remote, i.e. a `ClientMessage::Hello` field = a wire-protocol change.
- **Adjudication: accept for now, document, escalate.** A protocol bump is out of scope for a fix
  branch, and the exposure depends on a fact only Can can supply — whether `herdr --remote` is ever
  cross-user. On this fork federation is self-attach between the user's own machines, which makes
  it low-severity; that assumption is exactly what needs confirming rather than presuming.
- **Not silently absorbed:** carried into the handoff as an open decision.
