# Phase 3 e2e results — Windows client -> Ubuntu host

Host: `hvnguyen@131.172.248.163`, `Linux bio-1-ubuntu 6.8.0-137-generic x86_64`,
OpenSSH 9.6p1. Client: Windows 11, branch binary built from
`fix/remote-ssh-auth-loop-windows-federation`.

## Setup

Key auth installed with the operator's approval (ed25519, `herdr-e2e-windows`,
`SHA256:F7Aq1fjDK1ymW4rhLBq+cKL48R2blswfwj13mxCtogA`), appended idempotently to the remote
`~/.ssh/authorized_keys`. Password never written to disk or logs.

## Proven

1. **W1a detector, against a real server.** The host rejects with
   `Permission denied (publickey,password)` — the exact shape `reports_ssh_auth_failure` matches,
   so the warning fires here. After key installation the `BatchMode` probe SUCCEEDS, so the
   warning correctly goes silent. Both directions of the branch confirmed live.

2. **W1b, against a real server.** The generated remote-side wait script ran the full 5-attempt
   poll in 4 seconds over **one** ssh connection and returned parseable status JSON
   (`"running":true` -> the caller's "still responding" path). Before this change the same wait
   was one ssh connection per 100ms poll — up to ~50 connections, and with password auth, ~50
   prompts.

3. **The port is live.** `--remote-workspace` from Windows no longer prints
   `federated sessions are not supported on this platform`. That message and its silent
   degrade-to-classic path are gone; Windows now executes the real federation code.

4. **The federation handshake succeeds Windows -> Linux.** The observed failure is
   `link closed before a MountSnapshot arrived`, which in `client.rs:356` is reachable ONLY after
   the client has already received `HandshakeResponse::Accept` (`client.rs:322`). Frame codec,
   protocol version negotiation and capability agreement therefore all work cross-platform. This
   is the substantive part of the port working end to end against a real host.

## Not proven — the mount fails

The remote accepts the handshake, then closes before sending the snapshot.

Ruled out by direct probing:
- Remote `federation-serve` exists and behaves correctly: it waits silently for the client's
  handshake (client speaks first) and exits 0 cleanly on EOF. Not an unknown-subcommand case
  (unknown commands exit 2 with `unknown command`).
- Remote session `default` is running with real workspaces, so there is content to snapshot.
- No panic, no federation error in `~/.config/herdr/herdr-server.log`; its last federation entry
  predates the tests.

**Unresolved cause.** The remote binary is an older fork build (installed Aug 24) while the client
is this branch. Client/host build skew is the leading explanation, but it is NOT established —
a version mismatch should produce `HandshakeResponse::Reject`, not a silent close, so something
else may be happening on the host side between Accept and `host.mount()`.

The next step that would settle it is running client and host on the **same** build: install this
branch's Linux binary on the host and repeat. That requires a Linux build (CI artifact, or a
cross-build), which was not attempted here.

## Not tested — needs a human at a real terminal

`ssh` reads passwords from the TTY and herdr is a full-screen TUI; the agent shell has no TTY, so
the interactive half of Test C never ran. Still unverified on Windows:
pane rendering, keyboard input reaching remote panes (the console-parity risk, since keyboard
enhancement flags are a no-op there), mouse click/drag/split, clean terminal restore on quit, and
the new `src/pty/backend.rs` local-spawn backstop actually refusing a spawn.

Tests A and B (password-prompt counts, before vs after) also still need a human — key auth is now
installed, so reproducing the password path needs `PreferredAuthentications password` for this
host in `~/.ssh/config`.

## Side effect requiring cleanup

Each Test C run fell back to the classic attach, connected as a client, and **created a workspace
with a spawned pane** on the live remote session. Two were created:

    wC  iPEMS_Webapp  panes=1   (05:14:01Z)
    wD  iPEMS_Webapp  panes=1   (05:15:17Z)

Workspace count went 4 -> 6. These are artifacts of the test, not pre-existing. Left in place
pending the operator's decision — removing workspaces on a live server is theirs to authorize.

## Unresolved questions

- Why does the remote close after Accept? Needs matched builds on both ends.
- Should the classic-attach fallback create a workspace when the client has no TTY and is about to
  fail? Two throwaway workspaces from two failed attaches looks like a real wart, though it is
  pre-existing behaviour and out of this branch's scope.
