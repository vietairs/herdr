# Fix: remote agent identity relay (sidebar-missing root cause)

## What changed, file by file

- `src/remote/federation/protocol/mod.rs`: `AgentStatusMessage` gained
  `agent: Option<String>` (additive, `serde(default,
  skip_serializing_if)`). No `FEDERATION_PROTOCOL_VERSION` bump (still 3;
  handshake already rejects version skew before any channel frame
  decodes, and v3 is unreleased-but-already-bumped past the last tag).
- `src/remote/federation/protocol/codec.rs`: roundtrip fixture now
  exercises `agent: Some("claude")`; added
  `agent_status_frame_without_agent_field_decodes_with_none` (hand-built
  JSON without the field, confirms `serde(default)` gives `None`).
- `src/remote/federation/reducer.rs`: 5 test-fixture literals updated
  (`agent: None`) — the reducer itself is untouched; identity relay does
  not currently affect `RemoteMirror`'s own state, only the pane-detection
  side channel.
- `src/remote/federation/serve.rs`: dead-code-path (`serve::run`, unused
  in production per its own doc comment — production uses
  `federation_accept.rs`) construction site updated (`agent: None`) to
  keep compiling.
- `src/server/federation_actor.rs`: `FederationCommand::AgentStatuses`
  reply type is now `Vec<(String, AgentStatus, Option<String>)>`; the
  handler threads `AgentInfo.agent` through (already present on the
  `Method::AgentList` response, just not previously read).
- `src/server/federation_accept.rs`: `poll_agent_statuses` diffs on
  `(AgentStatus, Option<String>)` instead of bare status, so identity
  resolving after the first status poll still reaches the client; the
  outbound `AgentStatusMessage` now carries `agent`.
- `src/remote/federation/client.rs`: `TerminalChannelRouter`'s
  `agent_status_senders` map and `register_agent_status_sender`/
  `route_agent_status` now carry `pane::RelayedAgentStatus` (status +
  identity) instead of bare `AgentStatus`; `drive_mount_channel`'s
  `AgentStatus` arm builds it from the wire message. Test updated to
  assert both fields on the relayed value.
- `src/pane.rs`: new `pub(crate) struct RelayedAgentStatus { status,
  agent: Option<String> }`. `spawn_basic_detection_task`'s relayed-status
  branch parses the label and calls
  `agent_presence.observe_process_probe(Some(identified))` directly,
  bypassing the `pid > 0`-gated process probe (the actual root cause —
  that gate is always false for a remote-mirrored pane). Publishes
  `StateChanged` on an identity change alone, not just a status
  transition (identity-only changes were previously silently dropped).
  Existing relay test updated to send/assert identity.
- `src/terminal/runtime.rs`: `TerminalRuntime::relayed_agent_status_sender`
  wrapper's return type updated to match (`Sender<RelayedAgentStatus>`).

## Validation

- `ZIG=/Users/hvnguyen/.local/zig-0.15.2/zig cargo build --bin herdr`:
  clean, zero warnings from touched files (2 pre-existing unrelated
  warnings: `map_out`, `Capability::CLIPBOARD`).
- `ZIG=... cargo test --bin herdr -- --test-threads=4`: **2715 passed, 0
  failed** (baseline was 2714; net +1 for the new backward-compat decode
  test).
- `cargo clippy --bin herdr -- -D warnings`: could not run — the vendored
  `libghostty-vt` `ReleaseFast` build fails in this environment
  (Zig 0.15.2 bundled libcxx vs. the macOS SDK, unrelated to any file this
  fix touches; `cargo build`/`cargo test` dev-profile builds are
  unaffected). Pre-existing environment limitation, not introduced by this
  change.
- `cargo fmt`: run; diff limited to the 9 files this fix touched.

## Deviations from the debug report's minimal fix shape

- Identity is only ever *set* from a relayed frame, never cleared
  (`relayed_status.agent == None` is a no-op). The debug report's minimal
  fix shape didn't specify clear-on-exit behavior, and inventing a
  relay-side "agent disappeared" signal was out of scope for this fix —
  logged rather than silently added.
- `poll_agent_statuses`'s diff key widened from `AgentStatus` to
  `(AgentStatus, Option<String>)` — not explicitly asked for, but without
  it an agent identified *after* the first status poll (screen-text
  detection catching up post-mount) would never get its identity relayed
  at all, since the poll only re-sends on a diff. Minimal addition to make
  the identity fix actually reliable rather than racy.

## Unresolved / needs confirmation

- Live-VM re-verification (the report's original repro:
  `r:appn-ltu-vm-105#default:w3:p2` should now appear in `agent.list`) was
  not performed in this session — no live SSH/VM access from this task's
  scope. Recommend re-running the report's exact repro steps once this
  fix lands.
EOF
