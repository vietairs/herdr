# b2.2 mutation-guard seam map (READ-ONLY investigation)

Repo: `/Users/hvnguyen/Projects/herdr`, branch `feat/remote-workspace-federation` (verified via `git branch --show-current`). No files modified; no build run (per instructions — build is remote-only).

## 1. The API method enum

Confirmed: `pub enum Method` is at `src/api/schema.rs:45-208` (not 66-207 as the phase-plan pointer said — plan reference is stale by ~21 lines but the enum itself matches). 81 variants, each `#[serde(rename = "...")]`. Full list, bucketed:

### (a) READ-ONLY queries (snapshot/list/get/status) — 26
`Ping`, `SessionSnapshot`, `WorkspaceList`, `WorkspaceGet`, `WorktreeList`, `TabList`, `TabGet`, `AgentList`, `AgentGet`, `AgentRead`, `AgentExplain`, `PaneProcessInfo`, `LayoutExport`, `PaneNeighbor`, `PaneEdges`, `PaneList`, `PaneCurrent`, `PaneGet`, `PaneRead`, `EventsSubscribe`, `EventsWait`, `PaneWaitForOutput`, `PluginList`, `PluginActionList`, `PluginLogList`, `ServerAgentManifests`

### (b) PRESENTATION/NAVIGATION (select/focus/scroll/activate) — 6
`WorkspaceFocus`, `TabFocus`, `AgentFocus`, `PaneFocus`, `PaneFocusDirection`, `PluginPaneFocus`

**Evidence caveat (important):** these are NOT purely non-mutating. `WorkspaceFocus` → `handle_workspace_focus` (`src/app/api/workspaces.rs:73`) → `AppState::switch_workspace` (`src/app/actions.rs:1011-1026`), which sets `self.active = Some(idx)`, `self.selected = idx`, and calls **`self.mark_session_dirty()`** (`src/app/actions.rs:1018`) — i.e. focus writes in-memory session state and would normally schedule a persisted save. This is currently harmless for a federated App only because b2.1 already made `SessionPersistencePolicy::Disabled` cover save-scheduling (per `implementation-notes.md:1266-1282`), not because focus itself is inert. Flagging this so the b2.2 allowlist isn't designed on the false premise that bucket (b) never touches `AppState`.

### (c) REMOTE INPUT (send input/keys) — 4
`PaneSendText`, `PaneSendKeys`, `PaneSendInput`, `AgentSend`

### (d) REMOTE-TERMINAL RESIZE — 0 Method variants (see below)
**No `Method` variant exists for "remote-terminal resize."** `Method::PaneResize` is exclusively **local split-geometry** resize: `handle_pane_resize` (`src/app/api/panes.rs:388-418`) calls `tab.layout.resize_pane(pane_id, direction, amount, area)` and `self.schedule_session_save()` on the tab's split-ratio layout tree — it has nothing to do with PTY/terminal rows×cols. It belongs in bucket (e) (mutation), confirming the phase doc's D4 note ("`pane.resize` disambiguated: local-split-geometry resize FORBIDDEN, remote-terminal resize ALLOWED").

The actual "remote-terminal resize" the plan means is a **separate, non-`Method` code path**: `TerminalRuntime::resize` (`src/terminal/runtime.rs:266`, delegating to `PaneRuntime::resize` at `src/pane.rs:2706`), driven by host terminal-window resize detection at `src/app/runtime.rs:192-193` (`self.last_terminal_size`), not by any API dispatch. A `Method`-enum allowlist structurally cannot see this call — it stays allowed automatically because it never goes through `handle_api_request`/`Method` at all. This is worth stating explicitly in the design doc: "remote-terminal resize" is out of scope for a Method-based guard, not an allowlist entry.

### (e) MUTATIONS — change local workspace/tab/pane STATE — 42
`ServerStop`, `ServerLiveHandoff`, `ServerReloadConfig`, `ServerReloadAgentManifests`, `WorkspaceCreate`, `WorkspaceRename`, `WorkspaceMove`, `WorkspaceReportMetadata`, `WorkspaceClose`, `WorktreeCreate`, `WorktreeOpen`, `WorktreeRemove`, `TabCreate`, `TabRename`, `TabMove`, `TabClose`, `AgentRename`, `AgentStart`, `PaneSplit`, `PaneSwap`, `PaneMove`, `PaneZoom`, `PaneLayout`, `LayoutApply`, `LayoutSetSplitRatio`, `PaneResize`, `PaneRename`, `PaneReportAgent`, `PaneReportAgentSession`, `PaneReportMetadata`, `PaneClearAgentAuthority`, `PaneReleaseAgent`, `PaneClose`, `IntegrationInstall`, `IntegrationUninstall`, `PluginLink`, `PluginUnlink`, `PluginEnable`, `PluginDisable`, `PluginActionInvoke`, `PluginPaneOpen`, `PluginPaneClose`

**Naming trap — `PaneZoom` (`pane.zoom`):** the request's own bucket-(b) example text lists "zoom-view" as presumptively non-mutating. Source proves otherwise: `handle_pane_zoom` (`src/app/api/panes.rs:1085-1110`) calls `AppState::apply_pane_zoom` which mutates `tab.zoomed`, and on change calls `self.schedule_session_save()` (`panes.rs:1106-1107`) — this is persisted layout state, not ephemeral view state. The phase-09b plan doc itself lists "zoom" among the forbidden mutation family (`phase-09b-option-b-own-in-proc-session.md:143-144`: "rename/move/close/swap/zoom/layout/split-ratio"), which matches the source, not the bucket-(b) gloss. **Put `PaneZoom` in the FORBIDDEN/mutation set, not presentation.**

### (f) NOT cleanly in any of the 5 buckets — 3 (recommend default-FORBIDDEN, consistent with D4's "every other method")
`NotificationShow` (`src/app/api.rs:904`), `ClientWindowTitleSet`, `ClientWindowTitleClear` (`src/app/api.rs:907`) — client-display commands, not workspace/tab/pane state, but also not read-only queries or remote input. D4 says "EVERY other current/future API method default-FORBIDDEN," so a closed allowlist naturally forbids these by omission; no special-casing needed.

## 2. The dispatch entrance(s)

Confirmed **the phase-plan's "two entrances" framing is materially correct, but the plan's own line-number pointers (app/api.rs:41 deferred, :450 respawn) are stale** — current locations:

**Entrance A — the single production funnel, `handle_api_request_message`:**
`src/app/runtime.rs:60-90` (`pub(super) fn handle_api_request_message(&mut self, msg: crate::api::ApiRequestMessage) -> bool`). This is the one place `ApiRequestMessage` (network/API-socket traffic, `src/api/mod.rs:67-69`) enters the App. It does the deferred/sync split itself:
```rust
if matches!(&msg.request.method,
    crate::api::schema::Method::WorktreeCreate(_) | crate::api::schema::Method::WorktreeRemove(_))
{
    ...
    let deferred_changed = self.handle_deferred_worktree_api_request(msg.request, msg.respond_to);
    ...
}
let response = self.handle_api_request(msg.request);
```
(`src/app/runtime.rs:71-89`). This is the **best single choke point** — it sees every real API request exactly once, before either sub-path, and already has `&mut self` (App), so it can read a future `federated`/`persistence` flag directly.

**Entrance A′ — headless/server-mode mirror:** `src/server/headless.rs` around line 3040-3060 duplicates the same WorktreeCreate/Remove-vs-synchronous split for the headless-server ownership of App. Same method-matching pattern, separate call site. A guard placed only in `runtime.rs` will NOT cover this path — headless.rs has its own copy and needs its own gate (or the classifier needs to be factored into a shared fn both call).

**Entrance B — the synchronous giant match:**
`handle_api_request` (`src/app/api.rs:829-832`) → `handle_api_request_after_internal_events_drained` (`src/app/api.rs:834-843`), whose `match request.method { ... }` starts at `src/app/api.rs:843`. This is where all 81 variants except `WorktreeCreate`/`WorktreeRemove` are actually handled.

**Entrance C — the deferred worktree path:**
`handle_deferred_worktree_api_request` (`src/app/api/worktrees/deferred.rs:15-30`):
```rust
match request.method {
    crate::api::schema::Method::WorktreeCreate(params) => { self.start_api_worktree_create(...); true }
    crate::api::schema::Method::WorktreeRemove(params) => { self.start_api_worktree_remove(...); true }
    _ => false,
}
```
Only handles those two variants; falls through with `_ => false` for everything else (caller then routes to Entrance B).

Note also two internal dispatch helpers that funnel App-internal (non-network) synthetic requests through the same code: `dispatch_api_request`/`dispatch_deferred_api_request` (`src/app/api.rs:29-56`), re-exported as `dispatch_runtime_mutation`/`dispatch_deferred_runtime_mutation` (`src/app/runtime_mutations.rs:12-20`). These are used by internal call sites (e.g. `runtime_workspace_focus`) to build `Method` values and call the *same* `handle_api_request`/`handle_deferred_worktree_api_request` — so gating at Entrance B/C also covers internally-synthesized requests, not just network ones. All the `app.handle_api_request(...)` call sites in `src/app/mod.rs`, `src/app/api/panes.rs`, `src/app/api/plugins/mod.rs`, etc. that looked like extra entrances are inside `#[cfg(test)]` modules only (verified: `src/app/mod.rs:1760-1761 mod tests`, `src/app/api/panes.rs:1876 #[cfg(test)]`) — not production traffic.

## 3. The local-spawn seam

Confirmed `spawn_with_portable_pty` exists as a per-platform pair:
- `src/pty/backend/unix.rs:12-40` (unix impl, opens `native_pty_system().openpty(...)`, spawns the child)
- `src/pty/backend.rs:17` (re-export/dispatch surface for the platform-specific impl)

**It has exactly one call site in the whole tree:** `src/pane.rs:2182`, inside `PaneRuntime::spawn_command_builder` (private fn, `src/pane.rs:2145-2182`):
```rust
let spawned = crate::pty::backend::spawn_with_portable_pty(rows, cols, cmd)
    .inspect_err(|err| error!(pane = pane_id.raw(), err = %err, "{spawn_error_message}"))?;
```
`spawn_command_builder` is itself the funnel for every local-PTY-creating public API: `PaneRuntime::spawn` (`src/pane.rs:1673`), `spawn_with_initial_history` (thin wrapper), `spawn_shell_command` (`src/pane.rs:1914`), `spawn_argv_command` (`src/pane.rs:1946`) — all route through `spawn_command_builder` → `spawn_with_portable_pty`. `TerminalRuntime::spawn`/`spawn_with_initial_history`/`spawn_shell_command`/`spawn_argv_command` (`src/terminal/runtime.rs:80-230`) are thin 1:1 wrappers over the `PaneRuntime` equivalents, so they inherit the same funnel.

**`spawn_remote` is confirmed structurally separate:** `PaneRuntime::spawn_remote` (`src/pane.rs:1763-1800+`, wrapped by `TerminalRuntime::spawn_remote` at `src/terminal/runtime.rs:148-184`) never calls `spawn_with_portable_pty` — it builds a `GhosttyPaneTerminal` fed by `out_tx`/`output_rx`/`clipboard_tx` (federation channels) with an explicit source comment: "No local child ever exists for a remote-backed pane: `child_pid` stays 0" (`src/pane.rs:~1796`). Both are currently marked `#[allow(dead_code)]` / dormant outside tests (no CLI switch wires them yet — consistent with the plan's P5/P8 staging notes), but the code-path separation itself is real and already correct today: **the local-runtime-creation seam and the remote-pane path do not share a call site**, so a guard placed at `spawn_command_builder` (or its call to `spawn_with_portable_pty`) would categorically never block `spawn_remote`.

**Cost note for the "MINIMAL GUARD RECOMMENDATION" below:** the plan's own text (`phase-09b-option-b-own-in-proc-session.md:143-145`) says the low-level guard should also cover `navigate.rs:769/880/935`, `api.rs:450` (respawn), `agent_resume.rs:204`, `panes.rs:777` (move) — i.e. it wants defense-in-depth at multiple *call sites of* `spawn_command_builder`'s callers, not just the funnel itself. Those line numbers have drifted (current: `src/app/input/navigate.rs:845` `spawn_custom_command`, `:935` `spawn_pane_command`, `:1011` `spawn_overlay_argv_command`; `src/app/api.rs:491-509` `respawn_shell_for_launch_pane`). None of `PaneRuntime`/`TerminalRuntime`/`pty::backend` currently has any App-level context (no `&App`, no `persistence`/`federated` field) threaded into `spawn_command_builder` — gating there would require adding a new parameter (or a thread-local/global flag) through 4+ call-chain signatures (`spawn`, `spawn_with_initial_history`, `spawn_shell_command`, `spawn_argv_command`, `spawn_command_builder`), a materially larger blast radius than gating once at Entrance A where `self: &App` is already in scope.

## 4. Existing mode/kind flag on App

**No `federated: bool` or `SessionKind`/`Mode`-style discriminator exists yet on `App`.** Grepped `src/app/mod.rs` struct fields (`src/app/mod.rs:95-165` region) and found `enum Mode` does not appear as a top-level App-mode type (only local UI modes like `Mode::Terminal` referenced from `super::Mode` in `src/app/api.rs:16`, which is a different, UI-focused enum — not a federation marker).

The closest existing precedent is `pub(crate) persistence: SessionPersistencePolicy` (`src/app/mod.rs:104-106`), an enum at `src/app/mod.rs:218-227`:
```rust
pub(crate) enum SessionPersistencePolicy {
    Enabled,
    // Constructed by the federated constructor (b2.3) and the b2.1 tests; the
    // classic path only ever builds `Enabled`, so keep the variant until wired.
    #[allow(dead_code)]
    Disabled,
}
```
This was shipped in **b2.1** (7dc71ec, per `implementation-notes.md:1268,1276-1277`) and is explicitly scoped to **persistence** (restore/save/exit-save/history/clear) — not to mutation-gating. The comment at `src/app/mod.rs:223-225` states it's constructed by "the federated constructor (b2.3)" which **has not landed yet** (b2.2, this task's target, comes before b2.3 per the brick ordering in `implementation-notes.md:1266-1282`). So as of this snapshot:
- `App` has no field today that a b2.2 dispatch classifier can branch on to know "am I a federated session?" — `persistence.is_disabled()` (`src/app/mod.rs:232`) is the only currently-constructible signal, and it is semantically about persistence, not about mutation permission, even though a federated session will always set both together.
- b2.2 (this brick) will need EITHER (i) a new dedicated `federated: bool`/`SessionKind::Federated` field added now (clean, self-documenting, matches the plan's request for a mutation policy that's a first-class concept), OR (ii) reuse `self.persistence.is_disabled()` as the gate predicate (zero new state, but couples "may this App persist to disk" with "may this App mutate local runtime state" — those happen to co-occur for the one federated caller today, but conflating them is a latent trap if a future non-federated caller ever wants `Disabled` persistence without the mutation lockdown, or vice versa).

## MINIMAL GUARD RECOMMENDATION

**Two choke points, not one — both are needed for the "closed" guarantee, for different reasons:**

1. **Primary: a `Method`-classifier gate at Entrance A** (`handle_api_request_message`, `src/app/runtime.rs:60-90`), duplicated at its headless mirror (`src/server/headless.rs` ~3040-3060) — or better, factor the classifier into one shared `fn is_allowed_for_federated(method: &Method) -> bool` in `src/api/mod.rs` (next to the existing `request_changes_ui`, `src/api/mod.rs:21-65`, which is the established precedent for this exact "match on `&Method`, `matches!` over an explicit variant list" pattern) and call it from both `runtime.rs` and `headless.rs` before the deferred/sync split. This is cheap (one new fn + two call sites), covers all 81 `Method` variants including future ones by being a closed allowlist (`matches!(.., A | B | C)` → default `false`), and has `&App` in scope already for whatever flag b2.3 adds.
2. **Secondary/defense-in-depth, optional for b2.2's minimal scope: the `spawn_command_builder` funnel** (`src/pane.rs:2145`, sole caller of `spawn_with_portable_pty` at `src/pane.rs:2182`) is the *complete* backstop against any Method-classifier bug or future bypass (internal `dispatch_*` helpers, a new call site that forgets the gate, etc.) — since literally every local-PTY spawn funnels through this one fn. But wiring a flag into it costs 4+ signature changes down the `TerminalRuntime`/`PaneRuntime` chain for a fn that currently has zero App-awareness. **Recommend deferring this to b2.3** (when `App::new_federated` exists and can thread a marker down) rather than doing it in b2.2, per YAGNI — b2.2's Method-classifier alone already prevents every API-driven path to `spawn_command_builder` (all pane/tab/workspace creation and respawn go through `Method` variants in bucket (e)), so the *reachable* local-spawn seam is already covered without touching `pane.rs`.

**Exact closed ALLOWED set (33 variants) — everything else forbidden by omission:**
- Read-only (26): `Ping`, `SessionSnapshot`, `WorkspaceList`, `WorkspaceGet`, `WorktreeList`, `TabList`, `TabGet`, `AgentList`, `AgentGet`, `AgentRead`, `AgentExplain`, `PaneProcessInfo`, `LayoutExport`, `PaneNeighbor`, `PaneEdges`, `PaneList`, `PaneCurrent`, `PaneGet`, `PaneRead`, `EventsSubscribe`, `EventsWait`, `PaneWaitForOutput`, `PluginList`, `PluginActionList`, `PluginLogList`, `ServerAgentManifests`
- Presentation/navigation (6): `WorkspaceFocus`, `TabFocus`, `AgentFocus`, `PaneFocus`, `PaneFocusDirection`, `PluginPaneFocus`
- Remote input (4): `PaneSendText`, `PaneSendKeys`, `PaneSendInput`, `AgentSend`
- Remote-terminal resize: **not a `Method` variant** — nothing to add to the allowlist; `TerminalRuntime::resize` (`src/terminal/runtime.rs:266`) bypasses `Method` dispatch entirely and stays reachable regardless of the guard.

**`PaneZoom` is explicitly EXCLUDED** from the allowed set despite superficially matching the "zoom-view" framing in the task's own bucket description — source (`schedule_session_save` on state change, `src/app/api/panes.rs:1106-1107`) and the phase-09b plan text both treat it as a mutation.

**App flag:** recommend a **new dedicated field** be added in b2.3 (e.g. `federated: bool` or fold it as a second purpose onto a renamed policy type), not silent reuse of `persistence.is_disabled()`, to keep "may persist" and "may mutate local state" as independently named, auditable predicates — cheap now, avoids a coupling trap later. This is a recommendation for b2.3's design, not something b2.2 needs to build; b2.2's classifier fn can accept the boolean/predicate as a plain parameter and not care what backs it.

## Unresolved / needs confirmation before implementation

1. Whether `ServerStop`/`ServerReloadConfig`/`ServerReloadAgentManifests` should be allowed for a federated session (they're process/config-level, not workspace-state, but D4's "every other method forbidden" language suggests forbidding them too — a federated session probably shouldn't be able to reload the *local* config or force-quit the local App remotely). Not decided by any source found; flagged as a product-intent question, not a code fact.
2. Whether the headless-server mirror (`src/server/headless.rs` ~3040-3060) is actually reachable in the federated-App configuration at all (a federated in-proc session may never run inside `HeadlessServer`) — if it's dead in that configuration, factoring the classifier into a shared fn is still cheap insurance but may not be strictly required. Would need b2.3's actual construction path to confirm.
3. `NotificationShow`/`ClientWindowTitleSet`/`ClientWindowTitleClear` bucket placement (my bucket (f)) — confirmed forbidden-by-default via D4, but whether product intent wants these specifically *allowed* (they look harmless — display-only, client-directed) is a judgment call, not something the source resolves either way.

Status: DONE
Summary: Method enum (81 variants, src/api/schema.rs:45-208) bucketed with 3 corrections to the naive framing (PaneZoom is a mutation not presentation; PaneResize is local-only, "remote-terminal resize" isn't a Method at all; focus/presentation methods do write AppState via mark_session_dirty). Both dispatch entrances found (src/app/runtime.rs:60-90 primary + src/server/headless.rs ~3040-3060 mirror), plus the deferred worktree sub-path (src/app/api/worktrees/deferred.rs:15). spawn_with_portable_pty has exactly one call site (src/pane.rs:2182 inside spawn_command_builder); spawn_remote (src/pane.rs:1763) is structurally separate and untouched by that seam. No federated/mode flag exists on App yet — closest precedent is SessionPersistencePolicy (src/app/mod.rs:218-227), explicitly deferred to b2.3.
Concerns/Blockers: 3 unresolved product-intent questions listed above; recommend a Method-classifier at Entrance A (+ headless mirror) as the b2.2-scoped choke point, deferring the spawn_command_builder backstop to b2.3 when App-awareness can be threaded in cheaply.
