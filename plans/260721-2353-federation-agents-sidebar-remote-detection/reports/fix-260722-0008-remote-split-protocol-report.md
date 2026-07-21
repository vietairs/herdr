# Remote split protocol scaffolding + misfile guard

## Scope decision (read first)

The debug report's fix shape needs 4 pieces: (1) remote-mount detection in
`handle_pane_split`, (2) a new wire request/response pair, (3) a serve-side
handler that performs the real split, (4) client-side routing that awaits the
response and materializes the new pane. This fix delivers (1) and (2) fully,
and (3) only for the test-only `serve.rs`/`loopback.rs` path. Production's real
serve-side dispatch is `src/server/federation_accept.rs`, which hand-rolls its
own `TerminalChannelMessage` routing rather than implementing `FederationHost`
(confirmed: that trait has exactly one implementor, `loopback::FixtureHost`,
not `federation_accept.rs`) — that file is under `src/server/*`, **not** in
this task's owned-files list (`src/remote/federation/*`). (4) needs a
synchronous JSON-API handler to await an async round trip over a live mount's
reader loop, which requires new request-correlation state in `App`'s mount
registry — also not an owned file, and materially larger than "wire an
existing pattern" (no such correlation mechanism exists anywhere yet).

Per the assignment's own precedent for `src/pane.rs` ("report as a follow-up
instead of touching an unowned file"), the same treatment applies to
`server/federation_accept.rs` and the `App`-level bridge — logged as follow-up
below. What's shipped instead: full protocol scaffolding (tested end-to-end
over the loopback host) plus a hard guard that replaces the dangerous silent
misfile with an explicit, typed refusal.

## Files changed

- `src/remote/federation/protocol/mod.rs` — added `SplitPaneRequest`,
  `SplitPaneResponse`, `SplitDirection` (federation-local, independent of
  `crate::api::schema::common::SplitDirection`); added both to
  `FederationMessage` + `Channel::channel()` (routed to `Channel::Control`);
  bumped `FEDERATION_PROTOCOL_VERSION` 2 -> 3; added
  `split_pane_request_response_roundtrip_through_the_wire_codec` test.
- `src/remote/federation/protocol/codec.rs` — added the 3 new variants to
  `every_message_variant()`'s exhaustive round-trip fixture.
- `src/remote/federation/serve.rs` — added `FederationHost::split_pane(...)
  -> Result<(String, String), String>` to the trait; `handle_inbound` now
  matches `FederationMessage::SplitPaneRequest`, calls `host.split_pane`, and
  sends back `SplitPaneResponse::Created`/`Failed`.
- `src/remote/federation/loopback.rs` — implemented `split_pane` for
  `FixtureHost` (mints a sibling terminal id for a known target, errors for an
  unknown one); added 2 tests exercising the request/response round trip.
- `src/remote/federation/client.rs` — added an exhaustive match arm for the 2
  new variants in `drive_mount_channel` (client currently only
  sends/receives `SplitPaneRequest`/`Response` is a no-op pass-through today;
  no live send call site yet — see follow-up).
- `src/app/api/panes.rs` — `handle_pane_split` now classifies the target
  workspace's public id via `crate::remote::federation::id::classify`; if
  `IdClass::Remote(_)`, returns `encode_error(id, "remote_split_unsupported",
  ...)` before ever reaching `ws.split_pane`/`split_pane_with_ratio`. Added
  regression test `pane_split_in_a_federated_workspace_is_refused_not_misfiled_locally`.

Not touched: `src/workspace.rs`, `src/workspace/tab.rs`, `src/app/creation.rs`,
`src/protocol/wire.rs` (no client/UI wire-protocol change was needed — only
the independent federation protocol changed).

## Validation

- `cargo build --bin herdr` — clean (2 pre-existing unrelated dead-code
  warnings: `map_out`, `Capability::CLIPBOARD`).
- `cargo clippy --bin herdr --no-deps` — same 2 pre-existing warnings, no new
  ones.
- `cargo test --bin herdr federation -- --test-threads=4` — 121 passed, 0
  failed (includes the new protocol roundtrip test and 2 new loopback split
  tests).
- `cargo test --bin herdr -- --test-threads=4` (full suite) — **2702 passed,
  0 failed, 0 ignored**.
- `cargo test --bin herdr -- pane_split remote_split split_pane
  --test-threads=4` — 18 passed (existing local-split behavior unchanged).

## Deviations

1. Did not touch `src/server/federation_accept.rs` (production serve-side
   dispatch) — out of ownership; no real remote split is performed in
   production yet, only the wire protocol + a loopback-tested reference
   handler exist.
2. Did not build an `App`-level request/response correlation bridge — no such
   mechanism exists anywhere in the codebase today; would require new state
   in the mount registry (not an owned file) and is a materially larger
   change than this task's file boundary allows.
3. Net behavior change for users today: split-right/split-down inside a
   federated workspace now fails with a clear `remote_split_unsupported`
   error instead of silently spawning a misfiled local pane. This is strictly
   safer than the prior behavior but does **not** yet make remote split work
   end to end.

## Follow-up required (logged in implementation-notes.md too)

- `federation_accept.rs`: add a `SplitPaneRequest` handler performing a real
  split against the live `AppState` workspace, mirroring its existing
  hand-rolled terminal-channel routing.
- `App`: a per-mount request-id-keyed correlation map (or oneshot registry)
  so `handle_pane_split` can enqueue a request and await/poll the matching
  response, then materialize the new pane via the same path
  `build_remote_pane`/`spawn_remote` (`src/app/creation.rs`, `src/pane.rs`)
  uses for mount-time panes.
- Once both land, `handle_pane_split`'s refusal should gate on "no live mount
  for this workspace" rather than "workspace is remote" outright.
- Remote VM binaries need redeploy once/if the above follow-up lands (this
  fix alone changes only client-side/loopback-tested code paths — no new
  production wire behavior ships yet, so no redeploy is required for *this*
  change, but any future patch closing the follow-up above will need it).

## Unresolved questions

- Whether split ratio/cwd overrides should be honored identically on the
  remote side once real dispatch lands (params already carry both).
- Whether other pane-creation entry points (`split_pane_argv_command`,
  `split_focused_command`, new-tab creation) share the identical local-spawn
  pattern in federated workspaces — the debug report flagged this as likely
  but unproven; this fix only guards the split-right/split-down path exactly
  as scoped.

Status: DONE_WITH_CONCERNS
