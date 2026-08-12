# Phase 07 — Security hardening (untrusted remote data)

**Goal:** treat the federation ingestion path as a new adversarial trust boundary. Enforce bounded framing on
every channel, sanitize untrusted remote strings before they reach the local terminal, propagate origin tags,
and bound resource usage. Asserts ONLY things it fully owns — **badge ownership is P8** (RT-F8). **Must land
BEFORE P8 flips the default** (codex #7 / RT-F8 ordering). **Depends on:** P4, P5. **Blocks:** P8 default-flip.
**Shippable:** yes.

## Context
- Trust model (see `plan.md`): TRUSTED-REMOTE (we own both ends) BUT defense-in-depth required — a legitimately-
  authed host can be independently compromised; all previously-rendered state was self-generated.
- Verified: no app-layer auth anywhere — trust is `0o600` socket + SSH only (`src/api/server.rs`, wire scout §1).
- Verified: the P1 codec already carries per-channel caps (phase-01 R2). This phase confirms EVERY ingestion
  channel routes through it — no hand-rolled decode bypass.
- Scenario S11.1 (ANSI/OSC injection via crafted `custom_name`/`cwd`), S11.2 (oversized frame → OOM), S11.3
  (OSC52 clipboard), S11.4 (origin spoofing). RT-F7 (clipboard), RT-F8 (ordering/badge boundary).

## Requirements
1. **Sanitize remote strings (S11.1 Blocker):** every remote-sourced string rendered locally (`custom_name`,
   `identity_cwd`, `agent_name`, labels, notification text) is stripped/neutralized of terminal control/ANSI/OSC
   before it reaches the ratatui buffer. Applied at the P4 ingest choke point (single place) so nothing
   downstream must remember to escape. A remote workspace name cannot move the cursor, hide text, or fire OSC52.
   (Note: raw PTY bytes into a pane grid are legitimately terminal content — those go to the ghostty emulator,
   which is the correct sandbox; sanitization targets *chrome* strings, not pane bytes.)
2. **Frame-cap enforcement (S11.2 Blocker):** confirm every federation channel decodes through the P1 codec with
   per-channel `max_len` — no bypass/hand-rolled decode. A remote is not more-trusted than a normal client.
3. **Origin propagation (RT-F7 / S11.4 substrate):** the `origin_tag` on clipboard/OSC and the `HostKey` origin
   on every mirrored entity are preserved end-to-end so P8 can render an unspoofable badge and gate clipboard.
   This phase guarantees the tag EXISTS and is correct; P8 renders it.
4. **Clipboard OSC52 policy parity (S11.3):** remote-origin clipboard writes get NO more trust than local ones —
   gated through the same local OSC52 policy. If local auto-applies, document remote-origin clipboard as a known
   consideration (friction escalation deferred, flagged to user — not silently decided).
5. **Resource quotas / bounded ingestion (S2.2/S10.2):** ingestion buffers are bounded (ties to P5 isolation);
   a flooding/oversized remote cannot OOM or starve the local loop. Per-mount channel budgets.

## Files
- **Create** `src/remote/federation/sanitize.rs` — control/ANSI/OSC stripping for chrome strings (grep for an
  existing herdr escaping util first; DRY over a new impl).
- **Modify** `src/remote/federation/reducer.rs` — apply `sanitize_remote_string()` to all rendered remote chrome
  strings at the ingest choke point; assert all channels use the P1 codec.
- **Modify** `src/remote/federation/pane_source.rs` — remote-origin OSC52/clipboard gated through local policy
  with origin tag (locate the existing OSC52 handler; route through the same policy).
- **Modify** `src/remote/federation/client.rs` — per-mount channel budgets / bounded buffers.
- **Docs** — trust-model note (trusted-remote; SSH is the boundary; not a sandbox) in federation docs.

## TDD test plan (tests FIRST)
Run `cargo test -p herdr federation::sanitize federation::reducer federation::pane_source`:
1. **ANSI/OSC injection neutralized (S11.1):** a `custom_name` with `\x1b[2J`, cursor-move, OSC52, hidden-text →
   after `sanitize_remote_string` the rendered bytes contain no active control sequences; visible text preserved.
2. **Frame-cap enforced on ingestion (S11.2):** an over-cap payload on each channel is rejected by the P1 codec
   exactly as a normal client parser rejects it (same typed error; proves shared codec, no bypass, no oversize
   alloc).
3. **Origin tag preserved (S11.4 substrate):** a mirrored remote entity + a clipboard write both carry the
   correct `HostKey`/`origin_tag` end-to-end (asserted at the P8 boundary; badge rendering itself is a P8 test).
4. **OSC52 policy parity (S11.3):** a remote-origin clipboard write is gated by the same policy as a local one
   (no elevated trust).
5. **Bounded ingestion (S2.2):** oversized/flooding remote input is bounded, does not OOM the ingestion buffer;
   per-mount budget enforced.

## Implementation steps
1. Grep for an existing herdr control-sequence escaper; reuse if present (DRY), else implement `sanitize.rs`.
2. Write failing tests (1-5).
3. Apply sanitize at the ingest choke point; confirm all channels use the P1 codec; gate OSC52; add budgets.
4. Add trust-model docs. Full suite green. **Green is the gate before P8 flips the default flag.**

## Risks + rollback
- **Risk (top-5):** a missed rendered field lets ANSI/OSC through. Mitigation: sanitize at the single ingest
  boundary (not per-render-site) + injection test corpus (1). **Risk:** a hand-rolled decode bypasses caps.
  Mitigation: test 2 asserts identical rejection to the shared codec. **Rollback:** revert; but do NOT ship P8
  default-on without P7 green (sequencing gate in `plan.md`).

## File ownership
Exclusive: `src/remote/federation/sanitize.rs`. Shared (after P4/P5): `reducer.rs`, `pane_source.rs`,
`client.rs`. Does NOT own the badge (P8). Independent of P8's file set except the shared origin-tag contract.
