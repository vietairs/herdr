# Code review — upstream v0.8.0 merge

Scope: working tree of in-progress merge `master (572e7390)` + `MERGE_HEAD (v0.8.0)`.
Method: three-way symbol-set diffs (worktree vs master vs MERGE_HEAD vs merge-base c4c4b352),
`cargo check --all-targets`, per-file deletion audits. Read-only agent; transcribed by orchestrator.
Findings marked ORCH-VERIFIED were independently re-checked before transcription.

Status: **DONE_WITH_CONCERNS — 2 blocking items**, both now being fixed.

## Carried-forward concerns: resolved

| # | Concern | Verdict |
|---|---------|---------|
| 1 | `Osc52Forwarder` removal | **Correct.** Path intact end-to-end |
| 2 | `send_bytes_after` remote no-op | **REAL DEFECT.** Justification false. High severity |
| 3 | per-keystroke `key.clone()` | Avoidable; clean fix exists. Medium-low |
| 4 | `origin: None` | **Correct.** Only 1 production site, not 3 |

**(1)** Justified, not guessed: upstream introduced `ProcessBytesResult.clipboard_writes` fed from
`take_clipboard_writes()` — the same field the fork's forwarder fed. Callback registers in the
*single* `Terminal` constructor (`src/ghostty/mod.rs:844`), so remote mirror terminals get it too;
remote panes call the same `process_pty_bytes` (`src/pane.rs:2038`). Base64/multipart/OSC-52-read
suppression now inside the vendored lib; size bound `MAX_CLIPBOARD_BYTES` at `:605`.
Receiver-drop fix NOT re-broken (`src/app/api/workspaces.rs:221-249` untouched).
`CwdOscTracker` still covers remote panes — `src/pane/terminal.rs:1203-1209`,
`take_pwd_changes()...or(cwd_osc_reported_cwd)`, same precedence master had.

**(4)** Only ONE production site (`clipboard.rs:26`); the other two are test-side. `None` correct
per the `src/events.rs:146-152` contract. A local operator-initiated selection copy — even *of* a
remote mirror pane's text — must not be gated by remote-push policy.

## Blocking

### B1 — HIGH — upstream kitty-graphics fast path silently dropped (ORCH-VERIFIED)
`src/ghostty/mod.rs:1453-1498` lost `kitty_graphics()`, `kitty_graphics_generation()`,
`kitty_graphics_u64()` and both early-return branches incl.
`if generation == 0 || self.kitty_empty_generation.get() == Some(generation) { return Ok(Vec::new()) }`.
`kitty_empty_generation` is now write-only dead state — only `:792` decl and `:818` init.
Orchestrator check: helpers 1→0 in MERGE_HEAD→worktree; field 7→2 occurrences.
Compiler cannot catch this (initializer counts as a use). Fork never edited the function → verbatim
restore, no conflict. **Impact:** every render pass re-walks the full placement iterator for panes
that never transmit an image — reverts part of v0.8.0's headline CPU fix.

### B2 — HIGH — `send_bytes_after` no-op for remote panes (ORCH-VERIFIED)
`src/pane.rs:1384-1411`, `PaneRuntimeIo::Remote(_) => {}`. Comment claims the handle is not cheaply
cloneable; false — only `input_tx: mpsc::Sender<Bytes>` (`pane_source.rs:83`) needs cloning.
**Impact:** `src/app/api/agents.rs:102-109` sends prompt text over the wire, no-ops the delayed
Enter, returns `encode_success(AgentPrompted)`. Federated-pane callers get success with the prompt
unsubmitted. API contract violation, not a convenience gap.

## Non-blocking

- **MEDIUM** 7 upstream tests dropped from `src/ghostty/mod.rs`, incl. `first_rendered_row_text`
  helper. The merge kept an assertion-free phantom (`render_cells_handle_issue_453_unicode_payload`
  asserts nothing) and dropped upstream's asserting replacement.
  `unicode_width_helpers_match_terminal_layout_rules` covers live code → pure coverage loss.
- **MEDIUM** `src/pane/terminal.rs:2577-2600` keeps a fork workaround whose comment ("no absolute-row
  variant") is now FALSE after the vendor upgrade — `bindings.rs:2156` exposes
  `GHOSTTY_SCROLL_VIEWPORT_ROW`. Upstream's `scroll_viewport_row` was dropped.
- **MEDIUM** clipboard dispatch in 3 copies (`input/clipboard.rs:16` helper, `input/mod.rs:546-557`
  re-inlined, `copy_mode.rs:25-33`). Identical today; next semantic change diverges silently.
- **MEDIUM-LOW** merge-introduced dead code: unused `Ordering` in `app/api/workspaces.rs:3` and
  `workspace.rs:4` (RenderSignal migration), 9 unused schema imports `cli.rs:7`, orphaned
  `config_io.rs:103,111` writers (upstream removed the Settings→Experiments rows).

## Verified intact — stop worrying about these

- **No fork-original symbol lost.** `(master symbols) − (merge-base) − (worktree)` over all `src/`
  = **empty set**, for both `fn` and `struct|enum|const|static|trait|type`.
- API surface intact both directions: `serde(rename)` variant sets in `src/api/schema/` identical to
  BOTH parents; CLI subcommand set likewise.
- `src/events.rs` untouched by merge — `origin` field + doc contract verbatim.
- `PROTOCOL_VERSION = 19` = upstream's; `git diff MERGE_HEAD -- src/protocol/wire.rs` EMPTY. Fork
  adds nothing to the TUI wire protocol → no version-19 collision.
- `FEDERATION_PROTOCOL_VERSION` 4→5 correctly reasoned.
- Three tricky conflict regions resolved well, not luckily: `src/ui/panes.rs:172-176` unifies
  upstream's 4 new `direct_attach_resize_locks` sites with the fork's
  `federation_owned_terminal_sizes` behind one `host_owns_terminal_size()` helper (all 4 converted);
  `src/server/headless.rs:2850,3185` makes both new upstream resize paths yield to a federation
  mount; `src/app/api/panes.rs` preserves both federation guards plus upstream's close semantics.
- `RenderSignal` migration safe: fork-side conversions use `request_generic()` (over-renders, never
  under-renders); `request_pty(pane_id)` only in PTY read callbacks, matching upstream.
- No new production `unwrap()`; every `#[allow(dead_code)]` has an adjacent comment.
- `Cargo.toml` version 0.8.0 from upstream; fork's extra tokio features retained.

## Pre-existing, NOT merge findings

- `src/remote/federation/session.rs:321-322` — the OTHER mount entry point (`herdr attach <remote>`
  federated-session mode) still drops remote clipboard writes: `_outbound_clip_rx` bound with no
  drain task, own comment admits "no live forwarder in v1". Zero diff vs master. Same bug class the
  TUI path already fixed. **Worth a follow-up issue.**
- `src/app/api/panes.rs:1866-1878` uses the request-id STRING `"federation-close-pane"` as a
  trust-boundary discriminator for "is this caller a remote peer?". Any local socket-API client can
  send that literal id. In master (4 occurrences); flagged only because the merge edited nearby.
- `src/cli.rs:733,745` production `serde_json::to_string(..).unwrap()` — identical in master.

## Not verified

Windows build; full test suite (excluded by constraints — orchestrator runs it).

## Unresolved questions

1. Was the kitty-graphics generation path dropped deliberately (suspected interaction with the
   fork's remote mirror terminals) or accidentally? Fork never touches that function → accidental
   is the likely answer, but confirm.
2. Was upstream's removal of the Settings→Experiments UI rows intentional? If yes, delete the
   orphaned `config_io.rs` writers; if collateral, the rows need restoring.
3. Fork release tag for this merge — `v0.8.0-hvn.1`? The `FEDERATION_PROTOCOL_VERSION` comment
   asserts v4 shipped in `v0.7.5-hvn.2..6`, so the tag affects that comment's accuracy.
