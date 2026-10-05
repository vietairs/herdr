#!/usr/bin/env bash
# `conventional-commits` job: validate commit subjects. Run from anywhere; operates on the repo root.
#   push         -> every commit in $CI_BEFORE..$CI_AFTER
#   pull_request -> the PR title in $PR_TITLE
#   otherwise (local run) -> the subject of HEAD
set -euo pipefail
cd "$(dirname "$0")/../.."

case "${GITHUB_EVENT_NAME:-}" in
  push)
    python3 scripts/conventional_commits.py --range "${CI_BEFORE}..${CI_AFTER}"
    ;;
  pull_request)
    python3 scripts/conventional_commits.py "$PR_TITLE"
    ;;
  *)
    python3 scripts/conventional_commits.py "$(git log -1 --pretty=format:%s)"
    ;;
esac
