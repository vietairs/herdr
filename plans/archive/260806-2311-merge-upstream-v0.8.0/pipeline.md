# Pipeline — merge upstream herdrdev/herdr v0.8.0 into vietairs fork

Task: update forked codebase to match upstream release v0.8.0
Task source: free text (user, `/hvn:cortex ... --auto --advise`)
Timestamp: 2026-08-06 23:11 AEST

```
ROUTE CARD — merge upstream v0.8.0 into diverged fork, preserving federation work
Complexity: hard → hard — evidence pass found upstream FORCE-PUSHED after v0.7.5;
  naive merge base collapses to 2026-07-13 and replays 1069 files. Graft fixes it. (6 probes, ~04:00)
Risk: high — wire PROTOCOL_VERSION 17(fork) vs 19(upstream); vendored libghostty patches;
  federation surface (fork-only) overlaps 53 upstream-touched src files
Familiarity: high — fork owner did the v0.7.5 merge (354 conflicts); prior plan dir + memory exist
Scope: multi-phase — 40 conflicted files across rust core / app / ui / build / docs
Payoff: high — fork tracks upstream security+feature work; without it divergence compounds
  (v0.7.5 merge already cost 354 conflicts; delay makes v0.9.0 worse)
Change set: 40 conflicted files (change), 111 upstream-only src files (add/change, auto-merged)
Advise: 2 gates — plan-validation direction, before-merge approval — via kongming (--advise)
Route:
  1. worktree create — agent:git-manager                        [DONE]
  2. graft + merge (dry) — main-loop (irreducible: git surgery) [DONE]
  3. conflict resolution fan-out — 5 parallel agents, disjoint file ownership
  4. build + test — agent:tester
  5. code-review + security-scan — parallel, agent:code-reviewer / security-auditor
  6. ship-gate --hard
  7. PR (no merge — cortex stops at merge-ready)
Skips: blindspot (cause known + prior merge documented), brainstorm (no design debate —
  one correct reconciliation), predict (no new behavior designed; this is a port)
```

## Key mechanism — the graft

Upstream rewrote history after tagging v0.7.5.

- fork merged upstream commit `848f11f1` "release: v0.7.5" (2026-07-21 21:04:32 +0300)
- upstream tag `v0.7.5` now points at `ef4c23f5`, same message, same timestamp
- **trees are byte-identical**: both `2b2745aad19a5b7b1f65fc5789bfac4331c5570a`

Without correction `git merge-base master upstream-v0.8.0` = `64de9279` (2026-07-13),
replaying all of v0.7.5 as new work → 1069 files, ~175k insertions.

Fix applied in the worktree:

```
git replace ef4c23f5775bb8cfec05f05d0844226ff959a07a 848f11f12765ff4eb93f595efe249e1529ce5fc1
```

New merge-base `c4c4b352` (v0.7.5^), 130 commits to replay, **40 conflicts**.

NOTE: `git replace` refs are local-only and are NOT pushed. The recorded merge commit has
real parents, so the resulting history is valid without the replacement. The replacement only
affects merge-base computation during this merge. Future upstream merges will need it again
until a merge commit anchors the graft — after this merge lands, `master` will contain
`upstream-v0.8.0` as a real parent, so subsequent merges compute a correct base natively.

## Conflict ownership (parallel groups, disjoint)

| Group | Files |
|---|---|
| A rust-core | src/pane.rs, src/pane/osc.rs, src/pane/terminal.rs, src/ghostty/mod.rs |
| B rust-app | src/app/mod.rs, src/app/config_io.rs, src/app/runtime_mutations.rs, src/app/api/workspaces.rs, src/app/input/{mod,mouse,copy_mode}.rs |
| C rust-ui | src/ui/panes.rs, src/ui/sidebar.rs, src/server/headless.rs |
| D build | Cargo.toml, Cargo.lock, .github/workflows/ci.yml, vendor/libghostty-vt.patches.md |
| E docs | CHANGELOG.md, docs/next/**, website/** |

## Constraints carried into every group

- Fork-only federation/remote-workspace work MUST survive. Upstream has no federation.
- `src/protocol/wire.rs` PROTOCOL_VERSION: take upstream 19, then re-add fork federation
  messages above it (verify at build).
- Fork release convention is `v<ver>-hvn.N`; upstream release-channel files
  (`website/latest.json`, `website/preview.json`) take UPSTREAM (theirs) — fork does not
  publish to those channels.
- No `unwrap()` in production code; platform code stays compile-gated.
