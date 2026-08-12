Task: one command starts local + N remote federated workspaces in a single TUI.

Shipped: multi-remote federation launch flow; fail-loud rejection of
`--remote-keybindings server` + `--remote-workspace` combo (no server-side
seam exists — federation wire carries raw resolved bytes, no keybindings
handshake, would need a PROTOCOL_VERSION bump).

Deviations: keybindings=GLOBAL decision superseded by the fail-loud outcome
(logged in implementation-notes.md, reconciled at ship-gate).

Ship-gate: PASSED 2026-07-21 16:15 (user attested), 43 checks OK / 6 warnings
(test gaps / logged deviations). Merge handover left with the user — no
merge commit captured in the plan log.

Learnings: federation protocol has no keybindings-handshake seam; any future
per-remote keybindings feature needs a wire-format bump first.

Archived-at-SHA: d2ce3de280b2446a3d07575795416b6976077898
