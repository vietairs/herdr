# Group A rust-core conflict resolution — src/pane.rs, src/pane/osc.rs, src/pane/terminal.rs, src/ghostty/mod.rs

All conflict markers removed, files are valid Rust. `cargo check` run (ZIG=~/.local/zig-0.15.2/zig);
remaining errors in these 4 files are ALL attributable to one pre-existing, cross-cutting
blocker (see "BLOCKING cross-file dependency" below), not to anything newly broken by this
resolution. Errors in other files (app/*, api/mod.rs, terminal/runtime.rs, etc.) belong to
other groups, as expected.

## src/pane.rs (4 hunks + 2 latent gaps found during verification)

- Import block: fork added federation imports (`RemoteTerminalSourceConfig`,
  `RemoteTerminalSourceHandle`, `ClipboardMessage`, `FederationMessage`, `LocalChild`,
  `TerminalSource`); upstream added `PtyIoActor` (unused, dropped — see below) and
  `RenderSignal`. Merged: kept both sets.
- `from_handoff_fd` terminal setup: fork called `.enable_grapheme_cluster_mode()`, upstream
  called `.resize(cols, rows, cell_width_px, cell_height_px)`. Both are needed (independent
  operations on the restored terminal) — now two sequential calls.
- Two `on_read` closures (`from_handoff_fd` and the main local-PTY spawn path): fork's
  render-dirty signaling used `Arc<AtomicBool>::swap`; upstream replaced the whole
  render-dirty mechanism with `Arc<RenderSignal>::request_pty(pane_id)` (confirmed via
  `grep render_dirty` — the rest of the file, including `PaneRuntime::spawn`,
  `spawn_from_handoff`, etc., already exclusively uses `RenderSignal`). Picked upstream's API
  wholesale (same underlying feature, upstream's is the one actually wired through the rest
  of the file); kept fork's federation output-tee forwarding (`output_tee.send(...)`) which
  upstream doesn't have, unchanged.
- **Latent gap found (not inside a conflict marker) — `PaneRuntimeIo` match exhaustiveness**:
  `write_terminal_response` and `send_bytes_after` (both upstream-added, non-conflicting
  additions) had no `PaneRuntimeIo::Remote(_)` arm, so they silently failed to compile once
  fork's `Remote` variant and upstream's new methods landed in the same enum via git's
  non-conflicting auto-merge. Fixed:
  - `write_terminal_response`: `Remote(_) => {}` no-op — host-appearance/OSC-query response
    bytes are meaningless for a pane whose child is owned by the remote server.
  - `send_bytes_after`: `Remote(_) => {}` no-op, with a comment explaining why —
    `RemoteTerminalSourceHandle` (owns `JoinHandle`s for its reader/forward tasks) is not
    cheaply `Clone`, unlike `PtyIoActorHandle`, so it can't be moved into the detached
    `'static` `tokio::spawn` the Actor branch uses. This is used for delayed agent-prompt
    auto-submit (`AGENT_PROMPT_SUBMIT_DELAY`) — dropping it for remote-mounted panes is a
    narrow feature gap (not a correctness bug); fixing it properly would need
    `RemoteTerminalSourceHandle`/`pane_source.rs` changes, out of this file's ownership.
- Dropped unused import `PtyIoActor` (upstream's own import list also pulls it in but never
  references it by name elsewhere in `pane.rs` either — verified via grep against both HEAD
  and MERGE_HEAD). `PtyIoActorConfig`/`PtyIoActorHandle`/`PtyReadResult` are still imported
  and used.

## src/pane/osc.rs (1 hunk, but the real conflict extended far past the marked hunk)

The literal conflict marker (`Osc52Forwarder`/`CwdOscTracker`/`parse_osc52_clipboard_write`
plus their `Osc52ForwarderState` enum, ~170 lines) had an empty "theirs" side, so a naive
resolution would just keep HEAD's block verbatim. That is wrong: further down the file,
past the marker, upstream's non-conflicting changes silently deleted
`parse_osc52_clipboard_write` (still called from inside the kept HEAD block) and renamed the
shared `Osc52ForwarderState` state machine (also used, unconflicted, by
`AgentOscStateTracker` and `OscDebugTracker`) to a new `OscStreamCollector` engine. A naive
merge would have left the file referencing an undefined function — confirmed via
`grep -rn "fn parse_osc52_clipboard_write"` (zero hits) before the fix.

Investigated via `git diff <merge-base> HEAD -- osc.rs` / `git diff <merge-base> MERGE_HEAD --
osc.rs`: `Osc52Forwarder`, `CwdOscTracker`, and `parse_osc52_clipboard_write` all predate the
fork/upstream split (present at merge-base) — they are upstream's own earlier
OSC-52-forwarding hack, not fork-original code. Fork only added `CwdOscTracker` after the
split. Separately, `src/ghostty/mod.rs` (also mine, see below) shows upstream's v0.8.0
replaced the whole OSC-52-parsing hack with a **native** libghostty-vt callback
(`GHOSTTY_TERMINAL_OPT_CLIPBOARD_WRITE` → `Terminal::take_clipboard_writes()`), because the
underlying "`libghostty-vt` drops `.clipboard_contents`" limitation the hack's doc comment
cites is exactly what that native callback fixes. `terminal.rs`'s call site
(`core.terminal.take_clipboard_writes()`) was ALREADY present, non-conflicting, in the merge
result before I touched anything — confirming this is the actually-intended target design,
not a guess on my part.

Resolution: this is the "pick one side, same underlying feature, superseded by a strictly
better implementation" case, not an integrate-both case.
- Removed `Osc52Forwarder`, `Osc52ForwarderState`, `OSC52_MAX_PAYLOAD_BYTES`,
  `parse_osc52_clipboard_write` entirely (clipboard forwarding now flows through
  `Terminal::take_clipboard_writes()`).
- Reimplemented `CwdOscTracker` (a genuinely fork-original addition — absent at merge-base,
  and NOT superseded by anything upstream added, since upstream's native PWD callback only
  covers OSC 7, while `CwdOscTracker`/`parse_cwd_osc` also handles OSC 9;9 and OSC 1337) on
  top of upstream's new `OscStreamCollector` engine instead of the old duplicated
  hand-rolled state machine, matching the pattern `AgentOscStateTracker`/`OscDebugTracker`
  already use post-merge.
- `parse_cwd_osc`/`parse_reported_cwd` untouched.

## src/pane/terminal.rs (4 hunks, consistent with the osc.rs decision above)

- Import list: merged fork's `CwdOscTracker`/`Osc52Forwarder` set with upstream's trimmed
  set; then dropped `Osc52Forwarder` per the osc.rs decision (kept `CwdOscTracker`,
  `OscTerminator` — the latter is still used elsewhere in this file for default-color-query
  replies, so it must stay even though upstream's side of this one hunk dropped it).
- `GhosttyPaneCore` struct field, its constructor, and the `process_pty_bytes` body: kept
  `cwd_osc_tracker` field/observe call, dropped `osc52_forwarder` field/observe/drain (its
  `clipboard_writes` local was shadowed a few lines later anyway by
  `core.terminal.take_clipboard_writes()` — that second, now-sole assignment was already
  present pre-conflict).

## src/ghostty/mod.rs (4 hunks) — see BLOCKING cross-file dependency below

- `TerminalCallbackState`/`WritePtyCallbackState`: adopted upstream's unified
  `TerminalCallbackState` (holds `write_pty`, `pwd_changes`, `clipboard_writes`,
  `size_report`, `color_scheme` in one FFI-userdata-backed struct) in place of fork's
  standalone `WritePtyCallbackState`. Verified fork's struct had no extra fields to preserve.
- `write_pty_trampoline`: rewrote to read `state.write_pty` from `TerminalCallbackState`
  (upstream's shape) instead of the old dedicated struct.
- `clipboard_write_trampoline`/`capture_clipboard_write`/`borrowed_bytes`/
  `pwd_changed_trampoline`: adopted upstream's native callbacks wholesale (new code, no
  fork equivalent to preserve).
- `struct Terminal` fields and `Terminal::new`: **found the same "hunk boundary lies, real
  diff is bigger" pattern as osc.rs.** The literal conflict markers only covered the
  `Ok(Self { ... })` initializer inside `Terminal::new`; the struct's own field list
  (`write_pty_callback`/`last_pwd` vs. upstream's `callback_state`/`kitty_empty_generation`)
  sat OUTSIDE any marker and was silently left as fork's stale version — confirmed by
  diffing `MERGE_HEAD:src/ghostty/mod.rs`'s struct definition against the merged file. Fixed
  by hand: replaced the field list, rewrote `Terminal::new`'s tail to match upstream
  (`let mut terminal = Self {...}` + a block registering `USERDATA`/`SIZE`/`PWD_CHANGED`/
  `CLIPBOARD_WRITE`/`COLOR_SCHEME`/`GLYPH_PROTOCOL`, then `Ok(terminal)`, instead of the old
  one-shot `Ok(Self {...})`).
- `set_write_pty_callback`: updated to stop allocating/registering its own
  `WritePtyCallbackState` + re-setting `OPT_USERDATA` (now done once in `Terminal::new`);
  just assigns `self.callback_state.write_pty` and (re-)registers `OPT_WRITE_PTY`, matching
  upstream.
- Last hunk (`set_color_scheme`/`take_pwd_changes`/`take_clipboard_writes` accessors): took
  upstream's block. **Found a second silent duplicate**: fork's own polling-based
  `take_pwd_changes` (using `GHOSTTY_TERMINAL_DATA_PWD` + a hand-tracked `last_pwd` field,
  with a doc comment literally explaining it exists because "this vendored libghostty-vt does
  not expose the push-based OSC 7 'PWD changed' callback") survived, unconflicted, later in
  the file — a duplicate `fn take_pwd_changes` in the same `impl Terminal` block, which
  would not compile. Removed the polling version; its own doc comment names exactly the gap
  upstream's native callback closes.
- Type-naming note: upstream's `MERGE_HEAD` source uses `ffi::GhosttyTerminal` (no `_ptr`
  suffix) as the terminal handle type throughout (confirmed both at `MERGE_HEAD` and at the
  actual merge-base — this is the "real"/target naming). The CURRENT working-tree
  `src/ghostty/bindings.rs` still uses the older `GhosttyTerminal_ptr` naming (fork's, not
  bumped). I standardized on `ffi::GhosttyTerminal_ptr` everywhere I touched — matching
  today's actual `bindings.rs` and the ~90% of this same file I did NOT touch (e.g.
  `fn raw(&self) -> ffi::GhosttyTerminal_ptr`) — rather than upstream's renamed convention,
  since a partial rename inside 4 files while the rest of the module (and, presumably,
  other files) keeps the old name would be strictly worse than a consistent wrong name. This
  will need a follow-up rename pass once the vendor bump lands (see below); it is a
  short, mechanical, whole-file/whole-module `_ptr` suffix removal — flagging so it isn't
  missed.
- Did NOT port upstream's `kitty_empty_generation`-based image-placement dedup logic (used
  in kitty-graphics placement code around what's `MERGE_HEAD` line ~1466-1494) — that's a
  separate, non-conflicting feature difference in code neither of us touched via a marker,
  well outside the 4 assigned hunks. Added the field only because upstream's (now-adopted)
  `Terminal::new` initializes it; it's currently write-only/inert, allowed by the file's
  existing `#![allow(dead_code)]`.

## BLOCKING cross-file dependency (not fixable within these 4 files)

`src/ghostty/bindings.rs` (auto-generated from `vendor/libghostty-vt.vendor.json`, not owned
by this group) is currently **unbumped** — still the fork's older vendor commit — and is
missing every FFI symbol upstream's v0.8.0 `ghostty/mod.rs` needs:
`GhosttyClipboardWrite`, `GhosttyClipboardWriteResult`, `GhosttyClipboardLocation`,
`GHOSTTY_TERMINAL_OPT_PWD_CHANGED`, `GHOSTTY_TERMINAL_OPT_CLIPBOARD_WRITE`,
`GHOSTTY_TERMINAL_OPT_GLYPH_PROTOCOL`, `GHOSTTY_TERMINAL_OPT_COLOR_PALETTE`,
`ghostty_color_palette_default`, etc. `vendor/libghostty-vt.vendor.json` itself also still
points at the fork's old vendor commit (`0f7cd84b`) instead of `MERGE_HEAD`'s
`c5a21edfc` — confirmed via `git diff HEAD MERGE_HEAD -- vendor/libghostty-vt.vendor.json`.

This is NOT something I introduced: `default_palette()` (upstream, calls
`ghostty_color_palette_default`) and the `core.terminal.take_clipboard_writes()` call site in
`terminal.rs` were both already present in the merge result, unconflicted, before I made any
edit — i.e. the file was already structurally committed to the vendor bump regardless of how
I resolved my 4 conflict hunks. `cargo check` currently fails on these 4 files ONLY on the
missing-FFI-symbol errors listed above (~24 errors, all `E0425 cannot find ... in module
ffi`), plus (once vendor is bumped) the `GhosttyTerminal_ptr` → `GhosttyTerminal` rename
noted above. No other errors remain in `pane.rs`, `pane/osc.rs`, `pane/terminal.rs`, or
`ghostty/mod.rs`.

**Whoever owns `vendor/`, `Cargo.toml`/`Cargo.lock`, and `vendor/libghostty-vt.patches.md`
(already marked `UU`) needs to bump `vendor/libghostty-vt.vendor.json` to `MERGE_HEAD`'s
commit (`c5a21edfc`) and regenerate `bindings.rs`.** Until then these 4 files will not
compile — expected and unavoidable given the file ownership split.

## Files modified
- src/pane.rs
- src/pane/osc.rs
- src/pane/terminal.rs
- src/ghostty/mod.rs

## Verification
- `grep -c "^<<<<<<<"` on all 4 files: 0.
- `ZIG=~/.local/zig-0.15.2/zig cargo check`: fails repo-wide (other groups' files still
  conflicted, as expected). Filtered to my 4 files: only the vendor-bump-blocked FFI-symbol
  errors above remain; no syntax errors, no unresolved-name errors outside that category, no
  duplicate-definition errors.

Status: DONE_WITH_CONCERNS
Summary: All 4 files' conflict markers resolved, both sides integrated where genuinely
independent (federation output-tee, CwdOscTracker fallback, delayed-input best-effort for
remote panes) and upstream's superseding native clipboard/pwd mechanism adopted over the
now-redundant OSC52Forwarder hack. Found and fixed two additional latent merge-tool bugs
outside the literal conflict markers (osc.rs: dangling call to a function upstream silently
deleted; ghostty/mod.rs: duplicate `take_pwd_changes` + stale `struct Terminal` field list)
that would not have been caught by only touching the marked hunks.
Concerns:
- ghostty/mod.rs compilation is blocked on the vendor/bindings.rs bump (not my file) — flagged
  above with exact missing symbols and the vendor commit that fixes it.
- Once bindings.rs is regenerated, a mechanical `GhosttyTerminal_ptr` → `GhosttyTerminal`
  rename will be needed across this file (and possibly others using the same type) — I
  deliberately did not do this preemptively since it would desync from the ~90% of the file
  I didn't touch.
- `send_bytes_after` silently no-ops for remote-mounted panes (delayed agent-prompt
  auto-submit); flagged inline with a comment. Fixing properly needs
  `src/remote/federation/pane_source.rs` changes (out of my ownership).
- Did not port upstream's kitty-graphics `kitty_empty_generation` placement-dedup logic
  (unconflicted, unrelated feature diff, out of scope for the 4 assigned hunks).
