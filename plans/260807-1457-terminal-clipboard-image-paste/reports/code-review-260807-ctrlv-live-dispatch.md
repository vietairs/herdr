# Code review — ctrl+v live dispatch intercept

Scope: uncommitted diff on `fix/terminal-clipboard-image-paste` (base `f934965b`),
`src/app/input/terminal.rs` (+52), `src/app/input/mod.rs` (+169, tests only).
Verification: code reading of the full dispatch chain (`route_client_events_from` →
`InputLeaseTable` → `handle_terminal_key_headless_from` → `prepare_terminal_key_forward`),
plus `cargo test --bin herdr live_key_dispatch` (5 passed, 2.27s).

## Verdict

The core fix is correct and the fall-through path is safe: I could not find any case where a
non-matching key, a disabled binding, or a local pane loses its key. One real defect
(auto-repeat re-firing the clipboard read) is CONFIRMED at the code level and is the same
defect class as the prior duplicate-paste bug.

---

## H1 — CONFIRMED — a held ctrl+v fires one clipboard read + one staged paste per key repeat

`src/app/input/terminal.rs:90-127`

The implementation report claims only `KeyEventKind::Press` reaches
`prepare_terminal_key_forward`. That claim is **false**. Traced:

1. `src/app/mod.rs:1866-1882` — a Press that the intercept consumes returns `None`, so
   `complete_press` is called with `target = None`. The intercept does not change
   `terminal_input_context()` (mode stays `Terminal`, popup unchanged), so
   `initial_context == resulting_context` and the lease is stored as
   `ConsumedInputLease::ReprocessRepeats(Pane)` (`src/app/input/lease.rs:83-90`).
2. `src/app/mod.rs:1884-1892` — each subsequent `Repeat` event calls `plan_repeat`, which for a
   `ReprocessRepeats` lease in the same context returns
   `RepeatPlan::Reprocess { repetitions: key.repeat_count, tracked: true }`
   (`src/app/input/lease.rs:116-125`).
3. `execute_repeat_plan_headless` (`src/app/mod.rs:1788-1826`) then calls
   `handle_terminal_key_headless_from` again, once per repetition, with
   `kind = Repeat` — straight back into `prepare_terminal_key_forward`.
4. `remote_image_paste_decision` → `crate::config::terminal_key_matches_combo`
   (`src/config/keybinds.rs:1306-1308`) matches on code + modifiers + shifted codepoint only.
   **`kind` is never consulted**, so a `Repeat` ctrl+v matches exactly like a `Press`.
5. Even a single event can multiply: `complete_press` returns
   `Reprocess { repetitions: repeat_count - 1 }` when `repeat_count > 1`
   (`src/app/input/lease.rs:93-102`; existing test
   `new_semantic_press_recomputes_consumed_repeat_disposition` at `lease.rs:334-353` pins that
   behavior for a consumed key).

Repeat events really are delivered on Unix: `push_keyboard_enhancement_flags`
(`src/main.rs:22-28`) pushes `ime_compatible_keyboard_enhancement_flags()`, which includes
`REPORT_EVENT_TYPES` (`src/input/model.rs:219-223`), and `src/input/parse.rs:246` maps kitty
event type `2` to `KeyEventKind::Repeat`. Ghostty/Kitty/WezTerm hosts will produce them.

Failure scenario (inputs → wrong behavior): focus a pane of a live federated mount whose peer
advertises `FILE_STAGING`, have a PNG on the local clipboard, hold ctrl+v for ~1s. The host
emits Press + N Repeats. Each one calls `begin_remote_clipboard_image_capture`, spawning an
independent `osascript`/`wl-paste` read (`src/app/input/mod.rs:1070-1088`), each answering with
its own `RemoteClipboardImageCaptured` event, each staging a separate file on the remote and
pasting a separate path into the agent prompt. There is no in-flight or de-dup guard anywhere
in `begin_remote_clipboard_image_capture` / `pending_remote_clipboard_stages`.

Secondary form: on a mount **without** `FILE_STAGING`, the same loop raises the
`TOAST_TITLE_FAILED` toast once per repeat.

Suggested fix (either is small and testable):

```rust
// consume repeats without re-reading the clipboard: one press, one capture
RemoteImagePasteDecision::Capture { .. }
    if key_event.kind != crossterm::event::KeyEventKind::Press => return None,
```

or mark the lease `SuppressRepeats` from the intercept. Do **not** simply fall through on
`Repeat` — that would forward a bare `0x16` to the remote PTY after the image path, which is
worse. Add a regression test driving
`route_client_events(vec![Press(ctrl_v, repeat_count 3), Repeat(ctrl_v), Release(ctrl_v)])`
and asserting exactly one `RemoteClipboardImageCaptured` event.

This is the defect class that bit branch `feat/remote-workspace-paste-image-files`
(`3aed04a9`). I would fix it before landing.

---

## M1 — CONFIRMED — legacy `--no-session` popup path gains a new intercept

`src/app/input/terminal.rs:90` reached via `handle_terminal_key` at `terminal.rs:441`.

In the legacy loop, `handle_key` short-circuits to `handle_terminal_key` when a popup pane is
open (`src/app/input/mod.rs:82-84`), *bypassing* the old intercept. `handle_terminal_key` calls
`prepare_popup_key_forward` first, which returns `Consumed`/`Bytes` whenever
`state.popup_pane.is_some()` — so in practice the new intercept is still unreachable with a
popup open. **Verified: the "popups are handled before this point" claim holds** on both the
headless path (`terminal.rs:44-54`) and the legacy path (`terminal.rs:430-440`). No action;
recorded because the ordering is load-bearing and easy to break later.

Ordering otherwise faithfully reproduces the legacy chain: the legacy intercept sat above
`match self.state.mode`, i.e. above every terminal handler, so placing it above
`terminal_direct_non_indexed_navigation_action`, `command_for_key`,
`terminal_direct_indexed_navigation_action` and `is_prefix_key` is the same precedence.
Copy mode, prefix mode and modals never reach `prepare_terminal_key_forward` because
`terminal_input_context()` returns `None` outside `Mode::Terminal`/popup
(`src/app/mod.rs:1765-1773`), and `resolve_remote_paste_target` re-checks
`state.mode != Mode::Terminal` (`src/app/input/mod.rs:905-908`). Modal ctrl+v paste is
unaffected.

## M2 — CONFIRMED (behavioral, by design) — ctrl+v is permanently unavailable on an old peer

`src/app/input/terminal.rs:96-102`

`Unsupported` consumes the key and toasts. On a mount whose peer lacks `FILE_STAGING`, the user
can now never send `0x16` to that remote shell (readline quoted-insert, vim visual-block) — the
only escape is `keys.remote_image_paste = ''`, which also disables it for good mounts. This
matches the legacy intent, but the legacy code was dead, so this is a *new* user-visible
restriction. Worth a line in `docs/next` and worth confirming with Can that consume-and-toast
(rather than toast-and-forward) is still wanted now that it is live.

## M3 — Minor divergence from the legacy ordering

`src/app/input/terminal.rs:73-90` — the intercept now runs *after*
`self.state.clear_selection()`, `selection_autoscroll_deadline = None` and
`update_dismissed = true`; the legacy intercept ran before them. An intercepted ctrl+v
therefore also clears a retained selection and dismisses the update banner. Benign, arguably
desirable, but it is an unremarked behavior delta.

## L1 — Sample-config comment is now misleading

`src/main.rs:193`: `# remote_image_paste = "ctrl+v" # only active in herdr --remote; ...`.
The gate is a live federated mount with `FILE_STAGING`, not a `--remote` invocation, and until
this fix the binding was inert in every real session. Reword when `docs/next` is touched.

---

## Cleared checks

- **Fall-through / key swallowing (priority 1): clean.** `FallThrough` has no arm and no
  `return`; control flow continues to the identical chain. Byte-level pinned by the new tests:
  local pane asserts `vec![0x16]` off the pane's runtime channel
  (`mod.rs:2384-2402`), disabled binding asserts `vec![0x16]` off the federation wire
  (`mod.rs:2404-2427`), non-matching ctrl+x asserts `vec![0x18]` (`mod.rs:2429-2449`). These are
  real byte assertions, not decision-level tautologies.
- **Consistency with the `Paste` arm (priority 4): matches.** Same tri-state decision, same
  `raise_clipboard_stage_toast(TOAST_TITLE_FAILED, TOAST_REMOTE_TOO_OLD)`, same
  `begin_remote_clipboard_image_capture` off-loop helper, same consume-either-way contract as
  `src/app/mod.rs:1912-1962`.
- **Threading / mis-delivery (priority 5): safe.** Nothing blocks the key path;
  `begin_remote_clipboard_image_capture` snapshots `workspace_id` (stable String id, not an
  index) plus `target_pane_id` before spawning, and
  `handle_remote_clipboard_image_captured` re-resolves the workspace by id
  (`mod.rs:1121-1133`), so a focus change mid-read cannot deliver the image to a different pane;
  a closed workspace drops it with a `warn!`. 5s read timeout at `mod.rs:821`.
- **Security (priority 6): local-only.** The sole production entry is
  `route_client_events_from` fed by `HeadlessServer` from attached client-socket input
  (`src/server/headless.rs:2927`). No federation inbound path constructs `RawInputEvent`s, so a
  hostile peer cannot induce a local clipboard read. The capture gate requires
  `Mode::Terminal` + active workspace + a workspace whose `worktree_space().key` matches a live
  `remote_mirrors` entry + `Capability::FILE_STAGING` (`mod.rs:902-982`).
- **Conventions (priority 7): compliant.** No `unwrap()`, no `#[allow]`, `tracing::debug!` used,
  `#[cfg(unix)]` block with the platform call kept behind `crate::platform::read_clipboard_image`
  — identical gating to the sibling `Paste` arm and to `remote_image_paste_decision` itself, so
  the Windows build is unaffected (the decision fn does not exist there).
- **Backwards compatibility:** no protocol, API, schema or exported-signature change.

## Test quality (priority 8)

Four of the five tests are meaningful and byte-level. The `Unsupported` test additionally
asserts *no* frame reaches the peer, which is the right assertion.

On the self-raised concern: letting the real `crate::platform::read_clipboard_image` run is
**acceptable, not ideal**. It is read-only, bounded by the 5s timeout, and the assertion is on
the event's addressing (`workspace_id`/`target_pane_id`), which holds for `NoImage` too — so it
is not clipboard-content dependent. Cost: the run took 2.27s wall for 5 tests, essentially all
of it the `osascript` spawn, and it reads the developer's real clipboard. I would not block on
it, but note that `begin_remote_clipboard_image_capture` already takes the reader as a closure;
the only reason a seam is not usable is that the production call site hardcodes the platform fn.
Threading an injectable reader (e.g. a `#[cfg(test)]` override field on `App`) would make the
test deterministic and sub-millisecond — cheap enough to be worth doing if H1 is being fixed
anyway, since the H1 regression test needs to count captures and will be far more robust with a
counting stub than with N real `osascript` spawns.

Missing coverage: the repeat case (H1), and nothing pins that an intercepted ctrl+v does not
also reach a *popup* pane.

## Merge blocking

**H1 blocks merge** in my judgment: it ships a user-visible duplicate-paste/duplicate-subprocess
regression on the exact interaction this change makes live, and it is the second occurrence of
this defect class in this feature. Everything else is non-blocking.

## Unresolved questions

1. Is consume-and-toast (M2) still the wanted behavior for peers without `FILE_STAGING`, now
   that it actually reaches users and permanently removes ctrl+v from those panes?
2. Should the repeat fix suppress silently, or toast once ("image already being staged")?
3. Does this change need a `docs/next` entry, given the binding was documented but inert?
