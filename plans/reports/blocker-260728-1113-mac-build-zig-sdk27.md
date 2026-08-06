# Blocker: mac build broken — Zig 0.15.2 libcxx vs macOS SDK 27.0

Date: 2026-07-28. Host: Can-less fork checkout, `/Users/hvnguyen/Projects/herdr`,
macOS (Darwin 27.0), Apple Silicon. Severity: blocks ALL local mac builds and
therefore any live/e2e verification on this machine.

## Symptom

Every `cargo check` / `cargo build` / `cargo test` fails in `build.rs:78` — the
vendored libghostty-vt Zig build step — before a single line of Rust compiles:

```
thread 'main' panicked at build.rs:78:5:
zig build for vendored libghostty-vt failed: exit status: 1
Build Summary: 25/28 steps succeeded; 1 failed
+- compile lib ghostty-vt ReleaseFast aarch64-macos.13.0 1 errors
```

Underlying error is inside Zig's own bundled libcxx, reached while compiling the
vendored SIMD C++ (`vendor/libghostty-vt/src/simd/*.cpp`) against the SDK:

```
/Users/hvnguyen/.local/zig-0.15.2/lib/libcxx/src/random_shuffle.cpp:10
  #include <random>
  -> __random/geometric_distribution.h
  -> __random/negative_binomial_distribution.h
  -> __random/poisson_distribution.h
  -> __random/clamp_to_integral.h
```

## Cause

Zig 0.15.2's bundled libcxx does not compile against **macOS SDK 27.0**. Only
`Xcode-beta.app` is installed on this machine, and its SDK is 27.0.

`CLAUDE.md` pins Herdr to Zig 0.15.2, so this is a genuine version collision
between a pinned toolchain and a newer-than-supported platform SDK, not a
misconfiguration.

## Workarounds attempted — all failed

| Attempt | Outcome |
|---|---|
| `SDKROOT=/Library/Developer/CommandLineTools/SDKs/MacOSX26.5.sdk` | `xcrun` honors it (`xcrun --show-sdk-path` returns 26.5), but **Zig ignores it** — the emitted compile command still uses `MacOSX27.0.sdk`. Zig uses its own darwin SDK detection. |
| `DEVELOPER_DIR=/Library/Developer/CommandLineTools` | Changes the toolchain but not the SDK version: CommandLineTools' default `MacOSX.sdk` is **also 27.0** (`xcrun --show-sdk-version` -> `27.0`). |
| `LIBGHOSTTY_VT_SIMD=false` (env toggle read at `build.rs:52`) | Clears the C++ error, then fails at link with 10 undefined libc symbols: `_abort`, `_bzero`, `_clock_gettime`, `_fcopyfile`, `_free`, `_getenv`, `_isatty`, `_malloc_size`, `_posix_memalign`, `_realpath$DARWIN_EXTSN`. Not a viable path. |
| `xcode-select --switch` to a stable Xcode | **Not possible** — only `/Applications/Xcode-beta.app` exists; no stable Xcode installed. |

Available SDKs on this machine (all of `/Library/Developer/CommandLineTools/SDKs`
and the Xcode-beta platform dir) include `MacOSX26.5.sdk`, `MacOSX26.sdk`,
`MacOSX15.1.sdk`, `MacOSX14.5.sdk`, `MacOSX12.0.sdk` — an older SDK is present,
Zig just cannot be pointed at it via env.

## Options (all require a decision, none are local tweaks)

1. **Install a stable Xcode alongside the beta**, then
   `sudo xcode-select --switch /Applications/Xcode.app`. Needs sudo + a large
   download. Lowest risk to the repo; does not touch pinned deps.
2. **Bump the vendored Zig** past 0.15.2 to a version whose libcxx supports SDK
   27. Changes a pinned toolchain (`CLAUDE.md` states Herdr requires 0.15.2), so
   it is a project-level decision affecting CI, the Windows VM setup, and
   `vendor/libghostty-vt.vendor.json`.
3. **Force Zig's SDK selection** — patch `build.rs` to pass an explicit
   `--sysroot` / SDK path to `zig build`. Contained, but adds a local build
   hack and would need to be conditional so it does not break CI or Linux.
4. **Do nothing locally; build and test on Linux.** Current de-facto workaround
   (see below).

## Current workaround in use

Build and test on `appn-ltu-vm-100` (Linux, cargo 1.96.1, zig at
`~/.local/zig-0.15.2`, checkout at `~/Projects/herdr`):

```bash
# on the VM
git fetch origin && git checkout <sha>
# rsync/tar changed files in from the mac, then:
ZIG=~/.local/zig-0.15.2/zig cargo test --bin herdr
```

Note `cargo test` there needs `-- --test-threads=N`, not a bare `--test-threads`.

**Limitation that matters:** this only verifies compilation and unit tests. It
cannot verify anything whose runtime path is the *mac* client — notably the
federation clipboard fix (`fix-260728-remote-osc52-clipboard.md`), whose receiver
runs in `handle_federation_mount_ready` on the mounting client. That change is
therefore committed (`14ef6b3b`) compiled + unit-tested but **never run live**.

## Also discovered: the suite is not green on clean master

Unrelated to this blocker but found while establishing a baseline: full
`cargo test --bin herdr` fails on clean `master` with roughly 3-21 failures that
**change identity between runs** — observed `session::tests` (21, then 20), then
`app::api::plugins::*` + `pty::backend::unix::*` (3). Every one of them passes
under `--test-threads=1`. They mutate process-global env/socket state and
contaminate each other under parallel execution.

Consequence: a green full-suite run is currently not a usable merge gate on this
repo. Any "tests pass" claim must be paired with a stashed baseline comparison.
Worth fixing separately (serialize the offenders with a mutex, or mark them
`#[serial]`).

## Unresolved questions

1. Which option (1-4 above) do you want? Option 1 is least invasive to the repo;
   option 2 is the real fix if SDK 27 is going to be the norm.
2. Does upstream `ogulcancelik/herdr` already handle SDK 27, i.e. is this
   fork-local staleness or a genuine upstream gap? Not checked.
3. Does CI build on macOS, and if so is it already failing for the same reason,
   or does it pin an older runner image?
4. Should the parallel-test flakiness be filed/fixed before it masks a real
   regression? It currently makes the suite unusable as a gate.

## Note on recording

Filed on the fork at https://github.com/vietairs/herdr/issues/8 at the
maintainer's explicit direction. `CLAUDE.md`'s external-contributor guardrail
(agents must not open issues) targets the upstream `ogulcancelik/herdr` tracker;
this is the operator's own fork, which the guardrail exempts as a custom fork.
Nothing was filed upstream.

The fork's issue tracker was disabled (GitHub's default for forks) and was
enabled to allow this. Revert with
`gh repo edit vietairs/herdr --enable-issues=false`.
