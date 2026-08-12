# Diagnosis: remote agents show no cloud icon in agents panel

Worktree: federation-ui-labels @ a021433a. Read-only investigation.

## Root cause (confirmed, file:line)

The agent-panel entry projection drops the remote/federation badge that the
workspace-list projection applies. Two parallel label-building call sites,
one calls the badge helper, the other doesn't.

**Workspace row (sidebar list) — DOES apply the cloud badge:**
`src/ui/sidebar.rs:196-208` (`workspace_row_height`) and `:1314`
(`render_workspace_list`) both build the label then call:
```
let label = workspace_display_label_with_origin_badge(ws, label);
```
`workspace_display_label_with_origin_badge` (`src/ui/sidebar.rs:313-321`) calls
`federation_origin_badge` (`src/ui/sidebar.rs:305-307`):
```rust
pub(crate) fn federation_origin_badge(ws: &crate::workspace::Workspace) -> Option<String> {
    workspace_federation_origin(ws).map(|host_key| format!("\u{2601} {}", host_key.as_str()))
}
```
Glyph = `\u{2601}` (☁ U+2601 CLOUD) prefixed to the host key, e.g. `☁ appn-ltu-vm…`.
`workspace_federation_origin` (`sidebar.rs:294-301`) classifies remoteness from
`ws.id` via `crate::remote::federation::id::classify` — unspoofable, derived
only from the workspace id the mount code assigned.

**Agent-panel entry (agents panel) — does NOT apply it:**
`src/ui/sidebar.rs:136-183` (`collect_agent_panel_entries_with_runtimes`):
```rust
let workspace_label = ws.display_name_from(&app.terminals, terminal_runtimes);  // line 154
...
AgentPanelEntry {
    ...
    primary_label: workspace_label.clone(),   // line 167 — raw label, no badge call
    ...
}
```
This never calls `workspace_display_label_with_origin_badge`. `AgentPanelEntry`
(`sidebar.rs:23-40`) also has no `is_remote`/badge field at all.

The rendered row then consumes `entry.primary_label` verbatim as the workspace
token: `src/ui/sidebar/tokens.rs:57` —
`Some(ResolvedTokenKind::Workspace(entry.primary_label.clone()))`. No badge
text is ever present to render.

## Answering the 5 questions

1. Glyph: `\u{2601}` (☁), defined in `federation_origin_badge`
   (`src/ui/sidebar.rs:305-307`), applied to workspace rows at
   `sidebar.rs:208` and `sidebar.rs:1314`.
2. Agent row render: `render_agent_detail` (`src/ui/sidebar.rs:1458-1575`),
   pulling entries from `agent_panel_entries_from` →
   `collect_agent_panel_entries_with_runtimes` (`sidebar.rs:136-183`); actual
   token→span text comes from `resolved_agent_rows` (`sidebar.rs:597`) and
   `tokens.rs`.
3. Remoteness IS reachable at the render site: `render_agent_detail` has
   `app: &AppState` in scope and each `detail.ws_idx` (`sidebar.rs:1529` already
   uses `detail.ws_idx` to index `app.workspaces`), so
   `app.workspaces.get(detail.ws_idx)` → `federation_origin_badge(ws)` is a
   one-line lookup from render code. But by the time execution reaches render,
   the *entry construction* step has already thrown the badge away — `entry`
   only carries a flat `String primary_label`, no `ws_idx`-keyed
   re-derivation happens in the render loop, and `AgentPanelEntry` has no
   remote flag to check even if it wanted to skip the `app.workspaces` lookup.
4. Cause distinguished: this is **not** a pure render-time omission (case A)
   and **not** a grouping/keying loss (case C — grouping by tab/pane is
   unaffected, `ws_idx` survives fine, see item 3). It is case B: the
   projection function that builds `AgentPanelEntry` (`collect_agent_panel_entries_with_runtimes`,
   `sidebar.rs:136-183`) computes the label via `ws.display_name_from(...)`
   directly instead of the shared `workspace_display_label_with_origin_badge`
   helper the workspace-row path uses — the remote badge is dropped during
   projection, not during drawing.
5. Agent NAME (`APPNltu_s…`) — same underlying helper, different root value,
   not proven to the same depth. `display_name_from` /
   `display_name_from_terminals` (`src/workspace.rs:1173-1203`) all call
   `automatic_display_name_for_cwd(cwd)` (`workspace.rs:1205-1211`), which for
   a fresh/cache-mismatched cwd falls through to `fallback_label_from_cwd(cwd)`
   (`workspace.rs:1209`, imported at `workspace.rs:28`). Local and federated
   workspaces run through the *same* naming function — there is no separate
   "remote name source." The mangled look plausibly comes from what
   `identity_cwd`/`cached_auto_label` get set to for a federated workspace at
   mount time (`workspace.rs:1340-1342`, `cached_auto_label:
   fallback_label_from_cwd(&identity_cwd)`), i.e. `fallback_label_from_cwd` is
   fed a synthesized remote identifier instead of a real local path. I did
   NOT trace the exact federation mount code that sets `identity_cwd` for a
   mounted workspace to confirm what string it uses — this part is
   **plausible, unconfirmed**. It is a separate bug from the missing badge
   (item 3/4 root cause) even though both surface in the same screenshot.

## Ranked hypotheses

1. **[CONFIRMED]** Agent-panel projection (`collect_agent_panel_entries_with_runtimes`,
   `sidebar.rs:136-183`) never calls `workspace_display_label_with_origin_badge`,
   so `AgentPanelEntry.primary_label` never carries the ☁ badge that the
   workspace-list projection adds. Render (`render_agent_detail`) just prints
   whatever `primary_label` says — nothing to draw.
2. [ELIMINATED] "Remoteness unreachable at render site" — false; `app` and
   `ws_idx` are both present at the render call site (`sidebar.rs:1529`),
   proving the flag *could* be looked up there too, but the current
   architecture puts label formatting upstream in projection, not render.
3. [ELIMINATED] "Lost during grouping/keying" — grouping is by `ws_idx`/`tab_idx`/`pane_id`,
   unaffected by remoteness; no evidence of drops during
   `agent_view::apply_agent_view` (would need separate check if this surfaces
   later, but the badge is already absent before that stage since
   `collect_agent_panel_entries_with_runtimes` never attaches it).
4. [OPEN, unconfirmed] mangled agent/workspace NAME for remote agents — same
   naming function as local, likely fed a synthesized `identity_cwd` at
   federation-mount workspace creation. Not traced to the exact mount code
   that sets it.

## Fix-side classification (per CLAUDE.md runtime/client boundary)

Pure TUI-side fix. `federation_origin_badge`/`workspace_display_label_with_origin_badge`
already exist and are unspoofable-by-design (derived only from `ws.id`, a
client-trusted federation-assigned value — see the RT-F8 doc comment at
`sidebar.rs:286-293`). No new server/protocol state is needed: the fix is
threading the existing helper into `collect_agent_panel_entries_with_runtimes`'s
label construction (or adding a badge field to `AgentPanelEntry` and drawing
it), all inside `src/ui/sidebar.rs`. This is glyph/label placement —
presentation state — per CLAUDE.md's TUI-only category, not a runtime/server
concern.

## Unresolved questions

- Exact federation-mount code path that sets a mounted workspace's
  `identity_cwd` (source of the mangled `APPNltu_s…` name) — not located in
  this pass; would need a grep in `src/remote/federation/` mount/reducer code
  or `src/app/api/workspaces.rs` for where a federated `Workspace` is
  constructed.
- Whether `apply_agent_view` (`crate::app::agent_view::apply_agent_view`,
  called at `sidebar.rs:132`) does anything with `primary_label` that would
  also need adjustment once the badge is added upstream — not inspected.
