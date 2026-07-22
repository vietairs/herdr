# Pipeline — remote-workspace paste (images + files)

Task: add paste of images and files into panes belonging to a MOUNTED REMOTE WORKSPACE
(federation), so the pasted artifact lands on the remote host rather than as a dead local path.
Task source: free text (user), `--semi-auto`
Created: 2026-07-22 16:24 (Australia/Melbourne)
Worktree: /Users/hvnguyen/Projects/herdr-worktrees/remote-workspace-paste-image-files
Branch: feat/remote-workspace-paste-image-files (base 5ec2a10b)

## Route card (verbatim, write-once)

```
ROUTE CARD — paste images/files into panes on mounted remote workspaces
Task source: free text (--semi-auto)
Risk: HIGH — new federation message transfers arbitrary file bytes and WRITES FILES on a remote
      host (path-traversal + DoS surface; cf. existing sanitize_extension/0600/0700 hardening in
      src/server/clipboard_image.rs); touches the server/client wire contract (wire.rs:16 v17)
Familiarity: HIGH — 6 completed federation pipelines in plans/; user authored the fork
      (b5cb8ce8, 29fe7b64, 31880d62)
Scope: feature — protocol/ + remote/federation/{client,serve,pane_source} + server/clipboard_image
      + client capture; 4-6 modules
Payoff: HIGH — user must currently drop out of herdr entirely and use `cmux ssh appn-ltu-vm-105`
      to get an image to a remote agent (user statement)
Route (R7 — HIGH risk):
   0. /hvn-worktree — agent:hvn-git-manager
   1. /hvn:blindspot --deep — parallel hvn-scout fan-out
   2. /hvn-brainstorm --html -> /hvn-preview --html -> Artifact  ** HARD STOP: design approval **
   3. /hvn-predict (report saved for ship-gate)
   4. /hvn-plan --tdd -> red-team -> validate       ** HARD STOP: direction confirm **
   5. codex adversarial-review <plan> — gpt-5.6-sol xhigh
   6. /hvn:impl-notes init -> /hvn-cook (no --auto) -> /hvn:impl-notes review
   7. /hvn-code-review || /hvn-security-scan (parallel) + codex adversarial-review <diff>
   8. /hvn:ship-gate --hard
   9. /hvn-ship -> /hvn-review-pr --fix --reply     ** HARD STOP: before-merge approval **
Skips: nothing — --semi-auto skips only the route-card confirm and minor confirmations
Teardown (announced): after user merges -> git pull base -> remove local worktree -> plan-gc archive.
      Remote branch is KEPT (never deleted).
```

## Pre-classification evidence (intake scouting)

- `src/client/mod.rs:1443` — client reads local clipboard image, sends `ClientMessage::ClipboardImage`.
- `src/protocol/wire.rs:336` — `ClipboardImage { extension, data }`; `PROTOCOL_VERSION = 17` (wire.rs:16).
- `src/server/headless.rs:1727` — server calls `clipboard_image::stage(client_id, extension, data)`.
- `src/server/clipboard_image.rs` — stages to LOCAL `$TMPDIR/herdr-clipboard-images-<uid>/`,
  0600 file / 0700 dir, extension allowlist; pastes the LOCAL absolute path as text.
- `src/remote/federation/pane_source.rs:142` — pane input crosses federation as
  `TerminalChannelMessage::Input`; the PTY lives on the remote host.
- Root cause: a federated pane receives a path that only exists on the client host.
- `src/remote/federation/protocol/negotiate.rs:15-30` — capability negotiation is ADDITIVE
  (unknown capabilities dropped, never fatal); only `federation_protocol_version` mismatch rejects.
  => a new staging capability can ship WITHOUT a `FEDERATION_PROTOCOL_VERSION` bump.
- No generic (non-image) file paste exists today — "paste files" is net-new behavior.
