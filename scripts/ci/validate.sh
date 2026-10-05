#!/usr/bin/env bash
# `validate` job of distribution.yml: documentation lifecycle + distribution contract checks.
# Needs bun, node and python3 on PATH. Run from anywhere; operates on the repo root.
set -euo pipefail
cd "$(dirname "$0")/../.."

bun test scripts/docs/*.test.ts

python3 scripts/agent_detection_manifest_check.py --require-published
python3 scripts/config_reference_check.py
python3 scripts/docs_translation_parity.py --docs-root docs/next/website/src/content/docs
node scripts/docs/versions.mjs check
node scripts/docs/preview.mjs check

python3 -m unittest \
  scripts.test_agent_detection_manifest_check \
  scripts.test_config_reference_check \
  scripts.test_docs_translation_parity \
  scripts.test_preview
