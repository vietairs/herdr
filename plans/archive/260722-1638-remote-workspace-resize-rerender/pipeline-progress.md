# PIPELINE COMPLETE — merged 2026-07-23

PR #2 MERGED as `6058ce53` on `origin/master`. Worktrees torn down.

**Open item for the user:** local `master` (`5ec2a10b`, the unpushable v0.7.5 merge) has now DIVERGED from `origin/master` (`6058ce53` = the fix on top of `b5cb8ce8`). `git pull --ff-only` will refuse. Reconciling needs the large-files decision first — drop `vendor/libghostty-vt/macos/**` from the v0.7.5 merge and gitignore it, or git-lfs those paths — then merge/rebase local master onto origin.

Local branch `fix/remote-workspace-resize-rerender` (off `5ec2a10b`) was KEPT: its content is merged upstream under different SHAs, but it is the only copy based on local master. Remote branch `fix/remote-workspace-resize-rerender-on-origin` also kept — deleting it is the user's call.


- [x] 1. /hvn-worktree
- [x] 2. /hvn-debug — cause PROVEN (3 lenses + fable advisor)
- [x] 3. /hvn-fix — repaint nudge + mount size ownership
- [x] 4. /hvn-code-review — DONE, no blockers/majors
- [x] 4b. Fable-tier review (3 lenses + adversarial refutation, 10 agents) — 7 raised, 5 survived, none blocking; 1 fixed (`5187d738` stale nudge restore), rest are pre-existing follow-ups
- [x] 5. Live verification — PARTIAL (see below)
- [x] 6. Push + PR — **https://github.com/vietairs/herdr/pull/2** (OPEN, MERGEABLE, CLEAN; no CI configured on the fork)
- [x] 7. Merge — MERGED 2026-07-23 as 6058ce53
- [x] 8. Pre-merge review (15 fable agents) — 0 blockers; 3 findings fixed (7392001d, ae3af95b), 1 deferred
- [x] 9. Worktree teardown — both removed

## Branches

- `fix/remote-workspace-resize-rerender` (local only, off `5ec2a10b`) — commits `30c7dbe8`, `bf760e3a`. UNPUSHABLE, see below.
- `fix/remote-workspace-resize-rerender-on-origin` (pushed, PR #2, off `origin/master` `b5cb8ce8`) — same two commits cherry-picked clean: `25c38d97`, `54eef181`. Build + fmt + federation tests green on this base.

## The push blocker and the workaround

`git push` of anything based on local `master` (`5ec2a10b`) is rejected: the unpushed v0.7.5 merge added 10 files / 771 MB under `vendor/libghostty-vt/macos/GhosttyKit.xcframework/`, two of them over GitHub's 100 MB limit. `b5cb8ce8` has zero such files.

**Those binaries are not used by the build.** `build.rs:69` passes `-Demit-xcframework=false` and links `zig-out/lib/libghostty-vt.a`, a Zig build artifact; the `rerun-if-changed` list covers `build.zig`, `include`, `pkg`, `src`, `VERSION` — never `macos/`. They are upstream Ghostty artifacts that rode in with the vendoring.

Workaround used: branch from `origin/master` instead, cherry-pick, push. No history rewritten, nothing existing touched.

Permanent fix (user's call, not done): drop `vendor/libghostty-vt/macos/**` from the v0.7.5 merge and gitignore it, then push master. Alternative: git-lfs those paths.

## Live verification — what was and wasn't shown

- SHOWN: a federation mount hydrates and mirrors a real remote pane (`r:appn-ltu-vm-100#probe:w1`), and the pane renders correctly across a 40x130 -> 30x100 resize on the fixed serving host.
- NOT SHOWN: that the fix caused it. The A/B never discriminated — an unfixed host looked identical, because a mount whose size actually changes fires a genuine SIGWINCH and the app repaints on its own. The failing condition is narrower: same-size (dedupe-swallowed) resizes, and apps that ignore SIGWINCH. Constructing it deliberately was not cheap; abandoned after several harness iterations.
- CONFIRMED INDEPENDENTLY (this motivated the size-ownership commit): `appn-ltu-vm-105` runs its own foreground TUI (`pts/6`, `Sl+`, 3h34m) while serving the user's mount, so the remote-host clobber is live, not theoretical.

Environment left clean: probe daemons/sessions removed on both machines; vm-100's `~/.local/bin/herdr` left as a master build; the user's own vm-100 daemon and vm-105 session untouched throughout.

## Fable review outcome — all confirmed findings fixed

PR #2 now carries 5 commits. Beyond the two original fixes:

- `5187d738` nudge restores the current size, not the one frozen at queue time.
- `cce7df02` mount size ownership is per terminal (was a session-wide bool that froze every host terminal the mount never opens — the client opens them lazily), and outranks direct attach (both `headless.rs` paths were unguarded).
- `df571bb2` the nudge no longer sleeps 30ms on the PTY actor thread; the restore is scheduled on the run loop and flushed on exit.

Every new guard verified falsifiable: revert the fix, the test fails.

## Detail

Fixed: `src/pty/actor/unix.rs` nudge restored the size frozen at queue time, clobbering a resize applied in the same drain (`apply_pending_controls` runs resize before nudge). Now restores `last_applied_size`. Regression test verified to fail on the old code.

Rejected: gating the post-resize nudge on "size changed" (proposed by 2 of 3 lenses). `runtime.resize` is a latest-wins slot — coalesced Resizes can produce a no-op ioctl and no SIGWINCH even when neither client dedupe fired, so gating reintroduces the blank-pane bug during drag-resize. Unconditional is correct at that site.

Accepted as-is: the nudge's 30ms actor-thread sleep and transient rows-1 repaint during drag-resize (bounded by the coalescing slot to ~1 per drain; the F1 design cost).

## Deliberately out of scope

- Cell pixel metrics dropped on the wire (breaks kitty-graphics/sixel sizing).
- `serve.rs` mount-generation filter (silently drops all frames after a remount in one client process).
- Windows serving hosts keep the repaint gap (nudge is `cfg(unix)`).
- The size-ownership trade-off may want reversing: while mounted, the serving host's own TUI views those panes at the mount's geometry.
