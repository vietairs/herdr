# Auto-decisions — federation pane-close sync (--auto run, 2026-07-24 15:37 AEST)

1. **Route-card confirm skipped** (--auto). What: R5 medium-risk route chosen. Why: protocol change but v3 unreleased on fork, exact SplitPaneRequest template exists, familiarity high. Risk: if protocol compat with older peers matters more than assessed, R7 gates (red-team, codex) were skipped. Alternatives rejected: R7 (heavier gates buy little on a fork-internal unreleased protocol), R2 (too thin — cross-module + wire change). Reversibility: high — isolated worktree, no commits until user reviews.
2. **Brainstorm skipped.** Fix design already settled in prior diagnosis (memory: ClosePaneRequest mirroring SplitPaneRequest; reuse existing server→client mirror path). Nothing to debate.
3. **Plan-validation direction confirm auto-adjudicated** — plan agent self-validates against blindspot + predict findings; contradictions must be flagged in its output. Logged here in lieu of the interactive stop.
4. **Ship-gate attestation deferred to user** — workflow ends with uncommitted diff + green tests in the worktree; no commit, no PR, no merge without the user.
Run: Workflow wf_b2226c6c-949, worktree ../herdr-worktrees/federation-pane-close-sync, branch fix/federation-pane-close-sync.
