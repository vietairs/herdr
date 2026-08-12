# Remote paste three-gaps fix

Worktree: `/Users/hvnguyen/Projects/herdr-worktrees/remote-paste-cmdv-bridge`,
branch `fix/remote-paste-cmdv-bracketed-bridge`, base `b43cc5aa`. Not committed
— tree left staged-ready per instructions.

Gap 3 is the **live-confirmed** root cause: `osascript -e 'clipboard info'`
on the user's Mac showed the pasteboard held only `«class furl»` (a
Finder-copied image file), never `«class PNGf»`/TIFF, so the existing
`ctrl+v` capture path (`remote_image_paste_decision` → `read_clipboard_image`)
was always going to report "no image" for that copy method regardless of
launch route or paste style. Gaps 1 and 2 are the cmd+v/bracketed-paste and
launch-route gaps identified in the earlier diagnosis — both real, but latent
next to Gap 3 for this specific repro.

## Gap 3 — macOS clipboard file-URL fallback (implemented, live-confirmed cause)

`src/platform/macos.rs`: `read_clipboard_image` (~591) now tries the existing
`«class PNGf»` coercion first (renamed to `read_clipboard_image_via_pngf`,
behavior unchanged) and, only if that returns `None`, falls back to
`read_clipboard_image_via_file_url` (~654): resolves
`POSIX path of (the clipboard as «class furl»)` via `osascript`, then hands
the raw stdout text to `image_from_file_url_text` (~675), a small pure
function that composes `crate::image_path::local_image_path_from_text`
(shape check — single line, absolute, recognized extension) with
`crate::image_path::read_local_image_file` (existing-file, regular-file, and
`MAX_CLIPBOARD_IMAGE_PAYLOAD`-bounded read). `extension` comes from the
actual file extension via `local_image_path_from_text`, not a hardcoded
`"png"` — a `.jpg`/`.jpeg` file staged through this path gets `extension:
"jpg"` like every other route into `ClipboardImage`.

Deliberately **not** gated by `crate::image_path::is_recognized_image_drop_location`
(the temp-dir-only restriction Gap 1 added): that gate exists to stop the
*automatic* bracketed-paste bridge from mistaking ordinary pasted text for an
image drop. This fallback only ever runs behind the explicit
`remote_image_paste` keypress, already gated on a federation mount with
`FILE_STAGING` agreed (`RemoteImagePasteDecision::Capture`), so any image
file the OS clipboard genuinely names — including a screenshot on the
Desktop, not just one under `/var/folders/.../T` — is legitimate to read.
Same size cap and same "reject, never rewrite" validation as Gap 1; no
directories, no relative paths, no multi-line payloads, all inherited from
the shared `crate::image_path` contract rather than reimplemented.

Failure of the fallback is `None` at every step (`.ok()?`, `?` on the
validator, `?` on the reader) — no `unwrap`, no panic — so `read_clipboard_image`
still returns the same `Option<ClipboardImage>` contract as before and the
existing `NoImage` → `TOAST_NO_CLIPBOARD_IMAGE` toast path is unchanged for a
pasteboard that has neither shape.

`platform/mod.rs` contract: unchanged. `read_clipboard_image() -> Option<ClipboardImage>`
keeps its existing signature; Linux/Windows analogs (`platform/linux.rs`,
Windows via the client bridge) are untouched — this is macOS-local, per the
instruction to prefer that scope, since the furl-vs-PNGf pasteboard-type gap
is a macOS-specific `osascript` coercion limitation with no Linux/Windows
equivalent.

## Gap 1 — bracketed-paste path detection for remote panes (implemented)

New pure module `src/image_path.rs` (`#![cfg(unix)]`, registered in
`src/main.rs`): extracts the path-shape validation previously duplicated
inline in `client/mod.rs`'s `image_path_from_terminal_drop` —
`recognized_image_extension`, quote-stripping, backslash-unescaping,
single-line/absolute-path checks — into `local_image_path_from_text`. Also
adds `read_local_image_file` (bounded read via the existing
`platform::read_limited_reader`, same `MAX_CLIPBOARD_IMAGE_PAYLOAD` cap) and a
new `is_recognized_image_drop_location` (canonicalized `starts_with` against
`std::env::temp_dir()`).

`src/client/mod.rs`: `image_path_from_terminal_drop` (~1850) and
`read_image_file_from_terminal_drop` (~1822) now delegate to the shared
functions instead of duplicating them; the dead
`strip_matching_path_quotes`/`unescape_terminal_drop_path`/
`recognized_image_extension` copies were removed. Behavior for the classic
`--remote` bridge (`is_remote_client_process()` gate, bracketed-paste
unwrapping) is unchanged — all its existing unit tests pass unmodified.

`src/app/input/mod.rs`:
- Extracted `resolve_remote_paste_target` (~747-786) from
  `remote_image_paste_decision`: given `AppState`, resolves the active
  workspace's federation mirror and focused pane and whether `FILE_STAGING`
  is agreed, with no dependency on a `KeyEvent`. `remote_image_paste_decision`
  now calls it after the keybinding match, unchanged in behavior.
- New `bracketed_paste_image_decision` (~849-885): resolves the remote
  target, checks the pasted text against `local_image_path_from_text`, then
  gates on `is_recognized_image_drop_location` (temp-dir only) before
  checking capability and reading the file. Returns `FallThrough` for a local
  pane, a non-federated workspace, non-matching text, or a path outside the
  temp dir; `Unsupported` when the shape matches but the peer lacks
  `FILE_STAGING`; `Capture` with the read `ClipboardImage` otherwise.
- `handle_paste` (~161): after the existing non-terminal-mode redirect, calls
  `bracketed_paste_image_decision` and on `Capture` routes through the
  existing `handle_remote_image_paste` (same staging pipeline, same size cap,
  same toasts) instead of forwarding text; on `Unsupported` raises the same
  `TOAST_REMOTE_TOO_OLD` toast the ctrl+v gate uses; `FallThrough` continues
  to the original `send_paste` forward, byte-identical to before.

Security contracts preserved: size cap (16 MiB, via
`read_local_image_file`/`MAX_CLIPBOARD_IMAGE_PAYLOAD`), in-flight cap and
sanitization (unchanged, inside `remote_clipboard_stage.rs`, reached only via
`handle_remote_image_paste`), reject-don't-strip path validation (shape
check + drop-location check, both hard-reject to `FallThrough`, never
rewrite), non-silent failure (`Unsupported` toast).

## Gap 2 — launch-route marker (deliberately not enabled)

Investigated setting `REMOTE_KEYBINDINGS_ENV_VAR` for coexistence launches so
`is_remote_client_process()` becomes true and Mechanism A
(`should_bridge_clipboard_image_paste` / `image_path_from_terminal_drop` in
`client/mod.rs`) activates under `--remote-workspace`. Rejected: Mechanism A
runs on the **client** process and gates only on the global
`is_remote_client_process()` flag — it has no per-pane knowledge of which
workspace is a federation mount vs. local, and no `AppState` access to check
`FILE_STAGING`. Enabling the marker would make it intercept
temp-dir image paths pasted into **every** pane of that client process,
including local ones, which regresses the "local panes byte-identical"
requirement and duplicates Gap 1's server-side, capability-negotiated,
drop-location-gated interception without any coordination between the two.

Gap 1 alone already fixes the reported symptom (`--remote-workspace` /
federation mounts): `bracketed_paste_image_decision` only ever fires when
`resolve_remote_paste_target` finds a federation-mounted workspace, so it
naturally does not overlap with Mechanism A, which only fires when
`REMOTE_KEYBINDINGS_ENV_VAR` is set (classic `--remote` bridge, a disjoint
launch route today). Left the marker untouched; Gap 1 is the single owner
for federation-mounted panes, Mechanism A remains the single owner for the
classic bridge. No double-staging risk between them as shipped.

## Tests

- `src/image_path.rs`: 13 unit tests (later grown to 16 by the review-fix
  pass below) on `local_image_path_from_text` (accept
  single-line/quoted/escaped, reject multi-line/relative/non-image/empty),
  `recognized_image_extension`, `read_local_image_file` (missing file,
  directory, real file), `recognized_image_drop_location` (renamed from
  `is_recognized_image_drop_location` in the review-fix pass; temp dir vs.
  nonexistent path).
- `src/app/input/mod.rs` (`app::input::remote_image_paste_tests`), reusing
  `test_app`/`attach_remote_mount`/`attach_local_pane`/`stage_requests`:
  - `bracketed_paste_of_a_temp_image_path_stages_on_a_remote_pane` — valid
    temp-dir image path, `FILE_STAGING` agreed → decision is `Capture`,
    `handle_paste` stages it (wire request observed, no PTY forward).
  - `bracketed_paste_of_a_temp_image_path_on_a_local_pane_is_forwarded_unchanged`
    — same path text on a local pane → forwarded as raw bytes, unchanged.
  - `bracketed_paste_of_ordinary_text_on_a_remote_pane_is_forwarded_unchanged`
    — non-path text on a remote pane → no staging.
  - `bracketed_paste_of_a_temp_image_path_is_unsupported_without_file_staging`
    — valid path, capability not agreed → `Unsupported`, `TOAST_TITLE_FAILED`
    toast, no wire frame.
  - `bracketed_paste_of_a_path_outside_the_temp_dir_is_forwarded_unchanged` —
    same shape (existing file, recognized extension) but under `$HOME`, not
    the temp dir → `FallThrough`, proving the drop-location gate is load
    bearing on its own.
  - All 5 pass; existing `remote_image_paste_decision`/`handle_remote_image_paste`
    tests pass unmodified (the extraction did not change their behavior).

Mutation check: replaced `bracketed_paste_image_decision`'s body with an
unconditional `FallThrough` (short-circuit `return` before the real logic).
The two positive-path tests (`..._stages_on_a_remote_pane`,
`..._is_unsupported_without_file_staging`) failed as expected; reverted.

- `src/platform/macos.rs` (`platform::macos::tests`), 5 new tests around the
  extracted `image_from_file_url_text` seam (the AppleScript call itself
  stays thin/untested, per instructions — everything it feeds into is
  covered here without a live pasteboard):
  - `file_url_fallback_reads_a_valid_png` — valid temp-file PNG → bytes and
    `extension: "png"`.
  - `file_url_fallback_reads_a_valid_jpg_and_normalizes_jpeg` — a `.jpeg`
    file → `extension: "jpg"` (extension comes from the real file, not a
    hardcoded default).
  - `file_url_fallback_rejects_a_non_image_file` — `.txt` file → `None`.
  - `file_url_fallback_rejects_a_missing_path` — nonexistent path → `None`.
  - `file_url_fallback_rejects_an_oversized_file` — file one byte over
    `MAX_CLIPBOARD_IMAGE_PAYLOAD` → `None`.

Mutation check: replaced `image_from_file_url_text`'s body with an
unconditional `None`. The two positive-path tests (`..._reads_a_valid_png`,
`..._reads_a_valid_jpg_and_normalizes_jpeg`) failed as expected; reverted.

## Baseline verification

- `cargo build`: succeeds, 8 pre-existing warnings (unused imports/dead code
  in unrelated modules — `cli.rs`, `api/client.rs`, `remote/federation/id.rs`,
  `protocol/mod.rs`), none touching this change.
- `cargo clippy --all-targets`: 0 errors (pre-existing warnings only, in
  files this change did not touch: `pane/osc.rs`, `workspace.rs`,
  `app/api/workspaces.rs`, `app/input/mouse.rs`,
  `remote/federation/pane_source.rs`). Local baseline clippy-error count
  differs from the project-memory note of "3 pre-existing errors"; this run
  shows 0 `error`-level clippy diagnostics on this toolchain, so nothing new
  was introduced either way.
- `cargo fmt`: ran twice across the session (once per gap batch), `cargo fmt
  --check` clean after both.
- `cargo test --bin herdr -- --test-threads=4` (final run, all three gaps):
  3148 passed, 1 failed
  (`api::server::pane_graphics_stream::tests::inactive_owner_cancels_idle_stream_and_dispatches_close`,
  a timeout). Reran in isolation with `--test-threads=1`: passed — this is
  the pre-existing flake recorded in project memory
  (`herdr-354-conflict-merge-methodology`), unrelated to this change.

## Unresolved questions

- Whether Claude Code CLI on the remote host actually ingests a pasted
  absolute `.png` path as `[Image #N]`, or only reacts to its own clipboard
  read, is still unconfirmed (carried over from the diagnosis report) — out
  of scope for this fix, which only makes the staged path land correctly.
- Did not attempt a live `--remote-workspace` / live-clipboard repro on the
  VM or the reporting Mac from this worktree; verification here is
  unit/app-level only (Gap 3's `osascript` calls are exercised only through
  the extracted pure seam), per the delegated build/test environment, which
  has no VM or live-pasteboard access configured.

## Review fixes

Code review (`code-review-260724-remote-paste-three-gaps.md`) verdict:
APPROVE_WITH_NITS, one medium finding.

**M1 — TOCTOU between the drop-location gate and the file read (fixed).**
`is_recognized_image_drop_location` (bool) canonicalized the candidate path
to prove it resolved inside `temp_dir()`, but `bracketed_paste_image_decision`
then reopened the *original*, non-canonical path via `read_local_image_file`.
A symlink retargeted between the two calls (e.g. `/tmp/x.png` swapped from an
in-bounds temp file to `~/.ssh/id_ed25519` right after the gate passed) would
have staged and forwarded the new target's bytes. Fixed by changing the gate's
return type from `bool` to `Option<PathBuf>`
(`recognized_image_drop_location`, `src/image_path.rs:87-109`): it now hands
back the canonicalized path it validated, and the caller
(`src/app/input/mod.rs:880-895`) reads through that returned path instead of
the one it submitted. This collapses the window to a single resolve-then-open
pair — the same residual TOCTOU any path-based check inherently has — rather
than two independent resolutions that could see different targets. Gap 3's
`image_from_file_url_text` was already reading the path it validated directly
(no separate canonicalize step), so it needed no change; this class of race
was specific to Gap 1's drop-location gate.

Added `recognized_image_drop_location_resolves_a_symlink_to_its_real_target`
(`src/image_path.rs`): creates a real file and a symlink to it, both under
the temp dir, and asserts the function returns the *resolved* target path
(not the submitted symlink path), then reads through that returned path and
confirms it is the real file's bytes — the literal race isn't unit-testable,
but "the read goes through the canonical path, not the submitted one" is, per
the review's own framing. Also added
`recognized_image_drop_location_rejects_an_existing_file_outside_temp_dir`:
the pre-existing "rejects outside temp dir" test used a nonexistent path,
which short-circuits on `canonicalize()`'s `.ok()?` before ever reaching the
`starts_with` containment check — it would have passed even with that check
inverted. Confirmed by mutating the containment check to `if true` and
observing this specific new test fail (all others, including the old
nonexistent-path test, kept passing); reverted after confirming.

**N1 (nit, applied)** — added a comment to
`read_clipboard_image_via_file_url` (`src/platform/macos.rs`) noting it is
only ever reached through the existing blocking-thread wrapper
(`App::begin_remote_clipboard_image_capture`), matching the sibling
`read_clipboard_image_via_pngf`'s existing documentation. One-line, no
behavior change.

**N2 (nit, skipped)** — asked to confirm intent, not requesting a change; the
current silent-`FallThrough`-on-`canonicalize()`-failure behavior is already
the documented contract, so left as is per "skip anything speculative."

Renamed `is_recognized_image_drop_location` → `recognized_image_drop_location`
throughout (module, both call sites, all tests) since it no longer returns a
plain bool.

Re-verified after the fix: `cargo build` (8 pre-existing warnings, none in
touched files), `cargo fmt --check` clean, `cargo clippy --all-targets` (0
`error`-level diagnostics — same discrepancy noted before against the
project-memory "3 pre-existing errors" baseline; nothing new introduced
either way), and `cargo test --bin herdr -- --test-threads=4` at 3151/3151
passed on a clean rerun. One run in between showed 4 failures
(`pane_graphics_stream::...inactive_owner_cancels_idle_stream_and_dispatches_close`
plus three `server::headless::tests::*`), all `AddrInUse` port-contention
flakes from parallel socket-binding tests; all four passed individually with
`--test-threads=1`, and none touch the files this change modified.

Status: DONE
Summary: All three gaps implemented. Gap 3 (macOS `read_clipboard_image` now falls back from `«class PNGf»` to resolving and reading a `«class furl»` file-URL pasteboard entry) fixes the live-confirmed cause — the user's clipboard held only a file reference from a Finder copy, which the PNGf-only coercion could never see. Gap 1 (bracketed-paste image-path detection for federation-mounted remote panes) and Gap 2 (launch-route marker, deliberately left unset) remain as fixes for the latent cmd+v/bracketed-paste and launch-route gaps identified in the original diagnosis, layered on the same shared `crate::image_path` validation module Gap 3 also reuses. Code review's one medium finding (drop-location-check-to-read TOCTOU) is fixed: the gate now returns the canonicalized path it validated and the caller reads through that path, closing the window to a single resolve-then-open pair.
Concerns: Gap 1's bracketed-paste drop-location restriction (`std::env::temp_dir()` only) is narrower than "recognized screenshot location" — it doesn't cover e.g. `~/Desktop` screenshot defaults, only the iTerm2/Terminal.app temp-file convention the diagnosis observed; Gap 3's fallback is intentionally not restricted this way since it only fires behind an explicit keypress. None of the three gaps were exercised against a live remote host or live macOS pasteboard from this worktree — all verification is unit/app-level.
