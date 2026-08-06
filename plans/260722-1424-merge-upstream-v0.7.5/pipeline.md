# Pipeline: merge upstream v0.7.5 into fork

Task: merge upstream (ogulcancelik/herdr) v0.7.5 into vietairs fork master, resolve conflicts, keep remote-workspace federation features functioning.
Task source: free text (cortex)
Date: 2026-07-22 14:24

Classification:
- Risk: HIGH — protocol/API schema, persisted snapshot, app state overlap (26 files both sides)
- Familiarity: medium — federation work documented in plans/; upstream 0.7.4→0.7.5 unknown
- Scope: multi-phase
- Payoff: high — upstream currency + federation preserved

Facts:
- merge-base 64de927; ours +94 commits, upstream +80 commits to v0.7.5
- overlap files: Cargo.{toml,lock}, src/api/{mod,schema,schema/response,server}.rs, src/app/{actions,agents,api,api/panes,api/worktrees,creation,input/navigate,mod,runtime,state}.rs, src/main.rs, src/pane.rs, src/pane/terminal.rs, src/persist/snapshot.rs, src/remote/unix.rs, src/server/{client_transport,headless}.rs, src/terminal/runtime.rs, src/ui/sidebar.rs, docs/next/api/herdr-api.schema.json

Route (confirmed: Proceed, merge strategy not rebase):
1. worktree + branch merge/upstream-v0.7.5
2. analyze upstream changes in overlap files
3. git merge v0.7.5, resolve conflicts (federation semantics + upstream structure)
4. protocol/integration version check
5. build + tests (ZIG=~/.local/zig-0.15.2/zig, cargo test --test-threads=4)
6. federation regression pass (mount_remote, pane mirroring, agent identity relay)
7. code review merge diff → land on master

Skips: blindspot/brainstorm/plan/predict — merge-driven task.
