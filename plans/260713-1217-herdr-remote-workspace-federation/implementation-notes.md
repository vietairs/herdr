# Implementation Notes — herdr remote→local-workspace federation (Tier-2, two-server)

Append-only. 4-line entries: What / Why / Evidence / Reversibility. Log decisions, deviations,
surprises the moment they happen.

Plan: plan.md (v2, supersedes v1). Branch: feat/remote-workspace-federation (off master).
Architecture: our herdr on BOTH ends; remote = headless federation server; new federation protocol
over the SSH bridge; local single-source EventHub + per-mount replica reducer; TerminalSource seam
for remote-backed panes; capability negotiation preserves legacy full-screen `--remote` fallback.

## Deviations / Decisions / Surprises

- What: Started cook on root phases P1 (federation protocol + id-fencing) and P2 (TerminalSource seam).
  Why: both are independent roots per plan.md; P1 unblocks P3/P4, P2 unblocks P5.
  Evidence: plan.md dependency graph; phase-01/phase-02 disjoint file ownership.
  Reversibility: pure-additive lib (P1) + behavior-preserving refactor (P2) — trivially revertible before P3+.

- What: BLOCKED — herdr will not compile in this environment. Vendored libghostty-vt requires zig
  0.15 (build.zig uses 0.15 std.Build API; zig 0.16 breaks it), but zig 0.15.2 cannot link on
  macOS 27 / Darwin 27 — its bundled darwin libSystem stubs predate this OS, so even the zig build
  RUNNER fails with undefined _sigaction/_waitpid/_realpath$DARWIN_EXTSN/_sysctlbyname/_posix_memalign.
  Why: macOS 27 is newer than zig 0.15.2; nix (flake pins zig_0_15 + sysroot) is not installed here.
  Evidence: build.rs:78 zig panic; vendor/libghostty-vt/build.zig.zon minimum_zig_version="0.15.2";
  flake.nix:88 zig_0_15; clang links fine (SDK 27.0 present, Xcode-beta). 4 attempts: 0.16(API),
  0.15.2, +SDKROOT, +MACOSX_DEPLOYMENT_TARGET — all fail at the build-runner link step.
  Reversibility: N/A (environment). P1 code is written+committed (WIP, unvalidated: cannot cargo
  build/test). Unblock options: build on a Linux remote, install nix + `nix develop`, use an older
  macOS, or wait for zig macOS-27 support. Cook is PAUSED at P1 pending a buildable environment.
