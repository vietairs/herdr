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

- What: re-added `RemoteMountState::submitting`/`pending` (`src/app/state.rs`), wired to the real
  server-owned dial+mount task via `begin_submission`/`resolve_pending_target` and the two
  `FederationMountReady`/`FederationMountFailed` handlers (`src/app/api/workspaces.rs`). Put the
  re-entrancy guard inside `submit_remote_mount_via_api` itself (`src/app/remote_mount.rs`) rather
  than duplicating it at the `Enter` key-match arm, so the same guard also covers the mouse
  "mount" button click path (`AppState::request_submit_remote_mount`, set unconditionally by
  `src/app/input/mouse.rs:297` and drained by `App::run`/`headless.rs` into this same function) —
  that mouse path was explicitly left un-guarded in the prior remediation entry above and is now
  covered for free instead of adding a second, divergent guard.
  Why: task's design; DRY between the two submit entry points.
  Evidence: `cargo test --bin herdr remote_mount app::api::workspaces -- --test-threads=4` →
  17 + 28 passed; `cargo fmt --check` clean; `cargo check --bin herdr` clean (2 pre-existing
  dead-code warnings only).
  Reversibility: trivial revert of the two files' additions; no protocol/API change.

- What: `handle_federation_mount_ready`'s two early-return branches (already-mounted-conflict,
  materialize-failure) do NOT resolve `pending` for their target — only the function's true
  success exit (after `mount_drive_tasks.insert`) does.
  Why: those branches are pre-existing, rare, already-log-only failures with no other user-facing
  surfacing; resolving `pending` there without setting an error would read to the dialog as "this
  target succeeded" when it silently didn't. If the affected target was the last one pending, the
  dialog stays showing "mounting…" until the user dismisses it with Esc (which always works,
  submitting or not) rather than auto-closing on a false-success signal.
  Evidence: not exercised by any of the 7 required tests; documented here per "no TODO left, track
  the workaround" rather than silently narrowing scope.
  Reversibility: easy to extend later — add the same `resolve_pending_target` + error-set call at
  each of those two early returns if product wants the dialog to surface them too.

- What: outcome correlation only checks `remote_mount.is_some()` + `resolve_pending_target`
  returning true (target was actually in `pending`); it does not otherwise distinguish "dialog was
  reopened for a new submission after the old one resolved" from "dialog is still the original
  submission". Per the task's own fallback: "if you cannot cheaply distinguish that, prefer only
  touching targets present in `pending` (an unknown target is ignored)".
  Why: `RemoteMountState` carries no submission/generation id: doing this precisely would need a
  new field, which is a bigger surface for a narrow edge case (reopen the dialog with an
  overlapping target string while an old dial for that exact same string is still in flight).
  Evidence: `federation_mount_ready_after_dialog_dismissed_does_not_resurrect_it` test covers the
  required "dismissed, no resurrection" case; the "reopened with the same target string" case is
  the acknowledged residual gap.
  Reversibility: add a monotonic submission id to `RemoteMountState` and thread it through
  `pending` (e.g. `Vec<(u64, String)>`) if this edge case becomes a real report.

- What: adversarial review fix (F1) — `#[allow(dead_code)]` + comment on
  `RemoteMountState::resolve_pending_target` (`src/app/state.rs`).
  Why: both call sites (`handle_federation_mount_ready`/`handle_federation_mount_failed`,
  `src/app/api/workspaces.rs`) are `#[cfg(unix)]`, so it has zero callers on
  `x86_64-pc-windows-msvc` and fails CI's `-D warnings` clippy gate there. Verified
  `begin_submission` needs no such allow — its only non-test caller
  (`src/app/remote_mount.rs:133`) is not `cfg`-gated.
  Evidence: `grep -n "resolve_pending_target\|begin_submission" src/app/**/*.rs` — only the two
  `#[cfg(unix)]` handlers and cfg(unix) tests call `resolve_pending_target`.
  Reversibility: trivial, single attribute + comment.

- What: adversarial review fix (F2) — `handle_federation_mount_ready`'s two early-return branches
  (already-mounted conflict, materialize failure) now call `resolve_pending_target` and set
  `remote_mount.error` before returning (`src/app/api/workspaces.rs`), instead of leaving the
  dialog on " mounting…" forever with no recorded reason.
  Why: those branches previously only `tracing::warn!`ed; a target that hit either one had no
  user-facing signal at all until the whole submission timed out or the user gave up and hit Esc.
  Evidence: new test `federation_mount_ready_already_mounted_conflict_surfaces_an_error_instead_of_hanging`
  (`src/app/api/workspaces.rs`) exercises the conflict branch end-to-end; the materialize-failure
  branch is the identical one-line pattern, not separately unit-tested (no cheap deterministic way
  to force `materialize_federation_mount` to fail without fabricating internal error injection).
  Reversibility: trivial, revert the two added blocks.

- What: adversarial review fix (F3) — (a) `render_remote_mount_overlay` (`src/ui/remote_mount.rs`)
  now renders `remote_mount.error` ahead of `submitting` instead of `submitting` shadowing it in an
  `else if`; (b) Backspace/Char handlers (`src/app/remote_mount.rs`) no longer clear `error` while
  `submitting` is true.
  Why: with 2+ targets, an already-failed target's message was invisible until the last target
  resolved, and typing during that window silently erased the recorded failure so the dialog could
  close as a clean success even though one target actually failed.
  Evidence: `cargo test --bin herdr remote_mount app::api::workspaces -- --test-threads=4` → all
  pass; `federation_mount_partial_outcome_one_ready_one_failed_stays_open_with_error` (pre-existing)
  still covers the state; no new render-content test added (existing render tests only check rect
  geometry, matching this file's existing test scope).
  Reversibility: trivial, revert the `if`/`else if` swap and the two `submitting` guards.

- What: adversarial review fix (F4) — replaced the vacuous
  `federation_mount_ready_after_dialog_dismissed_does_not_resurrect_it` (seeded `remote_mount =
  None`, asserted `is_none()`) with
  `federation_mount_ready_for_a_dismissed_target_does_not_mutate_a_newer_dialog`: dismiss host-a's
  dialog, open a fresh one for host-b, feed host-a's stale `FederationMountReady`, assert host-b's
  dialog is untouched (`Some`, still submitting, no error, `pending == [host-b]`).
  Why: the old test's `remote_mount` was already `None`, so it stayed green even with the entire
  correlation block deleted (proved a claim ("doesn't resurrect") nothing was testing).
  Evidence: mutation-tested two ways — (1) deleting the entire correlation block leaves the new
  test passing too (logically inert: an absent block also never touches an unrelated dialog, so
  this specific deletion cannot discriminate this invariant — documented here rather than hidden);
  (2) the discriminating mutation — replacing the guarded correlation with an unconditional
  `close_remote_mount_dialog()` — makes the new test fail with `panicked at
  src/app/api/workspaces.rs:1485:14: host-a's stale outcome must not touch host-b's dialog`;
  restoring the guard makes it pass again. Full `app::api::workspaces` suite (29 tests) green after
  restore.
  Reversibility: trivial, restore the old test body if this scenario is unwanted.

- What: adversarial review fix (F5) — `enter_is_a_noop_while_a_submission_is_in_flight`
  (`src/app/remote_mount.rs`) now uses a *different* target in `name_input`
  ("different-host") than `pending` ("already-mounted-host"), is `#[tokio::test]`, and pre-seeds a
  mirror for "different-host" so an unguarded `begin_submission` hits the synchronous
  already-mounted-conflict ack (no real ssh child spawned).
  Why: the old test put the same string in both fields, so deleting the guard reproduced an
  identical post-state — it only failed by accident via a `tokio::spawn` panic outside a runtime,
  not via its own assertions.
  Evidence: mutation-tested — deleting the guard's `if ... return;` block makes the test fail with
  `assertion 'left == right' failed / left: ["different-host"] / right: ["already-mounted-host"]`;
  restoring the guard makes it pass. Full `remote_mount` suite (17 tests) green after restore.
  Reversibility: trivial, revert to the pre-fix test body if this scenario is unwanted.
