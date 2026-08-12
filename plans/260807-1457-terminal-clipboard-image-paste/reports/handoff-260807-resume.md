# HANDOFF — clipboard image paste to mounted remote workspace (Ctrl+V / Cmd+V)

Written 2026-08-07 16:30 (Australia/Melbourne). Resume point for a later session, human or agent.

**Resume command:** `/hvn:cortex continue` from `/Users/hvnguyen/Projects/herdr` — Resume Detection
will find `plans/260807-1457-terminal-clipboard-image-paste/pipeline-progress.md`. Or just read this
file; it is self-contained.

---

## 1. The ask

User: *"the paste image to a mounting workspace only works on cmux app, not working in apple
terminal app or warp app, fix it"* — later expanded to *"it needs to work with ctrl+V, cmd+V, and
paste from this mac clipboard to the remote workspace."* Flags `--auto --advise`.

## 2. Root cause (PROVEN — do not re-investigate)

herdr has **two** key/paste dispatchers. `remote_image_paste_decision` (the Ctrl+V clipboard-image
feature) was wired ONLY into `handle_key` (`src/app/input/mod.rs`), which does not run in a normal
session — the headless server nulls its input channel (`src/server/headless.rs:616-618`). The live
dispatcher is `route_client_events_from` (`src/app/mod.rs:1853+`) →
`prepare_terminal_key_forward` (`src/app/input/terminal.rs`), which never called it. So Ctrl+V fell
through to the raw PTY: no effect, and no toast, because no branch on that path could raise one.

The sibling bracketed-paste handler HAD been ported to the live `Paste` arm — which is why cmux
worked and nothing else did.

**Correction to an earlier belief:** `handle_key` is NOT dead code. It is live for `herdr --remote`
(`src/remote/federation/session.rs:376`) and `--no-session` (`src/app/runtime.rs:169,198`). Both
dispatchers therefore need every key feature — the fix uses one shared helper, not two copies.

## 3. MEASURED terminal behavior (user ran the probe; this is fact, not theory)

`printf '\033[?2004h'; stty raw -echo; xxd -c 16`, then Cmd+V with a screenshot on the clipboard:

| Terminal | Bytes on Cmd+V | Consequence |
| --- | --- | --- |
| Apple Terminal.app | **nothing** | Cmd+V unreachable, ever. Ctrl+V only. Not fixable in herdr. |
| Warp | `ESC[200~ESC[201~` (empty bracketed paste) | bridged in round 3 → Cmd+V works |
| cmux | `ESC[200~/var/folders/…/T/clipboard-….png ESC[201~` | already worked (path is inside `temp_dir()`) |

## 4. Work state

Worktree `/Users/hvnguyen/Projects/herdr/.claude/worktrees/terminal-clipboard-image-paste`,
branch `fix/terminal-clipboard-image-paste`, base master `f934965b`. **ALL UNCOMMITTED.** Nothing
pushed. Main checkout is untouched on master.

| Round | What | State |
| --- | --- | --- |
| 1 | Wire Ctrl+V into the live dispatcher | done; review found a blocking regression |
| 2 | Key-repeat fix (`KeyEventKind::Press` gate), shared helper across both dispatchers, toast-once instead of swallowing the key, docs correction, test seam | done |
| 3 | Empty-bracket bridge → Cmd+V on Warp; removed ~80 lines of pre-existing duplication | done |
| review | `code-review-260807-round3-final.md` | **APPROVE_WITH_NITS** |
| security | `security-scan-260807-clipboard-paste.md` | clean, nothing blocks merge |
| 4 | M1/M2/M3 + Q5 (see §5) | **DONE 16:36 — all four fixed. 3393 passed / 1 pre-existing failure.** |

Files touched: `src/app/input/mod.rs`, `src/app/input/terminal.rs`, `src/app/mod.rs`, `src/main.rs`,
`docs/next/CHANGELOG.md`, `docs/next/website/src/data/config-reference.json`.

### FIRST THING ON RESUME
Round 4 completed at 16:36 — the code is at a coherent stopping point. Sanity-check with:
```
cd /Users/hvnguyen/Projects/herdr/.claude/worktrees/terminal-clipboard-image-paste && git diff --stat
```

## 5. Round 4 — ALL FOUR FIXED (16:36)

- **M1 — off switch restored.** `dispatch_empty_bracketed_paste` returns `Forward` when
  `state.remote_image_paste_key.is_none()`. One switch governs both triggers; no second config key
  invented. Docs updated in `src/main.rs`, `config-reference.json`, `CHANGELOG.md`.
  *Open question left by the agent:* the non-empty cmux temp-path bridge is deliberately NOT gated
  by the binding (master behavior; it is a file read, not a clipboard read). Confirm that's wanted.
- **M2 — fixed client-side, no protocol change.** Origin is genuinely NOT distinguishable at the
  server paste arm: in production every TUI is a client (`headless.rs:2927`), so gating on
  `LOCAL_INPUT_SOURCE` would kill the feature for all local users, and neither `ClientConnection`
  nor `ClientMessage::Hello` carries a remoteness marker. The agent correctly stopped rather than
  adding a protocol field. Instead the *client* — which already knows via `is_remote_client_process()`
  — swallows the empty paste when it has no local image
  (`suppress_unbridged_clipboard_image_trigger`, `src/client/mod.rs`). Host clipboard is never read
  on a remote client's behalf. Local clients unaffected.
  **RESIDUE — needs Can's decision:** the configured-KEY shape is still forwarded (it also reaches
  non-federated panes), so that narrower path can still reach the host clipboard. Closing it needs
  a `Hello` field, i.e. a protocol change. Deliberately not done.
- **M3 — bounded per-pane in-flight flag** (not a debounce), `App::remote_clipboard_image_reads_in_flight`.
  `begin_remote_clipboard_image_capture` is now `&mut self` and skips while a read is outstanding;
  released before every early return in `handle_remote_clipboard_image_captured`, so a NoImage or
  timeout cannot wedge a pane. Bounds the key path AND the pre-existing cmux paste shape — retires
  the class for master too.
- **Q5 — purge wired.** `purge_remote_image_paste_pane_state_for_workspaces` clears both
  `PaneId`-keyed sets, modeled on `purge_remote_resync_pane_index_for_workspaces`, called at both
  existing close sites in `src/app/api/workspaces.rs`.

Verification: `3393 passed / 1 failed` at `--test-threads=2` (3389 + 4 new; the 1 is the known
pre-existing `pane_graphics_stream` failure, reproduced on a clean `f934965b`). `cargo fmt --check`
exit 0. Both new guards mutation-verified (`left: 8` reads without the M3 guard; `Consume` without
the M1 gate). 4 tests added, none weakened or deleted.

## 5b. Round-4 re-review — DONE (`code-review-260807-round4.md`, APPROVE_WITH_NITS)

Reviewed the parts nobody had seen: `src/client/mod.rs` (M2), the `&mut self` change (M3), Q5.
Verified against source rather than against claims, and ran clippy against a throwaway `f934965b`
baseline worktree — **10 bin warnings on both sides, per-lint inventory byte-identical, 0 new.**

- **M3 verified, no wedge.** The flag `remove()` is the first statement of the handler body, ahead
  of the `match` and all four exits, with nothing panic-capable or `?`-bearing before it. Reviewer
  also checked the other half: `spawn_clipboard_image_capture` funnels all four arms — including a
  `JoinError` from a panicking read — into one `send`. Nit: `spawn_blocking` is uncancellable, so a
  timed-out child outlives its released flag.
- **Q5 verified.** Both sites, both sets, and — the part that mattered — both purges run *before*
  `close_selected_workspace()`. After it, the id set is empty and the purge is a silent no-op.
- **M1 was half-connected → FIXED (see below).**
- **M2 residue confirmed end-to-end → escalated, not closed (see §10).**

### The M1 fix (applied after the review)
`should_bridge_clipboard_image_paste` (`src/client/mod.rs`) returned `is_remote_client` for the
empty-paste shape *before* consulting the binding — so `keys.remote_image_paste = ""` did not
actually turn the feature off on `herdr --remote`, while round 4's new doc strings claimed it did.
Fixed the code rather than softening the docs: the `None` early-return now precedes the
empty-shape check, and `suppress_unbridged_…` inherits the gate because it only runs inside the
bridge branch. An existing test asserted the buggy behavior; inverted it.

**Post-fix verification: `3394 passed / 0 failed` at `--test-threads=2`** (the flaky
`pane_graphics_stream` passed this run — same 3394 total as the 3393/1 run). `cargo fmt --check`
exit 0. Gate proven by inspection, not an executed mutation run — the pre-fix ordering could not
have produced the new assertion.

## 6. Remaining pipeline

1. `/hvn:ship-gate` (not yet run).
2. **Live validation — NOT DONE, and nothing above substitutes for it.** No one has pressed the keys
   against a real mount. See §7.
3. Commit + PR. Nothing has been committed; there is no PR.
4. Post-merge: sync base, remove worktree, archive this plan dir.

## 7. Live test recipe (owed before merge)

Restart the local server on the new binary first — a long-running `herdr server` does NOT pick up a
reinstalled binary, and this has burned a previous session:
```
export ZIG=$HOME/.local/zig-0.15.2/zig
export PATH="$HOME/.local/zig-0.15.2/xcrun-shim:$PATH"
cargo build --bin herdr
```
Then with `HERDR_LOG=herdr=debug` (log: `~/.config/herdr/herdr-server.log`), on a mounted remote pane:
- Apple Terminal: **Ctrl+V** with a screenshot on the clipboard → image staged on the remote.
- Warp: **Cmd+V** and **Ctrl+V** → both stage.
- cmux: **Cmd+V** → still works (regression check).
- Hold Ctrl+V ~1s → exactly ONE staged file, not N.
- Local (non-remote) pane: Ctrl+V still reaches the pane app (readline quoted-insert) unbroken.

## 8. Verified facts that override stale notes

- **Clippy: 0 errors, 10 WARNINGS on master** — all pre-existing (incl. `needless_borrow` at
  `src/app/input/mod.rs:919`). Note this was got wrong TWICE today: the old note said "3 baseline
  errors" (stale), then a reviewer reported "0 errors, 0 warnings" — which was a **cached no-op
  re-lint**. Force a real run with `touch src/main.rs` before believing any clippy result,
  especially a clean one.
- **Test ground truth at `--test-threads=2`:** baseline `f934965b` = **3374 passed / 1 failed**;
  branch after round 4 = **3393 passed / 1 failed**. Same single pre-existing failure in both
  (`api::server::pane_graphics_stream::tests::inactive_owner_cancels_idle_stream_and_dispatches_close`).
  An agent claimed "3390 passed, 0 failed" and it did NOT reproduce. **Do not put that number in a
  commit message.** The suite is also flaky at `--test-threads=4`; use 2.
- Build needs `ZIG=$HOME/.local/zig-0.15.2/zig`. No `just`, no `nextest` on this machine.

## 9. Decisions already made (don't relitigate without new evidence)

- **Helper app / global-hotkey daemon: REJECTED.** Needs `CGEventTap` + Accessibility TCC, can't
  reliably scope to "herdr focused AND pane federated", second notarized artifact, and it converts a
  client-local clipboard read into an RPC — a threat-model regression. Full costing in
  `research-260807-cmdv-screenshot-helper-options.md`.
- **Clipboard-watcher daemon: rejected** — not a trigger, and it persists every copied image to /tmp.
- **Apple Terminal Cmd+V: not achievable.** Zero bytes emitted; nothing to hook.
- **In-flight guard on the KEY path: rejected** (round 2) — distinct presses are distinct intents.
  This does NOT apply to the paste path, which is what M3 fixes.

## 10. Still open for the user

- **DECISION NEEDED — M2 residue: is `herdr --remote` ever cross-user?** On a remote client,
  `Ctrl+V` with no local image forwards `0x16`; the server reads the **host** clipboard and stages
  that image into the pane the remote operator is watching. Rounds 1-3 made this reachable.
  The empty-paste shape was closable client-side (no payload); the configured key is not — it also
  reaches non-federated panes where it is an ordinary keystroke a pane app expects. Closing it
  needs the server to know the client is remote = a `ClientMessage::Hello` field = a wire-protocol
  bump, deliberately out of scope for a fix branch. **Low severity IF federation is only ever
  self-attach between your own machines, which is the assumption currently carrying it.** Confirm
  or correct that assumption.

- **Cmd+V with a Finder-copied image FILE.** Needs the `temp_dir()` gate
  (`src/image_path.rs:87-109`) widened; three options offered, deliberately unranked because it is a
  coverage-vs-false-positive product call. **Current recommendation: skip it** — the measurement
  showed the gate is correctly tuned for the terminal-injected case, and Ctrl+V already covers every
  clipboard shape. Only worth doing if the user often copies images out of Finder.
- Whether Cmd+V-on-Warp should ship documented beyond the changelog line already added.

## 11. Key reports

All under `plans/260807-1457-terminal-clipboard-image-paste/reports/`:
`debug-260807-terminal-paste-root-cause.md` (cause + addendum) ·
`research-260807-cmdv-screenshot-helper-options.md` (helper costing) ·
`fix-260807-ctrlv-live-dispatch.md` (rounds 1-4) ·
`code-review-260807-ctrlv-live-dispatch.md` (round-1 REQUEST_CHANGES) ·
`code-review-260807-round3-final.md` (APPROVE_WITH_NITS) ·
`security-scan-260807-clipboard-paste.md` · `auto-decisions-260807-*.md`

## 12. Process lesson worth keeping

Two agent self-reports were wrong in ways that mattered: "only `KeyEventKind::Press` reaches this
function" (false — shipped a real regression, caught only because the reviewer was told to verify it
independently) and "3390 passed, 0 failed" (not reproducible). Both were *confident negative/absolute
claims*. Verify those specifically, and always demand a baseline comparison for test counts.
