- [x] 1. /hvn:blindspot --deep — done 16:22 — reports/blindspot-260811-close-forwarding.md — cost: 1 agent/~7:00, tokens est. ~40k
- [x] 2. design gate + kongming advise (x2, converged) — done 16:29 — reports/auto-decisions-260811-close-forwarding.md — cost: 2 agents/~9:00, tokens est. ~35k
- [x] 3. plan authored (controller, full context) — done 16:31 — plan.md — cost: main-loop
- [x] 4. red-team (2 angles, parallel) — done 16:43 — reports/redteam-{destructive,protocol}.md — cost: 2 agents/~12:00
- [x] 5. P1 wire protocol — done 16:35 — reports/impl-p1-protocol.md — cost: 1 agent/~8:00, tokens est. ~30k
- [x] 6. P2 host side — done 16:55 — reports/impl-host-side.md — cost: 1 agent
- [x] 7. P3 client side — done ~17:55 — cost: 1 agent (no report written; agent handle lost across a context compaction — see below)
- [x] 8. P4 surfaces (CLI + TUI + docs) — done 18:28 — reports/impl-p4-surfaces.md — cost: 1 agent
- [x] 9. P5 verify (test + clippy + fmt) — done; 3452 passed / 0 failed, clippy clean, fmt clean
- [x] 10. code-review + security-scan (parallel) — done 18:42/19:0x — reports/{code-review,security-scan}-close-forwarding.md — all findings triaged, C1+H2+H3+MEDIUM fixed
- [x] 11. ship-gate + commit — done 2026-08-12 10:5x — `def4c90d` on `feat/federation-multi-tab-workspace`, pushed to `origin` (vietairs/herdr, new remote branch, first publish of all 5 commits). Not merged; remote branch kept. Commit-gate advisory (kongming, `--advise`): no blocker, one commit. Live two-host validation still owed and not possible on this machine.

## Stage-5 (P1) controller verification — NOT taken on self-report

Project history says agent self-reports on protocol/merge work are unreliable, so
P1 was verified independently:

- `git diff --stat`: exactly 3 files, +185/-1 (protocol/mod.rs +142,
  codec.rs +23/-1, client.rs +21).
- Version constants UNCHANGED: diff contains no `+`/`-` line for either
  constant; live values still `FEDERATION_PROTOCOL_VERSION = 6` (mod.rs:74,
  moved from :65 by added doc comments) and `PROTOCOL_VERSION = 20`
  (wire.rs:16). `src/protocol/wire.rs` has 0 lines in the diff.
- No `todo!`/`unimplemented!`/`panic!`/`unwrap(` in the added client.rs arms
  (grep over added lines only) — required, since a panic in the federation
  receive loop would kill a live mount.
- `cargo test --bin herdr remote::federation::protocol`: 27 passed, 0 failed.

Plan correction made during execution: a protocol-only phase could not build.
`client.rs:591` has an exhaustive match over `FederationMessage` with no
wildcard, so new variants are an E0004 error until it is extended. Precedent
commit `1af58792` (ClosePaneRequest/Response) touched both files together for
the same reason. P1's scope was widened by one file, limited to 4 non-panicking
stub arms; real response handling belongs to P3.

Housekeeping: the P1 report was written into the WORKTREE's own `plans/` dir
(a known trap here — `plans/` is NOT gitignored, so it would have been committed
with the feature). Moved to the main checkout and the stray dir removed;
worktree diff is back to the 3 intended source files.

## Stage 6+7 (P2 host / P3 client) controller verification

Same policy — verified against source, not against agent self-reports.

- **Echo rule (I1) HOLDS.** A repo-wide grep for `WorkspaceCloseRequest` /
  `TabCloseRequest` finds exactly two client-side construction sites:
  `src/app/api/workspaces.rs:1179` and `src/app/api/tabs.rs:483`. Both are
  under `src/app/api/`, so no host-originated path (resync handlers, host-ack
  handlers) can echo a close back to the peer that asked for it.
- **Version constants still unchanged**: `FEDERATION_PROTOCOL_VERSION = 6`,
  `PROTOCOL_VERSION = 20`. The whole change rides on capability gating
  (`Capability::WORKSPACE_TAB_CLOSE`), as designed.
- **Lease gating (I4) present** on both new actor arms
  (`federation_actor.rs`): each checks `lease.is_mounted_controller(epoch,
  connid)` and returns a refusal BEFORE touching the `App`. The pre-existing
  `SplitPane`/`ClosePane`/`CreateWorkspace` arms still lack this gate — left
  alone deliberately; it is a separate follow-up, not this change's scope.
- **Close-exactly-one (D2) verified by assertion, not by reading.** The host
  helpers `close_federation_target_workspace` / `close_federation_target_tab`
  (`app/creation.rs`) bypass the `workspace.close` / `tab.close` JSON-API
  methods entirely, because on the host the target is a LOCAL workspace: the
  federated branch would not apply, `close_selected_workspace()` would run,
  and `close_indices_for` would return the whole worktree group. One peer's
  single-workspace close would have destroyed N host workspaces.
- **Not-found is host-side `Failed`**, matching the in-tree `ClosePane`
  precedent (a missing pane makes `pane.close` return an error, which the
  actor maps to `Failed`). The idempotence decision applies to the CLIENT's
  ack handler — an ack for a target a racing resync already removed is
  treated as success, mirroring `creation.rs:1213-1218`.

### Fixes the controller made during verification

1. **Four host-side tests hardcoded `Mode::Navigate`** as the "unmutated" UI
   mode, but the test fixture starts in `Onboarding`, so all four failed.
   Replaced with a captured before/after comparison (`let mode_before =
   app.state.mode;`) — fixture-independent and a strictly stronger assertion.
2. **Group-integrity assertion added** (this was owed and missing). The two
   sibling-survives tests asserted only that the sibling still EXISTS. That is
   insufficient: the target is detached from its worktree group before it
   closes, so a bug that detached the wrong workspace would still leave two
   ids present. They now also assert the survivor retains its `worktree_space`
   membership. It passes — the worktree-group amplification path is genuinely
   closed.
3. **Regenerated `docs/next/api/herdr-api.schema.json`** (`HERDR_UPDATE_API_SCHEMA=1`),
   +34 lines for the two new methods.

### Correction to the plan's own reasoning

plan.md at one point argued the API schema artifact would stay byte-identical.
That held only for the rejected pure-semantics design. The chosen new-verb
design necessarily adds `workspace.close_remote` and `tab.close_remote` to the
schema, and the artifact was regenerated accordingly. The ARGUMENT for the fork
is unaffected: an explicit new verb is precisely why the change is visible in
the schema rather than silent.

### Verification results after P1-P3

- `cargo test --bin herdr -- --test-threads=1`: **3441 passed, 0 failed**.
- `cargo clippy --bin herdr`: no errors.
- `cargo fmt --check`: clean (after `cargo fmt`).

Do NOT trust `--test-threads=4` here. One parallel run produced 21 failures
whose set changed every run (integration `uninstall_*`, `pane::tests::
login_shell_builder_*`, several `server::headless` tests) — machine contention,
not regressions. The same tree is fully green serially.

### Process note

The P3 client agent's handle was lost across a context compaction: it kept
editing the worktree but was no longer addressable via ListAgents/SendMessage,
and it never wrote a report. It did converge on its own (verified by watching
the test suite go green). If this recurs, verify from the tree state rather
than waiting for a report that will never arrive.

## Stage 8 (P4 surfaces) + stages 10 (review, security) — controller verification

P4 landed CLI `close-remote` subcommands, a "Close on host" context-menu item
gated on a new `ContextMenuState.federated` flag, response surfacing via toast,
and docs under `docs/next/` only.

### Controller fixes on top of P4

- **DRY, then a bug it introduced.** Collapsed P4's duplicated toast-delivery
  match into a shared `raise_remote_close_toast(kind, title, reason)`. The
  first version made the shared helper `#[cfg(unix)]` while its caller was
  ungated — a Windows-only break invisible to a Mac build. Helper is now
  ungated, with a comment saying why it is the one non-Unix-only piece.
- Removed plan ids / audit labels from newly added code comments and test
  doc comments (project rule). Pre-existing occurrences left alone.

### Security scan outcome

- **MEDIUM, fixed.** `parse_workspace_id` (`ids.rs:65-66`) falls back to a
  POSITIONAL array index (`"1"`, `"w_1"`) as CLI shorthand. The host-side close
  handlers reused it for WIRE input, so a stale or hostile numeric id from a
  peer would close whatever workspace occupied that slot. Added
  `parse_federation_workspace_id` / `parse_federation_tab_id` (exact id only,
  canonical `<ws>:t<encoded>` tab form only), switched both host helpers, and
  added a refusal test for `"1"`, `"w_1"`, `"t_1_1"`, `"1:1"`.
  Not a privilege widening — the mounted controller is a whole-server lease and
  the pre-existing `ClosePane` path already reaches any local pane. It fixes
  determinism: a peer request now names something real or fails.
- **LOW, doc fixed not code.** The capability comment implied receive-side
  enforcement. Enforcement is send-side only and that is correct (the hazard is
  emitting an undecodable frame). Comment now says so, and says explicitly it is
  NOT the receive-gated arrangement `FILE_STAGING` uses — that misreading is
  what produced the finding.
- Clean: authorization, blast radius, UI-state manipulation, DoS/pending-map
  growth, response-side id confusion, input validation, secrets.

### Code review outcome

- **C1 CRITICAL, fixed. Windows build was broken.** `server::federation_actor`
  is compiled on every platform, but both host close helpers were `#[cfg(unix)]`
  with no twin -> E0599 on Windows. Added `#[cfg(not(unix))]` twins that refuse,
  matching `nudge_child_redraw`'s existing cfg pair. A static sweep for other
  unguarded calls into unix-gated `App` methods from always-compiled modules
  found no further crossings.
- **H2 HIGH, fixed.** `close_remote` did not end a mount whose LAST mirrored
  workspace it retired, so the link, drive task and `remote_mirrors` entry were
  stranded and a remount reported the host as already live. The local
  `workspace.close` verb does end it, so the paired verbs disagreed. Added
  `end_federation_mount_if_no_mirrors_remain`, called from the workspace ack,
  the tab ack's last-tab branch, and the already-gone branch. Also folded the
  missing clipboard-stage purge into `purge_federation_state_for_workspaces`,
  which fixes the same omission on the pre-existing resync path.
  The new test was verified to FAIL with the fix reverted.
- **H3, fixed.** The shared-counter test hardcoded ids 80/81 and never called
  the minting function, so the exact regression its comment described would
  have left it green. Rather than widen a module's visibility for a test, added
  `dispatching_a_remote_workspace_close_mints_from_the_shared_close_id_counter`
  in `api/workspaces.rs`, which observes the dispatcher advancing the real
  counter.
- **M4, M6, L7, L8 fixed.** M4: a comment claimed a fixed internal request id
  that does not exist (behavior is stronger than described — the path never goes
  through `handle_api_request`); comment corrected. M6: the non-Unix branch
  silently dropped error feedback and its comment understated which codes reach
  it; now raises on every platform.
- **M5 accepted, not fixed.** A tab dying via last-pane close is not covered by
  the pending purge. Harmless: tab numbers are monotonic and never reused, so a
  stale entry cannot match a later tab.
- Invariants 1-7 hold; 8 was violated and is now fixed.

### Final verification

- `cargo test --bin herdr -- --test-threads=1`: **3452 passed, 0 failed**.
- `cargo clippy --bin herdr`: clean (after `touch src/main.rs`).
- `cargo fmt --check`: clean.
- `FEDERATION_PROTOCOL_VERSION` 6, `PROTOCOL_VERSION` 20 — unchanged.
- Stable docs, README, CHANGELOG: untouched.
- 29 files, +3338/-107.

### Follow-ups NOT done here (deliberate)

1. `parse_pane_id` delegates its workspace half to the same positional-fallback
   `parse_workspace_id`, so the pre-existing `ClosePane` wire path has the
   identical weakness the MEDIUM finding fixed. Same one-line substitution would
   close it; out of scope for this change.
2. `is_mounted_controller` is still absent on the pre-existing `SplitPane` /
   `ClosePane` / `CreateWorkspace` actor arms.
3. No TTL on `pending_remote_closes`.
4. **Live two-host validation is still owed and cannot be done on this machine.**
   Host and client binaries should deploy together — it is the only tested
   configuration.

   CORRECTION (2026-08-12, commit-gate advisory): the original wording here
   claimed a client shipped against an old host "waits forever for an ack that
   host's read loop silently drops", and called that worse than the bug being
   fixed. The code contradicts that. The frame never leaves the client: an old
   host never advertises `workspace_tab_close`, so the send-side capability
   check in `send_workspace_close_request` / `send_tab_close_request`
   (`src/remote/federation/client.rs:184-210`) refuses locally with
   `CapabilityNotAgreed`, and the M6 fix surfaces that refusal on every
   platform. Mixed-version deployment therefore degrades to "close on host does
   nothing, with visible feedback" — annoying, not a hang. Deploy-together
   remains the recommendation, not a correctness requirement.

   Residual uncertainty the advisory did not close: it verified the capability
   ADVERTISE sites (`federation_accept.rs:104`, `session.rs:166`) but did not
   trace an OLD client's handshake intersection against a NEW host's extra
   capability string. If that intersection is not symmetric, deploy-together
   moves back from "recommended" to "required". Worth confirming during the
   owed live two-host pass.

## Stage 12-14 (2026-08-12) — deploy, live two-host validation, PR

- [x] 12. deploy `def4c90d` to appn-ltu-vm-100 + vm-105 (+ mac worktree build) — done 11:25 — backups `herdr.bak-260812`; live `default` servers NOT restarted (they hold real panes)
- [x] 13. live two-host validation (mac client <-> vm-105 host, `--session fedtest`) — done 11:30 — reports/live-validation-260812-two-host.md — **PASS**
- [x] 14. PR opened — https://github.com/vietairs/herdr/pull/13 (5 commits, base `master` on the FORK)

### Validation outcome

T1 workspace close: closes EXACTLY the named workspace on both client and host;
siblings survive both sides. The "close one, lose all" regression class is closed.
T2 non-last tab close: exactly the named tab, both sides.
Mirrored `tab_count` 3/1/1 — tab identity survives resync.

### The one claim this validation OVERTURNED

The earlier note (and the commit-gate advisory) said deploy-together is "the
recommendation, not a correctness requirement", reasoning from the send-side
`Capability::WORKSPACE_TAB_CLOSE` gate.

`FEDERATION_PROTOCOL_VERSION` is **5 on master, 6 on this branch** — bumped by the
earlier multi-tab phases, not by `def4c90d`. The handshake hard-rejects a version
mismatch BEFORE capability intersection, so the capability gate is never the
operative mechanism across a version boundary. Verified live in both directions
(old client -> new host, new client -> old host): both refuse the mount with an
explicit message.

**Deploy-together is REQUIRED for this branch.** Safe failure (refused mount with
feedback, not a hang), but not optional. The advisory's residual question about
capability-string intersection is moot — the version guard fires first.

### Operational note

Both VMs now carry the NEW binary on disk while their `default` servers still run
the OLD image. Real federation between those sessions will hit the same version
refusal until those servers restart, which kills their live panes — user's call,
not done here.
