# Diagnosis — `--remote` SSH password loop + Windows federation workspace

Date: 2026-08-28 · Branch: master · Machine: Windows 11 laptop (`viete`)

## W1 — `--remote` re-prompts for the SSH password (root cause PROVEN)

Not a retry loop on a wrong password. Each correct password advances to the **next
`ssh` process**, and one attach spawns many.

`run_remote` (`src/remote/attach.rs:629`) spawns a separate `ssh` per step:

| step | site |
|---|---|
| remote platform probe (`uname -s`/`-m`) | `attach.rs:1405` |
| known-binary candidate probe | `attach.rs:1433` |
| `command -v herdr` | `attach.rs:1510`, `:1520` |
| server-ready / status / restart checks | `attach.rs:1565`, `:1582`, `:1911`, `:2050`, `:2064`, `:2102` |
| install prepare / stream / commit (first run only) | `attach.rs:1183`, `:1190`, `:1213` |
| federation snapshot dial | `attach.rs:465` |
| stdio bridge (long-lived) | `attach.rs:2510` (unix) / `:2565` (windows) |

On **unix** these share ONE authentication: `src/platform/unix_common.rs:28` sets
`multiplexing: true`, so `apply_managed_ssh_options` (`attach.rs:1299`) adds
`-S <ctl> -o ControlMaster=auto -o ControlPersist=yes` and every later spawn rides the
existing control socket.

On **Windows** `src/platform/windows.rs:120` sets `multiplexing: false`, because
Win32-OpenSSH does not implement connection multiplexing. Verified on this machine:
`C:\WINDOWS\System32\OpenSSH\ssh.exe` — `OpenSSH_for_Windows_9.5p2`. `ssh -G` echoes
`controlmaster auto` / `controlpath …` (the options parse) but no mux is performed.

Result: with key auth unavailable, every one of the ~8-10 spawns performs a full
password authentication. That is the reported loop.

Existing mitigation is documentation only — the passphrase/ssh-agent hint at
`src/remote.rs:29-33`, printed only after an auth *failure*.

## W2 — federation workspace on Windows: unimplemented, not broken

`herdr --remote --remote-workspace` (or `HERDR_REMOTE_FEDERATION=1`,
`attach.rs:67`, `:92`) never reaches the federated path on Windows.
`attach.rs:696` gates `run_federated_session` behind `#[cfg(unix)]`; the
`#[cfg(not(unix))]` arm prints "federated sessions are not supported on this
platform" and silently degrades to the classic full-screen attach.

Host side is stubbed outright — `src/remote.rs:11-24`:
`run_remote_client_bridge` and `run_federation_serve_bridge` both return
"not supported on Windows yet".

Unix gating on the federation surface (count of `cfg(unix)` per file):
`src/app/creation.rs` 82 · `src/client/mod.rs` 51 · `src/server/headless.rs` 50 ·
`src/app/api/workspaces.rs` 45 · `src/pane.rs` 42 · `src/server/handoff.rs` 37 ·
`src/remote/attach.rs` 33 · `src/events.rs` 32 · `src/remote/federation/client.rs` 28 ·
`src/app/actions.rs` 24. `src/remote/host_unix.rs` is built directly on
`std::os::unix::net::UnixStream`.

Mitigating asset: `src/ipc.rs` already provides a cross-platform `LocalStream`
(interprocess crate), used today by the Windows stdio-bridge path.

**This is a port, not a fix.** "Make sure the federation workspace is working in the
Windows build" cannot be satisfied by verification — the capability is absent by
construction.

## Local e2e blockers (this laptop)

1. **No Rust toolchain.** `cargo` and `rustc` are not installed / not on PATH
   (checked via PowerShell `Get-Command`). No fix can be built or e2e-tested here
   without either installing Rust or building through the fork's CI release job.
2. **No SSH remote target.** `%USERPROFILE%\.ssh\config` does not exist, so no host is
   configured for an e2e `--remote` run. A target must be supplied.
3. Installed fork binary: `C:\Users\viete\AppData\Local\Programs\herdr-hvn\herdr.exe`,
   reports `herdr 0.8.2`, dated 2026-08-26. Usable to *reproduce* W1; not to verify a fix.

## Addendum — verified after the advisory pass

- **The literal loop**: `wait_for_remote_server_shutdown` (`attach.rs:2076`) polls
  `remote_server_status` every `REMOTE_SERVER_SHUTDOWN_POLL_INTERVAL` (100ms) until
  `REMOTE_SERVER_SHUTDOWN_CONFIRM_TIMEOUT` (5s). Each poll is a fresh `ssh` spawn →
  up to ~50 password prompts from one call on Windows. Reached via
  `stop_remote_server` on the protocol/version-mismatch restart path.
- **The federation tunnel can never share auth**: `dial_federation` (`attach.rs:583`)
  forces `-S none` by design — `RemoteSsh`'s `Drop` runs `ssh -O exit`, which would
  tear down a multiplexed live tunnel. True on unix too.
- **Windows pays for federation it cannot use**: `attempt_federation_mount`
  (`attach.rs:452`) is NOT cfg-gated. Windows runs the full snapshot dial+mount
  (one more prompt) before the `#[cfg(not(unix))]` arm at `:720` prints
  "not supported" and falls back.

## Recommended sequencing (advisory counsel, `--advise`)

W1: (1) preflight `ssh -o BatchMode=yes -T <target> true` on platforms where
`remote_ssh_config_paths().multiplexing == false` — success proceeds unchanged with
zero prompts, auth-denied prints ONE actionable key-auth message instead of N blind
prompts; (2) move the shutdown poll loop into the remote script so it costs one
connection; (3) consolidate the probe `sh_output` calls into one tagged script.
Rejected: BatchMode on the real probes (breaks passphrase-without-agent users on
unix, and makes password hosts unattachable); a herdr-owned persistent ssh channel
(reimplements ControlMaster; floor is still 2+ prompts because of `-S none`).

W2: v1 scope = Windows **client** → single unix host. Phase 0 (hours): decide the
federation route BEFORE the snapshot dial via a platform capability input to
`decide_federation_route` (`attach.rs:429`, already pure + unit-tested). Phases 1-3
(~1-2 weeks): ungate events/actions/client, then `federation::session` +
`App::new_federated`, add the Windows PTY spawn backstop, e2e. Out of scope:
multi-remote daemon mounts, Windows-as-host (`host_unix.rs`,
`run_federation_serve_bridge`, `file_staging`).
Hardest blocker: cfg-fanout blast radius — an ungating mistake silently changes the
CLASSIC Windows build.
