# libghostty-vt local patches

This file tracks intentional local changes applied on top of the vendored
`libghostty-vt` source. Remove a patch only when the vendored source commit
contains the upstream fix and the listed verification still passes.

## 0001 default lib-vt panes to grapheme clustering

status: active

patch: `vendor/patches/libghostty-vt/0001-default-grapheme-cluster-mode.patch`

herdr issue: https://github.com/herdrdev/herdr/issues/243

upstream discussion: not opened; libghostty-vt currently exposes current mode mutation but no C API for configuring terminal default modes

upstream pr: not opened

vendored base: `c5a21edfcbc2d5b46540ad91b7980aca31f5f1f3`

local files:

- `vendor/libghostty-vt/src/terminal/c/terminal.zig`

reason: Herdr renders terminal cells directly and requires DEC private mode
2027 to store flags, ZWJ emoji, and other multi-codepoint grapheme clusters in
one cell. This patch makes clustering active for new terminals and keeps it as
the reset default so RIS (`ESC c`) does not disable it.

remove when: libghostty-vt exposes a C API for setting default mode 2027, or
upstream makes grapheme clustering the lib-vt default, and the reset-survival
regression passes without this patch.

verification:

```sh
cargo nextest run --locked grapheme_cluster_mode_is_default_and_survives_full_reset
cargo nextest run --locked grapheme_cluster_mode_renders_flag_emoji_in_single_wide_cell
cargo nextest run --locked grapheme_cluster_mode_renders_zwj_family_in_single_wide_cell
```

## removed: expose kitty image transmit time in the C API

Removed when bumping the vendored source to `c5a21edfcbc2d5b46540ad91b7980aca31f5f1f3`.
That commit natively exposes `GHOSTTY_KITTY_IMAGE_DATA_GENERATION` (index 9) on
`ghostty_kitty_graphics_image_get()` — a unique, monotonically increasing
per-image stamp that bumps on every (re)transmission
(`vendor/libghostty-vt/include/ghostty/vt/kitty_graphics.h`,
`vendor/libghostty-vt/src/terminal/c/kitty_graphics.zig`, tests `image
generation detects same-sized retransmission` and `storage generation via
get`). This is an equivalent (and better-documented) transmission serial to
the removed `GHOSTTY_KITTY_IMAGE_DATA_TRANSMIT_TIME_NS` this patch used to add,
so `src/ghostty/mod.rs::kitty_image_fingerprint_cached` was switched to read
`GHOSTTY_KITTY_IMAGE_DATA_GENERATION` from the stock bindings instead of a
locally patched symbol.

verification: `cargo nextest run kitty_image_fingerprint`

## removed: backport resizeCols cursor subtraction saturation

Removed when bumping the vendored source to `c5a21edfcbc2d5b46540ad91b7980aca31f5f1f3`.
That commit already contains upstream PR #12907: both `resizeCols` cursor
subtraction sites in `vendor/libghostty-vt/src/terminal/PageList.zig` use the
saturating `-|` operator, and the exact regression tests this patch backported
(`resize shrinks both axes with cursor at bottom` in
`vendor/libghostty-vt/src/terminal/c/terminal.zig`, `PageList resize less rows
and cols cursor at bottom` in `PageList.zig`) are already present natively.

verification:

```sh
zig build test-lib-vt -Demit-lib-vt -Doptimize=ReleaseSafe -Dtest-filter="resize shrinks both axes with cursor at bottom"
zig build test-lib-vt -Demit-lib-vt -Doptimize=ReleaseSafe -Dtest-filter="PageList resize less rows and cols cursor at bottom"
```
