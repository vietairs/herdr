Task: mount-dialog recents feature + investigate the herdr long-run
slowdown.

Shipped: recents feature (9 files, +483/-21). Perf investigation was
diagnosis-only initially (proven_cause=null) but the resync-purge leak was
confirmed and promoted to a fix in the same round.

Code review: BLOCKED (1 blocker + 3 majors) → all resolved; round 2
DONE_WITH_CONCERNS (minors only).

Commits: cd21be84 (fix(federation): resync purge) + 3a489601 (feat(tui):
recents). PR #9: https://github.com/vietairs/herdr/pull/9.

Overhead: 8 agents, ~1h20m wall, ~1.14M tokens.

Deferred to post-merge (per plan's own note — verify these were done):
sync master, remove worktree .claude/worktrees/mount-recents-perf, rebuild
+ restart local/remote servers.

Archived-at-SHA: d2ce3de280b2446a3d07575795416b6976077898
