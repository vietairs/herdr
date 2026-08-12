# Code review — round 3 final, whole uncommitted diff

Scope: complete uncommitted diff on `fix/terminal-clipboard-image-paste` (base `f934965b`),
reviewed as one change. 6 files, +812/-124.

- `src/app/input/mod.rs` (+840/-…: shared `dispatch_remote_image_paste_key`,
  `dispatch_bracketed_paste_image`, `dispatch_empty_bracketed_paste`, `clipboard_image_reader`
  seam, 15 new tests, 1 extended test)
- `src/app/input/terminal.rs` (+21, call to the shared key intercept)
- `src/app/mod.rs` (+65/-33: new `remote_image_paste_unsupported_notices` field; headless
  `Paste` arm collapsed onto the shared helper)
- `src/main.rs`, `docs/next/CHANGELOG.md`, `docs/next/website/src/data/config-reference.json`

Verification performed by me, not taken from the fix report:

| Check | Result |
|---|---|
| `cargo test --bin herdr -- --test-threads=2` (branch, run 1) | 3389 passed, **1 failed** (`api::server::pane_graphics_stream::tests::inactive_owner_cancels_idle_stream_and_dispatches_close`) |
| same, run 2 | 3389 passed, **1 failed**, same test |
| that test in isolation | passes (0.13s) |
| **baseline `f934965b` in a clean worktree, `--test-threads=2`** | 3374 passed, **1 failed**, *same test* — **CONFIRMED pre-existing** |
| `cargo fmt -- --check` | exit 0 |
| `cargo clippy --bin herdr` / `--all-features` | **0 errors, 0 warnings** |

Test-count arithmetic checks out: 3375 base + 5 (r1) + 3 (r2) + 7 (r3) = 3390.

**Correction to the fix report:** it claims "3390 passed, 0 failed" on rerun. I could not
reproduce a fully green full-suite run at `--test-threads=2` in two attempts; I get
3389/1 both times, and so does the untouched baseline. The failure is provably not caused by
this diff (baseline reproduces it, and the diff touches nothing in `api::server`), but the
"0 failed" claim in the report is not reproducible on this machine and should not be repeated
in a commit message.

---

## Verdict

**No blocking defect.** H1 is fixed and complete on both dispatchers and both repeat shapes.
The 80-line de-duplication is behaviour-preserving; I diffed both original callers against the
merged helper line by line and found exactly one delta, which is the intended round-3 feature.
Three findings below are worth acting on before landing, none of them blockers.

---

## M1 — CONFIRMED — the round-3 trigger has no off switch

`src/app/input/mod.rs:1180-1215` (`dispatch_empty_bracketed_paste`)

`dispatch_empty_bracketed_paste` never consults `state.remote_image_paste_key`. Setting
`keys.remote_image_paste = ""` disables the raw-key intercept only; an empty bracketed paste on
a `FILE_STAGING` mount still reads the local clipboard and still consumes the paste. There is
now **no configuration that turns the clipboard-image capture off** on a federated pane.

The corrected `config-reference.json` text ("Set it to an empty string to disable the raw-key
shortcut") is technically precise — "raw-key" is doing load-bearing work — but a user reading
it will reasonably conclude the feature is off.

Failure scenario: a user who deliberately disabled `remote_image_paste` (round 1's M2 escape
hatch, or privacy preference about clipboard reads) opens a federated pane in Warp, presses
Cmd+V intending a text paste, and gets a clipboard read plus a "no image on the clipboard"
toast, with no way to stop it.

Fix options, cheapest first: gate `dispatch_empty_bracketed_paste` on
`state.remote_image_paste_key.is_some()` (reuses the existing knob and matches what the docs
already say), or say explicitly in the config reference that the empty-paste bridge is always
on. The first is two lines and one test.

## M2 — CONFIRMED — a `herdr --remote` client can cause the *server host's* clipboard to be read

`src/client/mod.rs:1463-1494` → `src/app/mod.rs:1928-1943` → `src/app/input/mod.rs:1180`

`should_bridge_clipboard_image_paste` (`src/client/mod.rs:1855-1874`) claims
`ESC[200~ESC[201~` and the configured key for a remote client, but only *consumes* them when
`crate::platform::read_clipboard_image()` on the client machine returns `Some`. When the client
machine's clipboard has no image, the code falls through and the raw bytes are forwarded to the
server as `ClientInputEvent::Paste { text: "" }` (or as the key). On the server, rounds 1–3 now
make both of those read the **server host's** clipboard and stage that image into the focused
federated pane.

Failure scenario: user on laptop B runs `herdr --remote` against host A, which has a federation
mount to host C. B's clipboard holds text, not an image. B presses Cmd+V. A screenshot that has
been sitting on **A's** clipboard is read, staged to C, and its path pasted into the agent
prompt. The user on B never saw that image.

Not an authorization hole — the client socket is same-user and local, no remote-originated data
reaches the trigger (see priority-4 clearance below) — but it is a cross-machine data surprise,
and it was introduced by making these intercepts live (round 1 for the key, round 3 for the
empty paste), not by anything upstream. Worth a decision from Can rather than a silent land.
Fix if wanted: suppress the intercepts for a non-`LOCAL_INPUT_SOURCE` source id, which
`route_client_events_from` already carries.

## M3 — CONFIRMED (pre-existing class, newly extended) — held Cmd+V is not repeat-gated

`src/app/input/mod.rs:1131-1132`

The press gate that fixes H1 protects the *key* path. A bracketed paste carries no
`KeyEventKind`, and there is no lease/repeat machinery for pastes, so if the host terminal
auto-repeats a held Cmd+V it emits N independent `Paste("")` events and the intercept starts N
clipboard reads and N sequential stages. `MAX_IN_FLIGHT_STAGES_PER_MOUNT = 2`
(`src/app/remote_clipboard_stage.rs:81,259-270`) bounds *concurrency*, not the total, so the
user-visible outcome is the same duplicate-paste symptom H1 described: several staged files and
several pasted paths.

Severity is genuinely lower than H1 because this is not new — master already had it for the
non-empty cmux/iTerm2 temp-path shape, so round 3 extends an existing exposure to one more
terminal rather than creating a class. Whether Warp actually auto-repeats a held Cmd+V is
unmeasured, hence the shape is CONFIRMED in code and PLAUSIBLE in practice. I would not block,
but note that the round-2 rationale for rejecting an in-flight guard ("bounded by
`MAX_IN_FLIGHT_STAGES_PER_MOUNT`") is weaker than stated: that constant bounds memory, not the
number of pastes that land in the prompt.

## L1 — CONFIRMED — nested `herdr --remote` inside a federated pane loses its own Cmd+V signal

A federated pane running `herdr --remote` to a third host uses the identical
`ESC[200~ESC[201~` byte string as its clipboard-image trigger
(`src/client/mod.rs:1860-1862`). The outer intercept now claims that byte string before it can
reach the inner client. The outer behaviour is still sensible (it stages to the mount peer), but
the nested chain silently retargets. Extreme edge case; recording it because the byte string is
now overloaded at two layers of the same product.

## L2 — CONFIRMED — `remote_image_paste_unsupported_notices` is never pruned

`src/app/mod.rs:176-192`. Bounded by "panes on which the user pressed the binding against an
unsupported peer", one `PaneId` each, never reused. Not a leak worth fixing; noted only because
nothing removes an entry on pane close, and the round-2 report's own open question 2 (reset on
remount) has no hook today.

## L3 — Changelog omits one user-visible consequence

`docs/next/CHANGELOG.md`. The `Added` entry is accurate and correctly does **not** claim Cmd+V
works in Apple Terminal — it states the opposite, matching the measurement. But an empty *text*
clipboard on a federated pane now produces a "no image on the clipboard" toast where before the
paste was silent. That is a new user-visible noise source and belongs in the entry (one clause).

---

## Priority-by-priority clearance

### 1. H1 — fixed, and completely. CONFIRMED.

- Gate is inside the shared `dispatch_remote_image_paste_key`
  (`src/app/input/mod.rs:1092-1094`), so it applies to **both** dispatchers by construction:
  `src/app/input/terminal.rs:91-94` (headless) and `src/app/input/mod.rs:99-102` (legacy
  `handle_key`, live via `src/remote/federation/session.rs:376` and
  `src/app/runtime.rs:169,198`). There is no second copy left to drift.
- Separately delivered repeats: `route_client_events_from`'s `Repeat` arm →
  `plan_repeat` → `execute_repeat_plan_headless`, which rebuilds the key with
  `.with_kind(Repeat).with_repeat_count(1)` (`src/app/mod.rs:1809-1811`). The gate sees
  `Repeat`. Verified in source.
- Pre-counted `repeat_count > 1` in one event (`src/app/input/lease.rs:93-105`):
  `complete_press` returns `Reprocess { repetitions: repeat_count - 1 }`, expanded through the
  *same* `execute_repeat_plan_headless` rebuild. Also gated. Verified in source.
- **Mutation claim spot-verified by reasoning, not just accepted.** With the gate removed, the
  held-key test must produce 1 press + 5 reprocessed repeats = 6 reads, and the pre-counted test
  1 + (3-1) = 3. The report's recorded failures are exactly `left: 6` and `left: 3`. Those
  numbers are only derivable if the lease really takes the `ReprocessRepeats` disposition, which
  in turn is only true if the intercept leaves `terminal_input_context()` unchanged — so the
  mutation output independently corroborates round 1's trace. The tests are real regression
  tests.
- Legacy path: repeats arrive from the parser as discrete `Repeat` events; `handle_key` does no
  `repeat_count` expansion, so one press with `repeat_count = 3` yields one read there too.
- Round-3 empty-paste path reasoned about independently: it is not a key event, has no
  `KeyEventKind`, and never enters the lease table — so the key gate does **not** cover it. See
  M3. That is the one place the repeat question is still open, and it is a pre-existing shape.

### 2. The de-duplication — behaviour-preserving. CONFIRMED, one intended delta.

I diffed `git show master:src/app/input/mod.rs` (`handle_paste`, lines 242-276) and
`git show master:src/app/mod.rs` (headless `Paste` arm) against
`dispatch_bracketed_paste_image` (`src/app/input/mod.rs:1127-1163`), arm by arm:

| Aspect | master `handle_paste` | master headless arm | merged helper |
|---|---|---|---|
| `Unsupported` | toast(FAILED, TOO_OLD), `return` (consume) | toast(FAILED, TOO_OLD), `intercepted = true` | toast(FAILED, TOO_OLD), `Consume` |
| `Capture` | `begin_remote_clipboard_image_capture(ws_idx, pane, read_verified_image_drop_file)`, consume | identical | identical |
| `FallThrough` | falls to `rt.send_paste(text)` | falls to `try_send_paste(text)` | `Forward`, caller unchanged |
| Ordering vs popup | popup checked first (`handle_paste:199-206`) | `try_route_paste_to_popup` first (`app/mod.rs:1929`) | unchanged, both callers still check first |
| Ordering vs non-terminal mode | `mode != Terminal` → text input, before intercept | same | unchanged |
| Toast constants | same two | same two | same two |
| Off-loop closure | `move || read_verified_image_drop_file(&path, extension)` | identical | identical |

No delta in ordering, error handling, toast text, or consume-vs-forward. The single delta is the
new `text.is_empty()` branch taken *before* `bracketed_paste_image_decision` — previously empty
text reached the decision, failed `local_image_path_from_text`, and fell through. That is the
round-3 feature, and `bracketed_paste_of_the_measured_cmux_temp_path_still_stages`
(`src/app/input/mod.rs:2973`) pins the non-empty shape end-to-end through the real parser.

Note also that `resolve_remote_paste_target` / `RemotePasteTarget` were **already** shared on
master (`/tmp` copy, lines 895-902) — the dedup is genuinely only the two ~40-line caller
matches, so the "80 lines" figure is right but the risk surface is smaller than it sounds.

### 3. Round-3 regression surface — every gate checked. CONFIRMED safe except M1/M3.

- **Popup pane:** both callers short-circuit before the intercept —
  `src/app/input/mod.rs:199-206` and `src/app/mod.rs:1929` (`try_route_paste_to_popup` returns
  `true` whenever `popup_pane.is_some()`, `src/app/popup.rs:40-50`). Unreachable.
- **Copy mode, modals, rename, navigator, keybind help, settings, onboarding:** all are
  `Mode != Terminal`, handled by the `paste_into_active_text_input` branch above the intercept,
  and `resolve_remote_paste_target` re-checks `state.mode != Mode::Terminal`
  (`src/app/input/mod.rs:855-858`) as a second gate. Double-gated.
- **Non-focused pane:** the target is always `workspace.focused_pane_id()` of `state.active`,
  identical to the ctrl+v path. No way to address a background pane.
- **Local pane / non-federated workspace:** `resolve_remote_paste_target` returns `None` →
  `Forward`, no clipboard read. Pinned byte-level by
  `empty_bracketed_paste_on_an_ordinary_local_pane_is_forwarded_unchanged`.
- **The implementer's self-flagged concern** (a federated pane app that genuinely wanted an
  empty bracketed paste): real-world impact is negligible for ordinary TUIs — an empty paste
  carries no payload and the bracket framing conveys nothing an app can act on. The one concrete
  loser is the nested-`herdr --remote` case in L1.

### 4. Security — clean. CONFIRMED.

Traced every construction of `RawInputEvent::Paste` in production code:

- `src/raw_input.rs:780` — the local stdin parser.
- `src/protocol/wire.rs:332` — `ClientInputEvent::Paste` off the **client** socket
  (`src/server/headless.rs:2927`), i.e. an attached herdr client, same-user local unix socket.
- `src/server/headless.rs:1874` — `paste_client_clipboard_image_path`, a server-staged local
  path; always non-empty, so it cannot reach the new empty-paste branch.

No federation inbound path constructs input events; `route_client_events` / `route_client_events_from`
have exactly two call sites, both in `src/server/headless.rs`, neither fed by the federation
socket. Remote pane bytes become terminal *output*, parsed to cells, and never re-enter input
dispatch. **A hostile peer cannot induce a local clipboard read.** M2 above is a same-user
cross-machine surprise, not a trust-boundary violation.

Staging guards unchanged and untouched: extension whitelist and shape gate
(`crate::image_path::local_image_path_from_text`), advisory on-loop temp-dir containment
(`recognized_image_drop_location`), authoritative TOCTOU-safe re-proof against the opened fd
inside `read_verified_image_drop_file` during the off-loop read, `MAX_CLIPBOARD_IMAGE_PAYLOAD`
size cap (`oversized_clipboard_image_is_rejected_before_any_wire_send` still green), and
`MAX_IN_FLIGHT_STAGES_PER_MOUNT`. The empty-paste path bypasses none of them — it goes through
`begin_remote_clipboard_image_capture` and the same `handle_remote_clipboard_image_captured`
staging path as ctrl+v.

### 5. Test integrity — no weakening. CONFIRMED.

`image_paste_decision_is_unsupported_when_the_mount_lacks_the_staging_capability`
(`src/app/input/mod.rs:1935-1994`) is the only pre-existing test whose body changed. Old body:
decision `== Unsupported` + `assert_no_frame`. New body asserts strictly more —
`Unsupported { target_pane_id }` (the resolved pane, previously unasserted), the toast title,
`0x16` actually observed on the wire (a positive byte assertion replacing a negative one),
`pending_remote_clipboard_stages.is_empty()` (the fact that proves nothing was staged, since a
stage cannot reach the wire without an entry there), and a second press raising no second toast.
The positive control at the bottom is untouched. Strictly more, and one assertion changed
direction only because the behaviour deliberately changed (H2).

`git diff` shows no other removed assertion anywhere in the test module. No test deleted, no
`#[ignore]` added, no assertion loosened. The `clipboard_image_reader()` seam is a *test-build
substitution of the reader*, not of the code under test — the whole dispatch chain still runs,
and `the_measured_warp_cmd_v_bytes_parse_as_an_empty_paste` guards against the fixture drifting
away from the real parser, which is exactly the right anti-tautology guard.

One nit: `empty_bracketed_paste_reports_a_mount_that_cannot_stage_but_still_delivers_it`
(`:2934`) calls `dispatch_bracketed_paste_image("")` directly and asserts `Forward`, so it does
not prove the paste reaches the pane the way the sibling key test does with `recv_input_bytes`.
The caller wiring is proven elsewhere, so this is a coverage nit, not a gap.

### 6. Conventions — compliant. CONFIRMED.

No `unwrap()` added to production code; the only `panic!`/`expect` in the diff are inside the
`#[cfg(all(test, unix))]` module. No new `#[allow]`. Logging is `tracing::debug!` throughout,
consistent with neighbours. Every new item — `RemoteImagePasteKeyDisposition`,
`clipboard_image_reader`, `dispatch_remote_image_paste_key`, `dispatch_bracketed_paste_image`,
`dispatch_empty_bracketed_paste`, the `App` field and its initializer, and both call sites — is
`#[cfg(unix)]`-gated, and the headless arm keeps its `#[cfg(not(unix))] let intercepted = false;`
counterpart, so the Windows build sees the same shape as before. Comments state invariants (why
the key is claimed, why only a press starts a read, why the notice is per-pane, why the read is
off-loop) with no plan/phase/round references. `clippy` is clean. No protocol, API, schema, or
persisted-state change; the new field is TUI/client presentation state, correctly kept out of
server state per the runtime/client guardrail.

### 7. Docs — accurate, one omission (L3).

The changelog `Added` entry explicitly states Apple Terminal sends nothing for an image-only
clipboard and that `keys.remote_image_paste` remains the way to paste there. It does **not**
overclaim. `src/main.rs:193` and the `docs/next` config reference are both corrected and now
match the real gate. The stable docs tree was correctly left alone per CLAUDE.md. Missing: the
new "no image on the clipboard" toast on an empty text clipboard (L3), and — depending on M1's
resolution — a statement about whether the empty-paste bridge is disableable.

---

## Merge blocking

**Nothing blocks merge.** H1 is fixed and complete, the de-duplication is provably
behaviour-preserving, the new trigger is not remotely reachable, and no test was weakened.

Recommended before landing, in order:

1. **M1** — two-line gate on `remote_image_paste_key.is_some()` (or one doc sentence). Cheapest
   fix, closes the "documented as disableable but is not" gap.
2. **M2** — a decision from Can, not necessarily code: should a `herdr --remote` client be able
   to read the *host's* clipboard? If not, gate on `source_id == LOCAL_INPUT_SOURCE`.
3. **L3** — one clause in the changelog.
4. Correct the "3390 passed, 0 failed" claim if it reaches a commit message; the reproducible
   number on this machine is 3389/1 with a pre-existing flake, on branch and baseline alike.

M3, L1, L2 are records, not asks.

## Unresolved questions

1. M2: is a remote client reading the herdr host's clipboard intended, or an accident of making
   the intercepts live?
2. M1: should `keys.remote_image_paste = ""` also disable the empty-bracketed-paste bridge?
3. Still owed and unchanged from round 3: hand validation of Warp + a live federation mount +
   a real screenshot. Every claim in this review is source- and test-level.
