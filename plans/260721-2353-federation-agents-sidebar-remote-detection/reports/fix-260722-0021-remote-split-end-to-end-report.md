# Remote split end-to-end: server dispatch + client fire-and-forget bridge

## Scope delivered

1. **Production serve-side dispatch (item 1, fully working):**
   `federation_accept.rs`'s `reader_loop` now handles `SplitPaneRequest` by
   round-tripping (blocking, on its own `std::thread`) through a new
   `FederationCommand::SplitPane` (`server::federation_actor`), which calls
   the live `App`'s existing `Method::PaneSplit` JSON-API handler --
   the SAME code path the local TUI/CLI split action uses -- then replies
   `SplitPaneResponse::Created`/`Failed` on the connection's outbound queue.
   No duplicated split logic; `FederationHost::split_pane` (serve.rs) was
   not reused since `federation_accept.rs`'s architecture never implements
   that trait (confirmed by the prior report; App is `!Send`, held by
   `HeadlessServer`'s own event loop).

2. **Client bridge (item 2, partial -- see gap below):** `handle_pane_split`
   (`app/api/panes.rs`), for a remote-federated workspace, now looks up the
   target pane's own `PaneRuntime` and, if it is `spawn_remote`-constructed
   (has a live `remote_out_tx`), sends the real `SplitPaneRequest` over that
   pane's own mount tunnel and replies `remote_split_pending` (fire-and-forget
   -- see design note). Falls back to the original `remote_split_unsupported`
   only when the pane has no live mount. `client.rs`'s `drive_mount_channel`
   now logs (`tracing::info!`/`warn!`) `SplitPaneResponse` instead of
   silently dropping it.

3. **Tests kept green + extended** (no PTYs beyond the existing
   `spawn_remote` loopback pattern already used in this codebase).

## Design note: why fire-and-forget

`handle_pane_split` runs synchronously inline with `App`'s own tick and
cannot `.await` the async `drive_mount_channel` task that will eventually
read the `SplitPaneResponse`; blocking on a `oneshot::Receiver` there risks
stalling the very tokio worker the response needs to arrive on. The smallest
correct design given that hard boundary: send now, reply
`remote_split_pending` (not a fabricated `PaneInfo`), and log the eventual
response. Full materialization of the new pane into a real local
`Tab`/`PaneRuntime` needs a reactive hook that only `app/api/workspaces.rs` +
`events.rs`/`app/mod.rs` can provide (see gap below) -- out of this fix's
owned files. Full reasoning + the exact 3-file follow-up design is recorded
in `implementation-notes.md`.

## Files changed

- `src/server/federation_accept.rs` (owned): `reader_loop` routes
  `SplitPaneRequest` to new `handle_split_pane_request`; new test
  `reader_loop_routes_a_split_pane_request_and_replies_created`.
- `src/remote/federation/pane_source.rs` (owned): `RemoteTerminalSourceHandle`
  gains `raw_terminal_id()`/`out_tx()` accessors.
- `src/remote/federation/client.rs` (owned): `drive_mount_channel` now logs
  `SplitPaneResponse::Created`/`Failed` and warns on an unexpected inbound
  `SplitPaneRequest`, instead of silently dropping both.
- `src/app/api/panes.rs` (owned): `handle_pane_split`'s remote branch calls
  new `dispatch_remote_pane_split`; new module-level
  `next_remote_split_request_id()` counter.
- `src/pane.rs` (owned): `PaneRuntime::remote_terminal_id()`/
  `remote_out_tx()` accessors; new test
  `remote_runtime_exposes_its_raw_terminal_id_and_out_tx`.
- `src/server/federation_actor.rs` (**deviation, not in owned list** --
  see below): new `FederationCommand::SplitPane` variant + dispatch arm;
  2 new tests.
- `src/terminal/runtime.rs` (**deviation, not in owned list** -- see
  below): 2 delegation methods mirroring the existing
  `relayed_agent_status_sender` pattern.
- `plans/260713-1217-herdr-remote-workspace-federation/implementation-notes.md`:
  appended design/deviation/gap record.

Not touched: `src/workspace.rs`, `src/workspace/tab.rs`, `src/app/creation.rs`
(no changes needed -- the existing per-pane `remote_out_tx` already gave a
mount-tunnel handle without a new registry).

## Deviations from strict file ownership

Two small, mechanical edits landed outside the assigned owned-file list,
logged per orchestration-protocol rather than silently expanding scope:

1. `src/server/federation_actor.rs`: `federation_accept.rs`'s only path to
   the live `App` is this actor's existing `FederationCommand` channel; no
   existing variant can perform a split, so item 1 (this fix's primary ask)
   was structurally impossible without adding one.
2. `src/terminal/runtime.rs`: `App` stores `TerminalRuntime` (a newtype over
   `PaneRuntime`), not `PaneRuntime` itself; the accessors added to the
   owned `src/pane.rs` needed a 3-line delegation to be reachable from
   `app/api/panes.rs`.

Both are additive, mirror an existing precedent in the same file
(`relayed_agent_status_sender`), and invent no new logic.

## Validation

- `cargo check --bin herdr` (ZIG=~/.local/zig-0.15.2/zig): clean, only the 2
  pre-existing dead-code warnings (`map_out`, `Capability::CLIPBOARD`).
- `cargo clippy --bin herdr --no-deps`: same 2 pre-existing warnings, no new.
- `cargo test --bin herdr federation -- --test-threads=4`: **124 passed, 0
  failed**.
- `cargo test --bin herdr split_pane -- --test-threads=4`: **22 passed, 0
  failed** (includes the pre-existing
  `pane_split_in_a_federated_workspace_is_refused_not_misfiled_locally`
  regression test, unchanged and still passing).
- `cargo test --bin herdr -- --test-threads=4` (full suite): **2706 passed,
  0 failed, 0 ignored** (up from the pre-existing 2702 baseline; net +4).

## Remote VM binaries

**Yes, redeploy needed** for this patch (unlike the prior fix in this
chain, which only touched loopback-tested code). `federation_accept.rs` is
the real co-located production serve-side handler -- any already-deployed
remote host binary must be rebuilt to actually perform splits when it
receives a `SplitPaneRequest`. The requesting client's binary also needs
redeploy so `handle_pane_split` sends the request at all (old binaries keep
returning `remote_split_unsupported` unconditionally).

## Unresolved / follow-up (not guessed at, listed for the controller)

- The 3-file, ~1-AppEvent-variant follow-up needed to materialize the new
  remote pane into a real local `Tab`/`PaneRuntime` (exact design in
  `implementation-notes.md`) requires expanding ownership to
  `app/api/workspaces.rs`, `events.rs`, and `app/mod.rs` -- none owned by
  this fix.
- Whether `remote_split_pending` (a new error code, not a new
  `ResponseResult` variant) is an acceptable interim JSON-API contract vs.
  adding a proper "accepted" response shape once the materialization
  follow-up lands.
- Split ratio/cwd honoring on the remote side is already wired
  end-to-end (both flow through unchanged into the remote's own
  `Method::PaneSplit` call) -- confirmed working via
  `split_pane_against_a_known_target_pane_creates_a_real_pane_and_replies_ok`.

Status: DONE_WITH_CONCERNS
