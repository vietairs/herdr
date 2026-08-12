Task: remote workspace federation v2 — protocol, both-ends server/client,
remote panes, materialization mechanism.

Shipped: PR #1 — https://github.com/vietairs/herdr/pull/1, merged
2026-07-22 as `965c15ea`.

Force-archived despite UNKNOWN classification (pipeline-progress.md never
got the literal "# PIPELINE COMPLETE" marker) — verified via `gh pr view 1`
that the PR merged; the only unchecked step in the doc was "merge PR #1",
explicitly noted as outside this pipeline's own scope ("NOT cortex's to do
— belongs to the user").

D9 manual smoke (13a) passed all 4 checks live over SSH (vm100->vm105)
before merge: mount/render, live-stream round-trip, New Worktree correctly
rejected, clean disconnect.

Archived-at-SHA: 4ccf09a812e5227d95b6e7a8a511d3be7a8cc565
