# Fork feature verification after upstream v0.8.0 merge

Scope: worktree `/Users/hvnguyen/Projects/herdr/.claude/worktrees/federation-ui-labels`,
branch `fix/federation-ui-labels` @ `a021433a` (= merge commit).
Refs: fork-before-merge `master` (572e7390), merge-base `c4c4b352`, upstream `upstream-v0.8.0` (346411fa).
READ-ONLY. No edits outside this report.

## Method (evidence, not the prompt's list)

- Inventory derived from `git log --oneline master --not c4c4b352` (223 Viet-authored commits) and
  `git diff --stat c4c4b352 master -- src/` (86 files, +31810/-2724).
- Fork-added file survival: all 23 files added by the fork under `src/` exist in HEAD; the fork
  deleted no `src/` file.
- Fork-line-loss scan: per file, intersect `lines added base->master` with `lines removed master->HEAD`.
- Symbol survival: `(master symbols) − (base symbols) − (HEAD symbols)` for
  `fn|struct|enum|trait|const|static|type` across `src/`.
- Surface diffs: `serde(rename=...)` sets in `src/api/`, clap subcommand enum variants, config
  struct field names, `AppEvent` variants.
- Tests executed (see bottom).

## Feature verification table

| Feature | Code present (file:line) | Tests (master → HEAD) | Surface intact | Reachable (call path) | Verdict |
|---|---|---|---|---|---|
| Federation core (protocol/codec/negotiate/session/serve/tee/id/sanitize) | `src/remote/federation/*` all 17 files present; `protocol/mod.rs:56` | 113 → 113 | `FEDERATION_PROTOCOL_VERSION` 4→5 (deliberate, documented at `protocol/mod.rs:46-56`) | `server/mod.rs:9-12` → `headless.rs:1776` `accept_pending_federation_connections` → `headless.rs:3257` `federation_actor::dispatch` | PASS |
| Remote pane mirroring / mount | `src/remote/federation/client.rs`, `pane_source.rs`, `src/app/api/workspaces.rs:250+` | 23+8 → 23+8 | `rename = "workspace.mount_remote"` present both refs | `ui.rs:460` `Mode::MountRemoteWorkspace` → `app/remote_mount.rs:48` → `api/workspaces.rs` `materialize_federation_mount` | PASS |
| Mount dialog (in-app) + mount progress | `src/ui/remote_mount.rs` (byte-identical to master), `src/app/remote_mount.rs` (identical), `app/state.rs:640 RemoteMountState` | 6+21 → 6+21 | `begin_submission`/`resolve_pending_target`/`submitting`/`pending` all present | `input/modal.rs:96,152` `GlobalMenuAction::MountRemoteWorkspace` → `remote_mount.rs:48` → `ui.rs:460` render | PASS |
| Mount recents (last 5, persisted TOML) | `app/config_io.rs:128 save_recent_remote_mount_targets`; `app/mod.rs:342 dedup_capped_recent_remote_mount_targets`; `config/model.rs:844 recent_remote_mount_targets` | in `app/mod.rs` tests at 3754-3835, `remote_mount.rs:548` — all present | `[ui] recent_remote_mount_targets` key present; config field set has **zero** removals vs master | mount success → `remote_mount.rs:96-105` → `config_io.rs:128`; load `app/mod.rs:653`; reload `app/mod.rs:1632`; keyboard `remote_mount.rs:114-133`; mouse `input/mouse.rs:292-302`; render `ui.rs:76 remote_mount_recent_at` | PASS |
| Remote-origin OSC 52 clipboard applied locally | `api/workspaces.rs:200-249` (drain), `api.rs:103-119` (policy+toast), `events.rs:149 ClipboardWrite{content,origin}` | `config/model.rs` 43→43; `app/mod.rs:2597-2613` policy tests present | `events.rs` **byte-identical** master→HEAD; `remote.accept_clipboard_writes` at `config/model.rs:897` | mirror emulator → `pane.rs:2073` → federation Clipboard channel → `api/workspaces.rs:229 apply_remote_clipboard_writes` → `AppEvent::ClipboardWrite{origin:Some(host)}` → `api.rs:103` `handle_internal_event` | PASS |
| Local-pane OSC 52 (fork made it origin-tagged) | `pane.rs:2324`, `pane.rs:2502` (`origin: None`), `input/clipboard.rs:26` | — | — | PTY read → `ProcessBytesResult.clipboard_writes` (`pane/terminal.rs:146,1259`) → `pane.rs:2324/2502` → `api.rs:103` | PASS |
| Federation pane-close sync (bidirectional) | `api/panes.rs:387 FederationMessage::ClosePaneRequest`; `app/mod.rs:162 pending_remote_closes`; `creation.rs:863,1099,1195,1471` | `federation_accept.rs` 18→18, `api/panes.rs:4616-4661` present | `ClosePaneRequest` variant present; protocol v4 bump preserved, v5 bump layered on top | pane close → `api/panes.rs:387` send → remote `loopback.rs:717`/`federation_accept` → `ClosePaneResponse` → `creation.rs:1099` teardown | PASS |
| Remote resync-index purge (unmount + remote-ended) | `creation.rs:895-915 purge_remote_resync_pane_index_for_workspaces` | test `api/workspaces.rs:2192` present | — | `api/workspaces.rs:551` (mount ended) and `:905` (workspace close) | PASS |
| Remote image paste over federation | `input/mod.rs:962 remote_image_paste_decision`, `:1070 begin_remote_clipboard_image_capture`, `:1142 handle_remote_image_paste`; `src/image_path.rs` (**unchanged**); `app/remote_clipboard_stage.rs` | `image_path.rs` 14→14, `remote_clipboard_stage.rs` 24→24, `file_staging.rs` 20→20 | `keys.remote_image_paste` config key present (`main.rs:193`) | key press → `input/mod.rs:101` decision → `:125/:269` capture → `remote_clipboard_stage.rs` → `file_staging.rs` stage over link | PASS |
| macOS `«class furl»` clipboard fallback | `platform/macos.rs:537` (`or_else(read_clipboard_image_via_file_url)`), `:595`, `:614` | 5 `image_from_file_url_text` tests at `macos.rs:1094-1132` present | — | `read_clipboard_image()` → `:537` → `image_path::local_image_path_from_text` | PASS |
| Staging path-escape rejection | `file_staging.rs:446 validate_staging_root`, called `:81`, `:147`; tail-check `:260` | 3 guard tests at `:761,:792,:1029` present | — | every `stage()` entry validates root before sweep | PASS |
| Federation size lease / claim ownership | `server/federation_lease.rs` (unchanged); `headless.rs:369,582,1282,1540,1775` | 8 → 8 | — | `headless.rs:1283/1541` `sync_terminal_size_ownership`; `:1775` epoch gate on accept | PASS |
| Mount outcome correlation to submission | `api/workspaces.rs:184,261,385,444` `resolve_pending_target` | tests at `api/workspaces.rs:1577-1731` present | — | `FederationMountReady/Failed` events → `resolve_pending_target` | PASS |
| Remote target validation before dial | `src/remote.rs` `validate_remote_target` | `remote/unix.rs` 81 → 81 | — | `api/workspaces.rs:83` (dialog path) and `remote.rs:78,84` (CLI `--remote-workspace` path) | PASS |
| Multi-workspace launch (`--remote-workspace`) | `src/main.rs:769-786`, `src/remote.rs:78-84` | — | flag string present; clap subcommand enum **identical** to master | `main.rs` arg parse → `remote.rs` target list → federation mount | PASS |
| Repaint nudge / PTY nudge perf | `pty/actor/unix.rs:210 nudge_child_redraw_after_handoff`, `:446 nudge_restore_due` (deferred restore) | `pty/actor/unix.rs` tests present (`:1312`) | — | `pane.rs:1341`, `pane.rs:3001` | PASS |
| Auto-resize splits / balance-splits menu | `src/layout.rs` **byte-identical** master→HEAD; `input/mouse.rs:1154 auto_resize_enabled` | `mouse.rs` tests `:2450,:2480,:3049` present | `auto_resize_splits` config key present | pane right-click menu → `mouse.rs:1154` → `layout.rs` rebalance | PASS |
| Agent identity relay for remote panes | `federation_actor.rs` / `reducer.rs` (both unchanged) | 17 + 11 → 17 + 11 | — | `headless.rs:3257 federation_actor::dispatch` | PASS |
| Ghostty grapheme crash fix (#453) | `ghostty/mod.rs:3161-3195` — still `GRAPHEMES_LEN` + `GRAPHEMES_BUF`, comment at `:3183` intact | test renamed by upstream: `render_cells_handle_issue_453_unicode_payload` → `render_cells_preserve_issue_453_unicode_payload_exactly` (`ghostty/mod.rs:3744`) | — | `RenderState` row iteration → `grapheme_text_into` (`:3157,:3236,:3806`) | PASS (renamed) |

### Aggregate surface diffs (all clean)

- `src/api/` `serde(rename=...)`: **0 removed**. Only upstream additions `workspace.move_block`,
  `workspace.reordered`.
- `src/config/model.rs` field names: **0 removed**. 20 upstream additions.
- `AppEvent` variants: **0 removed** — `src/events.rs` is byte-identical master→HEAD.
- clap subcommand enum variants: **identical**.
- `src/protocol/wire.rs::PROTOCOL_VERSION` 17 → 19 (upstream).
- `FEDERATION_PROTOCOL_VERSION` 4 → 5 (fork-side merge decision, documented in source).

### Test counts

Per-file `#[test]`/`#[tokio::test]` across all fork files + all fork-touched files:
**master 905 → HEAD 939**. Every fork-only file is exactly equal. Only one net-negative file:
`src/pane/osc.rs` 56 → 41 — see finding F3.

Executed:

```
cargo test --bin herdr remote::     → 198 passed, 0 failed  (3172 filtered)
cargo test --bin herdr federation   → 204 passed, 0 failed  (3166 filtered)
```

## Findings

### F1 — MEDIUM (process, not code): the prior pass's "EMPTY SET" symbol claim is false

Re-running the stated diff yields **27 fork symbols absent from the merged tree**, not zero:
`GhosttyTerminal`, `GhosttyKeyEncoder`, `GhosttyOscParser`, `GhosttyRenderState*`,
`GhosttySgrParser`, `GhosttyFormatter`, `GhosttyMouse*`, their `*_ptr` typedefs,
`GhosttyKittyGraphicsImageData_..._TRANSMIT_TIME_NS`, `WritePtyCallbackState`, and
`render_cells_handle_issue_453_unicode_payload`.

I ran each to ground and **all 27 are benign**:

- 25 are `src/ghostty/bindings.rs` bindgen output; the vendored libghostty-vt commit changed
  (`0f7cd84b…` → `c5a21edf…`) so bindings were regenerated.
- `WritePtyCallbackState` was folded into a consolidated `CallbackState`
  (`ghostty/mod.rs:475 write_pty: Option<Box<WritePtyCallback>>`); behavior preserved
  (`:519` trampoline, `:957` setter).
- `render_cells_handle_issue_453_unicode_payload` was renamed upstream; the fix itself survives.

Impact: none on the tree. Impact on the *review*: the earlier "empty set" result was either
filtered or wrong, so it should not be cited as evidence. This verification supersedes it.

### F2 — INFO: a 4th change in `src/remote/`, not one of the 3 known

`src/remote/federation/pane_source.rs:105 input_tx()` is new (master→HEAD), beyond the stated
RenderSignal rename / protocol 4→5 / `unix.rs` PATH retry.

It is correct and load-bearing. Upstream v0.8.0 added `PaneRuntimeIo::send_bytes_after` (delayed
input, e.g. auto-submit). Its `Remote` arm needed an implementation; `RemoteTerminalSourceHandle`
owns non-cloneable `JoinHandle`s, so only the `mpsc::Sender` can move into the detached task
(`pane.rs:1400`). Without this the `Remote` arm would have been `=> {}` and delayed input to
remote panes would be **silently dropped** — the exact class of bug this review targets. The merge
resolved it correctly.

### F3 — INFO: `src/pane/osc.rs` lost 15 tests — upstream deletion, not fork loss

`Osc52Forwarder`, `Osc52ForwarderState`, `parse_osc52_clipboard_write` and their 15 tests are gone.
Verified: these existed at merge-base `c4c4b352` (upstream code, **not** fork-authored) and
upstream v0.8.0 removed them entirely (`git grep -c Osc52Forwarder upstream-v0.8.0` → 0), replacing
byte-level OSC 52 sniffing with emulator-level `take_clipboard_writes()`
(`ghostty/mod.rs:981`, `pane/terminal.rs:1202`).

The fork's clipboard commit `14ef6b3b` **never touched `src/pane/osc.rs`** (12 files, none of them
osc.rs), so nothing fork-authored was lost. The replacement path is fully wired at all three call
sites (`pane.rs:2073` remote, `:2324`/`:2502` local).

### F4 — LOW: dead branch removed in `src/server/socket_paths.rs`

The merge dropped an 8-line `if extension == "sock"` early-return at
`derive_client_socket_from_api_socket`. Behavior-identical: the removed branch returned
`parent.join(format!("{stem}-client.sock"))`, which is verbatim what the fallthrough returns, and
`stem` is computed before the branch. No behavior change. Noting only because it appeared in the
fork-line-loss scan.

### F5 — INFO: `FEDERATION_PROTOCOL_VERSION` 4 → 5 is a hard interop break

By design and documented at `protocol/mod.rs:46-56`: upstream added `EventKind::WorkspaceReordered`
(`api/schema/events.rs:201`), which is re-exported into `EventMessage.kind`, so a v4 peer cannot
decode the frame. `negotiate()` hard-rejects on mismatch. Operational consequence: **every** peer
must be rebuilt; a v0.7.5-hvn.* remote will refuse to mount. Not a defect — but it is a deploy
requirement, and the local+remote servers must both be restarted on the new binary.

## Could NOT verify (and why)

1. **Live/runtime behavior.** Everything here is static + unit-test evidence. No live SSH mount, no
   two-server federation handshake, no real clipboard round-trip was exercised — that needs two
   built servers on separate hosts, outside this read-only pass.
2. **v5↔v5 interop over a real link.** `negotiate()` unit tests cover the reject path; actual
   v0.8.0-merged-to-v0.8.0-merged mounting was not run.
3. **Non-macOS platform paths.** `src/platform/linux.rs` (+190) and `windows.rs` (+1696) changed
   substantially upstream; only macOS was compiled/tested here. Fork memory records Windows clippy
   already broken on this fork pre-merge, so Windows regressions cannot be attributed either way.
4. **Full test suite.** Per instruction only `remote::` (198) and `federation` (204) filters were
   run. The claimed 3370/3370 full-suite pass was not re-run.
5. **Non-`src/` fork assets.** CI workflow fork changes (`4f59e3e2`, `00b86618`: `-hvn.N` tag
   handling) and `docs/next` were out of scope; not inspected.

## Unresolved questions

1. Should `FEDERATION_PROTOCOL_VERSION` 5 be paired with a fork release tag bump so an operator
   cannot accidentally mount a v4 remote and see only a handshake reject?
2. Was the `render_cells_preserve_issue_453_unicode_payload_exactly` rename verified to still assert
   the same grapheme payload, or only that a test with a similar name exists? (I confirmed the
   *production* fix survives; I did not diff the two test bodies.)
3. Does anything besides `send_bytes_after` rely on remote-pane delayed input, i.e. is `input_tx()`
   currently single-use scaffolding or does more upstream v0.8.0 behavior need the same adaptation?
