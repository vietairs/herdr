Task: in-app dialog to mount a remote workspace.

Shipped: PR #3 — https://github.com/vietairs/herdr/pull/3, merged
2026-07-22 as `1c833031`.

Force-archived despite UNKNOWN classification (pipeline-progress.md never
reached its ship-gate/ship steps in the doc) — verified via `gh pr view 3`
that the PR merged. Remediation pass (step 9) reached verdict CLEAN: 5
parallel fixes (dead submitting path removed, client-side checks removed in
favor of server-authoritative validation, %target->?target fixed at 3
sites, docs corrected to ssh://user@host:port). 3066/3068 tests pass (2
pre-existing flakes outside the diff, pass in isolation).

Known gap carried forward: closed a pre-existing -oProxyCommand hole; never
live-tested end-to-end (see herdr-tui-mount-remote-dialog memory).

Archived-at-SHA: 4ccf09a812e5227d95b6e7a8a511d3be7a8cc565
