# Plan validation — TUI add remote workspace (adversarial, pre-code)

Verdict: **APPROVE_WITH_FIXES**

Source-verified against worktree `feat/tui-add-remote-workspace` @ 5ec2a10b.

## 1. Do the named files/symbols exist as claimed? — mostly YES

Verified accurate (line drift ≤2 unless noted):

- `WorkspaceMountRemoteParams { targets, remote_keybindings }` — `src/api/schema/workspaces.rs:28-32`. `remote_keybindings` genuinely unread by `handle_workspace_mount_remote` (`src/app/api/workspaces.rs:53-126`). ✅
- Handler trims/filters only, no validation — `src/app/api/workspaces.rs:59-72`. ✅
- `fn validate_remote_target` private — `src/remote/unix.rs:285`; windows twin `src/remote.rs:136`; `pub(crate) use unix::*` (`src/remote.rs:7`) makes the visibility bump reachable as `crate::remote::validate_remote_target`. ✅
- `is_local_target` / `remote_ssh_targets` `pub(crate)` — `src/remote/unix.rs:161,167`. ✅
- ssh argv sites — `src/remote/unix.rs:417-419`, `546-548`, `1044-1046`; `BatchMode=yes` only on the probe `:1185`; `stderr(inherit)` `:422,550`. ✅
- Only callers of the method: `src/app/api.rs:1025`, `src/server/autodetect.rs:333` via `mount_remote_request` (`src/remote/unix.rs:122`). No TUI/CLI surface. Gap confirmed. ✅
- Test harness shape (`App::new(&Config::default(), true, None, api_rx, EventHub)`, `app.event_rx`, `app.state.remote_mirrors`, `begin_federation_mount`) — `src/app/api/workspaces.rs:1270-1310`. ✅
- `Mode` enum `src/app/state.rs:793-814` (plan says 792-813), `wants_ascii_input` `:826-840` (plan 825-840). Exhaustive `Mode` matches: `src/ui.rs:427-457`, `src/app/input/mod.rs:87-116`, `src/app/mod.rs:1768-1823`. ✅
- `GlobalMenuAction` `src/app/input/modal.rs:75-82`, `global_menu_actions` `:84-95`, `apply_global_menu_action` `:129-142` (takes `&mut AppState`). `global_menu_labels` `src/app/input/sidebar.rs:199-208`. Index-addressed by `src/app/input/modal.rs:1317`, `src/app/input/mouse.rs:144,1182`, `src/ui/mobile.rs:200`. ✅
- UI helpers all exist — `src/ui/widgets.rs:32,39,51,62,146,151,164`; `src/ui/dialogs.rs:118,134,232`; `src/ui.rs:552`. ✅
- `RemoteConfig { manage_ssh_config }` single field — `src/config/model.rs:863-869`. ✅ (justifies copying the new-worktree template, not the searchable list)

## 2. Runtime/client boundary — NO violation

No schema/wire/event/socket change; the collector calls the existing `Method::WorkspaceMountRemote`. Proposed names (`remote_mount`, `request_open_remote_mount_dialog`, `runtime_workspace_mount_remote`) are neutral — no sidebar/row/card/widget. Server-side validation (phase 01) is correctly placed server-side.

Note: `federated_session_allows` (`src/api/mod.rs:99,156`) already classifies `WorkspaceMountRemote` as forbidden for view-only federated sessions, but is `#[allow(dead_code)]` and unwired — no effect today.

## 3. owned_files disjointness — YES at file level, but the set is INCOMPLETE (see fixes 1-3)

Phase 01 {`src/app/api/workspaces.rs`, `src/remote/unix.rs`} ∩ Phase 02 {…} = ∅. Correct. Phase 02 has a *semantic* dependency on 01 (its server-error test), acknowledged in-plan.

## 4. Tests — mixed. Two are tautological, one is factually wrong (fixes 4-5)

## 5. Protocol / deps / scope — CLEAN

No `PROTOCOL_VERSION`/`FEDERATION_PROTOCOL_VERSION` bump, no schema regen, no new crate, no `remote_keybindings` UI, no new abstraction. Scope stays a collector. ✅

## 6. Target validation & threat model — CORRECT

`Command::new("ssh").arg("-T").arg(target)` — no shell, so metachar injection is impossible; the real hole is **ssh's own option parsing** of a leading-`-` argv element (`-oProxyCommand=…` ⇒ RCE for anything that can write `herdr.sock`). Plan's correction of the discovery report's "SAFE" verdict is right. `localhost` divergence is real (`src/remote/unix.rs:161` filters CLI-side only).

## 7. Upstream-merge risk — real, unflagged

The `Mode` variant forces edits in 8 upstream-hot files with exhaustive matches plus three upstream label-assertion tests. Every one is a merge-conflict surface on the next upstream pull. Not a blocker; should be stated in the plan's risk table.

---

## REQUIRED fixes

1. **Phase 02 — add `src/server/headless.rs` to owned_files and drain both flags there.** `src/server/headless.rs:869-928` is a *second, parallel* drain loop (`request_new_linked_worktree`, `request_submit_worktree_create` at `:905`, `request_reload_config` at `:926`). Draining only in `src/app/mod.rs:1050-1096` makes the menu entry and the mouse submit **dead in server mode** — the mode a "running local herdr" actually uses. Mirror both blocks with a `crate::render_prof::event(...)` line like the neighbours.
2. **Phase 02 step 1 — a second `AppState` struct literal exists at `src/app/mod.rs:575-595`** (`App::new`), not just `src/app/state.rs:1895-1930`. All three new fields must be initialized in both.
3. **Phase 02 step 7 — updating `global_menu_labels` breaks three existing tests in the same file.** `src/app/input/sidebar.rs:591`, `:616`, `:638` assert exact label vectors; worse, `persistence_mode_menu_surfaces_detach_action` (`:604-628`) clicks `menu.y + 4` expecting `detach` at index 3 — inserting `"mount remote"` after `"reload config"` shifts detach to index 4 and that click will fire the new action instead. Plan must name these three tests as edits and `#[cfg(unix)]`-gate the expectations.
4. **Phase 01 test `mount_remote_accepts_plain_and_user_at_host_targets` — the pre-mounted-host trick DOES spawn a task.** `src/app/api/workspaces.rs:78-90`: the duplicate-`HostKey` branch `tokio::spawn`s a `FederationMountFailed` send before `continue`. Assert only "no ssh dial / `remote_mirrors.len() == 1`", never "no event". Same correction applies to phase 02's `submit_remote_mount_closes_dialog_on_success_ack`.
5. **Phase 02 test `submit_remote_mount_keeps_dialog_open_with_error_from_server` is unreachable as designed.** With `parse_mount_targets` rejecting `localhost` client-side (step 2), the request never leaves the client, so the `ErrorResponse` branch of `submit_remote_mount_via_api` is never exercised. Either (a) drop `localhost`/leading-`-` from the client parser and let the server be the single authority (preferred — DRY, and phase 01 already returns `invalid_request` synchronously), keeping only the blank-input check client-side; or (b) keep the parser but write the server-error test against a target the client accepts and the server rejects. As written, both layers duplicate the rule and the server-error path ships untested.
6. **Phase 01 — `validate_remote_target`'s error strings are CLI-flag-worded** (`"missing value for --remote"`, `"--remote target must not start with '-'"`, `src/remote/unix.rs:286-292`). Surfacing those verbatim in a TUI dialog for an API call is wrong. Wrap them in the handler (e.g. `format!("invalid remote target {target:?}: must not start with '-'")`) or add a neutral message at the call site; do not edit the CLI strings (CLI tests assert them).
7. **Phase 02 step 8 — make `modal_paste_target_active` a definite step, not a "check".** `src/app/input/mod.rs:608-620` ends in `_ => false`, and `src/app/mod.rs:1758-1765` gates clipboard paste on it. Without the variant, ctrl/cmd-v into the dialog is silently dropped even though `paste_into_active_text_input` routes it.
8. **Phase 03 — decide explicitly about the locale trees.** `docs/next/website/src/content/docs/` contains `ja/` and `zh-cn/`. State "en only, translations follow upstream" or list them; silence will produce an arbitrary choice.

## Nits

- `plan.md:39` phase-02 owned-file list omits `src/server/headless.rs` (fix 1) — also update the plan table, not just the phase file.
- `plan.md:76` acceptance says menu label `mount remote workspace`; `phase-02:156` step 7 says `"mount remote"`. Pick one (`"mount remote"` fits the existing lowercase 2-word menu style; keep `mount remote workspace` as the dialog header).
- `global_menu_actions_and_labels_stay_index_aligned` passes today — it is a regression guard, not a red-first test. Fine, but do not list it under "Tests first".
- `remote_mount_mode_keeps_the_ime` restates the allowlist; keep it (matches the existing convention at `src/app/mod.rs:1997-2011`) but it proves little.
- Phase 02 step 10 mouse-cancel should also clear `name_input_replace_on_type`, matching `src/app/input/mouse.rs:270-272`.
- Line refs `792-813` / `825-840` for `Mode` / `wants_ascii_input` are off by one (`793-814` / `826-840`).
- Add "next upstream merge = 8 hot-file conflict surface" to `plan.md`'s risk table.

Status: DONE_WITH_CONCERNS

Summary: The plan's factual base is unusually solid — every API/symbol/line it cites checks out, the threat model is correct, and it adds no protocol/dependency/scope. But phase 02 misses the headless server's parallel drain loop (feature would be dead in server mode), misses a second `AppState` literal, and misses three existing menu tests it will break; two of its tests assert things the code cannot do.

Concerns/Blockers: Fix 1 is functional, not cosmetic — without it the feature does nothing in the normal server runtime. Fix 5 asks for a real design decision (single vs duplicated validation authority) before phase 02 starts.

Unresolved questions:
1. Should client-side `parse_mount_targets` keep the `localhost`/leading-`-` rules at all, given phase 01 makes the server synchronously authoritative? (Affects fix 5 and several phase-02 tests.)
2. Should the menu entry be hidden when the current session is a view-only federated mirror, ahead of `federated_session_allows` (`src/api/mod.rs:99`) being wired?
