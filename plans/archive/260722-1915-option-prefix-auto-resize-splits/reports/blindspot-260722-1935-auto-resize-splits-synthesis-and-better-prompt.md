# Blindspot synthesis + better-prompt — auto-resize splits

Stage 2 output. Consolidates 3 parallel scouts + 2 main-loop verifications.
Feeds Stage 3 (predict) and Stage 4 (plan --tdd).

## What changed vs the route-card assumptions

Classification priced this as "net-new layout math + probable protocol work". Both were
wrong in the cheap direction — **most of the plumbing already exists**.

| Assumed at classify | Verified reality |
|---|---|
| Net-new geometry math | BSP tree, one `ratio: f32` per split (`src/layout.rs:73-81`). Balance = recursive ratio reset. |
| Need PTY resize plumbing | Automatic at render (`resize_tab_panes()`, `src/ui/panes.rs:166-211`). Nothing to write. |
| Need new protocol surface for ratios | **`Method::LayoutSetSplitRatio` already exists** — `handle_layout_set_split_ratio()`, `src/app/api/layouts.rs:218-248`. Also `Method::LayoutApply`. |
| Ratio changes may be invisible to events | They emit `LayoutUpdated` — `emit_layout_updated_event()` at `src/app/api/layouts.rs:246`. |
| Balance won't persist | It does: same handler calls `schedule_session_save()` (`:243`); ratios live in `TabSnapshot.layout`. |

## The one real defect this feature will expose

**Ratio changes do not resync mounted remote clients.** Verified directly:
`src/remote/federation/client.rs:824-829` gates resync on exactly six events —
`PaneCreated | PaneClosed | PaneMoved | TabCreated | TabClosed | TabMoved`.
`LayoutUpdated` is absent.

So balancing a federated workspace updates the host and leaves mounted clients rendering
stale ratios until an unrelated structural event forces a snapshot. Same family as this
fork's known remote-pane repaint gap. The persistent toggle amplifies it: ratio changes
become routine rather than user-initiated.

Fix is now concrete and one line at the trigger site — add `| EventKind::LayoutUpdated` at
`src/remote/federation/client.rs:829`. **Caveat that must be designed for:** manual resize
mode steps 0.05 per keypress (`src/app/actions.rs:1764`), so an un-debounced
LayoutUpdated trigger turns key-repeat into a snapshot request/response storm. Needs
debounce or coalescing — this is the single riskiest part of the change.

## Structural obstacle (menu)

`ContextMenuState::items() -> &'static [&'static str]` (`src/app/state.rs:1229`), 9 call
sites. The Collapse/Expand precedent (`src/app/state.rs:1244-1264`) selects between
hardcoded arms via a struct field. Adding a third bool to `ContextMenuKind::Pane` takes the
pane arms from 4 to 8. Recommend switching `items()` to build a `Vec<&'static str>`
(labels stay static, container becomes owned) — kills the explosion, costs 9 call-site
touches.

Existing context-menu tests assert by numeric INDEX (`modal.rs:1983, :2029, :2058`), so
inserting items breaks them by design.

## Open product decisions for the plan gate (Stage 5)

1. **Equal ratios vs equal areas.** All-0.5 on `Split(A, Split(B,C))` = 50/25/25. Equal area
   (33/33/33) needs leaf-count weighting. tmux ships both (`even-*` vs `tiled`).
2. **Balance scope** — focused pane's parent split, or the whole tab root?
3. **Toggle persistence scope** — global config bool (no migration, no protocol change,
   cheapest) vs per-tab (needs `SNAPSHOT_VERSION` 3->4, `src/persist/snapshot.rs:12`).
   No precedent exists for a server-owned persisted per-tab boolean (scout 2, finding 5).
4. **Federation**: fix the resync gap in this PR, or scope v1 to local workspaces and leave
   the gap documented?

## Better-prompt for Stage 4 (/hvn-plan --tdd)

> Add two items to herdr's pane right-click context menu: a one-shot "Balance splits" action
> and a persistent "Auto-resize splits" toggle that re-balances after every split and pane
> close.
>
> Reuse existing machinery — do NOT invent new geometry or protocol surface:
> - Balance = recursive reset of `Node::Split.ratio` in the BSP tree (`src/layout.rs:73-81`).
>   Rects and PTY sizes recompute automatically at render (`src/ui/panes.rs:166-211`).
> - Follow the shape of `handle_layout_set_split_ratio()` (`src/app/api/layouts.rs:218-248`):
>   mutate ratios, `schedule_session_save()`, `emit_layout_updated_event()`. Persistence and
>   eventing come free. Add a server-side API method for balance so the operation is
>   server-owned per CLAUDE.md's runtime/client guardrail; the menu item is the client's
>   thin trigger.
> - The lopsided-layout complaint originates in `remove_pane()` (`src/layout.rs:557-577`),
>   which collapses the tree but leaves ancestor ratios untouched. The toggle's close-hook
>   targets exactly this.
>
> Required design decisions to resolve in the plan: equal-ratio vs equal-area semantics;
> balance scope (parent split vs tab root); toggle persistence (global config bool vs
> per-tab needing SNAPSHOT_VERSION 3->4).
>
> Two hazards to plan explicitly:
> 1. `items()` returns `&'static [&'static str]` with 9 call sites; adding a third bool to
>    `ContextMenuKind::Pane` doubles the pane match arms 4->8. Prefer converting to a built
>    `Vec<&'static str>`.
> 2. Mounted remote clients do NOT resync on ratio changes — `client.rs:824-829` omits
>    `LayoutUpdated`. Adding it is one line but MUST be debounced, because manual resize
>    steps 0.05 per keypress and would otherwise cause a snapshot storm.
>
> TDD: existing context-menu tests assert by item index and will need updating. Cover
> balance math on asymmetric trees, idempotency (balance twice == once), the 0.1/0.9 ratio
> clamp interaction, toggle on/off behaviour across split and close, and session roundtrip.

## Unresolved questions

- Should the toggle also re-balance on manual pane resize (i.e. does it fight the user's
  explicit drag/keyboard resize)? Strong candidate for "no", but it needs an explicit
  decision or the feature will feel broken.
- Does the balance action apply to a zoomed pane state, or is it a no-op while zoomed?
