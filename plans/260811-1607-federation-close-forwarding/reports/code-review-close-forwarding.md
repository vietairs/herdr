# Code review — federation close forwarding (working-tree diff)

Scope: uncommitted `git diff` in `.claude/worktrees/federation-multi-tab-workspace`, 29 files / ~3140 insertions. Read-only review; no files modified.
Snapshot caveat: the tree moved while I reviewed (another agent landed `parse_federation_workspace_id`/`parse_federation_tab_id` in `src/app/ids.rs` mid-review). Findings below reflect the tree as of the final re-check.

Verification run locally:
- `cargo test --bin herdr -- --test-threads=1` → **3449 passed, 0 failed** (incl. the known `pane_graphics_stream` flake).
- `cargo clippy --bin herdr` → clean after the in-flight `ids.rs` edit settled.
- `cargo check --target x86_64-pc-windows-msvc` → **could not run**: vendored libghostty-vt fails to cross-build (highway/simdutf for `x86_64-windows-msvc`). Windows findings are therefore source-traced, not compiler-confirmed.

---

## Critical

### C1. Windows build is broken: ungated caller of two `#[cfg(unix)]` methods — CONFIRMED (source-traced, high confidence)

- `src/server/federation_actor.rs:550` → `app.close_federation_target_workspace(&target_workspace_id)`
- `src/server/federation_actor.rs:563` → `app.close_federation_target_tab(&target_tab_id)`
- `src/app/creation.rs:1990-1991` and `src/app/creation.rs:2042-2043` — both fns are `#[cfg(unix)]` with **no `#[cfg(not(unix))]` twin**.
- `src/server/mod.rs:10` declares `pub(crate) mod federation_actor;` **ungated** (unlike `federation_accept`, line 8-9, which is `#[cfg(unix)]`).

Failure: `cargo build` on Windows fails with `E0599: no method named close_federation_target_workspace found for struct App`. Same for the tests module (`federation_actor.rs:1367/1440/1500/1522/…`), so `cargo test` on Windows fails too. The `FederationCommand::CloseWorkspaceRemote`/`CloseTabRemote` variants and their `Debug` arms are also ungated, which is fine on its own.

This is exactly the class CLAUDE.md's "platform-specific code must be compile-gated" rule targets, and the file already demonstrates the correct pattern at `federation_actor.rs:642-647` (`nudge_child_redraw` with a `#[cfg(not(unix))] let _ = …` fallback).

Fix options (either is consistent with the file): add `#[cfg(not(unix))]` twins for both `App` methods in `creation.rs` returning `Err("federation is not supported on this platform")`, **or** `#[cfg(unix)]`-gate the two dispatch arms plus the two enum variants, `Debug` arms and tests.

---

## High

### H2. `workspace.close_remote` / `tab.close_remote` never end a mount whose last mirrored workspace they retire — CONFIRMED

- `src/app/creation.rs:1400-1449` (`handle_federation_workspace_close_ready`) and the last-tab branch at `src/app/creation.rs:1560-1595` (`handle_federation_tab_close_ready`) call `purge_federation_state_for_workspaces` + `close_single_workspace_at` and stop there.
- The local verb does more: `src/app/api/workspaces.rs:1035-1059` computes `siblings_remain` and calls `self.state.end_federation_mount(host_key)` when none do, and additionally purges `pending_remote_clipboard_stages` (which `purge_federation_state_for_workspaces`, `creation.rs:2116-2128`, does not cover).

Scenario: a mount serving exactly one workspace; user runs `herdr workspace close-remote r:alice@host:w1`; host acks. The mirror workspace disappears from the sidebar, but `state.remote_mirrors` still holds the `HostKey`, the SSH link and drive task stay alive, and (per the comment the local path carries at `api/workspaces.rs:1020-1027`) a later remount of that host reports "already live" with nothing visible to close. Same for `tab.close_remote` on the last tab of the last mirrored workspace.

Mitigating context: `handle_federation_resync_workspace_removed` (`creation.rs:2286-2306`) has the same shape, so the behavior class is pre-existing for host-initiated removals. It is nonetheless a new user-reachable path to the dead-end state, and the local `workspace.close` verb it is meant to pair with does not have it.

### H3. Test claims a shared-counter invariant it does not exercise — CONFIRMED

`src/app/creation.rs` — `a_pane_close_and_a_tab_close_registered_together_occupy_distinct_pending_entries` (around line 3690). Its doc comment says "every close kind mints its `request_id` from the SAME counter … Pins the actual invariant that protects", but the test hardcodes `80u64`/`81u64` and calls `register_pending_remote_close` directly. It never calls `next_remote_close_request_id`, so introducing a second counter (the exact regression the comment describes) would leave this test green.

Invariant 6 itself **holds in the code**: `next_remote_close_request_id` is the single `pub(super)` counter (`src/app/api/panes.rs:39-58`) and both new dispatchers mint from it (`api/workspaces.rs:1177`, `api/tabs.rs:485`). Only the test's claim is stronger than the test. A real pin would mint two ids through the dispatchers (or through `next_remote_close_request_id` directly) and assert inequality.

---

## Medium

### M4. Doc comment asserts a trust-boundary protection that does not exist — CONFIRMED

`src/app/creation.rs:1986-1988`: "Fixed internal request id (`"federation-close-workspace"`), matching `ClosePane`'s `"federation-close-pane"` trust-boundary pattern". `grep` finds no such string anywhere; `close_federation_target_workspace` never goes through `handle_api_request`, so it has no request id at all. The *behavior* is fine (it bypasses the API handler entirely, which is stronger), but the comment names a mechanism that isn't there — the highest-value class of false signal in a security-relevant path. Reword to say it bypasses the JSON-API close handler outright.

For contrast, the real mechanism it alludes to is at `src/app/api/panes.rs:1882-1885`, where `id == "federation-close-pane"` gates the worktree-group branch.

### M5. Stale pending tab-close survives a tab removed by pane-close — CONFIRMED, low impact

`purge_pending_remote_close_for_tab` is called from exactly two places (`api/tabs.rs:348`, `creation.rs:2511`). A mirror tab can also vanish via the last pane in it closing (`handle_federation_close_pane_ready`, `creation.rs:1230-1245`, when `should_close_workspace` is false) — that route does not purge.

Impact is bounded to a leaked map entry: `Workspace::next_public_tab_number` is monotonic and never reused (`src/workspace.rs:600-601`, `1089-1090`, `1393`; pinned by `workspace::tests::tab_public_numbers_are_stable_and_not_reused_after_close`), so the stale `RemoteCloseTarget::Tab(id)` can never match a later tab. The entry is reclaimed on workspace close / mount end. Worth noting because the purge helper's doc comment (`creation.rs:1615-1630`) justifies itself with "slot reuse", which the id scheme already makes impossible — the real justification is bounded memory + not acting on an ack for a gone tab.

### M6. Non-unix error feedback is silently dropped — CONFIRMED, minor

`src/app/input/navigate.rs` `surface_remote_close_response`: on `#[cfg(not(unix))]` the branch is `let _ = envelope.error.message;`, so a `workspace_not_found` / `tab_not_found` / `remote_close_unsupported` produces **no** user feedback at all on Windows (the pending toast still works). The comment claims the only non-pending code there is `remote_close_unsupported`, which is not true — `handle_workspace_close_remote` can return `workspace_not_found` (`api/workspaces.rs:1096-1101`) and `handle_tab_close_remote` can return `tab_not_found` (`api/tabs.rs:377`) on any platform. Consider raising the informational toast (which is ungated) instead of dropping the message.

---

## Low

### L7. Plan ids / audit labels in code comments and doc comments — CONFIRMED

CLAUDE.md ("Stable Code Artifacts" in the user rules) forbids plan ids, phase numbers and audit labels in code comments and test names. New occurrences:

- `src/app/creation.rs:2743` — `/// \`ClosePaneRequest\` (Gap A, plans/260724-1536-federation-pane-close-sync).`
- `src/app/creation.rs` — `/// T15 (adversarial review): …` on the distinct-pending-entries test.
- `src/app/creation.rs` — `/// Predict risk 3 counterpart …` on `workspace_close_ready_after_a_racing_resync_removal_is_idempotent`.
- `src/app/api/tabs.rs:911` — `(reported bug T10 in the plan's own test matrix)`.
- `src/app/api/tabs.rs` — `/// Amendment (final review round): …`.
- Several "Purge-gap fix:" prefixes (`creation.rs:1615`, `creation.rs:2511`).

The file already carries pre-existing violations of the same shape (`plans/260722-1327`), so this is consistency-with-a-bad-precedent, not a new pattern — but these are new lines and cheap to fix.

### L8. `#[cfg_attr(not(unix), allow(dead_code))]` without a why-comment — CONFIRMED, trivial

`src/remote/federation/protocol/mod.rs:129` (on `Capability::WORKSPACE_TAB_CLOSE`) and `src/app/creation.rs:2740` (on `RemoteCloseTarget`). CLAUDE.md requires `#[allow]` to carry a comment explaining why. Both have a doc comment about the *item*, none about the allow. Note the first one also appears unnecessary: the constant is read on every platform by the ungated send helpers in `client.rs:176-208`.

### L9. `remote_close_pending` is returned as an error envelope

`herdr workspace close-remote` therefore exits non-zero on the *success* path. This matches the existing `pane.close` precedent (`api/panes.rs:425-430`) and the docs describe it explicitly (`docs/next/website/src/content/docs/cli-reference.mdx`), so it is consistent — flagging only so the scripting contract is a conscious choice.

---

## Invariants — verdicts

1. **Echo rule — HOLDS.** `grep` for `FederationMessage::WorkspaceCloseRequest|TabCloseRequest` construction finds exactly two production sites, both in `src/remote/federation/client.rs:192,207`. Their only callers are `src/app/api/workspaces.rs:1178` and `src/app/api/tabs.rs:486`. `src/app/creation.rs` contains no request construction — the host-ack handlers and every resync handler only mutate local state. `drive_mount_channel` explicitly ignores inbound `*CloseRequest` (`client.rs:841-846`).

2. **Capability gating — HOLDS.** Both `send_*_close_request` helpers check `mirror.supports(WORKSPACE_TAB_CLOSE)` before touching `out_tx`, and they are the only send path (see 1). Neither dispatcher constructs a `FederationMessage` itself. Agreement comes from the host-computed intersection (`federation_accept.rs:1717-1725` → `client.rs:281-336` → `set_agreed_capabilities`), so a peer that never advertises it yields `false` and the dispatcher returns `remote_close_unsupported`. Covered by `dispatch_remote_{workspace,tab}_close_without_the_capability_agreed_is_refused`.

3. **No worktree-group amplification — HOLDS.** Both host helpers use `close_single_workspace_at` (`creation.rs:1962-1968`), which nulls `worktree_space` before `close_selected_workspace()`, making `close_indices_for` fall back to the single index. `close_{workspace,tab}_remote_closes_exactly_one_workspace_sibling_survives` assert both the survivor's existence **and** its retained `worktree_space` membership — a genuinely strong assertion, not a phantom.

4. **No remote-driven UI mutation on the host — HOLDS for the confirm dialog.** Neither host helper reaches `handle_tab_close`/`confirm_implicit_worktree_group_close`; `state.mode` is asserted unchanged in all four actor tests. One residual, benign: `close_single_workspace_at` sets `state.selected = ws_idx` and `close_selected_workspace` clears `state.selection` / adjusts scroll. `close_selected_workspace` (`app/actions.rs:1683-1708`) restores `selected`/`active` to the previously focused workspace by id when it survives, so the host user's focus is not stolen; the text selection is cleared. Not worth changing, but it is a real (tiny) remote-driven UI effect.

5. **Lease gating — HOLDS.** `federation_actor.rs:537-566`: both arms `return` early on `!lease.is_mounted_controller(epoch, connid)` before touching `app`. `(epoch, connid)` are threaded from `federation_accept.rs:550-570`. Covered by `close_workspace_and_tab_remote_are_refused_for_a_non_controller_connid`, which also asserts no state mutation on refusal. The pre-existing `SplitPane`/`ClosePane`/`CreateWorkspace` gap is untouched, as instructed.

6. **Request-id correlation — HOLDS in code, weakly tested.** See H3.

7. **Pending-entry lifecycle — HOLDS in effect.** Workspace-keyed purge covers mount end, local `workspace.close`, resync workspace removal, and both new ready-handlers. Tab-keyed purge covers local `tab.close` and resync tab removal. The one uncovered route (tab dying via last-pane close) is harmless because tab numbers are never reused — see M5.

8. **CFG gating — VIOLATED.** See C1. All other new cfg boundaries check out: the new `AppEvent` variants are `#[cfg(unix)]` and only emitted from `drive_mount_channel`, which is itself `#[cfg(unix)]` (`client.rs:600`); `purge_pending_remote_close_for_tab` has a `#[cfg(not(unix))]` no-op twin; `raise_remote_close_failed_toast` is only called under `#[cfg(unix)]`; the `api.rs` event handlers are gated at the call site; `client.rs`'s send helpers and `api/{tabs,workspaces}.rs` dispatchers use only cross-platform items.

---

## Other correctness notes (verified, no action needed)

- **Id translation.** `strip_mount_namespace` (`remote/federation/id.rs:202-208`) reads only `mount.host_key`, so the placeholder `Mount` with empty `ServerInstanceId`/`mount_generation: 0` is safe. The workspace path strips the local public id directly; the tab path correctly notes that the local `<ws>:t<n>` form does **not** wrap the raw remote tab id and reverses through `remote_resync_tab_index` by `(workspace_id, tab_number)`. That index is populated at mount time, not just on resync (`creation.rs:725-733`), so a freshly mounted tab is closable. `dispatch_remote_tab_close_sends_a_request_and_registers_pending` pins the end-to-end result (`target_tab_id == "w1-tab2"`) on a two-tab mirror, i.e. the non-trivial case.
- **`RemoteCloseTarget::Tab` stores the canonical public tab id**, which is `ws.id + stable tab.number` (`app/ids.rs:19-25`), not a positional `t_<ws>_<idx>` — so the doc comment's stated rationale is actually true.
- **Trust boundary on host-side id parsing** was fixed in-flight by another agent: `close_federation_target_{workspace,tab}` now use `parse_federation_{workspace,tab}_id` (`app/ids.rs:79-104`), which match by exact id only and reject the bare-numeric/`w_N` positional shorthand `parse_workspace_id` accepts. Without that, a stale or hostile `"3"` from a peer would have closed whatever workspace occupied slot 3. Worth a regression test that a numeric/`w_N` target is refused — I did not find one.
- **Blocking round-trip** in `handle_{workspace,tab}_close_request` (`federation_accept.rs:704-806`) mirrors `handle_close_pane_request` exactly (`blocking_send` + `blocking_recv`, `Failed` on a gone actor). No new deadlock shape; the reader thread blocks for the duration of the actor call as it already did for pane close.
- **`federated_session_allows`** additions (`api/mod.rs:163,171`) are consistent with `WorkspaceClose`/`TabClose` already being on the allowlist.
- **Context menu** `federated` flag is only computed at the three mouse-driven construction sites (`input/mouse.rs:1100-1175`); there is no keyboard path to open a workspace/tab context menu, so no site is left with a wrong `false`. The item is appended last in every arm and matched by label in `input/modal.rs`, so index drift is not a concern.
- **No `unwrap()`/`panic!`/`todo!` in new production code**; all occurrences in the diff are inside `mod tests`. `tracing` used throughout.

---

## Recommended actions (ranked)

1. Fix C1 before any Windows build/release — add `#[cfg(not(unix))]` twins for `close_federation_target_workspace`/`close_federation_target_tab`.
2. Decide H2: either mirror `handle_workspace_close`'s `siblings_remain` → `end_federation_mount` logic in both ready-handlers, or explicitly document that a mount outlives its last mirrored workspace by design.
3. Strengthen the H3 test to actually mint through `next_remote_close_request_id`.
4. Correct the M4 doc comment.
5. Add a host-side regression test that a numeric / `w_N` federation target id is refused (guards the in-flight `parse_federation_*_id` fix).
6. Sweep L7/L8 comment hygiene.

## Unresolved questions

- Is the mount intended to stay alive after its last mirrored workspace is closed via `close_remote` (H2)? The resync path behaves that way; the local `workspace.close` path does not.
- The `ids.rs` trust-boundary fix landed mid-review — is it final, and does it want the regression test above?
- Windows compilation could not be verified locally (vendored libghostty-vt does not cross-build from macOS). C1 needs a real Windows/CI build to confirm the fix.
