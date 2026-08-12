# pipeline

Task: Remote-workspace pane content does not auto re-render correctly when the window size changes (terminal resize, adding a split, closing a split).
Task source: free text + screenshot (herdr --remote appn-ltu-vm-105, two mounted remote panes side by side)
Timestamp: 2026-07-22 16:38 (Australia/Melbourne)
Worktree: ../herdr-worktrees/remote-workspace-resize-rerender (branch fix/remote-workspace-resize-rerender)

## Route card (verbatim)

ROUTE CARD — Remote-workspace pane content not auto-rerendering on window size change
Risk: medium — federation wire path (pane_source.rs RT-F10 pinned resize semantics, federation_accept.rs command routing); shared runtime contract; no auth/schema/migration keywords
Familiarity: high — 6 prior federation plan dirs, active impl-notes log, 4 recent federation fix commits
Scope: small–feature — resize propagation path: TUI layout -> mount client -> wire -> server actor -> remote runtime -> render stream
Payoff: medium — user is direct consumer; remote panes unusable after any layout change

Route (R10 — bug, root cause UNKNOWN):
  1. /hvn-worktree — agent:hvn-git-manager
  2. /hvn-debug — parallel root-cause investigators (3 lenses) + fable-tier advisor adjudication — agent:hvn:hvn-root-causer
  3. /hvn-fix — agent:hvn:hvn-implementer
  4. /hvn-code-review + build/test — agent:code-reviewer
  5. /hvn:ship-gate — main-loop (attestation)

Skips: blindspot / brainstorm / predict — cause-finding IS the discovery (R10 route rule)
Mode: --auto — no stop-and-ask gates; decisions logged to reports/auto-decisions-260722-remote-resize.md
