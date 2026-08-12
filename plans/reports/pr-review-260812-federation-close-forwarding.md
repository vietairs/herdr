# Pre-merge review — PR #13 (feat/federation-multi-tab-workspace)

3 independent top-tier reviewers (correctness / security / compatibility) + CI.
Original verdict: **NOT merge-ready.** All findings below are now fixed; see
"Resolution" per item.

## Corrections to our own record (both verified at source)

The pipeline record's protocol claims were scoped to `def4c90d` alone and are
wrong for the BRANCH:

| constant | last release | master | branch |
|---|---|---|---|
| `FEDERATION_PROTOCOL_VERSION` | 5 | 5 | **6** |
| `PROTOCOL_VERSION` | 19 | 19 | **20** |

Consequence, validated live: deploy-together is REQUIRED. See
`plans/260811-1607-federation-close-forwarding/reports/live-validation-260812-two-host.md`.

Generalizable trap: *"this commit bumps nothing"* is not *"this branch bumps
nothing"*. Diff against the merge base, not the top commit.

## CI, and it was OUR regression

master CI is green on recent commits, so none of this was inherited debt.

1. **`check (windows-latest)`** — dead-code error on `remote_resync_workspace_index`
   and `pending_remote_workspace_focus`, whose read sites are all Unix-gated.
   **Resolution:** fields made consistently `cfg(unix)`.
   A second, distinct Windows break appeared after the first fix round:
   `raise_remote_close_failed_toast` was gated `cfg(unix)` while both callers sit
   on the platform-neutral TUI close path (E0599 x2). **Resolution:** ungated,
   matching its sibling `raise_remote_close_toast`, which documents exactly this.
2. **`check (ubuntu-latest)`** — `tests/cli/agent_transport.rs`:
   `protocol_mismatch: client protocol 20 is newer than server protocol 19`.
3. **`check (macos-latest)`** — `tests/client_mode.rs` x2.

(2) and (3) were fallout of the `PROTOCOL_VERSION` 19->20 bump: hardcoded
expectations in `tests/` were not updated, which CLAUDE.md requires on a wire
bump. **Resolution:** `tests/support::CURRENT_PROTOCOL` -> 20, six hardcoded
literals de-hardcoded in `tests/cli/sessions.rs`.

**Why local validation could not see this.** Every local run was
`cargo test --bin herdr` — binary unit tests only, which never compiles the
`tests/` integration targets. "3452 passed" was true and irrelevant. Worse,
`tests/cli` is `#![cfg(not(target_os = "macos"))]`, so it runs ZERO tests on this
Mac; it was validated by shipping the tree to a Linux VM (99 passed, 0 failed).

## Findings, consolidated

### A. `FederationCommand::CreateWorkspace` had no controller lease gate — NEW code

`federation_actor.rs`. Verified: master's `FederationCommand` ends at
`ClosePane`; `CreateWorkspace` was added by `870a4bfd` on this branch. The two
sibling close arms added beside it DO gate. Worse, a comment excused it as one of
the "pre-existing" ungated arms — it is not.

Impact: a displaced/superseded peer (lease revoked, reader thread not torn down)
could loop workspace creates on the host, each spawning a PTY, uncapped.

**Resolution:** `(epoch, connid)` threaded through, gated on
`is_mounted_controller`, comment corrected to name only `SplitPane`/`ClosePane`,
refusal test added.

> *This finding defeated an exclusion I gave the reviewers ("CreateWorkspace is
> pre-existing, don't report it"). Both reviewers checked master and pushed back
> with evidence. The exclusion was mine and it was wrong. Do not hand reviewers
> exclusions you have not verified at source.*

### B. `end_federation_mount_if_no_mirrors_remain` applied at 3 of 5 removal sites

Missing at `handle_federation_resync_workspace_removed`, at
`handle_federation_resync_pane_removed`'s close branch, and at the tab
already-gone branch. The three sites that DID settle are the client-initiated
ones; the two that did not are the **normal remote-initiated flow** (the other
user closes it). Result: link + drive task + `remote_mirrors` entry survive with
nothing visible, and remount is refused `AlreadyMounted` until a server restart.
This PR introduced the invariant, so it was a half-applied new rule.

**Resolution:** applied at every removal site (7 call sites).

### C. `workspace.create` picks a MACHINE from TUI cursor state

With no `--cwd`, `workspace_creation_source()` reads `state.mode`/`state.selected`
and, if the cursor sits on a mirrored workspace, creates the workspace **on the
remote host**, returning `workspace_create_requested` instead of
`workspace_created`. A script running `herdr workspace create --label build` gets
a different machine depending on where the human's sidebar cursor happens to be.

**Resolution (user decision):** intended for all callers; logic unchanged,
documented in `cli-reference.mdx` and `socket-api.mdx`.

### C2. Chained create misreported as failed

A serving host re-enters that branch (`cwd: None`) and can chain the create to a
third host; the response mapper then treated the non-`WorkspaceCreated` success
as `workspace_create_failed` — the workspace IS created and reported as failed.

**Resolution:** exhaustive match; a redirect now reports
`workspace_create_redirected`, a distinct code. Open design question below.

### D. Both new `close_remote` verbs returned success as an ERROR envelope

`encode_error(id, "remote_close_pending", ...)` was the ONLY accepted outcome, so
`herdr workspace close-remote <id>` printed to stderr and **exited 1 on every
successful call**. The PR already modelled the correct shape elsewhere
(`ResponseResult::WorkspaceCreateRequested`).

**Resolution (user decision: fix now):** new `WorkspaceCloseRequested` /
`TabCloseRequested` success variants, schema regenerated, three tests re-pointed.
Live-verified: `EXIT=0`.

`pane.close_remote` deliberately keeps the old error envelope — it shipped on
master, so the released contract is left alone despite the inconsistency.

### E. Raw-index panic reachable from "Close on host"

`navigate.rs` -> `ids.rs` indexed `workspaces[ws_idx]` (not `get()`) with a
menu-open snapshot index, while remote resync can now shrink the list
asynchronously. Panic kills the server and every pane. Secondary: an in-bounds
shifted index closes a *different workspace on the remote machine*.

**Resolution:** `public_workspace_id_checked` + toast; the context menu snapshots
stable ids (`remote_close_target`) instead of a raw index and refuses when the
target is gone; two regression tests assert a shifted list refuses rather than
closing the neighbour.

### F. Minor

Pending entry consumed before target-kind check; no user feedback when
`public_tab_id` is `None`; merged/misplaced doc paragraph;
`WorkspaceCreateRequest` bypassing the "one gated send point" discipline;
`close_single_workspace_at` writing `state.selected` before validating the index.

## Confirmed clean (verified, not assumed)

Close-exactly-one blast radius; strict id parsing on the new verbs (no positional
fallback); no new production panics/unwraps in the federation receive loop;
capability advertise/agree on both ends; request-id correlation with origin
fencing; generated API schema in sync with source; serde additive-only, no
discriminant shift; persisted-state downgrade path safe; no integration-version
markers touched; docs confined to `docs/next/`; the two close verbs correctly
lease-gated and denied to view-only federated sessions.

## Known flake, not from this branch

`api::server::pane_graphics_stream::tests::inactive_owner_cancels_idle_stream_and_dispatches_close`
fails only under a fully serial full-suite run and passes 3/3 in isolation
(channel-recv `Timeout`, i.e. load-sensitive). Reproduced on a stashed tree, so
it predates this work. CI's nextest profile is parallel and does not hit it.

## Unresolved questions

1. Is bidirectional/chained mounting (A mounts B while B mounts C) supported?
   The severity of C2 and the host-side settle gap depends on it. Current
   recommendation: stop host B redirecting a peer's create at all.
2. Was the `PROTOCOL_VERSION` 19->20 bump deliberate? It is rule-permitted, but
   the client/server change is purely additive and `d5756da2` set the opposite
   precedent. Keeping it forces every installed CLI to restart its server.
3. The chained-create arm has no direct test — building one needs a nested live
   mount whose helper lives in a private test module.
