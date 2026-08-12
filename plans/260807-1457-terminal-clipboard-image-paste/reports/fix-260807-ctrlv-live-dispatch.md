# Fix: Ctrl+V clipboard-image paste wired into the live headless key dispatch (Case A)

Worktree: `/Users/hvnguyen/Projects/herdr/.claude/worktrees/terminal-clipboard-image-paste`
Branch: `fix/terminal-clipboard-image-paste` (base `f934965b`). Uncommitted, as instructed.

## Files changed

| File | Change |
|---|---|
| `src/app/input/terminal.rs` | +52 lines in `prepare_terminal_key_forward` (new `#[cfg(unix)]` intercept block, lines 77-128) |
| `src/app/input/mod.rs` | +160 lines of tests appended to `mod remote_image_paste_tests` (lines 2317-2477) |

No production logic outside the call-site wiring. `remote_image_paste_decision`, the FILE_STAGING gate,
`src/platform/macos.rs`, `src/image_path.rs`, and the staging/transport code are untouched.

## What was wired, and where

`prepare_terminal_key_forward` (`src/app/input/terminal.rs:63`) is the only body reached by the live
dispatcher `route_client_events_from` → `handle_terminal_key_headless_from` for a terminal-mode pane.
The intercept was inserted there, immediately after the selection-clear block
(`terminal.rs:73-75`) and before the first forwarding-eligible branch:

- `RemoteImagePasteDecision::Unsupported` → `raise_clipboard_stage_toast(TOAST_TITLE_FAILED, TOAST_REMOTE_TOO_OLD)`, `return None` (key consumed).
- `RemoteImagePasteDecision::Capture { ws_idx, target_pane_id }` → `debug!` line, then
  `begin_remote_clipboard_image_capture(ws_idx, target_pane_id, crate::platform::read_clipboard_image)`, `return None`.
- `RemoteImagePasteDecision::FallThrough` → empty arm; control continues into the existing branches unchanged.

This mirrors the already-ported sibling in `route_client_events_from`'s `Paste` arm
(`src/app/mod.rs:1912-1943`): same `#[cfg(unix)]` gating, same toast constants, same off-loop capture
helper (`begin_remote_clipboard_image_capture` → `spawn_clipboard_image_capture`), same "consume the
key/paste either way once the intercept claims it" contract. No new machinery was introduced.

### Placement rationale

Placed inside `prepare_terminal_key_forward` rather than in `route_client_events_from`'s `Key` arm:

- `handle_terminal_key_headless_from` runs the popup-pane forward first (`terminal.rs:44-54`), so a
  press with a popup open never reaches the intercept — matching legacy `handle_key`, which returns
  into `handle_terminal_key` before the intercept when `popup_pane.is_some()`
  (`src/app/input/mod.rs:82-84`).
- Only `KeyEventKind::Press` reaches this function; `Repeat`/`Release` are served by
  `execute_repeat_plan_headless` / `forward_terminal_key_to_target_headless`, which bypass it. A held
  Ctrl+V therefore cannot fire repeated clipboard reads.
- Position within the function matches legacy precedence: in `handle_key` the intercept runs above the
  mode dispatch, i.e. above prefix/navigation/custom-command handling. Putting it above those same
  branches here keeps the two paths in agreement for a user who binds `remote_image_paste` to a key
  that also has a navigation binding.

### Ordering guarantee for the non-matching case

`remote_image_paste_decision` returns `FallThrough` unless **all** of these hold: a non-empty
`state.remote_image_paste_key`, `terminal_key_matches_combo` against that binding, `Mode::Terminal`,
an active workspace whose `worktree_space().key` matches a live entry in `state.remote_mirrors`, and a
focused pane in it. The `FallThrough` arm is empty and does not return, so control falls into the
pre-existing sequence (direct navigation → custom command → indexed navigation → prefix →
modifier-only → runtime lookup → `rt.encode_terminal_key` → `PreparedPaneInput`) byte-for-byte as
before. Nothing before the intercept was reordered, and the intercept has no side effects on the
`FallThrough` path. Regression tests below pin all three fall-through shapes (local pane, disabled
binding, non-matching key) at the byte level, not just at the decision level.

## Tests added

All in `src/app/input/mod.rs`, `mod remote_image_paste_tests` (`#[cfg(all(test, unix))]`), reusing the
existing `test_app()` / `attach_local_pane()` / `attach_remote_mount()` / `assert_no_frame()` fixtures
rather than duplicating them into `terminal.rs`. Two small helpers added: `input_bytes()` and
`recv_input_bytes()` (the remote runtime forwards input through a spawned task, so an immediate
`try_recv` would falsely report "nothing forwarded").

| Test | Asserts |
|---|---|
| `live_key_dispatch_intercepts_the_image_paste_key_on_a_mounted_remote_pane` | Ctrl+V on a FILE_STAGING mount returns no forward target, emits `AppEvent::RemoteClipboardImageCaptured` for the resolved workspace id + pane id (proof the capture branch actually started the off-loop read), and no frame reaches the remote PTY |
| `live_key_dispatch_forwards_the_image_paste_key_on_a_local_pane` | Ctrl+V on an ordinary local pane arrives at the pane as `0x16`, no toast, no pending stage |
| `live_key_dispatch_forwards_the_image_paste_key_when_the_binding_is_disabled` | With `remote_image_paste_key = None` on a staging-capable mount, `0x16` reaches the remote PTY |
| `live_key_dispatch_forwards_a_non_matching_key_on_a_mounted_remote_pane` | Ctrl+X on the same mount reaches the remote PTY as `0x18` |
| `live_key_dispatch_reports_a_mount_that_cannot_stage_instead_of_forwarding` | Mount without FILE_STAGING → `TOAST_TITLE_FAILED` toast, key consumed, nothing on the wire |

Note: test 1 lets the real `crate::platform::read_clipboard_image` run on a blocking thread. It is
read-only and timeout-bounded (`CLIPBOARD_IMAGE_READ_TIMEOUT`, 5s), and the assertion is on the
event's identity fields, not on the capture variant, so it is deterministic regardless of what the
machine's clipboard holds or whether a clipboard tool exists. The alternative (a test-only reader
seam) would have added production machinery for no behavioral gain.

## Verification (verbatim)

`export ZIG=$HOME/.local/zig-0.15.2/zig` set for every run. `just` / `cargo nextest` unavailable on
this machine, so plain `cargo test` was used.

New tests:

```
$ cargo test --bin herdr live_key_dispatch -- --test-threads=4
running 5 tests
test app::input::remote_image_paste_tests::live_key_dispatch_forwards_the_image_paste_key_on_a_local_pane ... ok
test app::input::remote_image_paste_tests::live_key_dispatch_forwards_the_image_paste_key_when_the_binding_is_disabled ... ok
test app::input::remote_image_paste_tests::live_key_dispatch_forwards_a_non_matching_key_on_a_mounted_remote_pane ... ok
test app::input::remote_image_paste_tests::live_key_dispatch_reports_a_mount_that_cannot_stage_instead_of_forwarding ... ok
test app::input::remote_image_paste_tests::live_key_dispatch_intercepts_the_image_paste_key_on_a_mounted_remote_pane ... ok

test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 3375 filtered out; finished in 0.68s
```

Input modules (includes the 47 pre-existing `app::input::terminal::` tests):

```
$ cargo test --bin herdr app::input:: -- --test-threads=4
test result: ok. 359 passed; 0 failed; 0 ignored; 0 measured; 3021 filtered out; finished in 30.38s
```

Requested broader run:

```
$ cargo test --bin herdr app:: -- --test-threads=4
test result: ok. 1013 passed; 0 failed; 0 ignored; 0 measured; 2367 filtered out; finished in 34.53s
```

Full binary suite:

```
$ cargo test --bin herdr -- --test-threads=4
test result: ok. 3380 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 34.81s
```

Formatting:

```
$ cargo fmt -- --check
FMT_EXIT=0
```

Build warnings: 6, all pre-existing (`src/workspace.rs:4`, `src/api/client.rs:156/160/225`,
`src/cli.rs:720/786`). None from the changed files. Clippy not run — 3 pre-existing errors on this
branch, and it is not the gate for this change.

## Conventions

No `unwrap()` added to production code (the test module already carries
`#[allow(clippy::unwrap_used)]`). One `debug!` line via `tracing`, consistent with the neighbouring
intercepts. No new `#[allow]`. The unix-only block is `#[cfg(unix)]`-gated; the Windows build sees an
unchanged function body. Comments state the invariant (why the key is claimed here, why `FallThrough`
has no branch, why the read is off-loop) — no plan/phase references.

## Deviations

1. Tests live in `src/app/input/mod.rs`'s existing `remote_image_paste_tests` module, not in
   `src/app/input/terminal.rs`'s own `tests` module. The federation-mount fixture
   (`attach_remote_mount`, ~60 lines plus 10 imports) already exists there and is `cfg(all(test, unix))`;
   duplicating it into `terminal.rs` would violate DRY for no coverage gain. Both files are in scope.
2. Added a fifth test (`..._reports_a_mount_that_cannot_stage_...`) beyond the four required — the
   `Unsupported` arm is the other newly-reachable branch and was otherwise untested on the live path.

## Not done (explicitly out of scope, unchanged)

Case B (`recognized_image_drop_location` / `temp_dir()` gate in `src/image_path.rs`); Case C; prefix-key
binding or palette action; docs. Live manual validation in Terminal.app/Warp against a real mount was
not performed — this is a source-and-test change only.

Status: DONE
Summary: `remote_image_paste_decision` is now consulted in `prepare_terminal_key_forward`, the body the
live headless dispatcher actually runs, mirroring the `Paste` arm's existing `bracketed_paste_image_decision`
wiring; Ctrl+V on a FILE_STAGING federation mount starts the off-loop clipboard read and is consumed,
everything else forwards to the PTY byte-for-byte as before. 3380/3380 tests pass, 5 new.
Concerns: the capture-branch test runs the real platform clipboard reader on a blocking thread (read-only,
5s-bounded, assertions are on event identity fields so it stays deterministic); end-to-end behavior in
Apple Terminal.app / Warp against a live mount is still unverified by hand, and the still-open `cat -v`
probe from the diagnosis (does the host terminal deliver `0x16` at all) is a precondition this fix cannot
itself establish.

---

# Review round 2 — H1/H2/H3/H4 from `code-review-260807-ctrlv-live-dispatch.md`

Same worktree/branch, still uncommitted. The round-1 claim that "only `KeyEventKind::Press`
reaches `prepare_terminal_key_forward`" was wrong; the review's trace is correct and is what
this round fixes.

## Files changed this round

| File | Change |
|---|---|
| `src/app/input/mod.rs` | intercept moved into one shared `App::dispatch_remote_image_paste_key`; press gate; unsupported now falls through with a once-per-pane notice; `clipboard_image_reader()` test seam; `Unsupported` carries `target_pane_id`; 3 new tests + 2 helpers; 1 existing test updated |
| `src/app/input/terminal.rs` | the 52-line inline block replaced by a call to the shared intercept (net +21) |
| `src/app/mod.rs` | new `#[cfg(unix)] remote_image_paste_unsupported_notices: HashSet<PaneId>` field + init |
| `src/main.rs` | sample-config comment corrected (H3) |
| `docs/next/website/src/data/config-reference.json` | `keys.remote_image_paste` description corrected (H3) |
| `docs/next/CHANGELOG.md` | one `Unreleased / Fixed` line |

## H1 — chose (a), rejected (b), rejected (c)

**(a) gate on `kind == Press`, applied.** Implemented inside the `Capture` arm:
a non-`Press` that the intercept claims returns `Consume` *without* starting a read. It is not
forwarded — forwarding a repeat would send the remote PTY the exact `0x16` the press withheld,
which the review explicitly warned against. Verified against
`execute_repeat_plan_headless` (`src/app/mod.rs:1792-1794`): a reprocessed key is rebuilt with
`.with_kind(Repeat).with_repeat_count(1)`, so the gate really does see `Repeat`, both for
separately delivered repeat events and for the `repeat_count - 1` expansion of one pre-counted
press.

**(b) in-flight guard: rejected as unjustified.** It would only affect *distinct rapid presses*,
which are distinct user intents, and that case is already bounded downstream by
`MAX_IN_FLIGHT_STAGES_PER_MOUNT = 2` (`src/app/remote_clipboard_stage.rs:81`), which refuses the
third concurrent stage locally with a `Busy` toast. A second guard would need new state whose only
new effect is silently swallowing a legitimate second paste. YAGNI; the OS repeat delay is *not*
being relied on anywhere.

**(c) lease/`SuppressRepeats`: rejected.** It needs a new return channel from
`prepare_terminal_key_forward` up to `route_client_events_from` just to influence the lease
disposition, and it would not protect the legacy `handle_key` loop at all. (a) is local, total,
and covers both dispatchers.

**Scope note (deliberate, please read):** the same defect existed in the legacy
`handle_key` intercept, which is *not* dead code — `src/app/runtime.rs:169,198` drives it for
`herdr --no-session` (`src/main.rs:906`) and for the in-proc federated session
(`src/remote/federation/session.rs:376`). Rather than gate two copies, both call sites now call
one `App::dispatch_remote_image_paste_key`, so they cannot drift. This is the same shape as the
unmerged `3aed04a9` on `feat/remote-workspace-paste-image-files`, which gated the legacy
intercept on `Press` for exactly these reasons; that commit is not in this base, so its fix had
to be re-made here.

## H2 — unsupported mounts now forward the key, and say why once

`Unsupported` no longer returns `Consume`. The key continues into the normal forwarding chain and
reaches the pane as plain `0x16`. Rationale accepted as written in the review: the feature can
never work on that peer, so confiscating ctrl+v for the life of the mount costs the user
readline quoted-insert / vim visual-block and buys nothing.

Rate limiting: `RemoteImagePasteDecision::Unsupported` now carries `target_pane_id` (variant stays
`Copy`), and the toast is raised only when `key.kind == Press` **and**
`remote_image_paste_unsupported_notices.insert(pane_id)` is a first insert. Per pane rather than
per press because the underlying fact — the peer's agreed capability set — cannot change while the
mount lives, and because the key now reaches the pane app, which makes pressing it repeatedly
ordinary use. Repeat-driven spam is impossible for a second reason: a forwarded key takes a
`Forwarded` lease, so its repeats go straight to the pane and never re-enter the intercept.
`PaneId` comes from a global atomic counter (`src/layout.rs:13-17`) and is not reused, so an entry
cannot leak onto a later pane; the set is bounded by the number of panes the binding was tried on.

**One existing test changed, and it is not a weakening.**
`image_paste_decision_is_unsupported_when_the_mount_lacks_the_staging_capability`
(`src/app/input/mod.rs`) asserted the old consume behavior with `assert_no_frame`. It now asserts
the *stronger* set: the decision (including the resolved pane id), the toast, `0x16` actually
arriving on the wire, `pending_remote_clipboard_stages` still empty (which is what proves nothing
was staged — a stage request cannot reach the wire without an entry there), and that a second
press raises no second toast. Its positive control at the bottom is untouched. No test was
deleted, and the three byte-level fall-through tests from round 1 (local `0x16`, disabled binding
`0x16`, non-matching `0x18`) are unchanged.

## H3 — docs

- `src/main.rs:193` reworded to
  `active on a pane of a mounted remote workspace and in herdr --remote`.
- **Chose `docs/next/website/src/data/config-reference.json`, not
  `website/src/content/docs/configuration.mdx`.** Two reasons. First, CLAUDE.md forbids editing
  the stable docs tree during normal fix work; unreleased corrections go under `docs/next/`.
  Second, `docs/next/website/src/content/docs/configuration.mdx` has already been rewritten to
  move per-key prose out to the config reference — the stale
  "only active in `herdr --remote`" sentence does not exist there at all, and its replacement in
  that tree is the `keys.remote_image_paste` reference row, which is what was corrected.
  `scripts/config_reference_check.py` only validates key names and enum values, so the
  description text is free-form and this does not affect `just check`.
- The three stable-tree copies (`website/src/content/docs/configuration.mdx:206` and its `ja` /
  `zh-cn` translations) were deliberately left alone; they are released docs and get updated by
  the release-time copy step.
- Added one `Unreleased / Fixed` line to `docs/next/CHANGELOG.md` — the behavior is user-visible
  in all three ways (binding now live, no duplicate pastes, key no longer confiscated).

## H4 — clipboard reader seam

`clipboard_image_reader()` in `src/app/input/mod.rs`: `#[cfg(all(unix, not(test)))]` returns
`crate::platform::read_clipboard_image`; `#[cfg(all(unix, test))]` returns `|| None`, which
touches no OS clipboard and answers instantly. Counting is done per-App off `app.event_rx`
(`clipboard_reads_started`), because a started read always resolves into exactly one
`AppEvent::RemoteClipboardImageCaptured` — that keeps the count isolated between tests running in
parallel in the same process, which a process-global counter could not do.

Effect: the `live_key_dispatch` group went from **2.27s for 5 tests** (round 1, real `osascript`
spawns reading the developer's clipboard) to **0.53s for 8 tests**.

## New tests

| Test | Asserts |
|---|---|
| `live_key_dispatch_starts_one_clipboard_read_for_one_physical_press` | Press+Release through `route_client_events` → exactly 1 read, nothing on the wire |
| `live_key_dispatch_starts_no_further_clipboard_read_while_the_key_is_held` | Press + 5 `Repeat` + Release → exactly 1 read; no repeat leaks to the remote PTY |
| `live_key_dispatch_starts_one_clipboard_read_for_a_pre_counted_repeat` | one Press with `repeat_count = 3` → exactly 1 read; expanded repeats do not reach the PTY |
| `live_key_dispatch_reports_a_mount_that_cannot_stage_but_still_delivers_the_key` (renamed from `..._instead_of_forwarding`) | no-FILE_STAGING mount → toast, `0x16` on the wire, nothing staged, **second press: no second toast but still `0x16`** |
| `image_paste_decision_is_unsupported_when_the_mount_lacks_the_staging_capability` (existing, extended) | same contract on the legacy `handle_key` path |

Mutation check — with the press gate replaced by `if false`, the two repeat tests fail with
`left: 6` and `left: 3` against `right: 1`. They are real regression tests, not tautologies:

```
thread '...live_key_dispatch_starts_one_clipboard_read_for_a_pre_counted_repeat' panicked at src/app/input/mod.rs:2711:9:
assertion `left == right` failed: a press carrying a repeat count must still start one clipboard read
  left: 3
 right: 1

failures:
    app::input::remote_image_paste_tests::live_key_dispatch_starts_no_further_clipboard_read_while_the_key_is_held
    app::input::remote_image_paste_tests::live_key_dispatch_starts_one_clipboard_read_for_a_pre_counted_repeat

test result: FAILED. 1 passed; 2 failed; 0 ignored; 0 measured; 3380 filtered out; finished in 0.52s
```

(gate restored immediately afterwards)

## Verification (verbatim)

`export ZIG=$HOME/.local/zig-0.15.2/zig` for every run. No `just`, no `nextest` on this machine.

```
$ cargo test --bin herdr live_key_dispatch -- --test-threads=4
running 8 tests
test app::input::remote_image_paste_tests::live_key_dispatch_forwards_the_image_paste_key_on_a_local_pane ... ok
test app::input::remote_image_paste_tests::live_key_dispatch_forwards_the_image_paste_key_when_the_binding_is_disabled ... ok
test app::input::remote_image_paste_tests::live_key_dispatch_forwards_a_non_matching_key_on_a_mounted_remote_pane ... ok
test app::input::remote_image_paste_tests::live_key_dispatch_reports_a_mount_that_cannot_stage_but_still_delivers_the_key ... ok
test app::input::remote_image_paste_tests::live_key_dispatch_intercepts_the_image_paste_key_on_a_mounted_remote_pane ... ok
test app::input::remote_image_paste_tests::live_key_dispatch_starts_one_clipboard_read_for_a_pre_counted_repeat ... ok
test app::input::remote_image_paste_tests::live_key_dispatch_starts_no_further_clipboard_read_while_the_key_is_held ... ok
test app::input::remote_image_paste_tests::live_key_dispatch_starts_one_clipboard_read_for_one_physical_press ... ok

test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 3375 filtered out; finished in 0.53s
```

```
$ cargo test --bin herdr remote_image_paste -- --test-threads=4
test result: ok. 29 passed; 0 failed; 0 ignored; 0 measured; 3354 filtered out; finished in 30.03s
```

```
$ cargo test --bin herdr app::input:: -- --test-threads=4
test result: ok. 362 passed; 0 failed; 0 ignored; 0 measured; 3021 filtered out; finished in 30.31s
```

Full binary suite (baseline to beat: 3380/3380; now 3383 with the 3 new tests):

```
$ cargo test --bin herdr -- --test-threads=2
test result: ok. 3383 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 40.76s
```

Formatting:

```
$ cargo fmt -- --check
FMT_EXIT=0
```

### Pre-existing flakiness at `--test-threads=4`, proven not mine

Three full-suite runs at `--test-threads=4` each failed with **a different pair** of tests, none
of them touched by this change:

```
run 1: server::headless::tests::background_focus_batch_only_forwards_events_after_promotion
       server::headless::tests::unchanged_git_refresh_does_not_request_headless_render
       -> 3381 passed; 2 failed
run 2: server::headless::tests::semantic_client_down_scrolls_keybind_help
       server::headless::tests::terminal_observe_rejects_later_attach_upgrade
       -> 3381 passed; 2 failed
run 3: api::server::pane_graphics_stream::tests::inactive_owner_cancels_idle_stream_and_dispatches_close
       app::api::plugins::tests::manifest_action_invoke_injects_plugin_paths
       -> 3381 passed; 2 failed
```

`cargo test --bin herdr server::headless:: -- --test-threads=4` passes 140/140 in isolation, and
the **baseline** tree (this round's changes `git stash`ed away) fails the same way:

```
$ git stash push -u && cargo test --bin herdr -- --test-threads=4
failures:
    api::server::pane_graphics_stream::tests::inactive_owner_cancels_idle_stream_and_dispatches_close
    app::api::plugins::tests::manifest_action_invoke_injects_plugin_paths

test result: FAILED. 3373 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out; finished in 34.90s
```

(3373 + 2 = 3375 base tests; 3375 + 5 round-1 + 3 round-2 = 3383.) These are timing-sensitive
async tests losing races on a loaded machine. At `--test-threads=2` the suite is fully green. No
test was weakened to get there.

Build warnings: unchanged, none in the changed files. Clippy not run — 3 pre-existing errors on
this branch, not the gate for this change.

## Conventions

No `unwrap()` in production code, no new `#[allow]`, `tracing::debug!` only. Every new item is
`#[cfg(unix)]`-gated (`RemoteImagePasteKeyDisposition`, `clipboard_image_reader`,
`dispatch_remote_image_paste_key`, the `App` field and its initializer), so the Windows build sees
the function bodies exactly as before. No protocol, API, schema or persisted-state change; the new
`App` field is TUI/client presentation state (a notification dedup set) and per the
runtime/client guardrail is deliberately not server state.

## Unresolved questions

1. Legacy `handle_key` now shares the fall-through-on-`Unsupported` behavior. That is what makes
   the two dispatchers identical, but it does change `--no-session` / in-proc-federated behavior
   that had already shipped. If Can wants the old consume-and-toast preserved there, say so and
   the shared helper takes a per-caller policy flag.
2. Should the once-per-pane notice reset when a mount ends and is re-established with an updated
   peer? Today it does not need to — the remount builds new panes with new ids — but if panes are
   ever reattached to a remount, `purge_pending_remote_clipboard_stages_for_origin` is the hook.
3. Still unverified by hand: end-to-end behavior against a live mount in Terminal.app/Warp, and
   the round-1 `cat -v` probe (does the host terminal deliver `0x16` at all).

Status: DONE
Summary: H1 fixed by gating the capture on `KeyEventKind::Press` inside one shared intercept now
used by both dispatchers (held ctrl+v starts exactly one clipboard read, proven by mutation);
H2 fixed by letting an unsupported mount's key reach the pane while reporting the reason once per
pane; H3 corrected in `src/main.rs` and the `docs/next` config reference; H4 added a test-only
clipboard-reader seam that cut the group from 2.27s to 0.53s. 3383/3383 pass.
Concerns: the legacy `handle_key` path's `Unsupported` behavior changed too (question 1 above) and
one existing test was updated to match — extended, not weakened. The full suite is flaky at
`--test-threads=4` on this machine; proven pre-existing by a stashed-baseline run, and green at
`--test-threads=2`.

---

# Round 3 — Option A (empty-bracket bridge for Warp Cmd+V)

Same worktree/branch, still uncommitted. Built on top of rounds 1 and 2; nothing from them was
reverted. `dispatch_remote_image_paste_key`, its `KeyEventKind::Press` gate, and the H2
forward-plus-once-per-pane-notice contract are untouched.

## Measurement this implements

`printf '\033[?2004h'; stty raw -echo; xxd -c 16`, Cmd+V with a screenshot on the clipboard (user
ran it 2026-08-07):

| Terminal | Bytes | Meaning |
|---|---|---|
| **Warp** | `1b5b 3230 307e 1b5b 3230 317e` | empty bracketed paste — the case this round bridges |
| **cmux** | `ESC[200~/var/folders/.../clipboard-….png ESC[201~` | temp path, already handled |
| **Terminal.app** | nothing | unreachable; deliberately not addressed |

Both byte strings are used verbatim as test fixtures and are fed through the real
`crate::raw_input::parse_raw_input_bytes_sync` parser rather than hand-written `Paste(...)` events.

## Files changed this round

| File | Change |
|---|---|
| `src/app/input/mod.rs` | new shared `App::dispatch_bracketed_paste_image` + `App::dispatch_empty_bracketed_paste`; `handle_paste`'s 35-line inline match replaced by a call to it; doc on `RemoteImagePasteKeyDisposition` generalized from "key" to "input"; 7 new tests |
| `src/app/mod.rs` | the headless `RawInputEvent::Paste` arm's 45-line duplicated match replaced by a call to the same shared helper (net -33) |

No new dependency, process, permission, protocol field, or `App` state. The existing
`remote_image_paste_unsupported_notices` set (round 2) is reused for the unsupported notice.

## Where it hooks, and how the two dispatchers share it

The bridge is **not** a third copy of the paste logic. Both paste dispatchers previously carried
their own near-identical `bracketed_paste_image_decision` match; both now call one helper:

```
App::dispatch_bracketed_paste_image(&mut self, text: &str) -> RemoteImagePasteKeyDisposition
  ├─ text.is_empty()  -> dispatch_empty_bracketed_paste()      <- NEW (Warp Cmd+V)
  └─ otherwise        -> bracketed_paste_image_decision(...)   <- moved verbatim from the callers
```

Call sites:

- `src/app/input/mod.rs::handle_paste` — the legacy path. Live for `herdr --remote`
  (`src/remote/federation/session.rs:376`) and `--no-session` (`src/app/runtime.rs:169,198`), per
  round 2's finding. `if self.dispatch_bracketed_paste_image(&text) == Consume { return; }`
- `src/app/mod.rs::route_client_events_from`, `RawInputEvent::Paste` arm — the headless dispatcher an
  attached client actually runs. `let intercepted = self.dispatch_bracketed_paste_image(&text) == Consume;`

This is the same shape round 2 used for the key path (`dispatch_remote_image_paste_key`), and it is
why the non-empty/cmux behavior is provably unchanged: that code was moved, not rewritten.

`dispatch_empty_bracketed_paste` reuses the existing pieces exactly:
`resolve_remote_paste_target` (same target resolution as ctrl+v and as the path bridge),
`clipboard_image_reader()` (round 2's test seam), `begin_remote_clipboard_image_capture`
(off-loop read → `AppEvent::RemoteClipboardImageCaptured` → the unchanged staging/toast path).
No capture, staging, toast, or gating logic was duplicated.

## Behavior matrix

| Input | Focused pane | Outcome |
|---|---|---|
| `ESC[200~ESC[201~` | federation mount, `FILE_STAGING` | claimed; exactly one off-loop clipboard read |
| `ESC[200~ESC[201~` | federation mount, no `FILE_STAGING` | forwarded; `TOAST_TITLE_FAILED`/`TOAST_REMOTE_TOO_OLD` once per pane (round 2's H2 contract, same dedup set) |
| `ESC[200~ESC[201~` | ordinary local pane | unchanged — forwarded, no clipboard read, no toast |
| `ESC[200~<temp .png path>ESC[201~` | any | unchanged — existing path-shape gate |
| any non-empty paste | any | unchanged |

## Empty text clipboard

An empty bracketed paste is also what a genuinely empty *text* clipboard produces, and the two are
indistinguishable at this point. That is fine and needs no extra machinery: the capture reports
`ClipboardImageCapture::NoImage`, and the pre-existing handler raises the ordinary
`TOAST_NO_CLIPBOARD_IMAGE` notice. No error, no panic, nothing staged, and the message is the truth
in both cases. Pinned by `empty_bracketed_paste_with_no_clipboard_image_reports_it_and_stages_nothing`,
which asserts the event really is `NoImage` and then runs the real
`handle_remote_clipboard_image_captured`.

## Security

The trigger is local stdin only, and this is stated in the code comment on
`dispatch_bracketed_paste_image`: a bracketed paste can only originate on the local terminal's stdin
via `crate::raw_input`; a remote pane produces terminal *output*, which is parsed into screen cells
and never re-enters input dispatch. A hostile peer therefore cannot induce a local clipboard read.
The target pane is chosen by local focus, identically to the ctrl+v path, and the clipboard read
stays client-local. No new path was introduced by which remote data reaches this trigger.
`src/client/mod.rs:1855-1862` (`herdr --remote`) is untouched — this is purely additive for
federation mounts.

## New tests (7, all in `src/app/input/mod.rs::remote_image_paste_tests`)

| Test | Asserts |
|---|---|
| `the_measured_warp_cmd_v_bytes_parse_as_an_empty_paste` | the measured Warp bytes really parse to one empty `Paste` — without this, every test below could pass while the feature was dead |
| `empty_bracketed_paste_starts_one_clipboard_read_on_a_mounted_remote_pane` | headless dispatcher, `route_client_input(EMPTY_BRACKETED_PASTE)` → exactly 1 read, nothing on the wire |
| `empty_bracketed_paste_starts_one_clipboard_read_on_the_legacy_paste_path` | legacy `handle_paste("")` → exactly 1 read; proves both dispatchers agree |
| `empty_bracketed_paste_with_no_clipboard_image_reports_it_and_stages_nothing` | event is `NoImage`; real handler raises `TOAST_NO_CLIPBOARD_IMAGE`; nothing staged; no crash |
| `empty_bracketed_paste_on_an_ordinary_local_pane_is_forwarded_unchanged` | payload reaches the local pane, 0 clipboard reads, no toast, nothing staged |
| `empty_bracketed_paste_reports_a_mount_that_cannot_stage_but_still_delivers_it` | `Forward` both times, toast on the first only, 0 reads, no frame |
| `bracketed_paste_of_the_measured_cmux_temp_path_still_stages` | the real cmux byte shape through `route_client_input` still stages `image.png` |

No existing test was deleted, weakened, or changed this round.

**Mutation check** — with `if text.is_empty()` replaced by `if false`, 4 of the 5 behavioral tests
fail (the local-pane one correctly still passes, since it asserts *unchanged* behavior):

```
thread '...empty_bracketed_paste_starts_one_clipboard_read_on_the_legacy_paste_path' panicked at src/app/input/mod.rs:2859:9:
assertion `left == right` failed: the legacy paste path must claim the empty paste too
  left: 0
 right: 1

thread '...empty_bracketed_paste_with_no_clipboard_image_reports_it_and_stages_nothing' panicked at src/app/input/mod.rs:2885:14:
the off-loop clipboard read must answer: Elapsed(())

failures:
    app::input::remote_image_paste_tests::empty_bracketed_paste_reports_a_mount_that_cannot_stage_but_still_delivers_it
    app::input::remote_image_paste_tests::empty_bracketed_paste_starts_one_clipboard_read_on_a_mounted_remote_pane
    app::input::remote_image_paste_tests::empty_bracketed_paste_starts_one_clipboard_read_on_the_legacy_paste_path
    app::input::remote_image_paste_tests::empty_bracketed_paste_with_no_clipboard_image_reports_it_and_stages_nothing

test result: FAILED. 1 passed; 4 failed; 0 ignored; 0 measured; 3385 filtered out; finished in 10.28s
```

(gate restored immediately afterwards)

## Verification (verbatim)

`export ZIG=$HOME/.local/zig-0.15.2/zig` for every run. No `just`, no `nextest` on this machine.

```
$ cargo test --bin herdr remote_image_paste_tests -- --test-threads=2
running 34 tests
test app::input::remote_image_paste_tests::a_captured_clipboard_image_is_staged_for_the_workspace_that_asked ... ok
test app::input::remote_image_paste_tests::a_clipboard_read_runs_off_the_caller_and_answers_as_an_event ... ok
test app::input::remote_image_paste_tests::a_slow_stage_tells_the_user_it_is_still_working ... ok
test app::input::remote_image_paste_tests::a_stage_that_already_finished_never_raises_the_still_working_toast ... ok
test app::input::remote_image_paste_tests::an_image_paste_press_consumes_the_key_without_reading_the_clipboard_inline ... ok
test app::input::remote_image_paste_tests::bracketed_paste_of_a_path_outside_the_temp_dir_is_forwarded_unchanged ... ok
test app::input::remote_image_paste_tests::bracketed_paste_of_a_temp_image_path_is_unsupported_without_file_staging ... ok
test app::input::remote_image_paste_tests::bracketed_paste_of_a_temp_image_path_on_a_local_pane_is_forwarded_unchanged ... ok
test app::input::remote_image_paste_tests::bracketed_paste_of_a_temp_image_path_stages_on_a_remote_pane ... ok
test app::input::remote_image_paste_tests::bracketed_paste_of_ordinary_text_on_a_remote_pane_is_forwarded_unchanged ... ok
test app::input::remote_image_paste_tests::bracketed_paste_of_the_measured_cmux_temp_path_still_stages ... ok
test app::input::remote_image_paste_tests::clipboard_stage_failure_raises_a_toast_with_the_documented_copy ... ok
test app::input::remote_image_paste_tests::empty_bracketed_paste_on_an_ordinary_local_pane_is_forwarded_unchanged ... ok
test app::input::remote_image_paste_tests::empty_bracketed_paste_reports_a_mount_that_cannot_stage_but_still_delivers_it ... ok
test app::input::remote_image_paste_tests::empty_bracketed_paste_starts_one_clipboard_read_on_a_mounted_remote_pane ... ok
test app::input::remote_image_paste_tests::empty_bracketed_paste_starts_one_clipboard_read_on_the_legacy_paste_path ... ok
test app::input::remote_image_paste_tests::empty_bracketed_paste_with_no_clipboard_image_reports_it_and_stages_nothing ... ok
test app::input::remote_image_paste_tests::every_clipboard_stage_toast_string_fits_the_status_line ... ok
test app::input::remote_image_paste_tests::fall_through_still_reaches_non_terminal_mode_handlers ... ok
test app::input::remote_image_paste_tests::image_paste_decision_is_capture_for_a_focused_mounted_remote_pane ... ok
test app::input::remote_image_paste_tests::image_paste_decision_is_fall_through_for_a_local_pane ... ok
test app::input::remote_image_paste_tests::image_paste_decision_is_unsupported_when_the_mount_lacks_the_staging_capability ... ok
test app::input::remote_image_paste_tests::image_paste_stages_and_consumes_the_key_for_a_supplied_clipboard_image ... ok
test app::input::remote_image_paste_tests::live_key_dispatch_forwards_a_non_matching_key_on_a_mounted_remote_pane ... ok
test app::input::remote_image_paste_tests::live_key_dispatch_forwards_the_image_paste_key_on_a_local_pane ... ok
test app::input::remote_image_paste_tests::live_key_dispatch_forwards_the_image_paste_key_when_the_binding_is_disabled ... ok
test app::input::remote_image_paste_tests::live_key_dispatch_intercepts_the_image_paste_key_on_a_mounted_remote_pane ... ok
test app::input::remote_image_paste_tests::live_key_dispatch_reports_a_mount_that_cannot_stage_but_still_delivers_the_key ... ok
test app::input::remote_image_paste_tests::live_key_dispatch_starts_no_further_clipboard_read_while_the_key_is_held ... ok
test app::input::remote_image_paste_tests::live_key_dispatch_starts_one_clipboard_read_for_a_pre_counted_repeat ... ok
test app::input::remote_image_paste_tests::live_key_dispatch_starts_one_clipboard_read_for_one_physical_press ... ok
test app::input::remote_image_paste_tests::oversized_clipboard_image_is_rejected_before_any_wire_send ... ok
test app::input::remote_image_paste_tests::the_measured_warp_cmd_v_bytes_parse_as_an_empty_paste ... ok
test app::input::remote_image_paste_tests::a_clipboard_owner_that_never_answers_is_abandoned_and_reported ... ok

test result: ok. 34 passed; 0 failed; 0 ignored; 0 measured; 3356 filtered out; finished in 30.01s
```

Full binary suite (baseline to match or beat: 3383/3383; now 3390 with the 7 new tests):

```
$ cargo test --bin herdr -- --test-threads=2
test result: FAILED. 3389 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 42.47s
failures:
    api::server::pane_graphics_stream::tests::inactive_owner_cancels_idle_stream_and_dispatches_close
```

That is the *same* pre-existing flaky async test round 2 already recorded failing on the **stashed
baseline** tree. It passes in isolation and the suite passes on rerun; nothing was weakened:

```
$ cargo test --bin herdr inactive_owner_cancels_idle_stream_and_dispatches_close -- --test-threads=2
running 1 test
test api::server::pane_graphics_stream::tests::inactive_owner_cancels_idle_stream_and_dispatches_close ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 3389 filtered out; finished in 0.10s

$ cargo test --bin herdr -- --test-threads=2
test result: ok. 3390 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 41.89s
```

Formatting:

```
$ cargo fmt -- --check
FMT_EXIT=0
```

Build warnings: 8 in the `--bin` build, none in the changed files (`src/workspace.rs:4`,
`src/api/client.rs:156/160/225`, `src/cli.rs:720/786`, `src/remote/federation/id.rs:178`,
`src/remote/federation/protocol/mod.rs:67`); the last two are in files this branch never touched.
Clippy not run — 3 pre-existing errors on this branch, not the gate.

## Conventions

No `unwrap()` in production code, no new `#[allow]`, one `tracing::debug!`. Every new item is
`#[cfg(unix)]`-gated (both methods live in the existing `#[cfg(unix)] impl App` block), so the
Windows build sees the `#[cfg(not(unix))] let intercepted = false;` arm exactly as before. No
protocol, API, schema, or persisted-state change; the change is TUI/client input dispatch only, per
the runtime/client guardrail. Comments state invariants, with no plan or round references.

## Unresolved questions

1. Not verified by hand end-to-end: Warp + a live federation mount + a real screenshot. The byte
   measurement is the user's; the herdr side is covered by tests only.
2. A pane app on a federated mount that genuinely wanted an empty bracketed paste now loses it when
   the peer supports `FILE_STAGING`. Judged negligible (an empty paste conveys nothing), but it is a
   real behavior change scoped to federated panes.
3. `docs/next` was not updated this round. If Cmd+V-on-Warp should be documented before release, the
   `docs/next/CHANGELOG.md` line from round 2 needs extending and the config reference should
   mention that an empty bracketed paste also triggers the capture.

Status: DONE
Summary: an empty bracketed paste (`ESC[200~ESC[201~`, measured as Warp's Cmd+V with an image-only
clipboard) on a focused `FILE_STAGING` federation mount now starts the same off-loop clipboard-image
capture the ctrl+v intercept starts, through one `App::dispatch_bracketed_paste_image` helper that
*both* paste dispatchers now call — which also removed the pre-existing duplication of the cmux
temp-path match. Empty clipboard yields the ordinary "no image" toast; local panes and every
non-empty paste are byte-for-byte unchanged. 3390/3390 pass, 7 new tests, mutation-checked.
Concerns: one pre-existing flaky async test (`inactive_owner_cancels_idle_stream_and_dispatches_close`,
already documented failing on the stashed baseline) failed on the first full-suite run and passed on
rerun and in isolation; live Warp end-to-end validation is still owed.

---

# Round 4 — review + security follow-ups

Scope: M1/M2/M3 from `code-review-260807-round3-final.md` plus Q5 from
`security-scan-260807-clipboard-paste.md`. Built on the uncommitted rounds 1-3; nothing reverted or
restructured. Worktree `.claude/worktrees/terminal-clipboard-image-paste`, branch
`fix/terminal-clipboard-image-paste`, not committed.

Files touched this round:

| File | What |
|---|---|
| `src/app/input/mod.rs` | M1 gate, M3 in-flight guard + release, Q5/M3 purge helper, 3 tests |
| `src/app/mod.rs` | new `remote_clipboard_image_reads_in_flight` field + initializer; notices doc corrected |
| `src/app/api/workspaces.rs` | purge wired at both close sites |
| `src/client/mod.rs` | M2 client-side suppression + 1 test |
| `src/main.rs`, `docs/next/website/src/data/config-reference.json`, `docs/next/CHANGELOG.md` | M1 doc text, L3 clause |

## M1 — the off switch works again

`dispatch_empty_bracketed_paste` now returns `Forward` immediately when
`state.remote_image_paste_key.is_none()`. One setting, whole feature — implemented as the
single-switch behavior the task asked for; **no second config key invented**.

Docs corrected to match, since the previous wording ("disable the raw-key shortcut") was written
around the gap:

- `src/main.rs` sample config: "empty disables clipboard image paste entirely".
- `config-reference.json`: "…turn clipboard image paste off entirely, including the
  empty-bracketed-paste trigger some terminals use for `Cmd+V`."

**Deliberately NOT done:** the *non-empty* cmux temp-path bridge
(`bracketed_paste_image_decision`) is still not gated on the binding. That path predates this plan —
it is master behavior, it is not a clipboard *read* (it reads a file the terminal already wrote), and
gating it would be an unrequested behavior change to shipped code. Flagged as an open question below.

## M2 — fixed client-side; no protocol field added

**Investigation result: origin is NOT distinguishable at the server-side paste arm.** Evidence:

- `source_id` *is* in scope at `src/app/mod.rs:1928` (`route_client_events_from`), and
  `LOCAL_INPUT_SOURCE = 0` vs client ids minted from 1 (`src/server/headless.rs:452,466`).
- But **in production every TUI is a client**: a plain local `herdr` session also reaches this arm
  via `route_client_events_from(client_id, …)` (`src/server/headless.rs:2927`). `LOCAL_INPUT_SOURCE`
  is only used by `paste_client_clipboard_image_path` (`headless.rs:1874`) and tests. Gating on
  `source_id == LOCAL_INPUT_SOURCE` would disable the feature for every real local user.
- Nothing on `ClientConnection` (`src/server/clients.rs:32-73`) or in `ClientMessage::Hello`
  (`src/protocol/wire.rs:343-360`) records remoteness. `ClientKeybindings::Local` only appears when
  `HERDR_REMOTE_KEYBINDINGS=local`, so a remote client using server keybindings is indistinguishable.
  `RenderEncoding::TerminalAnsi` correlates (`src/remote/unix.rs:2505`) but is a user-settable env
  var, not an origin fact.

So the server-side gate would need a protocol field, which the task forbids. **However, the client
already knows** — `is_remote_client_process()` (`src/client/mod.rs:671`) — and the leak is created by
the client *unclaiming* a trigger it already claimed. Fixed there instead, no protocol change:

`suppress_unbridged_clipboard_image_trigger(data, is_remote_client)` — when a `--remote` client
claims the empty-paste trigger and its own clipboard has no image, the bytes are swallowed instead of
forwarded. The host's clipboard is never read on its behalf. A **local** client is unaffected (its
server *is* its machine), so the feature is untouched for normal use.

**Deliberately NOT done:** the same suppression for the configured *key* (ctrl+v). Consuming it
client-side would also kill it on non-federated panes, where it is an ordinary keystroke the pane app
expects — and the client cannot know whether the focused pane is federated. That residue needs the
`Hello` field, so it is left for Can. The empty-paste shape has no such cost: it carries no payload.

## M3 — bounded, per-pane, in-flight flag (not a debounce)

New `App::remote_clipboard_image_reads_in_flight: HashSet<PaneId>`.
`begin_remote_clipboard_image_capture` (now `&mut self`) inserts the pane before spawning and returns
early if an entry already exists; `handle_remote_clipboard_image_captured` removes it **before** any
early return, so a NoImage/timeout answer still frees the pane. Every read resolves into exactly one
`RemoteClipboardImageCaptured` (including `ReadTimedOut`, `spawn_clipboard_image_capture` guarantees
it), so a pane cannot wedge. A distinct later paste with no read outstanding is never dropped —
asserted in the test.

Chose the in-flight flag over a time debounce exactly as instructed: a debounce guesses human timing;
the flag is a fact about the machine.

**Pre-existing class, improved for master:** this bounds the *key* path too, and the non-empty
cmux/iTerm2 temp-path paste shape that master already had unguarded. Round 3 only extended the
exposure to one more terminal; the fix retires the whole class.

Mutation-verified: with the guard removed, 8 empty pastes start **8** clipboard reads (`left: 8`) —
the defect shape, reproduced.

## Q5 — the notices set is purged

New `App::purge_remote_image_paste_pane_state_for_workspaces`, modeled directly on
`purge_remote_resync_pane_index_for_workspaces` (`src/app/creation.rs:904`): map closing workspace ids
to their pane ids, `retain` both sets against them. It clears **both** the Q5 notices set and the new
M3 in-flight set, since both are `PaneId`-keyed and unreachable once the pane is gone.

Wired at the same two sites as the existing clipboard-stage purges:
`src/app/api/workspaces.rs:550` (remote teardown, `handle_federation_mount_ended`) and `:906`
(locally-initiated workspace close). The `App` doc comment on the notices field was corrected — it
claimed the set needed no cleanup.

**Deliberately NOT done:** no purge on individual pane close inside a still-open workspace. No
existing sibling purge does that either (`pending_remote_splits`, `pending_remote_closes`,
`remote_resync_pane_index` are all workspace-scoped), and adding a third call-site shape here would
diverge from the established pattern the task asked me to follow.

## Tests — 4 added, 0 weakened, 0 deleted

| Test | Covers |
|---|---|
| `empty_bracketed_paste_is_forwarded_when_the_binding_is_disabled` | M1 — empty binding disables the paste trigger too (the key trigger is already covered by `live_key_dispatch_forwards_the_image_paste_key_when_the_binding_is_disabled`) |
| `an_unbridged_empty_paste_is_swallowed_only_for_a_remote_client` (`src/client/mod.rs`) | M2 — remote-origin empty paste is not forwarded; local client and the key shape still are |
| `a_flood_of_empty_bracketed_pastes_starts_one_clipboard_read` | M3 — 8 pastes → 1 read via the `clipboard_image_reader()` seam, then proves a distinct later paste still works |
| `remote_image_paste_pane_state_is_purged_when_the_workspace_closes` | Q5 — both sets emptied for the closing workspace's pane, another workspace's entries survive |

`git diff` shows no removed assertion, no `#[ignore]`, no deleted test.

**Mutation check (both guards removed, then restored):**

```
test app::input::remote_image_paste_tests::empty_bracketed_paste_is_forwarded_when_the_binding_is_disabled ... FAILED
test app::input::remote_image_paste_tests::a_flood_of_empty_bracketed_pastes_starts_one_clipboard_read ... FAILED
---- app::input::remote_image_paste_tests::empty_bracketed_paste_is_forwarded_when_the_binding_is_disabled stdout ----
assertion `left == right` failed: a disabled binding must leave the empty paste to the pane
  left: Consume
---- app::input::remote_image_paste_tests::a_flood_of_empty_bracketed_pastes_starts_one_clipboard_read stdout ----
assertion `left == right` failed: eight pastes arriving while one read is outstanding must start one read, not eight
  left: 8
test result: FAILED. 0 passed; 2 failed; 0 ignored; 0 measured; 3392 filtered out; finished in 0.28s
```

The purge and client tests call their functions directly, so they are non-tautological by
construction; the purge *wiring* at the two call sites is not covered by a test, matching how
`purge_remote_resync_pane_index_for_workspaces` is tested today.

## Verbatim cargo output

`export ZIG=$HOME/.local/zig-0.15.2/zig` for all of these. No `just`, no `nextest`.

`cargo fmt -- --check`:

```
FMT_EXIT=0
```

`cargo test --bin herdr -- --test-threads=2`:

```
failures:

---- api::server::pane_graphics_stream::tests::inactive_owner_cancels_idle_stream_and_dispatches_close stdout ----

thread 'api::server::pane_graphics_stream::tests::inactive_owner_cancels_idle_stream_and_dispatches_close' (16599462) panicked at src/api/server/pane_graphics_stream.rs:991:14:
canceled idle stream should dispatch a close: Timeout
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace


failures:
    api::server::pane_graphics_stream::tests::inactive_owner_cancels_idle_stream_and_dispatches_close

test result: FAILED. 3393 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 48.56s
```

**3393 passed, 1 failed.** The single failure is
`api::server::pane_graphics_stream::tests::inactive_owner_cancels_idle_stream_and_dispatches_close`,
which the reviewer independently reproduced on an untouched `f934965b` worktree. It is
**PRE-EXISTING and not caused by this diff**; the diff touches nothing in `api::server`. Arithmetic
checks out: 3389 (round-3 branch, reviewer's number) + 4 new tests = 3393. Do not round this to
"0 failed" in a commit message.

`cargo clippy --bin herdr` (after `touch src/main.rs` to force a full re-lint):

```
warning: field `reader` is never read
   --> src/api/client.rs:156:5
warning: methods `next_value` and `next_event` are never used
   --> src/api/client.rs:160:12
warning: function `read_optional_json_line` is never used
   --> src/api/client.rs:225:4
warning: method `save_pane_history_persistence` is never used
   --> src/app/config_io.rs:103:19
warning: function `wait_for_agent_change` is never used
   --> src/cli.rs:720:15
warning: function `api_timeout_error` is never used
   --> src/cli.rs:786:4
warning: function `map_out` is never used
   --> src/remote/federation/id.rs:178:8
warning: associated constant `CLIPBOARD` is never used
  --> src/remote/federation/protocol/mod.rs:67:15
warning: this expression creates a reference which is immediately dereferenced by the compiler
   --> src/app/input/mod.rs:919:51
warning: manual implementation of `.is_multiple_of()`
   --> src/pane/osc.rs:309:28
warning: `herdr` (bin "herdr") generated 10 warnings (run `cargo clippy --fix --bin "herdr" -p herdr -- ` to apply 2 suggestions)
```

**Correction to the round-3 review's "0 errors, 0 warnings".** I could not reproduce that. All 10
warnings are **pre-existing**: I confirmed every offending construct exists verbatim in
`git show f934965b:<file>` for all seven files, including `src/app/input/mod.rs:919`
(`terminal_key_matches_combo(&key, binding)`, `needless_borrow`) which is untouched by this diff.
0 errors. **My changes introduced no new warning** — the warning count and set are identical before
and after this round. The reviewer's clean run was most likely a cached no-op re-lint. I did not
"fix" the eight dead-code warnings or the two lint nits in untouched files: out of scope, and each
would be a behavior-neutral edit to code this plan never opened.

## Unresolved questions

1. **M2 residue (for Can):** should the configured *key* also be suppressed for a `--remote` client
   whose local clipboard has no image? That closes the host-clipboard read completely, but costs
   ctrl+v passthrough on non-federated panes for remote clients. The clean fix is a remoteness flag
   on `ClientMessage::Hello` + a server-side gate on `route_client_events_from`'s `source_id` — one
   bool field, `PROTOCOL_VERSION` bump per CLAUDE.md, and a fallback for older clients. Not done:
   the task forbade adding a protocol field this round.
2. **M1 scope:** should `keys.remote_image_paste = ""` also disable the *non-empty* cmux temp-path
   bridge? It is a file read of a path the terminal handed over, not a clipboard read, and it is
   master behavior — so I left it. If "off means off for the whole bridge", it is a two-line change.
3. Still owed and unchanged since round 3: hand validation of Warp + a live federation mount + a
   real screenshot, and a `herdr --remote` run to confirm the M2 suppression behaves in practice.
   Every claim here is source-, test- and mutation-level.
4. The round-3 review's "clippy 0 warnings" claim is not reproducible (see above) — worth correcting
   in the review record so a future round does not treat a regression as baseline noise.

Status: DONE_WITH_CONCERNS
Summary: M1 (single off switch, docs corrected), M3 (per-pane in-flight guard, mutation-verified at
8→1 reads, retires a pre-existing master class), and Q5 (workspace-scoped purge of both PaneId sets,
wired at the same two close sites as the existing clipboard-stage purges) are fixed as specified. M2
is fixed for the empty-paste shape **client-side** — origin is provably not distinguishable at the
server without a protocol field, but the client already knows it is remote, so the leak is closed
where it originates instead. 3393 passed / 1 failed (pre-existing, reproduced on a clean baseline by
the reviewer); fmt clean; clippy unchanged at 10 pre-existing warnings, 0 introduced.
Concerns: the M2 fix does not cover the configured-key shape (needs the `Hello` field — Can's call);
the round-3 report's "clippy 0 warnings" and "3390/0 failed" claims are both non-reproducible here;
no live Warp / `herdr --remote` validation has been done by anyone yet.
