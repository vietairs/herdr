//! P9.2b b2: the in-proc federated session runner.
//!
//! `run_federated_session` is the local counterpart to `main.rs`'s classic
//! `run_app_session`: it dials a live federation tunnel (b1), mounts it under
//! timeouts (P4 `FederationClient`), materializes the remote mirror into a
//! view-only `App` (`App::new_federated` + `materialize_federation_mount`),
//! and drives the one mount channel while the App renders — until the user
//! quits or the tunnel faults (D2 fail-fast: exit to shell, no remount, no
//! classic fallback once terminal mode is entered).
//!
//! Everything here is DORMANT: nothing calls `run_federated_session` until b3
//! flips `run_remote`'s federated arm onto it. The classic path is untouched.
//!
//! Concurrency shape (the materialize-then-move-router model, which dodges the
//! option-a `Arc<Mutex>` deadlock): `materialize_federation_mount` populates
//! the App model AND `router.output_senders` AND queues the outbound `Open`s
//! into `out_tx`'s unbounded buffer; the writer task drains those to the
//! remote; then `router` + `reader` + `mirror` MOVE into ONE `drive_mount_
//! channel` task. No shared mutex — a clean ownership handoff. The drive task
//! routes inbound `Output` back through `router` into the App pane receivers
//! that `open_terminal` handed out during materialization.
//!
//! Dormant until b3: `run_federated_session` has no live caller yet, so the
//! whole module (bar `federated_session_active`, read by the PTY backstop) is
//! dead code — allowed module-wide rather than per item, matching the b1/P9
//! "additive, wired at the flip point" precedent.
#![allow(dead_code)]

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};

use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::execute;

use crate::app::{App, Mode};
use crate::config::Config;
use crate::remote::federation::client::{
    drive_mount_channel, spawn_mount_writer, FederationClient, TerminalChannelRouter,
};
use crate::remote::federation::id::HostKey;
use crate::remote::federation::protocol::{Capability, ClipboardMessage};
use crate::remote::{
    dial_federation, ManagedSshOptions, RemoteHerdr, FEDERATION_CONNECT_TIMEOUT,
    FEDERATION_MOUNT_TIMEOUT,
};

/// Inbound (remote -> local) clipboard sink bound. Bytes-only v1 (M9): the
/// receiver is held and dropped, never applied — matches the client-side
/// bounded budget so a flooding remote can't grow memory without bound.
const INBOUND_CLIPBOARD_CAPACITY: usize = 64;

/// Upper bound on how long teardown waits for the mount writer task to drain
/// after `out_tx` is dropped, before killing the ssh child regardless. Bounds
/// the fault case (a half-open peer that stopped reading) so teardown can never
/// hang and strand the user in the alt-screen; the clean case exits far sooner.
const FEDERATION_TEARDOWN_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Process-global: true only while an in-proc federated session is running.
/// Read by the low-level local-PTY spawn choke (`pty::backend::unix::
/// spawn_with_portable_pty`) as a last-resort backstop against any local-pane
/// creation path that bypasses the API mutation allowlist. Set by the RAII
/// guard below at session start, cleared on its `Drop` (incl. panic).
static FEDERATED_SESSION_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Sets the process-global federated-session flag. Only the RAII guard calls
/// this; kept `pub(crate)` for the guard + any future direct arming.
pub(crate) fn set_federated_session_active(active: bool) {
    FEDERATED_SESSION_ACTIVE.store(active, Ordering::Release);
}

/// True while an in-proc federated session is active (see the spawn backstop).
pub(crate) fn federated_session_active() -> bool {
    FEDERATED_SESSION_ACTIVE.load(Ordering::Acquire)
}

/// Arms `FEDERATED_SESSION_ACTIVE` for its lifetime; clears it on `Drop` (incl.
/// unwind) so the local-spawn backstop can never stay latched past the session.
struct FederatedSessionActiveGuard;

impl FederatedSessionActiveGuard {
    fn arm() -> Self {
        set_federated_session_active(true);
        Self
    }
}

impl Drop for FederatedSessionActiveGuard {
    fn drop(&mut self) {
        set_federated_session_active(false);
    }
}

/// Armed immediately after `ratatui::init()`; its `Drop` runs the exact TTY
/// restore sequence `main.rs` runs unconditionally on any `run()` return —
/// here it must fire on BOTH the app-quit and tunnel-wins `select!` branches
/// AND on panic. Declared before the App/tunnel so it drops LAST (TTY restore
/// is the final teardown step, after the App and ssh child are gone).
struct TerminalRestoreGuard {
    /// Whether host xterm modifyOtherKeys was enabled and must be reset.
    reset_modify_other_keys: bool,
}

impl Drop for TerminalRestoreGuard {
    fn drop(&mut self) {
        if self.reset_modify_other_keys {
            let _ = io::stdout().write_all(b"\x1b[>4;0m");
            let _ = io::stdout().flush();
        }
        if crate::kitty_graphics::is_enabled() {
            let _ = crate::kitty_graphics::clear_all_host_graphics();
        }
        let _ = pop_keyboard_enhancement_flags();
        let _ = execute!(
            io::stdout(),
            DisableFocusChange,
            DisableBracketedPaste,
            DisableMouseCapture
        );
        let _ = crate::terminal_modes::clear_host_mouse_reporting(&mut io::stdout());
        let _ = set_host_color_scheme_reports(false);
        ratatui::restore();
    }
}

fn push_keyboard_enhancement_flags() -> io::Result<()> {
    execute!(
        io::stdout(),
        PushKeyboardEnhancementFlags(crate::input::ime_compatible_keyboard_enhancement_flags())
    )
}

fn pop_keyboard_enhancement_flags() -> io::Result<()> {
    execute!(io::stdout(), PopKeyboardEnhancementFlags)
}

fn set_host_color_scheme_reports(enabled: bool) -> io::Result<()> {
    let sequence = if enabled {
        crate::terminal_theme::HOST_COLOR_SCHEME_REPORT_ENABLE_SEQUENCE
    } else {
        crate::terminal_theme::HOST_COLOR_SCHEME_REPORT_DISABLE_SEQUENCE
    };
    io::stdout().write_all(sequence.as_bytes())?;
    io::stdout().flush()
}

/// Local-capability set advertised to the federation host — identical to the
/// one-shot `attempt_federation_mount` snapshot dial (P4).
fn local_capabilities() -> std::collections::BTreeSet<Capability> {
    [
        Capability::new(Capability::SCROLLBACK_REPLAY),
        Capability::new(Capability::AGENT_STATUS),
        // Advertised unconditionally on the mounting side: this side only
        // sends bytes and reads back a path, it touches no filesystem, so it
        // has nothing to gate on the local platform. Whether a stage request
        // is ever sent is decided by what the *host* also advertised.
        Capability::new(Capability::FILE_STAGING),
    ]
    .into_iter()
    .collect()
}

/// Live outcome of dialing + mounting a federation target: everything the
/// caller needs to materialize the mount into an `App` and drive it, with no
/// terminal/TTY ownership assumed. Extracted out of `run_federated_session`
/// (REVISED Phase A step 3) so the server daemon's own async task can reuse
/// exactly this dial+mount sequence without pulling in any of
/// `run_federated_session`'s terminal-mode setup.
pub(crate) struct DialAndMountOutcome {
    pub(crate) mirror: crate::remote::federation::reducer::RemoteMirror,
    pub(crate) generation: u64,
    pub(crate) tunnel_guard: crate::remote::ChildGuard,
    pub(crate) tunnel_reader: tokio::process::ChildStdout,
    pub(crate) tunnel_writer: tokio::process::ChildStdin,
}

/// Dials `target`'s live herdr server and mounts a federation session over
/// it, applying the same connect/mount timeouts and empty-mirror rejection
/// `run_federated_session` always has. No `App`, no TTY — pure async I/O,
/// `Send`-shaped, safe to `tokio::spawn` from any tokio context (server
/// daemon or CLI process alike).
pub(crate) async fn dial_and_mount(
    target: &str,
    remote_herdr: &RemoteHerdr,
    session_name: &str,
    ssh_options: Option<&ManagedSshOptions>,
) -> io::Result<DialAndMountOutcome> {
    let tunnel = dial_federation(target, remote_herdr, session_name, ssh_options)
        .await
        .map_err(|err| io::Error::other(format!("federation dial failed: {err:?}")))?;
    let crate::remote::LiveTunnel {
        guard: tunnel_guard,
        reader,
        writer,
    } = tunnel;

    let client = FederationClient::new(
        HostKey::new(target, session_name),
        local_capabilities(),
        std::collections::BTreeSet::new(),
    );

    // Wrap `connect_and_mount`'s otherwise-unbounded reads: a live tunnel
    // (unlike the one-shot snapshot dial) can hang forever on a stalled
    // link. Use the connect budget for the whole handshake+mount round — it
    // is the tighter of the two; mount is bounded by the same await.
    let mount_budget = FEDERATION_CONNECT_TIMEOUT + FEDERATION_MOUNT_TIMEOUT;
    let mounted =
        match tokio::time::timeout(mount_budget, client.connect_and_mount(reader, writer)).await {
            Ok(Ok(mounted)) => mounted,
            Ok(Err(err)) => {
                return Err(io::Error::other(format!("federation mount failed: {err}")));
            }
            Err(_elapsed) => {
                return Err(io::Error::other(
                    "federation mount timed out before a live workspace was ready",
                ));
            }
        };

    // Non-empty required: an empty mount has nothing to render — abort so
    // the caller falls back to classic rather than entering an empty TUI.
    if mounted.mirror.workspaces().is_empty() {
        return Err(io::Error::other(
            "federation mount returned no remote workspaces",
        ));
    }

    let mirror = mounted.mirror;
    let generation = mirror.mount().mount_generation;
    Ok(DialAndMountOutcome {
        mirror,
        generation,
        tunnel_guard,
        tunnel_reader: mounted.reader,
        tunnel_writer: mounted.writer,
    })
}

/// Runs an interactive in-proc federated session against `target`'s live herdr
/// server. Mirrors `main.rs`'s classic runtime + terminal template.
///
/// Returns `Err` ONLY on a pre-run failure (dial / handshake+mount timeout or
/// error / empty mirror) — before terminal mode is entered — so b3 can fall
/// back to the classic attach. Once terminal mode is entered, every exit
/// (user quit, tunnel fault, link close) returns `Ok(())`: D2 fail-fast exits
/// to the shell.
pub(crate) fn run_federated_session(
    target: &str,
    remote_herdr: &RemoteHerdr,
    session_name: &str,
    ssh_options: Option<&ManagedSshOptions>,
    config: &Config,
    config_diagnostic: Option<String>,
) -> io::Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|err| io::Error::other(format!("failed to build federated runtime: {err}")))?;

    let result = rt.block_on(async {
        // --- Pre-run (classic-fallback-eligible): dial + mount, no TTY yet. ---
        let _active_guard = FederatedSessionActiveGuard::arm();

        let DialAndMountOutcome {
            mirror,
            generation,
            tunnel_guard,
            tunnel_reader,
            tunnel_writer,
        } = dial_and_mount(target, remote_herdr, session_name, ssh_options).await?;

        // --- Terminal mode (D2: from here every exit returns Ok, no fallback). ---
        let modify_other_keys_mode = crate::input::host_modify_other_keys_mode();
        let mut terminal = ratatui::init();
        // Guard armed the instant the alt-screen is up so the restore fires on
        // any early `?` below, on both `select!` branches, and on panic.
        let _terminal_guard = TerminalRestoreGuard {
            reset_modify_other_keys: modify_other_keys_mode.is_some(),
        };
        crate::terminal_modes::clear_host_mouse_reporting(&mut io::stdout())?;
        if config.ui.mouse_capture {
            execute!(io::stdout(), EnableMouseCapture)?;
        } else {
            execute!(io::stdout(), DisableMouseCapture)?;
        }
        execute!(io::stdout(), EnableBracketedPaste, EnableFocusChange)?;
        set_host_color_scheme_reports(true)?;
        push_keyboard_enhancement_flags()?;
        if let Some(mode) = modify_other_keys_mode {
            io::stdout().write_all(mode.set_sequence())?;
            io::stdout().flush()?;
        }

        // API request channel. b2 does NOT stand up a local API socket server
        // for the federated session — whether it exposes its own socket is a b3
        // integration decision (a client-bridge context may already own the
        // local socket). `app.run` drains the receiver; the sender is held so
        // the channel never reports all-senders-dropped.
        let (_api_tx, api_rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::api::ApiRequestMessage>();

        // The App: view-only, never persists the classic session.
        let event_hub = crate::api::EventHub::default();
        let mut app = App::new_federated(config, config_diagnostic, api_rx, event_hub.clone());

        // Writer first: draining `out_rx` so the `Open`s materialize queues
        // below flush to the remote immediately (eager-open; the queued Opens
        // sit in the unbounded `out_tx` buffer until this task drains them).
        let (out_tx, writer_handle) = spawn_mount_writer(tunnel_writer);

        // Two clipboard sinks (both dropped on teardown, M9 bytes-only v1):
        //  - inbound (remote -> local) BOUNDED: fed by the drive task; rx held.
        //  - outbound (local -> remote) UNBOUNDED: threaded into spawn_remote
        //    by materialize; its rx has no live forwarder in v1 (held/dropped).
        let (inbound_clip_tx, _inbound_clip_rx) =
            tokio::sync::mpsc::channel::<ClipboardMessage>(INBOUND_CLIPBOARD_CAPACITY);
        let (outbound_clip_tx, _outbound_clip_rx) =
            tokio::sync::mpsc::unbounded_channel::<ClipboardMessage>();

        let mut router = TerminalChannelRouter::new();
        // Eager-open: populates the App model + `router.output_senders` and
        // queues one outbound `Open` per pane into `out_tx` (flushed by the
        // writer task). `router` moves into the drive task right after.
        let opened =
            app.materialize_federation_mount(&mirror, &mut router, &out_tx, &outbound_clip_tx)?;

        // Land the user in the first materialized workspace, interactively.
        if let Some(&first_ws_idx) = opened.first() {
            app.state.active = Some(first_ws_idx);
            app.state.selected = first_ws_idx;
            app.state.mode = Mode::Terminal;
        } else {
            app.state.mode = Mode::Navigate;
        }

        // ONE drive task owns reader + mirror + router + inbound clipboard tx.
        // Spawned before awaiting `app.run` so inbound replay/output for the
        // just-flushed Opens is consumed live (the generation fence inside
        // `drive_mount_channel` keeps a superseded mount from mutating a newer
        // mirror — moot in v1, which never remounts).
        // Cloned (not moved): `out_tx` itself is dropped further below, after
        // this drive task ends, to signal the writer task to exit (see the
        // teardown comment at that `drop(out_tx)` call).
        let drive_out_tx = out_tx.clone();
        let drive_outbound_clip_tx = outbound_clip_tx.clone();
        let mut drive = tokio::spawn(async move {
            let mut reader = tunnel_reader;
            let mut mirror_task = mirror;
            let mut router_task = router;
            drive_mount_channel(
                &mut reader,
                &mut mirror_task,
                generation,
                &event_hub,
                &mut router_task,
                &inbound_clip_tx,
                &drive_out_tx,
                &drive_outbound_clip_tx,
                // The classic full-screen `--remote` session is a standalone
                // client process, not the server-owned materialization path
                // this follow-up wires (`app/api/workspaces.rs`); a remote
                // split here still logs, matching the prior behavior.
                None,
            )
            .await
        });

        // Supervision: whichever of the App and the tunnel ends first wins;
        // the other is torn down. Any drive outcome ends the session (D2).
        let app_result: io::Result<()>;
        tokio::select! {
            r = app.run(&mut terminal) => {
                app_result = r;
                drive.abort();
                let _ = drive.await;
            }
            outcome = &mut drive => {
                match outcome {
                    Ok(Ok(drive_outcome)) => {
                        tracing::info!(?drive_outcome, "federated tunnel ended; exiting to shell");
                    }
                    Ok(Err(err)) => {
                        tracing::warn!(%err, "federated tunnel I/O error; exiting to shell");
                    }
                    Err(join_err) => {
                        tracing::warn!(%join_err, "federated drive task aborted/panicked; exiting");
                    }
                }
                app_result = Ok(());
            }
        }

        // Teardown RAII order: App scope has ended (its `run` future is done or
        // cancelled) → drop App (remote-backed panes/workspaces) → drop out_tx
        // (ends the writer loop) → drain the writer under a bounded timeout →
        // drop the ChildGuard (kills the ssh child) → the terminal-restore
        // guard runs LAST at block end. `_inbound_clip_rx`/`_outbound_clip_rx`
        // drop here too.
        drop(app);
        drop(out_tx);
        // On a clean exit the peer is still reading, so once `out_tx` is dropped
        // the writer flushes and exits well within this cap. On a HALF-OPEN peer
        // (stopped reading), the writer's `write_all`/`flush` would block
        // forever — so bound the drain and then kill the ssh child regardless
        // (dropping `tunnel_guard`), which also breaks any still-pending write.
        // Teardown must NEVER hang on an unreadable peer: that would strand the
        // user in the alt-screen (the `TerminalRestoreGuard` never drops).
        let _ = tokio::time::timeout(FEDERATION_TEARDOWN_DRAIN_TIMEOUT, writer_handle).await;
        drop(tunnel_guard);

        app_result
    });

    // Kill the runtime immediately — matches the classic path's teardown, so
    // any lingering federation/PTY tasks cannot outlive the session.
    rt.shutdown_timeout(std::time::Duration::from_millis(100));
    result
}
