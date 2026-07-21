# Remote split materialization: mount drive task builds the runtime, App splices it into layout

## What was implemented

1. **`src/remote/federation/client.rs`** (owned): new `SplitMaterializationContext`
   (rows/cols/scrollback_limit_bytes/host_terminal_theme/events/render_notify/
   render_dirty). `drive_mount_channel` gained 3 new params: `out_tx`,
   `outbound_clipboard_tx`, `split_materialization: Option<&SplitMaterializationContext>`.
   On `SplitPaneResponse::Created`, when `Some(ctx)`: opens the new pane's
   `Terminal` channel via `router.open_terminal`, spawns a real
   `TerminalRuntime::spawn_remote`, registers its relayed-agent-status sender,
   builds `TerminalState`/`PaneState`, and sends the bundle to `App` via
   `AppEvent::FederationSplitPaneReady`. On `SplitPaneResponse::Failed`, sends
   `AppEvent::FederationSplitPaneFailed` when `Some(ctx)` (always still logs).
   Updated the 2 existing tests' call sites (`None`) and added a new test
   `drive_mount_channel_materializes_a_runtime_on_split_pane_created`.

2. **`src/events.rs`** (owned): new `AppEvent::FederationSplitPaneReady(Box<FederationSplitPaneReady>)`
   / `AppEvent::FederationSplitPaneFailed { request_id, reason }`, plus the
   `FederationSplitPaneReady` payload struct (pane_id/terminal_id/terminal/
   runtime/pane_state) with a manual `Debug` impl (mirrors `FederationMountReady`'s
   existing precedent, since `TerminalRuntime`/`TerminalState` don't derive it).

3. **`src/app/mod.rs`** (owned, per the recorded design's own "or app/mod.rs"
   allowance): new `App` field `pending_remote_splits: HashMap<u64,
   creation::PendingRemoteSplit>`, initialized in `App::new`.

4. **`src/app/creation.rs`** (owned): new `PendingRemoteSplit` struct
   (ws_idx/target_pane_id/direction/ratio/focus); `App::register_pending_remote_split`/
   `take_pending_remote_split`; `App::handle_federation_split_pane_ready`
   (pops the pending context, locates the target tab via
   `find_tab_index_for_pane`, calls `Tab::insert_existing_pane` — same
   primitive `materialize_federation_mount` uses — registers the
   runtime/terminal, focuses if requested, emits `PaneCreated`/layout-updated
   events); `App::handle_federation_split_pane_failed` (toast, mirroring
   `handle_federation_mount_failed`'s exact delivery-mode match).

5. **`src/app/api/workspaces.rs`** (owned): `handle_federation_mount_ready`'s
   drive-task spawn now captures rows/cols/scrollback/theme once and builds
   `Some(SplitMaterializationContext { .. })`, passed into `drive_mount_channel`
   along with `out_tx`/`outbound_clip_tx` (both already lived in this
   function, previously unused by the drive task).

6. **Deviations (4 small mechanical edits outside the owned-file list, logged
   in `implementation-notes.md` in full)**:
   - `src/app/api/panes.rs::dispatch_remote_pane_split` — one call to
     `register_pending_remote_split` after minting `request_id` (the only
     place that owns both the request_id and the local layout context).
   - `src/app/api.rs::handle_internal_event` — 2 new early-return arms for
     the new `AppEvent` variants (mirrors the existing 3 `FederationMount*`
     arms).
   - `src/app/actions.rs` — 2 `Vec::new()` arms in `AppState`'s exhaustive
     `AppEvent` match (Rust exhaustiveness forces every variant into every
     match over the enum).
   - `src/remote/federation/session.rs` — updated its own `drive_mount_channel`
     call site with 3 new positional args (`out_tx.clone()`/
     `outbound_clip_tx.clone()`/`None`); needed clones (not moves) since the
     outer scope still does its own pre-existing `drop(out_tx)` teardown.

## Validation

- `ZIG=~/.local/zig-0.15.2/zig cargo check --bin herdr`: clean, only the 2
  pre-existing dead-code warnings (`map_out`, `Capability::CLIPBOARD`).
- `cargo clippy --bin herdr --no-deps`: same 2 pre-existing warnings, no new.
- `cargo test --bin herdr federation -- --test-threads=4`: **125 passed, 0
  failed** (baseline 124, +1 new test).
- `cargo test --bin herdr -- --test-threads=4` (full suite): **2707 passed,
  0 failed, 0 ignored** (baseline 2706, net +1).

## Remaining gap (v1 scope, not a regression)

Materialization always inserts a horizontal-chain split at the request's own
direction/ratio against the target pane — it doesn't reconcile against
subsequent remote-side layout drift (e.g. two racing splits). Same caveat
`materialize_federation_mount` already carries for mount-time panes; not new
to this fix. Remote VM binaries: this fix is entirely client-side (the
requesting herdr process); the serve-side dispatch (already redeployed per
the prior report in this chain) is unchanged.

## Unresolved questions

- None outstanding for this fix's own scope; the only known gap is
  documented above and pre-existed structurally.

Status: DONE
