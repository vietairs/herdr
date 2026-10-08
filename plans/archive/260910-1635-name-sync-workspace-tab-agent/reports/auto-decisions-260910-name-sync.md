# auto-decisions — name-sync-workspace-tab-agent

Run mode: `--auto --advise`. Risk tier: medium (not HIGH, so no unattended-high banner).
Every gate below was auto-adjudicated with conservative bias because `--auto` skips the question.

## 1. Route-card confirm (skipped by --auto)
What: routed R5 (medium risk, no plan yet) WITH a brainstorm stage added.
Why: R5 skips brainstorm on the stated precondition "scope already concrete". That precondition is false
  here — four naming-policy questions have multiple viable answers. Including brainstorm is the faithful
  application of the row, not a deviation from it.
Risk: one extra design phase (3 opus proposals + 1 arbiter) on a medium-payoff task.
Alternatives rejected: R2 (too small — three defects across five files); R10 (cause is proven, not unknown);
  R7 (no HIGH-risk keyword — no auth/schema/migration/payment/security surface).
Reversibility: high — design output is a markdown file, nothing shipped.

## 2. Worktree location (deviation from cortex default)
What: worktree at ~/Projects/worktrees/herdr-name-sync instead of <repo>/.claude/worktrees/.
Why: matches the convention already in use in this checkout (git worktree list shows
  ~/Projects/worktrees/herdr-test-windows11-herdr-release) and avoids adding a .gitignore entry.
Risk: minimal; teardown path is identical.
Reversibility: high.

## 3. Base branch for the worktree
What: branched from origin/master (c2f4166a) after an explicit fetch, NOT from local master.
Why: local master was ahead 2 / behind 107 at run start. Branching from it would have replayed a stale base.
Risk: none — this is the correct base.
Reversibility: n/a.

## 4. Implementation fan-out suppressed
What: single implementer instead of the standing parallel-by-phase default.
Why: the three defects overlap on src/workspace.rs and src/app/actions.rs, so file ownership is NOT
  disjoint. Cortex gates parallel fan-out on clean ownership; concurrent edits here would conflict.
Risk: slower wall-clock.
Alternatives rejected: per-defect fan-out (would race on two shared files).
Reversibility: high.

## 5. --advise counsel
kongming is substituted for advisor under --auto. Counsel for the design gate is carried by the
Design-phase arbiter, which runs the full arbiter checklist (contradictions / unverified claims /
unresolved questions) before selecting a policy and writes it to reports/design-decision.md.

## Open policy questions handed to the arbiter rather than guessed silently
1. Does a rename permanently pin a workspace name, or resume auto-sync when the project changes?
2. Do tabs derive from cwd, detected agent, or running command?
3. Which direction does tab <-> agent propagation run?
4. Where does the logic live, given naming is shared session organization (server-side per CLAUDE.md)?
