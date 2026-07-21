# Pipeline — federation link-close cleanup + mount-failure toast visibility

Task: (1) on federation link close, fire a link-down event that calls `end_federation_mount` and removes that host's remote workspaces (user-approved semantics: mount lifetime = workspace lifetime; remount recreates); (2) make `handle_federation_mount_failed` surface failures for `system`/`terminal` toast delivery, not only `herdr`.
Task source: free text (user request after session root-cause report; gaps documented in plans/260721-1403-multi-remote-federated-workspace-launch/implementation-notes.md "known gap" entry)
Confirmed: 260721 18:31 (route + removal semantics via AskUserQuestion)

ROUTE CARD — federation link-close cleanup + mount-failure toast visibility
Risk: medium — workspace lifecycle/state, runtime event path
Familiarity: high — root-caused this session (impl-notes evidence chain)
Scope: small feature — ~3 files (events.rs, app/api/workspaces.rs, app/state.rs) + tests

Route:
  1. /hvn-predict — agent:general-purpose (sonnet)
  2. /hvn-plan --tdd — agent:planner (sonnet, high effort)
  3. /hvn-plan validate + direction confirm — main-loop (confirm)
  4. /hvn:impl-notes init — agent:hvn-scout
  5. /hvn-cook --auto — agent:hvn-implementer (single unit — both fixes share workspaces.rs)
  6. /hvn:impl-notes review — main-loop (distillation)
  7. /hvn-code-review — agent:code-reviewer (background)
  8. /hvn:ship-gate — main-loop (attestation)

Skips: /hvn:blindspot — unknowns already paid (live root-cause this session); brainstorm — scope concrete; red-team/codex — medium risk; worktree create — already on matching feature branch feat/remote-workspace-federation.
