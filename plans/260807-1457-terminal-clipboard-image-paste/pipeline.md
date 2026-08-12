# Pipeline — terminal-dependent clipboard image paste failure

Task: Clipboard image paste into a mounted remote (federated) workspace works in the cmux app but
not in Apple Terminal.app or Warp. Fix it.

Task source: free text (user request, 2026-08-07)
Flags: --auto --advise
Created: 2026-08-07 14:57 (Australia/Melbourne)
Worktree: .claude/worktrees/terminal-clipboard-image-paste (branch fix/terminal-clipboard-image-paste)

## Route card (verbatim, write-once)

```
ROUTE CARD — fix: clipboard image paste into mounted remote workspace fails in Apple Terminal & Warp (works in cmux)
Complexity: hard -> hard — evidence narrowed the surface but did NOT prove cause; both prior
            paste fixes confirmed present on master, so this is a distinct defect (2 scouts, ~2:30)
Risk: medium — federation + clipboard + input decode; no auth/schema/payment surface. Test
      coverage exists around image_path/macos clipboard (src/platform/macos.rs:1074,1121)
Familiarity: high — 3 prior plan dirs + 3 memories on this exact subsystem
Scope: small-to-feature — input decode layer + possibly a capability/keybinding fallback
Payoff: high — user cannot paste images to remote agents in their actual daily terminals
Change set: 4 files (probable, non-final)
  src/app/input/mod.rs (change) · src/raw_input.rs (change) · src/input/parse.rs (change)
  src/platform/macos.rs (change)
Advise: 2 gates — ship-gate attestation, before-merge approval — via kongming (--auto)
Route (R10 — bug, root cause UNKNOWN; cause-finding IS the discovery):
  1. /ak-worktree create — agent:git-manager
  2. /ak-debug prove cause — agent:hvn-root-causer (inherits tier)
  3. /ak-fix — agent:fullstack-developer
  4. /ak-code-review || /ak-security-scan — concurrent, read-only
  5. /hvn:ship-gate — main-loop (attestation)
Skips: blindspot, brainstorm, predict, plan — R10: the cause-finding is the discovery
Autonomy: --auto — all gates auto-adjudicated + logged to auto-decisions report
```

## Evidence carried into this run (do not re-derive)

- Entry A: keybinding `keys.remote_image_paste`, default `ctrl+v`, reads OS clipboard
  (`src/platform/macos.rs:536` PNGf -> furl fallback). Dispatched `src/app/input/mod.rs:99-132`.
- Entry B: bracketed-paste temp-path bridge, `src/app/input/mod.rs:228-286,1016`, validator
  `src/image_path.rs`, temp_dir-gated + TOCTOU-safe.
- Both landed on master in commit `b2e0713f` (2026-07-24). This defect is NOT a regression of them.
- Unmerged elsewhere: `3aed04a9` (KeyEventKind::Press duplicate-paste guard) on branch
  `feat/remote-workspace-paste-image-files` — a dedupe fix, not a no-paste cause.
- Prior art: plans/260722-1624-*, plans/260724-1034-*, plans/260724-1213-* (cmux comparison).
