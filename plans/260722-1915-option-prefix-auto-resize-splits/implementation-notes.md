# Implementation notes — balance splits + auto-resize toggle

Append-only. 4-line entries: What / Why / Evidence / Reversibility.
Plan: `plan.md` (5 phases). Worktree: `/Users/hvnguyen/Projects/herdr-worktrees/pane-auto-resize-splits`
Branch: `feat/pane-auto-resize-splits`. Started 2026-07-22 19:52.

## Locked decisions (user-confirmed, do not relitigate)

- D1 ship BOTH one-shot action + persistent toggle this PR.
- D2 EQUAL AREAS via leaf-count weighting: `ratio = leaves(first)/(leaves(first)+leaves(second))`.
- D3 one-shot = whole tab root; auto-rebalance = ancestor chain of changed pane ONLY.
- D4 federation local-only v1; `src/remote/federation/**` untouched; gap documented.
- D5 toggle = global `[ui]` config bool; NO SNAPSHOT_VERSION bump.
- D6 balance no-ops while any pane in the tab is zoomed.
- D7 toggle label = full swap `"Auto-resize splits: On"` / `"Auto-resize splits: Off"`.
- D8 `Method::LayoutBalance` reuses `LayoutExportParams` (tab_id/pane_id shape).
- D9 federation gap documented as code comment at hook site + one line in `socket-api.mdx`.

## Entries

### 2026-07-22 19:52 — Ground-truth corrections found during planning
What: Four caller-supplied ground-truth claims were wrong; planner verified live and corrected.
Why: Acting on them would have produced a feature that passes tests but does nothing in-app.
Evidence: `AppState::split_pane`/`close_pane`/`resize_pane` are `#[cfg(test)]`-only stubs —
real chokepoints `handle_pane_split` (src/app/api/panes.rs:40-161), `handle_pane_close` (:1663-1733).
`.items()` has 11 call sites not 9. `pane_borders`/`pane_gaps` have no in-app toggle; real
toggle-and-persist precedent is `save_agent_border_labels()` (src/app/config_io.rs:82-93).
Free fn `set_ratio_at()` (src/layout.rs:580-599) does NOT clamp — only call sites do.
Reversibility: n/a (documentation of fact, not a change).

### 2026-07-22 19:42 — Unit A shipped separately, scope substituted
What: Prefix set to `ctrl+a`, not the originally requested `option+b`.
Why: Empirically verified Option+B emits U+222B (compose) in Ghostty 1.3.2 with
`macos-option-as-alt` unset — `option+b` can never reach herdr as a keybind. User declined the
global Ghostty change and chose `ctrl+a`.
Evidence: user keypress test returned `∫`; `herdr server reload-config` -> `status: applied`,
0 diagnostics; `~/.config/herdr/config.toml:66`.
Reversibility: trivial — single config line, prior value preserved as comment on line above,
backup at `config.toml.bak-260713`.

### 2026-07-22 20:05 — Controller patched 5 construction sites out-of-phase
What: Main loop (not a phase agent) added `auto_resize_splits: false` to the `AppState` literal
(src/app/mod.rs:680) and `auto_resize_enabled` to 4 `ContextMenuKind::Pane` literals
(mouse.rs:1101 prod = `self.auto_resize_splits`, mouse.rs:2848 test, modal.rs:2008/2073 test).
Why: herdr is a SINGLE-BINARY crate (no `[lib]` target) — the whole crate must compile before
ANY test runs. Phase 2's new required fields left 5 sites uncompilable, so phases 1-4 could not
execute tests at all until phase 5 landed. Plan's phase graph implicitly assumed per-phase test
isolation; that assumption is false for this crate. Patching now restores TDD verification for
every remaining phase instead of deferring all verification to the end.
Evidence: phase-02 agent reported `cargo check --tests` E0063 x5, all outside its owned file;
post-patch `ZIG=... cargo check --tests` -> `Finished dev profile`, 0 errors.
Reversibility: trivial — 5 single-line insertions, all mechanical. Phase 3 must replace the
`false` at mod.rs:680 with `config.ui.auto_resize_splits` once UiConfig gains the field.

### 2026-07-22 20:08 — Equal-area test fixtures must be power-of-two, not lopsided
What: Depth-4 area-equality test initially failed ([1440,1440,1440,1472,1408]); fixed by using a
power-of-two-leaf / power-of-two-Rect fixture for AREA-equality assertions, keeping the lopsided
"1 vs rest" chain only for leaf-weighting and clamp-degradation tests.
Why: NOT a bug in the balance math. A lopsided chain multiplies a single 1-cell integer rounding
error by a large remaining dimension, so exact area equality is unrepresentable in cell units
regardless of ratio precision. Distinct from the H1 0.1..0.9 clamp limit.
Evidence: phase-01 agent; 28/28 layout tests pass post-fix; documented in a code comment at the
fixture site.
Reversibility: test-fixture only, no production behavior implication.

### 2026-07-22 20:08 — 4 pre-existing full-suite failures under --test-threads=4
What: Full suite = 2975 passed / 4 failed. Failures attributed to socket-timeout and
random-id-collision contention under parallelism, NOT to layout.rs.
Why: matches the known local constraint that this machine lacks just/nextest and runs plain
`cargo test --test-threads=4`, where these tests contend.
Evidence: phase-01 agent report; no failure referenced layout.rs.
Reversibility: n/a. UNVERIFIED BY CONTROLLER — must be confirmed against a clean master baseline
at code-review/ship-gate before accepting as pre-existing.

### 2026-07-22 20:15 — Flaky-test baseline partially verified
What: Phase 3 confirmed its 2 observed full-suite failures (`app::api::plugins::tests::
manifest_action_invoke_injects_plugin_paths`, `workspace::tests::
generated_workspace_ids_are_short_base32_handles`) pass in isolation AND on a `git stash`
baseline => pre-existing, not caused by this branch.
Why: earlier phase-01 claim of "4 pre-existing failures" was unverified assertion. Note the
failing SET DIFFERS between runs (4 vs 2, different names) — consistent with parallel-run
contention, not a stable regression.
Evidence: phase-03 agent ran `git stash` baseline comparison + isolation runs.
Reversibility: n/a. Controller still to do a full clean-baseline reconciliation at ship-gate.

### 2026-07-22 20:35 — Zoom state must be captured BEFORE split, not after
What: Auto-rebalance zoom guard in `handle_pane_split` reads `target_was_zoomed` captured
pre-split, not `tab.zoomed` post-split.
Why: `split_focused_with_runtime` (src/workspace/tab.rs) unconditionally CLEARS `zoomed` as a
side effect of any split. A post-split check therefore always sees `false`, so the D6 zoom
no-op would never fire — it would compile, pass review, and silently do nothing.
Evidence: caught by phase-04 TDD (test failed before fix). Deviation from phase file's literal
"insert after :138" instruction.
Reversibility: local to the hook site; revert = move the capture back inline.

### 2026-07-22 20:35 — Phase 4 touched 3 files outside its owned list
What: `src/app/api.rs:1101` (dispatch registration, anticipated by phase file) plus one match
arm each in `src/api/server.rs:397` and `src/api/mod.rs:188`.
Why: single-binary crate + a new exhaustive `Method` variant => two other exhaustive matches
outside the owned list must be updated or the crate cannot compile and no test can run. Same
root cause as the 20:05 controller patch. Mechanical, mirrored existing `LayoutSetSplitRatio`
treatment, no design judgment.
Evidence: agent flagged rather than silently absorbing; `cargo check --tests` clean after.
Reversibility: 3 one-line additions.

### 2026-07-22 20:35 — PROTOCOL_VERSION bump NOT required
What: `src/protocol/wire.rs::PROTOCOL_VERSION` left at 17.
Why: repo rule says bump only if source is not already greater than the latest RELEASED tag.
`v0.7.5` already ships 17, and that constant governs the client<->server frame/handshake wire
protocol, which this change does not touch — distinct from the JSON-RPC `Method` enum.
Evidence: phase-04 agent verified against the released tag.
Reversibility: n/a (no change made).

### 2026-07-22 20:52 — SHIP BLOCKED: branch cannot be pushed to GitHub
What: `feat/pane-auto-resize-splits` (off master 5ec2a10b) cannot be pushed; no PR possible.
Why: the v0.7.5 upstream merge (5ec2a10b) vendored 3 prebuilt static libs into git history —
385MB + 193MB + 193MB — over GitHub's hard 100MB/file limit. Pre-existing, unrelated to this
feature. Last-pushed commit b5cb8ce8 is verified CLEAN of >50MB blobs; remote HEAD still sits
there.
Evidence: `git ls-tree -r -l b5cb8ce8` -> no blob >50MB; same on 5ec2a10b -> the 3 libs.
`git ls-remote origin` HEAD = b5cb8ce8.
Reversibility: n/a — this is a fork-history problem, not a change. Options: (a) land locally
only, (b) rebase feature onto clean b5cb8ce8, (c) strip blobs from history (filter-repo/LFS).
User decision required; OUT OF APPROVED SCOPE for this run.

### 2026-07-22 21:09 — H1 close-hook scope bug found in review, fixed with proof
What: Close hook replayed a PRE-close path against the POST-collapse tree, descending one level
too deep into the PROMOTED SIBLING and rewriting its ratio. Fixed by truncating the replayed
path by 2 at the call site (`&path[..path.len().saturating_sub(2)]`, src/app/api/panes.rs).
Why: `remove_pane` promotes the sibling into the removed parent's slot, and
`balance_split_ratios_along_path` (src/layout.rs:730-752) balances the node it is ON before
consuming the next path element. Silently destroyed a user's manual ratio in an unrelated
subtree — a direct violation of the ancestor-chain-only scope decision.
Evidence: repro `root H(0.7){P1, V(0.6){P2, H(0.25){P3,P4}}}`, toggle ON, close P2 -> 0.25
became 0.5. Bug PROVEN by reverting the fix (test failed with 0.5) then restoring (passed).
Existing close tests could not catch it: their fixture always gave the closing pane a LEAF
sibling, so over-descent stopped at `Node::Pane`. New fixture uses a SPLIT sibling.
Reversibility: one-line slice truncation at a single call site.

### 2026-07-22 21:09 — M5 process failure: plan labels reached production code
What: `(D3)`, `(D6)`, `(D8)`, `(D4/D9)` appeared in production comments across panes.rs and
layouts.rs despite EVERY phase prompt explicitly forbidding plan IDs in code comments. Stripped
from 5 comment blocks; explanations kept, labels dropped.
Why: repo stable-code-artifacts rule. Worth recording as a process lesson — an explicit
instruction in every prompt was NOT sufficient; only code review caught it.
Evidence: code-review finding M5; grep of changed files.
Reversibility: comment-only.

### 2026-07-22 21:09 — M3 dispatcher drift closed by gating, not duplicating
What: free fn `apply_context_menu_action` gated `#[cfg(test)]` after verifying all its callers
are test-only; `_via_api` is the sole production path.
Why: the two dispatchers had already drifted (free fn did not persist the toggle, emitted no
LayoutUpdated). Gating removes the trap without duplicating persistence logic a second time.
Evidence: caller audit by fix agent; code-review finding M3.
Reversibility: remove the cfg attribute.

### 2026-07-22 21:09 — M1/M4 accepted, NOT fixed (deliberate)
What: M1 auto-rebalance overrides an explicit `pane.split {ratio}`; M4 toggle is not exposed
over the JSON-RPC API.
Why: M1 is acceptable for an OPT-IN toggle the user deliberately enabled. M4 is locked decision
D5 (config-only setting, no snapshot/protocol surface). Neither is a defect against the agreed
contract; both are recorded here rather than silently ignored.
Reversibility: n/a — no change made. Revisit if the toggle ever becomes API-controllable.

### 2026-07-22 22:45 — Fable review found the H1 fix was itself incomplete at depth 1
What: `saturating_sub(2)` in the close hook replaced with `checked_sub(2)` + skip
(src/app/api/panes.rs). New test `pane_close_auto_rebalance_preserves_promoted_sibling_ratio_at_root_depth`.
Why: closing a DIRECT CHILD OF THE ROOT gives a path of length 1; `saturating_sub(2)` clamps to
an EMPTY path, and an empty path is NOT a no-op — `balance_split_ratios_along_path` balances the
node it stands on, which post-collapse is the promoted sibling. So the original H1 fix still
destroyed a manual ratio, just one level shallower. At L<2 the parent WAS the root, zero
ancestors survive, nothing may be balanced. Every prior close-hook test used a length-2 path,
where the empty prefix is coincidentally correct — the whole test class missed L=1.
Evidence: found independently by two Fable review lenses, one reproducing it empirically.
Controller re-proved it: new test fails with `0.5` on the reverted code, passes with the fix.
Full suite 3000 passed / 3 failed (all three pass with `--test-threads=1` = known flakes).
Reversibility: one-line guard + one test.

### 2026-07-22 22:45 — Toggle click flipped live state instead of the label it rendered
What: `save_auto_resize_splits(!self.state.auto_resize_splits)` -> `!auto_resize_enabled`
destructured from the menu kind (src/app/input/modal.rs).
Why: the label is rendered from a snapshot taken at menu-open time; a config hot-reload while
the menu is open desyncs them, so the click does the OPPOSITE of what the visible item says.
Narrow window, but the fix is strictly more correct and costs nothing.
Evidence: Fable menu-state lens, confirmed by adversarial verifier.
Reversibility: one line.

### 2026-07-22 22:45 — socket-api.mdx federation claim was wrong a SECOND time
What: rewrote the ratio-federation paragraph again — it now states ratios are not federated at
all and a resync does not restore them.
Why: the first correction (M5-era) fixed one false clause and introduced another: it claimed a
structural resync recovers stale ratios. Traced: `SnapshotResponse` does carry `layouts`, but
`reconcile_by_diff` reconciles workspaces/tabs/panes only and never applies them. Lesson: this
paragraph has now been wrong in two different ways; doc prose about a subsystem the change does
not touch needs the same source-tracing as code.
Evidence: Fable api-surface lens traced reducer.rs:338-354 + client.rs:742.
Reversibility: doc-only.

### 2026-07-22 23:10 — H1 closed at the class level, not the instance level
What: Moved the pre-close path -> post-collapse ancestor translation out of the call site into
`TileLayout::balance_areas_after_removal` (src/layout.rs), and replaced the single-fixture close
tests with an exhaustive sweep `balance_after_removal_never_touches_a_non_ancestor_split` over
EVERY binary tree shape with 2-5 leaves x every removable pane.
Why: H1 was fixed twice and was wrong both times, each time one level shallower (full path ->
`saturating_sub(2)` -> still broken at depth 1). Two hand-picked fixtures each happened to miss
the shape that mattered. Root cause is an API footgun, not arithmetic: `balance_areas_along_path(&[])`
means "balance the root", so every "no ancestors survive" case silently degrades into "rebalance
the promoted sibling". Encoding the rule beside the tree code makes it unit-testable and gives
the L<2 case an explicit `checked_sub` early return instead of a clamp.
Evidence: sweep asserts the real contract (any split that is NOT an ancestor of the removed pane
in the pre-close tree keeps its exact ratio) using collision-proof marker ratios, plus a positive
assertion that surviving ancestors ARE rebalanced so a no-op impl cannot pass. Reverting to
`saturating_sub` fails the sweep at the smallest possible shape: 3 leaves, `path=[false]`,
marker 0.13 destroyed. Full suite 3002 passed / 4 failed, all four pass at `--test-threads=1`.
Reversibility: new method + call-site simplification + 3 tests; the old inline form is one edit away.

### 2026-07-22 23:25 — Push blocker routed around, not fixed: PR #4 open
What: Cherry-picked the 3 auto-resize commits onto `origin/master` (1c833031, clean) as
`feat/pane-auto-resize-splits-on-origin`; pushed; PR #4 open.
Why: local master (5ec2a10b) still carries 385MB+193MB+193MB under
`vendor/libghostty-vt/macos/GhosttyKit.xcframework/`. Verified those blobs are DEAD: build.rs:69
passes `-Demit-xcframework=false` and the `rerun-if-changed` list (:33-40) never covers `macos/`;
two of the three are iOS static libs. The permanent fix (drop the dir + gitignore, or LFS) is
still NOT done — this run only routes around it, same workaround the resize-rerender run used.
Evidence: `git ls-tree -r -l 5ec2a10b` vs `origin/master` (latter has zero >50MB blobs).
Two real cherry-pick conflicts, both from the base being pre-v0.7.5: (a) modal.rs add/add pulled
in a v0.7.5-only test referencing `workspace_create_label`, which does not exist on this base —
dropped; (b) socket-api.mdx method table — took the BASE inventory and added only `layout.balance`,
since taking our side would have documented v0.7.5 methods this base lacks.
Full suite on the new base: 2809 passed / 0 failed; fmt clean; schema artifact already current.
Reversibility: branch + PR are additive; nothing existing was rewritten or force-pushed.

### 2026-07-22 23:52 — Merged as 9e8ada16; base sync impossible, not forced
What: PR #4 merged (merge commit `9e8ada16`). Both auto-resize worktrees removed; both local
branches and the remote branch kept.
Why: `git pull --ff-only` on local `master` aborts — 81 local-only commits (the unpushed v0.7.5
merge lineage) vs 9 on origin. Forcing it would need a merge commit, which the ff-only rule
forbids and which would entrench the unpushable history further. Local `master` therefore no
longer contains work that IS on origin; treat `origin/master` as the source of truth for
anything shipped.
Evidence: `git rev-list --left-right --count master...origin/master` -> 81 / 9.
Reversibility: worktrees re-addable from the kept branches; nothing deleted remotely.
