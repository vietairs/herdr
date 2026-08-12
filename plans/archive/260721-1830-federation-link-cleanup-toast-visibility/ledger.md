Task: generation-fenced mount teardown when a federation link ends, plus
Terminal/System toast forwarding through the headless server.

Shipped: fixed generation race on link-close teardown; wired headless
toast-forwarding so mount-failure toasts reach the TUI; live e2e verified
(link kill → workspace.close → remount OK, no stale "already live" state).

Commits: 6d36a5e + e71547f. Official binaries redeployed (Mac + VMs).

Deviations: 2 logged in implementation-notes.md during /hvn-cook.

Code review: REQUEST_CHANGES → all 5 findings fixed with tests (2696/2696
green).

Ship-gate: PASS, attested by user 2026-07-21 19:32.

Archived-at-SHA: d2ce3de280b2446a3d07575795416b6976077898
