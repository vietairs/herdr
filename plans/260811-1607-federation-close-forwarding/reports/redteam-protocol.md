# Red-team: protocol correctness, compatibility, races

Angle: protocol/compat/races only. Read-only. Paths relative to the worktree
`/Users/hvnguyen/Projects/herdr/.claude/worktrees/federation-multi-tab-workspace`.

**Material fact the plan does not state:** P1 is already implemented in the
working tree (`src/remote/federation/protocol/mod.rs:391-441`, `:682-713`;
placeholder client arms at `src/remote/federation/client.rs:795-813`). So the
findings below are against real code, not plan text, for P1; P2/P3/P4 are still
plan text.

---

## Finding 1 — the no-bump decision is right about releases and wrong about the peer set that actually matters

**Severity: CRITICAL. PROVEN (mechanism); the trigger is prospective, not yet live.**

### What I verified empirically

I scanned all 87 tags for `FEDERATION_PROTOCOL_VERSION`:

- No tag, prerelease or otherwise, ships v6. The preview-tag hypothesis in the
  brief is **REFUTED**: `v0.8.0-hvn.2` and `v0.8.0-hvn.1` are both v5, and
  nothing in the repo's tag history is v6.
- `origin/master` is also v5 and has neither `WorkspaceCreateRequest` nor the
  close variants.

So the plan's literal claim ("v6 has not shipped in a release") is **true**.

### Why it is still the wrong test

v6 exists in exactly one place: the committed head of this branch.

```
feat/federation-multi-tab-workspace  -> FEDERATION_PROTOCOL_VERSION = 6
                                        WorkspaceCloseRequest occurrences: 0
```

(commit `870a4bfd`, 2026-08-11, "feat: create remote workspaces over the
federation wire" — the 5->6 bump; branch head `8eea3a59`.)

That gives **two distinct wire dialects both labelled 6**:

| dialect | where | has `WorkspaceCreateRequest` | has the 4 close variants |
|---|---|---|---|
| **v6-A** | committed branch head `8eea3a59` | yes | **no** |
| **v6-B** | working tree (P1 applied) | yes | yes |

The version field cannot distinguish them. `codec.rs:94` compares `6 == 6`,
passes, and hands the payload to `serde_json`. `FederationMessage` is an
externally-tagged enum, so a v6-A peer hits an unknown variant tag and fails
the whole frame.

The repo already documents this exact failure class as fatal, in its own words
(`src/remote/federation/client.rs:146-151`):

> An ungated send is fatal, not merely useless: `FederationMessage` is an
> externally-tagged enum, so a peer built before this variant existed fails
> to decode the frame, its `read_frame` returns `Err`, and its whole mount
> tears down — every pane on that link dies because of one paste.

The plan's non-goal reuses the version-history reasoning that protected v3/v4/v5
(each of which *was* deployed before its follow-on addition) and concludes v6 is
exempt. The exemption holds only while no v6-A **binary** exists on either side
of a link. The plan states no rule that keeps that true.

### Is the trigger real today?

**Not yet — verified.** Two live federation peers are running right now
(`herdr server` PID 24716 dialling `federation-serve` on `appn-ltu-vm-100` and
`appn-ltu-vm-105`), all from `~/.local/bin/herdr` dated **Aug 8**, i.e. built
before the Aug 11 v6 bump. Those peers are v5, so a v6 client meets them with a
clean `VersionSkew` at handshake. Good.

The trigger arrives during this branch's own validation cycle: the established
workflow for a federation-protocol change is to deploy the branch binary to both
ends. The moment v6-A is deployed to a remote host for multi-workspace
validation and a v6-B binary is run locally (or vice versa) — which is the
normal, expected sequence for landing P1 — the link is v6↔v6, handshake
succeeds, and the first close request kills the mount.

**Amendment (pick one, in order of preference):**

1. **Bump to 7.** The 6->7 bump costs one constant and one assertion; it turns
   this from a mid-session mount kill into a clean handshake reject. The
   CLAUDE.md rule ("bump only if source is not already ahead of the latest
   *release*") is a rule about *release* churn; it does not contemplate two
   incompatible builds sharing one unreleased version number. This is the only
   amendment that is safe regardless of what gets deployed where.
2. If the no-bump stands, gate all four variants behind a `Capability`
   (`Capability::FILE_STAGING` at `client.rs:154-160` is the working precedent:
   unknown capabilities are dropped from the agreed set rather than being
   fatal), and route every send through a single gated helper as
   `send_clipboard_stage_request` does.
3. At minimum, add to the plan an explicit, blocking deployment rule: *no v6-A
   binary may be deployed to any host*, and rebuild/redeploy both ends together.
   This is the weakest option — it is a process guarantee protecting a wire
   invariant.

---

## Finding 2 — an unknown variant at a matching version kills the mount and misreports the cause as `PeerClosed`

**Severity: MAJOR (observability). PROVEN.**

Exact trace for a v6-B frame arriving at a v6-A host:

1. `codec.rs:94` — version check passes (6 == 6).
2. `codec.rs:123` — `serde_json::from_slice` fails on the unknown enum tag,
   mapped to `CodecError::Malformed`.
3. `federation_accept.rs:144-145` — `read_frame_blocking` converts it with
   `io::Error::other(err.to_string())`.
4. `federation_accept.rs:566-571` — the reader loop's `Err(err)` arm runs
   `first_cause.set(TunnelExit::PeerClosed)` and returns `Err`, ending the
   connection.

Answers to the brief's questions:

- **Not** a panic, **not** a silent stall, **not** a clean `VersionSkew`.
- It is a **full connection teardown** — every mirrored pane on that mount dies,
  not just the close.
- The cause recorded and reported to the peer via `Fault` is
  **`PeerClosed`** — semantically "the peer hung up cleanly." The real cause
  (an undecodable frame) survives only in the `io::Error` string. The user sees
  a mount that dropped, attributed to the wrong side, with no version-skew
  message. That is the "silent-ish" breakage the brief suspected, and it is
  worse than suspected because the label actively misdirects diagnosis.
- Note `first_cause` is set **unconditionally** in that arm — a genuine decode
  failure cannot even be distinguished from a socket error downstream.

Same shape on the client side (`client.rs:588`, `read_frame(reader).await?`
propagates out of the drive loop).

**Amendment:** in `reader_loop`'s `Err` arm, distinguish a decode/`Malformed`
failure from a transport failure and record a distinct `TunnelExit` (or at
minimum `tracing::error!` the error string at the teardown site). A mount that
dies from a protocol defect must not be reported as `PeerClosed`. This is worth
doing independently of Finding 1, since it is the diagnostic that would make
Finding 1 debuggable in minutes instead of hours.

---

## Finding 3 — request-id collision under the revised one-map design

**Re-assessed against plan revision 16:31 (R4/R5), which post-dates my first pass.**

### 3a. The collision itself — REAL, and the plan already caught it

**Severity: would be CRITICAL. Already mitigated by R5. CONFIRMED as a real hazard.**

Under the original three-parallel-maps design my verdict was "refuted", because
correlation was **type-routed**: `ClosePaneResponse` was the only thing that
could reach `pending_remote_closes` (`client.rs:1024-1050` ->
`AppEvent::FederationClosePaneReady` -> `creation.rs:1187`), so two counters
both starting at 1 could never meet.

R4 (plan:85-90) consolidates pane/tab/workspace closes into **one** map. That
removes the type-routing guarantee, and the lead's hypothesis becomes correct:
two per-purpose `AtomicU64`s both seeded `new(1)` (`panes.rs:34-47`) would write
colliding keys into one `HashMap<u64, PendingRemoteClose>`. `HashMap::insert`
**overwrites**, so the first pending entry is silently destroyed — the failure
is not only "pops the wrong entry" but also "the overwritten close is lost
forever", and with no timeout on the map (Finding 7) it never surfaces.

**R5 (plan:71-83) already mandates the correct fix**: all three mint from the
existing `next_remote_close_request_id()`. That is the right call — one map, one
id space. A composite `(kind, id)` key would also work but is strictly worse:
it keeps two counters alive and makes the wire id non-unique, so a future
per-mount or retry path could still alias.

**Residual amendment:** R5 says "do not add a second close counter" but leaves
the misleading doc-comment at `panes.rs:38-42` in place — it still advertises
"a separate counter ... so a split and a close can never collide" as the
governing rationale. That comment is what would talk the next contributor into
re-splitting the counter. Require R5 to **rewrite** it: the reason splits and
closes may keep separate counters is that they land in separate maps; every
occupant of one map must share one counter.

### 3b. NEW — R4 removes a structural invariant and does not replace it

**Severity: MAJOR. PROVEN gap in the revised plan. This is the part R5 does not cover.**

With three maps, "a `TabCloseResponse` can only ever act on a pending tab close"
was enforced **structurally** — by which map the handler looked in. With one
map it becomes a runtime property that nothing currently checks.

Concrete failure, and note it survives every guard the plan lists:

1. Client sends `ClosePaneRequest{request_id: 7}` for a pane on mount M.
   `pending_remote_closes[7] = { kind: Pane{pane_id}, workspace_id, origin: M }`.
2. Host M — buggy, mid-refactor, or hostile — replies
   `TabCloseResponse::Closed{request_id: 7}`.
3. Origin check passes: the response really did arrive on mount M, so
   `pending.origin == origin` (`creation.rs:1192-1204`). **Origin validation
   does not help here** — this is one mount confusing its own two request kinds,
   not cross-mount confusion.
4. A shared-counter id space guarantees id 7 is unique, so R5 does not help
   either.
5. The tab-response handler pops entry 7, which is a `Pane` — and unless it
   verifies the kind, tears down the wrong kind of object, or panics on the
   wrong enum arm.

A single shared counter makes ids unique **among requests we sent**; it does not
constrain what id a peer puts in a response. The request id is peer-echoed data
crossing a trust boundary, so the kind tag must be validated on arrival, exactly
as `origin` is.

**Amendment (must be added to R4/R5, and to the test matrix):** each response
handler pops the entry and asserts `PendingRemoteClose.kind` matches the
response variant. On mismatch: `tracing::warn!`, perform **no** teardown, and
re-insert or leave the entry intact so the legitimate response can still resolve
it (mirroring the "authorize before mutating" discipline at
`creation.rs:1811-1821`). Add **T15**: *a `TabCloseResponse` carrying the id of
a pending pane close tears nothing down.*

### 3c. Cross-mount collision — still refuted

The counters are process-wide statics, not per-mount, so two mounts never mint
the same id. Combined with the `origin` check this remains sound. R5's closing
sentence states this correctly.

---

## Finding 4 — races

### 4a. Host resync arrives before the ack — SAFE for panes, and the plan can inherit it

**PROVEN safe, conditionally.** `handle_federation_resync_workspace_removed`
(`creation.rs:1780`) calls `purge_federation_state_for_workspaces`
(`creation.rs:1826`), which is the grouped helper that purges
`pending_remote_closes` (`creation.rs:1652-1658`). So the resync purges the
pending entry first; the late ack then finds nothing
(`creation.rs:1210-1216`, `take_pending_remote_close` -> `None` -> `warn` ->
return). No double teardown, no missing-index panic.

Second guard behind it: even with a live pending entry,
`creation.rs:1217-1223` re-resolves via `find_pane` and returns on `None`,
explicitly documented as idempotent success.

**Amendment:** the plan (line 104-105) says to purge "everywhere the existing
`pending_remote_closes` is purged (mount-ended + local-close purge blocks)".
That enumerates call sites. Register the two new maps in the **grouped helper**
`purge_federation_state_for_workspaces` instead — all three call sites
(`creation.rs:1658`, `workspaces.rs:555`, `workspaces.rs:1046`) then inherit it,
and a future fourth call site cannot forget one map.

### 4b. Orphan ack after the mount ended — SAFE

**PROVEN.** `take_pending_remote_close` returns `Option`
(`creation.rs:920-923`); the handler's `let ... else` at `creation.rs:1210`
warns and returns. No `unwrap`/`expect` on that path. The template's guards do
cover this, as the brief asked.

### 4c. Pending **tab** closes are not purged when a resync removes the tab — NEW

**Severity: MAJOR. PROVEN gap in the plan's purge design.**

`purge_federation_state_for_workspaces` keys on **workspace** id. A host resync
that removes a *tab* (`handle_federation_resync_tab_removed`,
`creation.rs:1977`) does not remove the workspace, so no purge fires. A pending
tab-close entry therefore survives its own target and lingers until the mount
ends.

That is tolerable only if the ack handler re-resolves and no-ops. It is **not**
tolerable if the handler caches an index — see Finding 4d.

**Amendment:** add a `purge_pending_remote_tab_closes_for_tabs(&HashSet<String>)`
and call it from `handle_federation_resync_tab_removed` and from the local
tab-close path, mirroring the workspace-level helper. Add a T13 to the matrix:
*a resync-removed tab purges its pending close.*

### 4d. Index-vs-id re-resolution at ack time

**Severity: MAJOR if implemented naively. Hypothesis partially refuted; a real narrower hazard remains.**

I checked whether a cached index could tear down the wrong thing. Results:

- **Workspaces: safe if canonical ids are stored.** `public_workspace_id`
  returns the stable `ws.id` (`ids.rs:15-17`), and `parse_workspace_id`
  (`ids.rs:60-67`) resolves by `ws.id` equality *first*. Storing
  `public_workspace_id(ws_idx)` — exactly what the pane path does
  (`panes.rs:404`) — and re-resolving at ack time is correct.
- **Tabs: safe only for the canonical id form.** `public_tab_id`
  (`ids.rs:19-25`) builds `<ws.id>:t<encoded number>` from the stable
  `tab.number`, and `parse_tab_id`'s second branch resolves by
  `position(|tab| tab.number == tab_number)` (`ids.rs:81-87`) — stable under
  reordering. My "wrong tab" hypothesis is **refuted for that form**.
- **The live hazard:** `parse_tab_id`'s *first* branch (`ids.rs:69-77`) parses
  `t_<ws>_<idx>` **positionally**, and `parse_workspace_id`'s two fallbacks
  (`w_<n>`, bare `<n>`) are positional too. `handle_tab_close` receives
  `target.tab_id` as a **caller-supplied string** (`tabs.rs:247-250`) which may
  be any of these forms. If the dispatch stores `target.tab_id` verbatim rather
  than the canonical `public_tab_id(ws_idx, tab_idx)` it computes one line later
  (`tabs.rs:251`), then a tab removed at a lower index between send and ack
  shifts the target, and the ack tears down a **different, live tab** — killing
  unrelated processes.

**Amendment (make this an explicit invariant in P3, not an implementation
detail):** `PendingRemoteTabClose` stores the **canonical** ids only —
`public_workspace_id(ws_idx)` and `public_tab_id(ws_idx, tab_idx)` — never the
caller-supplied `target.tab_id`, and never a `usize` index. The ack handler
re-resolves through `parse_tab_id` and treats `None` as idempotent success
(the `find_pane`-`None` analogue at `creation.rs:1217`). Add T14: *a pending tab
close whose neighbour tab is removed first resolves to the original tab or to
nothing, never to another tab.*

---

## Finding 5 — origin validation is sound; it is host-scoped, not connection-scoped

**Severity: MINOR. Largely REFUTED — no bypass found.**

`origin` is **not** attacker-controlled: it is stamped from the receiving
mount's own connection context (`ctx.origin.clone()`, `client.rs:1013`,
`:1029`, `:1044`), never read from the wire message. A malicious host cannot
forge another host's `HostKey`, so it cannot satisfy another mount's pending
entry. The check itself (`creation.rs:1192-1204`) compares before taking, and
correctly returns without mutating on mismatch — the same "authorize before
mutating" discipline as `creation.rs:1811-1821`.

One structural caveat worth recording rather than fixing: `HostKey` identifies a
**host**, not a **connection**. Two concurrent mounts to the same host share an
origin, so origin alone would not fence them apart. Today the globally-unique id
counter closes that gap (Finding 3). The two mechanisms are load-bearing
*together*; neither is sufficient alone. Document this in the pending-map
doc-comment so a future move to per-mount counters does not silently remove the
only remaining fence.

---

## Finding 6 — channel choice and ack/event ordering

**Severity: MINOR (cap); MAJOR as an untested requirement (ordering). PARTLY UNDETERMINED.**

**Cap: fine.** `Channel::Control.max_len()` = 4 KiB
(`protocol/mod.rs:635`). The four new payloads are a `u64` plus a short id or
reason string. The only overflow risk is an unbounded `reason` on the `Failed`
arms — a host-side `reason` built from an arbitrary-length error message could
in principle exceed 4 KiB and be rejected by
`federation_accept.rs:147-152` / the client's equivalent, turning a "close
failed" into a mount teardown. **Amendment:** truncate `reason` at the
construction site (P2), as a bounded field.

**Ordering: both orderings are possible, and only one is currently guarded.**
Control frames and event frames share a single `SyncSender<FederationMessage>`
per connection feeding one writer thread, so the wire is strict FIFO — but
*enqueue* order is not fixed. The response is enqueued directly by the request
handler on the reader thread, while the host's own `WorkspaceClosed` event
travels the async `server_event_tx` -> event-pump path. Either can win.

The pane path handles both: ack-first works normally; event/resync-first purges
the pending entry (4a) so the ack no-ops. The plan does not state that both
orderings must be handled — it describes only the ack path (I2). Given P3 adds
two new pending maps and 4c shows the tab purge is missing, "event first" is
exactly the case that will be under-tested.

**Amendment:** split T7 into T7a (ack before the host's own event) and T7b
(host event/resync before the ack) for both workspace and tab, and assert no
double teardown and no stranded pending entry in each.

**Undetermined:** I did not empirically measure which ordering dominates in
practice — that needs the two-host live run the plan already records as owed.
I state it as possible-both rather than guessing a winner.

---

## Finding 7 — P3 is declared "disjoint from P2", which ships a client that silently does nothing

**Severity: MAJOR. PROVEN.**

Plan line 86: "P3 — client side (depends on P1; disjoint from P2)."

If a P3 client talks to a host without P2, the request lands in the reader
loop's catch-all (`federation_accept.rs:558-561`):

```rust
Ok(Some(_other)) => {
    // The controller drives only the terminal channel inbound;
    // other inbound frames are ignored, not treated as fatal.
}
```

Silently dropped. And there is **no timeout on any pending map** — I grepped
`creation.rs` for `timeout` and found none. Combined with I2's ack-gated
teardown, the user's close does nothing, forever, with no error and no retry.
The command returns `*_pending` ("the workspace will disappear once the remote
host confirms"), so the UI promises a confirmation that can never arrive.

This is strictly worse than today's behavior, where the close at least removes
the local mirror.

**Amendment:** either (a) make P3 depend on P2 and forbid shipping a client
ahead of a host — which, with Finding 1, means both ends must be rebuilt
together anyway; or (b) add a bounded timeout to the pending maps that surfaces
a user-visible failure and leaves local state untouched. (a) is simpler and
matches the plan's own single-branch delivery model; (b) is what you need if the
host can ever be older, which the capability-gate option in Finding 1 would also
require.

---

## Summary table

| # | Finding | Severity | Status |
|---|---|---|---|
| 1 | Two incompatible v6 dialects (branch head vs working tree) | CRITICAL | PROVEN mechanism, prospective trigger |
| 2 | Undecodable frame kills the mount, reported as `PeerClosed` | MAJOR | PROVEN |
| 3a | Two counters into one shared map | CRITICAL if unfixed | REAL; already mitigated by plan R5 |
| 3b | One map removes kind-routing; response kind unvalidated | MAJOR | PROVEN gap, R5 does NOT cover |
| 3c | Cross-mount id collision | — | REFUTED |
| 4a | Resync-before-ack (workspace) | — | PROVEN SAFE |
| 4b | Orphan ack after purge | — | PROVEN SAFE |
| 4c | Pending tab closes never purged on resync tab removal | MAJOR | PROVEN gap |
| 4d | Non-canonical tab id re-resolves positionally | MAJOR if implemented naively | Narrowed, real |
| 5 | Origin validation bypass | MINOR | REFUTED; caveat recorded |
| 6 | Control cap fine; both ack/event orderings possible | MINOR / MAJOR-untested | Partly undetermined |
| 7 | P3 without P2 = silent no-op, no timeout | MAJOR | PROVEN |

## Plan amendments, prioritized

1. **Bump `FEDERATION_PROTOCOL_VERSION` to 7** (Finding 1) — or capability-gate
   all four variants. Reverse the non-goal at plan line 14.
2. Make **P3 depend on P2** (Finding 7).
3. **Validate the popped entry's `kind` against the response variant** (Finding
   3b) — R4's one-map consolidation deleted the structural guarantee that used
   to make this free, and R5 does not restore it. Also rewrite the misleading
   counter comment at `panes.rs:38-42` (Finding 3a).
4. Store **canonical ids only** in the pending entries; re-resolve at ack;
   `None` = idempotent success (Finding 4d).
4. Register the new maps in the **grouped** purge helper, and add a
   **tab-level** purge (Findings 4a, 4c).
5. Distinguish decode failure from transport failure at teardown (Finding 2).
6. Bound the `Failed { reason }` string (Finding 6).
7. Test matrix additions: T7a/T7b (both orderings), T13 (resync-removed tab
   purges its pending close), T14 (neighbour-tab removal cannot redirect a
   pending tab close). Amend T2 if the bump is taken.

## Unresolved questions

1. Has any v6-A binary already been deployed to `appn-ltu-vm-100` /
   `appn-ltu-vm-105`? The running peers are from an **Aug 8** binary, predating
   the Aug 11 v6 bump, so today's answer is almost certainly no — but I could
   not verify the remote binaries' protocol version without SSH'ing to them,
   which is outside a read-only review. Confirm before the next deploy.
2. Which of ack vs host-event actually wins on a real link (Finding 6)? Needs
   the owed two-host run.
3. Plan question 2 (surface `Failed` vs idempotent success for a host-side
   not-found) interacts with Finding 4c/4d: the pane path already chose
   idempotent success for `pane_not_found:` (`client.rs:1035-1042`).
   Diverging for workspace/tab would be inconsistent; I'd recommend matching the
   pane precedent.

Status: DONE
Summary: The plan's load-bearing no-bump decision is correct about *releases* but
tests the wrong thing — the committed branch head and the working tree are two
mutually undecodable dialects both labelled v6, so once a branch binary is
deployed for the validation this plan already owes, a v6↔v6 link tears the whole
mount down on the first close and misreports it as `PeerClosed`. Six further
findings follow (P3-without-P2 silently no-ops with no timeout; pending tab
closes are never purged on resync tab removal), while the request-id-collision
and origin-forgery attacks are refuted with evidence.
