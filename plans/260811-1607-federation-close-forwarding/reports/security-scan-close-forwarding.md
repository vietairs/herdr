# Security scan: federation close forwarding (workspace/tab)

Scope: uncommitted working-tree diff in
`.claude/worktrees/federation-multi-tab-workspace` (28 files, close forwarding
for `WorkspaceCloseRequest`/`TabCloseRequest`). Read-only review, no edits made.

Context established up front: `FederationLease` (`src/server/federation_lease.rs`)
is a **single-controller, whole-server** lease — one admitted mounted
connection at a time gets `is_mounted_controller` authority over the *entire*
host session (see pre-existing, ungated `ClosePane`/`SplitPane` arms reaching
`Method::PaneClose`/`PaneSplit` with no workspace-membership check at all).
This is not a per-workspace grant. That context matters for grading findings
below — "can close any local workspace" is not automatically privilege
escalation here, because the mount model already intends that.

## Findings

### 1. MEDIUM — id resolution accepts positional-index fallback for wire-supplied target ids (CONFIRMED)

`src/app/ids.rs:60-93`, reused by `close_federation_target_workspace`
(`src/app/creation.rs:1991`) and `close_federation_target_tab`
(`src/app/creation.rs:2043`) via `self.parse_workspace_id` / `self.parse_tab_id`.

`parse_workspace_id` tries an exact `workspace.id == id` match first, but
falls back to interpreting the string as a 1-based **index** into
`self.state.workspaces` (`id.strip_prefix("w_")?.parse::<usize>()` or bare
`id.parse::<usize>()`, then `checked_sub(1)`). `parse_tab_id` has the same
fallback for its workspace half, plus its own `t_<ws>_<n>` positional form.

This helper was written for trusted **local** shorthand input (CLI/JSON-API
callers typing `2` or `w_2` instead of the real id). This diff is the first
time it is fed **wire-supplied, peer-controlled** strings
(`WorkspaceCloseRequest.target_workspace_id`, `TabCloseRequest.target_tab_id`)
without any validation that the string is the real namespaced id the peer was
told about.

Verified real workspace ids can never collide with the fallback: they are
generated as `format!("w{}", encode_public_number(counter))`
(`src/workspace.rs:117-125`, base32-ish alphabet, no underscore, 1-indexed
counter) — never a bare digit string and never `w_<n>`. So for a crafted
`target_workspace_id` like `"1"`, the exact-match branch always misses and the
positional fallback always fires, resolving to `self.state.workspaces[0]`
regardless of which workspace (if any) was ever disclosed to that mount.

Exploit scenario: a mounted controller sends
`WorkspaceCloseRequest { request_id: N, target_workspace_id: "1" }` (or `"2"`,
`"3"`, …) instead of a real `w<n>` id. The host closes whatever workspace
currently sits at that array index, not a workspace the wire protocol ever
named to this peer. Same class of guess for `TabCloseRequest` via the
`ws_raw` half of `t_<ws>_<n>` or `<ws>:<tab>`.

Given the single-controller/full-host-authority context above, this is **not**
a new privilege boundary crossed (the same controller can already close any
pane via the pre-existing, ungated `ClosePane` arm). It is still worth fixing:
it breaks the documented invariant on the field itself ("Raw (un-namespaced)
remote workspace id to close, **as carried by the mount's own
SessionSnapshot/event stream**" — `src/remote/federation/protocol/mod.rs:412-414`),
makes wire behavior nondeterministic under a misbehaving/buggy peer (closes
the wrong workspace silently instead of failing "not found"), and reuses a
helper explicitly designed for trusted local shorthand on untrusted network
input. Recommend a wire-specific resolver that does the exact-match branch
only (skip the `w_`/bare-numeric and `t_`/positional fallbacks) for
federation-sourced ids.

### 2. LOW/informational — `WORKSPACE_TAB_CLOSE` capability is documented as receive-gated like `FILE_STAGING`, but isn't (CONFIRMED, not currently exploitable)

`src/remote/federation/protocol/mod.rs:114-128` claims: *"Same hard
requirement as `FILE_STAGING`, for the same reason... Both peers must
advertise this before either side emits a close frame."* `FILE_STAGING` is
enforced on the **receive** side in production: `reader_loop` only has a
`staging: Option<&StagingChannel>` when `agreed.0.contains(FILE_STAGING)`
(`src/server/federation_accept.rs:419-432`), and
`handle_clipboard_stage_request` drops the frame outright when that's `None`
(`src/server/federation_accept.rs:1364-1372`).

No equivalent exists for the two new request types.
`reader_loop`'s signature (`src/server/federation_accept.rs:508-519`) never
receives `agreed_capabilities` at all, and
`handle_workspace_close_request`/`handle_tab_close_request`
(`src/server/federation_accept.rs:704-802`) process and reply unconditionally,
with no check that the peer ever advertised `workspace_tab_close`.

Traced why this is not currently exploitable: unlike `FILE_STAGING` (added to
an *already-shipped* protocol version, so an old peer's decoder genuinely
cannot handle the variant), `WorkspaceCloseRequest`/`TabCloseResponse` etc.
ride the unreleased 5→6 protocol bump itself
(`src/remote/federation/protocol/mod.rs:62-72`, confirmed by
`federation_protocol_version_is_unchanged_for_the_close_forwarding_variants`).
Any peer that completes a v6 handshake at all can decode these frames
regardless of capability agreement, so there is no decode-crash/mount-teardown
path here today, and `is_mounted_controller` remains the real authorization
gate (finding 1's fallback aside). Flagging as a doc/code mismatch: the
capability comment overstates what is enforced, and a future protocol bump
that assumes "receive side already checks this like FILE_STAGING" would be
wrong.

## Clean areas (evidence)

- **Area 1, AUTHORIZATION** — clean. `dispatch_command`
  (`src/server/federation_actor.rs:537-566`) gates both new arms on
  `lease.is_mounted_controller(epoch, connid)` before touching `App`, unlike
  the pre-existing `SplitPane`/`ClosePane`/`CreateWorkspace` arms (known,
  out-of-scope gap). Confirmed by
  `close_workspace_and_tab_remote_are_refused_for_a_non_controller_connid`
  (`src/server/federation_actor.rs`), which asserts a `connid: 999` request is
  refused and the workspace set/mode are untouched.

- **Area 2, BLAST RADIUS** — clean.
  `close_federation_target_workspace`/`close_federation_target_tab`
  (`src/app/creation.rs:1991-2109`) call `close_single_workspace_at`, which
  detaches `worktree_space = None` on the target *before* calling
  `AppState::close_selected_workspace()`. `close_indices_for`
  (`src/app/state.rs:1902-1920`) short-circuits to `vec![index]` once
  `worktree_space()` is `None`, so the worktree-group fan-out never fires for
  a federation-originated close. Confirmed by
  `close_workspace_remote_closes_exactly_one_workspace_sibling_survives` and
  `close_tab_remote_closes_exactly_one_workspace_sibling_survives`, both of
  which seed two workspaces sharing a `WorktreeSpaceMembership` key and assert
  the sibling survives with its membership intact.

- **Area 3, UI-STATE MANIPULATION** — clean.
  `close_single_workspace_at` does set `self.state.selected = ws_idx` as an
  internal step, but `close_selected_workspace`
  (`src/app/actions.rs:1665-1716`) restores `selected` to the host's
  previously-*active* workspace by id afterward, as long as that workspace
  isn't the one being closed — the mutation is a same-call intermediate value,
  never rendered mid-operation, so it does not visibly move the host's
  sidebar cursor for a close of an unrelated workspace. (If the closed
  workspace *is* the one the host was looking at, the view necessarily
  changes — that's the unavoidable, in-scope consequence of the
  already-approved remote-close feature, not extra manipulation.) The
  confirmation-modal path (`confirm_implicit_worktree_group_close`, which does
  mutate `mode`/`selected` before refusing) is never called from either new
  federation helper — both go straight to `close_single_workspace_at`/
  `ws.close_tab`. Confirmed by both federation_actor.rs tests asserting
  `app.state.mode == mode_before` after a successful, a refused, and a
  not-found close.

- **Area 4, DOS / MOUNT TEARDOWN** — clean, with the capability-gate caveat in
  finding 2. `send_workspace_close_request`/`send_tab_close_request`
  (`src/remote/federation/client.rs:170-207`) correctly refuse to put a frame
  on the wire unless `mirror.supports(WORKSPACE_TAB_CLOSE)`, so a well-behaved
  local client never sends this to a peer that can't decode it.
  `pending_remote_closes` growth is bounded by *local* action only:
  `register_pending_remote_close` (`src/app/creation.rs:911-919`) is called
  exclusively from `dispatch_remote_workspace_close`/its tab equivalent,
  themselves reachable only from a local JSON-API `workspace.close_remote`/
  `tab.close_remote` call — never from an inbound wire message — so a peer
  cannot inflate this map by sending traffic.

- **Area 5, ID CONFUSION on the response path** — clean (separate from finding
  1, which is on the *request* path host-side).
  `handle_federation_workspace_close_ready`/`_failed` and their tab
  counterparts (`src/app/creation.rs:1378-1478`, `1490-`) check both
  `pending.origin != origin` (HostKey) and drop the pending entry only via
  `take_pending_remote_close(request_id)`. `request_id` is minted per-process
  by `next_remote_close_request_id()`, so it's already unique across mounts;
  origin is a secondary belt-and-suspenders check. Test coverage includes a
  `spoofed_origin` case (`src/app/creation.rs:3445`, `3600`).

- **Area 6, INPUT VALIDATION / panics** — clean beyond finding 1's semantic
  issue. Every `unwrap()`/`.expect()`/`panic!()`/direct-index touched by this
  diff is inside `#[cfg(test)]` modules (checked across
  `src/app/api/{tabs,workspaces}.rs`, `src/app/creation.rs`,
  `src/server/federation_{accept,actor}.rs`,
  `src/remote/federation/{client,protocol/mod}.rs`). `target_workspace_id`/
  `target_tab_id`/`reason` are plain `String` fields with no dedicated length
  cap, but they ride the same `Channel::Control.max_len()` frame-size bound
  every sibling control message already uses (`ClosePaneRequest`, etc.) — no
  new unbounded-string surface.

- **Area 7, standard pass** — clean. No secrets/tokens introduced. No new
  command execution, path traversal, or filesystem write driven by wire
  input — both new host-side handlers only call in-memory `App`/`AppState`
  mutation methods. `tracing::warn!`/`debug!` calls added by this diff log
  only request ids, failure reasons, and workspace/tab ids, no sensitive
  payload content.

## Unresolved questions

- Should finding 1's fallback resolvers be narrowed for *all* federation
  input generally (e.g. does `ClosePaneRequest`'s pane-id resolution have an
  analogous positional-guess path?), or is workspace/tab close forwarding the
  only wire-reachable caller today? Out of scope for this diff's review but
  worth a follow-up sweep given the same `App::parse_*` helpers likely back
  other JSON-API methods too.
- Is finding 2 (doc/code capability-enforcement mismatch) worth fixing now
  for correctness/consistency, or deferred until it's actually load-bearing
  (e.g. if `WORKSPACE_TAB_CLOSE` capability gating is ever relied on without
  a version bump again)?

Status: DONE_WITH_CONCERNS
Summary: Authorization (is_mounted_controller), blast-radius (single-workspace
detach-before-close), UI-state (no confirm-modal path, selected restored),
DoS (send-side gate, locally-bounded pending map), response-side id confusion,
input validation/panics, and secrets/logging all check out clean with source
evidence. Two real issues found: (1) MEDIUM — parse_workspace_id/parse_tab_id
reuse for wire-supplied target ids permits a positional-index fallback that
lets a crafted target id resolve to an arbitrary local workspace/tab by array
position rather than the id the wire protocol actually names, though it does
not cross a new privilege boundary given the pre-existing single-controller
full-host-close authority; (2) LOW — the WORKSPACE_TAB_CLOSE capability's
"receive-gated like FILE_STAGING" doc claim isn't enforced in reader_loop, though not currently exploitable since these variants ship inside the unreleased protocol v6 bump itself.
Concerns: Recommend narrowing id resolution on the federation-close path to
exact-match only before shipping, and either enforcing or re-documenting the
WORKSPACE_TAB_CLOSE capability's receive-side scope.
