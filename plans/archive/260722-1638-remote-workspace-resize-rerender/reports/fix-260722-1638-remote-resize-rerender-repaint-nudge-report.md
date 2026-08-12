# Fix — remote-workspace panes not re-rendering on resize / split add / split close

Status: IMPLEMENTED, builds + tests green, **not committed** (CLAUDE.md requires commit-message alignment first). Not yet empirically verified against a live mount.

Worktree: `../herdr-worktrees/remote-workspace-resize-rerender` (branch `fix/remote-workspace-resize-rerender`, base 5ec2a10b).

## Cause (advisor-adjudicated, confidence: strong)

Nothing ever produces the repaint the design depends on. The client deliberately skips its local replay-recovery for remote-backed panes (`src/pane/terminal.rs:1399`, pinned by comment + test) because "the remote repaint is the sole source of truth" — but the federation protocol has no repaint primitive: wire `Resize` is fire-and-forget, the only full-paint frame (`Open{replay}`) is emitted once per terminal, and the b5cb8ce8 resync is metadata-only. So the mirror shows ghostty's raw reflow until the remote app volunteers a repaint.

Three legs make that failure persistent, not transient:
- idle / primary-screen apps emit nothing on SIGWINCH;
- if the serving runtime is already at the requested size, `PaneRuntime::resize`'s dedupe (`src/pane.rs:2828`) swallows even the `TIOCSWINSZ` — **no SIGWINCH at all**, though the client mirror just reflowed;
- `Open{replay}` is `handoff_history_ansi`, which returns `None` on the alternate screen (`src/pane.rs:1756-1767`) — **agent panes receive zero full-paint bytes even at mount**.

All three user-reported triggers funnel through the same `compute_pane_infos` → `rt.resize` path, so one cause covers all three.

## Change

Two files, additive, **no wire-protocol change** (so no `FEDERATION_PROTOCOL_VERSION` / `PROTOCOL_VERSION` bump).

- `src/server/federation_actor.rs`
  - new `FederationCommand::NudgeRedraw { epoch, connid, terminal_id }` — lease-gated like `Resize`/`SendInput` (it drives a real ioctl on the host PTY), fire-and-forget, plus its `Debug` arm.
  - `Resize` arm nudges after `runtime.resize(...)`, **unconditionally** — including the deduped/size-unchanged leg, which is exactly the case that sends no SIGWINCH.
  - `nudge_child_redraw()` helper wraps the `#[cfg(unix)]` gate in one place.
- `src/server/federation_accept.rs`
  - `open_terminal` takes `epoch`/`connid` and requests a nudge once per terminal, **after** the `Open` frame is enqueued and the pump is spawned (preserves RT-F6 replay-before-live; bytes stream to a live listener).

Mechanism: the pre-existing jiggle primitive (`src/pty/actor/unix.rs:695-739`, already used for handoff at `src/server/headless.rs:1441`) sets `TIOCSWINSZ` to rows-1, sleeps 30ms, restores — the child repaints itself. Those are ordinary PTY bytes, so they reach the client through the existing tee → `Output` → `on_read` → `render_dirty` path.

Why this payload and not a refresh frame: a refresh frame answered with `scrollback_replay` would be **empty for alternate-screen apps**, i.e. it would fix everything except the agent panes in the user's screenshot. Making the child serialize its own screen works on every screen mode.

Deliberately NOT touched: the `!is_remote_backed` replay-recovery skip (pinned by comment + `resize_recovery_skips_replay_for_remote_backed_pane`) and RT-F10 (`pane_source.rs:186-194`).

## Verification

- `cargo build` — clean (2 pre-existing dead-code warnings only).
- `cargo fmt --check` — clean. `cargo clippy --bin herdr` — no new findings.
- New test `server::federation_accept::tests::opening_a_terminal_requests_a_child_repaint_exactly_once` — an `Open` yields subscribe → replay → nudge, and a duplicate `Open` adds nothing.
- Both pinned regression tests green: `resize_recovery_skips_replay_for_remote_backed_pane`, `open_terminal_delivers_replay_then_live_bytes_on_the_same_channel`.
- Full suite `cargo test --bin herdr -- --test-threads=1`: 2957 passed, 1 failed — `workspace::tests::generated_workspace_ids_are_short_base32_handles`, **reproduced identically on untouched master** (passes in isolation on both trees; full-suite ordering artifact). At `--test-threads=4` the known cross-test contention adds ~26 more, also unrelated (`just`/nextest not installed locally; nextest's process isolation is why CI is green).

## Deferred (advisor-specified, NOT shipped)

- **F3 — remote-host foreground clobber.** If the serving host has its own foreground TUI, its per-frame `compute_view` reverts mounted panes to the *remote* client's geometry (`src/server/headless.rs:3763-3782`), so the nudge's repaint gets re-clobbered one frame later — persistent mis-wrapped content that F1/F2 cannot fix. Fix would exempt mounted terminals from the host's own resize while leased, reusing the existing `direct_attach_resize_locks` precedent. **Requires a product decision** (the host's own TUI would then view those panes at the mount's geometry — the same trade-off direct attach already makes), so it is deliberately not shipped silently.
- **F5 — cell pixel metrics.** `TerminalChannelMessage::Resize` carries no px fields; the actor applies `0, 0`, so remote winsize reports 0x0 px (breaks kitty-graphics/sixel) and guarantees the dedupe-tuple mismatch that feeds F3. Additive and cheap while v3 is unreleased; independent of this bug.
- **serve.rs mount-generation filter** — the standalone `federation-serve` transport hard-filters inbound frames on constant `MOUNT_GENERATION = 1` while the client mints incrementing generations, so after any remount in one client process all Input/Resize/Open are silently dropped. Real latent defect, separate follow-up; not the reported bug (the `--remote` accept path never validates inbound generation, and it would kill typing too).

## Proposed commit message (needs alignment before committing)

```
fix(federation): repaint mounted panes by nudging the serving host's child

Mounted panes never repainted after a geometry change: the client skips
its replay-recovery for remote-backed panes in favour of "the remote
repaint", but nothing on the wire ever produced one. Force the child
itself to repaint via the existing SIGWINCH jiggle, on every federated
resize and once per opened terminal.
```

(No `refs #<n>` line — no issue exists, and agents must not open one in this repo.)

## Unresolved questions

1. Not empirically verified. A live mount + resize is the single check that upgrades this from strong to proven, and it also settles whether the screenshot shows the F1/F2 leg or the F3 clobber.
2. Does `appn-ltu-vm-105` have its own foreground TUI attached? Decides whether F3 is live for this user.
3. F3 policy call: mount-wins vs last-writer-wins vs tmux-style smallest-size.
4. Windows serving hosts keep the repaint gap (`nudge` is `cfg(unix)`; ConPTY has no equivalent). Accept as documented gap, or implement one?
5. Nudge churn during interactive window drags: each wire `Resize` costs 2 `TIOCSWINSZ` + a 30ms sleep on that pane's actor thread. Probably fine (mirrors handoff usage), but may want server-side coalescing if drags feel heavy live.
