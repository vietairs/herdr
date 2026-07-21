---
title: "Multi-remote federated workspace launch"
description: "One command starts local + N remote federated workspaces in a single TUI"
status: pending
priority: P2
effort: 2 phases (~2-3d)
branch: feat/remote-workspace-federation
tags: [remote, federation, cli, tdd]
created: 2026-07-21
---

# Multi-remote federated workspace launch

Target: `herdr --remote localhost 131.172.248.163 131.172.248.161 --remote-workspace`
→ 1 local + N remote federated workspaces, ONE TUI, per-host sidebar groups.

## Inputs (do not re-litigate)
- `plans/260721-1403-multi-remote-federated-workspace-launch/reports/blindspot-synthesis-multi-remote-launch.md`
- `plans/260721-1403-multi-remote-federated-workspace-launch/reports/blindspot-scout-cli-parse-lifecycle-surface.md`
- `plans/260721-1403-multi-remote-federated-workspace-launch/reports/predict-260721-multi-remote-federated-workspace-launch.md`
- `plans/260713-1217-herdr-remote-workspace-federation/phase-08-cli-federated-workspace.md`

## Locked decisions
- **Mount ownership (2026-07-21, supersedes original Phase A design):** federation mounts are server-daemon-owned state, reached from `main.rs` via a new `workspace.mount_remote` JSON API command (follows `Method::WorkspaceCreate` pattern, `src/api/schema.rs:65-66`). Original design (`main.rs` spawns an in-process background task feeding a shared `AppState`) was verified infeasible — `auto_detect_launch` execs `herdr server` as a separate process, and `main.rs` becomes a thin socket client with no `AppState` after autodetect (`plans/.../implementation-notes.md` "Phase A — BLOCKED" entry). See phase-a file for full rationale.
- Two-phase delivery: Phase A (local + ONE remote coexist), Phase B (generalize to N).
- `localhost` = local workspace, explicit `is_local_target` matcher, no SSH.
- Per-target fallback (not all-or-nothing) for Phase B.
- Concurrent dials, 25s budget for the whole batch (Phase B), not serial.
- Classic single-target `--remote` (no `--remote-workspace`) stays byte-for-byte unchanged — explicit acceptance test in both phases.
- `Option<RemoteMirror>` → `HashMap<HostKey, RemoteMirror>` happens in Phase B only.

## Phases

| Phase | File | Depends on | Summary |
|---|---|---|---|
| A | `phase-a-local-remote-coexistence.md` | none (P8 baseline in `feat/remote-workspace-federation`) | Reverse P9.2b: mount becomes server-daemon-owned state (new `workspace.mount_remote` API command, `src/api/schema.rs`/`src/api/server.rs`); local App renders alongside ONE federated mount driven server-side; TUI stays a thin client (CLAUDE.md runtime/client boundary guardrail). |
| B | `phase-b-multi-target-mounts.md` | Phase A green | `target: Vec<String>` CLI parse, `localhost` matcher, concurrent per-target dials, `HashMap<HostKey, RemoteMirror>`, per-host sidebar failure notices. |

## Acceptance criteria (whole feature)
1. `herdr --remote localhost host1 host2 --remote-workspace` renders local workspace + 2 remote-host groups in one sidebar, one TUI process.
2. `herdr --remote host1` (no `--remote-workspace`) behaves identically to pre-change code — explicit regression test, byte-for-byte CLI parse output.
3. One target's mount failure never kills the local session or sibling remote mounts — sidebar shows a per-host notice.
4. Rapid focus switch between local pane and any remote pane: zero cross-boundary keystroke leak (Phase A test carried into Phase B at N=2+).
5. `just check` green (fmt, nextest, maintenance tests) at the end of each phase.
6. Manual two-machine smoke (out-of-env, not run by CI) confirms real SSH dial to 2 live hosts — recorded in `implementation-notes.md`, same convention as phase-09b.

## Dependency graph
Phase A blocks Phase B (Phase B's concurrent-dial/HashMap work assumes the coexistence render loop from A exists and is tested). Within Phase A, CLI localhost-matcher work is independent of the coexistence-runtime work and can be built in parallel per phase-a file ownership section.

## Risks (top, cross-phase)
- **BLOCKER** coexistence architecture inverts a recorded P9.2b decision — mitigated by dedicated Phase A with its own focus-barrier regression tests before any multi-target work starts.
- **BLOCKER** CLI contract break (`target: String` → `Vec<String>`) — mitigated by keeping Phase A's CLI surface additive-only (still single remote target) and confining the `Vec<String>` change to Phase B with the explicit "classic path unchanged" test inverted from `extract_remote_args_rejects_duplicate_values`.
- **HIGH** wall-clock: N serial dials — mitigated by Phase B's concurrent `FuturesUnordered` dial with a 25s batch budget test.

## Links
- Phase A: `plans/260721-1403-multi-remote-federated-workspace-launch/phase-a-local-remote-coexistence.md`
- Phase B: `plans/260721-1403-multi-remote-federated-workspace-launch/phase-b-multi-target-mounts.md`
