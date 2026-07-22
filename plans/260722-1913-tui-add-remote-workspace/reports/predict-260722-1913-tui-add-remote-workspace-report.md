# predict — TUI affordance for `workspace.mount_remote` (5-persona pre-implementation debate)

Scope: add in-app collector for existing server method `workspace.mount_remote`. All paths verified in worktree `/Users/hvnguyen/Projects/herdr-worktrees/tui-add-remote-workspace`.

---

## 1. Systems Architect

**A1. `apply_global_menu_action` cannot dispatch — signature takes `&mut AppState`, not `&mut App`.**
`src/app/input/modal.rs:129` `apply_global_menu_action(state: &mut AppState, action)`. The API dispatch path needs `&mut App` (`src/app/runtime_mutations.rs:12`, `src/app/api.rs:32`). So a menu/context-menu entry **must** set a pending-request flag on `AppState` and be drained in `App::compute_view_and_handle_pending_state` — the exact pattern at `src/app/mod.rs:1050,1055,1073` (`request_new_linked_worktree`, `request_submit_worktree_create`) with fields declared at `src/app/state.rs:1423-1424`. Any plan that "calls the API from the menu handler" will not compile. Decision: name the fields neutrally (`request_open_remote_mount_dialog: bool`, `request_submit_remote_mount: bool`), not `request_sidebar_*`.

**A2. Guardrail is satisfiable but only if nothing new goes on the wire.** `Method::WorkspaceMountRemote` is already routed (`src/app/api.rs:1025`) and already ack'd with `ResponseResult::WorkspaceMountRemoteRequested` (`src/app/api/workspaces.rs:124`). Feedback already exists as `AppEvent::FederationMountFailed`/`Ready` (`src/events.rs:159,167`). So the whole TUI change is a collector + a `runtime_workspace_mount_remote` wrapper next to `runtime_workspace_create` (`src/app/runtime_mutations.rs:31-37`). If the plan proposes any new socket message, event, or params field, it has crossed the guardrail.

**A3. `remote_keybindings` is a dead param — do NOT surface it.** `WorkspaceMountRemoteParams.remote_keybindings` (`src/api/schema/workspaces.rs:31`) is never read by the handler (`src/app/api/workspaces.rs:54-126` binds only `params.targets`). A checkbox for it would be a UI that does nothing. Send `false`, like the existing test at `src/app/api/workspaces.rs:1296` and the autodetect caller (`src/server/autodetect.rs:322`).

**A4. Do not touch `WorkspaceMountRemoteParams`.** The generated schema `docs/next/api/herdr-api.schema.json:4516,8590` is asserted by `src/api/schema/tests.rs` and `tests/cli/surface.rs`. Any field add = schema regen + two test-file updates + a merge-conflict magnet on the next upstream pull.

---

## 2. Security Engineer

**S1. [REAL BUG] The API handler performs NO target validation — `validate_remote_target` is CLI-only.**
`validate_remote_target` (`src/remote/unix.rs:285-293`, rejects `-` prefix) is called only from `extract_remote_args` (`:212,218,224`). `handle_workspace_mount_remote` (`src/app/api/workspaces.rs:59-65`) only trims and drops empties. The target then lands as `command.arg("-T").arg(target)` at `src/remote/unix.rs:417-419` / `:546-548` / `:1044-1046`. `ssh -T -oProxyCommand=<cmd> …` — ssh parses a leading-`-` argv element as an **option**, not a host. That is arbitrary command execution reachable from any process that can open `herdr.sock`. The scout's "SAFE — mechanism prevents injection" verdict holds for the shell, **not** for ssh's own option parsing, and it assumed the CLI validator was on the path. MUST: make `validate_remote_target` `pub(crate)` and call it in the handler (server side, not just the dialog) before spawning.

**S2. Managed ssh config lifetime is shorter than the tunnel it serves.** `prepare_and_mount_federation_target` (`src/remote/unix.rs:692-726`) holds `remote_ssh` (whose `Drop` deletes `/tmp/herdr-ssh-<pid>-<attempt>/`, written by `write_managed_ssh_config`, `src/remote/unix.rs:2337-2410`) only until the function returns — but `dial_and_mount`'s returned `tunnel_guard` keeps the ssh child alive far longer. The dir with the `ControlPath` socket is unlinked underneath a live master. Today one mount per CLI invocation hides this; a dialog invites repeated mounts → N temp dirs, N unlink-under-live-ssh races. [WATCH], not a blocker.

**S3. No `BatchMode` on the dial paths.** Only the probe at `src/remote/unix.rs:1185` sets `BatchMode=yes`. Federation dial (`:417-419`, `:546-548`) and `sh_output` (`:1057-1062`) do not. ssh reads passwords/passphrases from `/dev/tty`, not stdin. Because the herdr server can share the controlling terminal the TUI is drawing on, a password- or unknown-host target can make ssh write a prompt straight into the TUI's terminal and steal keystrokes. `attempt_federation_mount` even uses `stderr(Stdio::inherit())` (`src/remote/unix.rs:422`). Decision: either force `BatchMode=yes` for API-initiated mounts, or document key-auth-only.

---

## 3. Performance / Reliability

**P1. The event loop is safe — but only because the handler never awaits.** `handle_workspace_mount_remote` `tokio::spawn`s per target and returns immediately (`src/app/api/workspaces.rs:92-121`). Budget per target is `FEDERATION_CONNECT_TIMEOUT` 10s + `FEDERATION_MOUNT_TIMEOUT` 15s = 25s (`src/remote/unix.rs:575,577`). So the dialog can close instantly. But the **pre-dial** phase is unbounded: `prepare_remote_herdr` + `ensure_remote_server_ready` run in `spawn_blocking` (`src/remote/unix.rs:702-713`) with **no timeout** — a target that TCP-blackholes can leave a blocking-pool thread parked for ssh's own connect timeout with zero user feedback and no cancel. MUST decide: show an in-flight indicator with no cancel affordance, or accept a silent 30–120 s dead period.

**P2. `Config::load()` runs on the async executor per mount.** `src/remote/unix.rs:693-697` — synchronous file IO outside the `spawn_blocking` block below it. One-off today; a dialog makes it user-triggerable repeatedly. [WATCH].

**P3. Partial failure of a multi-target mount is per-target and silent-ish.** Duplicate host → `FederationMountFailed` per target and `continue` (`src/app/api/workspaces.rs:78-91`); the rest still dial. Failure surfaces only as a toast via `handle_federation_mount_failed` (`src/app/api/workspaces.rs:271-304`), which is **suppressed entirely** when `toast_config.delivery` is `Terminal`/`System` and `local_terminal_notifications` is false (the `_ => {}` arm at `:301`). Result: user types 3 targets, 2 fail, and depending on config sees nothing. MUST: dialog-local error surface (like `WorktreeOpenState.error`, `src/app/state.rs:710`, rendered `src/ui/dialogs.rs:524-534`) rather than relying on the toast.

**P4. Dedup is on `HostKey::new(target, session_name)` — a raw string key.** `double_attach_conflict` (`src/app/state.rs:1672-1677`) matches the literal target. `alice@host` and `host` and `alice@host:22` are three distinct keys → three concurrent mounts of the same box, none rejected. The dialog will make this trivially reachable. [WATCH]; do not "fix" by canonicalizing in the client — that is a server fact.

---

## 4. UX Engineer

**U1. `localhost` behaves differently through the API than through the CLI.** `is_local_target`/`remote_ssh_targets` (`src/remote/unix.rs:160-173`) filter `localhost` on the CLI path only; the API handler does not. Typing `localhost` in the dialog attempts a real SSH dial. MUST decide: reject it in the collector with an inline message, or filter it server-side (shared fact → server side is correct).

**U2. Windows: the affordance must be cfg-gated or it is a dead button.** `#[cfg(not(unix))] handle_workspace_mount_remote` returns `unsupported_platform` (`src/app/api/workspaces.rs:28-38`), and Windows is a documented target (`website/src/content/docs/windows-beta.mdx`). Do not render the menu entry on non-unix. Per CLAUDE.md, use `#[cfg(unix)]` on the menu-action arm and dialog module, not a runtime `cfg!()`.

**U3. Template choice: "open existing worktree" is the wrong shape; "new linked worktree" is right.** There is no saved-targets list to browse — `RemoteConfig` has exactly one field, `manage_ssh_config` (`src/config/model.rs:865-869`); no persisted remote list exists. So the searchable-list machinery of `render_open_existing_worktree_overlay` (`src/ui/dialogs.rs:424-557`) has nothing to list. Copy the single-text-input + buttons shape of `render_new_linked_worktree_overlay` (`src/ui/dialogs.rs:232-325`) + `new_linked_worktree_button_rects` (`:134-151`) + key handling (`src/app/input/worktrees.rs:237-265`). Parsing multiple targets from one line by whitespace collides with S1 (`-o…` tokens) — validate every split token.

**U4. Discoverability: `GlobalMenuAction` is the only mouse-first entry point that fits.** `global_menu_actions` (`src/app/input/modal.rs:84-93`) lists Settings/Keybinds/ReloadConfig/WhatsNew/Detach; hit-testing already exists (`modal.rs:1307`). The worktree dialogs hang off a workspace-scoped context menu (`ContextMenuKind::GitWorkspace`, `modal.rs:701-710`), which is wrong here — mounting a remote is session-scoped, not workspace-scoped.

---

## 5. Maintainer

**M1. Merge-conflict surface.** The fork just merged upstream v0.7.5 (`5ec2a10b`). The touchpoints — `Mode` enum (`src/app/state.rs:793-814`), `Mode::wants_ascii_input` (`:826-838`), `global_menu_actions` (`modal.rs:84`), `dialogs.rs`, `mouse.rs`, `compute_view_and_handle_pending_state` (`mod.rs:1035-1090`) — are all upstream-hot. Minimize per-file footprint: new render fn in a **new** `src/ui/remote_mount.rs`, new key/mouse handling in a **new** `src/app/input/remote_mount.rs`, leaving only one-line additions in the hot files.

**M2. Adding a `Mode` variant has a non-obvious obligation.** `wants_ascii_input` (`src/app/state.rs:826-838`) is an explicit allowlist; a free-text dialog mode must be **omitted** from it (matching `RenameWorkspace`/`NewLinkedWorktree`). Also confirm no exhaustive `match state.mode` elsewhere breaks. Add a test asserting the new mode is not ASCII-forced.

**M3. Test strategy is already scaffolded.** `src/app/api/workspaces.rs:1252-1300` already tests `handle_workspace_mount_remote` on a real `App` (duplicate-mount rejection, `remote_keybindings: false`). Add there for S1 validation. Add `AppState::test_new()`-level tests for: open/close dialog transitions, input editing, target-list parse, and inline-error rendering state — no PTYs needed.

**M4. Protocol stability is fine if nothing is added.** `PROTOCOL_VERSION` (`src/protocol/wire.rs`) and `FEDERATION_PROTOCOL_VERSION` stay untouched — the method exists and is already versioned. `tests/cli/surface.rs` asserts the CLI surface: adding a CLI subcommand (not required by this task) would force that test update; a TUI-only change does not.

---

## RANKED consolidated list

1. **[MUST-ADDRESS] Server-side target validation is missing** — `handle_workspace_mount_remote` (`src/app/api/workspaces.rs:59-65`) never calls `validate_remote_target` (`src/remote/unix.rs:285-293`); leading-`-` target becomes an ssh **option** at `src/remote/unix.rs:417`. Fix in the handler, not only the dialog.
2. **[MUST-ADDRESS] Dispatch must go through a pending-request flag** — `apply_global_menu_action` has no `&mut App` (`modal.rs:129`); use the `mod.rs:1050-1090` drain pattern.
3. **[MUST-ADDRESS] Failure feedback cannot rely on the toast** — `handle_federation_mount_failed`'s `_ => {}` arm (`src/app/api/workspaces.rs:301`) silently swallows failures under some `toast_config` settings. Keep a dialog-local `error: Option<String>`.
4. **[MUST-ADDRESS] Do not surface `remote_keybindings`** — schema field is dead in the handler (`src/api/schema/workspaces.rs:31` vs `workspaces.rs:54-126`). Send `false`.
5. **[MUST-ADDRESS] cfg-gate the affordance for non-unix** — `#[cfg(not(unix))]` handler returns `unsupported_platform` (`src/app/api/workspaces.rs:28-38`).
6. **[MUST-ADDRESS] Decide `localhost` semantics** — CLI filters it (`src/remote/unix.rs:160-173`), API does not.
7. **[WATCH] Unbounded pre-dial phase / no cancel** — `spawn_blocking(prepare_remote_herdr + ensure_remote_server_ready)` has no timeout (`src/remote/unix.rs:702-713`); only the 25 s dial is bounded (`:575,577`).
8. **[WATCH] ssh interactive prompts on the TUI's terminal** — no `BatchMode` on the dial (contrast `src/remote/unix.rs:1185`), `stderr(Stdio::inherit())` at `:422`.

Also-ran [WATCH]: `HostKey` string-equality dedup (`src/app/state.rs:1672`); managed-ssh-config dir dropped while tunnel lives (`src/remote/unix.rs:692-726`, `:2337-2410`); `Config::load()` on the executor (`:693`).

---

## Predicted diff shape

| File | Change | ~LOC |
|---|---|---|
| `src/app/state.rs` | `Mode::MountRemoteWorkspace` variant (+ **omit** from `wants_ascii_input`), `remote_mount: Option<RemoteMountState>`, 2 request flags + `Default` init | ~40 |
| `src/app/input/remote_mount.rs` **(new)** | state struct, open/close, key handling, target parse+validate, tests | ~180 |
| `src/ui/remote_mount.rs` **(new)** | `render_remote_mount_overlay` + `remote_mount_button_rects` (mirrors `dialogs.rs:232-325,134-151`) | ~130 |
| `src/app/input/modal.rs` | `GlobalMenuAction::MountRemoteWorkspace` (cfg(unix)) + arm in `global_menu_actions`/`apply_global_menu_action` | ~15 |
| `src/app/mod.rs` | 2 drain blocks in `compute_view_and_handle_pending_state` (~`:1050-1090`) | ~15 |
| `src/app/runtime_mutations.rs` | `runtime_workspace_mount_remote` wrapper | ~8 |
| `src/app/input/mouse.rs` | button/field hit-test routing | ~40 |
| `src/ui/mod.rs` (render dispatch) | one overlay arm | ~5 |
| `src/app/api/workspaces.rs` | **server-side** `validate_remote_target` call + `localhost` decision + tests | ~35 |
| `src/remote/unix.rs` | `validate_remote_target` → `pub(crate)` | ~2 |
| `docs/next/website/src/content/docs/` (`persistence-remote.mdx` or `keyboard.mdx`) | short section | ~20 |

Expect ~450–500 net added lines, ~9 existing files touched with small edits + 2 new files. **No** change to `src/api/schema/`, `docs/next/api/herdr-api.schema.json`, `src/protocol/wire.rs`, or federation protocol.

---

Status: DONE_WITH_CONCERNS

Summary: The TUI-side work is a straightforward collector over an already-complete server path, but the discovery's "target string is safe" conclusion is wrong for the API entry point — `handle_workspace_mount_remote` skips `validate_remote_target`, so a leading-`-` target is parsed by ssh as an option. Five other must-address decisions (dispatch mechanism, failure feedback path, dead `remote_keybindings`, non-unix gating, `localhost` semantics) should be settled in the plan before any code.

Concerns/Blockers:
- Risk 1 is a pre-existing server-side hole that this feature makes materially more exposed; fixing it is in scope and belongs on the server, not the dialog.
- Risk 8 (ssh prompting on the TUI's own terminal) can corrupt the display; no clean fix short of `BatchMode=yes` for API-initiated mounts, which changes behavior for password-auth users.

Unresolved questions:
1. `localhost` in the dialog: reject in collector, or filter server-side to match `remote_ssh_targets` (`src/remote/unix.rs:167-173`)?
2. Multi-target in one dialog (whitespace-split, matching `--remote a b c`) or one target per mount?
3. Force `BatchMode=yes` on API-initiated dials, accepting that password/passphrase targets stop working from the TUI?
4. Should the in-flight mount show any progress state, given `FederationMountReady` is the only completion signal and there is no cancel path?
