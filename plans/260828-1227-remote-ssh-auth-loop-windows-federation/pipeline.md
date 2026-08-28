# Pipeline — remote SSH auth loop + Windows federation workspace

Task: Fix `herdr --remote` re-prompting for the SSH password on Windows, and make the
federated workspace work in the Windows build.
Task source: free text (user, 2026-08-28), flags `--auto --advise`
Branch/worktree: `fix/remote-ssh-auth-loop-windows-federation`
  at `.claude/worktrees/remote-ssh-auth-loop-windows-federation`

## Classification
Complexity: hard -> hard (cause proven for W1; W2 reclassified bug -> unimplemented port)
Risk: HIGH — rewrites the SSH authentication path; auth keyword; code-changing (R7)
Familiarity: HIGH — fork's own federation work, 6 prior federation plan dirs
Scope: MULTI-PHASE — two workstreams, W2 spans ~120 cfg(unix) gates over 10 files
Payoff: HIGH — fork ships Windows binaries since v0.8.2-hvn.2; `--remote` unusable
  there without pre-existing key auth

## Confirmed direction (user, direction-confirm gate)
- W2 scope: FULL PORT, Windows client -> unix host. Out of scope: multi-remote daemon
  mounts, Windows-as-host (`host_unix.rs`, `run_federation_serve_bridge`, `file_staging`).
- Build/verify: install Rust toolchain on this laptop (1.96.1, pinned).
- E2E: user supplies the ssh target when phases reach e2e.

## Route
1. worktree create
2. W1a preflight auth probe (BatchMode detection on non-multiplexing platforms)
3. W1b remote-side shutdown wait — remove the per-poll ssh spawn loop
4. W1c probe consolidation into one tagged remote script
5. W2 Phase 0 route decision before the snapshot dial + honest notice
6. W2 Phase 1 ungate events / app::actions / federation::client
7. W2 Phase 2 ungate federation::session + App::new_federated, Windows PTY backstop,
   flip attach.rs:696
8. build + test + clippy + windows-lint
9. W2 Phase 3 e2e against the real host
10. code review -> ship-gate -> PR -> pre-merge review & fix -> docs

Skips: brainstorm (direction already confirmed against advisory counsel);
`/ak-predict` persona debate (cause proven, approach chosen with advisory input).
