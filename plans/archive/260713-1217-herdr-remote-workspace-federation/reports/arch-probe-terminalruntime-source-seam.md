# Arch probe: TerminalRuntime I/O transport seam for remote-backed panes

Read-only. No source modified.

## 1. `TerminalRuntime` definition — hard-wired to local PTY, no transport abstraction

`src/terminal/runtime.rs:15`:

```rust
pub struct TerminalRuntime(crate::pane::PaneRuntime);
```

A single-field newtype tuple struct wrapping `crate::pane::PaneRuntime` — not a trait object, not an enum over transports. Every method on `TerminalRuntime` (`resize`, `send_bytes`, `try_send_bytes`, `cwd`, `child_pid`, handoff methods, etc.) is a 1:1 delegating wrapper to `self.0` (`src/terminal/runtime.rs:17-452`). The doc comment at `src/terminal/runtime.rs:10-14` confirms this is deliberate/known debt: *"The PTY implementation still delegates to the legacy pane runtime while the migration proceeds, but production code now depends on this terminal-layer type instead of the pane module's implementation detail."*

The real implementation is `PaneRuntime` (`src/pane.rs:905-921`):

```rust
pub struct PaneRuntime {
    pane_id: PaneId,
    terminal: Arc<PaneTerminal>,
    io: PaneRuntimeIo,
    current_size: Cell<(u16, u16, u32, u32)>,
    child_pid: Arc<AtomicU32>,
    ...
}

enum PaneRuntimeIo {
    Actor(PtyIoActorHandle),
    #[cfg(test)]
    TestChannel { sender: mpsc::Sender<Bytes>, resize_tx: watch::Sender<(u16, u16, u32, u32)> },
}
```

`PaneRuntimeIo` is the only place with variant-based indirection, and its non-test variant is `Actor(PtyIoActorHandle)` — a handle to the local-fd-based `PtyIoActor` (`src/pty/actor.rs:88-94`, cfg(unix); `src/pty/actor.rs` windows mod for `portable_pty` on Windows). There is no `TestChannel`-like production variant for a remote source; `TestChannel` only exists under `#[cfg(test)]` and is not usable in a running server.

**Byte flow in (read):** `PtyIoActor` owns `master_fd: OwnedFd` (the PTY master), spawns an OS thread (`src/pty/actor.rs:379-382`) that `poll(2)`s the raw fd and calls `self.file.read()` (`src/pty/actor.rs:627-643`), then invokes the `on_read: ReadCallback` closure (`Box<dyn FnMut(&[u8]) -> PtyReadResult + Send + 'static>`, `src/pty/actor.rs:43`). That closure is built in `PaneRuntime::spawn_command_builder` (`src/pane.rs:1876-1912`) and calls `terminal.process_pty_bytes(...)` directly — i.e. PTY-byte-to-VT-parser wiring is baked into the closure passed at construction time, not behind any read()-style trait method callers could swap.

**Byte flow out (write/input):** `PtyIoActorHandle::write_user_input` / `try_write_user_input` (`src/pty/actor.rs:102-159`) push `Bytes` onto an mpsc channel; the actor thread later `self.file.write()`s them (`src/pty/actor.rs:654-683`). `PaneRuntime::send_bytes`/`try_send_bytes` (delegated through `TerminalRuntime::send_bytes`/`try_send_bytes`, `src/terminal/runtime.rs:380-386`) forward to this handle via `PaneRuntimeIo::Actor(actor) => ...` match arms (pattern repeated at `src/pane.rs:932-1034` for every operation: shutdown, duplicate_handoff_fd, foreground_process_group_id, begin_handoff, resize, nudge_child_redraw_after_handoff).

## 2. Where `TerminalRuntime` is constructed and bound to a `Pane`

Trace: `Pane`/`PaneState` (state layer, holds `TerminalId`) → `TerminalRuntimeRegistry` (server/app layer, `HashMap<TerminalId, TerminalRuntime>`, `src/terminal/runtime_registry.rs:1-13`) → `TerminalRuntime::spawn*` → `PaneRuntime::spawn*` → `PtyIoActor::spawn`.

`TerminalRuntimeRegistry` (`src/terminal/runtime_registry.rs`) is the binding point: `get`, `insert`, `remove`, `values`, `iter`, plus PTY-specific bulk ops (`set_handoff_readers_paused`, `assume_handoff_ownership`, `nudge_child_redraw_after_handoff`, `drain_for_handoff`) that are all `#[cfg(unix)]` and PTY-handoff-specific — i.e. the registry's own API assumes a local-PTY runtime underneath (handoff = re-exec/fd-passing between herdr processes on the same host).

Every `TerminalRuntime::spawn*` constructor call site (15 total) assumes local PTY spawn semantics — `cwd: PathBuf`, `shell_config`/`launch_env`/`argv` for a **local** process, `#[cfg(unix)] master_fd` vs `#[cfg(windows)] master`:

- `src/ui/mobile.rs:1414`
- `src/app/agent_resume.rs:235`
- `src/app/actions.rs:3397`
- `src/ui/sidebar.rs:1854`
- `src/app/api.rs:509`, `:1610`, `:1702`
- `src/workspace/tab.rs:130`, `:144`, `:355`, `:368`, `:381`
- `src/server/notifications.rs:116`
- `src/persist/restore.rs:579`, `:597`

Underlying PTY spawn happens in `PaneRuntime::spawn_command_builder` (`src/pane.rs:1798-1936`ish): builds `portable_pty::CommandBuilder`, calls `crate::pty::backend::spawn_with_portable_pty(rows, cols, cmd)` (`src/pane.rs:1835`) which returns a `master_fd`/`master` + `child`, spawns a child-wait task, then `PtyIoActor::spawn(PtyIoActorConfig { master_fd: spawned.master_fd, ... })` (`src/pane.rs:1914-1922`). The local-PTY assumption is baked in at this single construction path — a local child process and its OS-level PTY master fd are prerequisites for every `TerminalRuntime`.

`TerminalId` usage: 71 occurrences across 16 files (registry, app/state, persistence, server render/attach, UI panes/navigator/sidebar) — but the vast majority just key `HashMap`/lookups; only the registry and the constructor call sites above actually touch runtime construction/PTY specifics.

## 3. Existing indirection a non-PTY byte source could plug into — none in production

**Answer: No.** The only enum-based indirection (`PaneRuntimeIo`) has a non-test PTY-only variant. `TestChannel` is `#[cfg(test)]`-gated and cannot be reached from a real server code path (no way to construct a `PaneRuntime`/`TerminalRuntime` with a `TestChannel` outside tests — no public/pub(crate) constructor takes an arbitrary sender). There is no trait (e.g. `TerminalSource`/`ByteTransport`) anywhere in `src/pty/`, `src/pane.rs`, or `src/terminal/` that abstracts read/write/resize/close.

**Minimal seam to introduce**, informed by the two operations every call site actually needs (`PaneRuntimeIo`'s match arms enumerate the full contract):

```rust
trait TerminalSource: Send {
    fn write_user_input(&self, bytes: Bytes) -> ...;      // async or channel-based, mirrors write_user_input/try_write_user_input
    fn resize(&self, rows: u16, cols: u16, cell_width_px: u32, cell_height_px: u32, terminal_responses: Vec<Bytes>);
    fn shutdown(&self);
    // Unix-only handoff surface (begin_handoff, duplicate_for_handoff, foreground_process_group_id,
    // rollback_handoff, release_after_commit, nudge_child_redraw_after_handoff) is PTY-process-handoff-specific
    // (same-host fd passing between herdr processes) and has NO remote equivalent — must stay
    // gated to the local-PTY implementor, not part of the general trait.
}
```

Read/byte-delivery is the other half: today reads arrive via the `on_read` closure captured at `PtyIoActor::spawn` time, feeding `terminal.process_pty_bytes` directly inside the actor thread (`src/pane.rs:1876-1912`). A remote source's equivalent "actor" (an async task reading a socket/ssh-relayed stream) would call the *same* closure with bytes it received over the network instead of `read(2)`-ing a fd — that part is already effectively closure-shaped and reusable in principle, since `on_read: Box<dyn FnMut(&[u8]) -> PtyReadResult + Send + 'static>` has no PTY-specific type in its signature.

**Call sites touching `TerminalId` → runtime**: ~15 constructor call sites (§2) that would need an alternate `TerminalRuntime::spawn_remote(...)`-style constructor (additive, not a rewrite of existing call sites — existing local-PTY call sites keep calling `spawn`/`spawn_argv_command`/etc.). `PaneRuntimeIo` gets one more variant. `TerminalRuntimeRegistry`'s handoff-only methods (`set_handoff_readers_paused`, `assume_handoff_ownership`, `nudge_child_redraw_after_handoff`, `drain_for_handoff` — 4 methods, all `#[cfg(unix)]`) need to no-op or skip non-PTY variants, similar to how `#[cfg(test)] TestChannel` arms already no-op today (`src/pane.rs:936-1034`) — that pattern is a direct template.

## 4. Resize/input/close: fd-shaped today, not message-shaped

- **Resize**: `PtyIoActorHandle::resize()` (`src/pty/actor.rs:161-185`) stages a `PtyResizeRequest` in a `Mutex`-guarded `SharedPtyControls` and wakes the actor thread; the actor thread applies it via `fd::resize_pty_fd` (`src/pty/actor.rs:685-693`, `src/pty/fd.rs:218`) — a **TIOCSWINSZ ioctl on the raw PTY fd**. Fd-shaped, not a serializable message today, though the call *signature* (`rows, cols, cell_width_px, cell_height_px, terminal_responses: Vec<Bytes>`) is already plain data that would serialize cleanly for a remote protocol — the ioctl is the last-mile fd-specific step, isolated to one function.
- **Input**: `write_user_input`/`try_write_user_input` take `Bytes` and enqueue for a raw `File::write()` (`src/pty/actor.rs:654-683`) — already message-shaped (`Bytes` in, no fd type in the public signature); a remote transport could reuse this exact API surface and just replace the actor's internal write target.
- **Close/shutdown**: `PtyIoActorHandle::shutdown()` sends a `PtyIoControlCommand::Shutdown` and the actor thread breaks its poll loop, closing `self.file` on drop (`src/pty/actor.rs:312-323`, `:571`) — again channel/message-shaped at the handle API, fd-specific only inside the actor's own loop.
- **Handoff-specific ops** (`begin_handoff`, `duplicate_for_handoff`, `foreground_process_group_id`, `rollback_handoff`, `release_after_commit`) are inherently local-process/fd-passing concepts (`src/pty/actor.rs:209-330`) with no remote analog — these must NOT be part of a generalized `TerminalSource` trait; they stay PTY-only.

**Verdict on shape**: the *handle-level* API (`PtyIoActorHandle`'s public methods) is already close to message-shaped — arguments are plain `Bytes`/u16/u32, no `RawFd` leaks into `write_user_input`/`resize` signatures. The fd-specific work (ioctl, raw `read`/`write`, `poll(2)`) is correctly isolated inside `PtyIoActorRunner` (the actor's private loop, `src/pty/actor.rs:396-746`), not smeared across call sites. This isolation is exactly why the trait-seam is tractable rather than requiring a deep rewrite.

## 5. `PtyIoActor` — poll(2)-on-raw-fd loop, confirmed; no reusable async/channel abstraction for sockets

Confirmed: `src/pty/actor.rs:334-394` spawns a dedicated **OS thread** (`std::thread::Builder::new().name(...).spawn(move || runner.run())`, not a tokio task) running `PtyIoActorRunner::run()` (`:418-472`), whose core loop calls `fd::poll_pty_and_wake(self.file.as_raw_fd(), self.wake_read_fd.as_raw_fd(), ...)` (`:436-441`) — a `poll(2)`/`ppoll`-style syscall wrapper in `src/pty/fd.rs:135`, over **raw fds** (`AsRawFd`/`OwnedFd`). Reads (`self.file.read()`, `:629`) and writes (`self.file.write()`, `:657`) go through `std::fs::File` wrapping the fd directly — no `tokio::net`/`AsyncRead`/`AsyncWrite` anywhere in this file.

There is a `wake_read_fd` (self-pipe trick, `fd::create_wake_pipe()`) used to interrupt the poll loop for cross-thread signaling (writes/resizes/control commands) — this is the one reusable pattern: the actor already multiplexes "data available on transport fd" and "control command pending" via poll-on-two-fds. A remote-socket-backed actor could reuse this exact *pattern* (a dedicated thread poll-looping over a socket fd + a wake pipe) with minimal conceptual change, since `poll(2)` works identically on a `UnixStream`/TCP socket fd as on a PTY master fd — but this would be a **new sibling implementation of the runner**, not a reuse of `PtyIoActorRunner` itself, because `PtyIoActorRunner` has PTY-only operations baked into its command enum (`ForegroundProcessGroup`, `DuplicateForHandoff`, ioctl resize) that a socket-backed runner should not carry.

No test evidence of a channel/async abstraction that a socket source could reuse beyond this poll-loop pattern; the Windows implementation (`src/pty/actor.rs` windows mod, lines 1-231 of that block) instead uses **blocking `std::thread::spawn` reader/writer/control threads** with `mpsc` — also not async, also fd/handle-shaped (`Box<dyn MasterPty>`), same story.

## 6. Verdict

**MEDIUM** — introduce a `TerminalSource`-style trait (write/resize/shutdown; explicitly excluding local-only handoff ops) implemented today by `PtyIoActorHandle` and, for Tier-2, by a new socket/ssh-relay-backed actor; add one `PaneRuntimeIo` variant and one additive `TerminalRuntime::spawn_remote`-style constructor; make the 4 `#[cfg(unix)]` handoff-bulk-methods on `TerminalRuntimeRegistry` skip non-PTY variants (template already exists via the `#[cfg(test)] TestChannel` no-op arms in `src/pane.rs:936-1034`).

Reasoning: `TerminalRuntime` is a bare newtype with zero existing transport abstraction (§1) and `PtyIoActor` is a raw-fd `poll(2)` thread loop (§5) — so this is not a small trait-bolt-on (S). But the actual byte-in/byte-out **handle API** is already `Bytes`-in/`Bytes`-out and free of `RawFd` leakage at the call-site boundary (§4), and the ~15 construction call sites are additive-friendly (new constructor, not rewriting existing ones) — so this is not a deep rewrite (L) either. The work concentrates in: (a) defining the trait, (b) writing one new actor implementation for the remote transport, (c) widening `PaneRuntimeIo`/`TerminalRuntimeRegistry` to tolerate a second variant. Genuinely PTY-only concerns (ioctl resize, process handoff/fd-passing, `portable_pty::CommandBuilder`/child-wait) must be explicitly excluded from the new trait and either stubbed as no-ops or made `Option`-returning for the remote variant — that scoping decision, not raw code volume, is the main design risk.

## Unresolved questions

- Whether Tier-2 remote panes need any handoff-equivalent semantics (e.g. reattaching a live SSH-relayed stream across a herdr client restart) — if yes, that is new protocol design, not covered by this probe.
- Whether resize's `terminal_responses: Vec<Bytes>` (responses written back to the PTY after an ioctl, e.g. terminal query replies) has a remote equivalent that needs round-tripping through the relay, or whether the remote host's own PTY already handles this locally and only the visual resize needs to propagate.

Status: DONE
