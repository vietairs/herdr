# Group C: Rust UI conflicts (panes.rs, sidebar.rs, headless.rs)

## src/ui/panes.rs (2 hunks, both same pattern)

- Upstream: changed `stable_terminal_inner_rect(pane_inner)` to
  `stable_terminal_inner_rect(pane_inner, app.pane_scrollbars)` (new scrollbar-aware
  inner-rect calc), and simplified the resize guard to
  `!app.direct_attach_resize_locks.contains(terminal_id)`.
- Fork: added `host_owns_terminal_size()` helper that also checks
  `app.federation_owned_terminal_sizes` so a mounted federation controller's
  authoritative pane size isn't clobbered by the host's own resize pass, in
  addition to the pre-existing direct-attach lock.
- Resolution: kept upstream's new `stable_terminal_inner_rect(pane_inner,
  app.pane_scrollbars)` call signature, combined with the fork's
  `host_owns_terminal_size(app, terminal_id)` guard (superset of upstream's
  direct-attach-only check). Applied identically to both hunks (zoomed-pane path
  and multi-pane loop path). This is integration, not a pick — both sides'
  behavior is preserved.

## src/ui/sidebar.rs (1 hunk)

- Upstream: `display_name()` was demoted to `#[cfg(test)]`-only (see
  `src/workspace.rs:1164`) and replaced in production call sites by
  `display_name_from_terminals(&app.terminals)`, which resolves the *live*
  terminal cwd instead of the possibly-stale `identity_cwd`. Also inlined
  `ws.branch()` directly instead of using a pre-computed local.
- Fork: used `ws.display_name()` (no longer compiles outside tests) and
  `branch.as_deref()`, where `branch` is `workspace_branch_for_display(ws)`
  computed earlier in the same function (line 198) — this is federation-aware:
  it returns `None` for a federated workspace even though `ws.branch()` would
  return stale locally-cached git metadata that nothing populates remotely
  (see the doc comment at sidebar.rs:328-333, P8 requirement 3).
- Resolution: adopted upstream's `ws.display_name_from_terminals(&app.terminals)`
  for the label (required — `display_name()` no longer exists outside tests,
  and it's also a genuine behavior improvement). Kept the fork's
  `branch.as_deref()` (the already-computed federation-aware value), NOT
  upstream's inline `ws.branch()`, because using `ws.branch()` directly here
  would silently reintroduce showing stale/wrong branch info for a mounted
  federated workspace. Confirmed no other sidebar.rs conflict — the
  `workspace_federation_origin` / `federation_origin_badge` remote-agent
  helpers sit outside the conflicted region and were untouched by upstream's
  restructuring, so the remote-agent-in-sidebar path is intact.

## src/server/headless.rs (1 hunk)

- Upstream: replaced the fallback `_ => { self.app.handle_internal_event(ev);
  true }` arm with `_ => self.app.handle_internal_event_with_render_impact(ev)`
  — a new method (`src/app/api.rs:65`) that returns whether the event actually
  requires a re-render, instead of unconditionally returning `true`.
- Fork: added a `#[cfg(unix)] AppEvent::FederationMountFailed { .. }` match arm
  above the fallback, to forward a toast to clients when a federated mount
  attempt fails.
- Resolution: kept the fork's `FederationMountFailed` arm verbatim, and changed
  only the trailing fallback `_` arm to upstream's
  `handle_internal_event_with_render_impact(ev)`. No overlap — the fork's arm
  only intercepts one specific event variant.

## Verification

No conflict markers remain in any of the three files. `cargo check` was run
(ZIG=~/.local/zig-0.15.2/zig); no errors were reported against panes.rs,
sidebar.rs, or headless.rs specifically. Other files in the tree are still
mid-conflict (owned by other agents), so a full green build wasn't expected/
attempted here.

## Confidence

All three resolutions are integrations of genuinely complementary changes
(upstream infra improvement + fork federation behavior), not same-change
duplicates. No low-confidence spots identified — the sidebar.rs branch choice
is deliberate and backed by the existing doc comment explaining why
`workspace_branch_for_display` differs from `ws.branch()`.

Status: DONE
Summary: Resolved 4 conflict hunks across panes.rs/sidebar.rs/headless.rs by integrating upstream v0.8.0 improvements with fork federation behavior; no markers remain.
Concerns: none
