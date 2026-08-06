# Converged root cause — remote-workspace panes don't re-render on resize / split add / split close

Status: DIAGNOSIS ONLY. No code edited. Fable advisor adjudication NOT run (stopped at user request) — but 3 independent lenses converged, and the key gate was independently verified in-session.

Full lens detail: `debug-260722-1638-remote-resize-rerender-lens-findings.md` (same dir).

## The cause (single, explains all three triggers)

A remote-backed pane's post-resize repaint is **outsourced to a repaint nobody ever requests**.

1. On any geometry change (window resize, split add, split close) the local resize path fires normally for mounted panes — no remote guard, `TerminalSource::resize` IS called. "Resize never fires" is ruled out (`src/ui/panes.rs:186/203/248/280` → `rt.resize`).
2. `PaneTerminal::resize` (`src/pane/terminal.rs:1374`) disables the blank-bottom **replay-recovery** for remote-backed panes:
   ```rust
   let replay_ansi = if !is_remote_backed && ... { ghostty_recent_ansi(...) } else { None };
   ```
   gate at `src/pane/terminal.rs:1399`; `is_remote_backed` comes from `self.io.is_remote()` at `src/pane.rs:2837-2843`. Rationale comment at `src/pane/terminal.rs:1364-1373` / `src/pane.rs:2832-2835`: "the remote repaint [is] the sole source of truth".
   A **local** pane therefore repaints itself synchronously in the same frame as the resize. A **remote** pane paints nothing and waits.
3. **Nothing ever produces that authoritative remote repaint.** Full screen content crosses the federation wire exactly ONCE per terminal — the `Open{replay}` ack's ScrollbackReplay (`src/server/federation_accept.rs:685-703`); a repeat `Open` is a no-op (`:679-681`). Everything after is raw PTY delta bytes (`:736-770`). `Resize` is fire-and-forget into `runtime.resize(rows, cols, 0, 0)` (`src/server/federation_actor.rs:247`), with no ACK and no repaint obligation.
4. The resync added by b5cb8ce8 does not cover this: it is **metadata-only** and fires only on structural pane/tab EventKinds (`src/remote/federation/client.rs:500-509,820`; `src/remote/federation/reducer.rs:1-8`).
5. Even on the serving host, its own resize-recovery repaint **structurally cannot** reach the client: recovery is a **grid write** (`src/pane/terminal.rs:1421`) while the client is fed only the **raw PTY output tee** (`src/pane.rs:2814-2821`).

Net: after a geometry change the remote pane displays whatever ghostty's raw reflow left (blank/cropped, worst for alternate-screen agents), corrected only if the remote app spontaneously repaints on SIGWINCH. Idle shells and agents at a prompt do not — exactly "stale until something else forces a redraw".

## Explicitly ruled out (do not re-investigate)

- **(cols,rows) transposition / off-by-one** across the wire — verified correct end-to-end (lens 2 H4, high confidence).
- **Mount lease dropping legitimate resizes** — verified it does not (lens 2 H4).
- **Reducer discarding dimension-mismatched frames / generation-epoch guard** — does not exist. `reducer.rs` is metadata-only (`:1-8`, no rows/cols/grid/dims references); `TerminalChannelRouter::route_inbound` forwards every Output byte unconditionally (`src/remote/federation/client.rs:432-438`). This is a **missing-repaint** problem, not a discarded-frame problem.
- **Local mirror grid never resized** — it IS resized (lens 3 H3).
- **Remote-panes-miss-a-dirty-flag** — no such asymmetry; neither local nor remote set `render_dirty` from resize itself (lens 3 H5).

## Secondary defects on the same path (fix or file separately — NOT the primary cause)

- **S1 (medium)** — Split add/close get **no forced full redraw**: window resize does an explicit resize + forced full redraw for every client, but split add/close only re-layout lazily on the next render pass, and for a mounted workspace land asynchronously via an `AppEvent`. Decouples the layout change from the remote's knowledge of it. (lens 3 H4)
- **S2 (medium)** — If the remote host has its **own foreground TUI client attached**, its per-frame `compute_view` re-resizes every pane to the REMOTE client's geometry, clobbering the federation-applied size within one remote frame; cell-pixel asymmetry means the dedupe never damps the ping-pong. Would render as permanently wrong wrapping. (lens 2 H2 / lens 3 H3)
- **S3 (low/medium)** — Cell pixel metrics forced to `0` across the wire (`runtime.resize(rows, cols, 0, 0)`), so the remote PTY winsize reports 0 pixel w/h. Breaks kitty-graphics/sixel sizing. (lens 2 H3, lens 1 H5)
- **S4 (medium)** — Wire resize is fire-and-forget with a silent server-side drop path: a lost resize has no retry and no client-visible error. (lens 1 H4)
- **S5 (low)** — Against a standalone `herdr federation-serve` host, every Input/Resize/Open frame is silently dropped after a re-mount in the same client process: client mints an incrementing `mount_generation`, serve host hard-filters on constant `1`. (lens 2 H5)

## Fix direction (NOT yet validated by the advisor — treat as a proposal)

The design intent — remote is the source of truth for its own screen — is sound; the gap is that the protocol has no way to ASK for a repaint. Two candidate shapes:

- **A. Add a repaint/resync request to the federation protocol.** Client sends it after applying a geometry change; server responds with a full screen snapshot for that terminal (the same content shape `Open{replay}` already produces, so the serialization exists). Lands on the **server/runtime** side of the runtime/client boundary guardrail — correct per CLAUDE.md, and reusable by non-TUI clients.
- **B. Re-enable local replay-recovery for remote-backed panes** as an interim repaint from the client's own mirror scrollback. Cheaper, purely client-side, but contradicts the pinned rationale and the existing test `resize_recovery_skips_replay_for_remote_backed_pane` (`src/pane/terminal.rs:4465`) — that test pins the skip deliberately, so B means changing a pinned decision, not fixing a bug.

Recommendation: **A**, with S1 (forced full redraw on split add/close) folded in, since without S1 the split triggers still won't request the repaint at the right time. Do NOT remove the `is_remote_backed` skip without an explicit decision — it is pinned by test and comment.

Respect the pinned **RT-F10** semantic in `src/remote/federation/pane_source.rs:186-194` (only the visual resize crosses the wire; synthesized terminal responses are dropped). A repaint-request frame is new protocol, not a terminal response, so it does not violate RT-F10 — but confirm this with the advisor before implementing.

Protocol change → check `src/protocol/wire.rs::PROTOCOL_VERSION` against the latest released tag per CLAUDE.md, and bump only if source protocol is not already ahead.

## Protocol-prerequisite check (verified 2026-07-22 19:15, post-resume)

Cheaper than the handoff assumed. Four facts, all verified in source:

1. **Wrong version constant.** Federation has its OWN version — `FEDERATION_PROTOCOL_VERSION = 3` (`src/remote/federation/protocol/mod.rs:37`), explicitly documented as "independent of `crate::protocol::wire::PROTOCOL_VERSION`". A federation frame does NOT touch `wire.rs::PROTOCOL_VERSION` (which is 17 in source AND at `v0.7.5` — equal, so it would need a bump if it were in scope; it isn't).
2. **No federation bump needed either.** `protocol/mod.rs:31-36` records the precedent: `SnapshotRequest`/`SnapshotResponse` were added WITHOUT a bump because **v3 has never shipped in a release** — no deployed peer can observe the addition as skew. Same rationale covers a new repaint frame. (Re-verify the "v3 unreleased" claim still holds at implementation time.)
3. **The request/response pattern already exists and is a direct template.** `SnapshotRequest` (`:309`) is a fieldless control-channel client→server request; the serving side answers on the Mount channel (`federation_accept.rs:558-594`); its no-fields design is justified by the single-controller lease ("the server always answers with its own current, unambiguous state") — the identical argument applies to a repaint request. Its `MountSnapshot` payload is **session structure, not grid content**, so it does not solve this bug — but its shape is the blueprint.
4. **Client-side apply path needs NO new plumbing.** `Open{replay}` bytes are delivered to the pane through the *same* byte channel as live output — `let _ = tx.try_send(Bytes::from(replay.bytes));` (`src/remote/federation/client.rs:427-428`). A post-hydrate `ScrollbackReplay` would flow through the existing `TerminalChannelRouter` → `RemoteTerminalSource` path unchanged. Watch RT-F6 ordering (replay-before-live is asserted by `open_terminal_delivers_replay_then_live_bytes_on_the_same_channel`, `client.rs:1142`); a mid-stream replay interleaves with live bytes and needs an explicit ordering decision.

Net: Option A shrinks to ~one new `FederationMessage` variant + a serving-side handler reusing the existing `scrollback_replay` producer + a client-side send on geometry change. No version bump. This strengthens the A-over-B recommendation.

> **SUPERSEDED by the advisor verdict (below).** Point 3's "reuse `scrollback_replay` as the refresh payload" is WRONG: `handoff_history_ansi` returns `None` on the alternate screen (`src/pane.rs:1756-1767`), so that payload is **empty for exactly the agent panes in the screenshot**. Points 1, 2 and 4 stand but are moot — the shipped fix needs no new frame at all. See `fix-260722-1638-remote-resize-rerender-repaint-nudge-report.md`.

## Unresolved questions

1. Fable advisor never adjudicated — the fix_spec, the A-vs-B call, and the per-trigger completeness check are unverified. Highest-value thing to run on resume.
2. Not empirically reproduced. No live mount was driven, no cargo test run — everything above is source-read. A live repro (mount a remote workspace, resize, `herdr agent read <pane> --source detection --format text`) would upgrade this from "strong" to "proven" and is cheap.
3. Is S2 (remote host with its own foreground client) actually the user's setup? The screenshot shows `herdr --remote appn-ltu-vm-105` — unknown whether that VM also has an attached TUI. Changes whether S2 is live or theoretical.
4. Does the local mirror have enough scrollback for option B to be viable as an interim, or is `Open{replay}` the only content it ever had?
