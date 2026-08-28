- [x] 1. worktree create — done 12:52 — MOVED to C:/Users/viete/hw-remote (zig cannot build from .claude/worktrees; see reports/windows-build-setup-260828-1311-toolchain.md)
- [x] 2. W1a preflight auth probe — done 13:05 — attach.rs warn_if_each_ssh_connection_will_prompt
- [x] 3. W1b remote-side shutdown wait — done 13:10 — attach.rs remote_server_shutdown_wait_script
- [x] 3b. toolchain setup — done 13:35 — rust 1.96.1, zig 0.15.2, VS BuildTools 2026
- [x] 3c. W1 committed — done 14:40 — c577945a; clippy gate clean, 5 new tests pass
- [ ] 4. W1c probe consolidation — DEFERRED (warm attach 6 -> 2 spawns; optimization, not the loop)
- [x] 5. W2 Phase 1 ungate events/actions/api/creation/app-state — done 15:10
- [x] 6. W2 Phase 2 session module + windows PTY backstop + keyboard-flag pair + attach.rs flip — done 15:25
- [x] 7. windows gate — done 15:30 — clippy --bin herdr -D warnings EXIT 0
- [x] 7b. federation unit tests on windows — done 15:52 — remote:: 176 passed / 0 failed
- [x] 7c. unix regression check — done on the real Ubuntu host — clippy -D warnings EXIT 0; 3803 passed, 1 pre-existing failure (same test fails on base c9ad7846)
- [x] 8. W2 Phase 3 e2e vs real host — done 16:10 — operator confirmed the federated attach works from Windows; see reports/e2e-260828-1502-phase3-results.md
- [x] 9. review -> ship-gate -> PR -> docs — done — two review rounds; PR #21 merged as ad7b4a81
- [x] 10. cut release v0.8.2-hvn.3 — done — published with all five assets

## Release note: flake-check blocked the automated publish

The Release workflow's `flake-check` job failed twice (~25 min apart) on crates.io
returning 403 to Nix's fetcher, fatally on `clap_complete-4.6.5`. All five platform
builds succeeded; `release` was skipped only because it declares
`needs: [build, flake-check, validate-release-inputs]`.

Published manually from run 33159438202's own artifacts, with the changelog body the
`release` job would have generated — same asset names, same body shape as v0.8.2-hvn.2.
Nothing was rebuilt.

The next `-hvn` tag will hit the same wall unless crates.io stops 403ing or
`flake-check` is skipped for fork tags, the way `close-released-issues` and
`update-latest-json` already are.

## Late finding — the password loop had a second, local cause

The reported loop survived key installation. Root cause: the generated private key
inherited an ACE for another account, so Win32-OpenSSH refused to load it, reported
`bad permissions`, and continued as if no key existed. Earlier "key auth works" checks
passed only because they ran through Git Bash's MSYS ssh, which ignores Windows ACLs;
herdr uses `C:\Windows\System32\OpenSSH\ssh.exe`, which does not.

Fixed locally with `icacls /inheritance:r /grant:r`. Fixed in the product by commit
02ba4e7f: the pre-attach warning now tells a skipped key apart from a host that accepts
no key, and advises restricting the key file instead of installing one that is already
installed.

## Scope decisions made during Phase 1/2
- Clipboard staging + image paste stay Unix-only (file_staging relies on 0600/0700 and POSIX
  separators). Their per-workspace purge CALL SITES are cfg-gated instead of dragging staging in.
- `FederationMountReady` / `FederationMountFailed` / `FederationMountEnded` stay Unix-only: their
  sole producer is `handle_workspace_mount_remote`, the daemon-owned MULTI-REMOTE mount API, which
  is out of scope. The single-remote federated session a Windows client runs never raises them.
- Windows-as-HOST remains unsupported (`remote.rs` stubs, `host_unix.rs`) — out of scope by design.
