# Blindspot Synthesis — herdr remote→local-workspace federation

Stage 1 of vibe R7 (discovery-only). Synthesis of 4 parallel architecture scouts (see sibling
`scout-*.md`). Main-loop synthesis; feeds Stage 2 brainstorm.

## Task restated
`herdr --remote ssh user@ip` should connect + start the remote herdr and mount it as a NEW
WORKSPACE inside the LOCAL herdr session, so local + remote workspaces coexist in one sidebar.
Today `--remote` is a full-screen ATTACH that replaces the local view with the remote's session.

## What the scouts resolved (unknowns → knowns)

1. **Today's `--remote` path** (`src/remote/unix.rs`): `run_remote()` → `prepare_remote_herdr()`
   (detect/install remote binary) → `ensure_remote_server_ready()` → `SshStdioBridge` (one raw
   byte pipe over ssh) → spawns a **separate `herdr client` child process** that inherits the real
   terminal. Forces `HERDR_RENDER_ENCODING=terminal-ansi`. It is session REPLACEMENT, not a
   workspace in the local server.

2. **The full-screen decision is structural, in two places**: (a) wire-level — the remote emits
   ONE composited full-screen `FrameData` cell grid, never per-pane/workspace events; (b)
   process-level — the remote client is a standalone child taking over the terminal, unaware of
   any local session.

3. **Reusable seams (cheap):** `prepare_remote_herdr` + `ensure_remote_server_ready` +
   `SshStdioBridge` are self-contained, headless-safe, no attach side effects. The terminal
   emulator boundary is clean — `on_read`/`process_pty_bytes` (`src/pane.rs:1722`,
   `src/pane/terminal.rs:183`) only want `&[u8]`; `Pane::render` only reads the ghostty grid,
   independent of byte source. A pane can be fed by `ssh -tt …` via the existing
   `spawn_argv`/`CommandBuilder` path with ZERO protocol change.

4. **Hard couplings blocking TRUE federation (expensive):**
   - Wire protocol carries a full-screen frame, not structured session deltas; nothing subscribes
     to a remote server's events and relays them into the local `EventHub` (single-source,
     single-sequence). A structured `session.snapshot` exists but no ingestion path.
   - Workspace/tab/pane IDs are per-process `AtomicU64` counters (`w1,w2…`) → **collide** across
     two servers; no id-origin/namespace field in any wire struct.
   - `PaneState {attached_terminal_id} → TerminalId → TerminalRuntime` is a hard 1:1 **local-PTY**
     binding across dozens of call sites (I/O, resize, history, detection) — no source
     abstraction.
   - Agent detection = local screen-scrape (portable, grid-based) **+ OS process-table probe**
     (`/proc`, `sysctl`, Win32 — needs PID on the same host). Remote detection must run remotely
     and be relayed.
   - Warm handoff passes the PTY master fd via `SCM_RIGHTS` over a UnixStream — **cannot cross a
     network**. Cold snapshot/resume (`src/persist/snapshot.rs`, `agent_resume.rs`) IS
     data-driven/portable.
   - The API socket has only a server-side impl today; federation needs a new "API client" role
     inside the local server process.

## Feasibility spectrum (the decision brainstorm must frame)

- **Tier 0 — no herdr change (exists today):** herdr-fleet-style — spawn `ssh` panes in a local
  workspace. Already validated this session. Not a herdr feature; external helper.
- **Tier 1 — small/medium herdr change:** teach `--remote` (or a new flag/subcommand) to create a
  LOCAL workspace and spawn the remote connection as pane(s) in it — reusing
  prepare/ensure-remote + `spawn_argv`. Each remote pane = an ssh-fed terminal (a remote shell, or
  a single embedded remote-herdr view). Status via **screen detection only**; no warm handoff; no
  native per-remote-pane structure. Delivers the user's literal ask ("as a new workspace") without
  the protocol rewrite.
- **Tier 2 — large / new subsystem (the "deeply" ambition):** true federation — local server
  subscribes to the remote's structured session events, relays into local EventHub, id-namespacing
  across servers, multi-source event fan-in, remote process-detection relay, remote-backed
  `Pane`/`TerminalRuntime` source abstraction, cold-resume for remote workspaces. Native status +
  resume fidelity. Effectively a rewrite of the remote layer + new protocol capability.

## Top risks / unknowns for the build (if it proceeds)
- ID collision is the first hard wall for any multi-server merge (Tier 2). Namespacing touches
  every `*Target`/`terminal_id`.
- Remote agent status: without process-table relay, Tier 1 status is best-effort screen-scrape.
- Reconnect/latency/version-skew across local↔remote herdr versions (protocol v16).
- One scout flagged a follow-up: does `src/terminal/TerminalRuntime` already abstract I/O
  transport? If yes, a "socket-backed runtime" could pull Tier 2's pane work down toward medium.

## Sharpened prompt for Stage 2 (brainstorm)
"Compare approaches for making `herdr --remote ssh user@ip` present the remote herdr as a local
workspace: (A) Tier-1 pane-tunnel — new/changed CLI path that opens a local workspace and spawns
ssh-fed panes reusing prepare/ensure-remote + spawn_argv, screen-detection status; (B) Tier-2 true
federation — local server relays the remote's structured session over the existing protocol with
id-namespacing + multi-source EventHub + remote-backed pane source; (C) hybrid — Tier-1 now,
protocol groundwork for Tier-2 later. For each: effort, blast radius on existing `--remote` users,
status/resume fidelity, and the smallest shippable slice. Recommend the smallest change that
satisfies 'remote as a local workspace in one sidebar'. Include a visual mock of the target
sidebar. Cite the file:line couplings from the blindspot scouts."
