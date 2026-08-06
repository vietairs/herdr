# Root cause: remote (federated) pane OSC 52 clipboard write never reaches local macOS clipboard

Date: 2026-07-28
Scope: read-only investigation, no code changed.

## Summary

CONFIRMED. The in-app OSC 52 clipboard write (e.g. Claude Code's "Copied" via `\x1b]52;c;<base64>\x07`)
running inside a federated/mounted remote pane on `appn-ltu-vm-100`/`-105` IS detected and reconstructed
correctly on the local Mac (the mirrored pane's `Osc52Forwarder` fires, same as for a local pane), but the
resulting `ClipboardMessage` is sent into an `mpsc::unbounded_channel` whose receiver is immediately dropped
at the real, live TUI mount call site. The message is silently discarded before it ever reaches
`platform::write_clipboard`. There is no bug in the byte-level detection, alt-screen handling, or the wire
transport of raw PTY bytes — the break is a genuinely dead/no-op consumer wired into an otherwise-complete
pipeline (`clipboard_tx.send(...)` succeeds at the `mpsc` layer even though nothing is listening; the
scaffolding intentionally documents this as a stubbed-out v1).

Mouse-selection copy from a remote pane works via a completely separate code path
(`src/selection.rs:327` → `crate::platform::write_clipboard`) that copies the locally-rendered mirrored
grid text directly — it never touches the federation clipboard channel at all, which is exactly why it is
unaffected by this gap.

## Local path (pane whose PTY is a real local child) — file:line

1. PTY bytes arrive and are handed to `GhosttyPaneTerminal::process_pty_bytes`
   (`src/pane/terminal.rs:1036`).
2. `core.osc52_forwarder.observe(bytes)` / `drain_pending()`
   (`src/pane/terminal.rs:1067-1068`) reconstructs any complete OSC 52 set sequence via the byte-level
   state machine `Osc52Forwarder` (`src/pane/osc.rs:397-460`), independent of alt-screen state — this
   tracker runs on every byte unconditionally, before/alongside the `filtered_bytes`/alt-screen gating
   further down (`src/pane/terminal.rs:1082-1098`), so alt-screen apps like Claude Code are not filtered
   out here.
3. `ProcessBytesResult.clipboard_writes` carries the decoded payload back to the pane's `on_read` closure.
4. For a **local** pane, that closure (`src/pane.rs:1952`, `:2194`, `:2367`, three call sites for the
   different local-runtime read loops) forwards each write into `AppEvent::ClipboardWrite`, which
   eventually reaches `crate::platform::write_clipboard` (`src/platform/macos.rs:537`).

## Remote path and where it breaks — file:line

1. A mounted/federated pane is a `PaneRuntime::spawn_remote` runtime (`src/pane.rs:1864`), NOT backed by a
   local child process. Its `on_read` closure (`src/pane.rs:1921-1964`) is fed by
   `RemoteTerminalSourceHandle` from bytes arriving over the federation tunnel
   (`remote::federation::pane_source`), and — critically — it calls the **exact same**
   `terminal.process_pty_bytes(...)` (`src/pane.rs:1923`) as the local path, so the same
   `Osc52Forwarder` (`src/pane/osc.rs:397`) fires on the mirrored/local ghostty emulator instance that
   renders the remote pane. This step is confirmed working: OSC 52 detection is not remote-host-specific
   or alt-screen-gated.
2. `src/remote/federation/sanitize.rs:10-13` explicitly documents that raw PTY bytes destined for the
   ghostty pane emulator are NOT stripped of control sequences (only *chrome* strings — workspace/tab/pane
   labels — are sanitized), so the OSC 52 sequence survives the wire transport intact. Not the break point.
3. Per RT-F7 comment (`src/pane.rs:1945-1957`), a **remote-origin** clipboard write is intentionally routed
   differently from a local one: instead of `AppEvent::ClipboardWrite`, it is sent as a
   `ClipboardMessage { origin_tag: "remote", payload }` on an `UnboundedSender<ClipboardMessage>`
   (`clipboard_tx`) that was threaded in at `spawn_remote` construction time.
4. That `clipboard_tx` is `outbound_clip_tx`, created at the **one real, live production mount call site**,
   `App::handle_federation_mount_ready` (`src/app/api/workspaces.rs:169`, handler for
   `AppEvent::FederationMountReady`, the actual TUI "mount remote workspace" flow):
   ```
   let (outbound_clip_tx, _outbound_clip_rx) = tokio::sync::mpsc::unbounded_channel::<
       crate::remote::federation::protocol::ClipboardMessage,
   >();
   ```
   (`src/app/api/workspaces.rs:199-201`). The receiver is bound to `_outbound_clip_rx` — underscore-named,
   never read, and dropped when it falls out of scope at the end of `handle_federation_mount_ready`
   (function returns at `src/app/api/workspaces.rs:236` onward; nothing captures or forwards the receiver
   into the spawned `drive_handle` task at `:267`).
5. The function that *would* consume this receiver and actually apply the write —
   `apply_remote_clipboard_writes` (`src/remote/federation/pane_source.rs:251`) — exists and is fully
   implemented (drains `ClipboardMessage`s and applies the same clipboard policy as a local write, per its
   doc comment at `pane_source.rs:223-245`), but it is **only ever called from unit tests**
   (`pane_source.rs:467`, `:508`). `grep -rn "apply_remote_clipboard_writes"` across `src/` returns no
   non-test call site.
6. Net effect: `clipboard_tx.send(ClipboardMessage{...})` at `src/pane.rs:1953` succeeds at the channel
   layer (an `mpsc::UnboundedSender::send` only errs if every receiver is dropped, and the receiver here is
   dropped only *after* `handle_federation_mount_ready` returns, i.e., well before any pane bytes are
   processed — so the send actually returns `Err` once the per-pane spawn's `on_read` first fires after
   mount, and the surrounding code discards that error with `let _ =`). Either way, no code downstream ever
   receives the message, so it never reaches `crate::platform::write_clipboard`. This is a complete,
   silent drop with no error surfaced to the user or logs.
7. The other call site that wires the same channels, `run_federated_session` in
   `src/remote/federation/session.rs:308-329` (the "classic full-screen `--remote` session" alternative
   entry point), has the identical stub pattern (`session.rs:315-322`: *"outbound ... its rx has no live
   forwarder in v1 (held/dropped)"*) — but that whole module is explicitly dead code
   (`#![allow(dead_code)]` at `session.rs:27`, doc comment *"Dormant until b3: `run_federated_session` has
   no live caller yet"* at `session.rs:23-26`), so it is not even in play for the reported bug; the live
   path is exclusively `app/api/workspaces.rs`.

### Why plain-shell OSC 52 "works" (mouse-selection copy) but Claude Code's does not

These are not the same feature and do not share the broken pipeline:

- Mouse-selection copy calls `crate::selection::copy_selection`-style logic
  (`src/selection.rs:320-327`), which reads the **already-locally-rendered** mirrored terminal grid text
  (built by the same ghostty emulator instance from step 1 above) and calls
  `crate::platform::write_clipboard(bytes)` (`src/platform/macos.rs:537`) **directly** — no
  `ClipboardMessage`/federation channel involved at all. It is unaffected by the dropped-receiver bug.
- Claude Code's own in-band OSC 52 emission is the *only* thing that goes through the
  broken `outbound_clip_tx` → dropped-receiver path described above.

## Ranked hypotheses

1. **CONFIRMED — dead consumer for remote-origin clipboard messages.** The `outbound_clip_tx` receiver
   created in the live mount handler (`src/app/api/workspaces.rs:199-201`) is never drained; the only
   consumer that could drain it (`apply_remote_clipboard_writes`, `pane_source.rs:251`) is wired only in
   tests. Confidence: high (direct grep + read confirms zero non-test callers, and the surrounding comments
   self-document this as intentional "v1 bytes-only, dormant" scaffolding, not an accident hidden by
   refactor).
2. **PLAUSIBLE, but eliminated as primary cause — alt-screen gating.** Investigated because Claude Code runs
   in the alternate screen and a plain shell does not. Eliminated: `Osc52Forwarder::observe` runs
   unconditionally on every byte before the `alternate_screen` check that only gates
   `maybe_filter_primary_screen_scrollback_clear` (`src/pane/terminal.rs:1082-1098`). OSC 52 detection is
   identical for alt-screen and primary-screen content.
3. **PLAUSIBLE, but eliminated — wire-level stripping of OSC 52 on the federation transport.** Investigated
   because this fork previously found and fixed a missing pane-close wire message (protocol v3→v4).
   Eliminated by `src/remote/federation/sanitize.rs:10-13`, which explicitly excludes raw PTY bytes bound
   for the pane emulator from sanitization — only chrome/label strings are stripped of control bytes.
4. **PLAUSIBLE, mostly eliminated — multiple near-duplicate `Osc52ForwarderState`/tracker structs causing
   confusion about which is "live."** There is exactly one `Osc52Forwarder` type (`src/pane/osc.rs:397`)
   and exactly one instance of it per pane core (`src/pane/terminal.rs:165`, `:921`), used identically for
   local and remote-mirrored panes. The other lookalike structs (`CwdOscTracker`, `AgentOscStateTracker`,
   `OscDebugTracker`) reuse the same tiny `Osc52ForwarderState` enum for their own OSC-scanning state
   machines (title/cwd/debug capture) — they are not competing/dead copies of the clipboard forwarder, just
   independent trackers sharing a state-machine shape. Not a contributing cause.

## Minimal fix options (described, not implemented)

- **Option A (smallest, most direct):** In `App::handle_federation_mount_ready`
  (`src/app/api/workspaces.rs:199-236`) and the equivalent dead `run_federated_session`
  (`src/remote/federation/session.rs:319-329`), keep the real receiver instead of naming it `_...rx`, and
  spawn a task that calls the already-implemented `apply_remote_clipboard_writes`
  (`src/remote/federation/pane_source.rs:251`) against it, applying whatever local clipboard-write policy
  local panes already use (the function's doc comment at `pane_source.rs:230-245` already states the
  intended policy parity and flags the open question of consent/confirmation for remote-origin writes).
- **Option B:** Route remote-origin clipboard writes through the exact same `AppEvent::ClipboardWrite`
  path a local pane uses instead of a separate `ClipboardMessage`/channel, if the origin tag distinction
  the RT-F7 comment calls out (`src/pane.rs:1945-1951`) is judged unnecessary; smaller surface, but throws
  away the origin-tagging the current scaffolding was deliberately built for.
- **Option C:** If remote-origin auto-clipboard-write is considered a trust/consent concern (the
  `pane_source.rs:240-245` comment explicitly raises this), gate Option A's forwarding behind a policy
  check/setting rather than auto-applying unconditionally — this is a product decision, not purely a bug
  fix, and should not be silently decided by whoever fixes this.

None of these were applied; this is a diagnosis only.

## Unresolved questions

- Was `apply_remote_clipboard_writes` ever wired to a live receiver in the past and regressed, or is this
  pipeline still mid-implementation (P5/P6/P7/P8/P9 phase markers throughout the touched files strongly
  suggest the latter — i.e., not a regression, but a feature that was scaffolded end-to-end except for the
  final consumer wire-up)? Git blame on `pane_source.rs:251` and `workspaces.rs:199` would settle this but
  was not run as part of this read-only pass.
- Whether the product wants remote-origin OSC 52 writes auto-applied without consent (current
  `apply_remote_clipboard_writes` doc comment flags this as an open policy question at
  `pane_source.rs:240-245`) — this affects which fix option (A vs C) is appropriate and is a decision for
  the user/maintainer, not purely a bug-fix call.
- Did not confirm remote-side `herdr`/`federation-serve` version or config on `appn-ltu-vm-100`/`-105`
  (`herdr` was not on `PATH` for the non-interactive SSH session used); not needed to confirm this root
  cause since the break is entirely local-side, but flagging that the requested remote check was
  inconclusive rather than skipped.

Status: DONE
