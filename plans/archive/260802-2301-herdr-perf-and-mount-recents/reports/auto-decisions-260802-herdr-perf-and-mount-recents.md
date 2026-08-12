# Auto-decisions — 260802 herdr perf + mount recents (--auto)

## Outcome lock (goal-warmup, auto — no interview)
- Outcome A: identify the proven root cause of long-run slowdown of `herdr --remote ... --remote-workspace` on this Mac; ship a fix only if the cause is proven and the fix is bounded/low-risk. Otherwise deliver an evidence-backed diagnosis.
- Outcome B: mount-remote-workspace dialog persists at least the 5 most recent successfully connected targets and lets the user pick them instead of retyping.
- Constraints: state/runtime separation (AppState pure), server-owned runtime facts vs TUI presentation split per CLAUDE.md guardrail; no protocol bump unless required; no unwrap in prod code; recents persistence must not store secrets.
- Non-goals: no federation protocol redesign; no cancellation-of-dial feature; no Windows validation (build pre-broken on fork).
- Assumptions (conservative): "slow" = UI/interaction latency growing over days, not memory exhaustion (RSS 37MB after 4d disproves leak); recents = success-correlated by target string (existing sharp edge, acceptable); persistence location = herdr's existing config/state dir alongside channel setting.

## Decisions
- 23:05 Route confirm skipped (--auto). Risk medium; safest viable route chosen (debug-then-fix + thin feature route). Alternatives rejected: R7 heavy gates (no contract change), inline fix without diagnosis (cause unproven).
- 23:08 `.claude/worktrees/` gitignore entry not added: scout-block hook denies `.git/info/exclude`; `.claude/` already untracked → no status pollution. Risk: none. Reversible: trivially.
- 23:55 Perf verdict accepted as diagnosis-only: investigator returned proven_cause=null for the FELT slowdown (top hypotheses: constant-factor render cost under federated load — git discovery reads per frame; remote-side/ssh-link degradation, 14 LinkClosed drops in 2 days). No speculative fix shipped. Risk of inaction: user's slowdown not directly fixed this run; mitigated by evidence report for a follow-up.
- 23:55 PROMOTED the rank-4 CONFIRMED leak (handle_federation_mount_ended never purges remote_resync_pane_index; sibling of db671fbc; high confidence, bounded one-call fix + test) into the fix round even though it does not explain the latency — outcome-lock allows proven+bounded fixes; correctness hazard (stale pane mappings after remount) justifies it. Honest labeling required in commit: not claimed to fix the slowdown.
- 23:55 Review returned BLOCKED (1 blocker: TOML escaping config-wipe; 3 majors). Auto-adjudication: fix-and-re-review round (safest viable) instead of shipping with concerns or aborting.
- 00:20 Ship-gate attestation + before-merge approval skipped (--auto). Re-review round 2 = DONE_WITH_CONCERNS (minors only, all deliberately out of scope). Committed as TWO focused commits (leak fix cd21be84 split from feature 3a489601 via hunk-level staging of workspaces.rs), pushed, PR #9 opened on vietairs/herdr. MERGE LEFT TO USER per cortex DO-NOT. Commit-message alignment question skipped per --auto; messages follow repo conventional style, no AI references (project rule overrides session trailer).
- 00:22 Worktree teardown + plan-gc DEFERRED until merge confirmed (never tear down before merge). Remote branch will be kept regardless.
