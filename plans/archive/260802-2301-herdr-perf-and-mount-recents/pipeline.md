# Pipeline — herdr long-run slowdown + mount-dialog recent targets

Task: (A) herdr with federated remote workspaces (`herdr --remote appn-ltu-vm-105 appn-ltu-vm-100 --remote-workspace`) gets really slow on this Mac after running a long time — diagnose, fix if cause proven. (B) mount-remote-workspace dialog should save at least the 5 last successfully connected targets and offer them for reuse.
Task source: free text + screenshot of mount dialog, `--auto`
Date: 2026-08-02 23:01 AEST

## Route card
Risk: medium — perf touches federation/runtime hot paths; recents adds persisted state + TUI surface. Familiarity: high — dialog built by us (PR #3); federation ops mapped in memory. Scope: two sub-deliverables (R10 debug + small feature). Payoff: high — daily-use pain on this Mac.

Live evidence at intake: herdr server pid 7049 up 4d10h RSS 37MB (no mem leak), CPU now 5.7% vs ~1.9% lifetime avg (growing busy-work). TUI client pid 51221 up 2d12h RSS 21MB.

Route:
1. worktree create `.claude/worktrees/mount-recents-perf` branch `feat/mount-recents-and-perf` — main-loop (setup)
2. investigate (parallel): perf root-cause (live sample + code scan, inherit tier) + feature scout (sonnet)
3. implement recents feature — worktree (sonnet implementer)
4. implement perf fix ONLY if cause proven + bounded, else diagnosis report — worktree
5. verify: fmt + `cargo test --bin herdr -- --test-threads=4` (ZIG env; known flakes re-run serially)
6. code-review on diff (inherit tier)
7. commit on branch, merge-ready; teardown deferred to post-merge resume

Skips: brainstorm (scope concrete), predict (thin route, no plan-stage), ship-gate attestation (`--auto` — logged to auto-decisions instead).
Autonomy: --auto — gates auto-adjudicated, logged to reports/auto-decisions-260802-herdr-perf-and-mount-recents.md
