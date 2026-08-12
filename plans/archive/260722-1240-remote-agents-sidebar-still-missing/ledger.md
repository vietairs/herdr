Task: remote agents still missing from the sidebar after the prior fix
(260721-2353) — live-evidence follow-up.

Root cause: agent IDENTITY was never set for remote panes (pid-gated probe,
no identity field on the wire).

Shipped: identity carried on the wire + pid-gate bypass for remote panes;
protocol stayed at v3 (no wire bump needed). Stale-identity cleanup via a
debounced miss path.

Code review: APPROVE_WITH_NITS (1 major stale-identity + 2 minor) → all
remediated, 2717/2717 green.

Shipped: commit 29fe7b6 pushed to master; Mac + vm100 + vm105 redeployed.
End-to-end verified live: Mac `agent.list` showed claude/idle on the remote
pane.

Overhead: 7 agents, ~40 min, ~500k tokens.

Archived-at-SHA: d2ce3de280b2446a3d07575795416b6976077898
