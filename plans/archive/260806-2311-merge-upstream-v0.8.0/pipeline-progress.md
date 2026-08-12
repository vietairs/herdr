- [x] 1. worktree create — done 23:16 — .claude/worktrees/merge-upstream-v0.8.0 — cost: 0 agents/00:20
- [x] 2. graft + merge (dry) — done 23:20 — 40 conflicts, merge-base c4c4b352 — cost: 0 agents/00:40
- [x] 3. conflict resolution fan-out (5 groups) — done 23:47 — all 40 files, 0 markers — cost: 5 agents/~26:00
      - [x] A rust-core — 4 files + 3 latent non-marker bugs found via cargo check
      - [x] B rust-app — 7 files; flagged clipboard.rs origin mismatch
      - [x] C rust-ui — 3 files, no concerns
      - [x] D build/vendor — 4 files; vendor-bump direction reported BACKWARDS (corrected by orchestrator)
      - [x] E docs/website — 22 files; fork federation API/config keys re-added
- [x] 3b. advisory pass (--advise) — done — 2 of 6 assumptions falsified by verification
- [x] 3c. orchestrator fixes — clipboard.rs origin (3 sites), FEDERATION_PROTOCOL_VERSION 4->5
- [x] 4a. build fixes, non-ghostty — done 23:49 — 0 non-ffi errors — cost: 1 agent/09:06
      RenderSignal migration (incl. federation client.rs root cause), Method::WorkspaceMoveBlock,
      input/mod.rs return-type + move bug
- [x] 4b. vendor upgrade c5a21edfc + patch rebase — done — 0 ffi errors
- [x] 4c. full build green + cargo test — done — cost: 1 agent/10:22
      serial: 3364 passed / 0 failed / 0 ignored
      --test-threads=4: 3362 passed / 2 failed — both verified PRE-EXISTING flakes
      (pane_graphics_stream.rs byte-identical to master; plugins/mod.rs diff is a
      cosmetic ogulcancelik->herdrdev URL rename only). No test weakened:
      #[ignore] count 0 (== master), #[test] count 3277 -> 3489.
      Also resolved: docs/next/api/herdr-api.schema.json had LIVE conflict markers
      missed by stage 3 — regenerated via HERDR_UPDATE_API_SCHEMA=1, not hand-edited.
- [x] 5. code-review + security-scan (parallel) — done — cost: 2 agents/~16:00 + ~3:30
      security: DONE — no merge-introduced regression on the 6 audited boundaries.
        Federation core zero-diff; only 3 files / 27 insertions under src/remote/ in whole merge.
        SSH -oProxyCommand guard intact (unix.rs:289). 0 secrets. Symlink hole not widened.
        NOT covered: ~21k lines of non-federation diff; vendored libghostty clipboard internals.
      code-review: DONE_WITH_CONCERNS — 2 BLOCKING (both orchestrator-verified, see notes 9/10):
        B1 kitty-graphics fast path dropped -> write-only dead state, reverts v0.8.0 CPU fix
        B2 send_bytes_after no-op on remote -> `agent prompt` returns success, never submits
      Both carried-forward worries #1 (Osc52Forwarder) and #4 (origin: None) cleared as CORRECT.
      Zero fork-original symbols lost (symbol-set diff = empty set).
- [x] 5b. blocking fixes (2 parallel agents, disjoint files) — done — cost: 2 agents/~7:15 each
      A: B1 kitty fast path restored (read at mod.rs:1503), 6/7 tests ported,
         scroll_viewport_row wrapper + call site restored
      B: B2 send_bytes_after remote implemented via cloned input_tx + regression test,
         clipboard dispatch de-duplicated (3 copies -> 1), key.clone() removed,
         9 unused cli.rs imports + 1 orphaned config_io writer removed
      B correctly REFUSED 2 wrong premises the orchestrator passed down (see note 12).
- [x] 5c. orchestrator: retired fork unicode_width fallback (note 11) + ported 7th test
- [x] 5d. full suite + fmt — done
      serial run 1: 3369 passed / 1 failed  -> failure reproduced as a LOAD-TIMING flake
                     (passes 3/3 isolated; file byte-identical to master)
      serial run 2: 3370 passed / 0 failed
      cargo fmt: 2 merge-artifact import wrappings fixed, now clean
      cargo check --all-targets: 0 errors (sole "unused Ordering" warning is the
      verified cfg(not(test)) false positive)
      index staged: 0 unmerged paths, 0 conflict markers, 957 files
      vendor: c5a21edfc; 1 on-disk patch, indexed, reverse-applies (2 documented as removed)
- [x] 6. ship gate — PASSED on everything this machine can validate
      3370/3370 serial, fmt clean, check clean, 0 markers, 0 unmerged, patch index consistent,
      0 fork-original symbols lost. NOT covered: Windows cfg arms, live federation (needs VMs).
- [x] 7. PR — https://github.com/vietairs/herdr/pull/10 OPEN, merge-ready, NOT merged
      commit a021433a; parents 572e7390 + 346411fa (real upstream commit, graft did not poison)
      CLAUDE.md restored to fork's version per user decision (note 13)

## Follow-ups for AFTER this merge (do not scope-creep into it)
- `src/remote/federation/session.rs:321` — `herdr attach <remote>` federated-session mode still
  drops remote clipboard writes (`_outbound_clip_rx`, no drain task). PRE-EXISTING, zero diff vs
  master. Same bug class the TUI path already fixed.
- `src/app/api/panes.rs:1866` — request-id string `"federation-close-pane"` used as a trust
  discriminator; any local socket client can send it. PRE-EXISTING (4 occurrences on master).
- Two parallel-only test-isolation flakes (pane_graphics_stream, plugins manifest_action_invoke).

## Review focus (carried forward — do NOT lose these)
- `src/pane/osc.rs`: `Osc52Forwarder` REMOVED, `CwdOscTracker` ported to upstream's
  `OscStreamCollector`. Biggest single behavioral change from conflict resolution.
- `src/pane.rs` `send_bytes_after`: no-op for remote panes — inferred, not copied. Feature gap?
- `src/app/input/mod.rs:101`: `key.clone()` per keystroke, workaround for a latent move bug.
- `FEDERATION_PROTOCOL_VERSION` 4->5 needs a live merged<->v4 mount to prove clean handshake reject.
- Windows `#[cfg]` arms are NOT covered by the mac build.
