# federation scope — Policy C

**Verdict: NOT BLOCKING.** Federation is sizable but coherent under a deliberately narrow contract.
No federation protocol bump is needed. The client protocol needs one bump (23 -> 24) and only because
Policy C promises clients a `NameSource`. Nothing here forces Policy C to change shape; it forces one
new early-exit rung in the resolver and one state-shape fix that federation should have had anyway.

Base: worktree `/Users/hvnguyen/Projects/worktrees/herdr-name-sync`, branch `fix/name-sync-workspace-tab-agent`,
on origin/master `c2f4166a`. All line numbers below are this worktree's, opened and verified.

---

## 1. Where a mounted scope's displayed label comes from today

**Resolved on the SERVING host and shipped over the wire, then pinned into the mounting host's
local override slots.** Both halves matter.

### Resolved remotely
`MountSnapshot` (`src/remote/federation/protocol/mod.rs:230-232`) embeds
`crate::api::schema::session::SessionSnapshot` verbatim — the same `WorkspaceInfo` / `TabInfo` /
`PaneInfo` the JSON API serves. The serving host fills those through its own full-context builders:

- `App::workspace_info` — `src/app/creation.rs:405`, label = `ws.display_name_from(...)` (:411)
- `App::pane_info` — `src/app/creation.rs:319`, label = `terminal.manual_label` (:355)
- `App::agent_info` — `src/app/agents.rs:366`, delegates to `pane_info`
- tab label likewise via `tab_info` -> `tab_display_name` (`src/workspace.rs:478`)

So the remote's *whole* naming ladder has already run before the string leaves that host.

### Then pinned locally into the override slots — this is the problem
`App::materialize_federation_mount` (`src/app/creation.rs:470`) writes those remote strings into the
**local user-override fields**:

| Scope | Local field written | Site |
|---|---|---|
| workspace | `Workspace::custom_name` | `Workspace::from_existing_pane(Some(ws_info.label.clone()), ...)` — creation.rs:562 |
| tab | `Tab::custom_name` | `from_existing_pane(.., Some(tab_info.label.clone()), ..)` creation.rs:563; `create_tab_from_existing_pane(moved, Some(tab_info.label.clone()), ..)` creation.rs:555, :611 |
| tab (post-mount resync) | `Tab::custom_name` | creation.rs:1713-1717 via `RemoteTabRef.label` |
| pane | `TerminalState::manual_label` | creation.rs:744 and `src/remote/federation/client.rs:1423` |

Policy C's rung 1 is "user override at this scope, inherited by descendants; the only thing persisted;
derived names are never stored as truth." Federation currently stores a derived name as truth in
exactly that slot. Two direct consequences if nothing changes:

1. **D3 misfires.** A mounted tab's mirrored label would inherit down onto every mirrored pane's agent
   display label, because the resolver cannot tell it from a rename the user typed. Nobody asked for
   that; it would arrive purely as a storage accident.
2. **`NameSource` lies.** Every mounted workspace and tab reports `Override` for a name the user never
   set. This is already observable today in degenerate one-bit form: `ClientShellWorkspace.custom_label`
   (`src/protocol/wire.rs:1063`) is fed by `state.custom_name.is_some()`
   (`src/server/client_shell.rs:64`) and `ClientShellTab.custom_label` (wire.rs:1096) by
   `!state.is_auto_named()` (client_shell.rs:108) — both unconditionally `true` for a mounted scope.
   The TUI branches on it: `src/client/shell/agent_sidebar.rs:264`,
   `src/client/shell/mobile.rs:663`, `:776`, `:886`.

### One more live hazard the pinning currently hides
`Workspace::from_existing_pane` is handed `PathBuf::from(first_pane.cwd...)` (creation.rs:564) — a
**remote** path string — as `identity_cwd`, and immediately runs `discover_workspace_git_identity(&identity_cwd)`
and `git_branch(&identity_cwd)` against the **mounting host's** filesystem (`src/workspace.rs:283-286`).
Whatever that produces lands in `cached_auto_label`. Today nothing reads it, because `custom_name` is
always `Some`. Under Policy C, rung 3 reads `cached_auto_label` — so the moment a user *clears* an
override on a mounted workspace, the label would snap to a name derived from the local filesystem at
a remote path. That someone already knew this is visible at `src/server/client_shell.rs:71-75`, which
explicitly gates `branch` off for federated workspaces for exactly this reason.

---

## 2. Should a local rename of a mounted remote tab apply to that tab's panes? Whose store?

**Yes, it applies to that tab's panes. The store is LOCAL, and only local. Nothing is forwarded and
nothing is persisted.** This is not a new policy — it is what the code already does, and it is coherent.

Evidence:

- **Rename is not forwarded and there is no RPC to forward it with.** `handle_tab_rename`
  (`src/app/api/tabs.rs:315`) and `handle_workspace_rename` (`src/app/api/workspaces.rs:853`) contain
  no federation branch whatsoever. Contrast create and close, which explicitly redirect over the mount:
  `tabs.rs:85` (`IdClass::Remote` -> `dispatch_remote_tab_create`), `tabs.rs:531`
  (`dispatch_remote_tab_close`), `workspaces.rs:1214` (`dispatch_remote_workspace_close`).
  Grepping `Rename|rename` across `src/remote/federation/protocol/mod.rs`,
  `src/remote/federation/client.rs` and `src/server/federation_accept.rs` returns **nothing** —
  `FederationMessage` has no rename variant.
- **A mounted scope's override cannot be persisted even if we wanted it.** `is_federation_materialized`
  (`src/persist/snapshot.rs:256`) filters federation-materialized workspaces out of session capture at
  `:310`/`:316`. An override on a mounted scope is ephemeral by construction — it dies with the mount,
  which is the right lifetime for it.
- **Inheritance down to the panes is purely local mechanics.** A mounted tab is a real local `Tab`
  holding real local `PaneState`/`TerminalState` entries; D3 resolves the tab override onto them at
  resolve time and nothing crosses the wire. No federation work is required to make this work.

So the answer to "whose override store" is: the mounting host's, exclusively, for the life of the
mount. A rename on this host does **not** rename anything on the serving host and will not under this
contract. Adding a rename RPC is a separate feature carrying its own trust-boundary work (the peer
label sanitize precedent is `src/server/federation_accept.rs:845-857`) — explicitly out of scope.

Known, accepted limitation: a local rename of a mounted scope is lost on remount, because remount
re-materializes from the remote label. Document it; do not fix it here.

---

## 3. Does `NameSource` cross the wire, and does it touch the federation protocol?

**It physically travels, but it must be discarded on arrival. Federation: no bump. Client: bump.**

`TabInfo`/`WorkspaceInfo`/`PaneInfo` are the same types on both protocols, so a `name_source` field
added for the JSON API automatically rides inside `MountSnapshot.snapshot`. The mounting host must
throw the received value away and stamp its own, because the remote's discriminant describes the
remote's store — the local user cannot act on it (they cannot clear a remote override from here) and
it would contradict the local resolve. Discard it in the existing single choke point where every
remote-sourced string is already sanitized before entering local state: `namespace_workspace`
(`src/remote/federation/reducer.rs:420`), `namespace_tab` (:435), `namespace_pane` (:451).

### Version constants, checked against the latest released tag

Latest released tag on this fork: `v0.9.0-hvn.1` (`c06a2cad`).

| Constant | Source (c2f4166a) | v0.9.0-hvn.1 | Already ahead? | Bump required |
|---|---|---|---|---|
| `PROTOCOL_VERSION` (`src/protocol/wire.rs:20`) | 23 | 23 | **No** | **Yes -> 24**, iff `wire.rs` structs change (they do; see below) |
| `FEDERATION_PROTOCOL_VERSION` (`src/remote/federation/protocol/mod.rs:93`) | 7 | 7 | **No** | **No** — see reasoning |

**Federation: do not bump.** The change is an additive `#[serde(default)]` field on types already
carried inside an existing message, not a new `FederationMessage` variant. The fork's own precedent
for exactly this case is `AgentStatusMessage.agent` — "Additive field within
`FEDERATION_PROTOCOL_VERSION` 3" (protocol/mod.rs:338-350). None of the `api/schema` types carry
`deny_unknown_fields` (verified: the only uses in the tree are under `src/detect/`, `src/config/`,
`src/client/endpoint/`), so an already-deployed peer decodes a frame containing the new field and drops
it, and a peer that never sends it leaves the new peer's `#[serde(default)]` to fill in. Bumping would
be **actively harmful**: `negotiate()` and `codec::decode` hard-reject any version mismatch, so 7 -> 8
would break every mount against a deployed `v0.9.0-hvn.1` host for a field that degrades gracefully.
The bump precedents in that doc comment (`Fault` 1->2, `SplitPaneRequest` 2->3, `ClosePaneRequest` 3->4,
`WorkspaceCreateRequest` 5->6, `TabCreateRequest` 6->7) are all *new top-level variants*. This is not one.

**Client: bump 23 -> 24.** Source equals the released value, so it is not already ahead and CLAUDE.md's
rule fires the moment the client wire protocol changes. Policy C says clients receive a resolved name
plus a `NameSource` and never re-derive; the TUI is a client, and the field it uses today is the
one-bit `custom_label` on `ClientShellWorkspace` (wire.rs:1063) and `ClientShellTab` (wire.rs:1096).
Surfacing the real discriminant changes those structs. Also update `check_client_version` fixtures and
the manual protocol fixtures CLAUDE.md names.

  *The one way to dodge the bump* is to leave `custom_label: bool` alone and keep computing it
  server-side from the resolved source. That satisfies "resolved server-side" but not "client receives
  a `NameSource` discriminant". **Not recommended** — `custom_label` is already wrong for every mounted
  scope (Section 1), and four TUI call sites branch on it. Fixing that lie is part of the job.

**Machine-readable contract:** `docs/next/api/herdr-api.schema.json` is a checked-in artifact
(`Cargo.toml:15`, `nix/package.nix:43`). Regenerate with `HERDR_UPDATE_API_SCHEMA=1`.

---

## 4. Degraded-resolve call sites (resolver needs tab+pane, gets only a `TerminalState`)

### Genuine — must be handled explicitly

1. **`TerminalState::border_label(&self, show_agent_labels)` — `src/terminal/state.rs:2140`.**
   This *is* the pane-label resolve, and it is a `&TerminalState` method with no scope at all. Sole
   production caller: `render_pane_border_titles` at `src/ui/panes.rs:648`. That caller holds
   `&AppState` and `&Workspace` but not a tab index; it renders the active tab, so `ws.active_tab`
   (or `ws.find_tab_index_for_pane(info.id)`) supplies it. **Not blocking**, but this is a per-pane
   per-frame render loop on a multiplicative path: pass the already-resolved tab override *in* as a
   parameter. `border_label` must not grow a lookup, an allocation, or a `&AppState` argument.

2. **Popup pane title — `src/server/client_shell.rs:471`**, `terminal.manual_label` on a bare terminal
   id. `PopupPaneState` (`src/app/state.rs:18`) carries only `pane_id` + `terminal_id`; a popup pane
   belongs to no tab and no workspace. This is a legitimately scope-less resolve and must **degrade to
   the pane-only ladder** (pane override -> agent identity -> literal `"popup"`) and never inherit a
   tab override. Document the degrade in `naming.rs` rather than letting it look like an oversight.

### Checked and clear — no degraded path

3. `App::pane_info` (`src/app/creation.rs:319`) resolves `tab_idx` itself at `:327`. Full context.
4. `App::agent_info` (`src/app/agents.rs:366`) delegates to `pane_info`. Full context.
5. `App::workspace_info` (`src/app/creation.rs:405`) is workspace-scope; no tab needed.
6. The serving host's outbound snapshot is built entirely through 3-5, so **`federation-serve` has no
   degraded resolve on the serve side at all.** Its labels leave fully resolved with full context.

On the mounting side the question is moot under FNC-1 below: a remote scope short-circuits before
rungs 2-4, and the mirrored string is a cached scalar already on the pane.

---

## Recommended contract — "Federation Naming Contract v1" (narrow, deliberately)

**FNC-1 — Remote scopes resolve remotely; the mounting host never re-derives.**
A mounted workspace/tab/pane displays the string the serving host resolved. Rungs 2-4 (agent identity,
cwd/git derivation, ordinal) are **never** run for a federation-materialized scope. The mounting host
has no git root, no cwd namespace and no detection authority for the remote's filesystem; running them
produces wrong names from local data — concretely, the `cached_auto_label` hazard in Section 1.

**FNC-2 — `resolve_name` gains one early-exit rung, not a second ladder.**
Signature stays `resolve_name(scope, sources) -> {text, source}`. `sources` gains
`mirrored: Option<String>`. The ladder becomes:

| Rung | Source | Applies to |
|---|---|---|
| 1 | user override at this scope, or inherited from nearest enclosing renamed scope | both |
| 1.5 | **mirrored remote label -> `NameSource::Mirrored`; STOP** | remote scopes only |
| 2 | agent identity | local only |
| 3 | cwd / git-root derivation | local only |
| 4 | stable ordinal (`public_tab_number`) | local only |

Remote-ness is decided by `remote::federation::id::classify(&ws.id)` — the same non-spoofable check
`workspace_info()` already uses for `federation_origin` (`src/app/creation.rs:437-443`), keyed off this
client's own trusted `HostKey` and never off anything the remote sends.

**FNC-3 — Move the mirrored label out of the override slot.** *(the largest single piece of work
federation adds, and pure state shape — zero wire impact)*
Add `Workspace::mirrored_name`, `Tab::mirrored_name`, `TerminalState::mirrored_label`. Materialization
(creation.rs:555, :562-563, :611, :744, :1713-1717 and client.rs:1423) writes those instead of
`custom_name`/`manual_label`. Rung 1.5 reads them. `custom_name`/`manual_label` then mean exactly what
Policy C says they mean at every scope, local or remote, and `NameSource` stops lying.

  *Cheaper fallback if the phase budget forces it:* keep writing into `custom_name`/`manual_label` and
  suppress inheritance whenever the scope classifies `Remote` — one guard instead of three fields. It
  leaves `NameSource` reporting `Override` for a name the user never typed, and leaves D2's gate
  (`changed |= ws.custom_name.is_none()`, `src/app/actions.rs:1555`) permanently false for every
  mounted workspace. Take the field split; accept the guard only under duress, and record it.

**FNC-4 — A local rename of a mounted scope is local, ephemeral, and does inherit downward.**
It overrides on this host only, for the life of this mount; it inherits to that tab's mirrored panes by
ordinary D3 mechanics; it crosses no wire; it is not persisted (snapshot.rs:256 already excludes it);
it is lost on remount. No federation rename RPC — out of scope.

**FNC-5 — `NameSource` never crosses the mount boundary as data.**
The field rides inside `MountSnapshot` because the types are shared, but the mounting host discards the
received value in `reducer.rs::namespace_{workspace,tab,pane}` (:420/:435/:451) and stamps its own:
`Mirrored` for a materialized scope with no local override, `Override` when the local user renamed it,
never `AgentIdentity`/`Cwd`/`Ordinal`. No federation bump follows from this (Section 3).

**Net federation work:** one new enum variant (`NameSource::Mirrored`), one early-exit rung, three new
fields plus their six write sites, three lines in `reducer.rs`. No new messages, no new capability, no
version bump, no serve-side change.

---

## Pre-existing bugs found — do NOT fix here, do NOT let tests enshrine them

**F1 — A remote-side rename never reaches the mounted mirror.**
`reconcile_tabs` emits `EventKind::TabRenamed` (`src/remote/federation/reducer.rs:559-567`) and
`reconcile_workspaces` emits `WorkspaceUpdated` (:500-505) into the local `EventHub`, but there is no
corresponding `AppEvent::FederationResync*Renamed` — `src/events.rs` carries only Created/Removed/Closed
federation resync variants — and no resync handler calls `set_custom_name`. A mounted tab's label is
frozen at materialization time and corrects only on remount. JSON API subscribers see the rename; the
TUI does not. **Verify this is still true before writing any test that asserts a mounted label updates.**

**F2 — `custom_label` misreports every mounted scope as user-renamed.** Section 1. Today's visible
effect: `agent_sidebar.rs:264` renders the tab row for a single-tab mounted workspace where a local one
would be hidden. Whether that is desirable is a product call; right now it is an accident, not a
decision. FNC-3 + FNC-5 make it explicit either way.

**F3 — Remote agents are not mirrored at all.** `reducer.rs:693` sets `agents: Vec::new()`
unconditionally; the remote's `AgentInfo` list never crosses the wire. So the remote's `agent_name`
(the addressable handle) does not exist on the mounting host, and Policy C's accepted limitation
("the handle does not follow a tab rename") is *trivially* satisfied across a mount — there is no
remote handle to break. A mirrored pane is a local `TerminalState` and can be given a local
`agent_name` that addresses it through the forwarded input path. Nothing to do; recorded so nobody
re-derives this later.

---

## Test obligations this contract adds (characterization FIRST, per CLAUDE.md refactor-risk)

1. **Invariant.** `Workspace::assert_invariants_for_test`: a workspace/tab whose id classifies
   `IdClass::Remote` carries its mirrored label in `mirrored_name`, and `custom_name` is `None` until a
   local rename. Adversarial state via `Workspace::test_adversarial_identity_state()`.
2. **Characterization, before any code moves.** The existing fixture-mount tests already drive the real
   path — `src/app/creation.rs:3115`, `:3196`, `:3225` call `materialize_federation_mount` with a
   `loopback` mirror. Pin, against today's behavior: resolved workspace/tab/pane labels equal the remote
   strings; a local `tab.rename` on a mounted tab changes only the local mirror and emits no federation
   frame.
3. **Resolver unit test — the concrete regression FNC-1 exists to prevent.** Construct a mounted
   workspace whose remote `cwd` string also names a real git root on the *local* machine, clear the
   override, and assert the resolved name is the mirrored remote label — never a name derived from the
   mounting host's filesystem (`cached_auto_label` / `git_branch`, `src/workspace.rs:283-286`).
4. **Render-path guard.** `border_label` takes the resolved tab override as a parameter; assert no
   `&AppState` and no lookup enters it. Per CLAUDE.md, profile fixed geometry at 1 and >=15 populated
   panes if the signature change adds any work to that loop.
5. **Protocol.** `check_client_version` and the manual protocol fixtures updated for 23 -> 24;
   `docs/next/api/herdr-api.schema.json` regenerated with `HERDR_UPDATE_API_SCHEMA=1`; a test asserting
   `FEDERATION_PROTOCOL_VERSION` is unchanged at 7, with the additive-field reasoning in its comment.
6. **`reducer.rs`.** A `TabInfo` arriving with a remote `name_source` is namespaced to the local
   stamp, not the remote's — alongside the existing sanitize tests at `reducer.rs:1060-1106`.

---

## Unresolved questions

1. **F2 is a product call.** Should a single-tab mounted workspace show its tab row? Today it does, by
   accident. Once `NameSource::Mirrored` exists the TUI can choose deliberately — but somebody has to
   choose. Default recommendation: preserve today's visible behavior (treat `Mirrored` like `Override`
   for that one row-visibility test) so this refactor ships with no visible federation change.
2. **Is a federation rename RPC wanted later?** FNC-4 says no for now. If it is ever wanted, the peer
   label trust boundary is already solved once at `src/server/federation_accept.rs:845-857`
   (sanitize + clamp) and would be reused — that is a real new `FederationMessage` variant pair and
   *would* force `FEDERATION_PROTOCOL_VERSION` 7 -> 8 plus a `Capability` gate.
3. **F1 unverified live.** I read the code, I did not run a mount. Whether a remote-side rename really
   fails to reach a mounted TUI should be confirmed against two live servers before any test asserts
   either way (federation live-validation recipe: isolate by session name, deploy both ends together).
4. **`identity_cwd` for a mounted workspace is a remote path used as a local one** (creation.rs:564).
   FNC-1 stops it producing a wrong *name*, but `discover_workspace_git_identity` + `git_branch` still
   run against it at mount time — filesystem and subprocess work on a path from another machine. Out of
   scope here; worth its own issue.
