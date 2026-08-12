# Pipeline — federation UI labels + PR #10 fork-feature verification

Task: verify every fork-developed function survived PR #10, then fix (a) mounted/remote workspaces
not showing the folder name the way local workspaces do, and (b) remote-workspace agents not
showing the cloud icon in the agents panel.

Task source: free text + 2 user screenshots (sidebar "spaces" list; "agents / grouped" panel)

Flags: `--auto` (no confirm stops, decisions logged) `--advise` (kongming counsel at gates)

## Route card

```
Complexity: hard -> (revised after 2b, see progress file)
Risk: medium — federation UI + shared workspace metadata; no auth/schema/migration.
              Touches CLAUDE.md runtime/client boundary (workspace facts are server-owned).
Familiarity: high — this session merged the surrounding code; extensive prior federation work
Scope: feature — 1 verification + 2 related display bugs
Payoff: high — user is the named consumer; remote workspaces render as second-class
        (no folder name, no cloud marker among agents)
Route:
  1. evidence pass — 3 parallel agents (2 diagnose, 1 verify PR #10)
  2. fix + tests
  3. code-review
  4. ship-gate
  5. PR (merge-ready, no merge)
Advise: 2 gates — pre-fix direction, pre-PR — via kongming (--auto substitution)
Autonomy: --auto
```

## Branch strategy (decision)

Branch `fix/federation-ui-labels` is stacked on `merge/upstream-v0.8.0` (commit a021433a),
**not** on `master`.

Reason: upstream v0.8.0 rewrote `src/ui/sidebar.rs` and the agent-list rendering. A fix authored
against `master` would be built on code that PR #10 replaces, and would conflict on merge. PR #10
is open and unmerged, so stacking is the only way to write the fix against the code that will
actually ship.

Consequence: this PR must merge AFTER PR #10, or be rebased if PR #10 is rejected/reworked.

## Observed symptoms (from screenshots)

Sidebar "spaces": local workspaces render two lines — name, then a dim second line (branch-like:
`dev`, `main`). The remote workspace renders ONE line: `▲ appn-ltu-vm…` — cloud glyph, truncated,
host-ish rather than a folder name, no dim second line.

Agents panel: `✓ APPNltu_s… · 1 / claude` — no cloud glyph, visually identical to local agents,
despite its workspace showing one in the sidebar.
