# Pipeline — cmux clipboard-over-SSH study (R19 thinned, --auto)

Task: analyze how https://github.com/manaflow-ai/cmux copies images/clipboard over a remote SSH connection; adapt applicable mechanism into herdr's in-flight remote-paste fix worktree; self-test with agents.
Task source: free text (/hvn:cortex --auto), 2026-07-24 12:13
Related pipeline: plans/260724-1034-remote-paste-live-failure-diagnosis/ (fix worktree ../herdr-worktrees/remote-paste-cmdv-bridge, live fall-through still under debug)

Route: 1) xia extract (cmux recon) → 2) compare/decision matrix → 3) adapt into existing worktree (conditional) → 4) agent self-test (suite + scripted checks; TUI paste e2e = manual fallback, stated plainly).
Autonomy: --auto — no stops; decisions logged to reports/auto-decisions-260724-cmux-clipboard-study.md.
