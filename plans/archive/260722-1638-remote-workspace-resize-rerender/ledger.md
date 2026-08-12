Task: remote panes went stale/blank after resize or split (repaint gap).

Root cause: `src/pty/actor/unix.rs`'s post-resize nudge restored the size
frozen at queue time, clobbering a resize applied in the same drain
(`apply_pending_controls` runs resize before nudge). Proven via 3 lenses +
fable advisor.

Shipped: nudge now restores `last_applied_size`. Rejected gating the nudge
on "size changed" — `runtime.resize` is a latest-wins slot, so gating
reintroduces the blank-pane bug during drag-resize; unconditional is
correct at that site. Accepted the nudge's 30ms actor-thread sleep and
transient rows-1 repaint during drag-resize as the design cost.

PR #2 — MERGED 2026-07-23 as `6058ce53`. Pre-merge fable review (15 agents):
0 blockers, 3 findings fixed (7392001d, ae3af95b), 1 deferred.

Deliberately out of scope: cell pixel metrics on the wire, `serve.rs`
mount-generation frame filter, Windows repaint gap (nudge is `cfg(unix)`).

Archived-at-SHA: d2ce3de280b2446a3d07575795416b6976077898
