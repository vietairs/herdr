# Pipeline — federation pane-close sync

Task: Add pane-close sync to the federation wire protocol: (1) client→server ClosePaneRequest so closing a remote pane in the TUI reaches the serving host; (2) server→client live pane-close propagation so a mounted client updates without remount.
Task source: free text (/hvn:cortex --auto), diagnosis from 2026-07-24 session; memory herdr-federation-no-pane-close-wire-message.
Mode: --auto (fully autonomous; gates auto-adjudicated + logged to reports/auto-decisions-260724-federation-pane-close-sync.md)
Timestamp: 2026-07-24 15:37 AEST

Classification:
- Risk: medium — wire-protocol change; unreleased protocol v3, SplitPaneRequest template exists, server→client pane mirroring exists (b5cb8ce8)
- Familiarity: high — both gaps diagnosed in prior sessions, fix sketched in memory
- Scope: feature — protocol + server + TUI, two directions
- Payoff: high — user hit this today; workaround is ssh pane close + full remount

Route (R5):
  0. /hvn-worktree — done inline: ../herdr-worktrees/federation-pane-close-sync, branch fix/federation-pane-close-sync
  1. /hvn:blindspot — agent (workflow)
  2. /hvn-predict — agent (workflow); report saved to reports/
  3. /hvn-plan --tdd — agent (workflow)
  4. plan validate — agent (workflow); direction confirm auto-adjudicated (--auto)
  5. /hvn:impl-notes init + /hvn-cook — implementer agent in worktree, tests-first
  6. /hvn-code-review — parallel lenses (correctness/protocol-compat/regression)
  7. fix findings — implementer agent
  8. /hvn:ship-gate — auto-adjudicated verdict, logged; merge stays with user

Skips: brainstorm (scope already concrete), red-team (medium risk), security-scan (no auth/payment keywords)
