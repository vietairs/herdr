# Root cause: split-right/split-down in a federated remote workspace spawns a local Mac shell

## Symptom
In a remote-federated workspace (`r:<host>#default:wN`), split right/down creates a new
pane whose backing process is a local shell (pwd `/Users/hvnguyen`) instead of a pane on
the mounted remote host. Existing (mount-time) remote panes work fine; only newly
created splits are wrong.

## Hypotheses considered

1. **Routing bug**: split action checks focus, but forgets to branch on "is this workspace
   a federation mount" and always calls the local spawn path. (would imply the remote
   protocol already supports remote pane creation and only client-side routing needs fixing)
2. **Protocol/feature gap**: `FederationMessage` has no "create pane" / "split" variant at
   all, so there is no wire request the client could even send to ask the remote host to
   split — the split handler's only option is the local spawn path, by construction.
3. **Correct routing but pane misfiled into remote layout**: local pane gets created, then
   incorrectly merged into the remote workspace's `Tab` layout (id-family mismatch), which
   is a secondary/compounding bug regardless of (1)/(2).

## CONFIRMED: (2) is the primary cause, and (3) is a real compounding effect. (1) is false — there's nothing to route to.

### Evidence chain

**Repro path (call graph, traced top-down from the split keybinding):**

- `src/app/input/navigate.rs:551` `split_focused_pane_via_api` → builds a
  `PaneSplitParams` with no workspace/remote distinction and calls
  `runtime_pane_split("tui.pane.split", ...)`.
- `src/app/runtime_mutations.rs:132-138` `runtime_pane_split` → dispatches
  `Method::PaneSplit(params)` through the same API-mutation path used for every pane
  (local or mirrored).
- `src/app/api/panes.rs:32-124` `handle_pane_split` — the single production entry point
  for all split requests. It resolves `target` purely by pane id / workspace id
  (lines 33-45), then unconditionally calls `ws.split_pane(...)` (line 84) or
  `ws.split_pane_with_ratio(...)` (line 71). **There is no check anywhere in this
  function for whether `ws_idx`'s workspace is a federation mount** (no `r:` prefix
  check, no `RemoteMirror`/mount lookup, no reference to
  `src/remote/federation/*` in this file at all — confirmed by `grep`).
- `src/workspace.rs:677-813` `split_pane` → `split_pane_with_ratio` →
  `split_pane_with_runtime` (private, line 799) — again no remote-awareness; it looks
  up `tab_idx` by pane id and always proceeds to spawn on the local machine.
- `src/workspace/tab.rs:326-386` `split_focused_with_runtime` — the actual PTY spawn
  site. For the ordinary split path (`command: None`) it unconditionally calls
  `TerminalRuntime::spawn(new_id, rows, cols, actual_cwd, ...)` (tab.rs:381-388), i.e.
  the **local-only** spawn constructor. `actual_cwd` defaults to
  `std::env::current_dir()` (tab.rs:344-345) when no explicit remote cwd is supplied —
  this is exactly the "pwd = /Users/hvnguyen" symptom.

**Why there's nothing to route to (the protocol gap):**

- `src/remote/federation/protocol/mod.rs:253-262` — the entire `FederationMessage`
  wire enum:
  ```rust
  pub enum FederationMessage {
      Handshake(Handshake),
      HandshakeResponse(HandshakeResponse),
      MountSnapshot(MountSnapshot),
      Event(EventChannelMessage),
      Terminal(TerminalChannelMessage),
      AgentStatus(AgentStatusMessage),
      Clipboard(ClipboardMessage),
      Fault(FaultMessage),
  }
  ```
  There is no `CreatePane`/`SplitPane`/`PaneCreate` variant, and a repo-wide grep for
  `CreatePane|SplitPane|PaneCreate` under `src/remote/federation/` returns nothing.
  Even if `handle_pane_split` correctly detected "this pane belongs to a federation
  mount," there is currently no message the client could send over the mount tunnel
  to ask the remote `federation-serve` host to actually perform a split there, and no
  handler on the serve side to act on it (`src/remote/federation/serve.rs` and
  `client.rs` were not modified by other in-flight agents' work per grep — confirmed
  absent, not merely unwired).
- All existing remote panes are instead created exactly once, eagerly, at **mount
  time** via `materialize_federation_mount` (`src/app/creation.rs:521-...`), which
  calls `self.build_remote_pane(...)` (creation.rs:572) → `PaneRuntime::spawn_remote`
  (`src/pane.rs:1763`, `src/terminal/runtime.rs:148`). This is a one-shot snapshot
  hydration path, not something the split action or any live user action re-invokes.
  There is no lazy/on-demand call site for `spawn_remote` outside mount
  materialization (grep confirms only the two definition sites, no other callers).

**Compounding effect — the new local pane is silently absorbed into the remote
workspace's layout (id-family mismatch), not merely a hallucinated separate pane:**

- `src/workspace.rs:112` `public_pane_id_for_number(workspace_id, pane_number)` derives
  the new pane's *public id* from `workspace_id`, which for a mounted workspace is the
  federation-namespaced id (e.g. `r:appn-ltu-vm-105#default:w3`). `register_new_pane_with_number`
  (workspace.rs:1159-1162), called from `split_pane`/`split_pane_with_ratio`, stamps the
  new — genuinely local — pane with a public id that *looks* like it belongs to the
  remote-namespaced workspace/tab family (e.g. `w3:p5`), because `split_pane_with_runtime`
  operates directly on the same `Tab`/`PaneLayout` object the mirrored panes live in
  (workspace.rs:814-820, `tab.layout.focus_pane(pane_id)` then
  `tab.layout.split_focused(direction)`). So the bad pane is not just co-located
  visually; it is layout-merged into the same tab as the correct remote panes under an
  id that is indistinguishable, by naming convention, from a real remote pane id.

## Ranking / elimination

- Hypothesis (1) "just a routing bug, protocol already supports it" is **eliminated**:
  there is no `FederationMessage` variant nor serve-side handler to route to. A pure
  client-side "check if remote, then call X" fix has no `X` to call yet.
- Hypothesis (2) is **confirmed**: this is a genuine feature gap. `handle_pane_split`
  (panes.rs:32) needs (a) a remote-mount detection check, and (b) a new
  request/response pair added to `FederationMessage` (e.g. `PaneSplitRequest` /
  `PaneSplitResponse` on the `Control` or a new channel) plus a serve-side handler in
  `src/remote/federation/serve.rs` that performs the split on the remote host and
  streams back the resulting pane/terminal, mirroring how `materialize_federation_mount`
  currently hydrates panes.
- Hypothesis (3) is **confirmed as a real, separate consequence** of (2): even after
  adding the protocol message, the fix must not let `handle_pane_split`'s local-spawn
  branch continue to silently produce a locally-backed pane stamped with a
  remote-workspace-namespaced public id. The routing check belongs before line 84/71
  in `src/app/api/panes.rs`, returning an explicit unsupported/dispatch-to-remote path
  rather than falling through to `ws.split_pane`.

## Fix shape assessment (not applied — reporting only)

This requires a **new protocol message**, not just a routing fix:
1. Add remote-mount detection at `src/app/api/panes.rs:32` (`handle_pane_split`) —
   likely via existing mount/workspace-id lookup helpers used elsewhere for `r:` ids.
2. Add a `PaneSplit`/`CreatePane` request+response pair to
   `FederationMessage` (`src/remote/federation/protocol/mod.rs:253`).
3. Add a serve-side handler in `src/remote/federation/serve.rs` that performs the real
   split on the remote host's own `Workspace`/`Tab` and returns the new pane info,
   analogous to the existing mount-snapshot hydration in
   `src/app/creation.rs:521` (`materialize_federation_mount`)/`build_remote_pane`.
4. Wire the client side to call `PaneRuntime::spawn_remote` (`src/pane.rs:1763`) for
   the resulting pane instead of `TerminalRuntime::spawn` (`src/workspace/tab.rs:381`).

## Unresolved questions

- Whether the fix should support split ratio and `cwd` overrides identically on the
  remote side (params already carry both; serve-side handler needs to honor them).
- Whether other pane-creation entry points (`split_pane_argv_command`,
  `split_focused_command`, new-tab creation) have the identical bug — grep suggests yes
  by construction (same `split_pane_with_runtime`/local-spawn pattern), but this
  report only proves the split-right/split-down path per the assigned repro.
