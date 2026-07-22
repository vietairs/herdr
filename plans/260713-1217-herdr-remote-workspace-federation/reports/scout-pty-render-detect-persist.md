# Scout: PTY/render/detect/persist — feasibility for remote-workspace federation

Area: PTY/terminal streaming, rendering, agent detection, persistence/resume.
Scope: read-only, no edits made.

## 0. Prior art already in the tree (changes the framing)

herdr already ships `herdr --remote user@ip` (`src/remote.rs`, `src/remote/unix.rs`, 3099 lines) and a full
client/server split (`src/client/mod.rs`, `src/server/handoff.rs`, `src/protocol/wire.rs`). This is NOT the
federation the goal describes — it's a **whole-session takeover**, not a per-workspace mount:

- `run_remote()` (`src/remote/unix.rs:155-192`) SSHes in, ensures a herdr *server* binary is running remotely
  (`ensure_remote_server_ready`), opens an `SshStdioBridge` that forwards a **local Unix socket to the remote
  server's client socket over the SSH connection**, then runs a normal local **client** process
  (`run_client_process`) against that forwarded socket exactly as it would against a local server.
- The wire protocol is whole-screen, not per-pane: `ServerMessage::Frame(FrameData)`
  (`src/protocol/wire.rs:599-612`) carries one composed ratatui `Buffer` diff per frame — the *server* renders
  the entire UI (sidebar + panes) and ships pixels; the client (`src/client/mod.rs`, `render_ansi::BlitEncoder`)
  is a dumb blitter with no local pane/terminal state of its own.
- Consequence: today's client cannot composite "one remote workspace + N local workspaces" in a single UI,
  because a server only ever emits its own full-screen buffer, and a client only ever attaches to one server.
  Federating at the workspace level is **not** a small extension of `--remote` — it needs either (a) new
  protocol work to fetch/composite sub-region frames from a second upstream server, or (b) real remote-backed
  `Pane`s living in the *local* server process, feeding local `Pane`/`Terminal` state from an SSH byte stream
  instead of a local PTY. The rest of this report evaluates (b), since it's what reuses the most existing code
  and matches "panes' real PTYs on the remote host, streamed to local UI."

## 1. PTY spawn → terminal emulator → render boundary

- `spawn_with_portable_pty` (`src/pty/backend/unix.rs:12-42`) uses `portable_pty::native_pty_system()`, opens a
  PTY, spawns `CommandBuilder`, and returns `SpawnedPty { master_fd: OwnedFd, child }` — a raw duplicated,
  close-on-exec fd (`src/pty/fd.rs:41-48`).
- `PtyIoActor::spawn` (`src/pty/actor/unix.rs:66-231`) owns that `master_fd` and drives it with `libc::poll()`
  on the raw fd plus a wake-pipe (`src/pty/fd.rs:127-195`, `poll_pty_and_wake`). This actor is **fd-and-poll
  based, not a generic `AsyncRead`/stream abstraction** — it is Unix-syscall-bound by construction.
- The actor's `on_read: FnMut(&[u8]) -> PtyReadResult` closure is constructed in `src/pane.rs` and is the real
  boundary: it calls `terminal.process_pty_bytes(pane_id, shell_pid, bytes, &response_writer)`
  (`src/pane.rs:1722-1755` normal spawn path; `src/pane.rs:1880-1913` for handoff-imported panes) then triggers
  render/detection bookkeeping. `process_pty_bytes` (`src/pane/terminal.rs:183`, `:1034`) feeds a
  `crate::ghostty::Terminal` (libghostty-backed emulator) that owns the grid, cursor, scrollback, etc.
- **Answer:** yes, terminal emulation is separable from the PTY source *in principle* — `process_pty_bytes`
  only wants `&[u8]`, it doesn't care where they came from. But the concrete `PtyIoActor` that currently drives
  it is hard-wired to a local raw fd via `poll(2)`. Feeding bytes from SSH means either (a) writing a new I/O
  actor variant that reads from an SSH channel/`tokio::process::Child` stdout and calls the same `on_read`
  closure, or (b) the zero-code-change shortcut: pass `CommandBuilder::new("ssh").args(["-tt", target, remote_cmd])`
  into the *existing* `spawn_with_portable_pty` — this already gives you "a pane whose real bytes originate on
  a remote host, streamed through a local PTY into the existing ghostty emulator," with **no PTY-layer code
  changes**. That covers a single remote shell pane, not a federated remote *workspace* with its own persisted
  panes/agents/detection running server-side.

## 2. Agent detection

Detection combines two independent signals, only one of which is host-portable:

- **Screen-text pattern matching (portable):** `terminal.detection_text()` (`src/pane/terminal.rs:1668-1674`)
  reads the ghostty grid's tail text via `ghostty_detection_text(&core)`; `crate::detect::detect_agent_with_osc`
  (`src/detect/mod.rs:204-227`) matches it plus OSC title/progress strings against per-agent manifests
  (`src/detect/manifest.rs`). This only touches the local terminal-grid object — it works identically whether
  the grid was fed by a local PTY or by SSH-relayed bytes, since it's downstream of `process_pty_bytes`.
- **Foreground-process inspection (NOT portable):** `spawn_basic_detection_task` (`src/pane.rs:540-780`) also
  calls `crate::detect::foreground_process_group_id(pid)` / `probe_foreground_process` (`src/detect/mod.rs:266`,
  `src/pane.rs:620-692`), which delegate to `src/platform/{linux,macos,windows}.rs` — real OS process-table
  introspection (`/proc/{pid}/stat` parsing confirmed at `src/detect/mod.rs:1190`, plus macOS
  `sysctl`/`libproc` and Windows equivalents). This *requires* a PID that exists in the same machine's process
  table as herdr. It's used to identify which agent CLI owns the foreground job and to detect process exit —
  it is the strongest, lowest-latency signal (`should_check_process`, `probe_foreground_process`).
- **Answer:** detection is **not** currently "the agent integration writes to the server" — it is local
  screen-scraping + local process inspection, both driven from inside the pane's own detection task. For a
  remote pane, screen-text matching can run unmodified once the (SSH-sourced) bytes have populated a local
  ghostty grid. Foreground-process inspection cannot run locally against a remote PID — it would need to run
  *on the remote host* and be relayed (e.g., over the SSH channel or a lightweight side protocol) as a
  substitute for `probe_foreground_process`'s return value. This is a real gap, not just relaying a
  ready-made "agent status" value — herdr has no such value today; it derives status from process introspection
  + screen text, both currently local-only in their current call sites.

## 3. Persistence / resume — two separate mechanisms, one is hard-blocked on locality

- **Warm handoff (in-place binary self-update, same host, same process tree):**
  `src/handoff_runtime.rs` + `src/server/handoff.rs`. `ImportedHandoffRuntime.master_fd: RawFd`
  (`src/handoff_runtime.rs:34-39`) is passed **by `SCM_RIGHTS` ancillary data over a `UnixStream`**
  (`send_fds`/`recv_fds`, `src/server/handoff.rs:377-450`, uses `libc::SCM_RIGHTS` directly). `Pane::from_handoff_fd`
  (`src/pane.rs:1653-1796`) reconstructs the `Pane` (ghostty terminal + `PtyIoActor`) around that raw fd, seeded
  from `HandoffRuntimeState` (rows/cols/keyboard-protocol/title/`initial_history_ansi`). **This is fundamentally
  local-machine-only** — POSIX fd passing cannot cross a network boundary, full stop. A remote-backed pane could
  never participate in this handoff path as-is; a herdr self-update on the local host would have to either drop
  remote panes from the live-fd handoff and instead reconnect them via SSH/reconnect-on-restore, or the *remote*
  server would need its own independent handoff for its own local processes (which it already has, being a full
  herdr server) while the local↔remote link is simply re-established.
- **Cold session snapshot (crash recovery / fresh launch):** `src/persist/snapshot.rs`. `PaneSnapshot`
  (`:97-108`) stores `cwd`, `label`, `agent_name`, `agent_session: Option<PaneAgentSessionSnapshot>`
  (`:110-116`), `launch_argv` — no fd, no live handle. `PaneHistorySnapshot` (`:118-120`) stores scrollback as
  plain **ANSI text**, not a live buffer — fully portable. Restore respawns via `agent_resume::plan()`
  (`src/agent_resume.rs:115-...`), which just builds an `argv` like `["claude", "--resume", session_id]per
  agent (`:120-150`) — pure data, no assumption about *where* it spawns. This path already generalizes cleanly:
  a `WorkspaceSnapshot`/`PaneSnapshot` extended with a remote-host field, and a restore path that re-establishes
  the SSH channel (or asks the remote herdr server to resume its own pane/agent session) instead of calling
  `spawn_with_portable_pty` locally, is a natural fit for this existing data model.
- **Answer:** the crash-recovery snapshot model (data-only, respawn-based) survives a local restart with no
  special-casing needed beyond adding remote-target metadata and a remote-aware restore branch. The warm-handoff
  fd-passing model is the one true hard blocker — it assumes local PTY ownership at the OS level and cannot be
  bridged.

## 4. Rendering

- `Pane::render(&self, frame: &mut Frame, area: Rect, show_cursor: bool)` (`src/pane.rs:2493`) renders off
  `self.terminal: Arc<PaneTerminal>` (`src/pane.rs:543, 907`) — the same ghostty-backed object populated by
  `process_pty_bytes`, constructed identically in the handoff path (`:1705`) and the fresh-spawn path (`:1832`).
  Rendering has **no dependency on how the grid was fed** — it just reads grid/cursor state out of the `Mutex`
  guarded `core` inside `PaneTerminal` (`ghostty_visible_text`/`ghostty_visible_ansi`/etc., `src/pane/terminal.rs:1652-1674`).
- **Obstacle:** none at the `Pane`-render level. The obstacle described in §0 is one level up — the existing
  `--remote` client/server protocol renders and ships a *whole composed screen* (`FrameData`) rather than
  per-pane state, so *that* path can't be reused for compositing one remote workspace's panes next to local
  ones. But if a remote-backed `Pane` is instantiated locally (per §1's SSH-CommandBuilder or a custom SSH
  I/O actor) and its ghostty grid is fed by SSH-relayed bytes, `Pane::render` works completely unmodified —
  it was already decoupled from the byte source.

## 5. Feasibility rating

| Slice | Invasiveness | Why |
|---|---|---|
| PTY byte source (single remote shell pane via `ssh -tt` as the spawned command) | **small** | Zero PTY-layer changes; `spawn_with_portable_pty` already accepts any `CommandBuilder`. |
| PTY byte source (proper SSH-channel actor, not shelling out to `ssh`) | **medium** | New `PtyIoActor`-equivalent that reads an SSH channel instead of polling a raw fd; reuses the same `on_read`/`process_pty_bytes` contract. |
| Rendering | **small** | `Pane::render` already source-agnostic; no changes needed once grid is populated. |
| Agent detection — screen text | **small** | Already source-agnostic (grid-based). |
| Agent detection — process introspection | **medium-large** | Must move `foreground_process_group_id`/`probe_foreground_process` execution to the remote host and relay results (new small protocol), since it's OS-syscall-bound (`/proc`, `sysctl`, Win32 APIs) and cannot observe a remote PID. |
| Persistence — cold snapshot/resume | **small-medium** | Data model (`PaneSnapshot`, `agent_resume::plan`) is already location-agnostic; needs a remote-target field and a restore branch that reconnects/relaunches over SSH instead of `spawn_with_portable_pty`. |
| Persistence — warm handoff (fd passing) | **large / architecturally blocked** | `SCM_RIGHTS` over `UnixStream` is local-machine-only; remote panes cannot participate. Requires either exclusion from local handoff (reconnect after restart) or delegating that responsibility entirely to the remote server's own (already-existing) handoff. |
| Workspace-level federation matching the literal goal ("mount as new workspace, panes' real PTYs on remote host") | **large** | Requires building genuine remote-backed `Pane`s server-side (custom I/O actor + relayed process-detection signal + remote-aware snapshot/restore), *not* reuse of the existing `--remote` whole-screen client/server protocol, which operates at a different granularity (§0) and would need separate protocol extensions to composite per-workspace frames from a second server if that path were chosen instead. |

**Hardest assumption of local-PTY ownership:** the `SCM_RIGHTS` fd-passing handoff (`src/server/handoff.rs`)
and the `poll(2)`-driven `PtyIoActor` (`src/pty/actor/unix.rs`, `src/pty/fd.rs`) are the two places that
genuinely assume a same-host raw OS file descriptor. Everything downstream of `on_read`/`process_pty_bytes`
(terminal grid, rendering, screen-based detection, cold-snapshot persistence) is already byte-source-agnostic
by construction and would need little to no change.

## Unresolved questions

- Does the intended federation model want remote panes rendered via genuine local `Pane` objects (reusing
  ghostty/detection/render as analyzed above), or via extending the existing whole-screen `--remote` Frame
  protocol to be per-workspace-composable? These are very different amounts of work; §0 suggests the former
  is far cheaper given what already exists.
- For agent-status relay from a remote host, is a full remote herdr *server* assumed to be running there
  (as `--remote` already requires today), or a lighter-weight agent (no server, just SSH-exec'd `/proc` probes)?
  This changes whether the process-introspection gap in §2 is solved by "ask the remote herdr server" (near
  free, it already has this code) vs. building new remote probing.

Status: DONE
