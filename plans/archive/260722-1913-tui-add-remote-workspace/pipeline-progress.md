- [x] 1. /hvn-worktree — done 19:16 — ../herdr-worktrees/tui-add-remote-workspace (feat/tui-add-remote-workspace) — cost: 0 agents/00:25, tokens est. 1k
- [x] 2. /hvn:blindspot (parallel scout fan-out) — done 19:28 — 3 scouts: ui-dialog-patterns, api-mount-path, target-parsing-security — cost: 3 agents/~10:00, tokens est. 180k
- [x] 3. /hvn-predict — done 19:36 — reports/predict-260722-1913-tui-add-remote-workspace-report.md — cost: 1 agent/~07:00, tokens est. 90k
- [x] 4. /hvn-plan --tdd --parallel + validate — done 19:52 — plan.md (3 phases) + reports/plan-validation-...md (APPROVE_WITH_FIXES, 8 required fixes, all injected into implementers) — cost: 2 agents/~15:00, tokens est. 260k
- [x] 5. /hvn:impl-notes init — done 19:52 — implementation-notes.md (created by plan stage)
- [x] 6. /hvn-cook --auto --parallel — done 20:05 — 3 phases: server validation / TUI collector / docs; 15 files changed +405/-36, 2 new files — cost: 3 agents/~13:00, tokens est. 420k
- [x] 7. /hvn:impl-notes review — done 20:05 — folded into phase reports + implementation-notes.md
- [x] 8. /hvn-code-review || /hvn-security-scan || test-suite — done 20:10 — 3 concurrent verifiers — cost: 3 agents/~06:00, tokens est. 250k
      FINDINGS: tests effectively green (2978/2980 bin + 89/89 integration, failures are 3 known
      baseline flakes outside the diff). BLOCKERS FOUND: cargo fmt --check fails in 4 branch files
      (master clean → ours); docs document unusable `user@host[:port]`; `submitting` flag dead
      (3 guards + 1 render branch unreachable); `..._error_from_server` test never reaches server
      (client rejects first → ErrorResponse branch untested); `%target` log injection.
      Security: the one real vector (leading-`-` → -oProxyCommand) is closed + tested at 3 sites;
      shell-metachar and ssh-config-CRLF injection structurally impossible (pure argv).
- [x] 9. remediation pass (3 parallel fixes + fmt/test rerun + re-review) — done 20:25 — verdict CLEAN
      — cost: 5 agents/12:03, tokens est. 354k
      F-A submitting deleted (zero hits in src/); F-B client checks removed, server authoritative —
      ErrorResponse branch PROVEN load-bearing (deleting it makes the test fail, verified by
      experiment); F-C %target→?target at :86,:94 plus a third site found at :109; F-D docs
      corrected to ssh://user@host:port after verifying argv construction in source.
      fmt: worktree already clean, cargo fmt rewrote 0 of 244 .rs files (earlier 4-file regression
      was fixed by the fix agents themselves). Suite: 3066 pass / 2 fail, both pre-existing flakes
      outside the diff, 3/3 pass isolated.
- [ ] 10. /hvn:ship-gate — pending
- [ ] 11. /hvn-ship + /hvn-review-pr --fix --reply — pending (HELD: outward-facing, PR must target
      this fork's master — external-contributor guardrail, acting account is vietairs)
