# pipeline

Task: auto-sync workspace ("space") / tab ("window") / agent names with the cwd or codebase name, and propagate a tab rename to the agent name.
Task source: free text (user request, --auto --advise)
Timestamp: 2026-09-10 16:35 (Australia/Melbourne)

## Route card

ROUTE CARD — auto-sync workspace/tab/agent names with cwd + propagate renames
Complexity: hard -> moderate — evidence proved the sync machinery already exists and is well-factored; cause located rather than unclear (3 scout units + 5 direct reads, ~04:30)
Risk: medium — no HIGH-risk keywords; but user-visible naming on shared runtime state, touches persisted snapshot (src/persist/restore.rs:414) and an established rename surface (68 `fn *rename*` in src/)
Familiarity: medium — no prior plan dir on naming; scouts mapped the territory to file:line; CRG probe unresolved (no code-review-graph entry in ~/.hvn/ledger.json)
Scope: feature — three distinct defects with clean file ownership
Payoff: medium — fork maintainer running many spaces identifies space/tab/agent at a glance instead of renaming by hand; evidence: this request
Change set: 8 files
  src/workspace.rs (change) — tab_display_name ordinal fallback :527; workspace display_name :1145-1191
  src/app/actions.rs (change) — auto-label refresh + custom_name freeze :2660-2695
  src/workspace/aggregate.rs (change) — pane_label / agent_label derivation :48-56
  src/workspace/tab.rs (change) — Tab::custom_name :39, set_custom_name :204
  src/terminal/state.rs (change) — agent_name :133, manual_label :135, set_agent_name :1871
  src/workspace/git/discovery.rs (reuse) — derive_label_from_cwd :21-40, fallback_label_from_cwd :27-40
  docs/next/website/src/content/docs/*.mdx (change) — keyboard / cli-reference / configuration
  docs/next/CHANGELOG.md (change) — user-facing runtime change
Armory: omitted — no domain match
Advise: 4 gates — brainstorm approval, plan-validation confirm, ship-gate attestation, before-merge approval — via kongming (--auto substitution); ~4 extra fable-tier spawns

## Diagnosis (evidence)

D1 — tabs have NO cwd derivation at all.
  src/workspace.rs:527 `tab_display_name` = `tab.custom_name.unwrap_or_else(|| (tab_idx + 1).to_string())` — a bare ordinal.
  Workspaces derive from git-root/cwd basename; tabs never do.

D2 — one workspace rename freezes the auto-label forever.
  src/app/actions.rs:2680 `changed |= ws.custom_name.is_none();` — once custom_name is Some, cwd changes stop
  producing a visible relabel. The refresh itself works: actions.rs:2660-2695 runs on the ~1.5s git-refresh cycle,
  and src/workspace.rs:1185-1191 `automatic_display_name_for_cwd` even falls back to a live basename on cache miss.

D3 — no propagation between the naming namespaces.
  Four independent rename surfaces (workspace.rename / tab.rename / pane.rename / agent.rename), each with its own
  CLI verb, socket method and keybinding. Tab name lives in Tab::custom_name; pane label in TerminalState::manual_label;
  agent name in TerminalState::agent_name (set by detection or API). Renaming a tab touches none of the others.

CWD tracking is NOT the defect: OSC 7 event capture plus on-demand process polling
(src/platform/macos.rs:931 `process_cwd`) keep TerminalState::cwd live; refresh runs off the render path.

## Open questions (carried into brainstorm, not resolved by evidence)

1. Should a renamed workspace ever resume auto-sync, or is a rename a permanent pin? (D2 policy)
2. Should a tab auto-name from cwd, from the detected agent, or from the running command? (D1 policy)
3. Which direction does D3 propagate — tab -> agent, agent -> tab, or a shared derived source?
4. Runtime/client boundary: naming is shared session organization, so changes belong in server state and the
   JSON API/event path, not the TUI client only (project CLAUDE.md guardrail).
