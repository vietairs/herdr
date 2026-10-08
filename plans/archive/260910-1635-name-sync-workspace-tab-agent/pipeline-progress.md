# pipeline-progress

- [x] 1. /ak-worktree create — done 16:44 — worktree ~/Projects/worktrees/herdr-name-sync, branch fix/name-sync-workspace-tab-agent from origin/master c2f4166a — cost: 0 agents/00:20, tokens est. 300
- [ ] 2. /hvn:blindspot — FOLDED into the Design phase (proposals each scouted the real code); not run standalone
- [x] 3. /ak-brainstorm --html — done 18:35 — Policy C selected BY USER (arbiter superseded); reports/design-decision.md — cost: 3 agents/06:00, tokens est. 155000
- [x] 4. /ak-predict --files — done 21:20 — via workflow wf_143b113d-783
- [x] 5. /ak-plan --tdd --parallel — done 21:20 — via workflow wf_143b113d-783
- [x] 6. /ak-plan validate (+kongming) — done 21:20 — federation scoping was the blocking gate; FNC v1 accepted, non-blocking
- [x] 7. /hvn:impl-notes init — done 21:20 — via workflow wf_143b113d-783
- [x] 8. /ak-cook --auto --parallel — done 21:20 — via workflow wf_143b113d-783
- [ ] 9. /hvn:impl-notes review — pending
- [~] 10. /ak-code-review — round 1 done 21:28: 13 findings, 6 BLOCKING (1 blocker + 5 major); fix round wf_98fa8093-1a2 running
- [ ] 11. /hvn:ship-gate — pending
- [ ] 12. /ak-ship (open PR, no merge) — pending
- [ ] 13. /ak-review-pr --fix --reply — pending
- [ ] 14. /ak-docs (pre-merge docs) — pending

## RESUMED 2026-09-10 18:37 — Policy C

User selected Policy C. Decision recorded at reports/design-decision.md (arbiter superseded, never ran).
Line-number authority resolved: use the WORKTREE's numbers; pipeline.md's are from a 107-commit-older tree.

RUNNING: workflow wf_143b113d-783 — Scope (federation, BLOCKING + predict) -> Plan -> Harness
(characterization tests, must be green on unchanged code) -> Implement -> Verify -> Review.
Script: ~/.claude/projects/-Users-hvnguyen-Projects-herdr/061f5a31-ac5c-4c97-9d55-69a5783f28dd/workflows/scripts/herdr-name-sync-policy-c-wf_143b113d-783.js

Artifacts it owes, all under reports/: federation-scope.md, predict-260910-policy-c.md,
../plan.md, ../implementation-notes.md.

STILL TRUE: no source file has been modified yet; the worktree is clean apart from untracked plans/.
The run self-halts with Status BLOCKED if federation scoping reports a blocking issue.


## ROUND 1 REVIEW OUTCOME (21:28)

Implementation of Policy C landed: 57 files, ~2558 insertions, new src/workspace/naming.rs.
Build green, suite 3658/3659 (the 1 failure confirmed pre-existing on the unmodified base).
Characterization harness green before any code moved, as required for refactor-risk work.

BLOCKING (fix round wf_98fa8093-1a2 dispatched):
  F1 blocker  wire.rs:1075   removing custom_label breaks generation-1 endpoint clients; PROTOCOL_VERSION
                             23->24 does NOT gate that path (ENDPOINT_PROTOCOL_GENERATION stayed 1).
                             DECIDED: keep custom_label as deprecated serde-default; do NOT bump generation
                             (fork has deployed v0.9.0-hvn.1 clients on multiple hosts).
  F2 major    workspace.rs:542   mirrored tab label inherits to panes -> every pane in a mounted tab renders
                             the same remote label. Exactly the D3 misfire FNC-3 exists to prevent.
  F3 major    creation.rs:411    double derivation + lost rung-1 short-circuit on the per-frame per-client
                             path: a RENAMED workspace went from 0 cwd lookups to 2 per frame per client.
  F4 major    state.rs:2162      agent_name_author latches to User forever, permanently excluding a pane
                             from tab-rename inheritance.
  F5 major    3 new clippy errors -> just check / fork CI fail. Base c2f4166a measured at ZERO clippy
                             errors, so these are regressions, not inherited debt.
  F6 major    creation.rs:356    pane naming resolves ONLY in the TUI border helper. PaneInfo lost the
                             mirrored label entirely and gained no name_source, so `herdr agent list` and
                             the agent sidebar still show stale names after a tab rename. This means the
                             user's literal request is NOT yet delivered on the API path.

MEMORY CORRECTED: the "~3 pre-existing clippy errors" note was disproven empirically (base = 0 errors;
the v0.9.0 merge cleared the old 17-error debt). An implementer acting on the stale note skipped clippy
entirely, which is why the branch would have failed CI.

## ROUND 2 REVIEW OUTCOME (2026-09-10)

Fix round `wf_98fa8093-1a2` closed F1–F8 structurally (clippy clean, build green, suite green),
but re-review returned STILL_BLOCKING with 10 findings, 5 genuinely blocking. Two were regressions
introduced BY the fix round; three were half-closures of the original findings.

Orchestrator independently confirmed before dispatching round 3:
- `cargo fmt --check`: 30 violations across 14 files, all on lines this diff added. The round-2
  verifier ran clippy but never ran fmt, and `just check` is fmt + nextest — so the branch was CI-red
  while being reported green. Prompt defect on my side: I asked for clippy, not for the repo's gate.
- `client_shell.rs:65` populates `custom_label: state.custom_name.is_some()`. Base called
  `from_existing_pane(Some(ws_info.label), Some(tab_info.label), ..)`, so a mounted scope had
  `custom_name = Some(..)` and `custom_label: true`. Moving the string to `mirrored_name` flipped it
  to `false` — a silent label loss on deployed generation-1 clients, i.e. exactly the population F1
  exists to protect.
- `creation.rs:785` writes `terminal.mirrored_label = pane_info.label`. Base wrote `manual_label`
  from a field that held ONLY the remote's manual label; F6 widened `PaneInfo.label` to the resolved
  ladder, so a mounted pane now freezes the remote's agent identity at materialization and never
  follows a live agent change. Regression against base.

Round 3 dispatched as `wf_e6e598a6-009` covering H1–H8.

## ROUND 3 OUTCOME (2026-09-10)

`wf_e6e598a6-009` closed H1–H8. All gates green and independently re-run by the verifier:
`cargo fmt --check` exit 0; clippy `--all-targets --locked -D warnings` zero errors after a forced
full re-lint (`find src tests -name '*.rs' -exec touch {} +`, because the first run was suspiciously
fast); `cargo build --workspace` clean; `cargo nextest run` 3675 tests, 3674 passed, sole failure the
known pre-existing `live_handoff_keeps_unmanaged_agent_name_bound_to_saved_session`.
Both adversarial re-review lenses (cross-version wire, ladder + multiplicative perf) returned ZERO
findings. The workflow's `STILL_BLOCKING` status was a script artifact: the verify agent returned
prose wrapping its JSON, so `verify.fmt_clean` read as undefined. Round 4 tightened the prompts to
demand a bare JSON object.

H2 was proven rather than asserted: production `effective_presentation()` call sites now number
exactly what base had (creation.rs plus five in terminal/metadata.rs tests), so zero builds were added
to the per-frame path.

One genuine NEW regression, introduced by the H5 fix and caught by the verifier auditing its own
round's work:

  Cross-build federation mount silently drops a remote user's renamed pane label.
  `PaneInfo::name_source` is `#[serde(default)]` and `NameSource`'s `#[default]` is `Ordinal`, so a
  v0.9.0-hvn.1 peer — same `FEDERATION_PROTOCOL_VERSION` 7, handshake accepts it — sends the field
  absent, it deserializes to `Ordinal`, `namespace_pane` stamps `AgentIdentity`, and
  `label_worth_mirroring` returns `None`. Base mirrored it. This is the exact trap `naming.rs`'s own
  doc comment forbids: reading a real absence as a resolved fact.

DECISION (auto-adjudicated, --auto): close it with a federation CAPABILITY, not a version bump and
not a schema change. The codebase already uses capabilities for behavior that varies within one
protocol version (see `Capability::WORKSPACE_TAB_CLOSE` doc comment); negotiation is additive and an
older peer simply drops a name it does not recognize. Capability agreed -> keep the
`label_worth_mirroring` gate; not agreed -> mirror unconditionally, because that peer's
`pane_info.label` is override-only by construction. This keeps the fleet-compatibility precedent
already set for `custom_label` (deprecated serde-default field rather than an
`ENDPOINT_PROTOCOL_GENERATION` bump) — no forced upgrade of the deployed hosts.

Round 4 dispatched as `wf_a0ee044b-ce0` (I1 capability gate, I2 double allocation on the
agent-identity rung).

## ROUND 4 OUTCOME (2026-09-10) — code complete

`wf_a0ee044b-ce0` closed I1 (capability gate) and I2 (double allocation). Re-review returned ZERO
findings. `STILL_BLOCKING` was again a script artifact — the verify agent returned its JSON as a
string, so `verify.fmt_clean` read undefined. Round 5's prompts demand a bare JSON object.

Gates re-run by the ORCHESTRATOR directly, not taken from any agent report:
  branch fix/name-sync-workspace-tab-agent, HEAD c2f4166a, 78 uncommitted files
  cargo fmt --check                                     exit 0
  clippy --all-targets --locked -D warnings (forced re-lint)   zero lints
  cargo nextest run --test-threads=4        3676 run, 3675 passed, 2 skipped
  sole failure: live_handoff_keeps_unmanaged_agent_name_bound_to_saved_session (pre-existing on base)
  FEDERATION_PROTOCOL_VERSION still 7; PaneInfo.name_source shape unchanged (non-Option)

The round-4 verifier did the strongest verification of the run: rather than reading the new tests and
believing them, it MUTATED the gate in both directions — forcing `true ||` made
`a_peer_that_does_not_report_name_source_still_mirrors_its_renamed_pane_label` fail, forcing
`false &&` made `a_mounted_pane_follows_the_remote_live_agent_identity_after_it_changes` fail — proving
each test actually discriminates, then restored the file and reconfirmed fmt and the four tests. That
is the standard the earlier rounds' "confirmed" claims lacked.

CARRYOVER FOUND, now being fixed: 61 ephemeral review/plan identifiers leaked into code comments
(F6 x13, F8 x10, FNC-3 x8, FNC-1 x5, F3 x5, F2 x4, plus F4/F7/G1/FNC-2/FNC-4/FNC-5), one of them
inside a test assertion string, plus "Phase 0"/"Phase 4"/"design-decision.md" prose. This violates
the standing rule against plan IDs, phase numbers, audit labels and finding codes in code artifacts —
`// F6 fix:` tells a future reader nothing. My own round-3 delegation prompt caused part of this by
saying existing FNC-N references "may stay"; the rule is absolute and I should not have carved out an
exception. Also fixing src/remote/federation/session.rs:25-26, whose doc comment claims its capability
set is identical to the one-shot mount dial — already false before this branch (FILE_STAGING,
WORKSPACE_TAB_CLOSE) and one entry more false now.

Cleanup dispatched as `wf_41b07fb3-49c`. After it lands: stages 9-14 (impl-notes review, code-review
closure, ship-gate, open PR, PR review + fix, docs).

## SHIPPED TO PR (2026-09-11)

Comment cleanup `wf_41b07fb3-49c` verified by an unusually strong method: the verifier wrote a Rust
comment-stripper (handling raw strings, byte strings, escapes, char-vs-lifetime, nested block
comments), stripped the pre- and post-cleanup trees, and diffed them pairwise across all 64 files.
Result: 2 files differ, 6 lines, every one an assert! trailing message argument — zero expression,
control-flow, signature, attribute or assertion-semantics changes. 0 ephemeral codes remain on added
lines. It also self-disclosed a scope gap (src/workspace/naming.rs is untracked so it appears in no
diff) and covered it by direct inspection instead of quietly ignoring it.

Full `just check` equivalent run by the orchestrator (`just` is not installed; each recipe run
directly):
  cargo fmt --check                                              exit 0
  cargo clippy --all-targets --locked -D warnings                zero lints (forced re-lint)
  cargo clippy --target x86_64-pc-windows-msvc -D warnings       zero lints
  cargo nextest run --no-fail-fast --test-threads=4              3676 run, 3675 passed, 2 skipped
  bun test ./scripts/docs                                        12 pass
Sole failure is the known pre-existing live_handoff test.

MEMORY CORRECTED (second stale-memory hit this run): `herdr-windows-typecheck-via-mingw` said the
windows-gnu target works locally and msvc does not. That is now exactly backwards — build.rs:16
panics `unsupported target for libghostty-vt build: x86_64-pc-windows-gnu`, while the msvc target
that `just windows-lint` actually uses completes clean in 33s. Had I trusted the memory I would have
shipped without any Windows lint at all, on a branch that adds `#[cfg_attr(not(unix), allow(dead_code))]`
capability constants.

Docs were already updated in-branch, including the ja and zh translations the release gate compares
by heading outline.

Commit b2a872f3 (amended: I had written a fabricated `refs #26` into the message with no issue behind
it, and removed it). Branch pushed. PR: https://github.com/vietairs/herdr/pull/26

REMAINING: watch CI, then stage 13 (`/ak-review-pr --fix --reply`). Cortex does NOT merge.
Post-merge, user-owned: `git pull --ff-only` on the base, remove the local worktree at
/Users/hvnguyen/Projects/worktrees/herdr-name-sync, then `/hvn:plan-gc archive`. Never delete the
remote branch.

## REBASED AND CI GREEN (2026-09-11)

The branch had gone stale during the review rounds: PRs #24 and #25 landed on master and GitHub
marked #26 CONFLICTING. Rebased c2f4166a -> 3fcce70f. Two conflicts:

1. `src/remote/federation/session.rs` `local_capabilities`. Master carried the old doc comment
   claiming the set is "identical to the one-shot attempt_federation_mount snapshot dial (P4)" (false
   even before this branch); this branch carried the corrected comment plus
   `#[cfg_attr(not(unix), allow(dead_code))]`. Kept the corrected comment, DROPPED the attribute:
   PR #25 made Windows clients mount federated workspaces, so this function is now live on Windows
   and the allow is obsolete. Verified empirically, not by argument — windows-msvc clippy passes with
   `-D warnings` and no dead-code lint, which only holds if the function is genuinely reachable there.
2. `docs/next/CHANGELOG.md` — kept both master's Windows Added/Fixed entries and this branch's Changed
   entries.

All gates re-run on the REBASED tree (a clean textual merge proves nothing semantically):
  fmt clean; unix clippy zero lints; windows-msvc clippy zero lints;
  3676 tests, 3675 passed (same known pre-existing failure); docs contract 12 pass.

GitHub checks, all 7 PASS: build, validate, conventional-commits, Windows ConPTY package,
check (ubuntu-latest), check (macos-latest), check (windows-latest).

NOTE ON MASTER'S OWN CI: master is currently RED (run 34459576931) on two jobs —
`check (windows-latest)` failing the single unrelated test
`sound::tests::windows_media_player_reports_invalid_media_without_waiting_for_timeout`, and
`conventional-commits` failing on the merge commit subject. That same Windows job PASSES on this
branch. This branch did not fix it; the sound test is timeout-sensitive and looks flaky. Recorded so
a future run does not read an inherited failure as a regression here, and does not credit this branch
with a fix it did not make.

Commits: 8407ba62 (feature), 6ad0715b (plan artifacts). Pre-merge review dispatched as
`wf_fa82ea87-3f4`, scoped to the genuinely new surface — the two conflict resolutions, the semantic
interaction with the new Windows federation mounting, and whether any earlier fix now rests on a
stale c2f4166a baseline.

## PIPELINE COMPLETE — PR #26 MERGE-READY (2026-09-11)

Pre-merge review `wf_fa82ea87-3f4` (2 lenses) found ONE real defect, and both lenses found it
independently: the new `PANE_NAME_SOURCE` constant carried a comment claiming "Federation only
negotiates capabilities on Unix; matches SCROLLBACK_REPLAY and WORKSPACE_TAB_CLOSE above" plus a
`not(unix)` dead-code allow — while rebased-in commit 97a03e68 had just DELETED that byte-identical
justification from those very constants, because the Windows mount work made it untrue. Fixed in
805c7af1. I had caught this same staleness class in session.rs during conflict resolution and missed
it here because protocol/mod.rs auto-merged without a conflict.

The workflow reported `CLEAN` with zero findings: my post-processing dropped them because both agents
returned their JSON as a string. Third occurrence of that script bug this run; the findings were only
recovered by reading journal.jsonl, exactly as the tool's diagnostics line instructs.

A TEST WAS WRITTEN AND THEN DELETED. Both reviewers noted no test covered the "agreed capability +
explicitly-named label" path. I added one; it passed; I then mutated the gate to
`peer_reports_name_source && false` and it PASSED AGAIN. That case is not behaviourally
distinguishable — both branches return `Some("deploy box")` — so no test of it can fail. Deleted
rather than banked as coverage. The two existing tests do pin both branches, each on an input where
they genuinely differ. Logged in implementation-notes.md.

FINAL STATE — commits 8407ba62, 6ad0715b, 805c7af1 on fix/name-sync-workspace-tab-agent:
  local: fmt clean; unix clippy zero; windows-msvc clippy zero; 3676 tests / 3675 pass; docs 12 pass
  GitHub: all 7 checks PASS; mergeable=MERGEABLE, CLEAN
  https://github.com/vietairs/herdr/pull/26

NOT DONE, and deliberately so: no live two-host mount test. The federation paths are covered by unit
tests plus a capability gate whose two branches were each proven to fail the suite when forced the
wrong way, but no real mount was exercised. See [[herdr-federation-live-validation-recipe]].

FOUND IN PASSING, NOT THIS BRANCH'S BUG: src/app/api/workspaces.rs:1127-1166, the workspace-close
path's federated-origin handling, is still entirely `#[cfg(unix)]`. Now that PR #25 lets Windows
clients mount, a Windows client closing a mounted workspace may take a different path than a Unix
one. Untested here and out of scope; looks like a real gap in the Windows federation work.

USER-OWNED NEXT STEPS (cortex does not merge): merge #26, then `git pull --ff-only` on the base,
remove the worktree at /Users/hvnguyen/Projects/worktrees/herdr-name-sync, then `/hvn:plan-gc
archive`. Never delete the remote branch.
