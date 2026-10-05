#!/usr/bin/env bash
# `flake check` job of nix.yml. Needs nix (flakes enabled) on PATH.
set -euo pipefail
cd "$(dirname "$0")/../.."

nix flake check --print-build-logs
nix flake check --all-systems --no-build --print-build-logs
