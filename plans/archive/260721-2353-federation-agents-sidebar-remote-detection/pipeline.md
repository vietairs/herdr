# Pipeline

Task: agents sidebar cannot detect/show Claude sessions from remote federation hosts (vm105 confirmed, vm100 untested); only local Mac sessions appear.
Task source: free text + screenshot (Claude Code running in remote workspace pane, absent from agents sidebar)
Mode: --auto
Timestamp: 2026-07-21 23:57 AEST

ROUTE CARD
Risk: medium — federation event propagation, no HIGH keywords
Familiarity: high — active federation branch, prior impl-notes
Scope: small — remote agent-detection state not reaching local sidebar
Route (R10):
  1. /hvn-debug — agent (root-causer, evidence chain)
  2. /hvn-fix — agent (implementer)
  3. /hvn-code-review — agent
  4. /hvn:ship-gate — auto-adjudicated (--auto)
Skips: pre-code stages (R10 — cause-finding is discovery); worktree create (already in matching feature checkout)
