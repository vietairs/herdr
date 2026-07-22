# Pipeline Progress

- [x] 1. /hvn-debug (remote agents missing from sidebar, live evidence) — done 12:40 — reports/debug-260722-1240-remote-agents-sidebar-root-cause-report.md (cause: agent IDENTITY never set for remote panes — pid-gated probe + no identity field on wire) — cost: 1 agent/6:17, tokens est. 91k
- [x] 2. /hvn-fix — done 13:03 — reports/fix-260722-remote-agent-identity-relay-report.md (identity on wire + pid-gate bypass; 2715/2715 green; protocol stays v3) — cost: 1 agent/13:44, tokens est. 149k
- [ ] 3. /hvn-code-review — gated
- [ ] 4. /hvn:ship-gate + redeploy — gated
