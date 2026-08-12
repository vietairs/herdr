# PIPELINE COMPLETE
# Pipeline Progress

- [x] 1a. /hvn-debug (sidebar agent detection) — done 00:00 — reports/debug-260721-2357-remote-agent-detection-not-in-sidebar-report.md (cause: client.rs:451 discards AgentStatus frames; relayed_agent_status_sender never wired)
- [x] 1b. /hvn-debug (remote pane rendering corruption, vm105) — done 00:08 — reports/debug-260722-0001-remote-pane-render-corruption-report.md (cause: local resize-recovery ANSI replay in pane/terminal.rs:1359-1389 fires for remote panes, racing async remote repaint)
- [x] 2b. /hvn-fix (gate resize-replay off for remote panes) — done 00:16 — reports/fix-260722-0008-remote-resize-replay-gate-report.md (is_remote_backed flag threaded through resize; 275 pane + 35 resize tests green, clippy clean)
- [x] 1c. /hvn-debug (split in remote workspace spawns local pane) — done 00:11 — reports/debug-260722-0002-remote-split-spawns-local-pane-report.md (cause: handle_pane_split has no remote branch; FederationMessage lacks any create/split-pane variant — protocol gap)
- [x] 2c. /hvn-fix (remote split protocol scaffolding + refusal guard) — done 00:20 — reports/fix-260722-0008-remote-split-protocol-report.md (DONE_WITH_CONCERNS: not end-to-end; 2702 tests green)
- [x] 2c-2. /hvn-fix (remote split: server dispatch + client request send) — done 00:39 — reports/fix-260722-0021-remote-split-end-to-end-report.md (2706 tests green; materialization still open)
- [x] 2c-3. /hvn-fix (remote split: local pane materialization) — done 00:54 — reports/fix-260722-0040-remote-split-materialization-report.md (2707 tests green, chain complete)
- [x] 2a. /hvn-fix (AgentStatus relay wiring) — done 00:07 — reports/fix-260722-0001-agent-status-relay-wiring-report.md (tests green, routed on raw terminal_id)
- [x] 3. /hvn-code-review — done 00:58 — reports/code-review-260722-0055-federation-three-fix-diff-report.md (REQUEST_CHANGES: 1 critical, 2 major)
- [x] 3b. /hvn-fix (remediate review findings) — done 01:06 — reports/fix-260722-0100-review-findings-remediation-report.md (critical + major#2 fixed, major#3 flagged product decision; 2708 tests green)
- [x] 4. /hvn:ship-gate — done 01:06 — --auto adjudicated PASS (attestation skipped + logged in auto-decisions report); uncommitted diff left for user commit
