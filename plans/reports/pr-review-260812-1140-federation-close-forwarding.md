# Pre-merge review — PR #13 (feat/federation-multi-tab-workspace)

3 independent top-tier reviewers (correctness / security / compatibility) + CI.
Verdict: **NOT merge-ready.** CI red with branch-introduced failures, plus one
authorization gap and one design fork that need a decision.

## Corrections to our own record (both verified at source)

The pipeline record's protocol claims were scoped to `def4c90d` alone and are
wrong for the BRANCH:

| constant | last release | master | branch |
|---|---|---|---|
| `FEDERATION_PROTOCOL_VERSION` | 5 | 5 | **6** |
| `PROTOCOL_VERSION` | 19 | 19 | **20** |

Consequence already validated live: deploy-together is REQUIRED (see
`plans/260811-1607-federation-close-forwarding/reports/live-validation-260812-two-host.md`).
The PR body claims `PROTOCOL_VERSION` "stays 20" — must be corrected.

Generalizable trap: *"this commit bumps nothing"* is not *"this branch bumps
nothing"*. Diff against the merge base, not the top commit.

## Merge blocker: CI red, and it is OUR regression

master CI is green on recent commits, so none of this is inherited debt.

1. **`check (windows-latest)`** — `error: fields remote_resync_workspace_index and
   pending_remote_workspace_focus are never read`, under `clippy -D warnings`.
   Both fields are new on this branch (0 occurrences on master); every read site
   is in `creation.rs` under Unix gating, so on Windows they are write-only.
   Same class as the C1 Windows break already fixed once in this PR.
2. **`check (ubuntu-latest)`** — `tests/cli/agent_transport.rs` fails with
   `protocol_mismatch: client protocol 20 is newer than server protocol 19`.
3. **`check (macos-latest)`** — `tests/client_mode.rs` x2, incl.
   `"server should report current protocol version"`.

(2) and (3) are fallout of the `PROTOCOL_VERSION` 19→20 bump: hardcoded protocol
expectations/fixtures in `tests/` were not updated. CLAUDE.md requires exactly
that on a wire bump.

**Why local validation could not see this.** Every local run was
`cargo test --bin herdr` — binary unit tests only. The failures are all in
`tests/` integration targets, which that command never compiles. "3452 passed"
was true and irrelevant to these three jobs. Use `just check` (nextest, all
targets) before claiming a branch is green.

## Findings, consolidated (deduped across reviewers)

### Agreed by 2+ reviewers

**A. `FederationCommand::CreateWorkspace` has no controller lease gate — and it is
NEW code.** `federation_actor.rs:207`, `:566`; accept handler
`federation_accept.rs:816` never threads `(epoch, connid)`. Verified: master's
`FederationCommand` ends at `ClosePane`; `CreateWorkspace` was added by
`870a4bfd` on this branch. The two sibling close arms added beside it DO gate.
Worse, the comment at `federation_actor.rs:170` excuses it as one of the
"pre-existing" ungated arms — it is not.
Impact: a displaced/superseded peer (lease revoked via `begin_revocation`, whose
reader thread is not torn down) can still loop workspace creates on the host,
each spawning a PTY, uncapped. Fix: thread `(epoch, connid)`, gate on
`is_mounted_controller`, correct the comment.

*Note: this defeated an exclusion I gave the reviewers ("CreateWorkspace is
pre-existing, don't report it"). Both reviewers checked master and pushed back
with evidence. The exclusion was mine and it was wrong.*

**B. `end_federation_mount_if_no_mirrors_remain` applied at 3 of 5 removal sites.**
Missing at `creation.rs:2348` (`handle_federation_resync_workspace_removed`) and
`creation.rs:2682` (`handle_federation_resync_pane_removed`'s close branch); also
the tab already-gone branch at `creation.rs:1530`. The three sites that DO settle
are the client-initiated ones; the two that don't are the **normal remote-initiated
flow** (the other user closes it). Result: link + drive task + `remote_mirrors`
entry survive with nothing visible, and remount is refused `AlreadyMounted` until
a server restart. This PR is what introduced the invariant, so this is a
half-applied new rule, not inherited.

### Single-reviewer, verified

**C. `workspace.create` picks a MACHINE from TUI cursor state** —
`api/workspaces.rs:630`. With no `--cwd`, `workspace_creation_source()` reads
`state.mode`/`state.selected` and, if the cursor sits on a mirrored workspace,
creates the workspace **on the remote host** and returns
`workspace_create_requested` instead of `workspace_created`. A script running
`herdr workspace create --label build` gets a different machine and a different
response shape depending on where the human's sidebar cursor happens to be, with
no opt-out. Also a CLAUDE.md runtime/client-boundary violation (TUI presentation
state deciding a shared runtime fact). Undocumented.

**C2.** A serving host re-enters that same branch (`cwd: None`) and can chain the
create to a *third* host; the response mapper at `federation_actor.rs:584` then
treats the non-`WorkspaceCreated` success as `workspace_create_failed` — the
workspace IS created, on a machine the caller never named, and reported as failed.

**D. Both new `close_remote` verbs return success as an ERROR envelope** —
`encode_error(id, "remote_close_pending", ...)`. It is the ONLY accepted outcome,
so `herdr workspace close-remote <id>` prints to stderr and **exits 1 on every
successful call**. This PR already models the correct shape elsewhere:
`ResponseResult::WorkspaceCreateRequested` is a success variant with identical
"accepted, not completed" semantics. New verb, contract still free to fix.

**E. Raw-index panic reachable from "Close on host"** — `navigate.rs:462` →
`ids.rs:15` indexes `workspaces[ws_idx]` (not `get()`) with a menu-open snapshot
index, while remote resync can now shrink the list asynchronously and AppEvent
dispatch is not gated on `Mode::ContextMenu`. Panic kills the server and every
pane. Shape is pre-existing, but remote-driven shrinkage is new with multi-workspace
federation. Secondary: an in-bounds shifted index closes a *different workspace on
the remote machine*.

**F. Minor** — pending entry consumed before target-kind check
(`creation.rs:1396`, `:1513`) lets a nonconforming peer evict an unrelated
in-flight close; no user feedback when `public_tab_id` is `None`
(`navigate.rs:521`); merged/misplaced doc paragraph at `creation.rs:1990`;
`WorkspaceCreateRequest` bypasses the "one gated send point" discipline
(`workspaces.rs:741`); `close_single_workspace_at` writes `state.selected` before
validating the index (`creation.rs:1971`).

## Confirmed clean (verified, not assumed)

Close-exactly-one blast radius; strict id parsing on the new verbs (no positional
fallback); no new production panics/unwraps in the federation receive loop;
capability advertise/agree on both ends; request-id correlation with origin
fencing; generated API schema in sync with source; serde additive-only, no
discriminant shift; persisted-state downgrade path safe; no integration-version
markers touched; docs confined to `docs/next/`; the two close verbs correctly
lease-gated and denied to view-only federated sessions.

## Live validation stands

The two-host pass (workspace close and tab close each closing exactly the named
object on both sides) is unaffected by any finding above — none of them touch the
close-forwarding path that was exercised.

## Unresolved questions

1. **C — is the remote-create redirect meant to apply to API/CLI callers at all,
   or only TUI actions?** The guard is `params.cwd.is_none()`, which catches
   scripts; the comment reads TUI-first. Materially changes the fix.
2. **D — change the contract now** (success variant, schema regen, docs) or keep
   the error envelope for symmetry with `pane.close`?
3. Is bidirectional/chained mounting (A mounts B while B mounts C) supported?
   Severity of C2 and the host-side settle gap depends on it.
4. Was the `PROTOCOL_VERSION` 19→20 bump deliberate? It is rule-permitted, but the
   client/server change is purely additive and `d5756da2` set the opposite
   precedent. Keeping it forces every installed CLI to restart its server.
