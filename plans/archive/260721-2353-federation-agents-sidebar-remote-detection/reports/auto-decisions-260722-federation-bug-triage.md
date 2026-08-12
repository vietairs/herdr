# Auto-decisions — federation bug triage (--auto run)

1. What: treated 3 reported symptoms (sidebar detection, render corruption, remote split) as one pipeline in plan dir 260721-2353, fanned out parallel root-causers.
   Why: same surface (federation), shared diff/review tail. Risk: mixed-cause coupling. Alternatives: 3 plan dirs (more overhead). Reversibility: high.
2. What: serialized fixes — 2a (AgentStatus wiring) first, then 2b (render replay gate) + 2c (remote split) after, 2b/2c in parallel.
   Why: file-ownership overlap (src/pane.rs, src/remote/federation/*) forbids parallel edits to same files. Risk: wall-clock. Reversibility: high.
3. What: split bug escalates fix 2c to feature scope — new FederationMessage variant + serve-side handler + split routing; no plan stage inserted, proceeding under R10 fix stage.
   Why: --auto conservative-minimal: implement smallest working remote-split path per debug report's fix-shape. Risk: wire-protocol change (PROTOCOL_VERSION rules apply); remote VM binaries must be redeployed before remote split works live. Alternatives rejected: blocking split in remote workspaces with an error toast only (does not deliver user-visible ask); full plan stage (overhead vs proven cause + narrow shape). Reversibility: medium (protocol additive variant).
4. What: fix 2c returned DONE_WITH_CONCERNS (protocol scaffolding + typed refusal only); spawned follow-up 2c-2 to implement federation_accept.rs serve dispatch + App-level split request/response bridge.
   Why: user ask is a working remote split; refusal alone doesn't deliver. Risk: larger surface (server accept path). Alternatives rejected: stop at refusal (safe but incomplete). Reversibility: high (additive).
5. What: 2c-2 landed server dispatch + client request-send but local pane materialization still open (needs app/api/workspaces.rs, events.rs, app/mod.rs); spawned final follow-up 2c-3 owning those files, using the 3-file design recorded in plans/260713-1217.../implementation-notes.md.
   Why: split still not user-visible without materialization. Risk: touches App event core. Alternatives rejected: ship as remote_split_pending stub. Reversibility: high (additive).
6. What: code review returned REQUEST_CHANGES (1 critical: pending_remote_splits keyed by shifting ws_idx, never purged on mount end; 2 major); spawned remediation agent to fix all findings then re-run tests.
   Why: critical is a cross-workspace pane-splice hazard. Risk: low (targeted fixes). Alternatives rejected: ship with known critical. Reversibility: high.
7. What: ship-gate auto-adjudicated PASS — all proven causes fixed, review critical/major#2 remediated, major#3 (remote_split_pending response shape) left as flagged product decision, full suite 2708 green, clippy clean. Attestation question skipped per --auto; diff left UNCOMMITTED for user.
   Why: --auto mode; commit stays with user per repo rules (propose message first). Reversibility: full (nothing committed).
