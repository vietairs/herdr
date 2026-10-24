#!/usr/bin/env bash
# `build` job of website.yml: validate docs snapshots, then build the published site and the draft.
# Needs bun, pnpm and node on PATH. Run from anywhere; operates on the repo root.
set -euo pipefail
cd "$(dirname "$0")/../.."

node website/scripts/docs-versions.mjs check
node website/scripts/docs-preview.mjs check

(cd website && pnpm install --frozen-lockfile && pnpm run build)
(cd website && pnpm run build:draft)
