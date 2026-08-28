- [x] 1. worktree create — done 12:52 — MOVED to C:/Users/viete/hw-remote (zig cannot build from .claude/worktrees; see reports/windows-build-setup-260828-1311-toolchain.md)
- [x] 2. W1a preflight auth probe — done 13:05 — attach.rs warn_if_each_ssh_connection_will_prompt
- [x] 3. W1b remote-side shutdown wait — done 13:10 — attach.rs remote_server_shutdown_wait_script
- [x] 3b. toolchain setup — done 13:35 — rust 1.96.1, zig 0.15.2, VS BuildTools 2026
- [x] 3c. W1 committed — done 14:40 — c577945a; clippy gate clean, 5 new tests pass
- [ ] 4. W1c probe consolidation — DEFERRED (warm attach 6 -> 2 spawns; optimization, not the loop)
- [x] 5. W2 Phase 1 ungate events/actions/api/creation/app-state — done 15:10
- [x] 6. W2 Phase 2 session module + windows PTY backstop + keyboard-flag pair + attach.rs flip — done 15:25
- [x] 7. windows gate — done 15:30 — clippy --bin herdr -D warnings EXIT 0
- [ ] 7b. federation unit tests on windows — running
- [ ] 7c. unix regression check (cross-target cargo check) — pending
- [ ] 8. W2 Phase 3 e2e vs real host — BLOCKED, awaiting ssh target from user
- [ ] 9. review -> ship-gate -> PR -> docs — pending

## Scope decisions made during Phase 1/2
- Clipboard staging + image paste stay Unix-only (file_staging relies on 0600/0700 and POSIX
  separators). Their per-workspace purge CALL SITES are cfg-gated instead of dragging staging in.
- `FederationMountReady` / `FederationMountFailed` / `FederationMountEnded` stay Unix-only: their
  sole producer is `handle_workspace_mount_remote`, the daemon-owned MULTI-REMOTE mount API, which
  is out of scope. The single-remote federated session a Windows client runs never raises them.
- Windows-as-HOST remains unsupported (`remote.rs` stubs, `host_unix.rs`) — out of scope by design.
