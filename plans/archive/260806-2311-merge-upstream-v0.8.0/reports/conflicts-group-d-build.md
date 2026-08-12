# Conflicts group D (build/vendor files) — merge upstream v0.8.0

Worktree: `/Users/hvnguyen/Projects/herdr/.claude/worktrees/merge-upstream-v0.8.0`
Merge: HEAD `572e7390` (fork master) x MERGE_HEAD `346411fa` (upstream v0.8.0), `--no-commit`.

All conflict markers removed in the 4 owned files. Nothing staged/committed per instructions.

## 1. Cargo.toml

Single hunk: `version = "0.7.5"` (fork) vs `"0.8.0"` (upstream). Took upstream's `0.8.0` — this
merge exists to land upstream v0.8.0.

Checked fork-added deps (`git diff 848f11f1 572e7390 -- Cargo.toml`): fork added extra `tokio`
features (`io-util`, `io-std`, `process`) and a new `tokio-util = { version = "0.7", features =
["rt"] }` dependency for the federation feature. Both were already present in the merged file
outside the conflict hunk (git merged that region cleanly), so nothing extra to restore. No new
deps introduced, per project rule.

## 2. Cargo.lock

3 conflict hunks. Per instructions, did not hand-merge: `git checkout --theirs Cargo.lock` then
regenerated:

```
ZIG=~/.local/zig-0.15.2/zig cargo update --workspace --offline   # failed: jsonc-parser not in offline cache
ZIG=~/.local/zig-0.15.2/zig cargo update --workspace             # succeeded
```

Output: `Locking 1 package to latest compatible version / Adding tokio-util v0.7.19` — only new
package added is `tokio-util` (the fork's own federation dependency getting locked against the
merged `Cargo.toml`), 80 other entries unchanged. Verified `name = "herdr"` / `version = "0.8.0"`
and `tokio-util` present in the regenerated lockfile. No conflict markers remain.

Note: `cargo metadata --offline` afterwards fails only on an unrelated offline-cache miss
(`futures v0.3.33` not cached locally) — this is an environment/network limitation, not a
Cargo.toml/Cargo.lock correctness problem; the lockfile itself parses and resolves fine (that's
what the successful `cargo update` proves).

## 3. .github/workflows/ci.yml

Single hunk, Windows check step: fork (HEAD) had a large inline PowerShell block (fmt, clippy,
targeted cargo tests, build, with a Zig-cache-clear-and-retry helper) vs upstream's `run: .\
scripts\windows_check.ps1 -Mode check` (upstream extracted the same logic into a checked-in
script).

Checked `git show 00b86618 -- .github/workflows/` (the fork's prior CI adaptation commit) — that
commit only touched `release.yml` (strip `-hvn.N` suffix for changelog lookup, skip
`close-released-issues`/`update-latest-json` for fork tags). It made no changes to `ci.yml`, and
grepping the current `ci.yml` for `hvn`/`release-channel`/`maintainer` returns nothing. So this
conflict is a generic upstream refactor, not a fork-specific CI adaptation.

Resolved by taking upstream's version wholesale (`.\scripts\windows_check.ps1 -Mode check`).
Verified `scripts/windows_check.ps1` exists on disk (added by the merge, not conflicted). No
fork-specific CI logic was lost.

## 4. vendor/libghostty-vt.patches.md

Single hunk on the surface, but investigation surfaced something more significant: **upstream
bumped the vendored libghostty-vt commit and independently introduced its own local patch, also
numbered "0001"**, colliding with the fork's own "0001" patch.

**Vendored commit change (important — flagged prominently as instructed):**

- HEAD (fork): `source_commit: 0f7cd84b880b203c98683e520e84b9db0c5938d8`
- MERGE_HEAD (upstream): `source_commit: c5a21edfcbc2d5b46540ad91b7980aca31f5f1f3`

Upstream bumped the vendored source. This matters because the fork's two local patches were
created against the old base and needed to be checked for continued applicability.

**What each side had:**
- Fork (HEAD): two local patches — `0001-backport-resizecols-cursor-subtraction.patch`
  (herdr issue ogulcancelik/herdr#465) and `0002-expose-kitty-image-transmit-time-ns.patch`
  (herdr issue ogulcancelik/herdr#947), both created against vendored base `0f7cd84b8`.
- Upstream (MERGE_HEAD): its own new local patch, also filed as "0001" —
  `0001-default-grapheme-cluster-mode.patch` (herdr issue herdrdev/herdr#243), created against
  the new vendored base `c5a21edfc`. This is a genuinely different patch (default DEC mode 2027
  grapheme clustering), not the same fix under two issue links — the two "0001" headings were a
  numbering collision, not the same content re-described.

**Resolution — unioned all three entries, renumbered to remove the collision:**
1. `## 0001 default lib-vt panes to grapheme clustering` — upstream's, kept verbatim (patch file
   `0001-default-grapheme-cluster-mode.patch`, vendored base `c5a21edfc...`).
2. `## 0002 expose kitty image transmit time in the C API` — fork's, kept (patch file
   `0002-expose-kitty-image-transmit-time-ns.patch`).
3. `## 0003 backport resizeCols cursor subtraction saturation` — fork's, renumbered from `0001`
   to `0003` in the heading only, to avoid the collision (patch file on disk is still literally
   named `0001-backport-resizecols-cursor-subtraction.patch` — did NOT rename the file per
   instructions not to touch/delete patch files, only the index heading number changed).

No entries dropped. `ls vendor/patches/libghostty-vt/` confirms all three patch files exist and
are all listed in the index (`0001-backport-resizecols-cursor-subtraction.patch`,
`0001-default-grapheme-cluster-mode.patch`, `0002-expose-kitty-image-transmit-time-ns.patch`).

**Vendored-base fields updated:** both fork patches' `vendored base:` lines were updated from the
stale `0f7cd84b8...` to the current `c5a21edfc...`, because verification (below) showed both
patches are still actually applied in the current working tree, i.e. they survived the vendor
bump/merge without needing manual reapplication.

**Reality check performed (as instructed) — do the fork's patches still apply / are they now
redundant because upstream's bump already contains the fix?**
- resizeCols saturation: `grep -n "remaining_rows = self.rows\|const current = self.rows"
  vendor/libghostty-vt/src/terminal/PageList.zig` shows the patched saturating-subtraction form
  (`self.rows -| (c.y + 1)`) already present in the current (post-merge) working tree — the patch
  survived the automatic merge of `PageList.zig`/`terminal.zig` (those files were not
  conflicted). Still needed; upstream's new base does not appear to include ghostty-org/ghostty
  PR #12907 (no separate evidence upstream took it, and the patched code is exactly the fork's
  patch's replacement text, not some other upstream fix). Not redundant.
- kitty image transmit time: `grep -rn "transmit_time_ns\|TRANSMIT_TIME_NS"` in
  `kitty_graphics.h`/`kitty_graphics.zig` shows the fork's C API additions
  (`GHOSTTY_KITTY_IMAGE_DATA_TRANSMIT_TIME_NS`, `.transmit_time_ns`, the retransmission test)
  already present in the current tree. Still needed; `introduced upstream: not yet` remains
  accurate — no evidence upstream added this API.

Neither patch's stated "remove when" condition appears to be met by this vendor bump. Both
patches applied cleanly through the automatic merge (their touched files were not in the
conflicted-file set), so no manual patch reapplication was required — only the doc index needed
fixing.

I did not run the `just check` maintenance test that reverse-applies these patches (no `just` on
this machine per task instructions, and full zig build is out of scope for a doc-only file); a
reviewer should run it once `src/**` conflicts from other agents are resolved and the tree
builds, to get a hard verification that all three patches still reverse-apply cleanly against
`c5a21edfc...`.

## Files touched
- `Cargo.toml`
- `Cargo.lock`
- `.github/workflows/ci.yml`
- `vendor/libghostty-vt.patches.md`

No other files touched. Did not `git add`/`commit`/`merge --continue`.

Status: DONE
Summary: All 4 owned conflict files resolved, markers removed, Cargo.lock regenerated cleanly (only adds tokio-util); vendor patches index unioned + renumbered to fix an 0001/0001 collision from upstream's own new libghostty-vt patch, with vendored-base fields updated to the new c5a21edfc commit and both fork patches confirmed still applied in the working tree.
Concerns: (1) Vendored libghostty-vt commit changed 0f7cd84b8 → c5a21edfc — flagged per instructions; verified both fork patches still present/applied post-merge, but a reviewer should still run `just check`'s patch-reverse-apply maintenance test once the tree builds, since I could not run it here (no `just`, offline cargo). (2) Renumbered the fork's resizeCols patch heading from "0001" to "0003" without renaming its on-disk filename (`0001-backport-resizecols-cursor-subtraction.patch`) — filename prefix and doc heading number are now intentionally out of sync; acceptable since instructions said not to rename/delete patch files, but worth a maintainer sanity check. (3) `cargo metadata --offline` still fails on an unrelated offline-cache gap (`futures v0.3.33` not cached) — not a Cargo.toml/Cargo.lock problem, just this sandbox's network/cache state.
