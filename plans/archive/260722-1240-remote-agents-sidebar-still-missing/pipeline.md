# Pipeline

Task: agents panel only detects agents from local mac; remote vm100/vm105 agents still missing after AgentStatus relay fix (merged 965c15e, deployed to all 3 hosts).
Task source: free text + screenshot (vm105 workspace running an agent; agents panel shows only local "herdr claude")
Mode: default interactive — user chose "Debug only first" at confirm
Timestamp: 2026-07-22 12:40 AEST

ROUTE CARD (R10)
Risk: medium — recurrence of a "fixed" bug; federation event propagation
Familiarity: high — pipeline 260721-2353 context, relay wiring known
Scope: small — one propagation chain
Payoff: high — sidebar blind to 2 of 3 hosts

Route:
  1. /hvn-debug — hvn-root-causer (live evidence) — APPROVED, run now
  2. /hvn-fix — hvn-implementer — GATED: confirm with user after debug report
  3. /hvn-code-review — code-reviewer — gated
  4. /hvn:ship-gate — main-loop — gated; then redeploy affected binaries

Skips: pre-code stages (R10); worktree (small fix on master)

Leading hypothesis (to verify, not assume): AgentStatus only relayed on change; agents already running at mount time never re-emit, so pre-mount sessions stay invisible.
