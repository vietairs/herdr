Task: fix federation UI labels (badge width ate the folder name), stacked
on PR #10 (upstream v0.8.0 merge).

Shipped: badge-width fix for federation UI labels.

PR #11 — MERGED as `83d23438` (after PR #10 merged as `b2319b55`, per the
plan's own ordering requirement).

Findings from this pass falsified 2 verification claims made during PR #10:
"zero fork-original symbols lost" was false (27 lost, all run to ground and
benign — regenerated vendor bindings, one consolidated callback struct, one
upstream rename); "CLAUDE.md restored to fork's version" was false — it's a
symlink to AGENTS.md, and the real document took upstream's version
(+39/-25), deliberately not reverted (user's call, logged in
implementation-notes.md).

Open item resolved by later commits: AGENTS.md kept upstream's version;
`git replace -d ef4c23f5775bb8cfec05f05d0844226ff959a07a` and worktree
cleanup were on the post-merge checklist for PR #10, not this dir.

Archived-at-SHA: d2ce3de280b2446a3d07575795416b6976077898
