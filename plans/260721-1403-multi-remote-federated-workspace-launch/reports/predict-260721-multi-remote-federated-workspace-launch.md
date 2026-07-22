# Predict report — multi-remote federated workspace launch (saved verbatim from persona debate)

Proposed: `herdr --remote localhost host1 host2 --remote-workspace` → 1 local + N remote federated workspaces, one TUI, per-host sidebar groups.

## 1. Architect
- BLOCKER — coexistence reverses P9.2b. run_federated_session (session.rs:169) builds its own App/runtime standalone; main.rs:706-716 local-autodetect is mutually exclusive. Multi-remote requires solving "local+remote coexist" first. Mitigation: own phase, sequenced before multi-mount.
- HIGH — remote_mirror: Option<...> must become keyed collection (state.rs:1494, 1522-1559). Data model already multi-remote-ready → additive Option→HashMap<HostKey, RemoteMirror>.
- MED — which App owns the runtime loop. Recommend: local App is single source of truth; each remote mount is a background task pushing mirror updates into shared AppState.

## 2. Security engineer
- HIGH — focus-barrier scales to N: input routing must key on (HostKey, PaneId); confirm PaneId allocation is process-global; add 2-remote rapid-focus-switch test.
- HIGH — HostKey collision/spoofing: string-based non-canonicalized identity (localhost vs 127.0.0.1 vs hostname) can defeat double_attach_conflict. Not new risk; document as limitation, no v1 canonicalization.
- MED — origin badge integrity per group: badge/group key must be mount's HostKey, never client-suppliable string.

## 3. Reliability engineer
- BLOCKER — 25s mount timeout × N serial = up to 75s. Mitigation: concurrent dials (FuturesUnordered), 25s budget applies to the set.
- HIGH — partial mount failure must not kill siblings or local session. Per-target Result; failures → sidebar notices (reuse FederationMountFailure, unix.rs:226-247).
- HIGH — teardown isolation per mount: own ChildGuard/writer task per mount; per-mount panic caught without aborting shared App (session.rs:341-365 pattern).

## 4. UX
- HIGH — localhost semantics: explicit is_local_target check before dispatch, no SSH; document exact matched strings.
- MED — per-host failure attribution in sidebar, not one stderr lump (unix.rs:516-521 designed for one target).
- MED — double-attach guard message must name which host collided.

## 5. Maintainer
- BLOCKER — CLI contract: RemoteLaunch.target String singular (unix.rs:61-71); test extract_remote_args_rejects_duplicate_values (~2808) enforces one target; Windows parity src/remote.rs:67,78. Classic single-target path must stay byte-for-byte identical — explicit acceptance test.
- HIGH — YAGNI: recommend ship "local + 1 remote coexisting" first (Phase A), then generalize to N (Phase B). Multi-target CLI parsing can land early.
- MED — test burden: concurrent-dial, partial-failure 2-of-3, per-host teardown isolation, HostKey-collision, localhost-as-local tests; real SSH federation stays manual-smoke verified.

## Consensus
Two-phase delivery:
1. Phase A: local workspaces + ONE federated mount coexist in the same App/render loop (reverses P9.2b for N=1; background mount task feeding shared AppState).
2. Phase B: Option→HashMap<HostKey, RemoteMirror>, concurrent dial, multi-target CLI, per-host failure notices.

Ordered risks: coexistence architecture (BLOCKER) → CLI contract break (BLOCKER) → serial-dial wall-clock (BLOCKER) → partial-failure isolation (HIGH) → focus barrier at N (HIGH) → HostKey/localhost semantics (HIGH) → per-mount teardown isolation (HIGH).

Explicit answers: (a) localhost = local workspace, no SSH, explicit matcher; (b) per-target fallback, all-or-nothing rejected; (c) extend local App with multi-mount background tasks, not N separate Apps; (d) concurrent dials, 25s budget for the batch.

Status: DONE
