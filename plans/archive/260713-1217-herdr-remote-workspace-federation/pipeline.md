# Vibe Pipeline — herdr remote→local-workspace federation

Task: Re-architect herdr `--remote ssh user@ip` to connect + start the remote herdr AND mount it
as a NEW WORKSPACE inside the local device's herdr session (federation), instead of the current
full-screen attach to one remote server.

Task source: free-text (/hvn:vibe)
Repo: ~/Projects/herdr (fork of ogulcancelik/herdr, 178K LOC Rust)
Confirmed scope: DISCOVERY-ONLY (user choice at route-card confirm) — run stages 1–2, STOP at the
design gate. Build decision deferred until feasibility/invasiveness is visible.

## Route Card (R7, truncated to discovery)
Risk: HIGH — inverts --remote semantics; SSH transport + wire protocol + workspace/session model +
sidebar render; high blast radius on existing --remote users.
Familiarity: LOW — fork <1h old, 178K LOC unfamiliar Rust.
Scope: MULTI-PHASE.

Stages (this run):
  1. /hvn:blindspot --deep — map remote+workspace+protocol+session architecture; feasibility read
  2. /ck:brainstorm --html — approach options (pane-tunnel vs proxy-protocol vs true federation) → DESIGN GATE (go/no-go)

Deferred (only if user approves build at gate): scenario → plan --tdd → red-team → validate →
codex plan gate → impl-notes init → cook → impl-notes review → code-review ‖ codex diff gate →
ship-gate --hard.

Skips this run: all build/gate stages (deferred by user).

## GATE DECISION (design gate, stage 2)
User chose: **Tier-2 true federation** (native status/resume; large/rewrite of remote layer).
Resuming full R7 build tail against Tier-2 scope. No code before plan approval.

## PLAN-GATE DECISION (stage 4-6, after red-team + codex)
Adversarial review found the v1 plan unbuildable (two-protocol conflation; remote-side surface
omitted). User decision: **build the FULL two-server system** — our forked herdr is installed on
BOTH local and remote; the remote runs headless as a federation server. Remote-side changes are
IN SCOPE. This unlocks a clean NEW federation protocol (we control both ends) instead of
retrofitting the attach stream.
Action: REVISE plan (fold F1-F8 + codex 1-8 + both-ends architecture), then implement.
