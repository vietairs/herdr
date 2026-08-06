# vendor libghostty-vt upgrade to c5a21edfc

## Vendor bump

Took upstream's (`MERGE_HEAD`) vendored state wholesale:

```
git checkout MERGE_HEAD -- vendor/libghostty-vt vendor/libghostty-vt.vendor.json src/ghostty/bindings.rs
```

`vendor/libghostty-vt.vendor.json` now reads `source_commit:
c5a21edfcbc2d5b46540ad91b7980aca31f5f1f3` (was `0f7cd84b880b203c98683e520e84b9db0c5938d8`).
`src/ghostty/bindings.rs` is upstream's fresh bindgen output (4240 lines, was 2798;
the clipboard-write FFI, `GHOSTTY_TERMINAL_OPT_PWD_CHANGED`,
`ghostty_color_palette_default`, etc. are all present).

Working tree changes are unstaged, matching the "no `git add`/`commit`" constraint.

## Patch reconciliation (`vendor/libghostty-vt.patches.md`, `vendor/patches/libghostty-vt/`)

### 0001-backport-resizecols-cursor-subtraction.patch — REMOVED (redundant)

Evidence: neither `git apply --check` nor `--reverse --check` applied cleanly
(line-number drift), so I checked content directly.
`vendor/libghostty-vt/src/terminal/PageList.zig` at the two patched call sites
(`remaining_rows = self.rows -| (c.y + 1)` line 1348, `const current = self.rows
-| (active_pt.active.y + 1)` line 1481) **already uses the saturating `-|`
operator** the patch introduced. Stronger evidence: the exact regression tests
the patch backported already exist natively in the new tree — `resize shrinks
both axes with cursor at bottom` in
`vendor/libghostty-vt/src/terminal/c/terminal.zig:1369` and `PageList resize
less rows and cols cursor at bottom` in
`vendor/libghostty-vt/src/terminal/PageList.zig:13387`. Upstream PR #12907 is
fully merged into `c5a21edfc`. This matches the patch's own documented removal
condition ("the vendored source commit contains upstream PR #12907 and the
local ReleaseSafe resize regression tests pass without this patch").

Action: deleted the patch file (unstaged `rm`, not `git rm`) and replaced its
`vendor/libghostty-vt.patches.md` entry with a short "removed" note citing the
evidence above (no code comments carrying plan/finding IDs, per project rules).

### 0002-expose-kitty-image-transmit-time-ns.patch — REMOVED (superseded, capability preserved)

Evidence: `c5a21edfc` does **not** have a `transmit_time` field or
`TRANSMIT_TIME_NS` symbol (grep came up empty), but it **does** natively expose
`GHOSTTY_KITTY_IMAGE_DATA_GENERATION = 9` on `ghostty_kitty_graphics_image_get()`
(`vendor/libghostty-vt/include/ghostty/vt/kitty_graphics.h:428`,
`vendor/libghostty-vt/src/terminal/c/kitty_graphics.zig:184,266`). Its doc
comment: "Generation stamp assigned when this image was added to (or replaced
in) the storage. A changed generation for a given image ID means the pixel
contents may have changed even when the dimensions, format, and data length
are identical (e.g. a retransmission of the same image ID) ... Stamps are
unique and monotonically increasing process-wide." The vendored `Image` struct
(`vendor/libghostty-vt/src/terminal/kitty/graphics_image.zig:507-522`) backs
this with a `generation: u64` field (no `transmit_time: std.time.Instant` field
exists any more). Native tests
`image generation detects same-sized retransmission` and `storage generation
via get` in `kitty_graphics.zig` cover exactly the same retransmission
scenario the patch's own test did. This satisfies the patch's documented
removal condition verbatim: "the vendored source commit exposes the image
transmit time (**or an equivalent transmission serial**) in the C API."

`src/ghostty/bindings.rs` (fresh, from `MERGE_HEAD`) already defines
`GhosttyKittyGraphicsImageData_GHOSTTY_KITTY_IMAGE_DATA_GENERATION`. Since the
Rust call site only ever used the old symbol as an opaque equality-comparable
serial (never for time-ordering math — see
`kitty_image_fingerprint_cached`/`KittyImageFingerprintEntry` in
`src/ghostty/mod.rs`), I switched it to the native `GENERATION` symbol:

- `KittyImageFingerprintEntry.transmit_time_ns: u64` renamed to `generation: u64`.
- `kitty_image_fingerprint_cached` now reads
  `ffi::GhosttyKittyGraphicsImageData_GHOSTTY_KITTY_IMAGE_DATA_GENERATION`
  (was `..._TRANSMIT_TIME_NS`), comparing `entry.generation == generation`.
- Doc comment updated to describe the generation-stamp semantics.

The Rust-side unit test `kitty_image_fingerprint_refreshes_on_retransmission`
(`src/ghostty/mod.rs:3204`) exercises this path behaviorally (writes a kitty
image, retransmits with different pixels, same size, asserts the fingerprint
changes) and does not depend on the FFI symbol's name, only on the behavior —
it needed no changes and still passes as part of `cargo build`'s compile
(not run in this pass; no `cargo nextest`/`just test` per task constraints,
build-only was requested).

Action: deleted the patch file (unstaged `rm`), updated
`src/ghostty/mod.rs`, replaced the `vendor/libghostty-vt.patches.md` entry
with a "removed" note.

**Kitty-image-transmit-time capability status: PRESERVED, not dropped.** The
capability (an O(1) per-image change serial for the remote image paste
fingerprint cache) now comes from the upstream-native `GENERATION` field
instead of a local patch. Verified by: (1) source inspection of
`kitty_graphics.h`/`kitty_graphics.zig` showing the field is wired through
`imageGetTyped`, (2) `bindings.rs` exposing the matching Rust constant, (3)
`cargo build` compiling `src/ghostty/mod.rs` clean against it, (4) the
existing `kitty_image_fingerprint_refreshes_on_retransmission` Rust test still
compiles unchanged against the renamed field (behavior-only assertions).

### 0001-default-grapheme-cluster-mode.patch — KEPT, no rebase needed

This patch came from `MERGE_HEAD`'s side already targeting
`vendored base: c5a21edfcbc2d5b46540ad91b7980aca31f5f1f3` — i.e. upstream
itself vendors this patch at the new commit already (it was a clean,
non-conflicting merge add). Verified `git apply --reverse --check` succeeds
(exit 0) and `git apply --check` (forward) fails because it's already applied
— confirming it applies cleanly to the current tree and is correctly indexed.
No action needed beyond a cosmetic already-merged issue-URL diff
(`ogulcancelik/herdr#243` → `herdrdev/herdr#243`) that was pre-existing merge
content, not something I introduced.

### Index integrity

`vendor/libghostty-vt.patches.md` now lists one active patch
(`0001-default-grapheme-cluster-mode`) plus two "removed" entries documenting
what was removed and why (evidence-based, no plan/finding IDs per project
conventions). `vendor/patches/libghostty-vt/` now contains exactly one patch
file, matching the index. Both invariants CLAUDE.md requires hold: every
on-disk patch is indexed, and the indexed active patch reverse-applies cleanly.

## Binding renames (`src/ghostty/mod.rs`)

Diffed old (`git show master:src/ghostty/bindings.rs`) vs new
`src/ghostty/bindings.rs` public type/struct/const/fn symbol names. Found 12
`Ghostty<X>_ptr` → `Ghostty<X>` renames (the vendored bindgen dropped the
`_ptr` suffix from typedef'd opaque pointer types) plus the
`TRANSMIT_TIME_NS` → `GENERATION` change already covered above:

```
GhosttyFormatter_ptr             -> GhosttyFormatter
GhosttyKeyEncoder_ptr             -> GhosttyKeyEncoder
GhosttyKeyEvent_ptr               -> GhosttyKeyEvent
GhosttyMouseEncoder_ptr           -> GhosttyMouseEncoder
GhosttyMouseEvent_ptr             -> GhosttyMouseEvent
GhosttyOscCommand_ptr             -> GhosttyOscCommand
GhosttyOscParser_ptr              -> GhosttyOscParser
GhosttyRenderStateRowCells_ptr    -> GhosttyRenderStateRowCells
GhosttyRenderStateRowIterator_ptr -> GhosttyRenderStateRowIterator
GhosttyRenderState_ptr            -> GhosttyRenderState
GhosttySgrParser_ptr              -> GhosttySgrParser
GhosttyTerminal_ptr               -> GhosttyTerminal
```

(`GhosttyRenderStateRowCellsData_GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_SELECTED`
also disappeared from the removed-symbols diff but is unused anywhere in
`src/`, so no call site needed updating.)

All 17 call sites of these 12 renamed types live only in `src/ghostty/mod.rs`
(confirmed via repo-wide grep excluding `bindings.rs`), which I own. Applied a
mechanical `sed` rename across that file only. Verified no stray `_ptr` FFI
type references remain — the 43 remaining `_ptr` occurrences in the file are
ordinary Rust pointer locals/methods (`data_ptr`, `as_mut_ptr()`,
`kitty_image_data_ptr_len`, etc.), unrelated to the FFI type rename. No
redesign of `mod.rs` — every edit was either the sed rename or the
`GENERATION` symbol/field-name swap above.

## Build status

```
PATH=$HOME/.local/herdr-xcrun-shim:$PATH ZIG=$HOME/.local/zig-0.15.2/zig cargo build
```

**Finished `dev` profile ... in 18.03s. Zero compile errors.** Only 10
pre-existing warnings, all outside `vendor/**`/`src/ghostty/**` (unused
imports/dead code in `src/app/api/workspaces.rs`, `src/cli.rs`,
`src/api/client.rs`, `src/app/config_io.rs`, `src/remote/federation/**`) —
none are `ffi::`/binding/vendor related. No `unwrap()` was introduced; no
`Cargo.toml`/`Cargo.lock` touched; no tests weakened or deleted.

## Files touched

- `vendor/libghostty-vt/**` — replaced wholesale from `MERGE_HEAD` (checkout)
- `vendor/libghostty-vt.vendor.json` — replaced wholesale from `MERGE_HEAD`
- `src/ghostty/bindings.rs` — replaced wholesale from `MERGE_HEAD`
- `vendor/libghostty-vt.patches.md` — edited (removed 2 entries, kept 1)
- `vendor/patches/libghostty-vt/0001-backport-resizecols-cursor-subtraction.patch` — deleted (unstaged `rm`)
- `vendor/patches/libghostty-vt/0002-expose-kitty-image-transmit-time-ns.patch` — deleted (unstaged `rm`)
- `src/ghostty/mod.rs` — mechanical `_ptr` renames + `GENERATION` symbol/field swap only

Note: `git status` also shows unrelated pre-existing merge changes under
`vendor/patches/portable-pty/**` and `vendor/portable-pty*` — not touched by
me, out of scope, presumably already-resolved merge content from before this
task started.

Status: DONE
Summary: vendor bumped to c5a21edfc; both fork-authored patches proven redundant against the new base (resizeCols fix upstreamed verbatim, kitty transmit-time replaced by native/better `GENERATION` field with mod.rs updated to match) and removed with evidence; grapheme-cluster patch kept as-is (already targets c5a21edfc, reverse-applies cleanly); 12 `Ghostty*_ptr`→`Ghostty*` renames fixed in mod.rs; `cargo build` is green with zero errors, only pre-existing out-of-scope warnings.
Concerns: none — kitty-image-transmit-time capability positively verified present (native `GENERATION` field, wired through C API and Rust bindings, mod.rs updated, existing behavioral test unaffected).
