# Fix: wire federation AgentStatus relay into the pane detection loop

Root cause: plans/260721-2353-federation-agents-sidebar-remote-detection/reports/debug-260721-2357-remote-agent-detection-not-in-sidebar-report.md (H3).

## Files changed

- `src/remote/federation/client.rs`
  - `TerminalChannelRouter`: added `agent_status_senders: HashMap<String, mpsc::Sender<AgentStatus>>`, `register_agent_status_sender`, `route_agent_status`; `forget` now also clears the agent-status entry.
  - `drive_mount_channel`: `FederationMessage::AgentStatus(status_msg)` now calls `mirror.apply_agent_status(&status_msg, generation, hub)` (mirror bookkeeping + fencing: `RejectedStale`/`Gap`/`Reset` handled like the event channel) then unconditionally `router.route_agent_status(&status_msg.terminal_id, status_msg.status)`. Previously this arm was `continue` (dropped).
  - Added `use crate::api::schema::common::AgentStatus;`.
  - New test `drive_mount_channel_relays_agent_status_to_the_registered_pane_sink`.
- `src/app/creation.rs`
  - `build_remote_pane`: after `TerminalRuntime::spawn_remote`, registers `runtime.relayed_agent_status_sender()` (if `Some`) into `router.register_agent_status_sender(raw_terminal_id, sender)`, keyed by the same raw terminal id `open_terminal` already uses.
- `src/terminal/runtime.rs`
  - Added `pub(crate) fn relayed_agent_status_sender(&self) -> Option<mpsc::Sender<AgentStatus>>` delegating to the wrapped `PaneRuntime` (the wrapper had no prior delegation for this method, blocking the `creation.rs` call site — see Deviations).
- `src/pane.rs`
  - Removed `#[allow(dead_code)]` from `PaneRuntime::relayed_agent_status_sender`; updated its doc comment to point at the real production call site instead of "dormant".
- `plans/260721-1830-federation-link-cleanup-toast-visibility/implementation-notes.md`
  - Appended a What/Why/Evidence/Reversibility entry for this wiring decision (per task instruction — that plan's notes file was the one specified, distinct from this fix's own plan directory).

## Key-mismatch resolution (previously flagged "Unresolved" in the debug report)

`RemoteMirror::apply_agent_status` looks up the target pane by its **namespaced** `terminal_id` (`map_in(msg.terminal_id, mount).to_public_id()`), matching against `PaneInfo.terminal_id` in the mirror. `TerminalChannelRouter`'s `output_senders`/now `agent_status_senders` are keyed by the **raw** (un-namespaced) terminal id — the same one `build_remote_pane` computes via `strip_mount_namespace` and passes to `open_terminal`. `AgentStatusMessage::terminal_id` on the wire is the remote's own raw id (server emits it straight from `host.agent_statuses()`, never namespaced). So routing keys off `status_msg.terminal_id` directly (raw-to-raw) rather than trying to resolve through the mirror's namespaced pane lookup — this sidesteps the mismatch entirely instead of reconciling two different id spaces. The mirror lookup still runs (for its own `PaneInfo.agent_status`/`PaneUpdated` bookkeeping), but its `ReducerAction` result does not gate whether the pane's detection loop is fed.

## Validation

- `ZIG=/Users/hvnguyen/.local/zig-0.15.2/zig cargo check` — clean (2 pre-existing unrelated warnings: `map_out`, `Capability::CLIPBOARD`, both dead-code, untouched by this change).
- `cargo test drive_mount_channel -- --test-threads=4` — both tests pass (existing terminal/clipboard-routing test + new AgentStatus-relay test).
- `cargo test --bin herdr federation -- --test-threads=4` — 118 passed, 0 failed.
- `cargo test --bin herdr pane:: -- --test-threads=4` — 274 passed, 0 failed.
- `cargo test app::creation:: -- --test-threads=4` — both federation-materialization tests pass (`build_remote_pane` call site untouched in behavior for the happy path).

## Deviations

- Added a small delegating method to `src/terminal/runtime.rs` (not in the original owned-files list, which named client.rs/reducer.rs/pane.rs/creation.rs). `TerminalRuntime` is a newtype wrapper (`pub struct TerminalRuntime(crate::pane::PaneRuntime)`) with no blanket `Deref`; every `PaneRuntime` method it exposes to `app/creation.rs` has an explicit one-line delegating wrapper (e.g. `detection_text`, `terminal_title`). `relayed_agent_status_sender` had no such wrapper, so `creation.rs`'s call site failed to compile (`E0599`). Added the minimal matching delegation, following the exact pattern of the adjacent methods — no new abstraction, no behavior change beyond exposing the existing `PaneRuntime` method through the existing wrapper.

## Unresolved

- None outstanding from the debug report's own "Unresolved" section — the terminal_id key-mismatch question is resolved above by routing on the raw id rather than the mirror's namespaced id.
- Live socket/dev-instance confirmation (`herdr agent explain <pane>` against a real federated mount) was not performed in this pass; validation is unit-test level only (new relay test + full federation/pane suites green). A live check would give an end-to-end runtime confirmation but was not required to close out this specific wiring gap.

Status: DONE
Summary: Wired `FederationMessage::AgentStatus` through `drive_mount_channel` into a new raw-terminal-id-keyed `TerminalChannelRouter::route_agent_status`, registered from `build_remote_pane`, removing the dead-code marker on `PaneRuntime::relayed_agent_status_sender`; new regression test plus full federation/pane suites (118 + 274 tests) pass.
Concerns/Blockers: none.
