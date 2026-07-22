# Pipeline — TUI: add a remote workspace from inside a local herdr

Task: Add a first-class in-app affordance (TUI dialog + input wiring) to mount a remote
workspace from a running local herdr, over the existing `workspace.mount_remote` API path.
Task source: free text — "add a new feature to allow add a remote workspace inside a local herdr --auto"
Timestamp: 2026-07-22 19:13 (Australia/Melbourne)
Autonomy: --auto (all stop-and-ask gates auto-adjudicated + logged to reports/auto-decisions-260722-tui-add-remote-workspace.md)
Worktree: ../herdr-worktrees/tui-add-remote-workspace (branch feat/tui-add-remote-workspace, base master)

## Route card (verbatim, write-once)

Risk: medium — additive UI surface, but the collected target string feeds an `ssh` exec
(src/remote/unix.rs:125). Trust boundary + target parsing already exist and are exercised by the
CLI/socket path; this adds a collector, not a new boundary. `/hvn-security-scan` appended for the
input path.
Familiarity: high — 5 completed federation pipelines in plans/; protocol v3, mount/teardown,
snapshot resync all landed in this fork by the same session lineage.
Scope: feature — TUI dialog + input wiring + client→server call + tests + docs/next.
Payoff: medium — the fork maintainer currently must hand-write newline-JSON to herdr.sock to mount
a remote; no TUI or CLI surface references `mount_remote` (verified by grep over src/). A dialog
makes the already-shipped federation feature reachable by normal users.

Route (R5 — medium risk, no plan yet):
  1. /hvn-worktree — agent:git-manager
  2. /hvn:blindspot — parallel hvn-scout fan-out (dialog patterns, client/server boundary, target parsing)
  3. /hvn-predict — report saved to reports/
  4. /hvn-plan --tdd --parallel → plan validate (direction auto-adjudicated under --auto)
  5. /hvn:impl-notes init → /hvn-cook --auto --parallel → /hvn:impl-notes review
  6. /hvn-code-review || /hvn-security-scan (concurrent, same diff)
  7. /hvn:ship-gate → /hvn-ship → /hvn-review-pr --fix --reply

Skips: /hvn-brainstorm, red-team, /codex:adversarial-review — R5 row; scope concrete, backend
contract (`WorkspaceMountRemoteParams`) already exists and is protocol-frozen at v3.

## Project constraints carried into every stage

- Runtime/client boundary guardrail (CLAUDE.md): the mount request is a shared runtime fact and
  already lives in the server API. ONLY the collector/presentation belongs in the TUI layer. Do
  not add new shared behavior reachable solely through the private TUI client socket.
- External contributor guardrail: acting account is `vietairs`, not `ogulcancelik`. No upstream
  issues, no upstream PR. Any PR targets this fork's own master.
- Protocol: FEDERATION_PROTOCOL_VERSION is 3 and the handshake hard-rejects mismatch. This feature
  must NOT bump it — it reuses the existing method.
- Rust: no `unwrap()` in production code; `tracing` for logs; platform code compile-gated under
  src/platform/. Tests next to code; AppState/Workspace testable without PTYs.
- Local build: ZIG=~/.local/zig-0.15.2/zig required; `just`/nextest unavailable locally — use
  cargo test --test-threads=4; 3 pre-existing clippy errors are the accepted baseline.
