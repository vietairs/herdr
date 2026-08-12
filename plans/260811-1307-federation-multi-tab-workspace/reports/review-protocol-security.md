# Review: federation multi-tab/multi-workspace — protocol compatibility + trust boundary

Branch `feat/federation-multi-tab-workspace` (`591b979a`, `870a4bfd`) vs `master`.
Worktree `/Users/hvnguyen/Projects/herdr/.claude/worktrees/federation-multi-tab-workspace`.
Read-only review. Lens: protocol compatibility + trust boundary. Style nits excluded.

## Scope

- 12 files, +2220/-62. Protocol: `src/remote/federation/protocol/mod.rs`, `codec.rs`.
  Serving host: `src/server/federation_accept.rs`, `federation_actor.rs`.
  Mounting client: `src/remote/federation/client.rs`, `reducer.rs`,
  `src/app/api/workspaces.rs`, `src/app/creation.rs`, `src/app/api.rs`, `src/events.rs`,
  `src/app/mod.rs`, `src/app/actions.rs`.
- Build verified: `ZIG=$HOME/.local/zig-0.15.2/zig cargo check --all-targets` — clean, no warnings.

## Verdict

Two blockers. One is a real, exploitable trust-boundary regression (peer-controlled
terminal-escape injection into the *serving host's* TUI); one is a silent
feature-defeating fallback for a supported config. The protocol work itself is
correct and well-tested.

---

## Critical

### C1. CONFIRMED — Peer-supplied `label` reaches the serving host's TUI unsanitized (terminal-escape injection)

**Path (all four hops verified):**

1. `src/remote/federation/protocol/mod.rs:387` — `WorkspaceCreateRequest { request_id, label: Option<String> }`, peer-controlled.
2. `src/server/federation_accept.rs:688` — `handle_workspace_create_request` destructures `label` and forwards it verbatim into `FederationCommand::CreateWorkspace { label, reply }`. No sanitization.
3. `src/server/federation_actor.rs:499-512` — `dispatch_command` puts it straight into `Method::WorkspaceCreate(WorkspaceCreateParams { label, .. })`.
4. `src/app/api/workspaces.rs:652-656` — `workspace.set_custom_name(label)` (`src/workspace.rs:1146`, a bare assignment). It then reaches:
   - `src/ui/sidebar.rs:2462/2479/2537/2541/2598` via `ws.display_name()` → written into the ratatui buffer of the **serving host's own user**;
   - `src/logging.rs:247` `workspace_renamed` (id only — log itself is clean);
   - the serving host's session save (`create_workspace_with_launch_env` → `schedule_session_save`), so the injected string is **durable across restarts** (SUSPECTED on the exact save call, CONFIRMED that create schedules a save on the normal path).

**Why this is a regression, not parity:** the project already owns this exact threat
model. `src/remote/federation/sanitize.rs` module docs: *"Every remote-sourced chrome
string (workspace/tab/pane labels, cwd, agent name, terminal title, …) is neutralized of
terminal control/ANSI/OSC sequences before it is allowed to reach the ratatui buffer"* —
"S11.1 blocker". It is applied at the reducer choke point
(`reducer.rs:424/439/457-473`) for the **serving host → mounting client** direction, and
in `file_staging.rs:344` for an inbound peer *filename*. This branch opens the first
**mounting client → serving host** chrome-string channel and does not use it.

Compare the other two client→host requests: `SplitPaneRequest`
(`protocol/mod.rs:309-319`) and `ClosePaneRequest` (`:355-363`) carry **only** `u64` and
opaque pane ids. `label` is genuinely the first free-text peer string that lands in the
serving host's rendered state.

**Exploit:** a malicious or compromised mounting client sends
`WorkspaceCreateRequest { label: "ok\x1b]52;c;<base64>\x07" }`. The serving host's TUI
renders it in the sidebar → OSC 52 clipboard write on the *victim* host, plus the whole
cursor-move / screen-clear / conceal set that `sanitize.rs`'s own tests enumerate. Repeat
with `\x1b[2J` to garble, or with conceal SGR to hide a workspace from the victim's view.
The serving host's user never consented to this; mounting only means "you may drive my
panes", and the existing model deliberately confines raw remote bytes to the ghostty
emulator sandbox.

**Fix:** sanitize at the serving-host ingress choke point — in
`handle_workspace_create_request` (`federation_accept.rs:688`), before the
`FederationCommand` is built:

```rust
let label = label.map(|l| crate::remote::federation::sanitize::sanitize_remote_string(&l));
```

Sanitizing in `federation_actor.rs` also works; do **not** sanitize inside
`handle_workspace_create` in `api/workspaces.rs` — that path also serves the trusted
local user and would change local behavior. Add a regression test asserting an
ESC/BEL-bearing label arrives stripped at `Workspace::display_name()`.

**Severity: Critical.** Cross-trust-boundary escape injection into a second user's
terminal, persisted, with an already-blessed one-line fix and an existing precedent the
branch skipped.

---

### C2. CONFIRMED — Feature silently falls back to a local workspace when `ui.prompt_new_workspace_name = true`

`src/app/api/workspaces.rs:625` gates the whole remote dispatch on `params.cwd.is_none()`:

```rust
if params.cwd.is_none() {
    if let Some(source_ws_idx) = self.workspace_creation_source() {
        if let Some(origin) = self.federation_host_key_for_workspace(source_ws_idx) {
            return self.dispatch_remote_workspace_create(id, source_ws_idx, origin, params.label);
```

But the named-workspace TUI path **always** supplies a cwd:

- `src/app/creation.rs:111-120` `begin_tui_workspace_create` — when
  `state.prompt_new_workspace_name` is set, it resolves a **local** cwd and opens the
  dialog instead of calling the API.
- `src/app/input/modal.rs:1060-1071` `save_rename_modal_via_api` — on submit calls
  `runtime_workspace_create` with `cwd: Some(cwd.display().to_string())` and the user's
  `label`.

So for any user with `prompt_new_workspace_name = true` (`src/config/model.rs:814`,
defaults to `false` per `:1042`), pressing "new workspace" while a federated workspace is
focused silently creates a **local** workspace seeded from a locally-resolved path —
exactly the bug this branch exists to fix — with no warning. The `cwd.is_none()` guard's
stated intent ("an explicit path is a deliberate local-directory choice") does not hold
here: the path was not chosen by the user, it was auto-derived by
`resolve_new_terminal_cwd`.

Corollary: on the *default* path (`creation.rs:127`) `label` is hardcoded `None`, and the
only path that carries a user label is the one that is now excluded. **The
`WorkspaceCreateRequest::label` field is unreachable from the TUI in either
configuration** — it is exercised only by tests, `herdr workspace create --label`, and the
JSON API. That undercuts the field's justification (and, note, is the field carrying the
C1 vulnerability).

**Fix:** distinguish an explicit user-supplied `cwd` from an auto-derived one. Simplest:
have `begin_tui_workspace_create` skip the local-cwd prefill (and pass `cwd: None`) when
`workspace_creation_source()` resolves to a federated workspace, so the dialog's label
still flows to the remote.

**Severity: High → blocking.** Silent wrong-target behavior on a supported config; no
error, no toast, no log.

---

## High

### H1. CONFIRMED — `workspace.create` now returns an *error* on the success path for federated workspaces

`src/app/api/workspaces.rs:744-750`: the remote dispatch acknowledges with
`encode_error(id, "remote_workspace_create_pending", ...)`. Any JSON-API/CLI client
(`src/cli/runtime.rs:24` `herdr workspace create`) gets a failure response for what is
actually a successful request, and never learns the eventual outcome — the failure notice
(`handle_federation_workspace_create_failed`, `creation.rs:1726-1763`) is a **TUI toast
only**, with no API event and no correlation back to the JSON request id.

Precedent exists (`api/panes.rs:321` `remote_split_pending`), so this is consistent rather
than novel, and I am not calling it a blocker. But this is the second occurrence of the
pattern, and it does deepen a TUI-only outcome channel for a shared runtime fact, which
sits awkwardly against CLAUDE.md's runtime/client boundary guardrail (see B1). At minimum,
document the `*_pending` codes as non-terminal acknowledgements in the API surface, and
consider emitting a runtime event on the create failure so a non-TUI client can observe it.

**Severity: High (contract), non-blocking.**

---

## Medium

### M1. CONFIRMED — Index entries are evicted before the origin fence in two removal handlers

- `src/app/creation.rs:1673` — `handle_federation_resync_workspace_removed` does
  `remote_resync_workspace_index.remove(&workspace_id)` **before** the
  `workspace_matches_federation_origin` check at `:1684`; on mismatch it returns without
  restoring the entry.
- `src/app/creation.rs:1852` — `handle_federation_resync_tab_removed` does
  `remote_resync_tab_index.remove(&tab_id)` before the origin check at `:1863`, same
  no-restore-on-mismatch shape.

**Not currently exploitable across mounts.** `map_in`
(`src/remote/federation/id.rs:169`) unconditionally prefixes with the *local* mount's own
`host_key`, so a hostile serving host that echoes back `r:victim@host:w1` gets
re-namespaced to `r:attacker@host:r:victim@host:w1`; `classify` (`:156`) then yields the
attacker's own host key. Cross-mount id forgery is structurally blocked. I verified this
holds for every new path.

So this is a latent-correctness issue, not a live escalation: it only bites if the
namespacing invariant is ever weakened, or on a legitimate origin race. Move the `remove`
after the fence.

**Severity: Medium (defence-in-depth).**

### M2. CONFIRMED — No length cap on `label` before it hits the 4 KiB control-channel ceiling

`Channel::Control.max_len()` is `4 * 1024` (`protocol/mod.rs:552`). `codec::encode`
(`codec.rs:55-73`) performs **no** cap check — only the receiver's `decode` does
(`codec.rs:107-114`, `FrameTooLarge`). A label ≥ ~4 KiB therefore produces a frame the
serving host rejects. Nothing in `dispatch_remote_workspace_create`
(`api/workspaces.rs:679-750`) bounds `label` first.

Whether that kills the mount depends on the reader's `FrameTooLarge` handling; I did not
trace it to a conclusion (SUSPECTED that it faults the link). Either way the request is
lost with no user-visible reason. Cap the label (the sidebar cannot render 4 KiB anyway)
before sending, and/or check `encode`d length against the channel cap in
`dispatch_remote_workspace_create`.

Note this is only reachable once C2 is fixed (today the TUI never sends a label).

**Severity: Medium.**

### M3. SUSPECTED — `Failed` reason can echo serving-host filesystem detail to the peer

`federation_actor.rs:521-547` builds `Err(format!("{code}: {message}"))` from the JSON API
error, and `api/workspaces.rs:665` produces
`encode_error(id, "workspace_create_failed", err.to_string())` — an `io::Error` from PTY
spawn / cwd resolution, which routinely carries a path (default shell, home directory).
That string is returned verbatim to the peer in `WorkspaceCreateResponse::Failed` and
toasted on the client.

Impact is small: the mounting peer already receives every pane's `cwd` in the mount
snapshot, so paths are in-band already. But a spawn error can name a binary path
(`/usr/local/bin/fish`) not otherwise exposed. Consider returning the `code` and a fixed
message, keeping the detail in the serving host's own `tracing` output.

**Severity: Medium (informational leak), low impact.**

---

## Clean categories (checked, no findings)

**Protocol version bump — CORRECT and REQUIRED.**
`git show v0.8.0-hvn.2:src/remote/federation/protocol/mod.rs` → `FEDERATION_PROTOCOL_VERSION = 5`;
`master` → also `5`. Per CLAUDE.md the source protocol is *not* already greater than the
latest released one, so a bump was required, and 5 → 6 is the right size. The change adds
two new top-level `FederationMessage` variants (`protocol/mod.rs:599-600`) — genuinely
wire-incompatible, not an additive field, so the doc comment's classification alongside
the `Fault` 1→2 / `SplitPaneRequest` 2→3 / `ClosePaneRequest` 3→4 precedents is accurate.
`src/protocol/wire.rs::PROTOCOL_VERSION` correctly untouched at 19 (this branch does not
change the server/client wire protocol).

**Hardcoded protocol expectations / fixtures.** Grepped every occurrence: all 27 sites
reference the `FEDERATION_PROTOCOL_VERSION` constant or arithmetic on it
(`+1`/`-1`). No bare integer literal anywhere in negotiation, handshake, or codec
fixtures (`federation_accept.rs:1570/1614/1641`, `client.rs:224/1295/1450`,
`loopback.rs:294`, `serve.rs:143/243`, `negotiate.rs:21-24`, `codec.rs:69/94/311/319/366`,
`protocol/mod.rs:907/916/941`). Nothing to update was missed.

**Codec symmetry / bounds.** `workspace_create_request_response_roundtrip_through_the_wire_codec`
(`protocol/mod.rs:698`) covers all four shapes including `label: None`. Channel assignment
(`:624`) matches `SplitPaneRequest`/`ClosePaneRequest` — `Channel::Control`, 4 KiB.
`decode` (`codec.rs:82-127`) checks version, then `claimed_len > max_len` **before** any
slice or copy, then buffer sufficiency. Serde JSON payload; no peer-controlled length
drives an allocation. No unbounded allocation on the new path.

**Version-mismatch behavior.** Fails cleanly, at the header, before any payload byte is
touched — `codec.rs:93-99` returns `CodecError::VersionSkew { local, remote }`. The branch
adds a dedicated test for the *downgrade* direction
(`codec.rs:328-350` `decode_rejects_a_peer_on_the_previous_federation_protocol_version`)
with a `const` assert pinning "shipped at 6"; the pre-existing `+1` test covers the
upgrade direction. Payload-level negotiation (`negotiate.rs:21`) also rejects. No partial
progress: the handshake is the first frame either way. Pre-existing, unchanged, worth
knowing: a v6 client mounting a v5 host will see `"link closed before a HandshakeResponse
arrived"` (`client.rs:246`) rather than a version message, because the v5 host's `Fault`
would itself be v5-stamped and rejected. Comprehensible enough; not introduced here.

**Uncapped workspace creation — confirmed NOT a new capability.**
`handle_workspace_create_request` (`federation_accept.rs:688`) is uncapped and unrated,
and the doc comment says so honestly. The same authenticated peer can already call
`SplitPaneRequest` (`federation_accept.rs:543` region, `federation_actor.rs` `SplitPane`)
without a cap, and each split allocates a pane **plus a real PTY** — strictly ≥ the cost
of one workspace-create (which allocates one workspace + one tab + one pane + one PTY,
i.e. ~1x the PTY cost with a little extra bookkeeping). The resource-growth primitive is
identical in kind and within ~1x in degree. The recorded no-cap decision is defensible as
parity. Flagging only for the record that the *aggregate* federation surface remains
unmetered — that is a pre-existing design gap, not something this branch introduced.

**Id namespacing / origin fencing — applied on every new path.** Verified individually:
- `handle_federation_resync_workspace_created` (`creation.rs:1635`) — `classify()` fence
  requiring the id's host key to equal the reporting origin, before anything is recorded.
- `handle_federation_resync_workspace_removed` (`:1684`) — `workspace_matches_federation_origin`.
- `handle_federation_resync_tab_created` (`:1788` materialized branch, `:1804` pending
  branch) — both branches fenced, including the "workspace announced but not yet
  materialized" case.
- `handle_federation_resync_tab_removed` (`:1863`).
- `handle_federation_resync_pane_created` (`:1366`) and
  `materialize_resync_workspace_from_pane` (`:1525`, fences against the origin that
  *announced* the workspace, not just the reporting one).

A hostile serving host cannot address another mount's or a local workspace's entities
(see M1's `map_in` analysis). `close_single_workspace_at` (`:1608`) correctly detaches
`worktree_space` first so retiring one remote workspace does not take the whole mount's
workspace group down — good catch by the author. Purge helpers are invoked at both
teardown sites (`api/workspaces.rs:559-562`, `:1021-1024`).

**Runtime/client boundary guardrail.** Names are neutral throughout —
`WorkspaceCreateRequest`, `FederationCommand::CreateWorkspace`,
`FederationResyncWorkspaceCreated`, `remote_resync_workspace_index`. No `sidebar`/`row`/
`card`/`widget` naming in the new surface. The shared runtime fact (a workspace exists on
the serving host) flows through server state and the JSON API/event path:
`Method::WorkspaceCreate`, `EventKind::WorkspaceCreated`/`WorkspaceClosed` — and the
branch correctly adds those two to `is_structural_event_kind` (`client.rs:1156-1157`), so
out-of-band remote workspace changes now trigger a resync at all. No new behavior is
TUI-socket-only. The one soft spot is H1's TUI-only failure toast.

**Platform gating.** Clean. All five new `AppEvent` variants are `#[cfg(unix)]`
(`events.rs:319-374`), their dispatch arms in `api.rs:238-286` and `actions.rs:2937-2945`
match, the new handlers and purge helpers in `creation.rs` are `#[cfg(unix)]`, and the two
new `App` fields are ungated with `#[cfg_attr(not(unix), allow(dead_code))]` on
`RemoteWorkspaceRef`/`RemoteTabRef` (`creation.rs:2081/2092`) — the same shape as the
existing `PendingRemoteClose`. `dispatch_remote_workspace_create` is intentionally ungated,
matching the ungated `dispatch_remote_pane_split` in `api/panes.rs` and the documented
rationale on `federation_host_key_for_workspace` (`api/workspaces.rs:1048-1052`). Nothing
here is OS-specific enough to belong in `src/platform/`, and I see no plausible Windows
break. (Not compiled for Windows — this is a read of the gating, not a build result.)

**Code conventions.** No `unwrap()` added in production code. `tracing` used throughout,
including on every dropped/fenced event. Both `#[allow]`s carry an explaining comment
(`federation_actor.rs:180`, `creation.rs:1504`).

**Test quality.** Not phantom. `create_workspace_creates_a_real_workspace_and_replies_with_its_ids`
(`federation_actor.rs:1052`) asserts real state change (`workspaces.len() == before + 1`),
the label landing in `display_name()`, and — good adversarial thinking — that the serving
host's `active` focus is *not* stolen. The two `reader_loop_*` tests use a mock actor but
are explicitly scoped to frame→command→response routing, with the real dispatch covered
separately; the failure test asserts the peer is answered rather than dropped. Gaps: no
test for C1 (label sanitization), none for C2 (the `cwd: Some(...)` fallback), none for M1.

---

## Recommended actions

1. **Blocker** — sanitize `label` at the serving-host ingress (C1), with a regression test.
2. **Blocker** — fix the `prompt_new_workspace_name` fallback (C2); otherwise the feature
   is off for that config and `label` is dead code from the TUI.
3. Move the index `remove` after the origin fence in both removal handlers (M1).
4. Bound `label` length before send (M2).
5. Consider a fixed-message `Failed` reason (M3) and documenting the `*_pending`
   acknowledgement contract (H1).

Items 1–3 are one-line-scale changes. Nothing here argues against the design.

## Unresolved questions

- Does a `CodecError::FrameTooLarge` on the control channel fault the whole mount, or is
  the frame skipped? Determines M2's real severity.
- Was the serving-host sanitization gap (C1) considered and deliberately deferred, or
  missed? The `sanitize.rs` precedent is close enough that a recorded decision would
  change how I rank it.
- Is `label` intended to be reachable from the TUI at all? If not, dropping the field
  removes C1 and M2 outright and simplifies the wire type.
