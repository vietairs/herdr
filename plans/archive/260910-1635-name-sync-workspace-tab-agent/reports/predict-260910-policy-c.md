# predict — adversarial persona debate on Policy C

Read-only pass over worktree `herdr-name-sync` @ `c2f4166a`. All line numbers verified in-tree.
Scope: the Policy C file list only. Policy C itself is locked; this report attacks the *plan that has
not been written yet*, not the decision.

**Headline:** the two findings that should reshape the plan are P1 (the resolver's rung 3 lands inside
the per-render-tick client-shell snapshot build, which already does one `proc_pidinfo`/`readlink` per
workspace per frame per client) and M1 (`agent_name_owner` does not encode the fact Policy C wants to
gate on — the stated safety property has no backing field). A2 is the cheapest to act on and would
prevent inventing a discriminant that already exists on the wire.

---

## Ranked by expected cost

| # | Angle | Prediction | Cost | Cheapest mitigation |
|---|---|---|---|---|
| P1 | PERFORMANCE | Tab-scope cwd derivation multiplies per-frame `process_cwd` syscalls by tab count | **Very high** — regresses the guardrail CLAUDE.md names first | Resolver reads `Tab` cached scalars only; add tab cache fields fed by the git pass |
| M1 | MAINTAINER | `agent_name_owner` cannot answer "user-authored?"; the stated non-clobber gate is unimplementable as written | **Very high** — silent data loss on agent handles | Add an explicit `AgentNameAuthor` discriminant, or drop D3 inheritance for named agents entirely |
| A1 | ARCHITECT | Rung 3 is *not* shared: workspace = repo basename, tab = relative suffix, pane = n/a. Ladder unifies rungs 1/2/4 only | **High** — the DRY premise is 50% true | Name the resolver's contract per-rung; keep `NameDerivation` a scope-specific trait impl, not a match arm pile |
| U1 | USER | Tab derivation is dead for the exact cases it was requested for: sibling tabs in one repo all collapse to the same suffix or fall to ordinal | **High** | Require a distinctness pass: derived tab names must be unique within a workspace or fall back to ordinal |
| A2 | ARCHITECT | `NameSource` duplicates the existing `custom_label: bool` already computed server-side and shipped on the TUI wire | **High** | `NameSource` *replaces* `custom_label`; that is a positional bincode break → `PROTOCOL_VERSION` 23→24 |
| G1 | UPGRADER | Federation stores the remote's *resolved* label as a local `custom_name`, i.e. a derived name is promoted to a rung-1 override at the mount boundary | **High** | Carry `NameSource` over the fed wire; store remote-derived names in a new non-override field |
| U2 | USER | Displayed agent label diverges from the addressable handle; `herdr agent send <what-you-see>` starts failing | **Medium-high** | Show the handle alongside the inherited label, or gate D3 to agents with no `agent_name` |
| P2 | PERFORMANCE | The client-shell snapshot is rebuilt unconditionally each tick and only *then* compared, so resolver cost is paid even when nothing changed | **Medium-high** | Fold resolution behind the existing dirty signal, or make resolve allocation-free |
| M2 | MAINTAINER | `client_shell.rs` zips `snapshot.tabs` against `state.workspaces.flat_map(tabs)` — order desync mislabels tabs with no error | **Medium** | Characterization test with ≥2 workspaces × ≥2 tabs asserting label↔tab_id pairing |
| G2 | UPGRADER | Bumping `SNAPSHOT_VERSION` makes downgrade a hard restore failure, not a degrade | **Medium** | Do not bump; every new field `#[serde(default)]` |
| M3 | MAINTAINER | `Workspace::test_new` gives every test workspace a `custom_name`, so workspace→tab inheritance breaks the suite wholesale | **Medium** | Decide explicitly: inheritance is tab→pane only, never workspace→tab. Write it in the module doc |
| U3 | USER | `mobile.rs` renders auto-named tabs as `format!("tab {}", label)` → "tab src/detect" | **Low-medium** | Switch on `NameSource` at that call site |
| G3 | UPGRADER | Nullable rename params break new-client → old-server; and no UI affordance exists to *clear* a tab override | **Low-medium** | `#[serde(default)]` on the option; add "reset to auto" to the rename overlay |
| P3 | PERFORMANCE | The git pass that would feed tab derivation can be entirely OFF depending on sidebar config | **Low-medium** | Make tab-name demand a third `GitStatusRefreshDemand` flag |

---

## ARCHITECT — does one resolver beat three chains, or centralize the divergence?

### A1 — Rung 3 is not shared. The ladder unifies rungs 1/2/4 and *pretends* to unify rung 3.

Today's workspace derivation is `automatic_workspace_label(cwd, repo_root)` at
`src/workspace/git/discovery.rs:65-71`, which returns **`repo_root.file_name()` and ignores `cwd`
entirely** except in the fallback. Policy C's D1 wants a tab name that is "the path suffix relative
to the workspace's cwd" (design-decision §3). Those are two different functions of two different
inputs. Pane scope has no rung 3 at all — `src/workspace.rs:478` `tab_display_name` is the only
pane-adjacent chain and it never reads cwd.

So `resolve_name(scope, sources)` will contain a `match scope` whose rung-3 arm is three unrelated
bodies. That is the *definition* of centralizing divergence: today the three chains are 8, 12 and 4
lines and each is locally readable (`:478`, `:1067`, `:1085`); after C, a reader of the tab rule must
read past the workspace and pane rules to find it.

Where C *does* win is rungs 1, 2 and 4 — those genuinely are the same logic three times, and rung 4's
`tab_idx + 1` vs `public_tab_number` bug (`src/workspace.rs:481` vs `:1028`) is exactly the kind of
drift one resolver prevents.

**Mitigation (cheap):** keep the ladder shared and make rung 3 a per-scope input, not a per-scope
branch — the caller supplies `Option<DerivedName>` already computed by the scope's own cached field.
`resolve_name` then has no `match scope` at all and the file stays under 120 lines.

### A2 — `NameSource` is a second copy of a discriminant that already ships.

`src/server/client_shell.rs:64` sets `custom_label: state.custom_name.is_some()` for workspaces and
`:108` sets `custom_label: !state.is_auto_named()` for tabs (`src/workspace/tab.rs:200`). These land
on `ClientShellWorkspace.custom_label` (`src/protocol/wire.rs:1063`) and `ClientShellTab.custom_label`
(`:1096`) and are consumed in six places: `tabs.rs:107,112`, `mobile.rs:663,776,886`,
`agent_sidebar.rs:264`, `sidebar.rs:610`, `overlay_input.rs:413`, `context_menu.rs:416`.

Adding `NameSource` on `TabInfo`/`WorkspaceInfo`/`PaneInfo` (JSON API) while leaving `custom_label`
on the bincode wire leaves the codebase with two authorities for "is this a user name?", which is the
same failure Policy C exists to fix, one layer up.

Note also that `ClientShellTab`/`ClientShellWorkspace` are **positionally** bincode-encoded — the
`federation_origin` doc comment at `src/protocol/wire.rs:1071-1079` says so explicitly and forbids
`skip_serializing_if` for that reason. Replacing `custom_label: bool` with a `NameSource` enum is
therefore a hard wire break. `PROTOCOL_VERSION` is `23` at `src/protocol/wire.rs:20` and the latest
released fork tag is also protocol 23, so per CLAUDE.md the bump to 24 **is** required, not optional.

**Mitigation (cheap):** plan `custom_label` → `name_source` as one replacement in the same commit,
and grep the six consumer sites up front so none silently keeps the boolean.

---

## PERFORMANCE — where does resolve-time inheritance land in a hot loop?

### P1 — Tab-scope derivation lands inside the per-render-tick, per-client snapshot build.

The chain, verified end to end:

- `src/server/headless/render.rs:423` — `client_shell_snapshot(...)` is called **inside**
  `for (client_id, (cols, rows), cell_size, _is_foreground, mode) in render_targets`, i.e. once per
  attached ClientShell client, every render pass.
- `src/server/client_shell.rs:13` — that function's first statement is `app.session_snapshot()`.
- `src/app/api/session.rs:33-42` — `session_snapshot` loops **every workspace × every tab**, calling
  `workspace_info(ws_idx)` and `tab_info(ws_idx, tab_idx)`.
- `src/app/creation.rs:411` — `workspace_info` calls
  `ws.display_name_from(&terminals, &terminal_runtimes)`.
- `src/workspace.rs:1085-1096` → `resolved_identity_cwd_from` (`:1046`) → `tab.cwd_for_pane`
  (`src/workspace/tab.rs:532-547`) → `rt.cwd()`.
- `src/pane.rs:3813-3823` — `cwd()` returns the OSC-7 `reported_cwd` under a `Mutex` **if present**,
  otherwise falls through to `crate::platform::process_cwd(pid)`.
- `src/platform/linux.rs:386` = `std::fs::read_link("/proc/<pid>/cwd")` (filesystem I/O).
  `src/platform/macos.rs:936-947` = `libc::proc_pidinfo(PROC_PIDVNODEPATHINFO)` (process inspection).

CLAUDE.md's "Multiplicative performance paths" bans exactly this — "Do not … inspect process trees,
perform filesystem I/O … when one scalar fact is enough" — inside render- and client-fanout-scaled
loops.

**Current cardinality:** `W × clients` per frame. `T` does not appear because `tab_info`
(`src/app/creation.rs:236`) uses `ws.tab_display_name(tab_idx)`, which is a pure `Option<String>`
clone-or-format with no I/O.

**Post-C cardinality if rung 3 is added at tab scope:** `W × T × clients` per frame. On the CLAUDE.md
15-pane profile shape (say 5 workspaces × 3 tabs, 2 attached clients) that is 10 → 30 process probes
per frame. Panes without shell integration — an agent process, `ssh`, `vim`, anything that does not
emit OSC 7 — take the syscall branch every single time.

**Mitigation (cheap, and it is what the design already asks for):** the resolver must read a
`Tab`-level cached scalar (mirroring `Workspace::cached_auto_label` / `cached_identity_cwd`,
`src/workspace.rs:1099-1104`), never call `cwd_for_pane`. Refresh those fields only from the ~1.5s
git pass. Then bench per CLAUDE.md with `just bench-render-scale` at 1 and ≥15 panes and report the
delta. Treat any `cwd_for_pane` / `resolved_identity_cwd_from` call reachable from `naming.rs` as a
plan-level defect, and add a grep-based architecture test asserting `src/workspace/naming.rs`
contains no `cwd_for_pane`, `process_cwd` or `display_name_from` reference.

### P2 — The snapshot is built unconditionally, then compared.

`src/server/headless/render.rs:436` — `if client.shell_snapshot.as_ref() != Some(&candidate)`. The
dedup is on the *result*, so building `candidate` (and every string allocation inside it) is paid on
every tick whether or not anything changed. Any allocation the resolver adds is therefore
unconditional, not amortized by the "nothing changed" case.

**Mitigation (cheap):** return `Cow<'_, str>` / `&str` from the resolver where the winning rung is an
existing owned `String`, so rung-1 and rung-2 hits allocate nothing new.

### P3 — The pass that would feed tab derivation may never run.

`src/app/git_refresh.rs:97-100`: `git_refresh_deadline()` returns `Some` only when
`git_identity_refresh_requested || !git_refresh_demand().is_empty()`, and `git_refresh_demand`
(`:102-112`) is derived purely from whether the user's **sidebar config** contains `Branch` or
`GitStatus` tokens. A user with neither runs the periodic pass never; the only trigger left is
`src/app/api.rs:599`, `request_git_identity_refresh` on `terminal_cwd_reported` — which requires OSC 7.

So D2's promise ("clearing the override snaps to the correct current project with zero refresh delay",
design-decision §2) holds for shell-integration panes and silently does not for the rest. Same for
any tab-level cache fed by the same pass.

**Mitigation (cheap):** add a third flag to `GitStatusRefreshDemand` set whenever any workspace or tab
is auto-named, so name derivation keeps the pass alive independent of sidebar tokens.

Secondary note: `workspace_git_refresh_items` (`src/app/git_refresh.rs:114-133`) produces
**workspace-only** items — `workspace_id`, `resolved_identity_cwd`, `cache_key_hint`. There is no tab
cwd anywhere in the pass. "Tabs JOIN rather than duplicate" is only true when the tab's cwd shares the
workspace's git root; a tab that `cd`'d into another repo joins nothing (see U1). Extending the pass
means changing `WorkspaceGitRefreshItem`, `WorkspaceGitStatus`, `apply_workspace_git_statuses`
(`src/app/actions.rs:1526-1571`) and `deduplicate_git_refresh_items` together — size that as its own
phase, not as a field addition.

---

## MAINTAINER — what reads as surprising in six months? Which tests does C invert?

### M1 — `agent_name_owner` does not encode what Policy C gates on.

design-decision §"Accepted limitation": *"A tab override never clobbers a hand-set agent name:
inheritance applies only where `agent_name_owner` says the name was auto-assigned or
detection-owned."*

That field cannot say that. `AgentNameOwner` (`src/terminal/state.rs:103-107`) is
`{ agent_label: String, session_ref: Option<AgentSessionRef> }` — it records **which agent identity
owns the name**, so that `reconcile_agent_name_owner` (`:2102-2140`) can *clear* `agent_name` when the
owning agent changes underneath it. It carries no authorship bit.

And both writers go through the same setter:

- user rename → `src/app/agents.rs:131` `terminal.set_agent_name(name)`
- managed launch → `src/terminal/state.rs:1918` `begin_managed_agent` → `set_agent_name`
- restore → `src/terminal/state.rs:2049` `restore_managed_agent` → `set_agent_name`

`set_agent_name` (`:1883-1908`) derives the owner identically in all three cases, from
`hook_authority` → `persisted_agent_session` → `effective_agent_label`. A user-typed `reviewer` and an
auto-assigned `pi-1` are indistinguishable afterwards.

Implementing the gate as written therefore either (a) never fires, so tab renames clobber hand-set
agent names — user-visible data loss on a name `herdr agent send` depends on, or (b) always fires, so
D3 does nothing. A plan that writes "gated on `agent_name_owner`" without adding a field will ship one
of those two.

**Mitigation:** add an explicit author discriminant (`AgentNameAuthor::{User, Managed, Detected}`) set
at each of the three call sites above, and thread it into `PaneSnapshot.agent_name`
(`src/persist/snapshot.rs:103`) so it survives restore. Cheaper alternative if the plan wants to stay
small: gate D3 on `terminal.agent_name.is_none()` — any agent with *any* handle keeps its own label,
and inheritance only fills the empty case. Loses a little of D3's reach, costs nothing, and is
provably safe.

### M2 — the client-shell zip is an unguarded ordering assumption.

`src/server/client_shell.rs:90-99`:

```rust
let tabs = snapshot.tabs.into_iter()
    .zip(app.state.workspaces.iter().flat_map(|workspace| workspace.tabs.iter()))
```

The API-shaped `snapshot.tabs` is zipped positionally against a fresh traversal of `state`. It happens
to line up today because `session_snapshot` (`src/app/api/session.rs:33-38`) walks the same nesting in
the same order. Nothing enforces it, and there is no assertion. Any Policy C change that makes
`tab_info` skippable (a `?` that returns `None` for an unresolvable name, say — note `:236` is already
`ws.tab_display_name(tab_idx)?`, a `?` in a `filter_map`) shifts the zip and every subsequent tab gets
its neighbour's `custom_label` and `zoomed`. Compiles clean, tests that use one workspace with one tab
pass clean.

**Mitigation (cheap):** characterization test before any code moves — two workspaces × two tabs, one
renamed, assert `(tab_id, label, custom_label)` triples pairwise. Or make the zip key on `tab_id`.

### M3 — `Workspace::test_new` renames every test workspace, so workspace→tab inheritance detonates the suite.

`src/workspace.rs:1231` — `custom_name: Some(name.to_string())`. Every `Workspace::test_new("…")` is a
*renamed* workspace. Read rung 1 literally ("user override at this scope, **or inherited from the
nearest enclosing renamed scope**") and every tab in every test resolves to the workspace name.

The test that makes this concrete is `moving_tab_keeps_active_identity_and_stable_tab_numbers`
(`src/workspace.rs:1685-1707`): workspace `test_new("test")`, and `:1695-1696` asserts
`labels == vec!["foo", "2", "3"]`. Under literal workspace→tab inheritance those become
`["foo", "test", "test"]` — and two identically-named tabs is a strictly worse UI than two ordinals.

`src/workspace.rs:1225` also sets `identity_cwd = std::env::current_dir()`, so under rung 3 every test
tab derives from the checkout dir and all of them collapse to `herdr-name-sync`.

**Mitigation (cheap, but it is a decision the plan must make explicitly and record):** inheritance
flows **tab → pane/agent only**. A workspace rename never renames its tabs. Put it in the `naming.rs`
module doc with the reason (sibling collision), because the ladder's own wording says otherwise and
the next reader will "fix" it.

### M4 — tests that encode intent C inverts

- `src/app/api/agents.rs:698` `agent_rename_does_not_replace_the_pane_label` — design-decision already
  flags this. Confirmed it still passes under C (it asserts `manual_label` is untouched, and stores
  stay independent), but its *name* becomes an active lie once labels do couple at resolve time.
  Rename it to say what it protects: `agent_rename_leaves_manual_pane_label_untouched`.
- `src/app/mod.rs:2523` — `assert_eq!(tab.label, "2")`.
- `src/app/api/plugins/mod.rs:3179` — `assert_eq!(context.tab_label.as_deref(), Some("1"))`.
- `src/workspace.rs:1695` — `["foo", "2", "3"]` (M3).
- `src/app/api/layouts.rs:814` — asserts `Some("dev")`, an override; safe.

### M5 — the adversarial fixture cannot exercise the ladder

`Workspace::test_adversarial_identity_state` (`src/workspace.rs:1289-1320`) is built for *identity*
adversity — tab-number vs index divergence, raw pane id vs public number. Its only named tab
(`test_add_tab(Some("removed"))`, `:1298`) is immediately closed at `:1303`; every surviving tab is
unnamed. CLAUDE.md mandates this fixture for identity/state refactors, but as it stands it will pass
Policy C trivially without touching a single inheritance path.

**Mitigation (cheap):** extend the fixture with a surviving renamed tab whose ordinal differs from its
index, and a pane whose agent has a hand-set name — then `assert_invariants_for_test` gains a real
naming invariant (e.g. "no two auto-named tabs in one workspace resolve to the same string").

---

## UPGRADER — a user restoring a snapshot written by the current binary

Walking `src/persist/snapshot.rs` field by field:

`WorkspaceSnapshot` (`:50-69`): `id`, `custom_name`, `identity_cwd`, `worktree_space`,
`public_pane_numbers`, `next_public_pane_number`, `public_tab_numbers`, `next_public_tab_number`,
`tabs`, `active_tab`. `TabSnapshot` (`:85-95`): `custom_name`, `layout`, `panes`, `zoomed`, `focused`,
`root_pane`. `PaneSnapshot` (`:98-110`) carries `label`, `agent_name`, `managed_agent_kind`.

**Good news, verified:**

- **Ordinals survive restore.** `src/persist/restore.rs:357` reads `snap.public_tab_numbers.get(idx)`
  indexed against `snap.tabs`, not against the *restored* tab list, and the drop path
  (`restore.rs:690-700`, `continue` at `:373`) does not shift `idx`. Rung 4 is stable across a restore
  that loses a tab. Legacy snapshots with an empty `public_tab_numbers` fall to `idx + 1` — also
  positionally correct.
- **Federated workspaces are not persisted.** `src/persist/snapshot.rs:308-317` filters on
  `is_federation_materialized`. So G1 below is an in-session hazard only, not a persisted one.
- Every existing field is `#[serde(default)]` where optional, so an old binary reading a new file is
  fine *for fields it knows*.

### G1 — federation promotes a derived name to a rung-1 override at the mount boundary. **This is the worst upgrade-shaped finding.**

`src/app/creation.rs:561-565`:

```rust
let mut workspace = Workspace::from_existing_pane(
    Some(ws_info.label.clone()),
    Some(tab_info.label.clone()),
    ...
```

Both arguments become `custom_name`. And `src/app/creation.rs:1710-1716` does the same for a
resync-discovered tab, taking `RemoteTabRef.label` (`:2876`) into
`create_tab_from_existing_pane(moved, label, …)`.

`ws_info.label` / `tab_info.label` are the **remote's already-resolved** names. Today the remote's tab
label for an unnamed tab is the ordinal `"2"`; after Policy C ships on the remote it is a derived cwd
suffix. Either way the local server records it as an override — the one rung the design says is
"only authoritative state; the only thing persisted".

Consequences, all in-session:
1. A mirrored tab can never fall back, and a later remote rename cannot demote it.
2. Under D3, that pinned string is now inherited by remote agent panes' display labels — a
   remote-influenced string reaching an identity surface. It is sanitized
   (`src/remote/federation/reducer.rs:433-441`, `namespace_tab` → `sanitize_remote_string`) so this is
   not a spoofing hole, but the *semantics* change from "remote's display text" to "local user
   override", and `custom_label` will report `true` for a tab nobody named.
3. `NameSource` does not cross the federation wire at all today, so the local client cannot tell a
   mirrored derived name from a mirrored user name.

design-decision already quarantines federation behind a scoping pass and says implementation must not
begin until it is answered. **This finding is the concrete answer to give that pass**, and it is
larger than "does `NameSource` survive the wire": the mount path currently *destroys* the distinction
before the wire question even arises.

**Mitigation:** add a non-override mirrored-label field on `Workspace`/`Tab` that federation writes
instead of `custom_name`, and have the resolver treat it as its own rung between 1 and 2. If that is
too large, the minimum viable answer is: federation mounts set `NameSource::Remote` explicitly and
D3 inheritance is disabled for any workspace with `federation_origin.is_some()`.

### G2 — do not bump `SNAPSHOT_VERSION`.

`src/persist/snapshot.rs:12` — `SNAPSHOT_VERSION: u32 = 3`. `parse_snapshot` at `:501-509` **hard
errors** on `raw.version > SNAPSHOT_VERSION`, and so does `parse_history_snapshot` at `:512-521`.

So bumping to 4 means: user runs the new build once (session auto-saves), then rolls back to the
stable channel or has an older `herdr` server on another machine reading the same file → the entire
session fails to restore, not degrades. Given fork release history (multiple `-hvn.N` builds in
flight, per-host binary swaps), this is a plausible sequence, not a theoretical one.

**Mitigation (free):** keep `SNAPSHOT_VERSION = 3`. Every new field `#[serde(default)]`. Nothing
Policy C needs to persist is a *removal* — overrides are already `Option<String>` and derived names
are explicitly never stored.

### G3 — nullable rename params break in the new→old direction, and there is no clear-override UI.

`src/api/schema/tabs.rs:28-31` (`TabRenameParams { tab_id: String, label: String }`) and
`src/api/schema/workspaces.rs:45-48` (`WorkspaceRenameParams`) both have `label` as required. Making
them `Option<String>`:

- old client → new server: fine.
- new client sending `{"label": null}` or omitting → **old server**: serde rejects a missing required
  field. Relevant for a mixed-version fleet and for federated tab-create forwarding
  (`src/remote/federation/client.rs:3500` clamps a label before framing).

Separately: `src/client/shell/overlay_input.rs:1034-1043` guards against *creating* a no-op override
(`auto_name && trimmed == original_name` suppresses the request — good, that guard is what stops a
user accidentally pinning a derived name by pressing Enter). But there is **no path to clear an
existing tab override** from the TUI at all. Today that barely matters, because auto = ordinal. Under
Policy C, auto = a meaningful derived name, so "reset to auto" becomes a feature users will want and
cannot reach.

Also note the golden schema gate: `src/api/schema/tests.rs:182-207`
(`generated_protocol_schema_artifact_is_current`) will fail until
`docs/next/api/herdr-api.schema.json` is regenerated with
`HERDR_UPDATE_API_SCHEMA=1`. And `tab_label` is a documented socket-API field
(`docs/next/website/src/content/docs/socket-api.mdx:278`, mirrored in `ja/` and `zh-cn/`), so a
`name_source` addition wants a docs pass in all three.

**Mitigation (cheap):** `#[serde(default, skip_serializing_if = "Option::is_none")]` on the new
optional `label`, plus an explicit rename-overlay action that sends the null. Both are small; the
plan just has to know they exist.

---

## USER — where are the new derived names *worse* than today's ordinals?

### U1 — the monorepo case is the requested case, and derivation is worst exactly there.

design-decision §3 states the motivation: "a single-repo workspace does not render four tabs all
reading 'herdr'". But four tabs in one repo whose panes sit at the repo root — the overwhelmingly
common shape for a herdr workspace — have **identical cwds**. The relative suffix is empty for all
four. The rule then falls to rung 4 and produces `1 2 3 4`, i.e. exactly today's behaviour, after
paying P1's syscall cost per tab per frame to discover that.

The bad middle case is worse: tabs at `repo/src/app`, `repo/src/api`, `repo/src/app` (two panes in the
same subtree) → `src/app`, `src/api`, `src/app`. Two tabs now share a name, and unlike ordinals there
is no tiebreaker. Ordinals are ugly but *injective*; derived names are pretty and not.

Nested monorepos amplify: `packages/web/apps/admin/src` truncated to a tab-bar cell of ~12 columns
(`src/client/shell/tabs.rs:96-100` computes `desired_widths` then `width.min(remaining)`) shows a
middle-elided fragment that distinguishes nothing.

**Mitigation:** make distinctness a hard rule of rung 3 — after deriving all tab names in a workspace,
any name colliding with a sibling falls back to its ordinal (or gets the ordinal appended). Cheap,
deterministic, and testable without PTYs via `Workspace::test_new`.

### U2 — the visible agent name stops being the name you type.

D3 makes an agent pane's *display* label follow a tab rename while `TerminalState::agent_name` — the
thing `herdr agent send <name>` resolves (`src/app/agents.rs:104-112` conflict check;
`AgentInfo.name` at `src/api/schema/agents.rs:190`) — does not. The sidebar's precedence is
`display_agent → name → agent → title` (`src/client/shell/agent_sidebar.rs:266-270`), so a user who
renames a tab to `backend` sees `backend` in the sidebar and must still type `reviewer`.

design-decision accepts this knowingly, and that is fine as a decision. It is not fine as a *silent*
one: today the sidebar shows `name`, so what you see is what you type, and C breaks that
correspondence without any visual cue.

**Mitigation (cheap):** when an agent's displayed label came from tab inheritance and differs from its
handle, keep the handle visible — the sidebar token system already supports this
(`AgentSidebarToken::Agent` vs a second token, `src/ui/sidebar/tokens.rs:88-99`). Or take M1's cheap
alternative and simply do not inherit for agents that have a handle.

### U3 — panes whose cwd never resolves, and the mobile client's literal string

Two concrete spots:

- `src/client/shell/mobile.rs:886-890`:
  ```rust
  let label = if tab.custom_label { format!("{} · {}", index + 1, tab.label) }
              else { format!("tab {}", tab.label) };
  ```
  With today's ordinal that reads "tab 3". With a derived name it reads **"tab src/detect"**. Nothing
  will catch this except a human looking at the mobile client.
- `src/client/shell/tabs.rs:107-118` styles auto-named tabs `DIM`. A meaningful derived name rendered
  dim next to a bold override reads as "this tab is somehow disabled".
- `src/client/shell/agent_sidebar.rs:263-265` — `tab.filter(|tab| tab_count > 1 || tab.custom_label)`
  suppresses the tab token in single-tab workspaces. Under C the derived name is often the most useful
  token there, and it is exactly the one hidden.

A pane whose cwd never resolves (no OSC 7, `process_cwd` returns `None` — a died child at
`src/pane.rs:3821-3822` loads pid 0 → `None`) falls to rung 4 and renders an ordinal next to named
siblings. Mixed ordinal/name tab bars are the steady state, not the edge case.

Remote mounts: per G1, mirrored tabs will show whatever the remote resolved and are pinned there
forever, so a mixed local/remote sidebar shows three naming regimes at once.

**Mitigation (cheap):** switch each of those three call sites on `NameSource` rather than
`custom_label`, and decide once whether an auto-derived name renders dim (probably not; only an
ordinal should).

---

## What the plan should defend against — condensed

1. **Hard rule:** `src/workspace/naming.rs` may not reference `cwd_for_pane`, `process_cwd`,
   `resolved_identity_cwd_from`, or `display_name_from`. Enforce with a source-grep test. (P1)
2. **Decide and document:** inheritance is tab→pane/agent only. Never workspace→tab. (M3)
3. **Before any code moves:** characterization tests for the `client_shell` zip (M2), the four
   ordinal-asserting tests (M4), and an extended adversarial fixture with a surviving renamed tab and
   a hand-named agent (M5).
4. **`agent_name_owner` does not do what the design says.** Either add an author discriminant or gate
   D3 on `agent_name.is_none()`. Do not ship the sentence as written. (M1)
5. **`NameSource` replaces `custom_label`**, which is a positional bincode break →
   `PROTOCOL_VERSION` 23→24. Do **not** bump `SNAPSHOT_VERSION`. (A2, G2)
6. **Federation scoping pass must answer G1 first:** the mount path writes remote resolved labels into
   `custom_name` at `src/app/creation.rs:562-563` and `:1710-1716`. That is a pre-existing
   override-promotion bug that Policy C's D3 turns into an identity-surface issue.
7. **Rung 3 needs a distinctness rule**, or D1 delivers nothing in the single-repo case it was
   requested for. (U1)
8. **Bench with `just bench-render-scale`** at 1 and ≥15 panes and report the scaling delta, per
   CLAUDE.md — this change touches the render/client-fanout path by construction.

## Unresolved questions

1. Does workspace→tab inheritance exist at all? The ladder's wording says yes; every behavioral
   example says no; `Workspace::test_new` makes "yes" break the suite. Needs an explicit ruling
   before `naming.rs` is written.
2. What is the derived name for **pane** scope? Policy C names three scopes but rung 3 has no pane
   definition and `TerminalState` has `manual_label` + `terminal_title` + `title` already competing
   there (`src/terminal/state.rs:2140+` `border_label`). Is pane scope in this change or not?
3. Federation: does a *local* rename of a mirrored remote tab propagate to the remote, stay local, or
   get refused? G1 shows the current storage cannot express "local override on top of a remote name".
4. Does `NameSource` need a variant for "inherited from an enclosing scope", distinct from "set at
   this scope"? Clients need it to decide whether "reset to auto" is meaningful on that row (G3).
5. Is `just bench-render-scale` runnable on this machine? `just` and `cargo nextest` are reported
   possibly absent in the task brief; if the bench cannot run, P1's evidence has to come from a
   deterministic operation-count test instead, which CLAUDE.md says it prefers anyway.
