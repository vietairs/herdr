# Pipeline — remote image paste live failure

Task: image paste from mac clipboard doesn't reach remote Claude Code on `herdr --remote appn-ltu-vm-105 --remote-workspace`; a macOS temp path arrives as plain text instead. Secondary ask (separate scope, re-classify later): Claude Code should show `[Image #1]` (ingest bytes) rather than a staged file path.
Task source: free text + user screenshot (2026-07-24 session)

ROUTE CARD (confirmed 2026-07-24 10:34, all stages checked)
Risk: medium — federation wire + input intercept surface; diagnosis-first
Familiarity: high — full artifact trail in plans/260722-1624-remote-workspace-paste-image-files/
Scope: small — likely env/handshake/trigger mismatch, not redesign
Payoff: high — user directly blocked pasting screenshots into remote sessions

Route (R10):
  1. /hvn-debug — agent:hvn-root-causer (inherits tier)
  2. /hvn-worktree — agent:git-manager (conditional: only if fix is code)
  3. /hvn-fix — agent:hvn-implementer (conditional)
  4. /hvn-code-review — agent:code-reviewer (conditional on code fix)
  5. /hvn:ship-gate — main-loop (attestation; no-op if no code shipped)

Skips: blindspot/brainstorm/predict/plan — R10, cause-finding is the discovery; prior artifacts map the code.
Prior context: feature merged to master via PR #5 (b43cc5aa), feat f1a11b3c. Old pipeline dir plans/260722-1624-remote-workspace-paste-image-files/ left untouched (its stages 7-9 moot post-merge).
