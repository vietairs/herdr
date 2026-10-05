#!/usr/bin/env bash
# `Windows ConPTY package` job of ci.yml. Windows only (never runs locally).
#   build   cargo build of the herdr binary
#   verify  tampered-bundle rejection, official ConPTY package build, enhanced-input probe, installer test
#   (none)  build, then verify
# Needs rust (x86_64-pc-windows-msvc), zig and PowerShell 7 (pwsh) on PATH.
set -euo pipefail
cd "$(dirname "$0")/../.."

stage="${1:-all}"

if [ "$stage" = build ] || [ "$stage" = all ]; then
  cargo build --locked --target x86_64-pc-windows-msvc
fi

if [ "$stage" = verify ] || [ "$stage" = all ]; then
  ps="$(command -v pwsh)"
  "$ps" -NoProfile -ExecutionPolicy Bypass -File scripts/ci/windows-conpty-package-verify.ps1
fi
