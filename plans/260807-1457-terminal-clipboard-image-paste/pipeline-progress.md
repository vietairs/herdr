# Pipeline progress

> **RESUMING? START HERE:** `reports/handoff-260807-resume.md` — self-contained handoff written
> 2026-08-07 16:30. Round 4 was in flight at that moment; verify it landed before doing anything.


- [x] 1. /ak-worktree create — done 15:02 — .claude/worktrees/terminal-clipboard-image-paste @ fix/terminal-clipboard-image-paste — cost: 0 agents (inline git)/0:05, tokens est. 400
- [x] 2. /ak-debug prove cause — done 15:09 — reports/debug-260807-terminal-paste-root-cause.md — cost: 1 agent/6:16 + resume 6:34, tokens est. 272k
- [x] 2b. /ak-research cost out Cmd+V helper options — done 15:33 — reports/research-260807-cmdv-screenshot-helper-options.md — cost: 1 agent/12:31, tokens est. 125k
- [x] 3. /ak-fix Case A (wire Ctrl+V into live dispatch) — done 15:30 — reports/fix-260807-ctrlv-live-dispatch.md — cost: 1 agent/8:19, tokens est. 123k
- [x] 4. /ak-code-review — done 15:37 — REQUEST_CHANGES, reports/code-review-260807-ctrlv-live-dispatch.md — cost: 1 agent/5:01, tokens est. 102k
- [x] 5. /ak-fix rounds 2-4 (key-repeat, fall-through, docs, test seam; empty-bracket bridge; M1/M2/M3/Q5) — done
- [x] 6. /ak-code-review re-review round 4 — done 23:13 — APPROVE_WITH_NITS, reports/code-review-260807-round4.md — M1 fix applied by controller after review; 3394 passed / 0 failed
- [x] 7. /hvn:ship-gate — PASSED 23:2x, attested by Can — 12 shipped-as-planned / 1 partial-logged / 2 accepted nits / 0 silently divergent. Explainer: plans/reports/ship-gate-260807-terminal-clipboard-image-paste.html. State file: ship-gate.state = PASSED (commit guard open). Step-7 change fragment SKIPPED — docs/changes/ is gitignored (.gitignore:10:/docs/*).
- [ ] 8. LIVE VALIDATION against a real mount — NOT DONE, owed before merge (recipe in handoff §7)
- [ ] 9. commit + PR — nothing committed yet

## Hexdump probe RESULT — 2026-08-07 15:37, run by the user, MEASURED (no longer an assumption)

Recipe: `printf '\033[?2004h'; stty raw -echo; xxd -c 16`, then Cmd+V with a screenshot on the
clipboard. Evidence: three screenshots supplied by the user.

| Terminal | Bytes emitted on Cmd+V | Consequence |
| --- | --- | --- |
| Apple Terminal.app | **none** (empty output) | Cmd+V unreachable — nothing to hook. Ctrl+V only. |
| Warp | `1b5b 3230 307e 1b5b 3230 317e` = `ESC[200~ESC[201~` (empty bracketed paste, repeated) | **Option A applies** |
| cmux | `ESC[200~/var/folders/ql/.../T/clipboard-2026-08-07-153756-C859F888.png ESC[201~` | already works via Entry B; path is inside `temp_dir()` so the existing gate accepts it |

Settles research report unresolved question 1. Terminal.app's zero-byte behavior is confirmed, so
the controller's original "no bytes reach herdr" claim was correct for Terminal.app and wrong as a
generalization to Warp.

## Queued next (AFTER round 2 lands — same files, would collide)

- **Option A — empty-bracket bridge.** `src/client/mod.rs:1855-1862` already treats
  `\x1b[200~\x1b[201~` as an image-paste trigger but gates it to `is_remote_client`; the federation
  paste path (`src/app/input/mod.rs:278`, `src/app/mod.rs:1962`) discards the empty paste. Bridge it
  → Cmd+V works in Warp. ~20-40 LOC. Security-neutral: the signal is local stdin; a remote pane
  writes output, never input.
- Caveat to handle: an empty bracketed paste is also what a genuinely empty text clipboard produces.
  Falling through to "read clipboard image, find none, toast" is the acceptable outcome — must not
  error or consume the key destructively.

## Open decisions for the user (not blockers for Case A)
- **Cmd+V with a Finder-copied file** — needs the `temp_dir()` gate in `src/image_path.rs:87-109`
  widened; three options offered, deliberately unranked (coverage vs false-positive trade-off).

## Verification log (controller, independent of agent self-reports)

- 15:34 `cargo test --bin herdr app::input:: -- --test-threads=4` → 359 passed, 0 failed. CONFIRMED.
- Diff inspected directly: +221 all additions, 52 production lines, `FallThrough` arm genuinely empty.
- Agent claim "only KeyEventKind::Press reaches this function" — DISPROVEN by review. Reinforces the
  standing rule: verify agent self-reports, especially negative/impossibility claims.
