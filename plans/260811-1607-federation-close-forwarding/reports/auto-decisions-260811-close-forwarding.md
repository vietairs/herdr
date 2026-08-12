# Auto-decisions — federation close forwarding

RISK: HIGH — ran unattended (`--auto --advise`).

Run: 2026-08-11 16:07 AEST. Plan dir
`plans/260811-1607-federation-close-forwarding/`. Worktree
`.claude/worktrees/federation-multi-tab-workspace`, branch
`feat/federation-multi-tab-workspace` (REUSED).

---

## D1 — Design gate (auto-adjudicated; brainstorm approval gate)

Advisory counsel ran first per `--advise` (kongming, `--auto` substitution for
`advisor`). Its verdict and Assumptions block are recorded below BEFORE the
adjudication, per contract.

### Counsel verdict (summary)

Option A (paired `WorkspaceCloseRequest/Response` + `TabCloseRequest/Response`),
CONFIRMED (ack-gated) teardown mirroring the pane precedent, riding the already
-bumped v6 with no further version change. Option C rejected. The change must
simultaneously add a neutral `workspace.unmount` verb, because `workspace.close`
is currently the unmount gesture.

### Counsel Assumptions (verbatim)

1. (high) Product intent is that closing a mirrored workspace/tab should delete
   on the host — taken from your framing ("state silently diverges... must close
   that gap"). If close-means-unmount was deliberate UX, the fix is labeling, not
   forwarding; user feedback would flip this.
2. (medium) No deployed binary already speaks v6 with the old variant set; if a
   v6 preview is live on any host, deploy host-side first so it never receives
   unknown variants from a newer client before it can decode them.
3. (medium) `handle_tab_close`'s non-last-tab branch (tabs.rs:340-362) is safe to
   reuse verbatim for federation-originated closes; I did not audit
   `remove_unattached_terminal_ids`/session-save side effects under a federation
   caller.
4. (low) `Channel::Control` is correct for the new pairs (matches
   `ClosePaneRequest` at protocol/mod.rs:648); I did not verify whether
   control-channel ordering vs resync matters for close races.
5. (medium) No teardown trigger beyond mount-ended, the two close handlers, and
   link drop; if session-restore/handoff tears down mirrors via the COMMAND
   handlers rather than the primitives, it would start forwarding deletes and
   must be repointed at the primitives.

### Decision

- What: Option A wire shape; confirmed teardown; add a `workspace.unmount` API
  verb; forwarding confined to the two top-level user-close handlers.
- Why: A's variant cost is zero this release (source v6 > released v5, so the
  bump is already paid). B is refuted because the three close semantics already
  diverge (tab close carries `closes_workspace` + `confirmation_required`; pane
  close carries the `federation-close-pane` trust boundary; workspace close
  carries mount lifecycle), so one generic response type would have to encode
  three failure vocabularies — and "subsume ClosePane later" costs a SECOND
  breaking change. C is disproven, not merely disfavored: VERIFIED at
  `panes.rs:1871-1883`, a federation-originated close of a worktree-group
  workspace's last pane is refused with `confirmation_required`, so the cascade
  deterministically stops one pane short, leaving a half-gutted host workspace
  and a fully-removed local mirror.
- Risk: forwarding is DESTRUCTIVE — it kills live processes on the host.
  Mitigated by (a) the unmount/delete verb split, (b) ack-gated teardown, (c) the
  layering rule below.
- Alternatives rejected: B (generic ContainerClose), C (pane-walk cascade).
- Reversibility: LOW for shipped protocol variants (removing one is another
  breaking change); HIGH for the client-side layering, which is ordinary code.

### Scope addition flagged to the user

`workspace.unmount` is a NEW JSON-API method the user did not request. It is
included because the requested forward, shipped alone, would convert the user's
only non-destructive exit from a mirror into a remote delete of live workspaces.
Stated plainly in the user-facing response rather than absorbed silently.

---

## D2 — Controller correction to a stage-1 security finding

The blindspot scan claimed "any connected federation peer... can already close a
pane". VERIFIED as OVERSTATED: `handle_connection` returns early unless
`acquire_controller` == `Admission::Accepted` (federation_accept.rs:297-303) and
holds the slot via `LeaseReleaseGuard` (:308), so any connection reaching the
command loop IS the admitted single controller.

VERIFIED residual gap, broader than reported: the per-arm
`is_mounted_controller` check is absent on THREE layout-mutating arms —
`SplitPane` (federation_actor.rs:412), `ClosePane` (:460), `CreateWorkspace`
(:499) — where it IS present on SendInput/Resize/NudgeRedraw/
ReleaseTerminalSize. Real effect is narrow but real: the per-arm check also
fences STALE-EPOCH commands from a superseded controller.

Decision: gate the NEW close commands on `is_mounted_controller` from the start
(cheap, correct). Counsel advised filing the three pre-existing arms as
follow-up rather than fixing here; controller ACCEPTS that for the two
pre-existing arms unrelated to this change, and notes the divergence: counsel
argued authz scoping generally is out of scope, which the controller agrees with
for per-workspace EXPOSURE-SET scoping (no such concept exists in the code — it
would be inventing a multi-tenant model speculatively), but the stale-epoch
fence is a different and much cheaper thing, so the new commands get it.

---

## D3 — Echo-hazard layering rule (auto-adopted, PROVEN)

`close_single_workspace_at` (creation.rs:1640) has five callers; three are
host-originated and must never forward:

| Caller | Origin | Forward? |
|---|---|---|
| creation.rs:1238 `handle_federation_close_pane_ready` | host ack | NO |
| creation.rs:1831 `handle_federation_resync_workspace_removed` | host event | NO |
| creation.rs:2155 resync tab-removed path | host event | NO |
| tabs.rs:289 user closes last federated tab | user | YES |
| workspaces.rs:1068 user closes workspace | user | YES |

RULE (mechanically checkable in review): the wire-send lives ONLY in the
`IdClass::Remote` branch of `handle_workspace_close` / `handle_tab_close`. No
federation-send code may appear in `close_single_workspace_at`,
`Workspace::close_tab`, `handle_federation_mount_ended`, any `handle_federation_
resync_*`, or any purge helper.

---

## D4 — Deferred to the human

- Live cross-host validation is NOT performed by this run (needs two machines and
  a real mount). Recorded as owed, per the standing pattern in prior federation
  work where self-reported success without a live test proved unreliable.
- Merge remains the human's call; cortex stops at a merge-ready PR.

## 2026-08-12 — commit gate (resumed run, --auto --advise)

**Advisory (kongming, --advise substitution under --auto), logged before the adjudication:**
No blocker. Land as ONE commit (P1 could not build standalone — E0004 on the
exhaustive `client.rs` match; precedent `1af58792` layered the same way).
Owed live validation + deploy-together each earn one body line (bodies do not
feed release notes; subjects do). Protocol constants correctly left at 6/20 —
already ahead of the latest released tag, second bump would be wrong.
Follow-up #4's "waits forever" claim contradicted by the send-side capability
gate; record corrected.

**What:** Committed the 29-file close-forwarding change as `def4c90d` and pushed
`feat/federation-multi-tab-workspace` to `origin` (vietairs/herdr) as a new
remote branch — first publish, carrying all 5 branch commits.

**Why:** User resumed the run with an explicit instruction to continue the held
commit, plus `--auto`. Stage 11 was BLOCKED only on commit-message alignment.

**Risk:** The exact message proposed in the pre-compaction turn is unrecoverable.
The committed message is a RECONSTRUCTION from the verification record, reviewed
by the advisory gate — not the byte-identical text the user originally saw.

**Alternatives rejected:** Stop and re-ask for message alignment (user had
already said continue and passed `--auto`); split into per-layer commits
(produces non-building intermediate states).

**Reversibility:** High. Feature branch, not merged. `git commit --amend` +
force-push to the fork branch reverses the message; no upstream, no PR, no tag.

**Not done, deliberately:** no merge, no PR opened, no remote-branch deletion,
no server deploy, no release tag. Live two-host validation still owed.
