# Build fixes: non-ghostty merge errors (v0.8.0 upstream merge)

Worktree: `/Users/hvnguyen/Projects/herdr/.claude/worktrees/merge-upstream-v0.8.0`

Started at 44 errors (per task brief); actual initial count when I began was 40
non-summary errors (24 ffi + 16 non-ffi), likely already trimmed by earlier
work on `src/ghostty/mod.rs`. All 16 non-ffi errors fixed. Final state: 18
errors remain, all `ffi::Ghostty*` "not found in ffi" in `src/ghostty/mod.rs`
(out of scope, explicitly excluded).

## 1. `RenderSignal` migration

Root cause: upstream replaced `Arc<AtomicBool>` render-dirty flags with
`Arc<RenderSignal>` (`src/render_signal.rs`), which coalesces two request kinds
— `request_generic()` for state changes and `request_pty(pane_id)` for PTY
output — instead of a single boolean. Fork-only federation code still built
against the old `Arc<AtomicBool>` shape (`.store(true, Ordering::Release)` /
`.swap(...)`).

Idiom confirmed by reading already-migrated upstream call sites
(`src/app/mod.rs`, `src/app/popup.rs`, `src/app/worktrees.rs`,
`src/app/theme_sync.rs`, and the already-migrated local-pane path in
`src/pane.rs`'s `spawn_command_builder`): generic app-state changes call
`self.render_dirty.request_generic()` then `self.render_notify.notify_one()`;
PTY-sourced output calls `render_dirty.request_pty(pane_id)`, which itself
returns whether this is the transition into "pending" (no separate `notify_one`
guard needed).

Fix, file by file:
- `src/render_signal.rs` — untouched (per instructions), only read to learn API.
- `src/app/creation.rs` — 6 call sites of `self.render_dirty.store(true, ...); self.render_notify.notify_one();` → `self.render_dirty.request_generic(); self.render_notify.notify_one();`. Also 6 test-only `let render_dirty = Arc::new(AtomicBool::new(false))` → `Arc::new(crate::render_signal::RenderSignal::new())` (needed once `TerminalRuntime::spawn_remote`'s param type changed; these are inside `#[cfg(test)] mod tests`, invisible to plain `cargo build` but required for `cargo test`/`cargo check --tests`).
- `src/app/api/workspaces.rs` — 4 identical `self.render_dirty.store(true, Ordering::Release);` → `self.render_dirty.request_generic();`. The 5th occurrence (`render_dirty: self.render_dirty.clone()`, building `SplitMaterializationContext`) needed no local edit — see `src/remote/federation/client.rs` below for the real type-mismatch root cause (task brief's "+ one E0308").
- `src/remote/federation/client.rs` — **not in the task's explicit file list, but the real root cause of the reported E0308** (`expected Arc<Atomic<bool>>, found Arc<RenderSignal>` when constructing `SplitMaterializationContext` in `workspaces.rs`). `SplitMaterializationContext::render_dirty` field was typed `Arc<AtomicBool>`; changed to `Arc<crate::render_signal::RenderSignal>`, plus its 5 constructors (1 production `drive_mount_channel`-context builder path is unaffected — only the struct-literal sites, all `AtomicBool::new(false)` → `RenderSignal::new()`; these are test-only literals). `ctx.render_dirty.clone()` (2 sites) already flow into `spawn_remote`, which now expects the same type — no further change needed there.
- `src/app/remote_clipboard_stage.rs` — 1 site, same generic-request substitution.
- `src/terminal/runtime.rs` — `spawn_remote` wrapper's `render_dirty: Arc<AtomicBool>` param → `Arc<RenderSignal>` (now just forwards to `pane::PaneRuntime::spawn_remote`, whose own signature required the same fix — see below); its `#[cfg(test)]` helper `let render_dirty = Arc::new(AtomicBool::new(false))` → `Arc::new(RenderSignal::new())`.
- `src/pane.rs` — **not in the task's file list under item 1, but required**: `PaneRuntime::spawn_remote`'s own `render_dirty: Arc<AtomicBool>` param (the function `terminal/runtime.rs::spawn_remote` wraps) was still on the old type, and its body used `render_dirty.swap(true, Ordering::AcqRel)` — the exact old idiom, not migrated to `request_pty`. Changed param type to `Arc<RenderSignal>` and both `swap(true, ...)` call sites (immediate render + delayed render) to `render_dirty.request_pty(pane_id)`, matching the already-migrated sibling `spawn_command_builder` a few hundred lines below (same file, same struct, real precedent — not inferred). `RenderSignal` was already imported in this file.

Confidence: high. Every substitution has a directly-adjacent, already-migrated
upstream precedent in the same file or a sibling function of the identical
shape (local pane spawn vs. remote pane spawn). No behavior invented.

## 2. `PaneRuntimeIo::Remote` non-exhaustive matches (task item 2)

**Not reproduced.** After fixing items 1, 3, 4, `cargo build` shows zero errors
in `src/pane.rs`. I enumerated every `match self { PaneRuntimeIo::Actor... }`
block in the file (`write_user_input`, `try_send_bytes`,
`write_terminal_response`, `send_bytes_after`, and others) — all already have
explicit `PaneRuntimeIo::Remote(...)` arms (several already correctly wired to
`TerminalSource`/`RemoteTerminalSourceHandle`, one an explicit commented no-op
for host-terminal-response, matching the "remote host owns its own responses"
rationale). The two errors cited at `src/pane.rs:1368`/`:1380` in the task
brief do not exist in the current tree — likely already resolved by the
conflict resolution itself, or resolved as a byproduct of another workstream's
concurrent edit before I started. No code changed for this item.

## 3. `Method::WorkspaceMoveBlock` non-exhaustive match

Root cause: upstream added `Method::WorkspaceMoveBlock(WorkspaceMoveBlockParams)`.
Two matches over `Method` exist in `src/api/mod.rs`:
- `request_changes_ui` (a `matches!` macro, implicit wildcard) — already had
  `WorkspaceMoveBlock` listed (pre-existing from the merge conflict
  resolution); no change needed.
- `federated_session_allows` (a real `match`, deliberately exhaustive, no
  wildcard, per its doc comment) — missing the new variant. Added
  `Method::WorkspaceMoveBlock(_)` to the "forbidden for a view-only federated
  session" arm (`=> false`), directly beside `Method::WorkspaceMove(_)`: it is
  a workspace-structure mutation (moving a block of tabs/panes between
  workspaces), same category as `WorkspaceMove`/`WorkspaceClose`/`TabMove`
  already forbidden there, and not a read-only query, presentation/navigation,
  or remote-input-forwarding method (the three allowed categories).

Confidence: high — the classification follows the same reasoning the existing
sibling `WorkspaceMove` arm already documents.

## 4. `src/app/input/mod.rs`

Two related bugs from a conflict resolution that mixed fork and upstream
code, plus one cascading bug discovered while fixing them.

- **E0069** (`return;` in a function returning `Option<TerminalInputTarget>`):
  compared `git show HEAD:src/app/input/mod.rs` (old fork, function returned
  `()`) against the current tree (upstream changed `handle_key`'s signature to
  return `Option<super::TerminalInputTarget>` so a caller can route the key
  onward). The fork's two remote-image-paste-intercept early returns
  (`TOAST_REMOTE_TOO_OLD` toast path, and the `Capture` path that starts async
  clipboard-image capture) were still bare `return;`. Both are "key fully
  consumed here, nothing for the terminal target dispatch to do" — the exact
  same semantics as the pre-existing `return None;` a few lines above (line
  90, the modal-paste-shortcut intercept) in the same function. Changed both
  to `return None;`.
- **E0308** (`expected &TerminalKey, found TerminalKey` at line 980, inside
  `remote_image_paste_decision`): upstream changed
  `terminal_key_matches_combo` (`src/config/keybinds.rs:1306`) to take
  `key: &TerminalKey` instead of by value. Fixed the call site to
  `&crate::config::terminal_key_matches_combo(&key, binding)`... i.e.
  `terminal_key_matches_combo(&key, binding)`.
- **Cascading E0382** (discovered only after the E0069 fix, not in the
  original 44/40-error set — a move that had never been exercised until the
  early-return paths started type-checking correctly): `handle_key` moves
  `key: TerminalKey` (not `Copy` — holds `generated_text: Option<String>`)
  into `remote_image_paste_decision(&self.state, key)` at line 101, then
  reuses `key` afterward (`Mode::Terminal => self.handle_terminal_key(key)`,
  etc.). This move-then-reuse bug was latent in the fork's own code (present
  at `HEAD`, matching `remote_image_paste_decision`'s by-value `key` param,
  unchanged by upstream) — it's plausible this exact path was never
  type-checked end-to-end before (e.g. built without `#[cfg(unix)]`, or an
  earlier `TerminalKey` was `Copy`). Fixed with the minimal, behavior-preserving
  change: `remote_image_paste_decision(&self.state, key.clone())` — one clone
  per keystroke on the hot input path, cheap (a `KeyCode`/`KeyModifiers`/
  small-`Option<String>` struct), preserves the original owned-key API and
  every existing call site (including ~8 test call sites) untouched.
  **Not fully confident this is the intended long-term shape** — the cleaner
  fix would be changing `remote_image_paste_decision` to take `key: &TerminalKey`
  throughout and updating all ~9 call sites (production + tests) to pass a
  reference, avoiding the clone. I chose the smaller, lower-risk diff since
  the task scope was the compile break, not a refactor; flagging this as a
  possible follow-up for whoever reviews the federation input-handling code.

## Files modified

- `src/pane.rs`
- `src/terminal/runtime.rs`
- `src/app/creation.rs`
- `src/app/api/workspaces.rs`
- `src/remote/federation/client.rs`
- `src/app/remote_clipboard_stage.rs`
- `src/api/mod.rs`
- `src/app/input/mod.rs`

No edits to `src/ghostty/mod.rs`, `Cargo.toml`, `Cargo.lock`, or
`src/render_signal.rs`. No `git add`/`commit`/`merge --continue` run.

## Unresolved questions

- Should `remote_image_paste_decision` be refactored to borrow `&TerminalKey`
  instead of the `.clone()` workaround in `src/app/input/mod.rs:101`? Low risk
  either way; flagging for reviewer judgment.

Status: DONE
Summary: 0 non-ffi errors remain; all 18 remaining `cargo build` errors are
`ffi::Ghostty*` "not found in ffi" in `src/ghostty/mod.rs` (excluded scope,
owned by another workstream).
Concerns: one deliberate `.clone()`-per-keystroke workaround in
`src/app/input/mod.rs` for a latent move-then-reuse bug uncovered while fixing
the E0069 return-type mismatch (see item 4) — functionally correct, flagged
for a possible cleaner refactor later. `PaneRuntimeIo::Remote` non-exhaustive
matches (task item 2) were not present in the tree when I started; verified
by full enumeration, no code changed for that item.
