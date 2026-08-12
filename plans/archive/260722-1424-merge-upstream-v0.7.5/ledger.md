Task: merge upstream v0.7.5 into the fork's master, keeping federation
intact.

Shipped: 4 conflicted files / 10 hunks resolved (agent.start redesigned
upstream; fork's emission fix dropped as moot). PROTOCOL_VERSION already at
17 == v0.7.5, no bump needed. 2955 tests pass (2 known cross-test-contention
failures pass in isolation). Federation/remote/mount test suites all green.

Merge commit: fork master fast-forwarded to 5ec2a10.

Learnings: this merge-base later turned out wrong for the subsequent
v0.8.0/force-push graft — see herdr-upstream-force-push-graft memory.

Overhead: 1 agent, ~40 min, ~150k tokens.

Archived-at-SHA: d2ce3de280b2446a3d07575795416b6976077898
