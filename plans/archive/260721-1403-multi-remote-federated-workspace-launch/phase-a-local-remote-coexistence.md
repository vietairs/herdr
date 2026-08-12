# Phase A — local + ONE remote coexist in a single App/TUI (server-side mount)

Reverses P9.2b (federated workspace rendered alone). Federation mount becomes
**server-daemon-owned state**, not client/TUI state — required by CLAUDE.md's
"Runtime/client boundary guardrail" (mount = shared runtime/session fact).
`main.rs`'s role: parse target → ensure local server running (existing
autodetect) → send the server a new `workspace.mount_remote` API command →
attach as a normal thin client. No in-process `AppState` sharing in `main.rs`;
the TUI never runs the federation driver itself.

**Architecture correction (2026-07-21 14:41, see implementation-notes.md):**
the original requirement 1 below ("main.rs starts LOCAL autodetect AND spawns
a background mount task feeding the same running app's shared AppState") is
infeasible — `auto_detect_launch` execs `herdr server` as a **separate OS
process** (`src/server/autodetect.rs:189-209`) and `main.rs`'s own process
becomes a thin socket render-client after that (`run_client_loop`,
`src/client/mod.rs:1271-1340`, no `App`/`AppState` construction). There is no
shared in-process `AppState` for `main.rs` to feed a background task into.
Requirements below are corrected to route the mount through the server
daemon's own process instead.

## Context links
- `plans/260721-1403-multi-remote-federated-workspace-launch/reports/predict-260721-multi-remote-federated-workspace-launch.md` §Architect, §Consensus
- `plans/260713-1217-herdr-remote-workspace-federation/phase-08-cli-federated-workspace.md`
- `plans/260721-1403-multi-remote-federated-workspace-launch/implementation-notes.md` — "Phase A — BLOCKED" + "unblock decision" entries (ground truth for why this phase file was rewritten)
- Project `CLAUDE.md` §"Runtime/client boundary guardrail" — mount = shared runtime/session fact → server state, not TUI state
- `src/remote/unix.rs:448-539` (`run_remote`, federation route dispatch — classic path, untouched)
- `src/remote/federation/session.rs:169` (`run_federated_session` — currently builds its own App; :182-235 dial+mount, :284-322 materialize+drive, :236 is where TTY/terminal-mode setup begins — confirms dial/mount/drive loop is separable from terminal ownership)
- `src/app/state.rs:1488-1559` (`remote_mirror: Option<RemoteMirror>`, `begin_federation_mount`, `end_federation_mount`, `double_attach_conflict` — this `AppState` now lives in the **server daemon's** process, not `main.rs`'s process)
- `src/server/autodetect.rs:189-209` (`spawn_server_daemon` — execs `herdr server` as a child process with its own PID), `:291-303` (`auto_detect_launch` ends in `crate::client::run_client()`)
- `src/client/mod.rs:1271-1340` (`run_client_loop` — socket-based blit render client, no `App`/`AppState`; confirms `main.rs`'s own process has nothing to feed a mount task into after autodetect)
- `src/api/schema.rs:65-66` (`Method::WorkspaceCreate(WorkspaceCreateParams)` — pattern to follow for the new mount command), `src/api/server.rs:285` (`handle_request`), `:319` (`Method::WorkspaceCreate(_) => "workspace.create"` dispatch arm — new `Method::WorkspaceMountRemote` variant follows this exact shape)
- `src/main.rs:693-716` (remote dispatch is mutually exclusive with local autodetect — still true; now the coexistence branch is "autodetect, then send server an API command," not "autodetect, then spawn an in-process task")

## Requirements
1. When `--remote-workspace` (or `HERDR_REMOTE_FEDERATION=1`) is set with exactly one `--remote <target>`, `main.rs` must: (a) ensure a local server is running via the existing `server::autodetect` machinery (reuse, do not fork a new server-start path), (b) send that server a new `workspace.mount_remote` JSON API request (`Method::WorkspaceMountRemote(WorkspaceMountRemoteParams)`, new variant in `src/api/schema.rs` following the `WorkspaceCreate` pattern at :65-66) carrying the remote target, then (c) attach as a normal thin client (`run_client_loop`/`run_client()`) — same terminal client path as any local session, no federation-aware code in the client. This replaces the original in-process background-task design.
2. Server-side: `handle_request` (`src/api/server.rs:285`, new arm alongside `:319`'s `WorkspaceCreate` match) dispatches `WorkspaceMountRemote` to a new handler that dials/mounts the remote target and drives it via a task **spawned inside the server daemon's own async runtime**, pushing updates into that daemon's `AppState.remote_mirror` via `begin_federation_mount`/reducer updates already defined in P4 (`src/remote/federation/reducer.rs`) — no new mirror-application logic, just relocate the driver from `run_federated_session`'s dedicated loop into a server-owned task.
3. Mount failure (SSH/timeout/unsupported) must degrade to a sidebar notice pushed through the server's existing event/render path to attached clients; local workspace (and the server daemon itself) keeps running unaffected (no process exit).
4. Focus barrier: input routing keyed on namespaced pane id must hold across local↔remote focus switches with zero cross-boundary keystroke leak (this is the risk carried over from P8 test 4, now exercised with local+remote *simultaneously present*, not fallback-only). This is a server-side `AppState`/reducer concern, not a client concern — the thin client only forwards input events.
5. Classic single-target `--remote` (no `--remote-workspace`) is untouched: still routes through `run_remote` → `run_client_process`/`SshStdioBridge`, never touches local autodetect, never sends `workspace.mount_remote`.
6. Teardown: server shutdown (or explicit unmount) tears down the mount's tunnel/ChildGuard without leaking a process; mount panic inside the server task must not crash the server daemon (reuse `session.rs:341-365` catch pattern, relocated into the server-owned task).
7. `PROTOCOL_VERSION` (`src/protocol/wire.rs:16`, currently 16) governs the **client↔server render/input wire** (`ClientMessage`/`ServerMessage`, `wire.rs:308`/`:599`), not the JSON API (`Method`/`Request`, `src/api/schema.rs`). The new `workspace.mount_remote` command is a `Method` addition (JSON API), so it does not, by itself, require a `PROTOCOL_VERSION` bump — confirm this distinction holds before implementation; if the mount also needs a new `ClientMessage`/`ServerMessage` wire variant (e.g., a push notification of mount state to the client), then apply the project's protocol-version bump rule (`CLAUDE.md` §Code Conventions: compare `PROTOCOL_VERSION` against the latest released tag, bump only if not already ahead). Do not decide the bump here — just note it.

## Files
- **Modify** `src/api/schema.rs` — add `Method::WorkspaceMountRemote(WorkspaceMountRemoteParams)` variant near `WorkspaceCreate` (:65-66 pattern).
- **Modify** `src/api/schema/workspaces.rs` — add `WorkspaceMountRemoteParams` struct (target host, remote-keybindings flag) near `WorkspaceCreateParams`.
- **Modify** `src/api/server.rs` — add dispatch arm in `handle_request` (near :319) and the handler that spawns the server-owned mount-dial-and-drive task.
- **Modify** `src/main.rs` (693-716 region) — coexistence branch becomes: ensure local server via existing autodetect, send `workspace.mount_remote` via the API client (`src/api/client.rs` pattern used by other CLI subcommands), then attach as a normal client. Else existing `remote_launch` branch unchanged.
- **Modify** `src/remote/federation/session.rs` — extract the mount-dial-and-drive logic from `run_federated_session` into a reusable function/task that does NOT own an `App` and does NOT assume terminal ownership; `run_federated_session` itself can stay for now as a thin caller (do not delete; Phase B decides its fate).
- **Modify** `src/app/state.rs` — no field shape change (still `Option<RemoteMirror>`); wire the server-owned mount task's producer side to call `begin_federation_mount`/`end_federation_mount` from the server daemon's async runtime context (this `AppState` instance already lives in the server process — no relocation of the type itself, only of who drives updates into it).
- **Modify** `src/ui/sidebar.rs` — confirm remote-origin group renders alongside local worktree groups in the same sidebar (P8 already built the badge/group primitive at 44-52 of phase-08; this phase proves it renders with local groups present, not alone) — rendered client-side from server-pushed state, unchanged from today's render path.
- **Create** none — this is a wiring/relocation phase, no new modules per YAGNI.

## TDD test list (write first)
1. `coexistence_local_and_remote_render_together` (state-level, `AppState::test_new()`, exercised as the server daemon would run it — no client/socket needed for this test): after a successful mount task completes, `AppState.workspaces` (local) and `AppState.remote_mirror` (remote) are both populated in the same instance — proves no more "federated-alone" branch.
2. `coexistence_mount_failure_keeps_local_session_alive`: mount task returns `Err`/`FederationMountFailure`; assert `AppState.workspaces` unchanged and a sidebar notice event fired; assert no process-exit path taken (test at the function boundary that decides the branch, not `std::process::exit`).
3. `coexistence_classic_remote_unaffected` — **explicit acceptance test**: `--remote host` with no `--remote-workspace` still parses to the pre-Phase-A `RemoteLaunch` and dispatch predicate (`main.rs` branch selection logic extracted to a testable function) selects the classic `run_remote` path, not the coexistence path (and never constructs a `workspace.mount_remote` request).
3b. `coexistence_dispatch_sends_mount_remote_request`: federation+single-target branch selection produces a `Method::WorkspaceMountRemote(...)` request value (test the request-construction function directly, not the socket round-trip) — proves `main.rs` sends the API command instead of spawning an in-process task.
4. `coexistence_focus_barrier_local_remote_rapid_switch` (carry over from P8 test 4, now with both present): buffered rapid focus switches between a local pane id and a federated pane id — each keystroke lands on the pane focused at send time.
5. `coexistence_teardown_no_leak`: dropping/ending the mount task's guard on local app quit tears down without local app blocking or panicking (mirrors `session.rs` `ChildGuard` drop test, relocated to the new call site).
6. `coexistence_mount_panic_isolated`: mount task panic is caught (reuse the `session.rs:341-365` catch pattern) and surfaces as a mount-failure notice, not a crash of the whole process (assert via the isolation wrapper's `Result`, not by triggering an actual `panic!` in CI if the existing pattern already has a unit-testable seam — otherwise assert the wrapper type signature forces `catch_unwind`).

## Tests to invert/keep from existing suite
- Keep: all 14 `extract_remote_args` tests unchanged (CLI parse untouched in Phase A — `target: String` stays singular).
- Keep: `extract_remote_args_rejects_duplicate_values` (`unix.rs:2808-2818`) — Phase A does not touch multi-value parsing; this stays green as-is.
- No P8 tests need inversion in Phase A; P8's "federated-alone" assumption was implicit in `run_federated_session` building its own App, not asserted by name in an existing test — confirm via `rg "run_federated_session" src -n` before starting and note any test that asserts single-App behavior by construction (none currently found in `session.rs`'s own test module per scout).

## Implementation steps
1. Write failing tests 1-6 (incl. 3b) against testable functions: the "coexistence vs classic vs local-only" branch selection, and the `workspace.mount_remote` request-construction function — both pure, extracted out of `main()` into `src/remote/mod.rs` or a `main.rs` test module (`main()` itself is not unit-testable).
2. Add `Method::WorkspaceMountRemote`/`WorkspaceMountRemoteParams` (`src/api/schema.rs`, `src/api/schema/workspaces.rs`) and the `handle_request` dispatch arm (`src/api/server.rs`).
3. Extract mount-dial-and-drive logic from `run_federated_session` into a function returning a spawnable task + handle (no `App`, no terminal ownership assumed inside it).
4. Wire the server handler: on `WorkspaceMountRemote`, spawn the mount task inside the server daemon's own async runtime, wire mount events into the server's `AppState` via existing reducer/mirror API.
5. Wire `main.rs` coexistence branch: ensure local server via existing autodetect, send `workspace.mount_remote` via the API client, then attach as a normal client.
6. Wire sidebar failure notice for mount failure (reuse `FederationMountFailure`, `unix.rs:226-247`), delivered to clients through the existing server push/render path.
7. Run focus-barrier and teardown/panic-isolation tests; fix any race in event ordering.
8. Full suite green; `just check`.

## Validation commands
```
cargo test -p herdr app::state:: remote::federation:: api::server:: api::schema:: main --lib
just test
just check
```

## Risks + rollback
- **Risk (BLOCKER):** relocating the mount driver out of `run_federated_session` regresses the single-target federated path's existing P8/P9 tests. Mitigation: keep `run_federated_session` callable standalone (used nowhere new yet) until Phase B; only add the new server-handler call site.
- **Risk:** async task lifetime vs server daemon runtime shutdown ordering causes a hang on server stop. Mitigation: test 5 equivalent at server level, explicit `ChildGuard`-equivalent drop ordering inside the server task.
- **Risk:** new `workspace.mount_remote` JSON API command needs auth/socket-permission parity with existing `Method` variants (e.g. `workspace.create`) — verify it goes through the same request validation path in `handle_request`, no bespoke bypass.
- **Rollback:** revert the `main.rs` coexistence branch + the new `Method`/handler; classic and existing federated-alone paths are untouched, so reverting this phase's commit fully restores prior behavior.

## File ownership
Phase A owns: `src/main.rs` (coexistence branch only), `src/api/schema.rs` + `src/api/schema/workspaces.rs` (new `Method` variant + params), `src/api/server.rs` (new dispatch arm + handler), `src/remote/federation/session.rs` (extraction), `src/app/state.rs` (wiring only, no field shape change), `src/ui/sidebar.rs` (verification only, no new code expected). No overlap with Phase B's `Option→HashMap` change since Phase A doesn't touch the mirror's type.

## Unresolved questions
1. Does `run_federated_session`'s existing terminal-mode entry point block cleanly when reused as a server-owned background task, or does it assume it owns the terminal? Needs a read of `session.rs:169-260` before step 3 — flag as spike if terminal-mode ownership conflicts. (Partially answered: `session.rs:236` is confirmed as where TTY setup begins, and `:182-235`/`:284-322` are confirmed decoupled from it — but full task-reuse semantics inside a server async runtime, not a CLI process, still need verification.)
2. Does the new `workspace.mount_remote` command need a dedicated `ServerMessage`/`ClientMessage` wire push (`src/protocol/wire.rs`) to notify already-attached clients of mount-state changes in real time, or does the existing render/event push path already cover `AppState.remote_mirror` changes generically? If a new wire variant is needed, apply the `PROTOCOL_VERSION` bump rule from `CLAUDE.md`.
