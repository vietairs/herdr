# implementation plan — Policy C name resolver

**Branch:** `fix/name-sync-workspace-tab-agent` (worktree `/Users/hvnguyen/Projects/worktrees/herdr-name-sync`, on origin/master `c2f4166a`)
**Locked decision:** `reports/design-decision.md` (Policy C). **Federation contract:** `reports/federation-scope.md` (FNC v1). **Adversarial pass:** `reports/predict-260910-policy-c.md`.

All line numbers are this worktree's, re-verified by opening each file. `pipeline.md` cites a 107-commit-older checkout; ignore its numbers.

---

## 0. Rulings this plan makes (must not be re-litigated during implementation)

Policy C is locked. These are the four gaps Policy C's own text leaves open; each is decided here, with the evidence, and each must be restated in the `naming.rs` module doc so the next reader does not "fix" it.

**R1 — Inheritance is tab → pane/agent ONLY. A workspace rename never renames its tabs.**
Rung 1's wording ("inherited from the nearest enclosing renamed scope") read literally would make every tab in a renamed workspace show the workspace name. Three pieces of evidence say no: (a) design-decision §3 defines the tab default as cwd derivation with ordinal fallback and never mentions the workspace name; (b) `Workspace::test_new` (src/workspace.rs:1231) sets `custom_name: Some(name)` on *every* test workspace, so the literal reading breaks the suite wholesale; (c) N sibling tabs all rendering one identical string is strictly worse than N ordinals, which are at least injective. This resolves predict's unresolved question #1.

**R2 — Pane scope has no rung 3 (no cwd derivation), and pane-border precedence is NOT reordered.**
`TerminalState::border_label` (src/terminal/state.rs:2140) today resolves `effective_title()` → `manual_label` → agent labels. `effective_title` is the hook/detection-owned presentation title from `src/terminal/metadata.rs:295` — it is part of the agent-identity authority chain that design-decision's rung 2 explicitly delegates ("has its own hook-over-detection authority chain"), not a competing user override. Reordering it is a visible behavior change outside Policy C's scope. The pane ladder is therefore: `effective_title` → `manual_label` (rung 1 own) → **inherited tab override (rung 1 inherited, new)** → agent identity (rung 2) → `None`. Resolves predict question #2.

**R3 — D3's non-clobber gate needs a new field; `agent_name_owner` cannot express it.**
Verified: `AgentNameOwner` (src/terminal/state.rs:103-107) is `{ agent_label, session_ref }`, a record of *which agent identity owns the name* so `reconcile_agent_name_owner` can clear it — it carries no authorship bit, and the user-rename path (src/app/agents.rs:131), `begin_managed_agent` (state.rs:1918) and `restore_managed_agent` (state.rs:2049) all funnel through the same `set_agent_name` (state.rs:1883-1908). Adding `AgentNameAuthor::{User, Managed, Detected}` (Phase 1). Shipping design-decision's sentence unchanged would either clobber hand-set handles or make D3 a no-op.

**R4 — Rung 3 is a per-scope *input*, not a per-scope *branch*.**
`resolve_name` takes `derived: Option<&str>` already computed by the caller from a cached scalar. It contains no `match scope` for rung 3. Workspace keeps `automatic_workspace_label` (src/workspace/git/discovery.rs:66); tab gets a new relative-suffix derivation computed on the git pass; pane supplies `None` (R2). This is A1's mitigation and it is also what keeps P1's syscalls out of the render path.

---

## 1. Version decisions (checked against the latest released fork tag `v0.9.0-hvn.1`)

| Constant | Source `c2f4166a` | `v0.9.0-hvn.1` | Decision |
|---|---|---|---|
| `PROTOCOL_VERSION` (src/protocol/wire.rs:20) | 23 | 23 | **Bump to 24** in Phase 5 |
| `FEDERATION_PROTOCOL_VERSION` (src/remote/federation/protocol/mod.rs:93) | 7 | 7 | **Stays 7** |
| `SNAPSHOT_VERSION` (src/persist/snapshot.rs:12) | 3 | 3 | **Stays 3** |

- **Client 23 → 24 is required, not optional.** Source equals the released value, so CLAUDE.md's "bump only if source is not already ahead" fires. `ClientShellWorkspace.custom_label` (wire.rs:1063) and `ClientShellTab.custom_label` (wire.rs:1096) are replaced by `name_source`, and `ClientShellTab`/`ClientShellWorkspace` are **positionally** bincode-encoded (the `federation_origin` doc comment at wire.rs:1071-1079 states this and forbids `skip_serializing_if` for that reason) — a hard wire break. Update `check_client_version` fixtures (wire.rs:3171/3178/3187, autodetect.rs:609) and regenerate `docs/next/api/herdr-api.schema.json`.
- **Federation stays 7.** `name_source` is an additive `#[serde(default)]` field on `TabInfo`/`WorkspaceInfo`/`PaneInfo`, types already carried inside `MountSnapshot`, not a new `FederationMessage` variant. `negotiate()`/`codec::decode` hard-reject any version mismatch, so 7 → 8 would break every mount against a deployed `v0.9.0-hvn.1` host for a field that degrades gracefully. Precedent for the additive case is `AgentStatusMessage.agent` (protocol/mod.rs:338-350). A test asserts the constant is unchanged, with this reasoning in its comment.
- **Snapshot stays 3.** `parse_snapshot` (snapshot.rs:501-509) and `parse_history_snapshot` (:512-521) **hard error** on a newer version — a downgrade loses the whole session rather than degrading, and this fork ships per-host binary swaps routinely. Every new persisted field is `#[serde(default)]`. Nothing Policy C persists is a removal.

---

## 2. Verified call-site inventory (Policy C's counts were claims; these are the greps)

`tab_display_name` — **6 production + 1 internal + 2 test**, not 8:
production `src/app/actions.rs:228`, `src/app/creation.rs:236`, `src/app/api/plugins/context.rs:248`, `:309`, `:335`, `src/app/window_title.rs:68` (via `active_tab_display_name`); internal `src/workspace.rs:475`; tests `src/workspace.rs:1695`, `src/app/api/layouts.rs:814`.

`display_name` family — **14 production**, not ~10:
`display_name_from` (workspace.rs:1085): `creation.rs:411`, `api.rs:686`, `api.rs:984`, `server/headless.rs:3387`, `server/notifications.rs:48`, `server/headless/notifications.rs:108`, `:183` (7).
`display_name_from_terminals` (workspace.rs:1067): `actions.rs:2118`, `window_title.rs:63` (2).
`display_name` (workspace.rs:1059, `#[cfg(test)]`-gated read of `identity_cwd`): production callers `actions.rs:2715`, `:2733`, `:2757`, `:2785`, `api/workspaces.rs:1926` (5). *Note: `display_name` is annotated `#[cfg(test)]` at :1058 but has 5 non-test callers — verify the cfg boundary before touching it.*

`manual_label` — **44 total references**, of which production reads/writes are: `creation.rs:355`, `:744`, `window_title.rs:75`, `api/panes.rs:1785-1786`, `api/layouts.rs:372`, `:536`, `api/plugins/panes.rs:39`, `:284`, `terminal/state.rs:132/167/1874-1880/2142`, `server/client_shell.rs:471`, `persist/snapshot.rs:379`, `persist/restore.rs:541`, `:638`, `remote/federation/client.rs:1423`, `client/shell/context_menu.rs:209` (via `pane.label`).

`custom_label` — **2 producers** (`client_shell.rs:64`, `:108`), **2 wire fields** (wire.rs:1063, :1096), **8 TUI consumers**: `client/shell/tabs.rs:107`, `:112`, `mobile.rs:663`, `:776`, `:886`, `agent_sidebar.rs:264`, `overlay_input.rs:413`, `context_menu.rs:416`, plus `sidebar.rs:610`.

`border_label` — **1 production caller**: `src/ui/panes.rs:648`.

---

## Phase 0 — characterization tests ONLY (must pass on the UNCHANGED tree)

No production file is touched in this phase. Every test below asserts *today's* behavior, including behavior Policy C will invert — those are marked **[inverts]** and are rewritten in the phase named.

### `src/workspace.rs` `mod tests`
| Test fn | Pins | Fate |
|---|---|---|
| `char_tab_display_name_uses_index_not_public_tab_number` | `tab_display_name` (:478) returns `tab_idx + 1` even when `tab.number` differs — build the moved-tab state from `moving_tab_keeps_active_identity_and_stable_tab_numbers` where numbers are `[2,3,1]` and labels are `["foo","2","3"]` | **[inverts]** Phase 4 |
| `char_workspace_name_ladder_override_then_cached_auto_then_fallback` | `display_name`/`display_name_from_terminals`/`display_name_from` (:1059/:1067/:1085) all short-circuit on `custom_name`, then `automatic_display_name_for_cwd` (:1099) returns `cached_auto_label` only when `cwd == cached_identity_cwd`, else `fallback_label_from_cwd` | keeps |
| `char_test_new_workspace_is_a_renamed_workspace` | `Workspace::test_new` sets `custom_name: Some(name)` (:1231) and `identity_cwd = current_dir()` (:1210) — the M3 tripwire, made explicit so R1 is not silently reversed | keeps |
| `char_public_tab_numbers_diverge_from_indexes_after_move` | `move_tab` leaves `tab.number` stable while indexes shift (:1704-1706) | keeps |

### `src/workspace/tab.rs` `mod tests`
| Test fn | Pins |
|---|---|
| `char_is_auto_named_is_exactly_custom_name_is_none` | tab.rs:200 |
| `char_cwd_for_pane_prefers_runtime_cwd_over_terminal_cwd` | tab.rs:532-547 — the function the resolver is forbidden to call |

### `src/app/mod.rs` `mod tests`
| Test fn | Pins | Fate |
|---|---|---|
| `char_tab_info_label_equals_tab_display_name_for_every_tab` | W×T sweep over 2 workspaces × 3 tabs incl. one moved tab: `app.tab_info(w,t).label == ws.tab_display_name(t)` for all pairs. Guards `creation.rs:236` | **[inverts]** Phase 4 |
| `char_workspace_info_label_equals_display_name_from` | `creation.rs:411` | keeps |

### `src/server/client_shell.rs` `mod tests`
| Test fn | Pins | Fate |
|---|---|---|
| `char_client_shell_tabs_pair_with_their_own_workspace_tab` | **M2** — the unguarded positional zip at :90-99. 2 workspaces × 2 tabs, one renamed; assert the `(tab_id, label, custom_label, zoomed)` tuples pairwise against `AppState`, not just count | keeps, and it is the safety net for every later phase |
| `char_custom_label_is_a_one_bit_override_flag` | `custom_label = custom_name.is_some()` (:64) and `!is_auto_named()` (:108) | **[inverts]** Phase 5 |
| `char_custom_label_is_true_for_every_mounted_scope` | **F2** — a fixture-mounted workspace/tab reports `custom_label: true` although nobody renamed it. Pins the accident so Phase 5's fix is visible in the diff | **[inverts]** Phase 5 |
| `char_popup_pane_title_is_manual_label_only` | :471 — the legitimately scope-less resolve | keeps (asserts the degrade in Phase 4) |

### `src/terminal/state.rs` `mod tests`
| Test fn | Pins | Fate |
|---|---|---|
| `char_border_label_precedence_title_then_manual_then_agent` | :2140 — `effective_title` outranks `manual_label`, which outranks agent labels. Extends the existing `border_label_prefers_manual_label_over_agent_label` (:3943) with the title case it never covered. **R2 depends on this being pinned first** | keeps |
| `char_agent_name_author_is_indistinguishable_today` | **M1** — set `agent_name` via the user path (`set_agent_name`) and via `begin_managed_agent`, assert the two resulting states are observationally identical. `agent_name_owner` is private, so assert via `reconcile_agent_name_owner` behaviour rather than field access | **[inverts]** Phase 1 |

### `src/app/creation.rs` `mod tests` (beside the existing fixture mounts at :3115, :3196, :3225)
| Test fn | Pins | Fate |
|---|---|---|
| `char_mounted_scope_labels_come_from_the_remote_snapshot` | mounted workspace/tab/pane resolved labels equal the remote strings, and today they live in `custom_name` (:562-563) / `manual_label` (:744) | **[inverts]** Phase 1 dual-write, Phase 4 single-write |
| `char_local_rename_of_a_mounted_tab_emits_no_federation_frame` | **FNC-4** — `handle_tab_rename` (api/tabs.rs:315) has no federation branch; assert only the local mirror changed and no frame was queued | keeps |
| `char_resync_tab_label_lands_in_custom_name` | :1713-1717 via `RemoteTabRef.label` | **[inverts]** Phase 4 |

### `src/persist/` `mod tests`
| Test fn | Pins |
|---|---|
| `char_snapshot_round_trip_preserves_overrides_and_ordinals` | workspace `custom_name`, tab `custom_name` **including the literal `"3"`** (the sub-decision: honor a numeric override as an override), `public_tab_numbers`, pane `label`, `agent_name`; restore then `AppState::assert_invariants_for_test()` |
| `char_ordinals_survive_a_restore_that_drops_a_tab` | restore.rs:357 indexes `snap.public_tab_numbers` against `snap.tabs`; the drop path (:373, :690-700) does not shift `idx` |
| `char_federation_materialized_workspaces_are_excluded_from_capture` | snapshot.rs:256 / :308-317 |

### `src/app/state.rs` `mod tests`
| Test fn | Pins |
|---|---|
| `char_adversarial_identity_state_passes_invariants_today` | `AppState::test_with_adversarial_identity_state()` + `assert_invariants_for_test()`, and `Workspace::test_adversarial_identity_state()` + `Workspace::assert_invariants_for_test()`. Baseline before Phase 1 extends the fixture (M5) |

### `src/protocol/wire.rs` `mod tests`
| Test fn | Pins | Fate |
|---|---|---|
| `char_client_shell_structs_round_trip_at_protocol_23` | `PROTOCOL_VERSION == 23`; `ClientShellWorkspace`/`ClientShellTab` bincode round-trip with `custom_label` | **[inverts]** Phase 5 |

**Phase 0 exit gate:** `cargo nextest run` green on the unchanged tree, zero production-file diff (`git diff --stat -- src | grep -v tests` empty except `#[cfg(test)] mod tests` blocks).

---

## Phase 1 — state shape and fixtures (no behavior change; nothing reads the new fields yet)

**Files:** `src/workspace.rs`, `src/workspace/tab.rs`, `src/terminal/state.rs`, `src/app/creation.rs`, `src/remote/federation/client.rs`, `src/persist/snapshot.rs`, `src/persist/restore.rs`, `src/app/agents.rs`.

1. **`Tab`** (tab.rs:38) gains `cached_auto_label: Option<String>` and `cached_auto_label_cwd: Option<PathBuf>`. Both default `None`, never persisted, never read this phase.
2. **`Workspace`** gains `mirrored_name: Option<String>`; **`Tab`** gains `mirrored_name: Option<String>`; **`TerminalState`** gains `mirrored_label: Option<String>` (**FNC-3**). Not persisted (`#[serde(skip)]` where a struct is serialized).
3. **Federation dual-write.** `creation.rs:555`, `:562`, `:563`, `:611`, `:744`, `:1713-1717` and `client.rs:1423` write the remote label into **both** `mirrored_*` and today's `custom_name`/`manual_label`. Dual-write is deliberate and temporary: it keeps Phase 1 visually identical (dropping the `custom_name` write before the resolver reads rung 1.5 would make mounted workspaces snap to locally-derived names mid-phase — the `cached_auto_label` hazard). Phase 4 removes the `custom_name`/`manual_label` half. Mark each site `// dual-write: removed in the call-site migration phase`.
4. **`AgentNameAuthor`** (**R3/M1**): `pub enum AgentNameAuthor { User, Managed, Detected }` in `src/terminal/state.rs`; `TerminalState::agent_name_author: Option<AgentNameAuthor>`. `set_agent_name` takes it as a parameter; the three callers pass `User` (app/agents.rs:131), `Managed` (state.rs:1918), `Managed` (state.rs:2049 restore). Persist it on `PaneSnapshot` (snapshot.rs:103 area) as `#[serde(default)]` beside `agent_name`, and restore it (restore.rs:541/:638 region) — a `None` on a legacy snapshot means "unknown", treated as `Managed` (safe direction: unknown handles are *not* protected from inheritance is the wrong default, so **unknown means protected** → treat `None` as `User`). Record this choice in the enum doc.
5. **Extend `Workspace::test_adversarial_identity_state`** (workspace.rs:1289) per **M5**: the current fixture's only named tab (`test_add_tab(Some("removed"))`, :1298) is closed at :1303, so every surviving tab is unnamed and the fixture would pass Policy C trivially. Add (a) a *surviving* renamed tab whose `number` differs from its index, (b) a pane whose terminal carries a hand-set `agent_name` with `AgentNameAuthor::User`, (c) a mirrored-name tab carrying a `IdClass::Remote` id.
6. **Extend `Workspace::assert_invariants_for_test`** (workspace.rs:1323) with the shape invariants only: a scope carrying `mirrored_name` classifies `IdClass::Remote` via `remote::federation::id::classify`; a scope that does not classify Remote has `mirrored_name == None`. (The resolution invariant — "no two auto-named tabs in one workspace resolve to the same string" — needs the resolver and lands in Phase 4.)

**Rewrites here:** `char_agent_name_author_is_indistinguishable_today` → `agent_name_author_distinguishes_user_rename_from_managed_launch`, asserting the User/Managed split and that a legacy `None` restores as `User`.

**Exit gate:** full suite green; `git diff` shows no change to any resolved name anywhere.

---

## Phase 2 — cache feed: per-tab derived labels on the existing git pass

**Files:** `src/app/git_refresh.rs`, `src/app/actions.rs`, `src/workspace/git/status.rs`, `src/workspace/git/discovery.rs`.

Sized as its own phase because it changes four coupled types together (predict P3 secondary note): `WorkspaceGitRefreshItem` (git_refresh.rs:114-133) carries `workspace_id` / `resolved_identity_cwd` / `cache_key_hint` and has no tab dimension at all.

1. `WorkspaceGitRefreshItem` gains `tabs: Vec<(TabNumber, PathBuf)>` — the tab's root-pane cwd, resolved **once here**, on the ~1.5 s pass, using `cwd_for_pane`. This is the *only* place `cwd_for_pane` may be called for naming.
2. `WorkspaceGitStatus` gains `tab_auto_labels: Vec<(TabNumber, Option<String>)>`. Derivation rule (D1): if the tab cwd is a descendant of the workspace's resolved identity cwd, the path suffix relative to it; if equal, `None`; otherwise `automatic_workspace_label` of the tab's own git root (a tab that `cd`'d into another repo joins nothing — U1's "join" premise only holds inside one root).
3. **Distinctness pass (U1).** After deriving all tab labels for a workspace, any label colliding with a sibling is set to `None` for *all* colliding tabs, so they fall to rung 4 ordinals. Ordinals are ugly but injective; two identically-named tabs are strictly worse. Implemented in the pure derivation function so it is testable without PTYs.
4. `apply_workspace_git_statuses` (actions.rs:1526) writes `tab.cached_auto_label` / `cached_auto_label_cwd`, matching on `tab.number` (never index).
5. **D2 gate fix.** actions.rs:1555 currently reads `changed |= ws.custom_name.is_none();` — extend to also flag when any tab's cached label changed and that tab is auto-named. Note :1552-1553 already refreshes `cached_auto_label` unconditionally; only the redraw signal is gated. That is design-decision §2's fix and it needs no other change.
6. **Third demand flag (P3).** `GitStatusRefreshDemand` gains `auto_names: bool`, set by `git_refresh_demand` (git_refresh.rs:102-112) whenever any workspace or tab is auto-named. Without it, `git_refresh_deadline` (:95) returns `None` for a user whose sidebar config contains neither `Branch` nor `GitStatus`, and every derived name is permanently stale.

**New tests** (`src/app/git_refresh.rs` and `src/app/actions.rs` `mod tests`, all PTY-free):
`tab_auto_label_is_the_suffix_relative_to_the_workspace_root`, `tab_auto_label_is_none_when_the_tab_cwd_equals_the_workspace_root`, `colliding_sibling_tab_labels_all_fall_back_to_none`, `tab_in_a_foreign_repo_derives_from_its_own_git_root`, `auto_named_scopes_keep_the_git_pass_alive_without_sidebar_tokens`, `apply_workspace_git_statuses_matches_tabs_by_public_number_not_index`.

**Exit gate:** full suite green. Nothing reads `cached_auto_label` for display yet, so no visible name changes.

---

## Phase 3 — the resolver module (`src/workspace/naming.rs`), unwired

New file beside `src/workspace/aggregate.rs`. Target: under 150 lines plus tests.

```rust
pub enum NameScope { Workspace, Tab, Pane }

pub enum NameSource { Override, Inherited, Mirrored, AgentIdentity, Cwd, Ordinal }

pub struct NameSources<'a> {
    pub own_override:       Option<&'a str>,  // rung 1, set at this scope
    pub inherited_override: Option<&'a str>,  // rung 1, tab -> pane/agent only (R1)
    pub mirrored:           Option<&'a str>,  // rung 1.5, remote scopes only (FNC-2)
    pub agent_identity:     Option<&'a str>,  // rung 2
    pub derived:            Option<&'a str>,  // rung 3, caller-supplied cached scalar (R4)
    pub ordinal:            Option<usize>,    // rung 4, public_tab_number — never tab_idx + 1
}

pub struct ResolvedName<'a> { pub text: Cow<'a, str>, pub source: NameSource }

pub fn resolve_name(scope: NameScope, sources: NameSources<'_>) -> Option<ResolvedName<'_>>;
```

- `Cow` because the snapshot is built unconditionally every tick and only *then* compared (`server/headless/render.rs:436`), so a rung-1 or rung-2 hit must allocate nothing new (**P2**).
- No `match scope` for rung 3 (**R4/A1**). `scope` is used only to decide whether `inherited_override` and `ordinal` are legal inputs, and is `debug_assert`-ed against them.
- No `unwrap()`; every fallthrough returns `None` and the caller supplies the scope's literal fallback (`"workspace"` for a workspace, `"popup"` for a popup pane, `None` for a pane border).

**Hard architecture rule (P1), enforced by a test in this module:** `src/workspace/naming.rs` must contain none of the strings `cwd_for_pane`, `process_cwd`, `resolved_identity_cwd_from`, `display_name_from`, `std::fs`, `Instant::now`. Test `naming_module_contains_no_io_or_derivation_calls` reads its own source via `include_str!("naming.rs")` and asserts. A grep test rather than a wall-clock bench is what CLAUDE.md says it prefers.

**Unit tests in `src/workspace/naming.rs`:**
`rung_1_own_override_wins_over_everything`, `rung_1_inherited_override_is_ignored_at_workspace_scope`, `rung_1_5_mirrored_stops_the_ladder_before_agent_identity`, `mirrored_never_outranks_a_local_override` (FNC-4: a local rename of a mounted scope still wins), `rung_2_agent_identity_beats_derived`, `rung_3_derived_beats_ordinal`, `rung_4_uses_the_supplied_ordinal_verbatim`, `pane_scope_rejects_an_ordinal_input`, `resolve_returns_borrowed_text_for_rung_1_and_2` (asserts `matches!(resolved.text, Cow::Borrowed(_))`).

**Exit gate:** full suite green; module compiles and is unreferenced outside its own tests.

---

## Phase 4 — call-site migration (the behavior change lands here)

**Files:** `src/workspace.rs`, `src/workspace/tab.rs`, `src/terminal/state.rs`, `src/ui/panes.rs`, `src/app/creation.rs`, `src/app/actions.rs`, `src/app/window_title.rs`, `src/app/api.rs`, `src/app/api/plugins/context.rs`, `src/server/headless.rs`, `src/server/notifications.rs`, `src/server/headless/notifications.rs`, `src/server/client_shell.rs`, `src/remote/federation/client.rs`.

1. `Workspace::display_name_from*` and `display_name` become thin wrappers that assemble `NameSources` and call `resolve_name`. Rung 3 input is `cached_auto_label` gated on `cwd == cached_identity_cwd`, preserving `automatic_display_name_for_cwd`'s existing semantics. All 14 call sites keep their signatures; only `creation.rs:411` additionally captures the `NameSource` for Phase 5.
2. `Workspace::tab_display_name` (workspace.rs:478) becomes a resolver call. **`tab_idx + 1` is replaced by `public_tab_number(tab_idx)` (workspace.rs:1028).** This is the rung-4 drift bug Policy C exists to kill, and it is the single most visible change in the whole plan.
3. **FNC-1/FNC-2 short-circuit.** Remote-ness is decided by `remote::federation::id::classify(&ws.id)` — the same non-spoofable check `workspace_info` already uses for `federation_origin` (creation.rs:437-443), keyed off this client's trusted `HostKey`, never off anything the remote sends. For a Remote-classified scope, rungs 2–4 inputs are passed as `None` and only `own_override` + `mirrored` are populated.
4. **Remove the Phase 1 dual-write.** `creation.rs:555/:562/:563/:611/:744/:1713-1717` and `client.rs:1423` now write `mirrored_*` only. `custom_name`/`manual_label` mean exactly Policy C's rung 1 at every scope.
5. **D3 inheritance.** `TerminalState::border_label` gains an `inherited: Option<&str>` parameter (**not** an `&AppState`, **not** a lookup — `src/ui/panes.rs:648` already holds `&AppState` and `&Workspace` and supplies it from `ws.active_tab`). Inheritance applies only when `agent_name_author != Some(User)` (**R3**) and the pane has no `manual_label`.
6. `App::pane_info` (creation.rs:319, resolves `tab_idx` itself at :327) and `App::agent_info` (agents.rs:366, delegates) get full context and need no degraded path. The popup pane title (`client_shell.rs:471`) **degrades to the pane-only ladder** — pane override → agent identity → `"popup"` — and never inherits a tab override; a `PopupPaneState` (app/state.rs:18) belongs to no tab. Document the degrade in `naming.rs`, not as an oversight at the call site.
7. **Extend `Workspace::assert_invariants_for_test`** with the resolution invariant: no two auto-named tabs in one workspace resolve to the same string.

### Existing tests rewritten in this phase

| Test | File:line | Today | Becomes |
|---|---|---|---|
| `moving_tab_keeps_active_identity_and_stable_tab_numbers` | workspace.rs:1685 | `labels == ["foo","2","3"]` | `labels == ["foo","3","1"]` — tab numbers after the move are `[2,3,1]` (:1704-1706), so rung 4 now reports the *public* number. Add a comment naming the drift this fixes. Tabs have no cached auto label in this fixture, so rung 3 does not fire |
| *(unnamed)* tab-info assertion | app/mod.rs:2523 | `assert_eq!(tab.label, "2")` | `assert_eq!(tab.label, "3")` — the same test already asserts `tab.number == 3` two lines above (:2522), so today's `"2"` was the bug in plain sight |
| *(unnamed)* plugin context | app/api/plugins/mod.rs:3179 | `tab_label == Some("1")` | Verify at implementation. The workspace cwd is `/tmp/issue` (:3177); if the tab carries no `cached_auto_label` in that fixture the assertion is unchanged, otherwise it becomes `Some("issue")`. Do not guess — run it and pin whichever it is, with a comment saying which rung produced it |
| `agent_rename_does_not_replace_the_pane_label` | app/api/agents.rs:701 | asserts `manual_label` survives an agent rename | **Rename to `agent_rename_leaves_manual_pane_label_untouched`.** It still passes — stores stay independent — but its old name asserts a decoupling Policy C deliberately ends at resolve time. Add: `/// Store vs resolution: an agent rename writes `agent_name` and never touches `manual_label`, which is the pane's own rung-1 override. Resolution is where the two meet — a pane with no `manual_label` may display a name inherited from its tab (D3), and that inheritance is gated on `AgentNameAuthor` so a hand-set handle is never clobbered. This test protects the *store* split only; the resolution coupling is covered in `src/workspace/naming.rs`.` |
| `border_label_prefers_manual_label_over_agent_label` | terminal/state.rs:3943 | 2-arg `border_label` | 3-arg; add cases: inherited label used when `manual_label` is `None` and author is `Managed`; inherited label *ignored* when author is `User`; `effective_title` still outranks both (R2) |
| `char_tab_display_name_uses_index_not_public_tab_number` | Phase 0 | index | inverted to `tab_display_name_uses_the_public_tab_number` |
| `char_tab_info_label_equals_tab_display_name_for_every_tab` | Phase 0 | — | kept; still true, now over resolver output |
| `char_mounted_scope_labels_come_from_the_remote_snapshot` | Phase 0 | labels in `custom_name` | labels in `mirrored_name`; `custom_name` is `None`; resolved text unchanged |
| `char_resync_tab_label_lands_in_custom_name` | Phase 0 | — | `resync_tab_label_lands_in_mirrored_name` |
| *(safe, no change)* | app/api/layouts.rs:814 | `Some("dev")` — an override | unchanged |
| *(safe, no change)* | actions.rs:2828/2866/4193/4210/4240/4284, api/workspaces.rs:1702/1874, app/mod.rs:3155, creation.rs:4817 | all assert overrides | unchanged |

### New tests

- `src/workspace/naming.rs`: `mounted_workspace_with_a_locally_valid_remote_cwd_resolves_to_the_mirrored_label` — **the concrete regression FNC-1 exists to prevent.** Construct a mounted workspace whose remote cwd string also names a real git root on the local machine (the `cached_auto_label` / `git_branch` hazard at workspace.rs:283-286), clear nothing, assert the resolved name is the mirrored remote label and never a locally-derived one.
- `src/ui/panes.rs`: `border_titles_pass_the_resolved_tab_override_without_a_lookup`.
- `src/app/api/tabs.rs`: `renaming_a_tab_changes_every_auto_named_pane_label_in_it` (D3) and `renaming_a_tab_does_not_change_a_user_named_agents_label` (R3).
- `src/app/api/workspaces.rs`: `renaming_a_workspace_does_not_rename_its_tabs` (**R1**, the ruling's regression guard).
- `src/app/actions.rs`: `clearing_a_workspace_override_snaps_to_the_live_derived_name` (D2).

**Exit gate:** full suite green; `just bench-render-scale` at 1 and ≥15 populated panes with the delta reported (see Phase 6 for the fallback if `just` is unavailable).

---

## Phase 5 — API and wire surface

**Files:** `src/api/schema/tabs.rs`, `src/api/schema/workspaces.rs`, `src/api/schema/panes.rs`, `src/protocol/wire.rs`, `src/server/client_shell.rs`, `src/remote/federation/reducer.rs`, `src/client/shell/*`, `docs/next/api/herdr-api.schema.json`.

1. `TabInfo` (tabs.rs:40), `WorkspaceInfo` (workspaces.rs:77), `PaneInfo` (panes.rs:527) gain `name_source: NameSource` with `#[serde(default)]` and a `serde(rename_all = "snake_case")` enum. Clients receive a resolved name plus a discriminant and never re-derive.
2. **`custom_label: bool` → `name_source: NameSource`** on `ClientShellWorkspace` (wire.rs:1063) and `ClientShellTab` (wire.rs:1096). Positional bincode → **`PROTOCOL_VERSION` 23 → 24** (§1). Update wire.rs:2714/:2728 fixtures, `check_client_version` tests at :3171/:3178/:3187, and `autodetect.rs:609`.
3. **All nine TUI consumers** switch off the boolean in the same commit so none silently keeps it: `tabs.rs:107`, `:112`, `mobile.rs:663`, `:776`, `:886`, `agent_sidebar.rs:264`, `overlay_input.rs:413`, `context_menu.rs:416`, `sidebar.rs:610`. Three carry product decisions:
   - `mobile.rs:886` renders `format!("tab {}", tab.label)` for auto-named tabs — with a derived name that reads **"tab src/detect"** (U3). Switch on `NameSource`: `Override`/`Inherited`/`Mirrored`/`Cwd` render the label bare, only `Ordinal` gets the `"tab "` prefix.
   - `tabs.rs:107-118` styles auto-named tabs `DIM`. Only `NameSource::Ordinal` renders dim; a derived name is meaningful and must not read as disabled.
   - `agent_sidebar.rs:264` hides the tab token in single-tab workspaces unless `custom_label`. **Preserve today's visible behavior**: treat `Mirrored` like `Override` for that one row-visibility test (federation-scope unresolved Q1's default recommendation), so this refactor ships with no visible federation change. Record the choice in a comment; changing it is a separate product call.
   - `sidebar.rs:591-620` and its tests `spoofed_custom_label_does_not_hide_the_remote_badge` (:818) / :840 assert the remote badge is independent of `custom_label`; rewrite them against `name_source`, keeping the same security property (a spoofed name_source must not hide the badge).
4. **FNC-5 — `NameSource` never crosses the mount boundary as data.** The field rides inside `MountSnapshot` because the types are shared, but `namespace_workspace` (reducer.rs:420), `namespace_tab` (:435) and `namespace_pane` (:451) — the existing single choke point where every remote string is already sanitized — **discard** the received value and stamp the local one: `Mirrored` for a materialized scope with no local override, `Override` when the local user renamed it, never `AgentIdentity`/`Cwd`/`Ordinal`.
5. **Rename params.** `TabRenameParams.label` (api/schema/tabs.rs:28-31) and `WorkspaceRenameParams.label` (workspaces.rs:45-48) become `Option<String>` with `#[serde(default, skip_serializing_if = "Option::is_none")]`, so `null` clears an override. New client → old server still breaks on a *missing* required field; that is accepted and belongs in the changelog's protocol note. Add the "reset to auto" action to the rename overlay (`client/shell/overlay_input.rs`, near the existing no-op-override guard at :1034-1043) — under Policy C, auto means a meaningful derived name, so clearing becomes reachable (G3).
6. **Regenerate** `docs/next/api/herdr-api.schema.json` with `HERDR_UPDATE_API_SCHEMA=1`; `generated_protocol_schema_artifact_is_current` (api/schema/tests.rs:182) is the gate.

### Tests

- Rewrite Phase 0's `char_custom_label_is_a_one_bit_override_flag` → `client_shell_reports_the_resolved_name_source`.
- Rewrite `char_custom_label_is_true_for_every_mounted_scope` → `mounted_scopes_report_mirrored_not_override` (**F2 fixed**).
- Rewrite `char_client_shell_structs_round_trip_at_protocol_23` → `..._at_protocol_24`.
- New `src/remote/federation/reducer.rs`: `a_remote_name_source_is_replaced_by_the_local_stamp` — beside the existing sanitize tests at :1060-1106.
- New `src/remote/federation/protocol/mod.rs`: `federation_protocol_version_is_unchanged_at_7`, with §1's additive-field reasoning in the comment.
- Update fixtures at `client/shell/tests/mod.rs:36/:50`, `mobile.rs:332/:343/:358/:545/:586`, `chrome_context.rs:11`, `keybindings_settings.rs:103`, `agents_worktrees_notifications.rs:108`, `sidebar.rs:765/:821/:840`.

**Exit gate:** full suite green; schema artifact regenerated and committed.

---

## Phase 6 — persistence check, docs, changelog, bench

1. **Persistence.** No new persisted state except `PaneSnapshot.agent_name_author` (Phase 1). `SNAPSHOT_VERSION` **stays 3** (§1). Confirm Phase 0's three `char_` persistence tests still pass unmodified — if any needed editing, the change persisted something it should not have.
2. **Docs** (English + `ja/` + `zh-cn/`; `scripts/docs_translation_parity.py` compares **heading outlines**, so any new heading must be added to all three or `just release-docs-check` fails):
   - `docs/next/website/src/content/docs/socket-api.mdx` (+ `ja/`, `zh-cn/`): `name_source` on `TabInfo`/`WorkspaceInfo`/`PaneInfo`, the enum's values, and `label` becoming nullable on `tab.rename` / `workspace.rename`. `tab_label` is already documented at :278.
   - `concepts.mdx` (+ `ja/`, `zh-cn/`): the naming ladder — override → agent identity → cwd/git derivation → tab number — plus R1 (a workspace rename does not rename its tabs) and R3 (a tab rename changes an agent's displayed label but not its `herdr agent send` handle, design-decision's accepted limitation). :24 already covers manual pane renames.
   - `connecting-machines.mdx` (+ `ja/`, `zh-cn/`): FNC-4 — renaming a mounted remote workspace or tab is local to this machine, is not sent to the serving host, is not saved, and is lost on remount.
3. **`docs/next/CHANGELOG.md`** — under `## Unreleased`. This is a user-facing runtime change and *does* get an entry (the fork's rule excludes only website/docs/CI/build/maintenance changes). Draft:

   ```markdown
   ### Changed
   - Workspace, tab, and agent names now resolve through one shared precedence chain: a name you
     typed, then the agent's identity, then the project directory or git root, then the tab number.
     Auto-named tabs now show the directory they sit in rather than a position, and they report the
     tab's stable number instead of its current position when no directory resolves — a tab you moved
     no longer changes its own name. Renaming a tab renames the agents inside it that you have not
     named yourself; the name you type into `herdr agent send` is unchanged. Renaming a workspace does
     not rename its tabs. Clearing a name now falls straight back to the current project instead of
     waiting for a refresh, and `tab.rename` / `workspace.rename` accept a null label to clear one.
   - Renaming a workspace or tab that belongs to a mounted remote machine applies on this machine
     only. It is not sent to the other host, is not saved, and is lost when the mount is re-created.
   - The server/client protocol version moved from 23 to 24, because workspaces, tabs, and panes now
     report where their name came from instead of a plain "renamed" flag. After upgrading, restart the
     Herdr server (`herdr server stop`, then start Herdr again) so the running server and the installed
     CLI speak the same protocol; until then CLI commands report a client/server version mismatch.
     The federation protocol version is unchanged at 7, so mounts to hosts running 0.9.0 keep working.
   ```

4. **Bench (CLAUDE.md, mandatory — this change touches the render/client-fanout path by construction).** `just bench-render-scale` at fixed geometry with 1 and ≥15 populated panes; report the scaling delta. `just` and `cargo nextest` may be absent on this machine — if the bench cannot run, the substitute is a deterministic operation-count test asserting that a full `session_snapshot()` over W×T performs **zero** `process_cwd`/`read_link` calls attributable to tab naming (count via a test-only counter on the runtime cwd accessor). CLAUDE.md explicitly prefers deterministic operation tests to wall-clock limits, so this is not a downgrade.

**Exit gate:** `just check` (or `cargo fmt --check` + `cargo nextest run` + `python3 -m unittest scripts.test_docs_translation_parity`) green; only the ~3 pre-existing clippy errors remain.

---

## 3. Disposition of every ranked predict finding

| # | Finding | Disposition |
|---|---|---|
| **P1** | Tab cwd derivation lands in the per-render-tick snapshot build (`render.rs:423` → `client_shell.rs:13` → `session.rs:33-42` → `creation.rs:411` → `workspace.rs:1085` → `tab.rs:532` → `pane.rs:3821` → `platform::process_cwd`), turning `W × clients` into `W × T × clients` | **Accepted in full.** Phase 2 moves all cwd resolution onto the ~1.5 s git pass; Phase 3 makes the resolver read cached scalars only and enforces it with `naming_module_contains_no_io_or_derivation_calls`; Phase 6 benches. Chain verified in-tree |
| **M1** | `AgentNameOwner` (state.rs:103-107) records which agent owns the name, not who authored it; all three writers funnel through one `set_agent_name` | **Accepted in full.** R3 + Phase 1 add `AgentNameAuthor`. Verified: the struct has exactly `{agent_label, session_ref}` |
| **A1** | Rung 3 is not shared — workspace = repo basename (`discovery.rs:66`), tab = relative suffix, pane = nothing; one resolver would centralize divergence | **Accepted.** R4: rung 3 is a caller-supplied input, not a `match scope` arm. The resolver's DRY win is rungs 1/2/4, which is where the `tab_idx + 1` vs `public_tab_number` drift lives |
| **U1** | Tab derivation is worst in the single-repo case it was requested for: sibling tabs at the repo root all derive the same (empty) suffix | **Accepted.** Phase 2 step 3 adds a distinctness pass — colliding siblings all fall to their ordinals. The nested-monorepo truncation concern (`tabs.rs:96-100`) is design-decision's own carried-forward question #2 and stays open for post-implementation eyeballing |
| **G1** | The mount path writes remote resolved labels into `custom_name` (`creation.rs:562-563`, `:1710-1716`) and `manual_label` (`:744`, `client.rs:1423`), destroying the override/derived distinction before the wire question arises | **Accepted in full.** FNC-3: Phase 1 adds `mirrored_*` (dual-write), Phase 4 removes the `custom_name` half. This is the concrete answer the design-decision's quarantined federation pass demanded |
| **A2** | `NameSource` duplicates `custom_label`, which already ships on a positionally-encoded bincode wire; two authorities for "is this a user name?" | **Accepted.** Phase 5 replaces `custom_label` rather than adding beside it, and migrates all nine consumers in the same commit. Forces `PROTOCOL_VERSION` 23 → 24, which §1 confirms is required anyway |
| **M2** | `client_shell.rs:90-99` zips API-shaped `snapshot.tabs` positionally against a fresh `state` traversal, unguarded; a `?` in `creation.rs:236`'s `filter_map` already exists and any skippable `tab_info` shifts every later tab's flags | **Accepted.** Phase 0's `char_client_shell_tabs_pair_with_their_own_workspace_tab` lands first and guards every later phase. Re-keying the zip on `tab_id` is the fallback if it ever trips; not done pre-emptively |
| **M3** | `Workspace::test_new` (:1231) renames every test workspace, so literal workspace→tab inheritance detonates the suite | **Accepted.** R1 rules inheritance is tab→pane/agent only, documented in `naming.rs`; Phase 0 pins the fixture fact explicitly |
| **M5** | `test_adversarial_identity_state` closes its only named tab, so it passes Policy C trivially | **Accepted.** Phase 1 step 5 extends it; Phase 1 step 6 and Phase 4 step 7 add the invariants |
| **G2** | Do not bump `SNAPSHOT_VERSION` — `parse_snapshot` (:501-509) hard-errors on a newer version, so a downgrade loses the session | **Accepted.** §1: stays 3, all new fields `#[serde(default)]` |
| **U2** | The sidebar's displayed agent name stops being the string you type into `herdr agent send` | **Accepted as a known limitation, mitigated.** design-decision §"Accepted limitation" takes this knowingly, but predict is right that it must not be *silent*: R3's gate means an agent you named yourself never changes, so the divergence can only appear for auto-assigned handles. Keeping the handle as a second sidebar token (`ui/sidebar/tokens.rs:88-99`) is **out of scope** — a token-layout change is TUI presentation work with its own config surface |
| **U3** | `mobile.rs:886` renders `format!("tab {}", label)` → "tab src/detect"; `tabs.rs:107` dims auto-named tabs; `agent_sidebar.rs:264` hides the most useful token | **Accepted.** Phase 5 step 3 fixes all three by switching on `NameSource`, preserving the single-tab row-visibility behavior deliberately |
| **G3** | Nullable rename params break new-client → old-server; no UI path exists to clear a tab override | **Accepted.** Phase 5 step 5 adds `#[serde(default, skip_serializing_if)]` and the "reset to auto" overlay action. The new→old break is real, unavoidable given the 23→24 bump, and goes in the changelog |
| **P2** | The client-shell snapshot is built unconditionally then compared (`render.rs:436`), so any allocation the resolver adds is unamortized | **Accepted.** Phase 3 returns `Cow`, with `resolve_returns_borrowed_text_for_rung_1_and_2` asserting it |
| **P3** | The git pass can be entirely off — `git_refresh_deadline` (:95) needs a sidebar `Branch`/`GitStatus` token — so derived names would never refresh for many users | **Accepted.** Phase 2 step 6 adds the `auto_names` demand flag |
| **M4** | Four tests encode intent Policy C inverts | **Accepted.** Each is named with its replacement assertion in Phase 4's rewrite table. `layouts.rs:814` re-verified as safe (it asserts an override) |
| **F1** (federation-scope) | A remote-side rename never reaches the mounted mirror — `reducer.rs:559-567`/`:500-505` emit events but `src/events.rs` has no `FederationResync*Renamed` and no handler calls `set_custom_name` | **Out of scope, and load-bearing as a test constraint.** Pre-existing; do not fix here. **No test in any phase may assert that a mounted label updates after a remote rename** until it is confirmed against two live servers (federation-scope unresolved Q3) |
| **F3** (federation-scope) | Remote agents are not mirrored at all (`reducer.rs:693` sets `agents: Vec::new()`) | **Out of scope.** Recorded so nobody re-derives it: there is no remote agent handle to break across a mount, so Policy C's accepted limitation is trivially satisfied there |
| federation-scope Q4 | `identity_cwd` for a mounted workspace is a remote path run through `discover_workspace_git_identity` + `git_branch` against the *local* filesystem (`creation.rs:564`, `workspace.rs:283-286`) | **Out of scope; FNC-1 defuses the visible half.** The resolver never surfaces the resulting `cached_auto_label` for a Remote scope. The wasted filesystem and subprocess work at mount time stays and deserves its own issue |
| predict Q5 | Is `just bench-render-scale` runnable here? | **Answered in Phase 6:** deterministic operation-count test as the substitute, which CLAUDE.md prefers regardless |

---

## 4. Standing constraints (apply to every phase)

- No `unwrap()` in production code. `#[allow]` only with a justifying comment. `tracing` for logging.
- Render stays pure: `compute_view()` mutates, `render()` only draws. The resolver is a pure function of cached scalars and is called from snapshot construction, never from `render()`.
- Nothing in `src/workspace/naming.rs` may touch the filesystem, a process table, a terminal snapshot, or a lock. Enforced by test, not by review.
- Resolution is server-side. Clients receive `{ label, name_source }` and never re-derive. No UI nouns (sidebar/row/card/widget) in server or API identifiers.
- Platform code stays compile-gated under `src/platform/`; nothing in this change is platform-specific.
- Build: `export ZIG=$HOME/.local/zig-0.15.2/zig`. Tests: `cargo nextest run`, else `cargo test -- --test-threads=4`. Never `--bin herdr` (skips `tests/` entirely). ~3 clippy errors pre-exist; only regressions introduced by this diff count.

---

## Unresolved questions

1. **`Workspace::display_name` (workspace.rs:1059) is annotated `#[cfg(test)]` but has five non-test callers** (`actions.rs:2715/:2733/:2757/:2785`, `api/workspaces.rs:1926`). Either the attribute is on the wrong item or those call sites are themselves test-gated. Resolve by reading workspace.rs:1050-1066 before Phase 4 touches it; it changes whether that function is part of the migration surface.
2. **Does the plugin-context tab label change?** `app/api/plugins/mod.rs:3179` asserts `Some("1")` with a workspace cwd of `/tmp/issue`. Whether rung 3 fires there depends on whether the fixture populates `cached_auto_label`. Run it; pin whichever it is. Do not guess.
3. **Deeply nested monorepo tab names** — design-decision's own carried-forward question #2. The distinctness pass (Phase 2 step 3) guarantees uniqueness but not legibility inside a ~12-column tab cell (`tabs.rs:96-100`). Needs real-world eyeballing after Phase 4, not a design decision now.
4. **F1 is code-read only.** Whether a remote-side rename really fails to reach a mounted TUI has not been confirmed against two live servers. Phase 5 must not add a test that asserts either direction until it is.
5. **Single-tab mounted workspaces show a tab row today by accident** (`agent_sidebar.rs:264`, via `custom_label` being unconditionally true for mounted scopes). Phase 5 preserves the visible behavior deliberately, but once `NameSource::Mirrored` exists someone should decide whether it is wanted. Product call, not a blocker.
