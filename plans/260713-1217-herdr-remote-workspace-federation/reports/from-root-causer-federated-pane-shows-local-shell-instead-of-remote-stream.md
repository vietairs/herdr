# Root-cause investigation: federated pane shows local vm100 shell instead of remote vm105 stream

## Symptom
`~/herdr-fed/herdr --remote 131.172.248.163 --remote-workspace --session fedsmoke` on vm100
renders a sidebar entry `131.172.248.163#fed...` (correct federation `HostKey` format,
id.rs), but the selected tab's pane shows a live, keystroke-responsive
`hvnguyen@bio-1-ubuntu:~$` prompt (vm100's own hostname) with two `^C` echoes, not vm105's
mirrored stream.

## Hypothesis given in the task (materialize spawns a real local pty) — DISCONFIRMED

- `App::materialize_federation_mount` (src/app/creation.rs:521-687) builds every pane via
  `build_remote_pane` (creation.rs:700-743), which calls
  `TerminalRuntime::spawn_remote` -> `PaneRuntime::spawn_remote`
  (src/terminal/runtime.rs:148-181, src/pane.rs:1763-1910).
- `PaneRuntime::spawn_remote` never calls `pane_shell_command_builder`/
  `spawn_with_portable_pty`. It pins `child_pid` at 0 for the runtime's whole lifetime
  (pane.rs:1796-1804) and wires I/O as `PaneRuntimeIo::Remote(RemoteTerminalSourceHandle::
  spawn(...))` fed only by the mount's `output_rx` (pane.rs:1864-1871). There is no code
  path from materialization to a local PTY. **Ruled out by source.**

## Hypothesis: FEDERATED_SESSION_ACTIVE guard bypassed by a normal in-app keybinding — DISCONFIRMED

- The guard (`src/pty/backend/unix.rs:12-24`) is armed for the whole
  `run_federated_session` lifetime via RAII (`FederatedSessionActiveGuard`,
  src/remote/federation/session.rs:82-95, armed at line 184, dropped only when the whole
  async block in `run_federated_session` ends).
- Every keyboard-triggered pane/tab/workspace-creation action (`NavigateAction::
  NewWorkspace` etc., src/app/input/navigate.rs:186-197) routes through
  `runtime_workspace_create` -> `dispatch_runtime_mutation` -> `dispatch_api_request`
  (src/app/runtime_mutations.rs:12-38), which is gated by `federated_mode` +
  `federated_session_allows` BEFORE any spawn is attempted (src/app/api.rs:851). Local
  key-driven mutations and socket-API mutations share one dispatch path — there is no
  separate unguarded local-only creation path. **Ruled out by source**: even if it somehow
  reached a spawn call, `spawn_with_portable_pty`'s own check would still reject it
  (defense-in-depth, unix.rs:20-24).

## What the source-only investigation could not do
No shell/log access to vm100/vm105 was available in this sandbox — everything below is a
source-supported hypothesis, not a live repro. It needs the artifacts listed at the end to
be confirmed.

## Ranked remaining hypotheses

### H1 (strongest, best source fit): D2 fail-fast "exit to shell" fired silently after the first correct render, and the observed shell is simply the restored underlying terminal, not a misrouted pane
- `run_federated_session`'s own module doc states the design explicitly: "until the user
  quits or the tunnel faults (D2 fail-fast: exit to shell, no remount, no classic fallback
  once terminal mode is entered)" (session.rs:7-9).
- Once terminal mode is entered, ANY drive-task outcome — `LinkClosed`, `Faulted(reason)`,
  or `ResyncRequired` (v1 never remounts, drive.rs comment client.rs:233-238) — takes the
  `outcome = &mut drive =>` branch of the `tokio::select!` (session.rs:333-346), which only
  `tracing::info!`/`tracing::warn!`s (to the log file, not the alt-screen) and sets
  `app_result = Ok(())`. Teardown then runs `drop(app)` -> `drop(out_tx)` -> drain ->
  `drop(tunnel_guard)` -> `TerminalRestoreGuard::drop` -> `ratatui::restore()`
  (session.rs:349-368, TerminalRestoreGuard at 102-127), and `run_remote`'s `Federated` arm
  returns `Ok(())` **with no message printed to the user** (unix.rs:513-514 — the
  `eprintln!` notice at unix.rs:516-521 only fires on the pre-terminal-mode `Err` path).
- Net effect: sidebar+workspace render correctly for one or more frames (proving mount +
  materialize genuinely worked, matching the federation-specific `HostKey` label the user
  saw), then the session exits to the shell **silently** the instant the drive task ends
  for any reason. Control returns to vm100's own real login shell in the same tty — which
  is why it is genuinely live, keystroke-responsive, and produces real `^C` echoes (it's a
  real bash process, not herdr-rendered content).
- This fits every observed fact, including the "no error, nothing pointed at vm105" feel,
  without requiring any spawn-guard bypass.
- **Not yet confirmed**: which `DriveOutcome` fired (or whether the process instead
  panicked) requires vm100's herdr log for that run (see "what would confirm" below).

### H2 (plausible, weaker): partial/failed first-frame repaint leaves the sidebar chrome painted but the content-pane region unrefreshed
- Ratatui writes screen diffs incrementally; a panic or early return mid-`terminal.draw()`
  during the very first frame (this smoke test is the first ever *live, interactive*
  exercise of `materialize_federation_mount`'s render path per
  `plans/.../reports/auto-decisions-260714-herdr-b2-federated-session-runner.md:234-253`,
  which only got a headless, no-TTY probe previously) could in principle leave some
  region unpainted. Weaker than H1 because it requires an actual panic to be identified,
  and a panic inside `app.run(&mut terminal)` (not a detached `tokio::spawn`) still
  unwinds through `_terminal_guard`'s `Drop` (session.rs:97-101, 107-127), which should
  still restore/clear the screen — so this alone doesn't cleanly explain a stable-looking
  sidebar coexisting with a live shell underneath without also invoking terminal-emulator-
  specific alt-screen-restore behavior (see H3).

### H3 (context, not a code bug): terminal-emulator alt-screen semantics
Exiting the alt screen (`ratatui::restore()`) always reveals whatever was in the *primary*
screen buffer before entry — normally the shell prompt/command line the user typed to
launch herdr. If H1's silent exit happened quickly, what the user is calling "the pane"
may literally be that restored primary buffer plus fresh live shell output, which is
indistinguishable at a glance from "the pane rendering wrong content" unless the exact
timing/screenshot sequence is known.

## Guard status answer (acceptance criterion 3)
`FEDERATED_SESSION_ACTIVE` is **neither bypassed nor irrelevant in the sense of "wrong
code path spawned a local pty"** — no source path was found where it should have fired
but didn't. The far more likely explanation is that no local pty was ever involved at all:
this looks like a **silent session-exit / terminal-handoff** issue (D2's own documented
behavior lacking a user-visible notice), not a spawn-guard failure.

## What would confirm/refute H1 vs H2
- vm100's herdr log file for the exact run (`crate::logging` output path) — look for
  `"federated tunnel ended; exiting to shell"`, `"federated tunnel I/O error"`, or
  `"federated drive task aborted/panicked"` (session.rs:336,339,342), or a panic backtrace.
- Confirm via `ps` on vm100 at the time of the screenshot whether the herdr process (the
  one launched by `~/herdr-fed/herdr --remote ...`) is still running or already exited.
- Re-run with the terminal scrollback intact and check whether the "federated" frame and
  the "local shell" frame are the same tty content at two different times, not one frame.
- Independently confirm vm105's own OS hostname (the smoke-test report at
  `auto-decisions-260714-herdr-b2-federated-session-runner.md:235-236` shows vm105 was
  freshly provisioned per this plan; if it also defaults to a generic cloud-image hostname,
  ruling out a hostname-string false-positive is good hygiene, but H1 does not depend on
  this — it explains the local-vm100 shell without needing a hostname coincidence).

## Unresolved questions
- Which exact `DriveOutcome` (or panic) ended the drive task in the live run — needs the
  vm100 log.
- Whether the eager-open burst of `Open` messages during `materialize_federation_mount`
  (creation.rs:585-681, one per pane synchronously queued) could itself race the mirror's
  generation fence and trigger `ResyncRequired`/`Faulted` on a live (not loopback) link —
  not exercised by any existing test (client.rs tests are loopback/duplex-only).
