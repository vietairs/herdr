# Remote federated pane not detected as agent in sidebar

## Symptom
Claude Code running inside a remote federated pane (`r:appn-ltu-vm-105#default:w3`) never
shows in the local agents sidebar. Local panes on the same session detect fine. The remote
pane itself renders/works (terminal bytes and events relay correctly).

## Hypotheses considered

### H1 (eliminated): Sidebar filters out `r:`-prefixed workspace/pane ids
Checked `src/ui/sidebar.rs` iteration (`app.workspaces.iter()` at lines 368, 404, 786) — it
walks local `AppState.workspaces` uniformly; federated workspaces are mounted into the same
`app.workspaces` list (confirmed by the remote-origin badge logic at `sidebar.rs:269-342`,
which explicitly renders a badge *for* federated workspaces, proving they are iterated, not
filtered). So the sidebar itself has no id-prefix exclusion. Eliminated.

### H2 (eliminated): Detection never runs locally for remote panes, and that's "by design"/acceptable
True that `src/detect/` never runs a manifest probe against a remote pane's screen locally —
but that's intentional: `spawn_remote`-constructed `PaneRuntime`s are meant to receive the
*remote's own* real detection result (its `AgentStatus`) relayed over the federation link,
not re-detect locally. See `src/pane.rs:2822-2831` doc comment on
`relayed_agent_status_sender`. So "no local detection" is expected; the failure is downstream
in the relay wiring (see H3).

### H3 (CONFIRMED): AgentStatus relay is designed but never wired into a live call site
The remote's real `AgentStatus` computed by `RemoteMirror::apply_agent_status`
(`src/remote/federation/reducer.rs:152`) and exposed via `agent_status_display`
(`reducer.rs:133`) is *only* invoked in reducer unit tests
(`reducer.rs:741,764,792,828,848,874-892` — all inside `#[cfg(test)] mod tests`). No
production code calls either function.

Trace of the full chain, each link confirmed by grep + read:

1. **Remote server side (works)**: `serve.rs:263-292,352-370` polls `host.agent_statuses()`
   and emits `FederationMessage::AgentStatus(AgentStatusMessage)` frames over the wire.
2. **Local client wire read (works)**: `drive_mount_channel` in `client.rs` reads every frame
   type off the socket in its `match msg { ... }`.
3. **Local client processing (BROKEN)**: at `client.rs:451-454` —
   ```rust
   FederationMessage::Handshake(_)
   | FederationMessage::HandshakeResponse(_)
   | FederationMessage::MountSnapshot(_)
   | FederationMessage::AgentStatus(_) => continue,
   ```
   with the adjacent comment (`client.rs:448-450`): *"Handshake/HandshakeResponse/
   MountSnapshot are already consumed during `connect_and_mount`; AgentStatus relay is P6
   scope. Neither is this driver's concern."* — the frame is received and silently dropped.
   It never calls `mirror.apply_agent_status(...)`.
4. **Pane-side sink (never fed)**: `PaneRuntime::relayed_agent_status_sender()`
   (`pane.rs:2822-2831`) is the mechanism meant to push a relayed `AgentStatus` into the
   pane's own detection/event loop, and it is marked `#[allow(dead_code)]`. Grep across
   `src/app/*.rs` and `src/server/*.rs` shows zero production call sites — the only callers
   are `pane.rs`'s own unit tests (`pane.rs:4463-4502`). `app/creation.rs:716`
   (`TerminalRuntime::spawn_remote(...)`) constructs the remote runtime but never retrieves
   or connects `relayed_agent_status_sender()` to anything driven by the federation client.

So: server emits it, client reads the frame off the wire and throws it away, and even if it
didn't, nothing downstream would forward it into the pane runtime that feeds the sidebar's
`AgentStatus`/`AgentState`. The pane's `AgentState` therefore stays at its default
(non-agent/idle) forever for federated panes, and the sidebar (which reads that state
uniformly for all panes, local or federated per H1) correctly shows nothing, because there is
truly nothing to show.

## Confirmed root cause
`drive_mount_channel` discards inbound `FederationMessage::AgentStatus` frames
(`src/remote/federation/client.rs:454`) instead of calling
`RemoteMirror::apply_agent_status` (`src/remote/federation/reducer.rs:152`), and even that
reducer output is never forwarded to `PaneRuntime::relayed_agent_status_sender()`
(`src/pane.rs:2829`, dead in production; only call sites are tests at `pane.rs:4472,4500`
and `app/creation.rs:716`'s `spawn_remote` never wires it). The feature is fully built
end-to-end except this one link — explicitly labeled in-code as unfinished ("P6 scope",
"Dormant until a live federation call site (P8/P9) drives it").

## Where a fix would go (not applied)
1. `src/remote/federation/client.rs:451-454` — route `FederationMessage::AgentStatus(msg)`
   into `mirror.apply_agent_status(&msg, ...)` instead of `continue`.
2. A new call site (likely in `client.rs`'s mount-driving loop or `app/creation.rs`'s
   `spawn_remote` setup) must take `mirror.agent_status_display(pane_id)` /
   `apply_agent_status`'s resulting status and push it through
   `PaneRuntime::relayed_agent_status_sender()` for the matching remote pane.
3. `pane.rs:2828` `#[allow(dead_code)]` should be removed once wired, as a completion check.

## Unresolved
- Live socket confirmation (via `herdr agent explain <pane>` on the dev instance) was not
  performed in this pass — the source-level chain is unambiguous (dead code + explicit
  "P6/P8/P9 scope" comments), so it was not necessary to prove the cause, but it would give a
  runtime `AgentStatus::Idle`-forever confirmation if wanted.
- Confirm the `w3`-style pane naming used for `agent_status_display`/`apply_agent_status`
  keys (`terminal_id` vs full `r:host#session:pane` id) matches what `spawn_remote`'s pane is
  registered under, once wiring is added — not required to prove today's bug, but relevant to
  whichever fix implementation follows.
