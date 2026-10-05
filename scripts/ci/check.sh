#!/usr/bin/env bash
# `check` job of ci.yml (ubuntu / macos / windows legs): lint, tests and platform smoke checks.
# Needs rust (rustfmt, clippy), just, cargo-nextest, bun, zig (and python3) on PATH.
# NEXTEST_FILTER overrides the per-OS default nextest filter. Run from anywhere; operates on the repo root.
set -euo pipefail
cd "$(dirname "$0")/../.."

case "${RUNNER_OS:-$(uname -s)}" in
  Linux) os=linux ;;
  macOS | Darwin) os=macos ;;
  Windows | MINGW* | MSYS* | CYGWIN*) os=windows ;;
  *) echo "check.sh: unsupported OS ${RUNNER_OS:-$(uname -s)}" >&2; exit 2 ;;
esac

# Windows has no nextest filter; macOS skips the live-handoff binary.
default_filter="all()"
[ "$os" = macos ] && default_filter="not binary(live_handoff)"
filter="${NEXTEST_FILTER:-$default_filter}"

export CARGO_INCREMENTAL=1
# A few timing-sensitive tests flake on the busier self-hosted machines. nextest reruns a failed
# test up to twice and reports a pass-on-retry as FLAKY; a test that keeps failing still fails.
export NEXTEST_RETRIES="${NEXTEST_RETRIES:-2}"

case "$os" in
  linux)
    just lint
    just ci-tests "$filter"
    LIBGHOSTTY_VT_SIMD=false LIBGHOSTTY_VT_OPTIMIZE=ReleaseSafe \
      cargo nextest run --locked -E 'package(ghostty-vt) | test(ghostty)' \
      --status-level fail --final-status-level fail --failure-output final --success-output never
    ;;
  macos)
    just ci "$filter"
    ;;
  windows)
    just check
    ps="$(command -v pwsh)"
    exe="$PWD/target/debug/herdr.exe"
    command -v cygpath >/dev/null && exe="$(cygpath -w "$exe")"
    "$ps" -NoProfile -ExecutionPolicy Bypass -File scripts/windows_smoke_conpty_path.ps1 \
      -ExePath "$exe" -Session "ci-windows-${GITHUB_RUN_ID:-local}-${GITHUB_RUN_ATTEMPT:-0}"
    ;;
esac
