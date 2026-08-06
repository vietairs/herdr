# Root cause: herdr long-run slowdown on mac (260802)

Scope: `herdr server` pid 7049 (up 4d10h) + TUI pid 51221 (`herdr --remote appn-ltu-vm-105 appn-ltu-vm-100 --remote-workspace`, up 2d12h). Read-only live investigation + source mapping. Repo untouched.

## Verdict up front

- **No leak-driven slowdown is provable in the live process today.** At sampling time the server is healthy: 0.3–0.9% CPU, RSS 26–37MB (peak footprint 91.5MB), 37 fds, all read APIs answer in <10ms including full-scrollback reads.
- **The "5.7% now vs 1.9% lifetime" CPU delta is an instantaneous-vs-average artifact, not proof of monotonic growth.** Cumulative CPU is 119:04 min over 4d10.5h = **1.86%** lifetime average, exactly the "1.9%" figure; instantaneous `%CPU` swings 0.2%→5.7% with activity (agents streaming output → 60fps renders + detection scans).
- **One unambiguous code defect was found and confirmed by code reading** (a sibling of the db671fbc leak): the remote-initiated federation teardown path leaks `remote_resync_pane_index` entries on every LinkClosed unmount — 14 such teardowns in this uptime. It is a real unbounded-growth bug with a one-line bounded fix, but its entries are tiny; it cannot honestly be blamed for user-perceived latency.
- The perceived "really slow after days" is best explained by **constant-factor per-frame costs that spike while federation mounts + agents are active** (see H1), not by an uptime-proportional leak. Notably, both remote mounts silently died on 07-31 (LinkClosed) — the current session has only 3 local workspaces, so today's live process cannot exhibit the federated-load state in which the slowness was felt.

## Evidence chain

### 1. Live process vitals (2026-08-02 ~23:07)

| Metric | Server 7049 | TUI 51221 |
|---|---|---|
| Uptime | 4d 10:27 | 2d 12:45 |
| Cumulative CPU | 119:04.43 → **1.86% avg** | 16:39.94 → 0.46% avg |
| Instantaneous %CPU | 0.2–0.9 (idle window) | 0.0 |
| RSS | 26–37 MB (footprint 58.6M, peak 91.5M) | 14–21 MB |
| Open fds (`lsof | wc -l`) | **37** | **23** |

No fd growth, no memory growth. Logs bounded: `herdr-server.log` 1.27MB (shared across restarts since 07-12), `session.json` 3.4KB, `session-history.json` 322KB.

### 2. 5s sample of server (sample 7049, /usr/bin/sample @1ms, 4021 samples/thread)

Main thread: 3993/4021 parked; 27 active samples all under `HeadlessServer::run`:
- 18 in `render_and_stream` → `ui::render_with_runtime_registry` → `ui::panes::render_panes` / `ui::sidebar::render_sidebar`, plus `FrameData::from_ratatui_buffer_with_hyperlinks` (5) and FrameData drop (4).
- **3 separate stacks bottom out in `open()`/`read()` under `workspace::git::discovery::git_repo_root` → `git_dir_for_repo_root` → `fs::read_to_string`** — reached from `Workspace::display_name{,_from}` → `derive_label_from_cwd` during *both* `compute_view_internal` (sidebar geometry: `workspace_row_height_in_body`, `agent_panel_visible_count_from`, `compute_workspace_card_areas`) and `render_sidebar` (`agent_panel_entries`). I.e. **the render path does uncached git-discovery filesystem I/O several times per frame per workspace** (src/workspace.rs:1084-1106, src/workspace/git/discovery.rs:21-39).
- 1 in `handle_scheduled_tasks_headless` → `start_git_status_refresh_if_due`.

Tokio workers: 3 workers showed active stacks (≈20 samples total ≈0.4% of a core at idle), all `PaneRuntime::spawn_command_builder` closure → `PaneTerminal::detection_text` → `ghostty_recent_text` → `Terminal::screen_cell` → `ghostty_terminal_grid_ref` → **`terminal.PageList.pin`** / `ghostty_cell_get`.

Transient threads: 4 short-lived threads in 5s (matches the 1.5s `GIT_REMOTE_STATUS_REFRESH_INTERVAL`, src/app/mod.rs:43) running `refresh_workspace_git_statuses_with_cache` → `git_status_snapshot_for_cwd` (git config/ref reads, `canonicalize`). A **new OS thread is spawned every 1.5s forever** (src/app/runtime.rs:566).

Steady-state threads: 6 `herdr-pty-N` poll threads (panes 1,2,15,35,38,49) + 6 blocking-pool workers parked in `Child::wait` — one pair per live pane; count matches the 6 live panes in session.json. **No task/thread accumulation.**

### 3. TUI sample: fully idle

All threads parked (kevent / recvfrom / stdin read / 100ms-ish resize poll doing an open+ioctl per tick). The TUI adds no meaningful load; slowness must originate server-side (or remote-side).

### 4. Live latency probes (read-only API)

- `herdr agent read w1S:p1 --source detection` ×3: **real 0.00s**.
- Full-scrollback ANSI (`--source recent-unwrapped --lines 1000000`) for all four probeable panes (wT:p1, wT:p9, w19:p1, w1S:p1): **≤0.01s each**.

So today, panes hold little scrollback (consistent with 322KB session-history.json — agents live in the alternate screen, which has no history), and every read path is fast.

### 5. Log timeline (current server lifetime, since 07-29)

- **14× `federated mount ended outcome=LinkClosed`** (07-29 → 07-31), each followed by `workspace closed workspace_id="r:appn-ltu-vm-{100,105}#default:wN"`. One mount died with `federation frame size 2161468 exceeds its channel's cap 2097152`.
- After 07-31T07:09 there are **no live federation mounts**; session saves report `workspaces=3` (all local). The TUI (started ~07-31) still carries `--remote` flags but its mounts are gone.
- Sporadic `WARN PaneDied for unknown pane` (panes 4, 36, 39, 47, 48, 50) — pane-close ordering races (death event after removal), benign.
- Session saved ~5s after each tab focus (SESSION_SAVE_DEBOUNCE=5s); capture — including `snapshot_history()` = `recent_unwrapped_ansi(usize::MAX)`, a full-scrollback ANSI read of every pane holding each pane's core mutex — runs **on the main loop thread** (`capture_session_save_job`, src/app/session.rs:39-57,72); only the file write is on the background thread.

## Confirmed code defect (sibling of db671fbc)

`handle_federation_mount_ended` — the **remote-initiated** teardown handler for LinkClosed/Faulted/IO-error (src/app/api/workspaces.rs:456) — purges:

- `purge_pending_remote_clipboard_stages_for_origin` (workspaces.rs:503)
- `purge_pending_remote_splits_for_workspaces` (workspaces.rs:538)
- `purge_pending_remote_closes_for_workspaces` (workspaces.rs:539)

but **never calls `purge_remote_resync_pane_index_for_workspaces(&closing_ids)`**. Commit db671fbc added that purge only to the *locally-initiated* `handle_workspace_close` path (workspaces.rs:823). Grep confirms workspaces.rs:823 is the sole production call site (helper at src/app/creation.rs:882).

Since every mount-time pane is indexed (`build_remote_pane`, creation.rs:1324 & 615-623), **every LinkClosed unmount leaks one `remote_resync_pane_index` entry per mounted remote pane** — 14 teardown events in this uptime alone. Impact: unbounded `HashMap<String, PaneId>` growth across link drops, plus a correctness hazard: a stale `remote_pane_id → dead local PaneId` mapping survives into a remount to the same host, so `handle_federation_resync_pane_removed` (creation.rs:1353, `remove(&pane_id)`) can resolve a remote's resync-removal onto a stale local pane id instead of the freshly-materialized one.

**Bounded fix** (small, local, no protocol/API change): in `handle_federation_mount_ended`, after computing `closing_ids`, add `self.purge_remote_resync_pane_index_for_workspaces(&closing_ids);` next to the two sibling purges (workspaces.rs:538-539), mirroring workspaces.rs:823, and clone the existing regression test (creation.rs:2280) for the mount-ended path.

This defect is **proven by code reading + log evidence of the triggering events**, but its entries are a few dozen bytes each — it does **not** explain user-perceived latency. It is reported as a must-fix leak, not as the slowdown cause.

## Ranked hypotheses for the perceived slowdown

**H1 (medium confidence) — Constant-factor render-loop overhead under federated + agent load, misread as uptime degradation.** While mounts were alive and agents streamed output, `render_dirty` keeps the server rendering full 226×72 frames at up to 60fps (`MIN_RENDER_INTERVAL` 16ms). Each full frame performs multiple uncached git-discovery filesystem reads per workspace on the main thread (sampled: 5/27 active main-thread samples inside `open()` under `derive_label_from_cwd`), builds+drops a `FrameData` per client, and competes for per-pane core mutexes against 300ms-cadence detection scans and PTY feeders; a git-status snapshot thread also spawns every 1.5s. None of this is uptime-proportional, but it scales with workspaces × panes × client size × output rate — all of which accumulate during a long working session and reset on restart, matching "slow after running a long time, fine after restart". Fix direction (bounded): cache `derive_label_from_cwd` per identity_cwd (invalidate on cwd change), which removes per-frame filesystem I/O from the hot render path.

**H2 (medium confidence) — The slowness lives on the remote side of the federation, not this mac.** The same LinkClosed churn (14 drops in 2 days) shows the SSH links and remote servers were unstable; remote pane interactivity round-trips the wire, and the known repaint gap (no repaint-request wire message) leaves remote panes stale/laggy after resize/split. A user attributing "the UI is slow" may be describing remote panes specifically. The mac-side processes measure healthy. Needs a targeted question/measurement on the VMs.

**H3 (low-medium confidence) — Scrollback-proportional screen reads on primary-screen panes.** `ghostty_recent_text` (detection + `agent read`) reads the bottom N rows via **per-cell** FFI `grid_ref` calls with screen-absolute coordinates (src/pane/terminal.rs:2442-2466 `ghostty_screen_row`; src/ghostty/mod.rs:848-853 `screen_cell`), and the sample shows the cost landing in ghostty `PageList.pin` — a page-list traversal whose cost rises with accumulated scrollback pages (cap 10MB/pane, src/config.rs:39). Today the panes hold little history and reads measure ≤10ms, so this is not currently biting; but any long-lived, high-output, primary-screen shell pane grows every 300ms detection scan and every recent-text read toward the cap. (Vendored ghostty source could not be inspected — scout-block hook denies `vendor/` — so pin's exact complexity is unverified; the sample stack is the supporting evidence.)

**H4 (high confidence as a defect, low as the slowdown cause) — `remote_resync_pane_index` leak on remote-initiated unmount** (see "Confirmed code defect" above).

## Recommendations

1. Ship the H4 one-line purge fix with a mirrored regression test (bounded; no protocol change).
2. Cache workspace display labels to eliminate per-frame git filesystem I/O from `compute_view`/`render` (bounded; render loop only).
3. Instrument next time it feels slow: `sample 7049 5` **during** the slow moment (this idle-window sample is the wrong moment), plus `ps -o time` twice a few hours apart to compute a true recent-average CPU slope.
4. Check the VMs' herdr servers (uptime, CPU, RSS) — the mac-side evidence points away from the local server.

## Unresolved questions

- Is the perceived slowness specific to remote panes / times when federation mounts are live? (Distinguishes H1 vs H2.)
- Exact complexity of vendored `PageList.pin` for screen-tagged points (vendor/ read blocked by scout-block hook).
- What re-mounts the federation links after LinkClosed during a TUI session, and why did remounting stop after 07-31T07:09 (mounts now absent while the TUI still runs with `--remote` flags)?
