# Plan v2 — remote close forwarding for federated workspaces/tabs

Supersedes plan v1 (design gate + two adversarial red-team passes rewrote it).
Evidence: `reports/blindspot-260811-close-forwarding.md`,
`reports/redteam-destructive.md`, `reports/redteam-protocol.md`,
`reports/auto-decisions-260811-close-forwarding.md`.

## What changed from v1, and why

v1 planned to make `workspace.close`/`tab.close` forward to the host, adding a
`workspace.unmount` escape hatch. Both red-teams independently attacked that, and
the user chose the safer fork. v1 is dead in three specific ways:

1. **v1 would have destroyed host worktree groups.** PROVEN: on the host a
   federation-originated close targets a LOCAL workspace, so the federated
   "retire one" branch never applies (`workspaces.rs:1037,1064-1071`) and
   `close_selected_workspace()` runs, which closes every workspace sharing the
   worktree-space key via `close_indices_for` (`state.rs:1862-1879`). One client
   close ⇒ N host workspaces destroyed. `handle_workspace_close` has NO
   `confirmation_required` gate to fall back on (grep: zero hits in that file).
2. **v1 would have widened the blast radius across a machine boundary,
   undetectably.** (Corrected premise — see below.) `workspace.close` is ALREADY
   destructive locally: `tests/cli/panes.rs:322`
   (`closing_workspace_terminates_processes_inside_it`) runs the real CLI and
   asserts a pane's pid does not survive it. The federated case is the SINGLE
   exception where it is non-destructive — which is exactly what
   `creation.rs:3134-3139` calls "the locally-initiated federated unmount". So
   the objection is not "it becomes destructive"; it is that v1 would have
   extended destruction ACROSS A MACHINE BOUNDARY with no opt-in, no
   confirmation, and no way for a caller to detect the change: a pure semantics
   change leaves `docs/next/api/herdr-api.schema.json:4711`
   (`"const": "workspace.close"`) BYTE-IDENTICAL, so every generated client is
   blind to it by construction. Adding new method names does change that schema.

   RETRACTED, do not cite: an earlier draft justified this with cli-reference.mdx:143
   "closes Herdr state only". That sentence is about not deleting the git
   checkout (its next clause names `worktree remove` as the checkout-deletion
   path), not about sparing processes. The reviewer that raised it withdrew it,
   and the test above disproves it. The DECISION is unchanged and still correct;
   only the reasoning above is load-bearing.
3. **v1's "no bump is fine" reasoning tested the wrong thing.** It compared
   against release TAGS (correct: no tag ships v6) but the live skew is between
   the committed branch head (v6 WITHOUT close variants) and the working tree
   (v6 WITH them) — two mutually undecodable dialects both labelled 6.

USER DECISION (asked, answered): keep `workspace.close`/`tab.close`
non-destructive; add explicit opt-in verbs for remote deletion; same rule for
tabs and workspaces.

## Design (v2)

- `workspace.close` / `tab.close` — **BEHAVIOR UNCHANGED**. Local retire/unmount
  exactly as today. They NEVER reach the wire. This deletes v1's entire
  unmount-vs-delete overload hazard structurally: a verb that cannot forward
  cannot accidentally delete.
- **NEW** `workspace.close_remote` / `tab.close_remote` — forward to the host and
  tear the local mirror down only on the host's ack. Named to pair with the
  existing `workspace.mount_remote`; `snake_case` after the dot per
  `schema.rs` convention.
- All new sends are **capability-gated**, not version-bumped (see D1).

## Outcome

A user can close a mirrored workspace or tab ON THE HOST, explicitly, and the
local mirror disappears only once the host confirms. Existing close behavior,
scripts, and muscle memory are untouched.

## Non-goals

- No `FEDERATION_PROTOCOL_VERSION` / `PROTOCOL_VERSION` bump (see D1).
- No change to `workspace.close` / `tab.close` semantics.
- No per-workspace authorization/exposure-set model (no such concept exists).
- No live two-host validation (needs real hardware; owed, see Risks).

## Key decisions

**D1 — capability gating, NOT a version bump.** The repo already solved this
exact hazard: `client.rs:144-151` documents that a peer lacking a variant fails
to decode, `read_frame` errors, "and its whole mount tears down — every pane on
that link dies because of one paste", which is why `ClipboardStageRequest` is
gated behind `Capability::FILE_STAGING` and funnelled through ONE send helper.
`negotiate()` (negotiate.rs:20-35) intersects capabilities and is fatal ONLY on
version mismatch — unknown capabilities are silently dropped. So:

- Add `Capability::WORKSPACE_TAB_CLOSE`, advertised by both sides.
- Route every new request through ONE send helper each, which refuses to send
  unless the capability was agreed — mirroring `send_clipboard_stage_request`.
- An older v6 peer never advertises it, so it never receives the variant: no
  decode error, no mount teardown, graceful degradation.
- This also respects CLAUDE.md's rule (source 6 > released 5 ⇒ do not bump), which
  bumping to 7 would violate.

**D2 — the host closes EXACTLY ONE workspace/tab.** A remote peer must never
trigger the host's worktree-GROUP close. The client asked to close one thing; the
host closes one thing. Group close stays a local-only UX affordance.

**D3 — confirmed (ack-gated) teardown**, mirroring `dispatch_remote_pane_close`
(`panes.rs:345-417`) → `handle_federation_close_pane_ready`
(`creation.rs:1187-1271`). The host can legitimately refuse, and resurrecting a
torn-down mirror is far harder than holding a pending entry.

**D4 — P2 and P3 ship TOGETHER.** v1 called them disjoint; that was wrong. The
host read loop has a catch-all `Ok(Some(_other)) => {}` ("ignored, not treated as
fatal", `federation_accept.rs:567-570`), so a client shipped without host support
would have its requests SILENTLY DROPPED — and with ack-gated teardown the close
would hang forever while the UI promises confirmation. Strictly worse than today.

## Invariants (must not regress)

I1. **Echo rule, mechanically checkable:** every client-side
    `FederationMessage::*Request` CONSTRUCTION site lives under `src/app/api/`
    (one grep verifies). Nothing in `creation.rs` (resync handlers, ack handlers,
    mount lifecycle, `close_single_workspace_at`) and no `Workspace`/`AppState`
    primitive may construct one or hold a `remote_out_tx`. Necessary because 3 of
    `close_single_workspace_at`'s 5 callers are host-originated
    (creation.rs:1238/1831/2155).
I2. Local teardown ONLY on host ack, in a handler validating the response
    `origin` HostKey.
I3. `workspace.close`/`tab.close` never reach the wire.
I4. New host command arms are gated on `is_mounted_controller(epoch, connid)`
    (fences stale-epoch commands from a superseded controller). Present on
    SendInput/Resize/NudgeRedraw/ReleaseTerminalSize; absent on the three layout
    arms — the NEW arms get it.
I5. **One shared pending map ⇒ one shared counter.** Closes of all kinds mint
    from the existing `next_remote_close_request_id()` (panes.rs:44-47). Adding a
    second close counter would start at 1 and collide in the shared map, popping
    the WRONG pending entry. (Both red-teams independently refuted a collision in
    the CURRENT code — correlation is type-routed — so this is a forward-looking
    guard on the shared-map design, not an existing bug.)

## Phases

### P1 — wire protocol — DONE (verified)

3 files, +185/-1; 27 protocol tests pass; both version constants unchanged.
Includes 4 non-panicking stub arms in `client.rs` (an exhaustive match there
makes a protocol-only phase unbuildable — precedent commit `1af58792` did the
same).

### P1b — capability plumbing (new, from D1)

Files: `src/remote/federation/protocol/mod.rs`, `src/remote/federation/client.rs`,
`src/remote/federation/serve.rs`, `src/server/federation_accept.rs`

- `Capability::WORKSPACE_TAB_CLOSE` const; advertise on both handshakes.
- ONE send helper per new request, refusing to send unless agreed — model on
  `send_clipboard_stage_request` (client.rs:144+) including its doc comment
  explaining why an ungated send is fatal.
- Tests: capability absent on one side ⇒ dropped, not fatal; present both sides
  ⇒ agreed; send helper refuses when not agreed.

### P2 — host side (ships with P3)

Files: `src/server/federation_accept.rs`, `src/server/federation_actor.rs`

- Decode arms + handlers mirroring `handle_close_pane_request`.
- `FederationCommand::CloseWorkspaceRemote` / `CloseTabRemote`.
- **D2:** close exactly ONE workspace/tab. Do NOT route through
  `close_selected_workspace()`. Use the single-retire primitive and assert the
  worktree group is untouched.
- **I4:** gate both arms on `is_mounted_controller`.
- Use a fixed internal request id (`"federation-close-workspace"` /
  `"federation-close-tab"`) per the `"federation-close-pane"` trust-boundary
  pattern, so the host's own confirmation modal is never opened by a remote peer.
  The refusal predicate must be NON-MUTATING: `tabs.rs:311` currently reaches
  `actions.rs:1979-1987` which sets `mode = ConfirmClose` before refusing.
- Map host-side not-found / refusal to `Failed { reason }`, never a silent drop.

### P3 — client side (ships with P2)

Files: `src/events.rs`, `src/app/api.rs`, `src/app/mod.rs`, `src/app/creation.rs`,
`src/app/api/workspaces.rs`, `src/app/api/tabs.rs`, `src/remote/federation/client.rs`

- `Method::WorkspaceCloseRemote` / `TabCloseRemote` handlers that dispatch,
  register pending, and return `remote_close_pending`. `workspace.close` /
  `tab.close` are NOT touched (I3).
- Translate mirror id → raw host id with `id::strip_mount_namespace` (already
  in-tree, documented "dormant until a live call site wires it" — this is that
  call site; drop its `#[allow(dead_code)]`).
- Extend `PendingRemoteClose` with a target kind (`Pane`|`Tab`|`Workspace`);
  ONE map, ONE counter (I5).
- Store the CANONICAL `public_tab_id`, never the caller-supplied
  `target.tab_id` — `parse_tab_id`'s `t_<ws>_<idx>` form is POSITIONAL, so a
  neighbour-tab removal could redirect the ack onto a live tab.
- Replace the P1 stub arms in `client.rs` with real response handling →
  `AppEvent` → origin-validated, idempotent teardown (must tolerate a racing
  resync having already removed the target).
- **Purge gap:** `purge_federation_state_for_workspaces` keys on WORKSPACE id, so
  `handle_federation_resync_tab_removed` (creation.rs:1977) purges no pending TAB
  close. Add tab-scoped purging.

### P4 — surfaces (revised: no unmount verb needed)

Files: `src/api/schema.rs`, `src/api/mod.rs`, `src/api/server.rs`,
`src/cli/runtime.rs`, `src/app/runtime_mutations.rs`,
`src/app/input/navigate.rs`, `src/app/input/modal.rs`, `src/logging.rs`

- Register both new methods; CLI subcommands.
- TUI affordance for remote close, DISTINCT from the plain close gesture, with a
  confirmation naming the host (the existing dialog at `ui/dialogs.rs:600-663`
  says "Close workspace? — name — N panes" and never mentions the host).
- **Surface the outcome.** All three close gestures currently DISCARD the
  response (`navigate.rs:452-455`, `:505`, `modal.rs:1189-1197`), so
  `remote_close_pending` and `Failed` would be invisible — the key would look
  broken and retries would stack destructive requests. Add a toast path for both.
  Note `modal.rs:1189` is the DEFAULT route (`confirm_close` defaults true), so
  the branch belongs inside `close_workspace_idx_via_api` (navigate.rs:452), not
  at the `NavigateAction` site.
- Docs: `docs/next/...` only (unreleased). Do NOT touch stable docs or root
  README/CHANGELOG.

### P5 — verify

`ZIG=~/.local/zig-0.15.2/zig cargo test -- --test-threads=4`; clippy (3
pre-existing baseline errors only); `cargo fmt --check`.

## Test matrix

| # | Test | Guards |
|---|---|---|
| T1 | each new request/response round-trips the codec | P1 done |
| T2 | `FEDERATION_PROTOCOL_VERSION` still 6 | D1 |
| T3 | capability absent one side ⇒ dropped not fatal; send helper refuses ungated | D1 |
| T4 | host closes exactly ONE workspace; sibling worktree-group members SURVIVE | **D2 / C1** |
| T5 | new commands refused without the mount lease | I4 |
| T6 | refusal reported as `Failed`; host `mode` unchanged (non-mutating predicate) | P2 |
| T7 | `workspace.close`/`tab.close` on a mirror send NOTHING | **I3** |
| T8 | `close_remote` does not remove locally before ack | I2 |
| T9 | teardown on ack only, and only for matching `origin` | I2 |
| T10 | non-last mirrored tab `close_remote` forwards | reported bug |
| T11 | resync-removed workspace/tab sends nothing (captured out_tx) | I1 echo |
| T12 | `handle_federation_mount_ended` sends nothing | I1 |
| T13 | ack after racing resync removal is idempotent, no panic | P3 |
| T14 | pending TAB close purged on tab-removed resync and on mount-ended | purge gap |

## Risks / owed

- **Deploy both sides together.** Branch-head and working-tree binaries are both
  "v6" but mutually undecodable. The capability gate (D1) prevents the mount
  teardown, but the host still cannot ACT on a close it cannot decode — so host
  and client must be deployed together for the feature to work. Matches the
  existing "restart both servers after binary updates" practice.
- **Live two-host validation is owed** and not performed by this run.
- No pending-close TTL exists; a refusing-but-alive host strands a pending entry.
  Accepted: the escape hatch is plain `workspace.close` / `tab.close` on the
  mirror, which retain today's local-only behavior in v2 and remove the mirror
  regardless of the pending ack. (v2 has NO unmount verb — earlier wording
  referring to one was stale.) TTL noted as follow-up.

## Follow-ups (not this change)

- `is_mounted_controller` missing on the pre-existing `SplitPane` /
  `CreateWorkspace` arms (`federation_actor.rs:412,499`).
- TTL/expiry for pending remote closes.

## Red-team reconciliation (controller arbiter pass)

Findings were cross-checked for contradictions, evidence, and dropped questions
before being folded in. Disposition of every blocker:

**Dissolved by the v2 design (not fixed — structurally impossible now):**

- **C2** (refusal gated on the host's `confirm_close` UX toggle) — moot. D2 closes
  exactly one workspace, so there is no group destruction to refuse and no
  dependence on a host-side preference.
- **M6** (same keystroke destructive or not depending on tab count) — moot. Plain
  `tab.close` never forwards in v2, so its destructiveness no longer varies.
- **M7** (P3/P4 contradicting each other on UQ1) — resolved by the user's decision:
  plain verbs stay local, remote deletion is a separate opt-in verb.
- **Stuck mirrored TAB with no escape** (raised in addendum 2) — resolved. Because
  plain `tab.close` remains local-only, it IS the escape hatch for a tab whose
  `close_remote` never acks. The stranded PENDING entry is handled by the P3
  purge-gap fix.

**Fixed in v2:** C1 (via D2 — host closes exactly one), the mount-killing decode
skew (via D1 capability gating — independently recommended by both reviewers),
the shared-map id collision (I5 + T15).

**Still open, carried into P4:** M3/M5 (all three TUI gestures discard the
response, so `remote_close_pending` and `Failed` render as nothing; `modal.rs:1189`
is the DEFAULT route since `confirm_close` defaults true, so the branch must live
in `close_workspace_idx_via_api`). M4 (no pending TTL) accepted as follow-up.

**Reviewer self-correction, accepted:** M8 was downgraded MAJOR→MEDIUM by the
reviewer itself on new evidence — `tests/cli/panes.rs:322`
(`closing_workspace_terminates_processes_inside_it`) proves `workspace.close`
ALREADY kills processes, and cli-reference.mdx:143's "closes Herdr state only"
is about not deleting the git checkout, not about sparing processes. So the
federated case was the lone silent EXCEPTION to an otherwise destructive verb.
This strengthens rather than undermines the chosen design: the reviewer's own
revised recommendation names a new verb as "cleanest", which is what the user
chose. Note the sharpest artifact it surfaced —
`docs/next/api/herdr-api.schema.json:4711` pins `workspace.close` as a const, and
a pure SEMANTICS change would leave that schema byte-identical, i.e. undetectable
to every generated client. v2 avoids this entirely by adding new method names,
which DO change the schema.

**Controller disagreement with a reviewer (recorded, not silently overridden):**
the destructive reviewer concluded the outcome "closing a mirrored workspace
closes the same thing on the host" is NOT achievable for host worktree groups,
and recommended refusing outright. REJECTED: that assumes the only host path is
`close_selected_workspace()`. D2 uses a single-retire path instead, so closing
exactly one member of a host worktree group is both achievable and coherent —
`close_single_workspace_at` already exists and is the established single-workspace
teardown. Refusing would deliver strictly less for no safety gain, since the
amplification it guards against is already prevented by D2. The host agent was
asked to verify this independently and report contrary evidence if my reading is
wrong.

## Resolved questions (were UQ1/UQ2)

**UQ1 — CLOSED in favor of D2 (close exactly one).** Refusing outright would
recreate a dead end where a user can never delete a mirrored worktree-group
workspace from the client, while D2 has no such gap. "Silently does less than a
local close would" is the correct trade, because the thing it declines to do —
a remote peer triggering a host GROUP close — is precisely the critical finding.

**UQ2 — CLOSED: idempotent success for a not-found target.** Matches the pane
precedent (`creation.rs:1213-1218` treats an already-gone target as success, not
an error) and makes a double-press safe, which matters more once the outcome is
surfaced as a toast and retries become visible.

## Additional test amendments (from the final review round)

- **T4 must assert group INTEGRITY, not just sibling existence.** The only in-tree
  single-close primitive (`close_single_workspace_at`, creation.rs:1640-1646)
  works by setting `ws.worktree_space = None` and then running the group close,
  relying on `close_indices_for`'s single-index fallback. A test that merely
  counts surviving workspaces would pass while the group was silently dissolved.
  Assert every surviving sibling still holds its `worktree_space` membership.
- **T13/T14 must also cover ack-after-LOCAL-close**, not only ack-after-resync:
  a user may run plain `tab.close` on a mirror while a `tab.close_remote` is
  still pending, so the ack arrives for a tab already locally gone.
