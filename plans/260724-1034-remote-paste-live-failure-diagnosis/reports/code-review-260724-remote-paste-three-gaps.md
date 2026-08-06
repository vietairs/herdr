# Code review: remote paste three-gaps fix

Worktree `herdr-worktrees/remote-paste-cmdv-bridge`, branch
`fix/remote-paste-cmdv-bracketed-bridge`, base `b43cc5aa`. Uncommitted diff
only. Build (`cargo build --bin herdr`) and targeted tests all pass; baseline
matches report (8 warnings, unrelated to this change).

## Verdict: APPROVE_WITH_NITS

## Medium

**M1 — TOCTOU between drop-location gate and file read (symlink race)**
`src/image_path.rs:93-101` (`is_recognized_image_drop_location`) canonicalizes
the candidate path to prove it resolves inside `temp_dir()`, but the actual
read in `read_local_image_file` (`src/image_path.rs:108-132`) re-opens the
*original*, non-canonical `path` via `std::fs::metadata`/`File::open`. If the
path element is a symlink and its target is swapped between the two calls
(TTOCTOU window, e.g. `/tmp/x.png` retargeted from an in-bounds temp file to
`~/.ssh/id_ed25519` right after the gate passes), the bounded reader will
happily stage and forward the new target's bytes to the federation peer. The
threat requires local filesystem write access to the shared temp dir, so the
attacker is already inside the trust boundary the "reject don't strip"
contract otherwise defends (same class as the existing clipboard-staging
symlink note in project memory), but this is exactly the kind of race that
contract is meant to close. Low practical urgency given the narrow local-only
threat model, but worth either re-validating containment on the just-opened
`File` (`fstat` + compare canonical dir, or `O_NOFOLLOW`-equivalent) or
documenting the accepted residual risk explicitly in
`is_recognized_image_drop_location`'s doc comment (currently implies the gate
alone is sufficient).

## Low / Nits

- **N1** `src/platform/macos.rs:654-666` (`read_clipboard_image_via_file_url`)
  shells out to `osascript` synchronously; it's reached only via the existing
  blocking-thread wrapper (`begin_remote_clipboard_image_capture`, confirmed
  at `src/app/input/mod.rs:113-121`), so this is not a UI-freeze regression —
  noting only because the function itself has no comment pointing back to
  that guarantee, unlike `read_clipboard_image_via_pngf`'s sibling code.
- **N2** `is_recognized_image_drop_location` silently returns `false` (i.e.
  `FallThrough`, not a toast) for a `canonicalize()` failure such as a broken
  symlink or permission-denied traversal. Consistent with the module's
  documented "never touches the filesystem for the shape check, silent-reject
  on the rest" contract, but confirm this is intended: a broken symlink
  dropped by a terminal's paste-as-path convention degrades to "paste the raw
  path as text into the remote pane" rather than any explicit failure
  signal — acceptable per the diagnosis's stated scope, flagging for
  awareness only.

## Verified clean (no findings)

1. **Correctness / gate ordering** — `bracketed_paste_image_decision`
   (`src/app/input/mod.rs:849-885`) is called only after the pre-existing
   popup-pane and non-terminal-mode early returns in `handle_paste`
   (`src/app/input/mod.rs:160-171`), so it cannot fire outside `Mode::Terminal`.
   `FallThrough` is the only path that reaches the original `send_paste`
   forward, and that forward is textually unchanged (`src/app/input/mod.rs:204-211`).
   Confirmed by test
   `bracketed_paste_of_a_temp_image_path_on_a_local_pane_is_forwarded_unchanged`
   asserting byte-identical forwarded bytes.
2. **No FallThrough regression on the ctrl+v intercept** — the pre-existing
   keybinding block (`src/app/input/mod.rs:93-121`) still has no `FallThrough`
   arm and sits above the `match self.state.mode` dispatch, unchanged by this
   diff (only `remote_image_paste_decision`'s internals were refactored to
   call the new `resolve_remote_paste_target` helper, same observable
   decisions — verified via existing unmodified test suite passing).
3. **furl fallback failure modes** — every step in
   `read_clipboard_image_via_file_url`/`image_from_file_url_text`
   (`src/platform/macos.rs:654-680`) uses `.ok()?`/`?`, no `unwrap`; a failed
   `osascript` spawn, non-UTF8 stdout, non-matching shape, missing file, or
   oversized file all fall through to `None`, preserving `read_clipboard_image`'s
   existing `Option` contract and the `NoImage` toast path. Verified by the 5
   new macOS tests plus a manual read of every `?`/`.ok()?` site.
4. **Security — image_path validator**: path traversal is moot (validator
   only accepts absolute paths, and traversal segments are inert once the
   path is absolute + extension-matched); extension spoofing is bounded by
   the fixed allowlist in `recognized_image_extension`; size cap is enforced
   *before* full read via `read_limited_reader`'s streaming bound, not after
   loading — confirmed the multi-GB comment in `src/image_path.rs:7-12` matches
   the implementation. Control-character handling only strips `\r`/`\n`
   (multi-line reject), other control bytes / NUL are inert against
   `PathBuf`/syscalls on Unix. Symlink behavior is deliberate (canonicalize +
   `starts_with`), correctly rejecting a symlink that points outside the temp
   dir — see M1 above for the one residual gap (TOCTOU, not a logic bug).
5. **client/mod.rs removed code (-83 lines)** — pure extraction to
   `crate::image_path`; grepped remaining call sites
   (`read_image_file_from_terminal_drop`, `image_path_from_terminal_drop`) and
   confirmed both now delegate to the shared module with no dropped callers.
   Confirmed via `is_remote_client_process()` (`src/client/mod.rs:667-668`,
   gated on `REMOTE_KEYBINDINGS_ENV_VAR`, client-process-only) vs.
   `resolve_remote_paste_target` (`src/app/input/mod.rs:796-822`, gated on a
   federation-mounted `AppState` workspace, server-process-only) that these
   two paths are structurally disjoint today — client-side classic `--remote`
   bridge only fires with the env var set; server-side federation bridge only
   fires when the active workspace resolves to a live federation mirror.
   Neither can hand the same paste to both handlers, matching the G2
   rationale in the implementer report.
6. **Rust conventions** — no `unwrap()` on any newly added production path
   (test-only `unwrap()`s in `#[cfg(test)]` blocks are fine); `tracing::warn!`
   used for the oversized-file case, no stray `println!`;
   `src/image_path.rs:1` is `#![cfg(unix)]` so it compiles out entirely on
   Windows (does not touch/break the Windows build); `src/main.rs:70`
   registers `mod image_path;` unconditionally, which is correct since the
   module itself is internally `cfg(unix)`-gated at the crate-attribute level.
7. **Test quality** — spot-checked 3 tests against production code:
   `bracketed_paste_of_a_path_outside_the_temp_dir_is_forwarded_unchanged`
   genuinely exercises the drop-location gate in isolation (same shape as the
   accepted case, only location differs) and would fail if that gate were
   removed or inverted; `bracketed_paste_of_a_temp_image_path_is_unsupported_without_file_staging`
   asserts both the toast title and the absence of a wire frame, so it would
   catch a regression that silently drops the paste instead of surfacing
   `Unsupported`; `file_url_fallback_rejects_an_oversized_file` builds an
   actual `MAX_CLIPBOARD_IMAGE_PAYLOAD + 1`-byte file rather than mocking the
   size check, so it exercises the real streaming-bound reader. The
   implementer's own report also documents inversion mutation checks for both
   `bracketed_paste_image_decision` and `image_from_file_url_text`, which I
   did not need to re-run given the passing test run above.

## Build/test verification performed

- `cargo build --bin herdr`: succeeds, 8 warnings, none in touched files —
  matches report baseline.
- `cargo test --bin herdr` filtered on `image_path::`, `bracketed_paste`,
  `file_url_fallback`: 12 + 15 + 5 = 32 tests, all pass.
- Did not run the full 3000+ suite (narrow-first per repo convention); no
  reason to expect broader regressions given the change is additive/extraction
  only outside the three touched call sites.

## Unresolved questions

- M1's residual TOCTOU: worth a follow-up decision on whether to harden (fstat
  after open, compare device/inode against a re-canonicalized parent) or
  explicitly accept as out-of-scope given the local-attacker precondition.

Status: DONE
Summary: Three-gap fix is structurally sound — G2's single-owner rationale holds (env-var-gated client path vs. federation-gated server path are provably disjoint), G3's furl fallback fails safe at every step, and G1's shared validator correctly rejects the traversal/extension/multi-line cases it targets. One medium finding: a TOCTOU window between the canonicalized drop-location check and the non-canonical file read that a local attacker with temp-dir write access could exploit via symlink retargeting.
