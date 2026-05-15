---
description: Audit next-release docs and changelog before release
---
Audit release readiness for this repo.

Optional starting ref override: `$1`
Extra user intent/context: `${@:2}`

Process:

1. Determine the base ref.
   - If `$1` is non-empty and looks like a ref/tag, use it.
   - Otherwise use the latest release tag, preferring the repo's semver tag style:
     ```bash
     git describe --tags --abbrev=0
     ```

2. Inspect the range from base ref to `HEAD`.
   - Use first-parent history for release context:
     ```bash
     git log --first-parent --reverse --format='%H%x09%s' <base>..HEAD
     ```
   - Also inspect full commits when needed:
     ```bash
     git log --reverse --format='%H%x09%s' <base>..HEAD
     ```

3. Detect merged PRs if any.
   - Look for first-parent subjects that indicate PR merges, including squash merges like `title (#123)`.
   - If GitHub CLI is available and the PR number is known, use it to fetch PR title/body for context.
   - Treat a merged PR as the primary release unit.
   - Do **not** also list individual commits that belong to that PR.

4. Handle direct commits separately.
   - Any commit in the range not represented by a merged PR should be considered on its own.

5. Infer what matters.
   - For each PR or direct commit, inspect changed files and diff stats.
   - Read the most relevant files in full when needed to understand user-facing impact.
   - Ignore pure housekeeping unless it has release value:
     - version bumps
     - release/tag commits
     - changelog-only commits
     - formatting-only changes
     - comment-only/doc-only changes unless they materially affect users

6. Audit `.pi/docs/CHANGELOG.md`.
   - Treat root `CHANGELOG.md` as the latest released changelog.
   - Treat `.pi/docs/CHANGELOG.md` as the next-release changelog.
   - Compare meaningful user-facing changes in the commit range against `.pi/docs/CHANGELOG.md`.
   - Flag missing entries for new features, bug fixes, removals, breaking changes, defaults, compatibility changes, user-visible command/config/API behavior, and security-relevant changes.
   - Flag stale entries that do not appear to correspond to shipped changes in the range.
   - Flag entries that are too implementation-focused or unclear for end users.
   - Preserve the existing changelog style and sections: `Added`, `Changed`, `Fixed`, `Removed`, and `Breaking Changes` when applicable.

7. Audit next-release public docs.
   - Treat root `README.md`, `CONFIGURATION.md`, `INTEGRATIONS.md`, and `SOCKET_API.md` as the latest released public docs.
   - Treat `.pi/docs/README.md`, `.pi/docs/CONFIGURATION.md`, `.pi/docs/INTEGRATIONS.md`, and `.pi/docs/SOCKET_API.md` as the next-release versions.
   - Compare meaningful user-facing changes in the range against `.pi/docs/` first.
   - Flag missing next-release docs for new or changed features, commands, config keys, protocol behavior, integrations, defaults, and compatibility notes.
   - Compare `.pi/docs/` against the root docs. Flag each difference as intended to ship in this release, stale, or needing user decision.
   - Also audit `website/` and example config snippets for release readiness, but keep them aligned with the latest published release unless the user explicitly asks for prerelease docs.

8. Verify finalization state.
   - Before `just release`, approved `.pi/docs/*` files must be copied to their root counterparts.
   - Run or recommend:
     ```bash
     just release-docs-check
     ```
   - This check must include `README.md`, `CONFIGURATION.md`, `INTEGRATIONS.md`, `SOCKET_API.md`, and `CHANGELOG.md`.
   - Do not run `just release` unless the working tree is clean and the docs check passes.

9. Apply changes only when asked.
   - Do not edit files during the audit unless the user explicitly asks you to apply fixes.
   - When asked to apply audit fixes, update `.pi/docs/CHANGELOG.md` and any relevant `.pi/docs/` files first.
   - When asked to finalize release docs, copy approved `.pi/docs/` files into the matching root files and run `just release-docs-check`.

Output format:

- `Base ref:`
- `PRs included:`
- `Direct commits included:`
- `Excluded as housekeeping:`
- `Next-release changelog audit:`
- `Next-release docs audit:`
- `Root finalization status:`
- `Required changes before release:`
- `Proposed changelog edits:`
- `Proposed docs edits:`

If the range has no meaningful user-facing changes, say that plainly instead of forcing entries.
