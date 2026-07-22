# Implementation notes

- What: phase 03 left `docs/next/website/src/content/docs/keyboard.mdx` unedited.
  Why: phase step 4 says add the entry "only if the file documents the global menu entries";
  `keyboard.mdx` documents keybindings only and has no such list (`configuration.mdx`, not owned,
  has the one existing global-menu mention).
  Evidence: `grep -n "global menu" docs/next/website/src/content/docs/keyboard.mdx` → no match.
  Reversibility: trivially reversible; add a line if a future phase gives keyboard.mdx a menu list.

- What: phase 03 doc changes (persistence-remote.mdx, CHANGELOG.md) are English-only; `ja/` and
  `zh-cn/` locale copies of persistence-remote.mdx are untouched.
  Why: phase 03 owns only the English path; translations are not in the owned-file list and are
  out of scope for this feature.
  Evidence: `plan.md:39` owned-file row lists only the en `docs/next/...` paths; `ja/`, `zh-cn/`
  exist under `docs/next/website/src/content/docs/` per plan-validation fix #8.
  Reversibility: n/a, no change made; translators can add localized copies later.

- What: phase 02 added a "mount remote" entry to `AppState::global_menu_labels()`
  (`src/app/input/sidebar.rs`, unix-only). This is unowned-file collateral: it grew the mobile
  switcher's menu section by one row, breaking two tests outside phase 02's file ownership —
  `ui::mobile::tests::switcher_leads_with_agents_and_shifts_spaces_below` (hardcoded scroll-clamp
  row math) and `ui::tab_surface::tests::mobile_full_app_semantic_frame_is_characterized` (a full-
  frame hash characterization test). Neither `ui/mobile.rs` nor `ui/tab_surface.rs` is owned by any
  phase in this plan.
  Why: leaving them red would violate "tests added"/"build passes"; the breakage is a direct,
  correct consequence of an owned-file change (one more global-menu row), not a bug, and no other
  phase touches these files, so there is no ownership conflict to escalate.
  Evidence: `ZIG=... cargo test --bin herdr ui::` failed only these two before the fix; the mobile
  test's magic scroll/row constants assumed the pre-change menu item count.
  Reversibility: trivially reversible — revert the two test edits (mobile.rs test now derives the
  expected row from `mobile_switcher_max_scroll_for_height` instead of a fixed offset; tab_surface.rs
  test only has an updated expected hash literal). No production logic changed in either file.

- What: remediation of LOW log-injection finding in `src/app/api/workspaces.rs` —
  changed `%target` to `?target` at 3 call sites (`handle_workspace_mount_remote`:
  validate_remote_target rejection, is_local_target rejection, and the
  double_attach_conflict "already mounted" warn a few lines below the two
  originally-named sites).
  Why: `validate_remote_target` only rejects empty/leading-`-` targets (confirmed
  by reading `src/remote/unix.rs:285-293`), so a target containing `\n` or ANSI
  escapes reaches all three `tracing::warn!` calls unescaped when logged with `%`
  (Display), letting an attacker forge herdr-server.log lines. `?` (Debug) escapes
  control characters, matching the adjacent user-facing `{target:?}` at :90 which
  already did this correctly.
  Evidence: `grep -n '%target\|?target' src/app/api/workspaces.rs` before the fix
  showed 8 total call sites logging `target` with `%`; only the double_attach_conflict
  one (originally line 109) sits on the same request-validation path as the two named
  sites (same function, same untrusted per-request target list, pre-dial). The other
  five (`handle_federation_mount_ready`/`_failed`/`_ended`) log `target` from
  already-materialized mount state after a successful dial, not the raw per-request
  input — left unchanged as out of scope per the remediation's narrow instruction.
  Reversibility: trivial, revert `?target` back to `%target` at the 3 sites; no
  behavior change beyond log escaping, `cargo test app::api::workspaces` 24/24 pass.

- What: remediation — deleted `RemoteMountState::submitting` (`src/app/state.rs`) and its 3
  reads: Esc guard + re-entrancy guard in `src/app/remote_mount.rs`, mouse-cancel guard in
  `src/app/input/mouse.rs`, plus the unreachable "mounting…" render branch in
  `src/ui/remote_mount.rs`.
  Why: field was never set `true` in production code (grep confirmed reads-only); the guards
  it fed were permanent no-ops and the render branch was dead. Dialog close already happens on
  server ack; mount progress/failure surfaces via the existing `FederationMountFailed` notice
  path, so wiring a real in-progress state would be new behavior, not this fix's scope.
  Evidence: `grep -rn "RemoteMountState\|\.submitting" src` post-fix shows only the struct
  definition and its 2 construction/assert sites, no reads.
  Reversibility: trivial revert; re-add field + guards if a future change actually sets it.

- What: remediation — removed client-side leading-`-` and `localhost` rejection from
  `parse_mount_targets` (`src/app/remote_mount.rs`); server (`handle_workspace_mount_remote`,
  `src/app/api/workspaces.rs:84-100`) is now the sole validator. Rewrote
  `submit_remote_mount_keeps_dialog_open_with_error_from_server` to submit "localhost",
  actually dispatch to the server, and assert on the server's own error message (was
  previously unreachable dead-branch test — deleting the `ErrorResponse` branch at
  `remote_mount.rs:113-117` still passed it before this fix).
  Why: client used `eq_ignore_ascii_case`, server uses exact `==` (`src/remote/unix.rs:161-
  163`) — two layers disagreeing on one field. Single source of truth removes the drift risk
  entirely rather than reconciling the two rule sets.
  Evidence: reverted the `ErrorResponse` branch deletion locally post-fix and reran the
  rewritten test — it FAILED (panicked: "server should return an error for a localhost
  target"), confirming the test now exercises the server path; restored the branch, reran,
  15/15 `remote_mount` tests pass.
  Reversibility: trivial revert of the client-side checks; server-side validation is
  unaffected either way.

- What: left the mouse-cancel guard (`src/app/input/mouse.rs:299-303`, now un-guarded after
  Fix A) NOT collapsed into `close_remote_mount_dialog` (`src/app/remote_mount.rs`).
  Why: type mismatch, not a behavior question — `close_remote_mount_dialog` is `impl App`
  (`&mut self: App`), the mouse-cancel arm is inside `impl AppState` (`src/app/input/mouse.rs:
  69`, `&mut self: AppState`; `App { state: AppState, .. }`, `src/app/mod.rs:98-99`). `App`'s
  method is not reachable from an `AppState` receiver without restructuring one of the two
  impls, which is out of this remediation's minimal-change scope.
  Evidence: `grep -n "^impl App\b\|^impl AppState\b" src/app/remote_mount.rs src/app/input/
  mouse.rs` — remote_mount.rs:41 `impl App`, mouse.rs:69 `impl AppState`.
  Reversibility: n/a, no change made.
