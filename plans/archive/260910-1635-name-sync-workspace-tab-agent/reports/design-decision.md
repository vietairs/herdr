# design decision — naming policy

**Chosen: Policy C — one shared derivation chain with explicit precedence at every layer.**

Selected by the user directly on 2026-09-10. The arbiter agent was interrupted by the pause and never
ran, so this record is the user's choice plus the arbiter checklist performed by the controller — not a
model-selected verdict. Full proposals: `reports/design-proposals.md`.

## Line-number authority

Policy C cites lines from the WORKTREE (origin/master `c2f4166a`). The earlier evidence pass in
`pipeline.md` cites the main checkout, which is 107 commits older. Both were correct in their own tree.
**The worktree numbers are authoritative for implementation.** Verified in-tree:

| Thing | Worktree (use this) | Main checkout (stale) |
|---|---|---|
| `tab_display_name` | src/workspace.rs:478 | :527 |
| D2 redraw gate | src/app/actions.rs:1555 | :2680 |
| `display_name` family | src/workspace.rs:1059 / :1067 / :1085 | :1145 / :1153 / :1171 |
| `automatic_display_name_for_cwd` | src/workspace.rs:1099 | :1185 |
| `public_tab_number` | src/workspace.rs:1028 | — |
| contradicting test | src/app/api/agents.rs:699 | — |

## Precedence ladder (identical at every scope, highest first)

| Rung | Source | Notes |
|---|---|---|
| 1 | User override at this scope, or inherited from nearest enclosing renamed scope | Only authoritative state; the only thing persisted |
| 2 | Agent identity | Has its own hook-over-detection authority chain (src/terminal/state.rs) |
| 3 | cwd / git-root derivation | Reads cached scalars only; refresh stays on the ~1.5s git pass |
| 4 | Stable ordinal | `tab.number` via `public_tab_number` (:1028), NOT `tab_idx + 1` |

## Behavioral contract

1. **Workspace never renamed, user cd's to another project** — label follows the new git-root/basename, as today.
2. **Workspace renamed, then user cd's** — stays pinned to the chosen name. New: the derived name stays live
   underneath and is observable via `NameSource`, so clearing the override snaps to the correct current
   project with zero refresh delay. This is D2's fix: actions.rs:1555 gates only the redraw signal, while
   :1552-1553 already refreshes the derived label unconditionally.
3. **Tab default name** — git-root or basename of its root pane's cwd; where that cwd is a descendant of the
   workspace's cwd, the path suffix relative to it, so a single-repo workspace does not render four tabs all
   reading "herdr". Ordinal only when no cwd resolves. This is D1.
4. **Tab renamed** — every agent pane inside it with no pane-scope override shows the new label at next
   resolve, no refresh cycle. This is D3.

## Accepted limitation (does NOT fully deliver the original request)

The agent's **display label** follows a tab rename. The agent's **addressable handle**
(`TerminalState::agent_name`) does not — it keeps its own uniqueness and charset rules
(`^[a-z][a-z0-9_-]{0,31}$`, plus server-wide conflict checks) that tab and workspace names are not subject to,
so `herdr agent send reviewer` keeps working after a tab rename. The user asked for the agent name to sync
with the window name; C delivers that for what is displayed, not for what is addressable. Accepted knowingly.

A tab override never clobbers a hand-set agent name: inheritance applies only where `agent_name_owner`
says the name was auto-assigned or detection-owned. Deliberate exception to uniformity; document it, do not hide it.

## Sub-decisions on C's own open questions

- **Persisted numeric tab overrides** (a `TabSnapshot.custom_name` of "3" is indistinguishable from a typed
  ordinal): **honor it as the override it is.** The alternative drops overrides equal to their own ordinal at
  restore, which deletes user data on a heuristic. Conservative option chosen; no migration.
- **Federation**: Policy C flagged this as unsized, not estimated. It is a real gap on this fork — pane and
  tab identity cross the federation wire. **A scoping pass runs before implementation** and must answer:
  does a local tab rename apply to a mounted remote tab's panes, and does `NameSource` survive the wire?
  Implementation must not begin until that answer exists.
- **Protocol version**: `TabInfo`/`WorkspaceInfo`/`PaneInfo` gain a `NameSource` field and rename params
  become nullable, so `src/protocol/wire.rs::PROTOCOL_VERSION` must be compared against the latest released
  tag per CLAUDE.md — bump only if source is not already ahead of the released protocol.
- **The contradicting test** `agent_rename_does_not_replace_the_pane_label` (src/app/api/agents.rs:699) keeps
  passing under C (stores stay independent; only resolution couples). Its stated intent now reads backwards,
  so it must be rewritten with an explicit comment or it becomes a misleading signal.

## Arbiter checklist

**Contradictions between proposals** — the only one found was the line-number divergence above, resolved by
verifying in-tree; it was a tree difference, not a disagreement about behavior.

**Unverified claims passed downstream** — none accepted as fact. Every structural claim in C used for
planning was re-verified in the worktree (table above). C's federation claims are explicitly unverified by
its own admission and are therefore quarantined behind the scoping pass rather than planned against.

**Unresolved questions carried forward** — (1) federation semantics, above; (2) whether the tab
path-suffix rule produces good names in deeply nested monorepos, which needs real-world eyeballing after
implementation; (3) C is refactor-risk by CLAUDE.md's definition, so characterization tests and
`assert_invariants_for_test` must land BEFORE any code moves; (4) C bundles D1 and D2 behind a refactor that
must land atomically — a mid-way abort leaves the tree worse than either narrow fix alone, so phase ordering
must keep the suite green at every step.
