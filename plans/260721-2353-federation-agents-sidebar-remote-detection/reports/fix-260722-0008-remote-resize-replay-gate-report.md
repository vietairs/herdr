# Fix: gate resize replay-recovery off for remote-backed panes

## What was implemented

- `src/pane.rs`:
  - Added `PaneRuntimeIo::is_remote(&self) -> bool` (true only for `PaneRuntimeIo::Remote(_)`).
  - `PaneRuntime::resize` now passes `self.io.is_remote()` into `self.terminal.resize(...)` as a
    new final argument, with a comment stating the invariant (remote repaint is authoritative and
    asynchronous, so the local replay heuristic must not run for remote-backed panes).
- `src/pane/terminal.rs`:
  - `PaneTerminal::resize` (thin wrapper, line ~194) gained an `is_remote_backed: bool` parameter,
    forwarded unchanged to `GhosttyPaneTerminal::resize`.
  - `GhosttyPaneTerminal::resize` (line ~1341) gained the same parameter with a doc comment
    explaining why: the replay-recovery block papers over a transient blank frame that a
    *locally* resized PTY app produces before its near-synchronous SIGWINCH repaint; a
    remote-backed pane's real repaint is instead an async round trip over the federation link
    (RT-F10 in `pane_source.rs`), so replaying stale pre-resize ANSI there would race the later
    authoritative remote repaint. The `replay_ansi` computation now short-circuits to `None` when
    `is_remote_backed` is true, via `!is_remote_backed && ...` added to the existing condition.
    Everything else in the function (offset/scroll restoration, resize itself, terminal_responses
    drain) is untouched.
  - Updated all 4 existing test call sites of `resize(...)` to pass `false` (preserves exact prior
    local-pane behavior/assertions unchanged).
  - Added new test `resize_recovery_skips_replay_for_remote_backed_pane` asserting the gate
    compiles/threads correctly and resize still applies (viewport_rows == 3) with
    `is_remote_backed = true`.

No other files touched (respected ownership boundary: did not touch
`src/app/api/panes.rs`, `src/workspace/*`, `src/remote/federation/*`).

## Validation

```
ZIG=/Users/hvnguyen/.local/zig-0.15.2/zig cargo build --bin herdr
  -> Finished, 0 errors (2 pre-existing unrelated dead_code warnings)

ZIG=/Users/hvnguyen/.local/zig-0.15.2/zig cargo test --bin herdr pane:: -- --test-threads=4
  -> test result: ok. 275 passed; 0 failed; 0 ignored

ZIG=/Users/hvnguyen/.local/zig-0.15.2/zig cargo test --bin herdr resize -- --test-threads=4
  -> test result: ok. 35 passed; 0 failed; 0 ignored
     (includes resize_recovery_skips_replay_for_remote_backed_pane: ok,
      and the 3 pre-existing resize-recovery tests: ok)

ZIG=/Users/hvnguyen/.local/zig-0.15.2/zig cargo clippy --bin herdr
  -> 0 error-level clippy findings (baseline of 3 pre-existing errors mentioned in task
     was not observed in this run; no new findings introduced by this change either way)
```

Implementation notes appended to
`plans/260721-1830-federation-link-cleanup-toast-visibility/implementation-notes.md`
under "Remote federated pane render corruption fix (260722, resize replay-recovery gate)".

## Deviations

None from the assigned scope. One implementation detail not explicitly named in the task prompt:
there is an intermediate thin wrapper `PaneTerminal::resize` (distinct from `GhosttyPaneTerminal`,
both in `src/pane/terminal.rs`) between `PaneRuntime::resize` (src/pane.rs) and the actual
replay-recovery logic; it also needed the new parameter threaded through it to compile. This is
within the owned file (`src/pane/terminal.rs`) and is pure plumbing, not a design change.

## Unresolved questions

None.

Status: DONE
Files changed: src/pane.rs, src/pane/terminal.rs,
plans/260721-1830-federation-link-cleanup-toast-visibility/implementation-notes.md
Test results: cargo build clean; pane:: 275/275 pass; resize 35/35 pass; clippy 0 errors.
