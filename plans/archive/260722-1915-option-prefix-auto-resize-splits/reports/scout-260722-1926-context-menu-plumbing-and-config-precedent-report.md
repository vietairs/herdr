# Scout report — context menu plumbing + config/toggle precedent

Stage 2 (blindspot), scout 3 of 3. Read-only. Worktree:
`/Users/hvnguyen/Projects/herdr-worktrees/pane-auto-resize-splits`

## Findings

1. **Pane menu variants** — 4 arms at `src/app/state.rs:1266-1313`, keyed on two bools in
   `ContextMenuKind::Pane { ws_idx, tab_idx, pane_id, source_pane_id, has_manual_label }`
   (`src/app/state.rs:1211-1217`):
   - `source_pane_id: Some, has_manual_label: true` -> Rename / Clear name / Swap / Split right /
     Split down / Zoom / Close (7 items)
   - `Some, false` -> drops "Clear pane name"
   - `None, true` -> drops "Swap with focused pane"
   - `None, false` -> 5 items

2. **`items()` signature blocks runtime labels** — `pub fn items(&self) -> &'static [&'static str]`
   at `src/app/state.rs:1229`. Compile-time constant slices. 9 call sites:
   `state.rs:1216`, `menus.rs:300`, `modal.rs:699, 914, 1114, 1128, 2015, 2046, 2080`.

3. **Toggle precedent = Collapse/Expand** (`src/app/state.rs:1244-1264`). Label text stays a
   static literal; the *variant selected* changes based on a struct field (`collapsed: bool`).
   Two alternate arms, not a runtime-formatted suffix.

4. **Menu width is dynamic** — `context_menu_rect()` at `src/app/input/mouse.rs:1212-1225`:
   `max_item_w + 4`, clamped to `[14, screen.width]`. Longer labels widen the popover; no
   hard cap issue, but a long label on a narrow pane widens the menu noticeably.

5. **Right-click -> menu, end to end**:
   `mouse.rs:1075` Right button down (not in sidebar) -> `mouse.rs:1076` `pane_mouse_target()`
   -> `mouse.rs:1096-1101` build `ContextMenuKind::Pane` -> `mouse.rs:1095-1107` set
   `context_menu` + `Mode::ContextMenu` -> `state.rs:1229` `items()` -> `menus.rs:286`
   `render_context_menu()` (items at `:300`) -> `modal.rs:897` `handle_context_menu_key()`
   -> `modal.rs:693` `apply_context_menu_action()` matching `(kind, item)` pairs at `:699`.

6. **Config bool pattern** — `src/config/model.rs:31-47` (`UpdateConfig`):
   `#[derive(Debug, Clone, Copy, Deserialize)]` + `#[serde(default)]` on the struct, plain
   `pub field: bool`, explicit `impl Default` supplying the default. Doc comment above each
   field (see `SessionConfig` ~`model.rs:270`).

7. **Tests** — 30 `#[test]` in `modal.rs`. Context-menu ones:
   `context_menu_close_pane_last_parent_group_pane_keeps_confirmation_mode()` (`:1983`),
   `api_context_menu_close_tab_...` (`:2029`), `api_context_menu_close_pane_...` (`:2058`).
   They hand-construct `ContextMenuState`, call `items()`, assert side effects by INDEX.
   **Adding items shifts indices — these tests are index-sensitive and will need updating.**
   Also `src/app/state.rs:2835+` has menu-shape tests.

## Design implication (for the plan gate)

Adding a third bool (`auto_resize`) to `ContextMenuKind::Pane` and following the
Collapse/Expand precedent literally means **4 variants -> 8 hardcoded match arms**, each a
7-9 item literal slice. That is a DRY violation and a maintenance trap.

Recommended alternative for the plan: change `items()` to return `Vec<&'static str>` built
programmatically (push items conditionally). Labels stay `&'static str` — only the container
becomes owned. Costs 9 call-site updates (mostly `.get(idx).copied()` -> `.get(idx).copied()`
still works on a Vec; `.len()` unchanged), kills the combinatorial explosion, and makes both
new items trivial to insert. This is the KISS/DRY-correct move even though it touches more
call sites than the copy-the-precedent approach.

Note re index-sensitive tests: whichever approach, existing tests assert by numeric index, so
inserting items mid-list WILL break them. Prefer appending new items near the end (before or
after "Close pane") to minimise churn, and update the affected tests deliberately.

## Unresolved questions

- Toggle label wording: "Auto-resize splits" / "Auto-resize: on" / Collapse-Expand-style pair
  ("Enable auto-resize" / "Disable auto-resize")? Affects menu width (item 4).
- Where does the toggle live — per-tab, per-workspace, or global config? (Scout 2 covers the
  server/client ownership half; the persistence scope is still a product call.)

Status: DONE
Summary: Menu plumbing fully traced; `items()` static return type is the main structural
obstacle, and the literal Collapse/Expand precedent would cause a 4->8 match-arm explosion.
Concerns: existing context-menu tests assert by item INDEX and will break when items are added.
