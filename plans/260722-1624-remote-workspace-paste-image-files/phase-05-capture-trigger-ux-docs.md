# Phase 05 — capture trigger, toast copy, docs

## Context (verified against source)

### Why the existing trigger does not fire (predict correction C5 — confirmed)

- `src/client/mod.rs:666-669` — `is_remote_client_process()` is a **whole-process** flag:
  `env::var(REMOTE_KEYBINDINGS_ENV_VAR)` (`HERDR_REMOTE_KEYBINDINGS`) — the Unix definition is
  `src/remote/unix.rs:29`, re-exported through `src/remote.rs:7`; `src/remote.rs:12` is the
  `#[cfg(windows)]` copy. (There is no `src/remote/mod.rs`.) Set only for the thin `herdr --remote`
  SSH bridge client.
- `src/client/mod.rs:1799-1812` — `should_bridge_clipboard_image_paste(data, is_remote_client,
  remote_image_paste_key)`; `:1686-1699` — `client_remote_image_paste_key` returns `None` outright
  when the process is not the bridge client.
- `src/client/mod.rs:1424-1455` — the capture site: read clipboard, size-check against
  `MAX_CLIPBOARD_IMAGE_PAYLOAD` at `:1430-1437`, send `ClientMessage::ClipboardImage`.
- Server side: `src/server/headless.rs:2969-2993` handles `ServerEvent::ClientClipboardImage` →
  `write_client_clipboard_image` (`:1721-1732`) → `clipboard_image::stage`. Note `:2988-2991` logs a
  `warn!` and sends **no** notify on failure, unlike its `ClientPasteRejected` sibling at
  `:2952-2966` — the "nearest precedent is silent" warning (P10).

**Conclusion:** in the mounted-workspace topology the App owns the local machine's clipboard
directly, so the capture belongs in the App's own key handling, not in the thin client. The gate
becomes per-workspace/per-pane.

### Where the new gate goes

- `src/app/input/mod.rs:74-117` — `App::handle_key`. There is already an in-`handle_key` clipboard
  read precedent at `:79-86` (`modal_paste_target_active` → `read_clipboard_text`). The new
  intercept sits alongside it, before the `match self.state.mode` at `:87`.
- `src/config.rs:90-98` — `Config::remote_image_paste_key()` parses `keys.remote_image_paste`;
  `src/config/model.rs:948` — the default `"ctrl+v"`; `src/config.rs:299-311` — existing default and
  disable tests.
- `crate::platform::read_clipboard_image()` — used at `client/mod.rs:1429`; returns
  `ClipboardImage { bytes: Vec<u8>, extension: &'static str }` (`src/platform/mod.rs:88-94`).
  **There is no filename in the capture path** — `extension` is the only naming signal, drawn from a
  fixed allowlist and magic-byte validated (`linux.rs:410-416`, `macos.rs:626-629`; macOS currently
  only ever yields `"png"`, `macos.rs:628`).
- `src/protocol/wire.rs:28` — `MAX_CLIPBOARD_IMAGE_PAYLOAD = 16 MiB`.

### Toast rendering constraint

- `src/app/state.rs:1319` `ToastKind`, `:1332` `ToastNotification`.
- `src/ui/status.rs:108-127` — title and context are each a single `Line`; there is **no `.wrap()`**,
  so ratatui hard-clips with no ellipsis. Every string must stay under ~60 display cells.

### Docs

- `docs/next/website/src/data/config-reference.json:358-362` — the `keys.remote_image_paste` entry
  (`"Local-client shortcut that sends a clipboard image to a remote Herdr session."`).
- `docs/next/website/src/content/docs/config-reference.mdx` renders that JSON via
  `ConfigReference.astro`; prose belongs in `configuration.mdx` / `persistence-remote.mdx`.
- `scripts/config_reference_check.py` compares **key names and enum values** against the Rust
  config model; no new key is added here, so it stays green. `scripts/docs_translation_parity.py`
  covers the `ja` / `zh-cn` content trees — check whether the prose file edited has translated
  siblings before committing.
- Stable docs (`website/src/content/docs/`) are **not** edited during feature work.

## Requirements

1. A new per-workspace/per-pane capture gate with **exactly three branches**, decided by a pure
   function `remote_image_paste_decision(&AppState, key) -> RemoteImagePasteDecision`:
   - **`FallThrough`** — the focused pane is local (or the key does not match the binding, or the
     mode is not `Mode::Terminal`). The key is **not** consumed and the intercept **does not return
     from `handle_key`**: control continues into the existing `match self.state.mode`
     (`input/mod.rs:87`) so a local `ctrl+v` keeps reaching the pane app and non-terminal modes keep
     their handlers. This is the behavior the v0.7.5 changelog explicitly restored for Vim/Neovim; do
     not regress it.
   - **`Unsupported`** — the focused pane belongs to a mounted remote workspace whose live mount does
     **not** have `file_staging` in its agreed set. Raise the "remote herdr is too old" toast and
     **consume** the key. Consuming is the point: falling through here would silently send a raw
     `Ctrl-V` to the remote PTY *and* could never produce `CapabilityNotAgreed`, so the user would
     see nothing at all.
   - **`Capture`** — mounted remote pane with the capability. Read the clipboard and stage.
   The `CapabilityNotAgreed` error from phase 03's gated send helper therefore stays a defence-in-
   depth path (a mount that lost the capability between decision and send), not the normal old-peer
   route; it maps to the same toast copy.
2. Explicit non-goal, documented in the module comment: the thin `herdr --remote` bridge topology
   (App on a remote host) is unchanged and keeps its existing `ClientMessage::ClipboardImage` path.
   **Known limitation, accepted for v1:** in that topology the App runs on the far host, so if that
   App also has a mounted remote workspace, the new intercept would call
   `crate::platform::read_clipboard_image()` on the App host — the wrong machine's clipboard — and
   consume the key. There is no App-side predicate for "I am serving a bridge client" today
   (`is_remote_client_process` is a *client*-process env check, `client/mod.rs:667-668`), so no guard
   is invented here. Record it in the module comment and in plan.md Deferred; it is a
   mount-inside-bridge nesting, not the shipped topology.
2b. **`original_filename` is synthesised here.** The wire field has no OS source (see Context);
   build it as `format!("image.{}", image.extension)` from the validated `ClipboardImage.extension`
   and pass it to `App::begin_remote_clipboard_stage`. Do not invent a filename from pane/agent
   state. The phase-02 contract still validates it as untrusted input — that contract defends
   against a hostile mounting client, not against this call site.
3. **Client-side size precheck before send** (predict section E): reject
   `bytes.len() > MAX_CLIPBOARD_IMAGE_PAYLOAD` locally with the size toast. Without it, an oversize
   payload surfaces as a closed connection (`server/client_transport.rs:736-745`) and collides with
   the "disconnected" copy.
4. Failures always surface (P10): every rejection path raises a toast; none is a bare `warn!`.
5. Slow-path affordance: nothing under ~1.5s; past it a toast
   `saving image to remote host…` (no spinner — not YAGNI-justified).
6. Exact error copy (title / context), each under ~60 display cells:
   - `image paste failed` / `remote workspace disconnected before the image was saved`
   - `image paste failed` / `remote host is out of disk space`
   - `image paste failed` / `image is over 16MB, herdr's remote paste limit`
   - `image paste failed` / `clipboard has no image herdr can paste (png/jpg/gif/webp/bmp only)`
   - `image paste failed` / `remote herdr is too old to support image paste; update it`
   - `image paste failed` / `remote host did not respond; check the mount and retry`
   - `image paste failed` / `remote host is busy saving another image; retry`
   - `image paste failed` / `remote host rejected the file name; retry`
   - `image paste failed` / `remote host could not read the image data; retry`
   - `image paste failed` / `remote host has no usable temp folder for images`
   The strings and the mapping function are **defined in phase 04's module** (phase 04 requirement
   10 / step 2); this phase specifies them and tests them, it does not declare them.
   Map: `QuotaExceeded`/`WriteFailed`→disk space; **`InvalidPayload`**→could-not-read copy;
   **`StagingUnavailable`**→no-temp-folder copy; `PayloadTooLarge`→16MB; `UnsupportedExtension`/
   no-clipboard-image→clipboard copy; **`Busy`→busy copy** (phase 03's bounded staging queue is
   backpressure, not a disk failure); **`InvalidFilename`→rejected-file-name copy** (distinct: with
   the filename synthesised locally per 2b, this variant means the peers disagree on the contract,
   which is not "your clipboard is empty"); `CapabilityNotAgreed`→too old; timeout→did-not-respond;
   mount gone/purged→disconnected.
7. Docs (A3): document `keys.remote_image_paste` including the readline quoted-insert / vim
   visual-block collision and how to rebind or disable (`remote_image_paste = ''`).
   **`docs/next/…` only.**

## Files

| Action | File | Owner |
|---|---|---|
| modify | `src/app/input/mod.rs` | phase 05 (exclusive) |
| modify | `docs/next/website/src/data/config-reference.json` | phase 05 (exclusive) |
| modify | `docs/next/website/src/content/docs/configuration.mdx` | phase 05 (exclusive) |
| modify | `docs/next/CHANGELOG.md` | phase 05 (exclusive) |

`src/app/remote_clipboard_stage.rs` is **phase 04's, exclusively, in both directions**: the toast
copy constants, the failure→copy mapping, and the ~1.5s slow-transfer affordance are all declared
and unit-tested there (phase 04 requirement 10, step 2, tests 10b/10c). Phase 05 only *references*
them — it does not add code to that file. `src/client/mod.rs` is **not** modified (requirement 2).
No config key is added, so `src/config/model.rs` is untouched and `config_reference_check` stays
green.

## Implementation steps

1. In `src/app/input/mod.rs::handle_key`, after the `popup_pane` early return (`:75-78`) and
   alongside the existing modal-paste clipboard read (`:79-86`), add the intercept. **The whole
   block must be `#[cfg(unix)]`.** `input/mod.rs` is compiled on Windows and `handle_key` has no
   existing cfg split, but phase 04's `begin_remote_clipboard_stage` is `#[cfg(unix)]`; an ungated
   call is an unresolved symbol under `just windows-lint`. Contents:
   - call the pure `remote_image_paste_decision(&self.state, key)` (requirement 1) and match its
     three arms;
   - **only `Unsupported` and `Capture` consume the key and `return` from `handle_key`.
     `FallThrough` is a no-op inside the intercept — no `return`, no side effect — and execution
     falls through to the existing `match self.state.mode` at `:87`.** The intercept is placed
     *before* that match, so an unconditional `return` on the default arm would swallow every local
     key and every non-terminal mode. Shape it as
     `if let Unsupported/Capture = decision { …; return; }`, mirroring the existing early-return
     blocks at `:75-78` and `:80-85`, rather than a `match` whose `FallThrough` arm returns.
   - `Unsupported { .. }` → raise the "too old" toast, consume the key, return;
   - `Capture { ws_idx, target_pane_id }` → `crate::platform::read_clipboard_image()`, then hand the
     result to the seam below. On `None`, toast the clipboard copy and consume the key.
1b. **Test seam (required — without it test 1's success branch cannot run in CI).** `handle_key`
   performs the OS clipboard read and nothing else; everything after capture lives in
   `App::handle_remote_image_paste(&mut self, ws_idx, target_pane_id, image: ClipboardImage) ->
   ImagePasteOutcome`, also in `input/mod.rs` (phase 05 owned). It does the size precheck, synthesises
   `image.{extension}` (requirement 2b), calls phase 04's `App::begin_remote_clipboard_stage(...)`,
   maps `Err` to the matching toast, and always reports the key as consumed. Tests construct a
   `ClipboardImage` literal and call it directly; no OS clipboard, no injected global, no trait.
2. (The "still saving" affordance is phase 04's — requirement 10 of that phase. Nothing to do here.)
3. Update the `keys.remote_image_paste` description in `docs/next/.../config-reference.json` to
   cover both topologies without naming UI internals.
4. Add a short prose block to `docs/next/.../configuration.mdx` under the keybindings section: what
   the key does on a mounted remote workspace, that it defaults to `ctrl+v`, that `ctrl+v` is
   readline quoted-insert and vim visual-block so pane apps that need it should rebind, and that
   `remote_image_paste = ''` disables it. Check for `ja`/`zh-cn` siblings before committing
   (`scripts/docs_translation_parity.py`).
5. Add a `docs/next/CHANGELOG.md` entry under Added.

## Tests — TESTS FIRST

Write 1-3 before step 1.

1. `image_paste_decision_is_capture_for_a_focused_mounted_remote_pane` — pure decision function on
   an `AppState::test_new()` app with a mounted mirror advertising `file_staging`; asserts `Capture`
   with the focused pane's id. No clipboard involved.
1b. `image_paste_stages_and_consumes_the_key_for_a_supplied_clipboard_image` — calls
   `handle_remote_image_paste` with a constructed `ClipboardImage`; asserts the outcome is
   "consumed", that a pending stage was registered, and that the outbound `ClipboardStageRequest`
   observed on the mount's `out_tx` receiver carries `original_filename == "image.png"`.
   **Assert the filename on the wire request, not on the pending entry** —
   `PendingClipboardStage` (phase 04 requirement 2) deliberately stores only workspace, pane, origin,
   epoch, payload length and deadline; `original_filename` lives on `ClipboardStageRequest`
   (phase 01), so a pending-entry assertion is not writable against the phase-04 contract. Do not
   widen the pending struct to make a test convenient.
2. `image_paste_decision_is_fall_through_for_a_local_pane` — same key, local pane; asserts
   `FallThrough`, and a sibling `handle_key` assertion that the key reaches normal terminal handling
   (the Vim/Neovim `ctrl+v` regression guard). Drive the real `handle_key` with a
   `TerminalRuntime::test_with_channel` pane and assert the `ctrl+v` bytes **arrive on that
   receiver** — an assertion on the decision value alone passes against an intercept that returns on
   `FallThrough` and swallows the key.
2b. `fall_through_still_reaches_non_terminal_mode_handlers` — with `state.mode` set to a non-terminal
   mode (e.g. `Mode::Navigate`) and the binding key pressed, `handle_key` must still run that mode's
   handler. Guards the second half of the same control-flow error: the intercept sits above the
   `match self.state.mode`, so an early `return` on the default arm would disable every modal
   keymap, not just terminal keys.
3. `image_paste_decision_is_unsupported_when_the_mount_lacks_the_staging_capability` — mounted remote
   pane, empty agreed set; asserts `Unsupported`, and that the `handle_key` path raises the "too old"
   toast, **consumes** the key, and performs no wire send and no PTY write (pairs with phase 03's
   `stage_request_is_not_sent_when_capability_not_agreed` and its serving-side sibling). The
   consume-vs-fall-through distinction is the whole point of this test; do not weaken it.
   **Positive control required:** the same fixture with `file_staging` in the agreed set must put a
   `ClipboardStageRequest` on the `out_tx` receiver. Otherwise "no wire send" is satisfied by a
   fixture that could never send anything.
4. `clipboard_stage_failure_raises_a_toast_with_the_documented_copy` — table-driven over every
   failure variant in requirement 6, run against phase 04's mapping function; asserts the exact
   title/context strings and that **every** variant produces a toast (the P10 guard). Phase 04's
   test 10b holds the compile-time exhaustiveness; this one holds the exact copy.
5. `every_clipboard_stage_toast_string_fits_the_status_line` — asserts each context string's display
   width is under 60 cells, since `ui/status.rs:120-126` hard-clips.
6. `oversized_clipboard_image_is_rejected_before_any_wire_send` — the client-side precheck, driven
   through `handle_remote_image_paste` on a mount that **can** send: assert the oversize image
   produces the 16MB toast, no `out_tx` frame and no pending entry, and that an under-limit image
   through the identical fixture does send. The negative half alone would pass against a fixture with
   no live mount at all.

## Risks and rollback

- **Risk (A3, accepted):** `ctrl+v` stays the default and collides with readline quoted-insert and
  vim visual-block **on remote panes only**. Changing a shipped default is a breaking change;
  documented instead. Test 2 guarantees local panes are untouched.
- **Risk:** `read_clipboard_image()` runs inline on the App's key path and can block on a slow
  clipboard owner (X11). Same exposure the existing `read_clipboard_text()` call at
  `input/mod.rs:81` already accepts. If it proves visible, move it to `spawn_blocking` — do not
  restructure preemptively.
- **Risk:** consuming the key in every branch could swallow `ctrl+v` on a remote pane when the
  clipboard holds text, not an image. Accepted and toasted (the clipboard copy string). Users who
  want raw `ctrl+v` on remote panes rebind, as documented.
- **Rollback:** reverting this phase leaves phases 01-04 in place and completely inert — no trigger,
  no user-visible behavior change.
