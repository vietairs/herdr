# Phase 3 e2e runbook — Windows client -> Ubuntu host

Target: `hvnguyen@131.172.248.163` (credential held by the operator; never recorded here).
Remote: `SSH-2.0-OpenSSH_9.6p1 Ubuntu-3ubuntu13.18` — a unix host, i.e. the supported
federation HOST side. Client: this Windows 11 laptop.

## Why these steps are manual

`ssh` reads a password from the controlling TTY, not stdin, and the agent shell runs with stdin
detached. No `sshpass`/`plink` is installed and no local key exists, so nothing can authenticate
non-interactively. herdr is also a full-screen TUI, so it needs a real terminal (Windows
Terminal), not an embedded one.

## Already verified without auth

- Host reachable; rejects with `Permission denied (publickey,password)`.
- That exact string is what `reports_ssh_auth_failure` matches, so the W1a warning fires on this
  host. Confirms the detector against a real server, not only a unit test.

## Binaries

- BEFORE: `%LOCALAPPDATA%\Programs\herdr-hvn\herdr.exe` (installed, reports 0.8.2)
- AFTER:  `C:\Users\viete\hw-remote\` + `target\release\herdr.exe` (this branch)

## Test A — baseline prompt count (BEFORE)

Run the installed binary with `--remote hvnguyen@131.172.248.163`.
Count how many times it asks for the password before the TUI appears. Record the number.
Expect several, with no explanation of why.

## Test B — same attach on this branch (AFTER)

Run the freshly built binary with the same arguments.

Expect, BEFORE the first prompt, three lines saying ssh on this platform cannot share one
authentication between connections, that this host accepts no key/agent identity, and how to set
up key auth. Then count the prompts again.

Honest expectation: W1a explains the prompting and W1b removes the pathological case (the
shutdown-confirmation wait, which polled with a fresh ssh connection every 100ms for up to five
seconds — up to ~50 prompts, and only on the server-restart path). The ordinary probe sequence is
still several connections; collapsing those to one is W1c, which is deferred. So if Test A and
Test B show the SAME count, that is the expected result of what has landed so far, not a failure —
and it is the measurement that decides whether W1c is worth doing now.

## Test C — federation workspace on Windows (the port)

Run the branch binary with `--remote hvnguyen@131.172.248.163 --remote-workspace`.

BEFORE this branch, a Windows client printed
`federated sessions are not supported on this platform` and silently fell back to the classic
full-screen attach. That message and that fallback are deleted.

Expect now: the federated session actually starts and renders the remote workspace mirror, or it
fails with a specific reason (dial/mount/timeout) and falls back citing THAT reason. Either is
informative; the old blanket "not supported" is the thing that must be gone.

Check while attached:
1. Remote panes render and update.
2. Keyboard input reaches remote panes — the console-parity risk, since keyboard enhancement
   flags are a no-op on Windows, mirroring `main.rs`'s classic pair.
3. Mouse click/drag/split behave.
4. Quitting restores the terminal cleanly (no stuck alternate screen, no dead mouse reporting).
5. Local pane spawn is refused during the federated session — the backstop newly added to
   `src/pty/backend.rs`, which the Windows backend never had.

## Optional — make future runs non-interactive

Installing a key would let the agent drive e2e without a human. It writes to the remote
`~/.ssh/authorized_keys`, so it needs an explicit go-ahead first:

    ssh-keygen -t ed25519 -f $env:USERPROFILE\.ssh\id_ed25519 -N '""'
    type $env:USERPROFILE\.ssh\id_ed25519.pub | ssh hvnguyen@131.172.248.163 "mkdir -p ~/.ssh && cat >> ~/.ssh/authorized_keys"

The password path stays reproducible afterwards by adding `PreferredAuthentications password`
for this host in `~/.ssh/config` — herdr's managed ssh config includes the user config first.

## Unresolved

- Whether the remote already has a herdr binary installed, and at what protocol version. Not
  checkable without auth; a first attach may trigger the install path (more connections, more
  prompts) and possibly the server-restart path that W1b fixes.
