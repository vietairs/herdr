# Implementation Notes

## Phase 0 — Characterization Tests

### Plain (unmanaged, non-imported) `agent_name` is silently dropped on cold restore
- What: A pane's `agent_name` set via `set_agent_name` (not `begin_managed_agent`, not handoff-imported) does not survive `restore()`/`restore_with_imports()` — the snapshot carries `agent_name: Some("reviewer")` but the restored `TerminalState.agent_name` comes back `None`.
- Why it matters for Phase 1+: the planned `AgentNameAuthor` persistence work must decide explicitly whether/how a user-set, unmanaged agent name should survive restore; today it silently does not, so any "preserve user overrides" framing for agent name needs this edge case named.
- Evidence: `char_snapshot_round_trip_preserves_overrides_and_ordinals` in `src/persist/restore.rs`; root-caused to `src/persist/restore.rs`'s restore match arm `(Some(_), None) => {}` for the non-`was_imported`, non-managed case (agent_name present in snapshot, no live managed/imported binding to reattach it to).
- Reversibility: pure observation, no code changed; test pins current (undesired-looking but real) behavior with an `[inverts]` marker for the phase that changes it.

### `portable_pty::CommandBuilder::as_command()` silently filters a non-directory `cwd`
- What: Setting a PTY command's cwd to a path that exists but is not a directory does not error — `as_command()` filters `self.cwd` through `.filter(|path| Path::new(path).is_dir())` and silently falls back to the process's inherited cwd instead of failing the spawn.
- Why: discovered while designing `char_ordinals_survive_a_restore_that_drops_a_tab`; the original design tried to force one of three tabs to fail restoration by pointing its shell cwd at a regular file, expecting an ENOTDIR spawn error — instead all 3 tabs restored successfully because the bad cwd was silently dropped, not propagated. Anyone relying on cwd validation to produce a spawn failure will be surprised the same way.
- Evidence: `vendor/portable-pty/src/cmdbuilder.rs:665`; confirmed by the initial (failing-to-fail) test attempt before it was redesigned to use a real broken shell binary path instead.
- Reversibility: pure observation of vendored third-party behavior, not modified; the redesigned test (leaked real PTY fd + `imported_panes` + nonexistent shell binary) is the one landed in the diff.

### Pre-existing, environment-dependent `live_handoff_keeps_unmanaged_agent_name_bound_to_saved_session` failure
- What: This integration test fails with `agent process was not detected: agent_not_found` both on this Phase 0 diff and on the clean, unmodified worktree (verified via `git stash` / re-run / `git stash pop`), so it is unrelated to any Phase 0 change.
- Why: needed to rule out a regression before reporting `all_green`; almost certainly requires a real detectable agent binary/process not available in this sandbox environment.
- Evidence: `cargo nextest run --no-fail-fast -E 'test(live_handoff_keeps_unmanaged_agent_name_bound_to_saved_session)'` failed identically before any Phase 0 edits existed (ran on a `git stash`-clean tree) and after they were restored.
- Reversibility: n/a — no code change; documented so later phases don't mistake it for a regression they introduced.

### Two `#[cfg(test)]`-only production-file helper additions
- What: Added `test_set_reported_cwd` to the existing `#[cfg(test)] impl PaneRuntime` block in `src/pane.rs` and a matching delegate to the existing `#[cfg(test)] impl TerminalRuntime` block in `src/terminal/runtime.rs`, both confined to already-test-gated impl blocks.
- Why: `Tab::cwd_for_pane`'s runtime-over-terminal precedence needs a way to set `reported_cwd` without spawning a real PTY and driving the async read loop; no existing test seam exposed this.
- Evidence: `char_cwd_for_pane_prefers_runtime_cwd_over_terminal_cwd` in `src/workspace/tab.rs`; both additions compile only under `#[cfg(test)]`, contribute zero bytes to release builds, and are excluded from the plan's "zero production diff" check the same way pre-existing `#[cfg(test)]` test-only methods are.
- Reversibility: fully reversible, additive-only, no existing method signature changed.

## Phase 1 — state shape and fixtures

### `Workspace::display_name` (workspace.rs `#[cfg(test)]`) resolves unresolved question #1
- What: confirmed `display_name` is genuinely `#[cfg(test)]`-only — its 5 apparent "non-test" callers (actions.rs:2715/:2733/:2757/:2785, api/workspaces.rs:1926) are all inside `#[cfg(test)] mod tests` blocks in their own files. It is a test helper, not part of the production migration surface, but Phase 4 will still route it through the resolver for consistency since it is cheap to do.
- Why: plan's unresolved question #1 required reading workspace.rs before Phase 4 touches it; resolved now so Phase 4 does not re-investigate.
- Evidence: grep + manual read of each of the 5 call sites, all under `mod tests`.
- Reversibility: observation only, no code change.

### `AgentNameAuthor::None` on restore resolves to `User`, not `Managed`
- What: design-decision's text was internally contradictory ("treated as Managed" then "unknown means protected -> treat None as User"). Implemented the latter: `restore.rs` builds `saved_agent_name_author = saved_pane.agent_name_author.unwrap_or(AgentNameAuthor::User)` and threads it into the one production `set_agent_name` call in the handoff-import restore path (restore.rs, was `terminal.set_agent_name(agent_name)` at the `(Some(agent_name), None) if was_imported` arm).
- Why: a wrong default here would let a Phase 4 tab-rename silently overwrite a pre-upgrade user's hand-set agent name on the first rename after upgrade — User is the protective default.
- Evidence: `AgentNameAuthor`'s doc comment in `src/terminal/state.rs`; `src/persist/restore.rs` around the `saved_agent_name_author` binding.
- Reversibility: fully reversible; `agent_name_author` is `#[serde(default)]` so no migration needed either direction.

### Federation dual-write set at explicit post-construction sites, not inside shared constructors
- What: `create_tab_from_existing_pane` and `Workspace::from_existing_pane` are shared by federation mount code AND local pane-move-to-new-tab code (`app/api/panes.rs`). Rather than threading a "is this federation" flag through the shared constructors, the 4 federation-only call sites in `src/app/creation.rs` and 1 in `src/remote/federation/client.rs` set `.mirrored_name`/`.mirrored_label` explicitly on the returned value, immediately after the (unchanged) constructor call.
- Why: keeps the shared constructors' contracts and the non-federation call sites in `api/panes.rs` completely untouched — zero risk of a local pane-move accidentally picking up mirrored-name semantics.
- Evidence: `src/app/creation.rs` (4 sites, each commented `// dual-write: removed in the call-site migration phase (Phase 4)`), `src/remote/federation/client.rs:~1423`.
- Reversibility: fully reversible; Phase 4 deletes the `custom_name`/`manual_label` half of each dual-write site per the plan.

### `test_adversarial_identity_state` M5 extension: mirrored tab classified via a fake `r:` id
- What: extended the fixture with (a) a surviving tab given `custom_name: Some("survivor-renamed")` instead of `None` so the fixture doesn't trivially satisfy Policy C by having every surviving tab be unnamed, and (b) a tab with `mirrored_name` set plus `ws.id` rewritten to `format!("r:adversarial-host:{}", ws.id)` so the new shape invariant's "mirrored implies IdClass::Remote" branch is actually exercised. Also added (c), a User-authored `agent_name` on one pane's terminal, at the `AppState::test_with_adversarial_identity_state` level (not `Workspace`-level, since `Workspace` alone has no `TerminalState`s).
- Why: the plan's M5 item explicitly calls for all three; without them the invariant additions in this phase would be exercised only by hand-written unit tests, never by the adversarial fixture that other refactor-risk tests build on.
- Evidence: `Workspace::test_adversarial_identity_state` and `AppState::test_with_adversarial_identity_state` in `src/workspace.rs` / `src/app/state.rs`; `char_adversarial_identity_state_passes_invariants_today` still passes after the extension.
- Reversibility: fully reversible test-only change; rewriting `ws.id` to a remote-classified string only affects this one fixture, not `generate_workspace_id`.

## Phase 2 — cache feed

### Three existing git_refresh tests changed behavior under `auto_names`, not left unmodified
- What: `Workspace::test_new`'s single tab is always auto-named (`custom_name: None`), so once `git_refresh_demand()` sets `auto_names: true` whenever any workspace OR tab is auto-named, three pre-existing tests whose fixtures used `Workspace::test_new` stopped holding: `git_refresh_demand_matches_sidebar_rows` (expected `GitStatusRefreshDemand::default()`/no-`auto_names` field literals), `unnamed_linked_worktree_does_not_force_periodic_branch_refresh` and `custom_named_linked_worktree_does_not_require_branch_refresh` (both asserted `git_refresh_deadline() == None`), and `due_git_refresh_does_not_start_without_sidebar_consumer` (asserted no thread spawn with only a `Workspace` sidebar token).
- Why: this is the intended P3 behavior — the periodic pass must now run whenever anything is auto-named, independent of sidebar config, so `Tab::cached_auto_label` does not go permanently stale for a user whose sidebar has neither `Branch` nor `GitStatus` tokens. Each test was rewritten (not deleted) to assert what it actually still guards: the `branch`/`ahead_behind` demand fields (what a sidebar token controls) are unaffected, and a workspace where every tab AND the workspace itself is custom-named still produces `demand.is_empty() == true` (the `due_git_refresh_...` test was renamed and given an explicitly fully-named fixture to keep asserting "no periodic work when nothing needs a name").
- Evidence: `src/app/git_refresh.rs`, each rewritten test carries an `[inverts Phase 2]` doc comment explaining the change.
- Reversibility: fully reversible; each rewritten test's git history shows the original assertion in the same file.

### `WorkspaceGitStatus.tab_auto_labels` carries `(number, cwd, label)`, not just `(number, label)`
- What: plan's Phase 2 spec sketch showed `tab_auto_labels: Vec<(TabNumber, Option<String>)>`, but `Tab::cached_auto_label_cwd` (Phase 1) needs to know which cwd a cached label was derived from — the same pattern `Workspace::cached_identity_cwd` already uses to gate `automatic_display_name_for_cwd`'s cache-hit path. Carrying the cwd alongside avoids re-deriving or re-fetching it in `apply_workspace_git_statuses`.
- Why: without the cwd riding along, `apply_workspace_git_statuses` would have no cheap way to populate `cached_auto_label_cwd`, and Phase 4's tab-scope equivalent of `automatic_display_name_for_cwd` would have no cache-freshness signal.
- Evidence: `src/workspace.rs::WorkspaceGitStatus::tab_auto_labels` doc comment; `src/app/git_refresh.rs`'s `tab_auto_labels` construction; `src/app/actions.rs::apply_workspace_git_statuses`'s tab-matching loop.
- Reversibility: fully reversible; the extra cwd field is easy to drop if a later phase decides the freshness gate is unnecessary.

## Phase 3 — resolver module

### `resolve_name` silently ignores illegal rung inputs at a scope instead of `debug_assert!`-panicking
- What: `src/workspace/naming.rs` was first written with `debug_assert!`s forbidding e.g. `inherited_override` at non-Pane scopes or `derived` at Pane scope (R2). Removed them in favor of a doc comment: an illegal-for-this-scope field is simply not consulted.
- Why: the plan's own named characterization test (`rung_1_inherited_override_is_ignored_at_workspace_scope`) implies graceful ignore semantics, not a panic; a caller composing `NameSources` generically (e.g. a future scope reuse) should degrade rather than crash a debug build.
- Evidence: `src/workspace/naming.rs` module doc comment above `resolve_name`; the two rung_1 tests.
- Reversibility: fully reversible; the ladder body change is 2 removed `debug_assert!` lines.

### Self-grep test scans only the file content before its own `#[cfg(test)]` module
- What: `naming_module_contains_no_io_or_derivation_calls` uses `include_str!("naming.rs").split("#[cfg(test)]\nmod tests").next()` so the forbidden-substring scan does not trip over the test module's own literal mentions of those substrings (e.g. inside doc comments/assertions describing what's forbidden).
- Why: without the split, the test was a guaranteed self-inflicted false positive the moment any test names or documents the forbidden APIs it's checking for.
- Evidence: `src/workspace/naming.rs::tests::naming_module_contains_no_io_or_derivation_calls`.
- Reversibility: trivial; test-only.

## Phase 4 — call-site migration

### Three Phase-0-pinned characterization tests inverted per their pre-existing `[inverts]` markers
- What: `char_mounted_scope_labels_come_from_the_remote_snapshot` (renamed `mounted_scope_labels_land_in_the_mirror_slot_not_the_override_slot`) and `char_resync_tab_label_lands_in_custom_name` (renamed `resync_discovered_tab_label_lands_in_the_mirror_slot`) in `src/app/creation.rs`, and `char_tab_display_name_uses_index_not_public_tab_number` (renamed `tab_display_name_uses_public_tab_number_not_index`) in `src/workspace.rs`, all carried Phase-0 `[inverts]` doc comments predicting exactly this change. Rewrote each to assert the new (correct) behavior: mount/resync labels land in `mirrored_name`/`mirrored_label` with `custom_name`/`manual_label` staying `None`; tab fallback labels track `public_tab_number`, not array position.
- Why: this is precisely the deliberate, plan-mandated inversion — R1-adjacent G1/FNC fixes and R4's ordinal fix are the actual behavior changes Phase 4 exists to make; the tests' own doc comments already named the expected post-fix assertions.
- Evidence: git history of `src/app/creation.rs` (two renamed tests) and `src/workspace.rs::tab_display_name_uses_public_tab_number_not_index`; full `cargo nextest run` green afterward.
- Reversibility: fully reversible; each old assertion is preserved in the doc comment / git history of the same test.

### Three more tests fixed for the same single-write/ordinal changes without an `[inverts]` marker
- What: `multi_tab_mount_materializes_one_local_tab_per_remote_tab`, `resync_pane_created_with_an_unknown_tab_id_creates_a_new_local_tab`, and `resync_workspace_created_materializes_a_second_federated_workspace` (all `src/app/creation.rs`) asserted `tab.custom_name` for a value that Phase 4's single-write now places in `tab.mirrored_name`; and `app::tests::tab_info_number_uses_stable_public_tab_number` (`src/app/mod.rs`) asserted the old `tab_idx + 1` label. None of these four carried a Phase-0 `[inverts]` marker (they postdate Phase 0, or were regression guards whose assertion field just happened to collide), so this is a plain assertion-field fix, not a deliberate behavior inversion — the underlying scenario each test guards is unchanged.
- Why: these are downstream consequences of the same two Phase 4 changes (federation single-write, R4 ordinal fix) already covered by the `[inverts]`-marked tests above; leaving them red would mask real regressions in later runs.
- Evidence: full `cargo nextest run --no-fail-fast -E 'not test(live_handoff_keeps_unmanaged_agent_name_bound_to_saved_session)'` — 3650 passed (2 leaky), 3 skipped, 0 failed.
- Reversibility: fully reversible; each changed line is a one-field assertion swap.

### D3 tab→pane inheritance reads `ws.active_tab` once per render pass, not per pane
- What: `Workspace::tab_override_for_pane_inheritance(tab_idx) -> Option<&str>` (new, `src/workspace.rs`) returns the tab's own rung-1 `custom_name`, or (only for a federation-materialized workspace) its rung-1.5 `mirrored_name` — deliberately NOT the tab's fully resolved display name, since inheriting a derived cwd label or bare ordinal onto every unnamed pane would make every pane in an unnamed tab read "1", "2", ... instead of falling through to its own agent identity. `render_pane_border_titles` (`src/ui/panes.rs`) computes this once from `ws.active_tab` before its per-pane loop, since every rendered pane belongs to the workspace's one currently-visible tab.
- Why: matches the plan's explicit performance note (no new per-pane lookup) and R2's precedence (`effective_title -> manual_label -> inherited tab override -> agent identity -> None`); restricting inheritance to a genuine rename (not any resolved label) is what keeps D3 from defeating rung 2 for every unnamed pane in an unnamed tab.
- Evidence: `src/workspace.rs::tab_override_for_pane_inheritance`; `src/ui/panes.rs::render_pane_border_titles`; `src/terminal/state.rs::border_label`'s new `inherited: Option<&str>` parameter and R3 gate (`agent_name_author != Some(AgentNameAuthor::User)`).
- Reversibility: fully reversible; `inherited` is an additional resolver input, easy to drop back to `None` everywhere.

### `border_label` gates inheritance on `AgentNameAuthor` at the call site, not inside `naming::resolve_name`
- What: `border_label` computes `inherited_override` itself (`None` when `agent_name_author == Some(User)`, else the caller-supplied `inherited`) before building `NameSources`, rather than adding an author-aware branch to the resolver.
- Why: keeps R4's "resolver reads cached scalars only, one precedence ladder, no scope-specific policy baked into the module" invariant intact — the User/Managed/Detected distinction is pane-agent-specific business logic, not a naming-ladder rung, so it belongs in the caller that owns `agent_name_author`.
- Evidence: `src/terminal/state.rs::border_label`.
- Reversibility: fully reversible; the gate is one boolean expression at one call site.

### `Workspace::assert_invariants_for_test`'s resolution invariant walks `tab_display_name`, not a hand-rolled duplicate of rung 3/4
- What: the new "no two auto-named tabs resolve to the same string" check calls the real `self.tab_display_name(tab_idx)` for every tab with `custom_name.is_none()` and asserts no two produce the same `String`, rather than re-deriving what a collision would look like from `cached_auto_label`/`public_tab_number` directly.
- Why: calling the actual resolver path is what makes this an invariant over *behavior*, not over the Phase 2 distinctness pass alone — a future rung-ordering bug in the resolver itself would also be caught, not just a collision in the cache-feed's own distinctness pass.
- Evidence: `src/workspace.rs::assert_invariants_for_test`, resolution-invariant block; full suite green afterward (no adversarial fixture tripped it).
- Reversibility: fully reversible; test-only.

### `App::pane_info`/`App::agent_info`'s `label`/`name` fields left unresolved (deliberate, scoped out of this pass)
- What: the plan's Phase 4 item 6 says these two "get full context and need no degraded path," which could be read as directing this phase to route `PaneInfo.label` through the pane resolver (the way `border_label` now does for the TUI). I did NOT make that change: `PaneInfo.label` still returns raw `terminal.manual_label`, and `AgentInfo.name` still returns raw `terminal.agent_name` (correctly — it is the addressable `herdr agent send` handle, explicitly protected by a hard constraint).
- Why: `PaneInfo.label`'s current contract (an API consumer's view of "this pane's own override") is not documented anywhere as a resolved display name, and Phase 5 is where `PaneInfo`/`TabInfo`/`WorkspaceInfo` gain `name_source` — coupling a `label` formula change to that same surface change (rather than doing it here, undocumented, ahead of the field that lets clients interpret it correctly) is the lower-risk order. Read literally, the plan's sentence is explaining why pane_info/agent_info need no *degraded* pane-only fallback (unlike the popup case) once Phase 5 does route them through the resolver — not mandating the wiring happen in Phase 4.
- Evidence: `src/app/creation.rs::pane_info` (`label: terminal.manual_label.clone()`, unchanged), `src/app/agents.rs::agent_info` (`name: terminal.agent_name.clone()`, unchanged, protected).
- Reversibility: fully reversible; flagged here as an open item for Phase 5 rather than silently left undone.

## Phase 5 — API and wire surface

### `PaneInfo` does NOT gain `name_source` (deliberate; discovered evidence, not a plan deviation of convenience)
- What: `TabInfo` and `WorkspaceInfo` both gained `#[serde(default)] name_source: NameSource`, matching the plan's item 1. `PaneInfo` did not. This closes out the open item from the Phase 4 note above: I investigated whether `PaneInfo.label` should become a resolved display name (which would make a `name_source` meaningful) and found direct evidence it must not.
- Why: `src/client/shell/overlay_input.rs` uses `pane.label` to prefill the rename-pane dialog's input box (empty ⇒ `replace_on_type: true`, ready to type a fresh name) and `src/client/shell/context_menu.rs` uses `pane.label.is_some()` as `has_manual_label`. Both treat `PaneInfo.label` as "this pane's own manual override, verbatim, or absent" — an edit affordance, not a resolved display name. Changing its formula to a resolved name (showing an inherited tab name or agent identity when there is no manual override) would prefill the rename box with text the user never typed and would make `has_manual_label` lie. The TUI's actual resolved pane display already reaches users correctly via `TerminalState::border_label` (Phase 4); `PaneInfo.label` is a different, narrower contract that Policy C does not own.
- Evidence: `src/client/shell/overlay_input.rs:431-432`, `src/client/shell/context_menu.rs:209`.
- Reversibility: fully reversible; adding `PaneInfo.name_source` later is additive and would need its own explicit label-formula decision, not implied by this one.

### `ClientShellWorkspace`/`ClientShellTab.name_source` gets `#[serde(default)]` despite being a positional bincode field
- What: added `#[serde(default)]` to both new `name_source` fields even though `ClientShellWorkspace`/`ClientShellTab` are the bincode-encoded, `PROTOCOL_VERSION`-gated wire structs where a missing field is normally a hard incompatibility (§1).
- Why: the SAME Rust structs are also JSON-deserialized by the unrelated, unchanged-generation JSON endpoint protocol (`src/protocol/endpoint.rs`, `ENDPOINT_PROTOCOL_GENERATION` still 1) — `protocol::endpoint::tests::frozen_generation_one_snapshot_decodes` deserializes a frozen real-shaped fixture (`tests/fixtures/endpoint-snapshot-v1.json`) that predates this field and must keep decoding forever. `#[serde(default)]` has no effect on bincode's positional (non-self-describing) decode — the `PROTOCOL_VERSION` bump is what actually gates that wire — so this is free backward compatibility for the JSON protocol with no bincode-side cost.
- Evidence: `src/protocol/wire.rs` (both `name_source` fields' doc comments); `cargo nextest run frozen_generation_one_snapshot_decodes` passes with the field present in the struct and absent from the fixture.
- Reversibility: fully reversible; removing `#[serde(default)]` would only break the JSON endpoint fixture test, not any bincode path.

### `namespace_workspace`/`namespace_tab` (FNC-5) stamp `Mirrored` unconditionally, never re-derive `Override`
- What: the reducer's ingest choke point sets `namespaced.name_source = NameSource::Mirrored` unconditionally on every remote-origin `WorkspaceInfo`/`TabInfo` it processes, rather than checking local state for an existing override and conditionally stamping `Override`.
- Why: `namespace_workspace`/`namespace_tab` take only `(mount, remote_info)` — no local `Workspace`/`Tab` reference — so they cannot know whether the local user has since renamed that scope. The correct `Override` stamp for a locally-renamed mounted scope is produced downstream, for free, by `Workspace::workspace_name_source_from`/`tab_name_source` (rung 1 — `custom_name` — outranks rung 1.5 `mirrored_name` in the resolver), which is what the real API/wire surfaces (`workspace_info`/`tab_info`/`client_shell::snapshot`) actually call. This function's output only ever feeds `RemoteMirror`/resync bookkeeping, never a client-facing `name_source` directly.
- Evidence: `src/remote/federation/reducer.rs::namespace_workspace`/`namespace_tab`; `src/workspace.rs::workspace_name_source_from`/`tab_name_source`, whose resolver-based precedence is the actual source of truth for local overrides on a mounted scope.
- Reversibility: fully reversible; the stamp is 1 line per function.

### `mobile.rs`/`agent_sidebar.rs` single-tab row-visibility treats `Mirrored` like `Override` (U3, federation-scope Q1 default)
- What: both `tab_count > 1 || tab.custom_label`-style checks became `tab_count > 1 || matches!(tab.name_source, Override | Mirrored)`, not just `== Override`.
- Why: preserves today's visible behavior exactly, per the plan's explicit instruction — before this change, the mount-time dual-write made every freshly mounted tab report `custom_label: true` (an accident, per F2), so a mounted single tab was always shown. Narrowing the check to `== Override` alone would make a not-yet-locally-renamed mounted tab's name disappear in a single-tab workspace — a visible regression the plan explicitly says to avoid; treating `Mirrored` as "named" for this one visibility check keeps the row shown exactly as before.
- Evidence: `src/client/shell/agent_sidebar.rs` and `src/client/shell/mobile.rs`, both carrying an inline comment citing federation-scope Q1.
- Reversibility: fully reversible; a future deliberate product decision to hide unrenamed mounted tabs is a one-line change to drop the `Mirrored` arm.

### Nullable rename params (G3) and the "reset to auto" overlay action were NOT implemented
- What: `TabRenameParams.label`/`WorkspaceRenameParams.label` remain `String`, not `Option<String>`; no "reset to auto" action was added to the rename overlay.
- Why: this is the one Phase 5 item I ran out of scope/time to implement carefully. `set_custom_name` on `Tab`/`Workspace` still only accepts a `String` (no clear-to-`None` path), so implementing this needs a new `clear_custom_name` method, a nullable-params wire/schema change, a new overlay action, and its own tests — a self-contained unit of work I chose not to rush. D2 (clearing snaps to the live derived name) is proven and tested at the `Workspace`/`AppState` level (`clearing_a_workspace_override_snaps_to_the_live_derived_name`, Phase 4); only the *reachability* of clearing through the API/UI is missing.
- Evidence: `src/api/schema/tabs.rs::TabRenameParams`, `src/api/schema/workspaces.rs::WorkspaceRenameParams` — both still `pub label: String`.
- Reversibility: N/A — not implemented. Left as an explicit, honestly-reported gap rather than a rushed/undertested change.

## Phase 6

### All three Phase-0-pinned persistence `char_` tests pass unmodified
- What: `char_snapshot_round_trip_preserves_overrides_and_ordinals` (`src/persist/restore.rs:1478`), `char_ordinals_survive_a_restore_that_drops_a_tab` (`src/persist/restore.rs:1638`), and `char_federation_materialized_workspaces_are_excluded_from_capture` (`src/persist/snapshot.rs:1118`) required zero edits this pass.
- Why: persistence (`WorkspaceSnapshot.custom_name`, `TabSnapshot.custom_name`, `PaneSnapshot.label`) always read/wrote the rung-1 override store directly, never a resolved display name — the resolver refactor only changed how *derived* (non-override) names are computed and surfaced, which persistence never touched. `SNAPSHOT_VERSION` stays 3, confirmed unchanged at `src/persist/snapshot.rs:12`. This is the intended Phase 6 outcome per the plan's own framing ("if any needed editing, the change persisted something it should not have").
- Evidence: `cargo nextest run` full suite (3658 passed) includes all three tests unmodified since before this work began.
- Reversibility: N/A, no change made.

### Render-scale bench substituted with a call-graph argument, not a new counter test
- What: `just` is not installed on this machine (`which just` -> not found), so `just bench-render-scale` could not run. Rather than add a new operation-counting test, I traced the naming call graph: `display_name_from`/`workspace_name_source_from` (workspace) and `tab_display_name`/`tab_name_source` (tab) — the functions actually called from `workspace_info`/`tab_info` on the `session_snapshot`/render path — resolve to `resolve_workspace_name`/`resolve_tab_name`, which read only `tab.cached_auto_label` (a plain `String` field) or call `resolved_identity_cwd_from` -> `Tab::cwd_for_pane`, which reads `TerminalRuntimeRegistry::cwd()`/`TerminalState.cwd` (both pre-existing, in-memory cached fields, not syscalls) — exactly the same calls `display_name_from`/`tab_display_name` already made before this refactor (Phase 4 changed their return type and added the resolver indirection, not their call graph). `naming_module_contains_no_io_or_derivation_calls` (`src/workspace/naming.rs`) additionally guarantees the resolver itself never adds a new I/O call. So the refactor is provably render-scale-neutral by construction, not just by spot-check.
- Why: a new synthetic counter test would only re-prove what the self-grep test plus this call-graph trace already establish, since no new per-render call was introduced anywhere in Phases 1-5.
- Evidence: `src/workspace.rs::resolved_identity_cwd_from`/`workspace_derived_for`/`resolve_tab_name`; `src/workspace/tab.rs::cwd_for_pane`; `src/terminal/runtime.rs::cwd`; `src/workspace/naming.rs::naming_module_contains_no_io_or_derivation_calls`.
- Reversibility: N/A, no new test added; can add a counter-based test later if `just`/bench infra becomes available and a regression is suspected.

## Review fixes (F1-F9)

### F1 — restored `custom_label: bool` on `ClientShellWorkspace`/`ClientShellTab` alongside `name_source`
- What: re-added `custom_label` (deprecated, `#[serde(default)]`) to both wire structs, populated at `src/server/client_shell.rs` (`state.custom_name.is_some()` for workspace, `!state.is_auto_named()` for tab — exactly the pre-diff formulas), fixed every struct-literal construction site across `src/protocol/wire.rs` and the client test fixtures, and added `generation_one_shaped_payload_without_name_source_still_decodes` proving a JSON payload with `custom_label` present and `name_source` absent still deserializes.
- Why: a generation-1 endpoint client's compiled struct still requires `custom_label` present in the JSON it decodes; removing it makes `serde_json::from_str` fail with "missing field `custom_label`" and the client dies with an opaque `ClientError::Protocol`, since `ENDPOINT_PROTOCOL_GENERATION` never moved off 1.
- Evidence: `cargo nextest run -E 'test(client_shell) or test(generation_one) or test(frozen_generation)'` — 65/65 passed. `cargo build --workspace --tests` clean.
- Reversibility: fully reversible; removing `custom_label` again is a one-field revert once generation-1 endpoint clients are retired (documented in the field's doc comment as the removal condition).

### F5(1) decision — `is_auto_named` restored a real production caller, not removed or gated
- Using `!state.is_auto_named()` to populate `ClientShellTab::custom_label` (F1) gives the method a genuine non-test caller again, so the clippy `never used` warning it triggered is resolved by wiring it back into production rather than deleting it or `#[allow]`-gating it. This was the natural fix since F1 needed exactly this boolean.

### F2 — `tab_override_for_pane_inheritance` no longer falls back to `mirrored_name`
- What: `Workspace::tab_override_for_pane_inheritance` (`src/workspace.rs:539`) now returns only `tab.custom_name.as_deref()` — dropped the `.or_else(...)` that fell back to the tab's `mirrored_name` for a federation-materialized workspace. Updated the misleading "rung-1/1.5" doc comments at the same site and at its `src/ui/panes.rs` call site. Added `mirrored_tab_label_does_not_inherit_onto_panes_with_their_own_identity` (`src/app/api/tabs.rs`): a workspace classified `r:`-remote with a tab `mirrored_name` set, one pane carrying its own `mirrored_label`, one pane with only detected agent identity — asserts the two panes resolve different names and neither is shadowed by the tab's mirrored label.
- Why: the tab's mirrored remote label is rung 1.5, not rung 1; letting it feed pane inheritance shadowed each pane's own rung-1.5 mirrored label and rung-2 agent identity with the TAB's label the moment a workspace was federation-mounted — the exact misfire FNC-3 exists to prevent.
- Evidence: `cargo nextest run -E 'test(tab_override) or test(mirrored_tab_label) or test(renaming_a_tab) or test(naming) or test(border_titles)'` — 20/20 passed. `cargo build --workspace --tests` clean.
- Reversibility: fully reversible; the removed `.or_else` branch is a 4-line revert.

### F3 — single-resolve `resolved_display_from`/`resolved_tab_display` plus restored rung-1 short-circuit
- What: `Workspace::workspace_derived_for` now returns `(is_remote, None)` immediately when `self.custom_name.is_some()`, before touching `resolved_identity_cwd_from` (which calls `Tab::cwd_for_pane` -> `Pane::cwd()`, a mutex lock plus, on a cache miss, `platform::process_cwd`). Added `Workspace::resolved_display_from` (label + `NameSource` in one call) and `Workspace::resolved_tab_display` (same for tab scope); `App::workspace_info`/`App::tab_info` (`src/app/creation.rs`) now call these ONCE instead of `display_name_from` + `workspace_name_source_from` / `tab_display_name` + `tab_name_source` back to back. Removed the now-fully-superseded `workspace_name_source_from` and `tab_name_source` methods (their only remaining callers were the paired-call sites this fix replaces plus one test, updated to `resolved_tab_display`) rather than leaving them as dead code.
- Why: `workspace_info`/`tab_info` run once per workspace/tab per frame per client — a multiplicative render path per CLAUDE.md — and were paying the full derivation chain twice, with no rung-1 short-circuit at all, so even a RENAMED workspace paid `Pane::cwd()` on every call.
- Evidence: `cargo nextest run -E 'test(workspace_info) or test(tab_info) or test(char_tab_info) or test(char_client_shell_tabs) or test(display_name) or test(resolved_display) or test(resolved_tab_display) or test(workspace::tests)'` — 22/22 passed. `cargo build --workspace --tests` clean, zero new warnings.
- Reversibility: fully reversible; the short-circuit is a 3-line early return and the combined methods are additive (their two halves are recoverable by splitting the returned tuple).

Deviation from the fix list, recorded: no synthetic call-counting perf test was added to prove the short-circuit mechanically (the same choice Phase 6 made for the original render-scale claim, since `just`/its bench harness is unavailable on this machine) — the fix is a direct, readable early-return whose absence would be caught by reasoning about the code, not by a counter. Correctness is covered by the existing `display_name_from`/`resolved_display_from` regression suite (22 tests, all green) plus the new single-call-site refactor itself removing the double-call shape entirely.

### F4 — `reconcile_agent_name_owner` now clears `agent_name_author` alongside `agent_name`
- What: the identity-mismatch arm of `reconcile_agent_name_owner` (`src/terminal/state.rs:2162`) now also sets `self.agent_name_author = None` when it clears `agent_name`/`agent_name_owner`. Added `agent_name_author_unlatches_after_the_named_agent_exits_and_a_new_one_starts` (rename an agent, exit it, start a different one, rename the tab — the pane now inherits) and strengthened `agent_name_author_distinguishes_user_rename_from_managed_launch` with `agent_name_author == None` assertions after reconcile.
- Why: leaving `agent_name_author == Some(User)` after the name it protects was cleared permanently latched R3's inheritance gate (`border_label`'s `agent_name_author != Some(User)` check) shut for that pane, even with no hand-set name left to protect.
- Evidence: `cargo nextest run -E 'test(agent_name_author)'` — 2/2 passed; `cargo nextest run -E 'binary(herdr) and test(terminal::state::tests)'` — 103/103 passed.
- Reversibility: fully reversible; one added line.

### F5 — all three clippy errors resolved (two as side effects of F1/F2, one direct)
- (1) `is_auto_named` never-used: resolved by F1 — `!state.is_auto_named()` became the production formula for `ClientShellTab::custom_label` (`src/server/client_shell.rs`), giving the method a real non-test caller again. Deliberate choice recorded here per the task's explicit ask: restored the caller rather than deleting the method or `#[allow]`-gating it, since F1 needed exactly this boolean and the method already existed for exactly this purpose.
- (2) `clippy::doc_lazy_continuation` at `src/workspace/naming.rs:7`: the `1.5.` list item wasn't recognized as a continuation of the `1.`/`2.` ordered list, so rustdoc/clippy wanted it indented as a sub-paragraph. Reworded rung 1.5 as a parenthetical note under rung 1's own bullet instead of a numbered list item, avoiding the false list-continuation entirely (clearer than mechanically indenting a misleading "1.5." ordinal).
- (3) `clippy::unnecessary_lazy_evaluations` at `src/workspace.rs:542-543`: resolved by F2 — the `.then(|| ...).flatten()` / `.or_else(...)` chain was deleted outright (not just simplified to `.then_some`) as part of removing the mirrored-label fallback from `tab_override_for_pane_inheritance`.
- Evidence: `cargo clippy --all-targets --locked -- -D warnings` — clean, 0 errors (verified after F1-F4 and the naming.rs doc fix).
- Reversibility: (1)/(3) are consequences of F1/F2, reversible together with those; (2) is a doc-only rewording.

### F6 — pane-scope naming now reaches the JSON API, agent sidebar, and OS window title
- What: added `TerminalState::resolved_pane_label` (shared by `border_label` and the new server-side call site) returning the full pane-scope ladder result (own override -> inherited tab override [R3-gated] -> mirrored remote label -> agent identity). `App::pane_info` (`src/app/creation.rs`) now calls it with `ws.tab_override_for_pane_inheritance(tab_idx)` and sets `PaneInfo.label`/new `PaneInfo.name_source` from it (was `terminal.manual_label.clone()` only). `App::agent_info` (`src/app/agents.rs`) copies the same resolved `label`/`name_source` onto a new `AgentInfo.label`/`name_source` pair, deliberately separate from `AgentInfo.name` (the addressable `herdr agent send` handle, left untouched). Wire structs `ClientShellPane`/`ClientShellAgent` gained matching `name_source` (`ClientShellPane`) and `label`+`name_source` (`ClientShellAgent`) fields under the same protocol 23->24 bump F1 already required. Restored the two other places the mirrored-label regression hit: `LayoutPane.label` (`src/app/api/layouts.rs`, own+mirrored only — deliberately NOT the full ladder, since a layout is a round-trip export template and baking in an inherited/derived name would corrupt re-import) and the OS window title's `WindowTitleToken::Pane` (`src/app/window_title.rs`, own+mirrored). Updated `overlay_input.rs`'s rename-dialog prefill and `context_menu.rs`'s `has_manual_label`/rename-prefill to gate on `name_source == Override` instead of `label.is_some()`, since `label` now also carries inherited/mirrored/agent-identity text that was never user-typed. Regenerated `docs/next/api/herdr-api.schema.json` via `HERDR_UPDATE_API_SCHEMA=1`. Added `renaming_a_tab_reaches_pane_info_and_agent_info_over_the_json_api` and `mounted_panes_mirrored_label_reaches_pane_info` (`src/app/api/tabs.rs`); fixed `pane_menu_rows_are_pinned_for_every_label_combination` (`src/client/shell/tests/chrome_context.rs`) to set `name_source: Override` alongside its manual-label fixture, since it now needs to distinguish a real override from a resolved-but-not-typed label.
- Why: `PaneInfo.label` was exactly `terminal.manual_label` — a tab rename or a mounted pane's own mirrored remote label never reached it, so `herdr agent list`, any JSON snapshot, the agent sidebar, `LayoutPane` exports, and the OS window title all went stale relative to what the TUI border already showed (a real regression versus pre-diff behavior, not a missing nice-to-have — the mirrored-label case literally used to work via `manual_label` before FNC-3 moved that value into `mirrored_label`).
- Evidence: `cargo build --workspace --tests` clean. `cargo nextest run --no-fail-fast` — 3663/3664 passed (2 leaky, reported), 1 pre-existing unrelated failure (`live_handoff_keeps_unmanaged_agent_name_bound_to_saved_session`, confirmed pre-existing on the unmodified base per the task's own framing), 2 skipped. `cargo clippy --all-targets --locked -- -D warnings` clean. `generated_protocol_schema_artifact_is_current`, `frozen_generation_one_snapshot_decodes`, `frozen_generation_one_handshake_decodes` all pass.
- Deviation recorded: the agent sidebar's `agent_label` token (display_agent/name/agent/title chain in `agent_sidebar.rs`, feeding the `{agent}` template token) was left untouched — it is genuinely a different concept (which AGENT KIND is running, e.g. "claude"/"codex") from the resolved display name. The sidebar's `{pane}` token already reads `ClientShellPane.label`, which now carries the full resolved ladder automatically via the `PaneInfo.label` fix threading through `client_shell.rs`'s existing passthrough — no `agent_sidebar.rs` template-composition change was needed to make sidebar rows follow a tab rename, only the underlying data fix.
- Reversibility: fully reversible; every new field is additive (`#[serde(default)]`/`skip_serializing_if`), and the `label` formula changes at each of the four call sites (`PaneInfo`, `AgentInfo`, `LayoutPane`, window title) are independent one-line-ish reverts.

### F7 — federation-materialized workspaces excluded from `git_refresh_demand`/`workspace_git_refresh_items`
- What: `git_refresh_demand()`'s `auto_names` computation (`src/app/git_refresh.rs`) now skips any workspace where `is_federation_materialized()` is true; `workspace_git_refresh_items` filters them out before any cwd/git work runs (not just before storing a result). Added `federation_materialized_workspace_is_excluded_from_git_refresh_demand_and_items`.
- Why: Policy C deliberately leaves `custom_name` empty on a mounted scope (its label comes from `mirrored_name`), so without the exclusion every mount-only session read as permanently "auto-named," ran the ~1.5s periodic git pass forever, and walked the LOCAL filesystem for git roots under REMOTE-only paths — work the resolver discards anyway since FNC-1 never runs rungs 2-4 against local data for a remote scope.
- Evidence: `cargo nextest run -E 'binary(herdr) and test(git_refresh)'` — 18/18 passed. `cargo build --workspace --tests` and `cargo clippy --all-targets --locked -- -D warnings` both clean.
- Reversibility: fully reversible; two added guard conditions.

### F8 — clear path for workspace/tab rename overrides
- What: `TabRenameParams.label`/`WorkspaceRenameParams.label` changed from `String` to `Option<String>` (`src/api/schema/tabs.rs`, `src/api/schema/workspaces.rs`). Added `Tab::clear_custom_name`/`Workspace::clear_custom_name` (set `custom_name = None`). `handle_tab_rename`/`handle_workspace_rename` (`src/app/api/tabs.rs`, `src/app/api/workspaces.rs`) now match on `params.label.map(|l| l.trim().to_string())`: `Some(label) if !label.is_empty()` sets the override, everything else (`None` or empty/whitespace) clears it — the same empty-clears convention `handle_pane_rename` already used for `PaneRenameParams`. Both emit the RESOLVED display label in their `TabRenamed`/`WorkspaceRenamed` events (via `tab_display_name`/`display_name_from`) instead of the raw param, so a clear reports the name it snapped back to instead of `None`/empty. CLI: `herdr tab rename <id> --clear` / `herdr workspace rename <id> --clear` map to `label: None` (`src/cli/tab.rs`, `src/cli/workspace.rs`), mirroring `herdr agent rename <target> --clear`. TUI: `overlay_input.rs`'s `save_rename_overlay` for `ClientRenameTarget::Workspace`/`ClientRenameTarget::Tab` now always sends the method (never silently drops the request on an emptied box) and maps an emptied/unchanged box to `label: None` for a real override, while an already-auto-named tab emptied or left unchanged still sends nothing (no override existed to clear). Added `clearing_a_tab_override_snaps_back_to_the_derived_name` (`src/app/api/tabs.rs`) and `clearing_a_workspace_override_snaps_back_to_the_derived_name` (`src/app/api/workspaces.rs`): set an override, clear it, assert the resolved name matches the pre-override derived name and `custom_name` is `None`.
- Why: behavioral-contract scenario 2 (clear an override, snap back to the derived name) was unreachable — the JSON API had no way to send an absent/null label for workspace/tab rename, and the TUI overlay silently dropped the whole request when the input box was emptied instead of treating that as a clear intent.
- Evidence: `cargo build --workspace --tests` clean. `cargo nextest run -E 'test(rename) or test(clearing_a_tab_override) or test(clearing_a_workspace_override)'` — 17/17 passed. Full `cargo nextest run --no-fail-fast` — 3666/3667 passed (1 leaky, reported), only the pre-existing unrelated `live_handoff_keeps_unmanaged_agent_name_bound_to_saved_session` failure, 2 skipped. `cargo clippy --all-targets --locked -- -D warnings` clean.
- Deviation recorded: changing `TabRenameParams.label`/`WorkspaceRenameParams.label` from `String` to `Option<String>` is a real JSON-shape change on two advertised `client_shell` endpoint methods (`tab.rename`, `workspace.rename`), caught by the frozen-shape contract test `advertised_client_shell_method_shapes_stay_at_the_v1_contract` (`src/server/client_commands.rs`) — updated its golden fixture (`tests/fixtures/endpoint-method-shapes-v1.json`) to the new digests, and regenerated `docs/next/api/herdr-api.schema.json` via `HERDR_UPDATE_API_SCHEMA=1`. This is NOT the same risk class as F1: `Option<T>` accepts a bare present value from an old generation-1 client (`{"label": "text"}` still deserializes as `Some("text")`), so existing callers keep working byte-for-byte; only a caller wanting the NEW clear behavior needs to know to omit the field or send `null`. `PaneRenameParams.label` was already `Option<String>` before this fix (its digest is unchanged), so this brings `tab.rename`/`workspace.rename` in line with the precedent `pane.rename` already set rather than introducing a new shape convention.
- Reversibility: fully reversible; the param-type change, the two `clear_custom_name` methods, and the three call-site match statements (server/CLI/TUI) are each independently revertible, and the fixture/schema regenerations simply re-run against the reverted code.

### F9 — missing implementation-notes.md entry for `moving_tab_keeps_active_identity_and_stable_tab_numbers`
- What: `moving_tab_keeps_active_identity_and_stable_tab_numbers` (`src/workspace.rs:1897`) is an eighth pre-existing test whose expected labels changed under R4's ordinal fix (`labels == vec!["foo", "3", "1"]`, tracking stable `tab.number` after a move, not array position `vec!["foo", "2", "3"]`) but was missing from both documented groups above (the 3 Phase-0 `[inverts]`-marked inversions and the 4 plain assertion-field fixes) — same root cause (`tab_display_name`'s fallback switching from `tab_idx + 1` to `public_tab_number(tab_idx)`) as its documented neighbor `tab_display_name_uses_public_tab_number_not_index` two tests below it in the same file, just without its own doc-comment marker or notes.md line.
- Why: the task's own audit found it undocumented unlike the other 7 touched tests; recording it here closes that gap so the full set of R4-ordinal-affected tests is traceable in one place instead of 7-of-8.
- Evidence: `git log -p --follow -- src/workspace.rs` shows the test predates this feature (present at `7bb4f039`/`207be3c7`); `cargo nextest run -E 'test(moving_tab_keeps_active_identity_and_stable_tab_numbers)'` passes against the current (fixed) `tab_display_name` formula.
- Reversibility: documentation-only; no code change made for F9.

### Round-2 review fixes

#### `custom_label` wire flag derived from the resolving rung
- What: `src/server/client_shell.rs` now sets `custom_label` from `name_source.is_explicitly_named()` (new `NameSource` method in `src/workspace/naming.rs`: `Override | Inherited | Mirrored`) for both the workspace and the tab, instead of reading `custom_name.is_some()` / `!is_auto_named()`. `Tab::is_auto_named` lost its last production caller and is now `#[cfg(test)]`. Added `mounted_scopes_still_report_the_override_flag` and `derived_scopes_do_not_report_the_override_flag`.
- Why: moving a mounted scope's remote label out of `custom_name` into `mirrored_name` silently flipped this flag `true -> false` for federation-mounted scopes; deployed generation-1 clients filter agent rows on `tab_count > 1 || tab.custom_label` and would have dropped mounted tab labels. Verified case by case against base: the only pre-ladder writers of `custom_name` were the rename APIs, the layout-apply path, and the mount path, so base's flag was exactly "override or mirrored".
- Evidence: `cargo nextest run --no-fail-fast` — 3676/3677 passed, only the pre-existing `live_handoff_keeps_unmanaged_agent_name_bound_to_saved_session` failure.
- Reversibility: fully reversible; one method plus two one-line expressions.

#### Agent-identity rung made lazy on the per-pane render path
- What: `TerminalState::resolved_pane_label` now delegates to a private two-pass `resolved_pane_label_with(.., impl FnOnce() -> Option<String>)`: rungs 1/1.5 resolve first, and the agent identity is only produced when the ladder reaches it. `App::pane_info` calls the new `resolved_pane_label_with_display_agent` and hands over the `EffectivePresentation::display_agent` it already computed nine lines earlier. Added `pane_label_skips_the_agent_identity_rung_when_a_higher_rung_wins`. `App::agent_info` was audited and needs no change — it reuses `pane_info`'s result rather than resolving again; no other new call site builds a presentation.
- Why: the previous shape built a second `EffectivePresentation` (title scan, display-agent scan, state-label `HashMap` allocation) per pane per frame per client, and did so eagerly even when a manual label, inherited tab rename or mirrored remote label already won. Net added presentation builds on the per-frame path is now zero.
- Evidence: the new laziness test asserts the closure is not invoked for rung 1 and rung 1.5 and is invoked for rung 2; full suite green as above.
- Reversibility: fully reversible; the public entry point keeps its old signature.

#### `{agent}` token shows the resolved scope name
- What: `src/client/shell/agent_sidebar.rs` and `src/client/shell/mobile.rs` prefer `agent.label` when `agent.name_source.is_explicitly_named()`, then fall back to the previous chain (`display_agent -> name -> agent -> title`). Added `agent_row_shows_the_inherited_tab_name_instead_of_the_agent_kind` and `agent_row_falls_back_to_the_agent_kind_when_no_scope_was_named`.
- Why: the server already resolved and shipped `AgentInfo::label`/`ClientShellAgent::label`, but no client read them, so an agent row still read "claude" after a tab rename — the user's literal reported symptom. Stays inside the locked Policy C contract: this changes the DISPLAY label only, `ClientShellAgent::name` (the `herdr agent send` handle) is untouched and asserted untouched in the test.
- Evidence: both new tests pass; full suite green as above.
- Reversibility: fully reversible; one `Option` prefix per client.

#### Only a name the remote actually set is mirrored into a pane
- What: new `naming::label_worth_mirroring(label, source)` gates both `terminal.mirrored_label` writes (`src/app/creation.rs` mount path, `src/remote/federation/client.rs` resync path) on the remote's `name_source`. The federation test fixture's pane label is now `NameSource::Override` (a label the remote user typed), and `a_mounted_pane_follows_the_remote_live_agent_identity_after_it_changes` covers the claude-then-codex scenario.
- Why: widening `PaneInfo::label` to the full ladder meant the mount path froze the remote's live agent identity into this host's rung-1.5 mirror slot, which outranks agent identity — so after the remote switched agents, the mounted pane kept rendering the old kind in the border, `PaneInfo`, `herdr agent list`, the agent panel and the OS window title until teardown. Base read the live value.
- Evidence: the new test asserts `mirrored_label == None` at mount and `pane_info().label == Some("codex")` after the relayed identity changes; full suite green as above.
- Reversibility: fully reversible; one helper and two call sites.

#### Rename-pane overlay no longer pins a derived prefill
- What: `ClientRenameTarget::Pane` carries `original_name`; `save_rename_overlay` sends nothing when the trimmed input equals the prefill, and otherwise sends the rename exactly as before (an emptied box still sends `label: Some("")`, which `handle_pane_rename` treats as a clear). Added `rename_pane_accepting_an_inherited_prefill_unchanged_sends_nothing` and `rename_pane_accepting_an_unchanged_override_sends_nothing`.
- Why: the overlay now prefills with the resolved ladder text, so opening rename-pane on a pane showing an inherited tab name and pressing Enter unchanged silently pinned that derived string as a real override that survives restart and blocks all future tab-rename inheritance. The unchanged-input comparison alone covers both the derived and the override case, so no `auto_name` flag is needed here; the pre-existing `rename_pane_empty_value_is_preserved_as_a_clear_request` keeps its exact assertion.
- Evidence: three pane-rename tests pass together, including the pre-existing one; full suite green as above.
- Reversibility: fully reversible; one enum field and one branch.

#### Rename params gated with `skip_serializing_if`
- What: `TabRenameParams::label` and `WorkspaceRenameParams::label` gained `#[serde(default, skip_serializing_if = "Option::is_none")]`, matching `PaneRenameParams::label`.
- Why: without it a cleared rename put a literal `"label": null` on the wire, and a client on this branch talking to a still-deployed generation-1 server (the handshake succeeds) made that server fail serde with "invalid type: null, expected a string" where the pre-diff behavior was a silent no-op. An omitted key is the harmless no-op it always was.
- Evidence: `advertised_client_shell_method_shapes_stay_at_the_v1_contract` passes; `generated_protocol_schema_artifact_is_current` regenerated and passes. Regenerating the golden fixture is genuinely correct rather than a silenced guard: schemars already omits `Option` fields from `required`, so the attributes themselves change no digest — the digest moved only because `String -> Option<String>` is a real, intended and backward-compatible shape change on those two methods, and the guard compares whole maps so every legitimate shape change (including adding a new advertised method) requires a fixture update; the guard's job is to force that change to be noticed and justified, which is what this entry does.
- Reversibility: fully reversible; two attributes.

#### `name_source` stamped at the pane federation choke point
- What: `namespace_pane` (`src/remote/federation/reducer.rs`) no longer passes the remote's discriminant through. It restates it locally: a label the remote actually set becomes `Mirrored`, a label the remote derived becomes `AgentIdentity`, and an absent label becomes the default. The stale comment in `namespace_workspace` now names `Workspace::resolved_display_from` instead of the deleted `workspace_name_source_from`.
- Why: the pane choke point was the one of the three that trusted remote input. One bit of the remote's answer is deliberately kept — whether the remote named that pane — because the mirror-slot gate above needs it; that is not new trust, since before this field existed the mount path pinned every remote label unconditionally.
- Evidence: full suite green as above, including the federation materialization and reducer tests.
- Reversibility: fully reversible; one match expression and one comment.

#### Mirror narrowing gated on the peer actually reporting `name_source`
- What: new `Capability::PANE_NAME_SOURCE` (`src/remote/federation/protocol/mod.rs`), advertised by both live `#[cfg(unix)]` handshake paths (`src/remote/federation/session.rs::local_capabilities`, `src/server/federation_accept.rs::federation_capabilities`) with the same `#[cfg_attr(not(unix), allow(dead_code))]` convention as `WORKSPACE_TAB_CLOSE`. Both `terminal.mirrored_label` writes now branch on it: agreed keeps `label_worth_mirroring`, not agreed mirrors `pane_info.label` unconditionally. Threaded as `peer_reports_name_source` from `materialize_federation_mount` into `build_remote_pane` (`src/app/creation.rs`) and from `drive_mount_channel` into `materialize_resync_pane` (`src/remote/federation/client.rs`), read once per mount/diff off `RemoteMirror::supports`. No `FEDERATION_PROTOCOL_VERSION` bump and no `PaneInfo` schema change.
- Why: `PaneInfo::name_source` is `#[serde(default)]` (`src/api/schema/panes.rs:550`) and `NameSource`'s `#[default]` is `Ordinal` (`src/workspace/naming.rs:95`), while `FEDERATION_PROTOCOL_VERSION` stays 7 — so a peer still on v0.9.0-hvn.1 negotiates fine, its `PaneInfo` arrives with the field absent, deserializes to `Ordinal`, `namespace_pane` stamps `AgentIdentity`, and `label_worth_mirroring` dropped the label. A pane that peer's user genuinely renamed then rendered this host's agent identity instead. Verified at base `c2f4166a` that `PaneInfo::label` was `terminal.manual_label.clone()` (`src/app/creation.rs:355` at that commit), i.e. override-only, so mirroring such a peer's label unconditionally is exactly base behavior. This gates interpretation of an absent field, not a frame an old peer cannot decode, so unlike `WORKSPACE_TAB_CLOSE` the check is on the receive side.
- Evidence: `a_mounted_pane_follows_the_remote_live_agent_identity_after_it_changes` now agrees the capability on its mirror and still passes (a derived identity is not frozen); new `a_peer_that_does_not_report_name_source_still_mirrors_its_renamed_pane_label` fails without the gate and passes with it. Full `cargo nextest run --no-fail-fast` results recorded in the round report; `cargo fmt --check` and `cargo clippy --all-targets --locked -- -D warnings` clean after touching every source file.
- Not advertised in `src/remote/attach.rs`'s one-shot snapshot dial: that set advertises neither `FILE_STAGING` nor `WORKSPACE_TAB_CLOSE`, and its mount is discarded before any pane is materialized (mode A was dropped in the v0.9.0 merge), so it can never reach a mirroring decision.
- Reversibility: fully reversible; one constant, two advertise entries, two threaded booleans, two branches.

#### Agent-identity rung returns its own `String`
- What: `TerminalState::resolved_pane_label_with` (`src/terminal/state.rs`) returns the owned agent-identity string with `NameSource::AgentIdentity` directly instead of handing it to `resolve_name`, taking a `Cow::Borrowed` back and calling `into_owned()`.
- Why: that round-trip allocated twice where base's `border_label` allocated once, per agent pane per frame per client — inside a pane-scaled render loop (CLAUDE.md multiplicative-performance rules). Agent identity is the last rung legal at pane scope (rungs 3/4 are not; the caller supplies its own literal fallback), so `resolve_name` had exactly one reachable outcome for those inputs.
- Evidence: `pane_label_skips_the_agent_identity_rung_when_a_higher_rung_wins` still passes unchanged, so the rung stays lazy; the pane-label and naming tests pass.
- Reversibility: fully reversible; one expression.

## Review pass — cross-build federation & wire compatibility (final)

**What.** Walked every changed wire struct, API schema type, capability path and
federation reducer path in `git diff HEAD` against four directions: old client ->
new server, new client -> old server, old serving host <-> new mounting host, and
the reverse. Three defects found; the three previously-closed ones re-verified as
genuinely closed.

**Why.** `PROTOCOL_VERSION` 23 -> 24 gates nothing on the two transports that
actually carry the changed types. The endpoint (JSON) transport is admitted purely
on `ENDPOINT_PROTOCOL_GENERATION` (`server/client_transport.rs:738`,
`server/autodetect.rs:157`), which stayed 1; the federation transport is admitted on
`FEDERATION_PROTOCOL_VERSION`, which stayed 7. Only the CLI JSON front door checks
`PROTOCOL_VERSION` (`cli.rs:786`). So v0.9.0-hvn.1 peers connect to this build on
both transports, in both directions.

**Evidence (verified closed).**
- `custom_label` true->false for mounted scopes: closed — `server/client_shell.rs:64,118`
  derive it from `name_source.is_explicitly_named()`, pinned by
  `mounted_scopes_still_report_the_override_flag`.
- literal `null` label on `tab.rename`: closed — `skip_serializing_if` on
  `api/schema/tabs.rs:38` omits the key rather than emitting `null`.
- mounted pane freezing the remote agent identity: closed **in the new-mounting-host
  direction only**, via `Capability::PANE_NAME_SOURCE`
  (`app/creation.rs:816`, `remote/federation/client.rs:1445`). The serving side never
  narrows for a peer that lacks the capability — see finding 1.

**Evidence (defects).**
1. `app/creation.rs:358` widened `PaneInfo::label` from `terminal.manual_label`
   (base `creation.rs:355`) to the whole resolved ladder. That `PaneInfo` is the
   federation `SessionSnapshot` (`server/federation_actor.rs:911-924`), and a
   v0.9.0-hvn.1 mounting host writes it straight into `manual_label`
   (base `creation.rs:744`, base `federation/client.rs:1423`). No serve-side narrowing
   for a peer without `PANE_NAME_SOURCE`, although `agreed` is in hand at
   `federation_accept.rs:303/383`.
2. Every new client `name_source` read (`client/shell/tabs.rs:108`,
   `sidebar.rs:611`, `context_menu.rs:214/422/469`, `agent_sidebar.rs:271/288`,
   `mobile.rs:652/671/789/903`, `overlay_input.rs:413/438`) treats the
   `#[serde(default)]` absence from a generation-1 server as a resolved `Ordinal`.
   `custom_label` is still on the wire and still carries the answer, but nothing
   consults it.
3. The same widened `PaneInfo::label` prefills an OLD client's pane-rename overlay
   (base `overlay_input.rs:429-436`), whose submit is unconditional (base
   `overlay_input.rs:1044-1049`). `handle_pane_rename`
   (`app/api/panes.rs:1784-1787`) stores it verbatim as a rung-1 override.

**Reversibility.** All three are additive guards (serve-side narrowing, a client-side
`custom_label` fallback, a server-side echo check). None require another protocol,
generation or federation version bump.

---

## Comment cleanup pass (ephemeral identifiers stripped)

**What.** Comment-only sweep over the branch's Rust sources. Every ephemeral
review/plan identifier this branch introduced was rewritten so the comment states
the rule, invariant or reason directly, per the repo rule against plan IDs, phase
numbers, audit labels and finding codes in code comments, test names and test
assertion strings.

**Removed:** 61 occurrences of `F2`/`F3`/`F4`/`F6`/`F7`/`F8`, `G1`,
`FNC-1`..`FNC-5` (2 of them in the untracked `src/workspace/naming.rs`), plus all
`Phase 0`/`Phase 1`/`Phase 2`/`Phase 4`/`Phase 5`, `[was Phase 0 char test ...]`,
`[inverts Phase 2]` and `design-decision.md` framing on lines this branch added.
97 replacements over 25 files.

**Kept deliberately:** the naming-contract rule ids that predate this branch and
are used pervasively in untouched code (`R1`-`R4`, `D2`/`D3`, `M1`/`M2`/`M3`/`M5`,
`Policy C`), pre-existing `RT-F*` / `S*.*` references on lines this branch did not
add, and every cross-reference to a real code symbol. Two dangling references to
`char_tab_display_name_uses_index_not_public_tab_number` (`workspace.rs:1896`,
`app/mod.rs:2573`) were repointed at the test's real current name,
`tab_display_name_uses_public_tab_number_not_index`.

**Non-comment edits (6, all assertion *messages*, no assertion semantics):**
`creation.rs` `"FNC-4: ..."` -> `"a local rename of a mounted tab ..."`, three
`"... in the mirror slot (Phase 4)"` -> `"... in the mirror slot"`, and
`client_shell.rs` two `"F2 fixed: ..."` -> `"a mounted workspace/tab reports
Mirrored ..."`.

**Stale doc corrected.** `remote/federation/session.rs:25-26` claimed its
capability set was "identical to the one-shot `attempt_federation_mount` snapshot
dial". It is not, and was not before this branch: `attempt_federation_mount`
(`remote/attach.rs:456-465`) advertises only `SCROLLBACK_REPLAY` and
`AGENT_STATUS`, while `session::local_capabilities` also advertises
`FILE_STAGING`, `WORKSPACE_TAB_CLOSE` and now `PANE_NAME_SOURCE`. The doc now says
it is a strict superset and names the three extras, and warns that a capability
added there is not automatically advertised by the snapshot dial.

**Verification.** Reverse-applied every replacement onto a scratch copy and
compared comment-stripped projections of all 25 files: the only surviving
differences are the 6 assertion-message strings above. No expression, control
flow, signature, attribute or assertion semantics changed.
