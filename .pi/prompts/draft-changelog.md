---
description: Draft changelog entry from commits and PRs since last tag
---
Draft a changelog entry for this repo.

Optional starting ref override: `$1`
Extra user intent/context: `${@:2}`

Process:

1. Determine the base ref.
   - If `$1` is non-empty and looks like a ref/tag, use it.
   - Otherwise use the latest release tag, preferring the repo's semver tag style (for example `v0.1.2`):
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
   - Do **not** also list the individual commits that belong to that PR.

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

6. Audit public docs for release readiness.
   - Treat `HEAD` as the release candidate that will be released after the changelog is drafted.
   - Treat root `README.md`, `CONFIGURATION.md`, `INTEGRATIONS.md`, and `SOCKET_API.md` as the latest released public docs.
   - Treat `.pi/docs/README.md`, `.pi/docs/CONFIGURATION.md`, `.pi/docs/INTEGRATIONS.md`, and `.pi/docs/SOCKET_API.md` as the next-release versions of those public docs.
   - Compare meaningful user-facing changes in the range against `.pi/docs/` first. Flag missing next-release docs for new or changed features, commands, config keys, protocol behavior, integrations, defaults, and compatibility notes that should be documented for this release.
   - Compare `.pi/docs/` against the root docs. Flag each difference as either intended to ship in this release, stale, or needing user decision.
   - Also audit `website/` and example config snippets for release readiness, but keep them aligned with the latest published release unless the user explicitly asks for prerelease docs.
   - Do not edit docs unless the user explicitly asks; report the docs audit alongside the changelog draft.
   - When the user asks to apply the release draft, update `CHANGELOG.md`, apply approved documentation changes to `.pi/docs/`, then copy the approved `.pi/docs/` files into the matching root public docs so `just release-docs-check` passes.

7. Draft the changelog entry.
   - Group items under these sections when applicable:
     - `### Added`
     - `### Changed`
     - `### Fixed`
     - `### Removed`
     - `### Breaking Changes`
   - Write for end users, not for commit archaeology.
   - Merge related low-level commits into one human-readable bullet when appropriate.
   - Keep bullets concrete and outcome-focused.
   - Prefer one bullet per meaningful shipped change.
   - If there are both PRs and direct commits, include both, but exclude direct commits already covered by PRs.
   - For merged PR items, append the PR reference and contributor thanks inline when appropriate, in the form `(#123, thanks @author)`. Do this for merged PRs, not for direct commits.

8. Respect repo reality.
   - If `CHANGELOG.md` exists, read it before proposing edits and follow its existing style.
   - If no changelog file exists, say so explicitly and produce a draft entry only.
   - Do not edit files yet unless the user explicitly asks you to apply the draft.

Output format:

- `Base ref:`
- `PRs included:`
- `Direct commits included:`
- `Excluded as housekeeping:`
- `Next-release docs audit:`
- `Public docs finalization plan:`
- `Proposed changelog entry:`

If the range has no meaningful user-facing changes, say that plainly instead of forcing entries.
