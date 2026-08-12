# Wave 2 — federation contract fixes

Branch `feat/federation-multi-tab-workspace`, worktree
`/Users/hvnguyen/Projects/herdr/.claude/worktrees/federation-multi-tab-workspace`.
Uncommitted, built on wave 1. Nothing committed.

## 1. `close_remote` returns success

**What changed**

- `src/api/schema/response.rs:89` `ResponseResult::WorkspaceCloseRequested { origin }`,
  `:128` `TabCloseRequested { origin }`. Field naming/serde tags copied from
  `WorkspaceCreateRequested` (untagged field `origin: String`, `#[serde(tag="type",
  rename_all="snake_case")]` → wire types `workspace_close_requested` /
  `tab_close_requested`).
- `src/app/api/workspaces.rs:1211` and `src/app/api/tabs.rs:520`: `encode_error(id,
  "remote_close_pending", …)` → `encode_success(id, ResponseResult::…CloseRequested {
  origin })`. `origin` is snapshotted as `origin.as_str().to_string()` before the
  `HostKey` moves into `PendingRemoteClose`; the pending-close bookkeeping is unchanged.
- `src/app/input/navigate.rs:571` `surface_remote_close_response` now parses
  `SuccessResponse` first and raises the informational "close request sent" toast for the
  two new result types; an error envelope still routes to the attention toast. Toast text
  moved from the (now gone) error message into this function, wording preserved.
- Doc comments at `workspaces.rs:1089`, `tabs.rs` test doc, `workspaces.rs` test doc updated
  off `remote_close_pending`.
- Three existing tests re-pointed at the success envelope:
  `dispatch_remote_workspace_close_sends_a_request_and_registers_pending`,
  `dispatch_remote_tab_close_sends_a_request_and_registers_pending`,
  `tab_close_remote_ack_after_a_racing_local_tab_close_is_idempotent` (the last one parsed
  `ErrorResponse` only to prove the dispatch happened — now parses `SuccessResponse`).
- `docs/next/api/herdr-api.schema.json` regenerated with
  `HERDR_UPDATE_API_SCHEMA=1 cargo test --bin herdr api::schema` (+34 lines); the guard test
  `api::schema::tests::generated_protocol_schema_artifact_is_current` passes without the env
  var afterwards.

**CLI exit 0** — verified by reading the path, not by a live mount: `herdr workspace
close-remote` → `cli/runtime.rs:52 workspace_close_remote` → `print_method_response` →
`cli.rs print_response`, which returns 1 only when `response.get("error").is_some()`. The
handler now emits `{"id":…,"result":{"type":"workspace_close_requested",…}}`, so the
`error` key is absent and the exit code is 0. Same for `herdr tab close-remote`.

**Not changed:** `pane.close_remote` (`src/app/api/panes.rs:427`) still returns
`remote_close_pending`. It shipped on `master` (commit `1af58792`), so it is a released
contract and outside the approved scope — but it is now inconsistent with its two siblings
and the CLI still exits 1 on a successful `pane close-remote`. Flagged, not touched.

## 2. Remote-create redirect documented (behaviour untouched)

No code change. Trigger condition read from source, not paraphrased from the brief:
`handle_workspace_create` (`workspaces.rs:630`) redirects when **all** of

- `params.cwd.is_none()`, and
- `workspace_creation_source()` (`creation.rs:96`) resolves — the workspace pinned when the
  name dialog opened, else the sidebar-selected workspace while `Mode::Navigate`, else
  `active`, else `selected`, and
- `federation_host_key_for_workspace(source)` is `Some` — that workspace's `worktree_space`
  key is `federation:<host_key>` and the host key is in the live `remote_mirrors` registry.

Then the create is sent over the mount and the response is `workspace_create_requested {
origin }`; with no live mount the dispatch fails with
`remote_workspace_create_unsupported` rather than falling back to a local create.

Documented in:

- `docs/next/website/src/content/docs/cli-reference.mdx` — new paragraph under `workspace
  create`: no `--cwd` + federated current workspace ⇒ created on that host, response carries
  no ids, pass `--cwd` for a guaranteed local create, `remote_workspace_create_unsupported`
  on a dead mount.
- `docs/next/website/src/content/docs/socket-api.mdx` — new paragraph above the close_remote
  paragraph documenting the redirect rule and both response types.

## 3. Chained-create misreport

`src/server/federation_actor.rs:598-660`. `.and_then(… _ => None)` +
`.unwrap_or_else(dig "error" out of the envelope)` meant *any* non-`WorkspaceCreated`
**success** fell into the error-digging path, found no `error` key, and replied
`workspace_create_failed`. Now `.map(|success| match success.result { … })` handles every
success shape explicitly, and `unwrap_or_else` runs only when the response is not a
`SuccessResponse` at all — i.e. a genuine error envelope. A success can no longer be
laundered into `workspace_create_failed`.

**Decision — what a chained/forwarded create reports back (please review):** the peer gets
`Err("workspace_create_redirected: …")` (`federation_actor.rs:621-635`), plus a
`tracing::warn!`. Reasoning:

- The federation reply channel is `Result<(workspace_id, tab_id, pane_id, terminal_id),
  String>` and the wire enum `WorkspaceCreateResponse` has only `Created`/`Failed`. A
  redirected create has produced no ids on this host, and adding a third variant is a
  `FEDERATION_PROTOCOL_VERSION` change, which is out of scope (stays 6).
- Semantically the peer asked host B for a workspace *on B*. When B redirects onto host C,
  no workspace exists on B and none ever will, so from the peer's point of view the request
  was not fulfilled. Reporting it as refused is accurate; what is fixed is that it is no
  longer reported as `workspace_create_failed`, which claimed a create had been attempted
  and broken.
- Any other unexpected success gets its own `workspace_create_unexpected_result` code
  (`:644`) rather than being folded into the same bucket.

I did **not** stop host B from redirecting a peer's create (that would need either the
frozen redirect logic from task 2 changed, or the federation caller to compute and pass an
explicit `cwd`, duplicating `resolve_new_terminal_cwd`). My recommendation for a follow-up:
a peer-originated `workspace.create` should never consult B's ambient TUI focus — it should
always create locally — which removes the chained case rather than reporting it.

**Test gap:** no direct test for the chained arm. Producing a real
`WorkspaceCreateRequested` inside the actor requires the serving `App` to hold a live nested
mount (an `out_tx`-backed `remote_mirrors` entry); the helper that builds one lives in
`api/workspaces.rs`'s private test module and is not reachable from `federation_actor`'s
tests. The redirect itself is covered by `app::creation` tests
(`creation.rs:4938`, `:5183`) and the actor's happy path/error path stay covered.

## 4. Stable ids in the context menu

- `src/app/state.rs:1356` new `RemoteCloseMenuTarget { workspace_id, tab_id: Option<String> }`.
- `src/app/state.rs:1375` `ContextMenuState.federated: bool` → `remote_close_target:
  Option<RemoteCloseMenuTarget>`; `items()` gates "Close on host" on `.is_some()`, so the
  boolean and the ids can no longer disagree.
- `src/app/input/mouse.rs:1903` new `AppState::remote_close_menu_target(ws_idx, tab_idx)`
  snapshots the ids at menu-open time (returns `None` for a non-federated workspace, which
  is what hides the item); the three `ContextMenuState` constructions in `mouse.rs` use it.
- `src/app/input/modal.rs:1271` `apply_context_menu_action_via_api` destructures the menu so
  both "Close on host" arms (`:1333`, `:1364`) resolve the snapshotted ids and ignore
  `ws_idx`/`tab_idx` entirely.
- `src/app/input/navigate.rs:468` `close_workspace_idx_remote_via_api(usize)` →
  `close_workspace_remote_via_api(String)`; `:547` `close_tab_idx_remote_via_api(usize,
  usize)` → `close_tab_remote_via_api(String)`. A target that no longer resolves raises the
  same "no longer here" toast as before instead of being sent.
- `src/app/ids.rs:187` new `parse_current_public_tab_id`, mirroring the existing
  `parse_current_public_pane_id`: rejects the positional shorthands `parse_tab_id` accepts,
  so a stale snapshot cannot resolve to whatever tab now sits in that slot.
- `src/app/ids.rs` — removed `public_workspace_id_checked` (added by wave 1, now unused;
  it produced a dead-code warning).

**Deviation from the brief, stated explicitly:** the ids live on `ContextMenuState`, not
inside `ContextMenuKind`. `ContextMenuState` is where the existing federated snapshot lived,
it is the only consumer, and it keeps the change to ~13 constructor sites instead of ~25
plus the `ContextMenuKind` index-remap and invariant code. `ws_idx`/`tab_idx` are kept on
the kind for all the other actions that legitimately still use them.

**Regression tests** (`src/app/input/modal.rs:2561`, `:2609`):
`context_menu_close_workspace_on_host_via_api_refuses_a_shifted_index` opens the menu over
mirrored workspace `w1`, removes it so mirrored `w2` shifts into index 0, then picks "Close
on host" and asserts the toast is exactly `"that workspace is no longer here"` and `w2` is
untouched. Under the old index path index 0 resolves to `w2` and the toast would instead be
the `remote_close_unsupported` live-mount message, so the assertion discriminates. Tab
counterpart does the same with a removed tab. Both set
`toast_config.delivery = Herdr` (default is `Off`, so no toast is recorded otherwise).

## 5. Wire bump recorded

`docs/next/CHANGELOG.md` — new `### Changed` entry under Unreleased: protocol version 19 →
20, restart the server after upgrading or CLI commands report a version mismatch. Uses
`herdr server stop` + restart because there is no `herdr server restart` subcommand
(`src/cli/server.rs` only has `stop`). Root `CHANGELOG.md`, `README.md` and `website/`
untouched.

## Verification — verbatim final lines

`cargo test --bin herdr -- --test-threads=1`:

```
test result: FAILED. 3455 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 101.23s
```

The single failure is
`api::server::pane_graphics_stream::tests::inactive_owner_cancels_idle_stream_and_dispatches_close`
(`canceled idle stream should dispatch a close: Timeout`). Proven pre-existing and unrelated:
stashing every uncommitted change (wave 1 + wave 2) and rerunning the same command gives

```
test result: FAILED. 3451 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 106.56s
```

with the same single failure. Run in isolation on both trees it passes:

```
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 3455 filtered out; finished in 0.10s
```

`cargo test --test client_mode -- --test-threads=1`:

```
test result: ok. 14 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 12.54s
```

`cargo test --test api_ping -- --test-threads=1`:

```
test result: ok. 11 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 22.50s
```

`touch src/main.rs && cargo clippy --bin herdr`:

```
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 10.95s
```

(no warnings, no errors emitted.)

`cargo fmt --check` (after one `cargo fmt`):

```
FMT CLEAN
```

Schema guard, run without the update env var:

```
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 3455 filtered out; finished in 0.02s
```

`PROTOCOL_VERSION = 20`, `FEDERATION_PROTOCOL_VERSION = 6` — unchanged. `git status` shows
no `README.md`, root `CHANGELOG.md`, or `website/` modification.

## Could not do / unresolved

1. No live two-host validation of the new success envelopes or the redirect docs — no
   federated mount available here. CLI exit 0 is reasoned from `print_response`, not observed.
2. Chained-create arm has no direct test (reason above). Its behaviour is a judgment call —
   see the decision in §3; reverse it if you prefer the peer to be told the create succeeded
   somewhere it cannot reach.
3. `pane.close_remote` still returns the `remote_close_pending` error envelope and still
   exits 1 on success. Released contract, deliberately untouched — decide whether to align
   it in a later, explicitly-scoped change.

Status: DONE_WITH_CONCERNS
Summary: all five tasks implemented and verified green (one pre-existing unrelated flake);
concerns are the chained-create reply being a distinct refusal rather than a success, the
untouched `pane.close_remote` inconsistency, and no live federated validation.
