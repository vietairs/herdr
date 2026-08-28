# Windows build setup for this fork (verified 2026-08-28)

Nothing in the repo documented how to build on Windows; this is what it actually took.

## Prerequisites
- **VS Build Tools 2026** with the VC x86/x64 component — already present on this machine
  (`vswhere -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64`). Needed for the
  MSVC linker.
- **Rust 1.96.1**, pinned by `rust-toolchain.toml`. Installed via
  `winget install Rustlang.Rustup`; the pinned toolchain syncs on first cargo run.
- **Zig 0.15.2**, pinned by `.github/workflows/ci.yml:96`. `build.rs` shells out to `zig build`
  for the vendored libghostty-vt and panics with `program not found` without it. Installed to
  `%USERPROFILE%\.local\zig\zig-x86_64-windows-0.15.2`.

## The non-obvious part: the checkout path must be short and dot-free

Building from `C:\Users\viete\Projects\herdr\.claude\worktrees\<slug>` fails:

```
run exe uucode_build_tables (tables.zig) failure
error: failed to spawn and capture stdio from ...uucode_build_tables.exe: FileNotFound
```

The exe exists and Defender is not involved. Running it directly shows the real fault:

```
error: FileNotFound
  Ucd.zig:358 in parseUnicodeData
    const file = try std.fs.cwd().openFile(file_path, .{});
```

The `uucode` table generator opens Unicode data files through a path Zig computes RELATIVE to
its package cache (`%LOCALAPPDATA%\zig\p\uucode-0.2.0-...`). From a deep, dot-prefixed worktree
that relative path traverses far enough for Windows to return `OBJECT_PATH_NOT_FOUND`.
`ZIG_GLOBAL_CACHE_DIR` does not help — the package dir is what matters.

**Fix: check out somewhere short.** Moving the worktree from
`…\herdr\.claude\worktrees\remote-ssh-auth-loop-windows-federation` to `C:\Users\viete\hw-remote`
made the zig build succeed with no other change. Long paths are already enabled
(`LongPathsEnabled=1`), so this is Zig's relative-path handling, not MAX_PATH.

Consequence for the cortex worktree convention: `<repo>/.claude/worktrees/<slug>` is unusable for
Rust builds in this repo on Windows.

## What to run

The Windows gate is NOT the full test suite — see `justfile:40` and `justfile:52`:

```
LIBGHOSTTY_VT_SIMD=false cargo clippy --bin herdr --locked -- -D warnings
.\scripts\windows_check.ps1 -Mode check
```

`cargo check --all-targets` fails on Windows for pre-existing reasons: `tests/cli/agents.rs`
uses `fs::Permissions::from_mode` (unix-only) in several integration test binaries. That file is
untouched by this branch and the API is unix-only, so the failures are unrelated to these changes
(inferred, not bisected against `master`). They are outside the Windows gate either way.
