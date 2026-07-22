# Red-Team Review — Tier-2 Remote Workspace Federation Plan

Adversarial review of `plan.md` + `phase-01..08`, cross-checked against `reports/` evidence.
Read-only. Findings ranked most-severe first. Each cites phase / plan section / source file:line.

TL;DR: the phase decomposition is genuinely good and the risk register is honest, but the plan
has a **transport/protocol-layer blind spot**: it treats federation as a pure JSON-API-client
problem, while the per-pane byte relay that P4/P5 actually depend on rides the *separate binary
wire protocol* (`protocol/wire.rs`, v16, `AttachTerminal`). Three of the top findings all stem
from that one conflation. Plus a real feature regression (clipboard) and an ordering knot (P8↔P6).

---

## F1 — BLOCKER — id-namespacing choke point misses the binary-wire `AttachTerminal` path P4 relies on
**Where:** P1 (phase-01 Requirements 4 + Files) vs P4 (phase-04 R1). Plan §"Verified id-origin map".

P1 scopes namespacing to the **JSON API** id space: workspace/tab/pane/`TerminalId` (files:
`src/api/schema/*`, `src/workspace.rs`, new `id_map.rs`). But the actual per-pane byte transport
in P4 is the **binary wire protocol** — `ClientConnectionMode::TerminalAttach{terminal_id}` via
`ClientMessage::AttachTerminal`/`ObserveTerminal`/`ControlTerminal` (`src/server/clients.rs:9`,
`src/protocol/wire.rs`). The wire scout §5 explicitly lists id-remapping as needing to cover
"`wire.rs::ClientMessage::AttachTerminal`/`ObserveTerminal`/`ControlTerminal` **target strings**,"
which P1's Files section does not include (`src/protocol/wire.rs` is never touched by P1 or P3).

**Why it bites:** the single choke point P1 promises (phase-01 Risks: "P3 centralizes all
ingress/egress through one proxy translate layer") is only a choke point for the JSON socket. The
attach target string `terminal_id` that routes a keystroke to a specific remote PTY flows on a
*different* socket that no `map_in`/`map_out` covers → exactly the input-misroute class (S8.1) P1
exists to prevent, re-introduced on the transport P4 uses to send input.
**Fix:** add `AttachTerminal`/`ObserveTerminal`/`ControlTerminal` target-string translation to P1's
surface (or a dedicated wire-proxy sub-item in P4) with its own round-trip test; update phase-01
Files to include `src/protocol/wire.rs` target structs.

## F2 — BLOCKER/MAJOR — P4 needs a second transport (binary attach socket) that P3 never builds
**Where:** P3 (phase-03 R2, "owns the SSH-tunneled connection") vs P4 (phase-04 R7, "reuse one SSH
connection per host, logical channels per pane … over the P3 connection").

The remote exposes two socket modes (wire scout §3): **App-mode** = line-delimited JSON
`session.snapshot`/`events.subscribe` (what P3 builds); **TerminalAttach/Observe** = 2 MB-framed
raw binary PTY stream (what P4 needs for pane bytes). These are different framings on different
connections. P4's "open per-pane logical channels over the P3 connection" silently assumes the
JSON API-client connection can also carry binary attach frames — it can't.

**Why it bites:** P3 is sized/estimated as "connect + snapshot + subscribe." The real work is *two*
concurrent client roles (one App-JSON, N binary-attach) multiplexed over one SSH tunnel with
`control_path`. That second transport (a second forwarded socket / SSH channel + a binary-attach
client) is unowned by any phase. P4's estimate absorbs a transport subsystem it didn't budget.
**Fix:** make P3 (or a new P3.5) explicitly own *both* client roles and the tunnel multiplexing;
state the binary-attach socket as a first-class deliverable, not "a logical channel."

## F3 — MAJOR — binary wire protocol v16 skew is never negotiated
**Where:** P3 R4 (capability negotiation) reads `Pong.protocol`/`capabilities` only. Plan title +
Unresolved-Q mention "protocol v16" but no phase acts on it.

There are **two independent version counters**: the JSON API `protocol: u32` (checked in
`ensure_remote_server_running`, `src/remote/unix.rs:221-241`) and the binary
`PROTOCOL_VERSION: u32 = 16` (`src/protocol/wire.rs:16`), "bumped on wire-format changes"
independently (wire scout §1). P3 negotiates only the former; the per-pane relay (F1/F2) rides the
latter. A host on a different `wire.rs` version passes P3's Pong check and then corrupts the attach
byte stream (S3.1 is rated Blocker for exactly "silent skew = corrupted session state").
**Fix:** negotiate BOTH counters before mount; add a wire-version field to the federation capability
check and a skew-rejection test.

## F4 — MAJOR — the "federation capability flag" P3 checks for has no producer
**Where:** P3 R4 + test 4 ("mock a `Pong` without federation capability"). All phase Files are
local (`src/remote/federation/*`, `src/api/*`, `src/pane.rs`, …).

App-mode `session.snapshot`/`events.subscribe` already exist (wire scout §3), so nothing new is
needed remote-side to *consume* them — meaning the advertised "federation capability flag" either
(a) is really just "protocol/wire version ≥ X," in which case say so and test against version, or
(b) is genuine remote-side production work (adding a `federation` capability to the remote's `Pong`)
that **no phase in this plan ships** — the entire plan is local-side. Test 4 currently asserts
behavior against a flag that nothing produces (vaporware TDD).
**Fix:** decide (a) vs (b); if (a), rewrite R4/test4 as a version gate; if (b), add a remote-side
phase (and note the remote must be upgraded — an install/version prerequisite, cf. `prepare_remote_herdr`).

## F5 — MAJOR — EventHub multi-source spike is buried on the critical path; timeline is unbaselined
**Where:** P3 R1 + impl step 1 ("Run the EventHub design spike"). Plan effort line "~24-32d".

The scenario doc's own feasibility note: the multi-source EventHub is "the single largest undersized
line item across all 4 scout reports … Recommend a dedicated design spike **before committing to a
Tier-2 timeline**," and it feeds three separate blockers (S2.3/S6.2/S14.3). The plan instead nests
that spike as step 1 *inside* P3 — a phase already on the critical path (P1→P3→P6) with a committed
day estimate — and the spike may force real surgery in `src/api/subscriptions.rs` (825 lines,
phase-03 Unresolved-Q). Estimating P3–P8 (and the 24-32d headline) before the spike resolves is
committing to a number the plan itself says is unknowable.
**Fix:** promote the spike to a gating P0 (or explicit pre-phase) whose output re-baselines the
P3–P8 estimate; do not present 24-32d as firm until it lands.

## F6 — MAJOR — remote pane scrollback / history-on-hydrate is missing from P4
**Where:** P4 R2/R7 + tests (lazy-hydrate wires `on_read` for *live* bytes only). No requirement or
test for initial history.

herdr ships "pane history replay" as a first-class feature (docs/session-state); local restore
seeds `initial_history_ansi` (pty scout §3). P4 lazy-hydrates a ghostty grid on focus and pipes
only *new* relayed bytes into it — so a freshly focused remote pane (e.g. a long-running remote
agent) shows a **blank grid until the next output**, and scrolling up shows nothing (the local grid
holds only post-hydrate content; the remote holds the real scrollback). Whether `AttachTerminal`
auto-replays scrollback on attach is unstated and untested.
**Why it bites:** "native-ish" is the headline value prop; an empty pane on focus is the opposite.
**Fix:** add a P4 requirement + test for initial-history hydrate (via AttachTerminal replay if it
exists, else a `session`/pane-history fetch), bounded by `advanced.scrollback_limit_bytes` and the
P8 frame caps.

## F7 — MAJOR — clipboard bridging is dropped: a regression vs the command it extends
**Where:** scope §(a) defers OSC52 "to a follow-up"; P8 R4 only gates remote→local writes to local
policy; S13.3 (local→remote paste) and image paste unaddressed.

Classic `herdr --remote` explicitly "bridges your local clipboard (**including image paste**)"
(herdr.dev/docs/remote). Federated panes — sold as the *upgrade* path — silently lose clipboard
bridging. That is a feature downgrade users will hit immediately, not an exotic edge case.
**Fix:** either plumb paste/clipboard for federated panes in P4/P6, or make the regression an
explicit, user-facing documented limitation in P6 scope (not buried as an OSC52 security deferral).

## F8 — MAJOR — P8↔P6 ordering knot on the remote-origin badge (double ownership + circular test)
**Where:** P8 R3 + test 3 ("badge/prefix/color **from P6**"; "P6 integration") vs P8 header
("must land **before P6** flips the default") vs P6 R3 (creates the badge).

P8's origin-unspoofable requirement is defined in terms of a P6 deliverable, tested as "P6
integration," yet P8 is sequenced before P6's flip. The badge is owned by both phases. As written,
P8 cannot go green on test 3 without P6's badge existing.
**Fix:** badge ownership entirely in P6; P8 asserts only string-sanitization (R1) + frame caps (R2)
— both P6-independent. Sequence: P6 badge lands → P8 hardening → P6 default-flip. State that the
"land P8 before P6" gate refers to sanitization/caps, not the badge.

## F9 — MINOR — P1 file-ownership self-contradiction; "root blocker" is overstated
**Where:** plan.md:52 ("P1 = `src/api/schema/*`, `src/workspace.rs`, new id_map") vs phase-01 Files
("Modify **none** of the id generators … Later phases (P3) call into this module") + phase-01 R4.

As phase-01 is actually written, P1 touches *zero* existing files — it is a pure library. That
makes "P1 ∥ P2 disjoint ownership" trivially true, but also means P1 derisks **nothing** at the
integration layer: the real id-wiring risk (every JSON choke point + the F1 wire path) lives in
P3/P4. Calling P1 "the root blocker for the whole feature" (plan §Dependency graph) oversells a
lib with no call sites.
**Fix:** reconcile plan.md:52 with phase-01 Files; relabel the true root-integration risk as P3
(JSON) + the F1 wire-path item.

## F10 — MINOR — resize `terminal_responses` round-trip left unresolved, untested
**Where:** P4 R8 ("default to remote-host-local handling … only propagate visual resize") + arch-probe
§4 open Q. No P4 test covers it.

The local ghostty grid can emit terminal query replies (DA/DSR) on resize; `resize(...,
terminal_responses: Vec<Bytes>)` carries them. If those must reach the *remote* PTY and don't, or
if the remote answers queries the local emulator issued, the grid can desync. Deferring the decision
is fine; shipping P4 with no test asserting the chosen behavior is not.
**Fix:** pin the decision in the P4 design and add a resize/query-reply test.

## F11 — MINOR — S8.2 double-attach guard (P6 R6) has no detection mechanism
**Where:** P6 R6 + test 6 ("classic `--remote` to an already-federated host → warn/block").

The remote server accepts multiple App-mode clients (wire scout §3, S8.2), so nothing server-side
blocks a classic attach onto a federation-mounted host. The local server has no host-identity
registry to detect "this host is already mounted." P6 asserts the guard without specifying how the
local side recognizes the collision.
**Fix:** specify the detection key (host-key from P1 `HostKey`) and where the guard lives; or
downgrade to "documented, not enforced" for v1.

---

## Cross-cutting observations
- **TDD realism is mostly OK** — mock-transport injection (P3/P4/P5) is legitimate. The exceptions
  are F4 (test asserts against a non-existent capability producer) and F8 (P8 test 3 needs P6).
- **P2 "pure refactor" is the plan's soundest claim** — arch-probe rates it MEDIUM, the handle API
  is already `Bytes`/u16/u32-shaped with no `RawFd` leak (arch-probe §4), and handoff ops are
  correctly kept off the trait. Residual risk (leaky abstraction: `PaneRuntimeIo::Remote` still gets
  enum-matched at the 4 `#[cfg(unix)]` handoff sites) is handled in P4/P7, not P2. No change needed.
- **Scope decisions are largely defensible.** Semi-trusted-remote + SSH-as-boundary is sound (no app
  auth exists, `0o600` socket). Permanent warm-handoff exclusion is architecturally forced
  (`SCM_RIGHTS` can't cross a network) and correctly surfaced — users lose *warm* self-update
  continuity for remote panes, mitigated by the P7 cold-resume path; acceptable if documented. The
  one scope cut that will surprise users is **clipboard (F7)**, which is framed as a security
  deferral rather than the feature regression it is.

## Suggested plan amendments (priority order)
1. F1+F2+F3: add an explicit **binary-wire federation transport** work item (own the `AttachTerminal`
   client role, its id-remap, and wire-v16 skew) — split from P3's JSON client. This is the biggest
   structural miss.
2. F5: pull the EventHub spike out as a gating pre-phase; re-baseline 24-32d after it.
3. F4: resolve capability-flag as version-gate vs remote-side work; fix P3 test 4.
4. F6: add history-on-hydrate to P4. F7: decide clipboard (plumb or document-as-regression).
5. F8: move badge ownership to P6; P8 gate = sanitize+caps only.

## Unresolved questions for the author
- Does `AttachTerminal` replay pane scrollback on attach, or only stream live bytes? (Decides F6 size.)
- Is the "federation capability" a new remote-side field, or just `wire`/`protocol` version ≥ N? (F4.)
- Is the local↔remote federation link one SSH tunnel carrying both JSON-API and binary-attach
  multiplexed, or two? Who owns that multiplexing? (F2.)

Status: DONE
