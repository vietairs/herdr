Task: remote agents missing from the agents sidebar; remote pane render
corruption on vm105; split in a remote workspace spawning a local pane.

Shipped (3 chained fixes): AgentStatus relay wiring (client.rs was
discarding AgentStatus frames); gated local resize-recovery ANSI replay off
for remote panes (is_remote_backed flag); remote split protocol scaffolding
+ end-to-end wiring (new FederationMessage variant, server dispatch, client
request send, local pane materialization).

Code review: REQUEST_CHANGES (1 critical, 2 major) → critical + major#2
fixed; major#3 flagged as a product decision, not blocking.

Ship-gate: --auto adjudicated PASS 2026-07-22 01:06 (attestation skipped +
logged in auto-decisions report); diff left uncommitted for the user.

Learnings: federation protocol had 3 independent gaps in one area (agent
identity relay, resize-replay racing, no split variant) — worth an
end-to-end federation-parity audit if similar surfaces reappear.

Archived-at-SHA: d2ce3de280b2446a3d07575795416b6976077898
