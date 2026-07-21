# Phase B — multi-target CLI + concurrent mounts + HashMap mirror

Generalizes Phase A's local+1-remote coexistence to local+N-remote. CLI
`--remote` accepts space-separated targets; `localhost` = local workspace
(no SSH); concurrent dials with a 25s batch budget; per-target failure
isolation; `Option<RemoteMirror>` → `HashMap<HostKey, RemoteMirror>`.

**Architecture note (carried from Phase A rewrite, 2026-07-21):** mounts are
server-daemon-owned (CLAUDE.md runtime/client boundary guardrail). Phase B's
concurrent-dial/HashMap work happens **inside the server's** `WorkspaceMountRemote`
handler (`src/api/server.rs`, added in Phase A), not in `main.rs`. `main.rs`'s
part of Phase B is unchanged in kind from Phase A: parse N targets, filter
`localhost`, send N `workspace.mount_remote` API requests (or one request
carrying a target list — implementer's choice, keep CLI parse as previously
scoped), then attach as a client. The CLI multi-target *parsing* work
(`extract_remote_args`, `RemoteLaunch.target: Vec<String>`) is unaffected by
the server-side move and stays as originally scoped below.

## Context links
- `plans/260721-1403-multi-remote-federated-workspace-launch/reports/blindspot-scout-cli-parse-lifecycle-surface.md` §2, §6, §7
- `plans/260721-1403-multi-remote-federated-workspace-launch/reports/predict-260721-multi-remote-federated-workspace-launch.md` §Reliability, §Maintainer
- `src/remote/unix.rs:60-71` (`RemoteLaunch.target: String`), `:82-174` (`extract_remote_args`), `:112-129` (singular-target guards), `:293-378` (`attempt_federation_mount`, ~25s timeout at line ~347)
- `src/remote.rs:67,78` (Windows parity for the singular-target guard)
- `src/app/state.rs:1488-1559` (`remote_mirror` field + guard fns to convert to `HashMap<HostKey, RemoteMirror>`)
- Phase A's coexistence branch in `main.rs` (this phase generalizes it to N)

## Requirements
1. `RemoteLaunch.target: String` → `target: Vec<String>`. `extract_remote_args` accepts multiple space-separated values after `--remote`, terminated by the next flag or `--`.
2. `localhost` (exact string match — document precisely which strings match: `localhost` only, not `127.0.0.1`/hostname, per predict's "no v1 canonicalization" decision) is filtered out of the SSH-dial list and instead triggers the local autodetect path from Phase A. If `localhost` is the *only* target, behaves like today's local launch (no federation machinery invoked for it).
3. Remaining (non-localhost) targets get concurrent dials via `FuturesUnordered` (or equivalent), with a single 25s budget for the whole batch, not per-target serial timeouts stacking.
4. Per-target failure isolation: one target's `FederationMountFailure` produces a per-host sidebar notice (name the host) and does not abort siblings or the local workspace (reuses `unix.rs:226-247` per-target, called N times not once).
5. `AppState.remote_mirror: Option<RemoteMirror>` → `remote_mirrors: HashMap<HostKey, RemoteMirror>` **inside the server daemon's `AppState`** (Phase A already relocated the mount driver to run in the server process; this field-shape change happens in that same process, no client-side change). `begin_federation_mount`/`end_federation_mount`/`double_attach_conflict` updated to key on `HostKey` and support N entries instead of rejecting a second mount.
6. Classic single-target `--remote host` (no `--remote-workspace`) stays byte-for-byte unchanged: `extract_remote_args` still returns a launch usable by the old single-`run_remote` path (internally the `Vec` has one element; the classic dispatch branch is unaffected because it never sends `workspace.mount_remote` and never touches federation machinery).
7. Windows parity: `src/remote.rs:67,78` singular-target guards updated to match the new multi-value grammar.
8. Sidebar groups scale to N hosts using the existing per-host group primitive from P8 (`src/ui/sidebar.rs`) — no new grouping logic, just proven at N>1; rendered client-side from server-pushed state, same render path as Phase A.
9. `main.rs`'s coexistence branch (Phase A) generalizes from "1 target → 1 `workspace.mount_remote` request" to "N non-localhost targets → N requests (or one request with a target list, implementer's choice)" sent to the already-running local server before attaching as a client — no in-process concurrent-dial logic in `main.rs` itself; the concurrency (requirement 3) lives in the server handler.

## Files
- **Modify** `src/remote/unix.rs` — `RemoteLaunch.target` type change, `extract_remote_args` multi-value parse loop, remove/replace the singular-target guards at 112-129. `attempt_federation_mount` call site's concurrent-driver logic moves to the server handler (`src/api/server.rs`), not this file, since the driver already lives server-side after Phase A.
- **Modify** `src/remote.rs` — Windows parity for the same grammar change (lines ~67, ~78).
- **Modify** `src/api/schema/workspaces.rs` — `WorkspaceMountRemoteParams` (added in Phase A) gains a target-list shape if Phase B chooses one-request-N-targets over N-requests (implementer's choice per requirement 9).
- **Modify** `src/api/server.rs` — `WorkspaceMountRemote` handler (added in Phase A) becomes the concurrent per-target driver: `FuturesUnordered` dial, 25s batch budget, per-target failure isolation.
- **Modify** `src/app/state.rs` — `remote_mirror` → `remote_mirrors: HashMap<HostKey, RemoteMirror>` (server-daemon `AppState`); `begin_federation_mount` no longer returns `AlreadyMounted` for a *different* host, only for a literal duplicate `HostKey`; `double_attach_conflict` checks membership instead of single-value equality.
- **Modify** `src/main.rs` — coexistence branch from Phase A generalized: filter `localhost` from the CLI-parsed target list, send the remaining targets to the server via `workspace.mount_remote` (N requests or one list-request), then attach as a client. No concurrent-dial logic here — that's server-side.
- **Modify** `src/ui/sidebar.rs` — verify/extend per-host group rendering for N>1 (should be additive given P8's grouping key is already `HostKey`-derived); client-side render only, unaffected by the server-side handler change.
- **No new files** — HashMap conversion and concurrent-dial loop live in existing modules per YAGNI/KISS.

## TDD test list (write first)
1. `extract_remote_args_accepts_multiple_targets`: `--remote localhost host1 host2 --remote-workspace` → `RemoteLaunch.target == vec!["localhost","host1","host2"]`.
2. `extract_remote_args_single_target_classic_path_unchanged` — **explicit acceptance test**: `--remote host` (no `--remote-workspace`) parses to a `Vec` of len 1 AND the classic dispatch predicate (from Phase A test 3) still selects `run_remote`'s single-target path, proving byte-for-byte behavior preserved.
3. `extract_remote_args_localhost_only_no_ssh`: `--remote localhost --remote-workspace` → filtered target list for SSH dialing is empty; local-only launch predicate selected.
4. `extract_remote_args_rejects_empty_value_between_targets`: `--remote host1 --remote-workspace` (single, still valid) plus a malformed `--remote host1 -- host2` edge (value starting with `-` still rejected per existing `validate_remote_target`, `unix.rs:176-184`) — replaces the old duplicate-value rejection semantics.
5. `mount_hashmap_allows_two_distinct_hosts`: `begin_federation_mount` called twice with distinct `HostKey`s succeeds both times; `remote_mirrors.len() == 2`.
6. `mount_hashmap_rejects_duplicate_host_key`: `begin_federation_mount` called twice with the SAME `HostKey` → still returns `FederationMountConflict::AlreadyMounted` for that host (guard narrows, doesn't disappear).
7. `concurrent_dial_batch_budget_not_serial`: mock 3 targets each taking ~20s to "dial" (fake clock or bounded mock) → total batch completes within the 25s budget, not 60s+ (proves `FuturesUnordered`/concurrent join, not sequential awaits).
8. `partial_mount_failure_isolates_siblings`: 3 targets, 1 fails (`FederationMountFailure`) → assert the other 2 mount successfully into `remote_mirrors` AND local workspace unaffected AND exactly one sidebar notice references the failed host by name.
9. `sidebar_groups_scale_to_n_hosts`: with 2 entries in `remote_mirrors`, sidebar rendering produces 2 distinct per-host groups with correct badges (extends P8's single-host badge test to N=2).
10. `windows_remote_arg_parse_multi_target_parity`: `src/remote.rs` Windows-path parse test mirrors test 1 (run on the compile-gated Windows parse function; if not cross-compilable in CI, test the shared parsing logic directly per existing project convention for `src/platform/` testable-contract splitting).

## Tests to invert
- **Invert `extract_remote_args_rejects_duplicate_values` (`unix.rs:2808-2818`)**: old assertion was "second `--remote` value → error." New behavior: second+ value is accepted as an additional target. Rename/replace with test 1 above; do not leave the old assertion in the suite unmodified (it will fail by design).
- **Keep unmodified**: the 12 other `extract_remote_args` tests not about duplicate-value rejection or `--remote-workspace` flag detection (per scout §6, `unix.rs:2653-2676` flag tests) — these assert flag/keybinding parsing orthogonal to the target-count change.

## Implementation steps
1. Write failing tests 1-10 + the inverted test.
2. `RemoteLaunch.target: String → Vec<String>`; rewrite `extract_remote_args` multi-value loop (remove 112-129 singular guards, accept repeated bare values until next flag).
3. Add `is_local_target` matcher (`target == "localhost"`) filtering step before the SSH-dial list is built.
4. `AppState.remote_mirror → remote_mirrors: HashMap<HostKey, RemoteMirror>`; update the 3 guard fns.
5. Concurrent dial (server-side, in the `WorkspaceMountRemote` handler): wrap N `attempt_federation_mount` calls in `FuturesUnordered`, apply one 25s budget to the joined future set.
6. Per-target failure → per-host sidebar notice (loop the existing `unix.rs:226-247` handling per result).
7. Windows parity in `src/remote.rs`.
8. Sidebar N-host verification/extension.
9. Full suite green; `just check`.

## Validation commands
```
cargo test -p herdr remote:: app::state:: ui::sidebar:: --lib
just test
just check
```
Manual two-machine smoke (out-of-env, same convention as phase-09b — record result in `implementation-notes.md`, do not gate CI on it):
```
herdr --remote localhost 131.172.248.163 131.172.248.161 --remote-workspace
```
Verify: one TUI, local workspace + 2 remote host groups, kill one remote mid-session → sibling + local unaffected.

## Risks + rollback
- **BLOCKER (wall-clock):** naive per-target sequential dial reintroduces 75s startup for 3 hosts. Mitigation: test 7 enforces concurrency structurally, not just by inspection.
- **HIGH:** `HostKey` non-canonicalization (`localhost` vs `127.0.0.1` vs hostname) could let a spoofed classic `--remote 127.0.0.1` attach evade `double_attach_conflict`. Documented limitation per predict's explicit "no v1 canonicalization" — no code change requested beyond documenting in the CLI help/README note.
- **HIGH:** partial-failure sidebar UX regressing to "one stderr lump" for 3 targets. Mitigation: test 8 asserts distinct per-host attribution.
- **Rollback:** this phase's `Vec`/`HashMap` changes are additive supersets of Phase A's single-value shapes; reverting the phase's commit restores Phase A's local+1-remote behavior cleanly since Phase A's tests remain green throughout (CI gate).

## File ownership
Phase B owns: `src/remote/unix.rs` (target type + parse), `src/remote.rs` (Windows parity), `src/api/schema/workspaces.rs` + `src/api/server.rs` (concurrent-dial handler, extends Phase A's `WorkspaceMountRemote`), `src/app/state.rs` (HashMap conversion), `src/main.rs` (localhost-filter + N-request send, extends Phase A's branch), `src/ui/sidebar.rs` (N-host verification). No file is touched by both phases concurrently — Phase B starts only after Phase A merges (sequential dependency, not parallel).

## Unresolved questions
1. Exact 25s-budget mock strategy for test 7 — does the codebase have an existing fake-clock/mock-timeout harness for `tokio::time::timeout`, or does this need a small test-only seam added to `attempt_federation_mount`? Check `src/remote/unix.rs` test module before implementation.
2. Should `--remote-keybindings` apply globally across all remote targets or per-target? Scout flagged this as unresolved (§ "unresolved questions" 4); recommend global-only for v1 (YAGNI) unless user requires per-host — confirm before Phase B implementation starts.
