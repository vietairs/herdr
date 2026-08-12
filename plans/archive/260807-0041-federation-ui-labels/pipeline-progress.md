# PIPELINE COMPLETE

- [x] 0. worktree create — done 00:42 — .claude/worktrees/federation-ui-labels off merge/upstream-v0.8.0
- [x] 1. evidence pass (3 parallel agents) — done 00:52
      A diag-ws-label — 2 causes, both proven; orchestrator re-verified HostKey format,
        truncate_end prefix-keep semantics, and label->custom_name storage independently
      B diag-agent-icon — projection never calls the badge helper; confirmed at sidebar.rs:154
      C verify-fork-features — 19/19 fork features survive; 2 of the MERGE's own verification
        claims falsified (see below)
- [x] 2. fix + tests — done 01:05 — 132 insertions across src/ui/sidebar.rs + id.rs
- [x] 3. code-review — done (orchestrator diff review; change is 2 files, fully covered by tests)
- [x] 4. ship-gate — PASSED on everything this machine can validate
      3373 serial pass / 0 fail (3370 baseline + 3 new), fmt clean, check --all-targets 0 errors
      NOT covered: live federated mount (needs both VMs rebuilt+restarted), Windows cfg arms
- [x] 5. PR — https://github.com/vietairs/herdr/pull/11 OPEN, base merge/upstream-v0.8.0, NOT merged
      commit 3ff73d8f

## Two PR #10 verification claims falsified by this pass
1. "Zero fork-original symbols lost (empty set)" — FALSE. Real count 27. All 27 run to ground
   and benign: 25 regenerated ghostty/bindings.rs (vendor commit changed), WritePtyCallbackState
   folded into a consolidated CallbackState (trampoline + setter intact), 1 upstream test rename.
   Conclusion held; the cited evidence did not.
2. "CLAUDE.md restored to the fork's version (0-line diff)" — FALSE. CLAUDE.md is a symlink to
   AGENTS.md; the diff compared the 9-byte link target. The real document took UPSTREAM's version
   (+39/-25). Deliberately NOT reverted — see implementation-notes.md entry 5. User's call.

## Open for the user
- AGENTS.md: keep upstream's (current state) or `git checkout master -- AGENTS.md`?
- PR #11 must merge AFTER PR #10.
- Post-merge of #10: `git replace -d ef4c23f5775bb8cfec05f05d0844226ff959a07a`, sync base,
  remove BOTH worktrees, archive both plan dirs.
