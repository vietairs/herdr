# Ledger: 260910-1635-name-sync-workspace-tab-agent

- Task: resolve workspace, tab, and agent names through one precedence chain.
- Shipped: PR #26 (vietairs/herdr), merge 95e61cb2, merged 2026-09-10T15:35:57Z.
  Branch commits 8407ba62 (feat), 6ad0715b (docs), 805c7af1 (fix). Close-out 661d91c8.
- Verified at ship: fmt clean; unix and windows-msvc clippy zero; 3676 tests, 3675 pass;
  7/7 GitHub checks PASS. The one failure is a pre-existing, environment-dependent test
  (implementation-notes.md, Phase 0).
- Not done: no live two-host mount test. Federation paths are covered by unit tests and a
  capability gate whose branches were each forced to fail.
- Key deviations (implementation-notes.md):
  - PaneInfo does not gain name_source.
  - Nullable rename params (G3) and the "reset to auto" overlay action were not implemented.
  - Render-scale bench replaced by a call-graph argument; no synthetic counter test.
  - ClientShellWorkspace/Tab name_source uses serde(default) on a positional bincode field.
- Learnings:
  - A test was added, passed, and was deleted. Forcing the gate to "peer_reports_name_source
    && false" still passed, because both branches return the same label. Not distinguishable.
  - Open gap, not this branch: src/app/api/workspaces.rs:1127-1166 (federated-origin close)
    is still #[cfg(unix)]. A Windows client closing a mounted workspace may diverge.
- Archived-at-SHA: 203289d18ed9aadaf9a9673b9585b168d9df32d7
