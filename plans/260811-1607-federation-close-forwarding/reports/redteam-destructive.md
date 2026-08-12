# Red team — data loss / destructive semantics

Adversarial review of `plans/260811-1607-federation-close-forwarding/plan.md`.
Read-only; no files edited. Posture: refute. Every claim carries `file:line`.

> **ADDENDUM (post-P1, answering the lead's three prioritized questions) — see
> the "Addendum" section at the end of this report.** Short form: the
> unknown-variant-at-matching-version failure is a **full mount teardown, no
> panic, misattributed as `PeerClosed`** — and on the client it removes every
> mirrored workspace of that host, which is indistinguishable from a successful
> close (this upgrades m9 to **MAJOR**). The shared-map/shared-counter collision
> the lead flagged is **real and already correctly resolved** by the plan's new
> R5; one residual doc-comment fix is needed.

Threat calibration up front: this is one user mounting their own machine over
SSH. Nothing here is cross-tenant or a security incident. What is at stake is
**live processes and long-running agents on the user's other machine**, killed
by a keystroke whose current meaning is "stop looking at this". That is real,
irreversible loss (no undo, no confirmation copy naming the host), and the plan
underweights it in three places.

Verdict: **the plan is not safe to implement as written.** Two CRITICAL defects
and six MAJOR gaps below. Most are cheap amendments; C1 is a design error.

---

## C1 — CRITICAL. One mirrored workspace close destroys the host's whole worktree group

**PROVEN.**

The plan's P2 mitigation is: "a federation-originated close that would trigger
`confirmation_required` must be REFUSED and reported as `Failed`". That gate
**does not exist for workspace close**. `grep confirmation_required
src/app/api/workspaces.rs` → zero hits. `handle_workspace_close`
(`src/app/api/workspaces.rs:1001-1083`) has no confirmation branch at all,
unlike `handle_tab_close` (`src/app/api/tabs.rs:311-316`) and
`handle_pane_close` (`src/app/api/panes.rs:1877-1883`). The confirmation for
workspace close lives only in the TUI gesture layer
(`src/app/input/navigate.rs:227-229`), which a federation-originated command
never traverses.

What actually happens on the host: the target is a *local* workspace, so
`federated_origin` is `None` (`workspaces.rs:1037`), so `retire_one_only` is
false (`workspaces.rs:1069-1072`), so the handler calls
`self.state.close_selected_workspace()` (`workspaces.rs:1073`). That closes
`close_indices_for(selected)` (`src/app/actions.rs:1672`), which returns **every
workspace sharing the target's `worktree_space.key`** when there are ≥2 of them
(`src/app/state.rs:1862-1879`).

Concrete failure: host has repo `herdr` open as a worktree group of 3
workspaces (`master`, `issue/82`, `issue/91`), each with 2 panes running agents.
Client mounts, sees all 3 mirrored, closes **one**. Host executes
`close_selected_workspace()` → all 3 workspaces and all 6 panes are torn down,
every agent killed. The client user selected one thing and destroyed six.

Amendment: the host-side `FederationCommand::CloseWorkspace` arm must NOT reuse
`Method::WorkspaceClose` as-is. Either

- refuse when `self.state.workspace_close_would_close_worktree_group(idx)` is
  true (unconditionally — see C2) and answer `Failed`, or
- close exactly one workspace via `close_single_workspace_at`, never
  `close_selected_workspace`.

Add to the test matrix: **T13 — a forwarded close of a workspace that is one
member of a host worktree group closes exactly one workspace (or is refused);
the siblings survive.** The current matrix's T3 ("actor closes the right
workspace") would pass while this defect is live, because a single-workspace
fixture never forms a group.

---

## C2 — CRITICAL. The group-close refusal is gated on a host *UX preference*

**PROVEN.**

The pane precedent the plan tells P2 to mirror
(`src/app/api/panes.rs:1871-1883`) builds its federation refusal as:

```rust
let federation_would_close_worktree_group = id == "federation-close-pane"
    && self.state.close_pane_would_close_workspace(ws_idx, pane_id)
    && self.state.confirm_close                       // panes.rs:1875
    && self.state.workspace_close_would_close_worktree_group(ws_idx);
```

`confirm_close` is a user-facing config toggle documented as "Ask for
confirmation before closing a workspace" (`src/main.rs:296-297`), read at
`src/app/actions.rs:1980`. A host whose owner set `confirm_close = false` —
a pure interaction preference, chosen for the *local* keyboard experience —
silently loses the only barrier against a remote peer cascading a worktree-group
destruction. Copying this line into the new handlers propagates the defect to
two more verbs.

Amendment: the federation-originated refusal must be **unconditional on
`confirm_close`**. It is a trust-boundary rule, not a prompt preference. Fix the
new arms; file the `panes.rs:1875` instance as a follow-up (it is pre-existing,
but it is the exact line the plan says to imitate, so the plan must say
"imitate, minus `confirm_close`").

---

## M3 — MAJOR. The TUI shows the user nothing: not the pending state, not the failure

**PROVEN.**

Every TUI close gesture **discards the response string**:

- `close_workspace_idx_via_api` — `src/app/input/navigate.rs:452-455`
- `close_active_tab_via_api_requires_confirmation` — `navigate.rs:505`
- `confirm_close_accept_via_api` — `src/app/input/modal.rs:1189-1197`, which
  additionally resets `state.mode` to `Terminal`/`Navigate` immediately,
  unconditionally.

So `remote_close_pending` (the plan's I2 contract) and any `Failed` reason are
invisible. After P3, the user presses the close key on a mirrored workspace,
the confirm modal disappears, and **the workspace is still there**. There is no
spinner, no "closing…" marker, no toast. The natural response is to press it
again — which mints a second `request_id` and a second pending entry, and
double-sends a destructive request.

Pane close has a failure toast (`src/app/creation.rs:1281-1315`,
`handle_federation_close_pane_failed`), but the plan's P3 acceptance criteria
and test matrix (T6/T7) require only that teardown happen on ack — nothing
requires the failure path to exist at all for workspaces/tabs.

Amendment: P3 must explicitly require `Failed` handlers that mirror
`handle_federation_close_pane_failed` (drop pending + toast), and the plan must
add a pending affordance or a `*_pending` toast so the gesture is not silently
inert. Add **T14 — a `Failed` response drops the pending entry and raises a
toast.**

---

## M4 — MAJOR. A pending close can strand indefinitely; there is no timeout

**PROVEN.**

`pending_remote_closes` is a plain `HashMap<u64, PendingRemoteClose>`
(`src/app/mod.rs:169`). It is only ever removed by (a) an ack/fail response
(`src/app/creation.rs:922`), (b) `purge_pending_remote_closes_for_workspaces`
(`creation.rs:933-939`) driven from mount-ended (`workspaces.rs:555`) or local
close (`workspaces.rs:1046`). **There is no TTL and no reaper.**

Trace of "the ack never arrives":

- Host refuses (C1's amendment → `Failed`): pending is dropped, but the
  workspace stays local forever. With M3, the user sees nothing.
- Link drops: `handle_federation_mount_ended`
  (`src/app/api/workspaces.rs:472-585`) purges the pending maps
  (`:555`) and removes the workspaces (`:566-567`). So a link drop does
  self-heal — but by destroying the *entire local mirror*, which looks to the
  user like the close succeeded when the host was never touched.
- Host alive but wedged (never answers): the entry lives until the process
  exits. Every retry adds another.

The plan's I2 asserts confirmed teardown but never asserts liveness. Amendment:
state explicitly what the user's escape is (P4's unmount, see M5/M7), and either
add a bounded pending TTL or at minimum require idempotent de-duplication so a
repeated gesture does not stack requests.

---

## M5 — MAJOR. P4's file list omits `modal.rs`, the *default* path to workspace close

**PROVEN.**

`close_workspace_idx_via_api` (`navigate.rs:452`) has three callers:

| Caller | Reached when |
|---|---|
| `navigate.rs:230` (`NavigateAction::CloseWorkspace`) | only when `confirm_close == false` (`navigate.rs:227`) |
| `navigate.rs:498` (close-tab on a 1-tab workspace) | always |
| `src/app/input/modal.rs:1189` (`confirm_close_accept_via_api`) | **the default path — `confirm_close` defaults true (`src/main.rs:297`)** |

P4's file list is `schema.rs, api/mod.rs, api/server.rs, app/api/workspaces.rs,
app/runtime_mutations.rs, app/input/navigate.rs, logging.rs` — `modal.rs` is
absent. An implementer who adds the "mirrored → unmount" branch at the
`NavigateAction::CloseWorkspace` site leaves the *most common* path
(confirm-modal accept) routed at the destructive `workspace.close`. The plan's
headline safety property would be false for the default config.

Amendment: put the mirrored→unmount branch **inside**
`close_workspace_idx_via_api` (`navigate.rs:452`) so all three callers inherit
it, and add a test that drives the confirm-modal accept path on a mirrored
workspace and asserts no wire send.

---

## M6 — MAJOR. Same keystroke, opposite destructiveness, decided by tab count

**PROVEN.**

`close_active_tab_via_api_requires_confirmation`
(`src/app/input/navigate.rs:485-511`):

- workspace has **1 tab** → `close_workspace_idx_via_api` (`:498`) → after P4,
  **unmount**. Harmless.
- workspace has **2+ tabs** → `runtime_tab_close` (`:505`) → after P3,
  **forwards a destructive tab delete to the host**, killing that tab's panes.

And the non-last-tab path has **no confirmation whatsoever** — `tabs.rs`'s only
`confirmation_required` is on the `closes_workspace` branch (`tabs.rs:311-316`).
So the more destructive of the two outcomes is the *unconfirmed* one.

The user's mental model is "close this tab". Whether that kills processes 400km
away now depends on a count they are not thinking about. P4 addresses the
workspace gesture and says nothing about the tab gesture; there is no
tab-granularity unmount at all.

Amendment: P4 must decide the tab gesture too. Minimum: a confirmation for a
federated tab close that names the host, or route the destructive tab delete
behind an explicit, distinct action.

---

## M7 — MAJOR. P4 makes the workspace half of the feature TUI-unreachable

**PROVEN by construction from the plan + `navigate.rs:452/498`, `modal.rs:1189`.**

If every TUI route to a mirrored workspace close maps to unmount (P4's proposal,
and the correct one for M5), then the only caller left for the new destructive
`workspace.close` forwarding is the CLI/socket (`src/cli/runtime.rs:44-49`).
P1-P3's entire workspace half ships as a path no interactive user can reach,
while the tab half (M6) ships live and unguarded. The plan's stated outcome —
"Closing a mirrored workspace … closes the same thing on the host" — is not
delivered by the plan.

This is the plan's own Unresolved Question 1, but the plan does not treat it as
blocking: P1-P3 are specified as if the answer were "delete-on-host" and P4 as
if it were "unmount". Those two answers cannot both ship.

Amendment: resolve UQ1 **before** P1. If the answer is unmount, either add an
explicit destructive affordance (e.g. a second button in the confirm-close
overlay, `src/ui/dialogs.rs:666-742`, whose title/detail names the host) or cut
the workspace forwarding from scope and ship only the reported tab bug + the
`workspace.unmount` verb.

---

## M8 — MAJOR. Silent breaking change to a documented public contract; the safe default is inverted

**PROVEN.**

The plan changes what an existing, shipped, **documented** JSON-API/CLI method
does:

- `website/src/content/docs/cli-reference.mdx:143` — "`workspace close` closes
  Herdr state only." (also `ja/` :139 and `zh-cn/` :139)
- `website/src/content/docs/socket-api.mdx:103` lists `workspace.close` in the
  public method table.
- `herdr workspace close <workspace_id>` is a first-class CLI verb
  (`src/cli/runtime.rs:44-49`, `cli-reference.mdx:123`).

Who relies on today's meaning: any script that iterates `workspace.list` and
closes what it does not need — a perfectly reasonable cleanup loop written
against the documented "Herdr state only" guarantee. After this change, run on a
machine with a mount live, that loop kills remote processes. It will not error,
warn, or ask. No version negotiation exists on the JSON API to detect the
semantic flip.

**The strongest case against the plan's choice**, stated plainly: the plan makes
the *old, familiar, widely-called* name destructive and the *new, unknown* name
safe. Every existing caller is silently upgraded onto the destructive branch and
must be individually audited and rewritten to opt back out. The inverse —
`workspace.close` keeps its documented meaning, and a **new** verb
(`workspace.close_remote`, or `workspace.close` with an explicit
`{"on_host": true}` field defaulting false) carries the destruction — has
strictly better properties on every axis that matters here:

- zero existing callers change behavior; the docs sentence stays true;
- the destructive capability is opt-in by construction, not by a follow-up
  remapping in `navigate.rs` that M5 shows is easy to get wrong;
- it does not depend on P4 landing correctly for P3 to be safe (today, P3 alone
  is a live footgun until P4 lands — and P3/P4 are separate phases);
- it removes M7 entirely: the destructive verb is new, so nothing needs
  rebinding, and the TUI can adopt it deliberately.

The only cost is one extra method name. The plan's rationale for the split
(D1 in `auto-decisions`) argues for *having* an unmount verb; it never argues
that the destructive meaning must inherit the existing name. I can find no
evidence in the plan or reports that this specific trade was examined.

Amendment: invert the split. Keep `workspace.close` = today's behavior; add the
new destructive verb. If the plan's direction is kept anyway, it must at minimum
(a) update `cli-reference.mdx:143` and the two translations in the same change,
(b) add a `docs/next` entry, and (c) classify the change as a breaking API
change in the changelog.

Adjacent, non-blocking: adding `Method::WorkspaceUnmount` forces the exhaustive
match in `federated_session_allows` (`src/api/mod.rs:100+`, deliberately
wildcard-free per its doc comment at `:90-93`). P4's file list covers
`src/api/mod.rs`; the plan should state the classification — **forbidden** for a
view-only federated session, alongside `WorkspaceClose` at `src/api/mod.rs:161`.

---

## m9 — MINOR (operational). Unknown variant at the same version tears the mount down and *mimics* a successful close

**PROVEN mechanism; exposure is deployment-dependent.**

`codec::decode` rejects only on a *version* mismatch (`codec.rs:94-99`). A frame
carrying a variant the peer's `FederationMessage` enum lacks fails serde, and
`read_frame_blocking` maps any decode error to an `io::Error`
(`src/server/federation_accept.rs:144-145`) that kills the read loop. The code
says so itself at `federation_accept.rs:1226-1230`: emitting a variant the peer
never negotiated "fails its decoder and tears down its whole mount."

I verified the plan's version claim: the `FEDERATION_PROTOCOL_VERSION = 6`
commit is `870a4bfd`, `git tag --contains` returns nothing, and
`git branch --contains` returns only `feat/federation-multi-tab-workspace`.
So no *released* peer is at v6 and released peers are cleanly rejected. The
residual exposure is hand-deployed dev builds — which on this fork is the normal
state of affairs.

Why it matters more than a generic skew bug: on the client, the faulted mount
drives `handle_federation_mount_ended` → `close_selected_workspace()`
(`workspaces.rs:566-567`), which removes **every** workspace of that mount. The
user pressed "close workspace", the entire mirror vanished, and the host was
never touched. That is the most confusing possible failure shape for a
destructive verb.

Amendment: restore counsel assumption 2 (dropped from the plan's non-goals) as
an explicit P5 deploy-order requirement — upgrade the **host** binary first —
and note the failure signature so it is not misdiagnosed as a successful close.

---

## m10 — MINOR. The tab-close refusal check mutates host state before it refuses

**PROVEN.**

P2 says the refusal must never "hijack the host's session into a modal". But
`handle_tab_close`'s existing gate calls
`self.state.confirm_implicit_worktree_group_close(ws_idx)` (`tabs.rs:311`), and
that function **mutates before returning true** — `self.selected = ws_idx;
self.mode = Mode::ConfirmClose;` (`src/app/actions.rs:1979-1987`). Merely
reaching that line from a federation-originated close pops a modal on the host
user's screen and moves their selection, even if the response is `Failed`.

`panes.rs` avoids this with the non-mutating predicate
`workspace_close_would_close_worktree_group` (`panes.rs:1877`). The plan's P2
states the goal but not the mechanism, and the obvious implementation ("reuse
`Method::TabClose` via `handle_api_request_after_internal_events_drained`",
P2 bullet 2) walks straight into `tabs.rs:311`.

Amendment: P2 must name the non-mutating predicate explicitly and require the
federation branch to short-circuit **before** `tabs.rs:311`. T5 must assert the
host's `mode` is unchanged, not just that the response is `Failed`.

---

## Attacks that FAILED — plan claims that hold up

Stated so the amendments above are not read as blanket distrust.

- **Session restore is not a vector.** I3 is correct. Federation-materialized
  workspaces are filtered out of the persisted snapshot:
  `is_federation_materialized` (`src/persist/snapshot.rs:251-261`) is applied at
  `snapshot.rs:313` and `:319`, with a regression test at `:953`. A restored
  session cannot resurrect a mirror and cannot route a restore-time teardown
  into the close handlers.
- **Host-side partial failure is not real.** `handle_workspace_close` has
  exactly one bail-out, the id/index validation at `workspaces.rs:1001-1007`.
  Past that point every step is infallible, so there is no half-closed host
  state. The client/host divergence risk is a lost *response*, which reconnect
  resync reconciles. The genuine "partial" hazard is amplification (C1), not
  atomicity.
- **Second client is bounded.** Per D2, `handle_connection` admits a single
  controller. The concurrent second actor is the host's own local user, and the
  loser of that race gets a clean `workspace_not_found`.
- **I1's layering rule is correct and well-evidenced.** The five-caller table in
  D3 matches `close_single_workspace_at`'s call sites.

---

## Answers to the six posed attack angles

1. **Irreversible loss.** Muscle-memory flip is real and is the dominant risk
   (M6, M7); scripted CLI callers are a second, quieter one (M8); restore is
   not a vector. P4's mapping is **not sufficient as specified** — it misses
   `modal.rs` (M5, the default path) and says nothing about tabs (M6). The
   confirm overlay itself is a trap: `confirm_close_overlay_text`
   (`src/ui/dialogs.rs:600-663`) renders "Close workspace? — `<name>` — N panes"
   with **no mention of the host**, so even the confirmed path never tells the
   user they are about to kill processes on another machine.
2. **Verb split.** Yes, a silent breaking change to a documented contract, and
   the safe default is inverted. See M8 for the full argument and the strictly
   safer alternative.
3. **Confirmed-teardown gaps.** Yes, a close can strand (M4: no TTL), and the
   user sees nothing at all (M3: all three gestures discard the response). A
   user would read it as "the close key stopped working."
4. **Partial failure.** Not an atomicity problem — an amplification problem
   (C1). Host-side close is all-or-nothing; the "all" is just far larger than
   what the client asked for.
5. **Worktree-group refusal.** With P4's unmount gesture the user is not
   *permanently* stuck — they can still stop viewing. But per M3 they are never
   told why the destructive close failed, and per C2 the refusal only fires when
   the host happens to have `confirm_close = true`. Reporting `Failed` is
   acceptable **only** if paired with a toast (M3) and an unconditional gate
   (C2).
6. **Underweighted in the plan's own UQs.** UQ1 is not a "product call" to be
   deferred — P3 and P4 encode contradictory answers to it (M7), so it blocks
   P1. UQ2 (idempotent vs `Failed`) is minor by comparison. Missing from the UQ
   list entirely: the docs contract (M8), the absent confirmation gate in
   `handle_workspace_close` (C1), and the absence of any user-visible pending
   state (M3).

---

## Recommended plan amendments, in order

1. **Resolve UQ1 before P1** (M7). P3 and P4 currently disagree.
2. **Invert the verb split** (M8): new destructive verb, `workspace.close`
   unchanged. If rejected, ship the docs change in the same commit.
3. **Rewrite P2's host-side bullet** (C1): forbid reuse of
   `Method::WorkspaceClose`'s group-close path; require single-workspace close
   or unconditional refusal. Add T13.
4. **Drop `confirm_close` from the federation refusal condition** (C2).
5. **Move the P4 gesture branch into `close_workspace_idx_via_api`** and add
   `src/app/input/modal.rs` to P4's file list (M5).
6. **Specify the tab gesture** (M6) — confirmation or a distinct action.
7. **Require `Failed` handlers with toasts + a pending affordance** in P3 (M3);
   add T14.
8. **Name the non-mutating worktree predicate in P2** and strengthen T5 to
   assert the host's `mode` is unchanged (m10).
9. **Add a deploy-order note** (host binary first) to P5 (m9).

## Unresolved questions

- Is there any real consumer of `workspace.close` today (user scripts, plugins,
  the `.claude` tooling) that would be silently upgraded onto the destructive
  branch? I found no in-repo caller beyond the CLI and the TUI, but I cannot see
  the user's own scripts.
- Should a forwarded close of a host worktree-group member close one workspace
  or refuse? Refusing is safer; closing one is friendlier and matches what the
  client user actually selected. This needs a product answer (it is the
  amendment for C1 and I recommend refusal until the confirm UX names the host).
- Does the client's mirror surface enough host context (host name, pane count,
  running agents) at close time to write an honest confirmation string? The
  current overlay (`src/ui/dialogs.rs:600-663`) does not.

---

# Addendum — answers to the three prioritized questions

## A1/A2 — What a v6 host does with `WorkspaceCloseRequest` it cannot decode

**Answer: it tears down the entire mount. No panic, no typed error to the
sender, no silent wedge — and the host misattributes the cause as `PeerClosed`.
PROVEN, end to end.**

Full trace, matching version, unknown variant:

1. **Header passes.** `codec::decode` compares only the version word
   (`src/remote/federation/protocol/codec.rs:89-99`). Versions match (6 == 6),
   so it does **not** return `VersionSkew`. Length/cap checks pass (`:101-118`).
   The payload is handed to `serde_json::from_slice` (`:120-122`).
2. **Serde hard-errors.** `FederationMessage`
   (`src/remote/federation/protocol/mod.rs:668-696`) carries only
   `#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]` — no
   `#[serde(other)]`, no `#[serde(untagged)]`, no `rename_all`. It is therefore
   serde's default **externally tagged** representation, and an unrecognized
   variant key is an "unknown variant" error, not an ignorable field.
   The repo states this itself as settled fact at
   `src/remote/federation/client.rs:146-151`: *"`FederationMessage` is an
   externally-tagged enum, so a peer built before this variant existed fails to
   decode the frame, its `read_frame` returns `Err`, and its whole mount tears
   down — every pane on that link dies because of one paste."*
3. **The error is widened to an I/O error.** Host (sync) path:
   `read_frame_blocking` does
   `codec::decode(...).map_err(|err| io::Error::other(err.to_string()))?`
   (`src/server/federation_accept.rs:144-145`). Client (async) path is
   identical: `src/remote/federation/serve.rs:194-195`. Note the shape — a
   *typed* `CodecError` is flattened into an untyped `io::Error` carrying only a
   string, so no caller downstream can distinguish "unknown variant" from
   "socket died".
4. **The read loop treats it as the peer vanishing.** The command loop's `Err`
   arm (`src/server/federation_accept.rs:571-576`) does
   `first_cause.set(TunnelExit::PeerClosed); return Err(err);`. The loop's
   `Ok(Some(_other))` catch-all at `:566-569` ("other inbound frames are ignored,
   not treated as fatal") **is never reached** — that arm only catches variants
   the enum *can* decode. Decode failure short-circuits above it.

So the ordering is: no panic (nothing `unwrap`s the decode), no reply of any
kind to the sender, and the connection dies at once.

**Why this is worse than a generic skew bug, and why I am raising m9 to
MAJOR.** The failure is silent *and* mimics success:

- Host-side diagnosis is actively misleading. `TunnelExit::PeerClosed` says the
  *peer* hung up. In reality the host's own decoder rejected a frame. Anyone
  reading host logs will chase a network/SSH problem.
- Client-side, the faulted link drives `handle_federation_mount_ended`
  (`src/app/api/workspaces.rs:472-585`), which calls
  `self.state.close_selected_workspace()` (`:566-567`) — removing **every**
  workspace of that mount, not just the one the user targeted.

Concrete scenario: user has 3 workspaces mirrored from a host running a
hand-built current-`master` v6 binary. They press close on one. The client sends
`WorkspaceCloseRequest`; the host's decoder rejects it and kills the link; the
client's mount-ended path wipes **all 3** mirrored workspaces from the screen.
The user sees more than they asked for disappear, concludes the close worked
(too well), and the host is **completely untouched** — every process still
running. The next mount attempt silently resurrects all 3, which reads as a bug
elsewhere entirely.

The plan's non-goals dismiss the version question purely on tag evidence, which
I independently confirm is correct **for tags**: the `FEDERATION_PROTOCOL_VERSION
= 6` commit is `870a4bfd`; `git tag --contains 870a4bfd` is empty and
`git branch --contains 870a4bfd` returns only
`feat/federation-multi-tab-workspace` — v6 is not on `master` and not in any
tag, so every *released* peer is v5 and is cleanly rejected at the header
(`codec.rs:94-99`, test at `codec.rs:349-371`). The exposure is entirely
hand-deployed builds, which on this fork is normal practice.

**Amendment (raise m9 to MAJOR):**

1. Add an explicit P5 deploy-order gate: **upgrade the host binary before the
   client**, for every host in the fleet, and record which hosts were updated.
   A newer client against an older-v6 host is the only broken combination; the
   reverse is safe because the new host only ever emits the new response
   variants in reply to a request an old client cannot send.
2. Document the failure signature in the plan so it is not misdiagnosed:
   *"whole mirror vanishes on close + host log says `PeerClosed` + host state
   unchanged" == protocol skew, not a successful close.*
3. Optional but cheap and strictly better than either: negotiate a
   `Capability` for the close verbs, exactly as file staging already does
   (`src/remote/federation/protocol/mod.rs:76-81` documents capabilities as the
   sanctioned additive-evolution path, and `client.rs:144-152` is the
   gate-every-send precedent). Then an unsupported host degrades to a clean
   local refusal instead of a mount kill. This is the same defect class the
   project already solved once for clipboard staging; the plan re-introduces it.

## A3 — Shared pending map + per-purpose counters: the collision is REAL

**CONFIRMED as a genuine defect — and the plan's new R5 already fixes it
correctly. No further change needed beyond one doc-comment repair.**

Verification. Every request-id generator in the codebase is an independent
`static NEXT_ID: AtomicU64 = AtomicU64::new(1)`:

- `next_remote_split_request_id` — `src/app/api/panes.rs:34-37`
- `next_remote_close_request_id` — `src/app/api/panes.rs:44-47`
- `next_remote_workspace_create_request_id` — `src/app/api/workspaces.rs:20-23`
- `next_clipboard_stage_request_id` — `src/app/remote_clipboard_stage.rs:131-134`

They all start at **1**. Separate counters do not avoid collisions — they
*guarantee* them: split #1 and close #1 both exist simultaneously. What makes
that safe today is exclusively that they land in **different maps**
(`pending_remote_splits` vs `pending_remote_closes`, `src/app/mod.rs:169`).

So the lead's hypothesis is correct: had R4's single map been paired with a new
`next_remote_tab_close_request_id()` starting at 1, the concrete failure is:

1. User closes a mirrored **pane** → `next_remote_close_request_id()` → id `1` →
   `pending_remote_closes.insert(1, {kind: Pane, ...})`.
2. User closes a mirrored **tab** on the same mount → new counter → also id `1`
   → `insert(1, {kind: Tab, ...})` **overwrites** the pane entry.
3. The host's `ClosePaneResponse::Closed { request_id: 1 }` arrives.
   `handle_federation_close_pane_ready` (`src/app/creation.rs:1187-1204`)
   validates only `pending.origin != origin` — a **HostKey** comparison. Both
   entries came from the same mount, so **the origin check passes**. It is not a
   defense against this at all.
4. Result: the pane ack pops the *tab's* pending entry. Depending on how
   carefully the kind tag is checked, this either tears down the wrong object
   (a whole tab instead of one pane — destructive) or, at best, drops the tab's
   pending entry, stranding it forever per M4 while the pane close silently
   never completes.

The plan's **R5** states exactly this and mandates the single shared
`next_remote_close_request_id()`. I verified its cited line numbers
(`panes.rs:39-43` for the misleading comment, `panes.rs:44-47` for the
generator) — both correct. **R5 is right; adopt it as written.**

One residual, MINOR: R5 must also **correct** the comment at
`src/app/api/panes.rs:39-43`, which currently claims a separate counter is used
"so a split and a close minted at the same time can never collide". That stated
rationale is backwards and, left in place next to a now-shared map, is precisely
the kind of once-true comment that invites the next implementer to add a fourth
counter. Rewrite it to say what actually holds: *ids are unique within one
pending map; every entry in `pending_remote_closes` must be minted from this one
counter; `pending_remote_splits` is a separate map and so may use its own.*

Add **T15 — a pane close and a tab close registered back to back hold two
distinct entries in the shared map, and the pane ack tears down the pane, not
the tab.** This is the regression test that pins R5; without it, a future
"tidy-up" that reintroduces a per-verb counter passes every other test in the
matrix.

## Cross-check against the landed P1

Confirmed present on `feat/federation-multi-tab-workspace`: the four variants at
`src/remote/federation/protocol/mod.rs:682-685`, all mapped to
`Channel::Control` at `:711-712`, and `FEDERATION_PROTOCOL_VERSION = 6`
unchanged at `:74`. The client stub arms are the right call — a `todo!()` in the
receive loop at `client.rs:588+` would panic the drive task and, by the same
path as A1, wipe the mirror.

One note on P1's own correction text: it says the exhaustive match is at
`client.rs:591`. The `match msg` is at `client.rs:590`, entered from the
`read_frame` at `:588`. Immaterial to the reasoning.

---

# Addendum 2 — in-tree dependency evidence, and a correction to my own M8

## Correction: M8 drops from MAJOR to MEDIUM

I went looking for in-tree signals that something depends on today's
`workspace.close` semantics. What I found partly **refutes my own M8 framing**,
and the correction changes the recommendation, so it is stated up front.

**`workspace.close` already kills processes, and there is a test that pins it.**
`tests/cli/panes.rs:322` — `closing_workspace_terminates_processes_inside_it` —
starts a `python3 ... time.sleep(1000)` in a pane, records its pid, runs
`herdr workspace close <id>` via the real CLI, and asserts
`wait_for_pid_exit(pid, ...)` with the message *"process {pid} survived workspace
close"* (`tests/cli/panes.rs:369-379`).

So my earlier reading of `website/src/content/docs/cli-reference.mdx:143`
("`workspace close` closes Herdr state only") was too strong. Read in context —
the very next clause is "`worktree remove` is the explicit checkout deletion
path" — that sentence is about **not deleting the git checkout or branch**, not
about sparing processes. It is not a promise that processes survive.

That reframes the change materially, and in the plan's favor:
`workspace.close`'s meaning is already "terminate this workspace and the
processes in it". The federated case is the **single silent exception** — and
that exception is exactly what `src/app/creation.rs:3134-3139` calls "the
locally-initiated federated unmount". The plan removes an inconsistency; it does
not invent a destructive meaning out of nothing. **M8 is therefore MEDIUM, not
MAJOR, and I withdraw "silent breaking change" as the characterization.**

**What survives the correction, and still justifies action:** in that one
exception case, today's behavior is the *safe* one, and the change silently
widens the blast radius across a **machine boundary**. A script that closes
workspaces it does not need — a reasonable thing to write against a verb that is
documented and tested as workspace-scoped — kills processes on a *different
computer* the moment it runs while a mount happens to be live. Nothing in the
request, the response, or the schema lets the caller detect that. The API path
has no confirmation of any kind (C1: `handle_workspace_close`,
`src/app/api/workspaces.rs:1001-1083`, has no confirmation branch at all).

Revised amendment (weaker than my original "must rename", and rebased on
blast-radius rather than on contract breakage): a **new** destructive verb is
still the cleanest option, but an explicit opt-in parameter on the existing one
(`workspace.close` + `{"on_host": true}`, defaulting false) is now equally
acceptable and cheaper. What is **not** acceptable is the plan's current shape —
destruction crossing a machine boundary with no opt-in, no confirmation, and no
signal in the response.

## In-tree dependency signals: what exists

Searched `*.sh *.py *.mjs *.js *.ts *.json *.toml *.lua *.fish *.zsh *.bash`
across the repo (excluding `target/`, `node_modules/`), plus `skills/`,
`scripts/`, `tests/`, `justfile`, and the plugin docs.

| Signal | Weight |
|---|---|
| `docs/next/api/herdr-api.schema.json:4711` — `"const": "workspace.close"` in the published machine-readable JSON Schema | **Strongest.** This is exactly the "machine-readable contracts" artifact class the repo's own documentation rule calls out. Its critical property here: a **semantics** change leaves the schema byte-identical, so the change is invisible to every generated client by construction. No schema consumer can detect it, ever. |
| `tests/cli/panes.rs:322-381` — asserts `workspace close` terminates the pane's child process | Strong, but see the correction above: it **supports** the plan's direction rather than opposing it. |
| `tests/api_ping.rs:1611` — `{"method":"workspace.close","params":{"workspace_id":"1"}}` over the real socket | Weak. A local-workspace lifecycle test; no federation involvement. |
| `src/cli/runtime.rs:44-49` + `cli-reference.mdx:123` (+ `ja/` :119, `zh-cn/` :119) | The user-facing verb exists in three languages; a semantics change needs all three updated. |
| Plugins | **None.** No plugin doc, skill, or script in-tree invokes `workspace.close`. |

Honest bottom line on "does anything depend on today's semantics": **no in-tree
caller does.** The dependency risk is entirely out-of-tree (the user's own
scripts) and unfalsifiable from here. I am not going to inflate it. The schema
row is the one that matters, and it matters for a structural reason rather than
a headcount one — the change is undetectable to schema-driven clients.

## Item 2 — stuck state: does `workspace.unmount` actually cover it?

**Mostly yes for workspaces; NO for tabs.** Detail:

- Ack never arrives, host alive but wedged: the mirror stays (I2's ack-gated
  teardown), pending never expires (M4 — `pending_remote_closes`,
  `src/app/mod.rs:169`, has no TTL and only workspace-scoped purges at
  `src/app/creation.rs:933-939`). `workspace.unmount` **does** get the user out,
  provided P4 wires it where all three gestures reach it (M5 — the plan's file
  list omits `src/app/input/modal.rs:1189`, the default path).
- Host answers `Failed` (including the worktree-group refusal, item 3): same —
  unmount is the escape, and the pending entry is dropped.
- Link drops: self-heals, but destructively —
  `handle_federation_mount_ended` (`src/app/api/workspaces.rs:472-585`) calls
  `close_selected_workspace()` (`:566-567`), removing **every** workspace of that
  mount, not just the stuck one.
- **The genuine gap: a stuck mirrored TAB.** There is no `tab.unmount`, and no
  tab-granularity local teardown at all. If a tab close forwards and the host
  never acks or answers `Failed`, the user's only exits are (a) close the whole
  workspace, or (b) drop the mount. Both are far larger than what they asked
  for. Combined with M3 (the TUI discards the response, so they are told
  nothing) the user cannot even tell the tab is stuck versus the key being
  broken.

Amendment: P4 must state the tab answer. Either add tab-granularity local
removal, or accept that the tab close is non-recoverable-until-remount and say
so in the plan and in the failure toast.

## Item 3 — the worktree-group refusal: limitation, not a dead end

**Not a dead end, and not a new bug — but only because unmount exists, and only
if the refusal is made legible.**

If P2 inherits `panes.rs:1871-1883`'s refusal for workspace close, then a
mirrored workspace whose **host-side** counterpart belongs to a worktree group
can never be deleted-on-host from the client. The user retains: unmount (stop
viewing, per P4) locally, and closing it on the host directly. So they are not
trapped — the workspace is removable from their screen.

Three things make this acceptable rather than a bug, and all three are
conditions the plan does not currently meet:

1. **The refusal must be legible.** Per M3 the TUI discards the response
   string (`navigate.rs:452-455`, `:505`, `modal.rs:1189-1197`), so today a
   `Failed{reason}` renders as *nothing at all*. A refusal the user cannot see
   is indistinguishable from a dead end. The toast must name the reason and the
   remedy ("this workspace is part of a worktree group on `<host>`; close it
   there, or unmount here").
2. **The refusal must not depend on the host's `confirm_close`** (C2,
   `panes.rs:1875`) — otherwise the "dead end" and the "group destruction" are
   selected by a host-side UX preference, which is the worst of both.
3. Refusing is strictly better than the alternative. The alternative is C1:
   `close_selected_workspace()` (`src/app/api/workspaces.rs:1073`) resolving
   `close_indices_for` (`src/app/state.rs:1862-1879`) to the whole group. Given
   the choice between "this one action is unavailable from the client" and
   "this one action silently destroys N workspaces", refusal is obviously
   correct. I recommend it.

The honest cost: the plan's stated outcome — "closing a mirrored workspace
closes the same thing on the host" — is **not achievable** for host worktree
groups. The plan should say so in Non-goals rather than let it surface as a
user-visible surprise.

Status: DONE
Summary: The plan's own P2 mitigation targets a `confirmation_required` gate
that does not exist in `handle_workspace_close`, so a forwarded close of one
mirrored workspace would run `close_selected_workspace()` and destroy the host's
entire worktree group — every sibling workspace and every running agent in them
(C1). Beyond that, the verb split inverts the safe default by making the
documented, already-scripted `workspace.close` the destructive one (M8), and
P3/P4 encode contradictory answers to the plan's own Unresolved Question 1,
leaving the workspace half of the feature either TUI-unreachable or unguarded
(M7).
