---
title: "herdr Remote Workspace Federation (v2 — clean federation protocol)"
description: "Mount a headless remote herdr as a NEW local workspace via a purpose-built federation protocol over SSH — local+remote coexist in one sidebar, native agent status + cold-resume."
status: pending
priority: P1
effort: ~30-42d (9 phases; P3/P4/P5 are the heavy ones)
branch: main
tags: [federation, remote, ssh, protocol, terminal, tdd, high-risk, v2]
created: 2026-07-13
---

# herdr Remote Workspace Federation — v2 Plan

**Supersedes v1** (`plan.md` @ 12:41 + phase-01..08). v1 was found UNBUILDABLE by red-team (F1/F2 blockers)
and codex review: it treated federation as a pure JSON-API-client problem while the per-pane byte transport it
depended on actually rides a *rendered-ANSI-diff, single-writer* attach stream — not raw PTY bytes — and it left
the entire remote-side surface unowned.

**Decisive new architecture constraint (resolves the biggest objection):** OUR forked herdr is installed on
BOTH ends. The remote runs our herdr HEADLESS as a dedicated **federation server**. Remote-side changes are
therefore IN SCOPE and expected. We design a **clean new federation protocol** carried over the existing SSH
bridge, rather than retrofitting the rendered attach stream.

Goal (unchanged): `herdr --remote ssh user@ip` connects + starts the remote (headless) herdr and mounts its
session as a NEW WORKSPACE inside the LOCAL herdr server — local + remote workspaces in one sidebar, native
agent status + cold-resume.

## Why v1 failed (verified against source)

| v1 assumption | Source reality (verified) | v2 resolution |
|---|---|---|
| Per-pane bytes ride `AttachTerminal` raw PTY stream | Attach = `TerminalAnsi{ blit_encoder, seq }` rendered **diffs** of the composited grid; single-writer (`terminal_attach_owners`, `headless.rs:2348`); `render_stream.rs:16` | New protocol carries **raw** PTY bytes tapped at `process_pty_bytes` source on the remote (P3) |
| EventHub becomes multi-source | Single global `next_sequence`, 512 cap, per-sub serial cursors (`event_hub.rs:8,13`; `subscriptions.rs:54,317`) | EventHub stays **single-source**; per-mount **replica reducer** (P4) translates remote stream → local state + local events |
| SSH bridge can carry both JSON API + binary attach | Bridge proxies only the **client socket** binary wire (`unix.rs:194-217`); JSON API is a separate socket | ONE new multiplexed federation protocol over the bridge to a new remote subcommand (P3) |
| SessionSnapshot identifies the remote instance | Only `version`+`protocol`, no server-instance epoch (`session.rs:9`) | Handshake carries `server_instance_id`; atomic `{server_instance_id, snapshot, cursor}`; **mount-generation fencing** |
| TerminalSource seam = write/resize/shutdown | Lifecycle (local child + `master_fd`) baked into `spawn_*` + `on_read` closure at construction (`pane.rs:1722,1880,1798-1924`) | Seam includes a **construction-level transport factory** (LocalChild vs Remote lifecycle policy) |

## The new federation protocol (summary)

A dedicated, versioned, self-framed protocol (shared Rust types compiled into BOTH ends), multiplexed over ONE
`SshStdioBridge` connection to a new remote subcommand `herdr federation-serve`:

1. **Handshake** — `capability` set + `federation_protocol_version` + `server_instance_id` (fresh per remote
   boot). Version/capability mismatch ⇒ clean reject (local falls back to legacy attach).
2. **Atomic mount** — one `{server_instance_id, snapshot, cursor}` message: the full `SessionSnapshot` plus the
   event cursor it is consistent with (no snapshot/stream gap).
3. **Ordered resumable event stream** — one channel, per-source `source_seq`, explicit `gap`/`reset` markers;
   consumed by the replica reducer which owns its own cursor (never touches EventHub internals).
4. **Raw per-terminal byte channels** — multiplexed, tagged `{terminal_id, mount_generation}`: `output` (remote→
   local raw PTY bytes), `input`/`resize`/`close` (local→remote). Bounded, backpressured. Scrollback replayed on
   channel open.
5. **Per-pane agent-status stream** — relays the remote's existing `AgentInfo`/`pane.agent_status_changed`.
6. **Bounded framing on every channel** — one shared framed codec with per-channel caps; no hand-rolled decode.

**Mount-generation fencing:** local refs = `HostKey + server_instance_id + local mount_generation`. Every
inbound message is validated against the live generation; stale traffic after a remote restart is rejected
(resolves red-team F4 / codex #4 restart-staleness class).

## Ordered phases (1-line goal each)

| # | Phase | Goal | Root? |
|---|---|---|---|
| P1 | federation-protocol + id-fencing | Shared Rust protocol types (both ends): handshake, atomic mount, ordered event stream, raw terminal channels, agent stream, framed codec + caps; `HostKey`/`server_instance_id`/`mount_generation` newtypes + map/fence primitives. Pure, no I/O. | **ROOT A** |
| P2 | TerminalSource transport seam | Refactor: extract write/resize/shutdown behind `TerminalSource` + a construction-level transport factory (LocalChild vs Remote lifecycle). Local PTY path byte-for-byte unchanged. | **ROOT B** |
| P3 | remote federation server | New headless `herdr federation-serve` subcommand: raw PTY byte tap at `process_pty_bytes` source, atomic snapshot+cursor emitter, ordered event stream, agent stream, handshake w/ `server_instance_id`. Ships the in-process loopback harness. | no |
| P4 | local federation client + replica reducer | `FederationClient` over `SshStdioBridge` + per-mount **replica reducer** → local workspace/pane metadata + locally-emitted events; owns cursor/gap/reset; non-interactive mount. EventHub untouched. | no |
| P5 | remote-backed panes | `RemoteTerminalSource` (P2) fed by the raw output channel → `process_pty_bytes`; input/resize/close out as protocol msgs; scrollback-on-hydrate; clipboard/OSC channel (origin-tagged); never kills remote / never emits local `PaneDied` on drop; per-pane isolation. | no |
| P6 | agent-status relay | Feed relayed remote agent status into namespaced panes; suppress the local process-table probe for remote panes; keep source-agnostic screen-text detection; stale marker. | no |
| P7 | security hardening | Enforce bounded framing on the whole ingestion path, sanitize remote strings, propagate origin tags, quotas — lands BEFORE P8 flips the default. | no |
| P8 | CLI federated workspace | Feature-flagged `--remote` federated path w/ capability negotiation + legacy-attach fallback; per-host sidebar group + unspoofable origin badge; namespaced focus barrier; single-mount + double-attach guards. | no |
| P9 | lifecycle + resume | Disconnect/reconnect FSM, mount-generation re-fencing on remote restart, cold-resume for federated workspaces, warm-handoff exclusion, shutdown disconnects-never-kills. | no |

## Dependency graph

```
P1 (protocol + id-fencing) ──┬─> P3 (remote server) ──┬─> P4 (client + reducer) ──┬─> P5 (remote panes) ─┬─> P6 (agent status)
                             │                        │                          │                     │
P2 (TerminalSource seam) ────┼────────────────────────┼──────────────────────────┘                     ├─> P7 (security)
                             │                        │                                                 │
                             └────────────────────────┴─────────────────────────────────────> P8 (CLI) <┘   (P7 lands before P8 flip)
                                                                                                 │
                                                                                                 └─> P9 (lifecycle)
```

- **P1 + P2 are the two roots** — independent, parallelizable, disjoint file ownership:
  P1 = new `src/remote/federation/protocol/*` + `id.rs` (pure lib, zero existing files touched);
  P2 = `src/terminal/`, `src/pane.rs`, `src/pty/` (refactor).
- P3 needs P1. P4 needs P1+P3. P5 needs P2+P4. P6 needs P4+P5. P7 needs P4+P5 and MUST land before P8's
  default-flip. P8 needs P5+P6+P7. P9 needs P8.
- **P3 + P4 are the tightest-coupled pair** (loopback harness in P3 is P4's test transport); they can be built
  by one owner or two coordinating owners, but not two independent teams.

## Findings-resolution table

Red-team F1-F8 (`reports/redteam-federation-plan-review.md`) + codex Critical 1-3 / High 4-8 → resolving phase + how.

| Finding | Severity | Resolved in | How |
|---|---|---|---|
| **RT-F1** namespacing misses binary-wire `AttachTerminal` path | BLOCKER | P1 + P5 | We do NOT use `AttachTerminal`. Raw byte channels are a new protocol surface tagged `{terminal_id, mount_generation}`; ALL ids namespaced+fenced in P1, routed in P5. No un-wrapped second id space exists. |
| **RT-F2** P4 needs a 2nd transport P3 never builds | BLOCKER | P1 + P3 + P4 | ONE multiplexed federation stream over the SSH bridge carries every channel (mount, events, raw bytes, agent). P3 owns the remote end, P4 the local end. No two-socket ambiguity. |
| **RT-F3** binary wire v16 skew never negotiated | MAJOR | P1 + P4 | Federation has its OWN `federation_protocol_version` + capability handshake; independent of `wire.rs` v16. Skew ⇒ clean reject + fallback (test in P1/P4). |
| **RT-F4** "federation capability flag" has no producer | MAJOR | P3 (producer) + P8 (fallback) | P3's `federation-serve` IS the producer; it advertises capability + version in the handshake. P8 negotiates and falls back to legacy attach when absent. |
| **RT-F5** EventHub multi-source spike on critical path, unbaselined | MAJOR | P1 + P4 (decision) | **Decision made, spike eliminated:** no multi-source EventHub. Per-mount replica reducer owns cursor/gap/reset over `source_seq`; local EventHub keeps today's single-source semantics. Effort re-baselined per phase (below). |
| **RT-F6** scrollback / history-on-hydrate missing | MAJOR | P3 + P5 | Raw output channel replays bounded scrollback on channel open (P3 emits, P5 hydrates), capped by `advanced.scrollback_limit_bytes` + P7 frame caps. Explicit test. |
| **RT-F7** clipboard/image-paste bridging dropped | MAJOR | P5 + P7 | Clipboard/OSC carried on an origin-tagged channel (P5), routed through local policy with origin propagation (P7). Image-paste in-scope; documented in P8. Not a silent regression. |
| **RT-F8** P8↔P6 badge ordering knot (double ownership) | MAJOR | P7 + P8 | Badge owned entirely by P8. P7 asserts only sanitize + frame caps (P8-independent). Ordering: P7 hardening → P8 default-flip. |
| **CX-1** EventHub no cursor/epoch + 512 cap + serial per-sub cursors | CRITICAL | P1 + P4 | Reducer owns its own resumable cursor + `gap`/`reset` over protocol `source_seq`; never depends on EventHub's 512-cap ring or per-sub serial cursors. Local EventHub unchanged. |
| **CX-2** SessionSnapshot has no server-instance epoch | CRITICAL | P1 + P4 + P9 | `server_instance_id` in handshake; atomic `{server_instance_id, snapshot, cursor}`; `mount_generation` fencing rejects stale post-restart traffic (P9 re-fences). |
| **CX-3** SSH bridge only proxies client socket, not the JSON API | CRITICAL | P3 + P4 | New multiplexed federation protocol over a NEW remote subcommand reached via `SshStdioBridge`; does not reuse the client-socket attach path at all. |
| **CX-4** direct attach = rendered ANSI diffs, fixed-mode single-writer | (load-bearing) | P3 + P5 | Federation taps **raw** PTY bytes at the `process_pty_bytes` source (P3), a distinct consumer (not the single attach owner); local emulator re-parses raw bytes (P5). No double-emulation. |
| **CX-5** TerminalSource seam must cover construction/teardown lifecycle | (load-bearing) | P2 + P5 | Seam includes a construction-level transport factory: Remote lifecycle policy never spawns/kills a local child, never emits local `PaneDied` on drop. |
| RT-F9 P1 file-ownership self-contradiction / "root blocker" overstated | MINOR | P1 | v2 P1 owns real protocol+id types consumed by P3/P4/P5; the two roots (P1 protocol, P2 seam) are genuinely independent. |
| RT-F10 resize `terminal_responses` round-trip unresolved/untested | MINOR | P5 | Decision pinned + tested in P5 (default: remote-host-local; propagate visual resize only). |
| RT-F11 double-attach guard has no detection mechanism | MINOR | P8 | Detection keyed on `HostKey`; guard lives in the local mount registry (P8). |

## Scope decisions (explicit)

- **Trust model — TRUSTED-REMOTE (we own both ends), defense-in-depth still required.** We control and install
  both binaries; SSH is the boundary (no app-layer auth — `0o600` socket only, verified `api/server.rs`). A
  legitimately-authed remote can still be independently compromised and is a new trust boundary the app never
  had (all rendered state was previously self-generated). So P7 stays in scope, NOT downgraded: bounded framing
  on ingestion, remote-string sanitization, origin propagation, quotas. We do NOT build an app-layer
  auth/capability model (YAGNI — SSH is the boundary).
- **DEFERRED / EXCLUDED:**
  - **Multi-remote (>1 host): DEFERRED.** Data model is multi-remote-*ready* (`HostKey` + `server_instance_id` +
    `mount_generation` per mount), but v1 **enforces a single mount** (P8 guard).
  - **Warm handoff for federated panes: PERMANENTLY EXCLUDED.** `SCM_RIGHTS` fd-passing
    (`server/handoff.rs:377-450`) cannot cross a network — no local fd exists. Federated panes are dropped from
    the warm-handoff set and reconnected via the cold path (P9).
  - **kitty-graphics over federation: DEFERRED.** Federated panes are text-grid + agent-status only in v1.
  - **Local-echo prediction: EXCLUDED** (dumb relay like SSH; faked echo desyncs the grid).

## Global acceptance criteria

1. Existing `herdr --remote ssh user@ip` full-screen behavior is **byte-for-byte unchanged** until P8, and even
   then only behind an opt-in flag/config. Existing remote/client suites stay green every phase.
2. The federation protocol round-trips through an **in-process loopback server** (P3), so P4-P9 are fully
   testable without a real remote.
3. Two servers' `w1` never collide in one `AppState`; input typed in a remote pane never reaches a local pane
   and vice-versa; stale traffic after a remote restart is rejected by `mount_generation` — all proven by test.
4. A network blip does NOT require a full remount; remote crash shows an explicit **disconnected** sidebar
   state, never a silently-frozen "fine-looking" pane.
   **P9.2b PHASE EXCEPTION (260714):** the initial P9.2b live-wiring milestone (option-b own-in-proc
   federated session, D2) intentionally EXITs to shell on post-start transport failure instead of showing
   a disconnected state / avoiding remount. Reconnect + disconnected-state is deferred to and becomes FINAL
   acceptance in P9.3. AC4 is thus satisfied at P9.3, not at P9.2b.
5. Local restart re-establishes federated workspaces (cold-resume) or shows disconnected; warm handoff never
   panics on a federated pane.
6. Untrusted remote strings cannot inject ANSI/OSC locally; every ingestion channel enforces bounded framing
   with per-channel caps (no bypass) — proven by test.
7. Local shutdown **disconnects**, never kills, the remote federation server.
8. A remote-backed pane dropping never spawns/kills a local child and never emits a local `PaneDied`.
9. Every phase is independently reviewable with clean file ownership; TDD — tests land before/with impl.

## Risk / rollback summary

- Every phase is additive and dormant behind the absence of a live mount (P1-P7) or behind the feature flag
  (P8). **Rollback = revert the phase commit**, no data migration. P8 is the only behavior-flip
  (`HERDR_REMOTE_FEDERATION` / config) — rollback = flip flag off; classic `--remote` never modified breakingly.
- P2 is the highest-blast-radius refactor (15 `spawn_*` sites + `PaneRuntimeIo` + 4 `#[cfg(unix)]` handoff
  methods): mitigated by "no behavior change + full existing suite green" gate before any remote code exists.
- P3 remote-side changes ship behind the new `federation-serve` subcommand — an old remote binary simply lacks
  the capability and the local side falls back to legacy attach (no forced remote upgrade to keep classic
  `--remote` working).
- Snapshot schema change (P9) is additive `#[serde(default)]` (`WorkspaceSnapshot.remote_origin`) → old
  snapshots load unchanged; rollback-safe.

## Top 5 risks (mitigations in phase files)

1. **Raw-byte tap correctness on the remote** (P3) — the tap must deliver the exact bytes `process_pty_bytes`
   consumes, not rendered frames. Mitigation: tap at the `on_read` source (`pane.rs:1722,1880`), loopback test
   asserts local grid == remote grid for the same byte stream.
2. **Replica reducer ordering/gap handling** (P4) — a dropped/reordered event must not silently corrupt mirrored
   state. Mitigation: explicit `source_seq` + `gap`/`reset` protocol markers; reducer re-syncs via a fresh
   atomic snapshot on gap; property tests.
3. **Mount-generation fencing gaps** (P1/P4/P9) — stale post-restart traffic misrouted into a live pane.
   Mitigation: validate generation on EVERY inbound message; fence test corpus.
4. **P2 refactor blast radius** (15 sites + handoff). Mitigation: pure refactor, existing suite is the oracle.
5. **New adversarial trust boundary** (P7) — ANSI/OSC injection + memory exhaustion via crafted remote data.
   Mitigation: sanitize-on-ingest at one choke point + shared framed codec caps; must land before P8 flip.

## Re-baselined effort (per phase; multi-source spike eliminated)

| Phase | Effort | Notes |
|---|---|---|
| P1 | 3-4d | pure types + framed codec + id/fence primitives + tests |
| P2 | 3-4d | refactor across 15 sites + transport factory |
| P3 | 4-6d | new subcommand, raw tap, snapshot/cursor/event/agent emitters, loopback harness |
| P4 | 4-6d | client lifecycle + replica reducer + cursor/gap/reset + non-interactive mount |
| P5 | 4-6d | RemoteTerminalSource + raw channel + scrollback + clipboard + isolation |
| P6 | 2-3d | agent-status relay + probe suppression |
| P7 | 3-4d | framing caps + sanitize + origin propagation + quotas |
| P8 | 3-4d | flag + negotiation/fallback + sidebar badge + focus barrier + guards |
| P9 | 4-5d | reconnect FSM + re-fencing + cold-resume + handoff exclusion + shutdown |
| **Total** | **~30-42d** | firm-er than v1's 24-32d: spike removed, remote-side scoped |

## Phase files
- [phase-01-federation-protocol-and-id-fencing.md](phase-01-federation-protocol-and-id-fencing.md) — ROOT A
- [phase-02-terminalsource-transport-seam.md](phase-02-terminalsource-transport-seam.md) — ROOT B
- [phase-03-remote-federation-server.md](phase-03-remote-federation-server.md)
- [phase-04-local-federation-client-replica-reducer.md](phase-04-local-federation-client-replica-reducer.md)
- [phase-05-remote-backed-panes.md](phase-05-remote-backed-panes.md)
- [phase-06-agent-status-relay.md](phase-06-agent-status-relay.md)
- [phase-07-security-hardening.md](phase-07-security-hardening.md)
- [phase-08-cli-federated-workspace.md](phase-08-cli-federated-workspace.md)
- [phase-09-lifecycle-resume.md](phase-09-lifecycle-resume.md)

## Unresolved questions
- Exact remote tap point: intercept at the `on_read` closure vs. a dedicated broadcast tee inside `PtyIoActor` —
  decide in P3 (must not perturb the existing local render path).
- Does the raw output channel need the remote's own scrollback ring, or is `handoff_history_ansi()`
  (`headless.rs:980`) reusable as the replay source? — resolve in P3.
- Single SSH tunnel for all channels vs. one control tunnel + on-demand channels — resolve in P1 framing design
  (default: single multiplexed tunnel, `ManagedSshOptions.control_path` reuse).
