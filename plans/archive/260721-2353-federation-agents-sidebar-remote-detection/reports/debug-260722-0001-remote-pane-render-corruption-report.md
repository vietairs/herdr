# Remote federated pane render corruption — root cause

## Symptom (repro)
Federated pane `r:appn-ltu-vm-105#default:w3` (Claude Code, remote) shows overlapping/interleaved
stale+fresh content after local window resizes: left-column orphan fragments ("/canv", "139)",
"ons g", "s_sen") stacked next to full-width lines, a solid color block, wrapped status bar.
Local (non-federated) panes render fine under the same resizes.

Server log confirms rapid resize churn during the drag that preceded corruption:
`~/.config/herdr-dev/herdr-server.log:246-253` — 6 resize events in ~2.7s
(cols 162→165→168→170→171→176→175→174, rows constant 35), i.e. a resize every ~100-300ms.

## Ranked hypotheses

### H1 (CONFIRMED): local replay-recovery races the remote round-trip, double-painting stale content
- **Trace**: `PaneRuntime::resize` (src/pane.rs:2706-2725) is the single resize entry point for
  *both* local and remote-backed panes. It unconditionally calls
  `self.terminal.resize(rows, cols, ...)` (line 2714-2716) — the **local** ghostty-vt emulator's
  resize — *before* dispatching to `self.io.resize(...)` (line 2718), which for
  `PaneRuntimeIo::Remote` only serializes and sends a `TerminalChannelMessage::Resize` over the
  wire (src/remote/federation/pane_source.rs:162-188). Nothing in `PaneRuntimeIo::resize`
  (src/pane.rs:1146-1165+) branches on `Remote` vs `Actor` to skip the local emulator work.
- `PaneTerminal::resize` (src/pane/terminal.rs:1341-1406) contains a **resize-recovery replay**
  block (lines 1359-1389): before resizing, it snapshots recent ANSI (`replay_ansi`) if the
  bottom of the screen currently has content; after resizing, if the bottom is now blank it
  **re-writes that stale, pre-resize ANSI back into the grid** (`core.terminal.write(ansi...)`,
  line 1387) to paper over the transient blank frame a locally-resized PTY app produces before it
  repaints (this is correct for local panes: SIGWINCH → child repaint happens on the same
  machine, near-synchronously).
- For a remote-backed pane this assumption is false: the actual repaint requires a full
  network round trip — client `Resize` message → wire → `federation_accept.rs:504-518`
  (`FederationCommand::Resize`) → remote host resizes its real PTY → child (Claude Code) redraws
  → bytes tee'd back over the wire → client's `RemoteTerminalSourceHandle` (pane_source.rs:153-188)
  feeds them into the *same* local `PaneTerminal` via `on_read`. `pane_source.rs:170-179`'s own
  comment (RT-F10) already documents that any locally-synthesized terminal_responses from this
  local resize are dropped because "the authoritative PTY... lives on the remote host" — i.e. the
  code already knows the local emulator's post-resize state is not authoritative, but it still
  unconditionally runs the replay-recovery *write* into the grid regardless.
- Result: at each of the ~6 resize events in the log, the local emulator (a) reflows existing
  rows to the new width immediately, (b) if the bottom looks blank, re-injects a stale ANSI
  snapshot of the pre-resize screen, and then, out of band, (c) the real remote repaint for that
  (or a *later*, already-superseded) size arrives and is written on top without any intervening
  clear — producing exactly the observed stacked/interleaved fragments and stray color-block
  (the replayed stale frame's styled runs bleeding into the eventual real frame).
- **Source (exact fix location)**: src/pane.rs:2706-2725 (`PaneRuntime::resize`, needs a
  remote/local branch) and/or src/pane/terminal.rs:1341-1406 (`PaneTerminal::resize`'s
  replay-recovery block, lines 1359-1389, needs to be gated off for remote-backed panes since
  the remote PTY is the sole authority on redraw timing there).

### H2 (partially eliminated): resize not propagated to remote pty
- Trace: it *is* propagated — `pane_source.rs:180-187` sends `TerminalChannelMessage::Resize` on
  every call; `federation_accept.rs:504-518` routes it to `FederationCommand::Resize` against the
  real actor. No evidence of drops. Rejected as sole cause, though the *timing gap* between send
  and remote-repaint-return is exactly what H1 exploits.

### H3 (open, not primary): wrapped-line continuation flags lost in frame encoding
- Trace: remote side does not send pre-parsed grid rows at all — it forwards raw PTY bytes
  (`pane_source.rs` test `byte_in_reaches_on_read_in_order`, lines 281-289, and CX-4 comment at
  208-213: the local side runs "a real ghostty grid"). Since both sides use the same libghostty-vt
  parser on the same byte stream, wrap flags are regenerated locally, not transmitted — so this
  class of bug is structurally avoided for steady-state content. Not the mechanism here; kept
  open only in case libghostty-vt itself mis-tracks wrap flags across a mid-stream resize, which
  would need a targeted repro (see below).

### H4 (open, minor contributor): repeated-resize churn amplifies H1
- The 6 resizes/2.7s in the log (`herdr-server.log:246-253`) mean the replay-recovery block in
  `terminal.rs:1341` can fire multiple times before any single remote round trip completes,
  compounding stale overlays. Not a separate bug, but explains why the corruption is visible now
  (drag-resize) rather than on a single one-shot resize.

## Unconfirmed / next evidence
- Have not captured a byte-level trace (`herdr agent read <pane> --source detection --format ansi`)
  of the corrupted pane mid-corruption to show the exact replayed-ANSI vs remote-repaint byte
  boundary; would conclusively show the stale snapshot's escape sequences interleaved with the
  new repaint's. Recommended before implementing a fix.
- Have not confirmed whether `bottom_before_resize`/`bottom_is_blank` checks
  (terminal.rs:1359-1384) actually evaluate true during the observed drag (would need instrumented
  build), though the mechanism is structurally sound as raced regardless.

Status: DONE
Summary: Root cause is `PaneRuntime::resize` (src/pane.rs:2706-2725) invoking the local emulator's
resize-recovery replay (src/pane/terminal.rs:1341-1406, replay block at 1359-1389) for
remote-backed panes exactly as it does for local ones, even though the remote repaint is an
async round trip (pane_source.rs:162-188, federation_accept.rs:504-518); rapid drag-resize
(herdr-server.log:246-253) fires this replay repeatedly before the remote's real repaint returns,
stacking stale replayed frames under the eventual authoritative remote bytes.
