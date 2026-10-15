#!/usr/bin/env bash
# `build` job of website.yml: validate docs snapshots, then build the published site and the draft.
# Needs bun and node on PATH. Run from anywhere; operates on the repo root.
set -euo pipefail
cd "$(dirname "$0")/../.."

node website/scripts/docs-versions.mjs check
node website/scripts/docs-preview.mjs check

(cd website && bun install --frozen-lockfile && bun run build)
(cd website && bun run build:draft)
