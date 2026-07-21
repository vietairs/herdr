# Pipeline — multi-remote federated workspace launch

Task: one command launches 1 local + N remote federated workspaces:
`herdr --remote localhost 131.172.248.163 131.172.248.161 --remote-workspace`
Task source: free text (/hvn:cortex, --semi-auto)
Timestamp: 2026-07-21 14:03 AEST

Classification:
- Risk: medium — public CLI contract change + concurrent multi-mount lifecycle; no auth/schema change
- Familiarity: high — federation plan + P1–P9b shipped on feat/remote-workspace-federation
- Scope: feature — main.rs arg parse, remote/unix.rs, mount registry (single-mount guard S10.1), sidebar per-host groups

Route: R5 (medium risk, no plan yet) — semi-auto
1. /hvn:blindspot — parallel hvn-scout legwork + main-loop synthesis
2. /hvn-predict — persona debate; save report to reports/
3. /hvn-plan --tdd — planner agent
4. Plan-validation direction confirm — HARD STOP (semi-auto gate)
5. /hvn:impl-notes init
6. /hvn-cook --auto — parallel where file ownership is clean
7. /hvn:impl-notes review
8. /hvn-code-review
9. /hvn:ship-gate

Skips: brainstorm (scope already concrete), red-team (medium risk).
Worktree: REUSE existing feat/remote-workspace-federation (session already inside matching feature branch).
Related prior pipeline: plans/260713-1217-herdr-remote-workspace-federation (its item 13 smoke re-run stays blocked there, untouched).
