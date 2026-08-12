# Code review: remote agent identity relay fix

Reviewed uncommitted diff (10 files) against debug report and implementation-notes.md.

## Verdict: APPROVE_WITH_NITS

## Lens 1 — wire backward-compat: PASS
`AgentStatusMessage.agent: Option<String>` uses `#[serde(default, skip_serializing_if = "Option::is_none")]` (`src/remote/federation/protocol/mod.rs:230-231`). New encoder omits the field when `None`, so old v3 decoders (field-absent struct) still parse the frame. New decoder given an old-shaped frame (no `agent` key) fills `None` via `serde(default)` — verified by the added test `codec.rs:313-336` which hand-builds a pre-fix JSON payload and asserts clean decode. No `FEDERATION_PROTOCOL_VERSION` bump needed since the handshake already gates version skew before any channel frame decodes (documented in mod.rs:227-229) — consistent with existing project precedent for additive fields.

## Lens 2 — pid-gate bypass correctness: PASS, one accepted deviation
`relayed_status_rx` is `Some` only for `spawn_remote` (`src/pane.rs:1935-1943`) and always `None` for local `LocalChild`-backed spawn (`src/pane.rs:2166-2173`, explicit comment "no relay input"). A genuinely local pane can never receive a relayed identity — no clobber path exists between local process-probe and the relay branch.

**MAJOR (documented, not blocking): stale identity never clears.** `src/pane.rs:712-722` — when `relayed_status.agent` is `None` the branch is a no-op; once `agent_presence.current_agent()` is set to `Some(Agent::Claude)` via a relayed frame, nothing on this path can move it back to `None`. If the remote agent process exits (server-side `AgentInfo.agent` presumably reverts to `None` once its own detector loses identity) and the serve loop diffs `(status, agent)` and sends `agent: None`, the client-side branch simply skips clearing — `terminal.agent_name`/`is_agent_terminal()` stays permanently `true` for what is now a plain shell pane, showing a stale "claude" sidebar entry indefinitely. This is called out explicitly in implementation-notes.md as a known, deliberately deferred gap ("clearing would need a second relay-side contract that doesn't exist yet") — acceptable as a scoped follow-up per the debug report's minimal-fix framing, but should be tracked as a real follow-up issue, not left implicit. Recommend filing a tracking issue before this ships to a release, since a stale "agent still working" sidebar entry after the remote agent exits is user-visible and could mislead someone into interacting with a dead pane.

`clear_agent_osc_state()` call on identity change (`src/pane.rs:719`) is scoped only to the identity-transition branch, reuses an existing method (`pane/terminal.rs:464`/`1029`), no new abstraction.

## Lens 3 — (status, agent) diff key: PASS
`src/server/federation_accept.rs:833-861` — diff key changed from `AgentStatus` to `(AgentStatus, Option<String>)`, `HashMap<String,(AgentStatus,Option<String>)>`. This only adds a frame when either dimension changes (not per-tick), so no added chatter versus the pre-fix status-only diff; it closes a real gap (identity resolving after the first status poll would otherwise never reach the client). `serve.rs`'s parallel `poll_agent_statuses` (`serve.rs:390-393`) hardcodes `agent: None` — confirmed this path is exercised only by `FixtureHost` (`loopback.rs:134`, test-only `FederationHost` impl); production traffic goes through `federation_accept.rs`, which is correctly wired. Not a regression, but the two near-duplicate `poll_agent_statuses` implementations (prod vs. fixture) are worth eventually collapsing — pre-existing duplication, not introduced by this diff.

## Lens 4 — project rules: PASS
No `unwrap()`/`println!` added in production code (all `unwrap()`/`expect()` in the diff are in `#[cfg(test)]` blocks). No new `cfg` leaks — `client.rs`'s `AgentStatus` import correctly narrowed to `#[cfg(test)]` since production code now only touches `RelayedAgentStatus`. `RelayedAgentStatus` is a small, justified struct (not a "manager"/generic abstraction) directly modeling the wire payload's identity+status pairing; reused across client router, pane channel, and terminal runtime type signatures — no parallel reimplementation of existing status-relay plumbing.

## Minor nits
- `src/pane.rs:1927-1932` — the "P6: dormant... Nothing in production code drives `relayed_agent_status_tx` yet" comment is now stale (this diff and the prior 6bbc829 fix both actively drive it via `drive_mount_channel` → `router.route_agent_status`). Low-risk doc drift, but will confuse the next reader tracing this path.
- `src/remote/federation/serve.rs:393` unconditional `agent: None` has no comment noting it's fixture-only/dead in prod — a future reader wiring a second real `FederationHost` impl could silently regress identity relay without realizing `agent_statuses()` needs updating too.

## Tests
Added/updated tests are real (not phantom): codec roundtrip with `agent: Some(...)`, dedicated old-frame-decodes-with-None test, reducer fixtures updated, client router test asserts both `status` and `agent` on the relayed value, pane remote-spawn test asserts `agent: Some(Agent::Claude)` in the resulting `StateChanged` event for a pane whose `child_pid` never leaves 0 — this is the one test that actually exercises the pid-gate-bypass path end-to-end.

## Unresolved Questions
- Should stale-identity clearing (relay-side "agent disappeared" signal) be scoped now or tracked as a follow-up issue? Recommend the latter but explicit tracking, not silent deferral.

Status: DONE
Summary: APPROVE_WITH_NITS — 0 CRITICAL, 1 MAJOR (documented/deferred stale-identity-never-cleared gap, needs explicit tracking), 2 MINOR (stale "dormant" comment, undocumented fixture-only `agent: None` in serve.rs). Wire backward-compat, pid-gate scoping, and diff-key correctness all verified sound.
