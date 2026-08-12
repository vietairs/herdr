# Scout: Workspace/Session/Sidebar model vs. remote-workspace federation

Scope: src/workspace.rs, src/workspace/, src/session.rs, src/app/ (state/creation), src/ui/sidebar.rs, src/persist/. Read-only.

## 1. Workspace model

`Workspace` struct — src/workspace.rs:145-170:
```
pub struct Workspace {
    pub id: String,                       // "w<base32>"
    pub custom_name: Option<String>,
    pub identity_cwd: PathBuf,
    cached_git_branch / cached_git_ahead_behind / cached_git_space,
    pub worktree_space: Option<WorktreeSpaceMembership>,
    metadata_tokens, metadata_token_sequences,
    pub public_pane_numbers: HashMap<PaneId, usize>,
    next_public_pane_number, next_public_tab_number,
    pub tabs: Vec<Tab>,
    pub active_tab: usize,
}
```
`Tab` (src/workspace/tab.rs, not fully dumped but referenced at workspace.rs:1195-1206) owns `panes: HashMap<PaneId, PaneState>`, `layout: TileLayout`, `runtimes: HashMap<PaneId, TerminalRuntime>`, `root_pane`, `zoomed`.

`PaneState` — src/pane/state.rs:6-11:
```
pub struct PaneState {
    pub attached_terminal_id: TerminalId,
    pub seen: bool,
}
```
That's the entire pane model. `TerminalId` indexes into `AppState.terminals: HashMap<TerminalId, TerminalState>` (src/app/state.rs:1317-1318) and into a `TerminalRuntimeRegistry` — both process-local, in-memory maps owned by the one running server. `TerminalRuntime` (src/terminal) is the local PTY/process handle (spawned via `Tab::new` / `Workspace::new_with_tab`, workspace.rs:344-417, which calls `Tab::new`/`Tab::new_argv_command` — local process spawn, not a network source).

**No trait/enum indirection exists between a pane and its data source.** Every constructor path (`Workspace::new_with_tab`, `create_tab_with_runtime`, `split_pane_with_runtime`) produces a `(Tab/NewPane, TerminalState, TerminalRuntime)` triple tied 1:1 to a locally-forked PTY. `PaneState` carries only a `TerminalId`, a bare local-process handle key — there is no `enum PaneSource { Local(..), Remote(..) }` or similar anywhere in this layer.

## 2. App/server workspace ownership + sidebar rendering

`AppState.workspaces: Vec<Workspace>` — src/app/state.rs:1323. Single flat `Vec`, indexed by plain `usize` (`ws_idx`) everywhere: `app.active: Option<usize>`, `app.selected: usize`, drag/reorder state, etc. There is exactly one `AppState` per running herdr server process — it is the render/event root (src/app/mod.rs, 4869 lines).

Sidebar (src/ui/sidebar.rs):
- `agent_panel_entries_with_runtimes` (line 113) does `app.workspaces.iter().enumerate()` and builds `AgentPanelEntry{ ws_idx, tab_idx, pane_id, ... }` directly off local indices.
- `workspace_list_entries_inner` (line 318) groups by `worktree_space().key` (a local git-worktree grouping concept, unrelated to remote hosts) and again walks `app.workspaces` by index.
- `render_sidebar_collapsed` / `render_workspace_list` iterate `app.workspaces` by index to draw rows; nothing keys on a host/origin.

**Workspace identity is local-only and would collide.** `generate_workspace_id()` (workspace.rs:76-79) is a per-process `AtomicU64` counter formatted as `w1, w2, w3...` (base32 via `encode_public_number`). `reserve_workspace_ids()` (workspace.rs:120-142) only bumps this counter past ids found in *this process's own* restored snapshot. Two independent herdr server processes (local + remote) each start their own counter at `w1`. If their `Vec<Workspace>` lists were merged into one sidebar, ids `w1`, `w2`, etc. would directly collide — nothing in the id scheme carries a host/session discriminator. Same story for pane/tab public ids (`public_pane_id_for_number`, workspace.rs:112-118 — `"{workspace_id}:p{n}"`, inherits the collision).

## 3. Any existing non-local pane/workspace source?

Grep for `remote|proxy|ssh` across `src/workspace.rs`, `src/workspace/*.rs`, `src/pane.rs`, `src/app/state.rs`, `src/app/mod.rs`: **zero hits** except one comment in `src/pane.rs:59-60` about terminfo propagation across SSH inside a shell (irrelevant to pane sourcing) and unrelated `last_git_remote_status_refresh` (git remote tracking, not network-remote panes). No existing notion of a pane/workspace backed by anything other than a locally-spawned PTY process.

## 4. Session model

One session = one server process = one on-disk data dir. `src/session.rs`: `data_dir_for(name)` → `config_dir/sessions/<name>` (or default), `api_socket_path_for` → one Unix socket per session. Sessions are a *local multiplexing* concept (multiple independent local herdr servers you can `session attach <name>`), not a multi-host concept — nothing here models "this session also proxies workspaces from remote host X".

`SessionSnapshot` (src/persist/snapshot.rs:16-29):
```
pub struct SessionSnapshot {
    pub version: u32,
    pub workspaces: Vec<WorkspaceSnapshot>,
    pub active: Option<usize>,
    pub selected: usize,
    ...
}
```
`WorkspaceSnapshot` (line 50-69) has `id`, `custom_name`, `identity_cwd: PathBuf`, `worktree_space`, pane/tab numbering, `tabs: Vec<TabSnapshot>`. `PaneSnapshot` (line 98-108) has `cwd: PathBuf`, `label`, `agent_name`, `agent_session`, `launch_argv: Option<Vec<String>>`. On restore (src/persist/restore.rs, 1694 lines, not fully read but confirmed by field shape) `launch_argv`/`cwd` are used to **re-spawn a local process** for each pane — the whole snapshot format assumes every pane is locally re-launchable.

Could it hold a "remote-backed" workspace descriptor? Structurally yes — `#[serde(default)]` fields make additive schema changes cheap (e.g. add `pub remote_origin: Option<RemoteOrigin>` to `WorkspaceSnapshot`). But that only stores the fact; the *restore* path would still need special-cased logic to not locally spawn PTYs for such a workspace, and the *live* pane model (§1) has nothing to attach that Snapshot to except a locally-spawned `TerminalRuntime`.

## 5. Feasibility

**Existing `--remote ssh user@ip`** (src/remote.rs + src/remote/unix.rs) is unrelated machinery worth noting for contrast: `run_remote` (unix.rs:155-189) sets up an SSH stdio bridge forwarding a local Unix socket to the *remote* server's client socket, then calls `run_client_process` — the same full-screen TUI client used for local attach. `run_remote_client_bridge` (unix.rs:194-217) is a raw byte pump (`copy_flush` both directions) between stdin/stdout and the socket. **The wire protocol here carries pre-rendered terminal frames/bytes for one client attach session, not structured workspace/pane state.** There is no API today for a client to pull a remote server's `Vec<Workspace>`/pane content as data and splice it into a local `AppState`.

**What must change in THIS area (workspace/session/sidebar) to federate:**
1. `PaneState` needs a source discriminator (local `TerminalId` vs. remote handle) — currently a bare struct with one field, no enum/trait. Small in isolation, but every call site touching `attached_terminal_id` (dozens across workspace.rs, tab.rs, ui/panes.rs, agent detection) needs to branch or go through a new abstraction.
2. `Workspace`/`Tab` constructors (workspace.rs:236-417, 499-559, 799-881) all assume local PTY spawn (`Tab::new`, `Tab::new_argv_command` return `(Tab, TerminalState, TerminalRuntime)`). A remote-backed workspace needs a parallel construction path that doesn't fork a local process but instead attaches to a stream.
3. Workspace/pane/tab id generation (`generate_workspace_id`, `public_pane_id_for_number`, `public_tab_id_for_number`) needs host/session namespacing to avoid collisions once two servers' workspace lists coexist in one `AppState.workspaces`.
4. Sidebar (`workspace_list_entries_inner`, `agent_panel_entries_with_runtimes`) needs a host/origin label per workspace (grouping logic already exists for worktree_space — a similar "remote origin" grouping/badge could reuse that pattern) plus handling for workspaces whose git/branch/cwd metadata isn't locally resolvable (`cached_git_branch` etc. are computed via local `git` shellouts against `identity_cwd`, workspace.rs:217-219 — meaningless/wrong for a remote path).
5. Session/persist: `SessionSnapshot`/`WorkspaceSnapshot`/`PaneSnapshot` assume every workspace is locally restorable via `launch_argv`+`cwd`. A federated workspace either needs to be excluded from local snapshotting (re-established live from the remote each attach) or the schema needs a `remote_origin` variant that restore.rs special-cases to skip local process spawn.

**Invasiveness: LARGE, bordering on rewrite for full "true" federation** (structured remote pane state merged live into local render/event loop). Reasoning:
- The blocking coupling isn't really in Workspace/Session/Sidebar structure per se (that part is mechanically extensible — add an enum, add a namespace prefix, add a sidebar label). The **hardest coupling is one layer down**: `PaneState → TerminalId → TerminalRuntime` is a hard 1:1 binding to a locally-spawned OS process everywhere pane content is touched (terminal write/resize, agent-state detection in src/detect.rs, scrollback/history persistence, mouse/keyboard routing) — none of that has an existing seam for "this pane's bytes come from a remote socket instead of a local PTY." This scout's area only touches the *edges* of that problem (how Workspace/App/Sidebar reference panes by id), not the terminal/PTY layer itself where the real rewrite lives.
- No existing protocol carries structured remote workspace/pane state — today's `--remote` path is a full byte-stream client swap, not an API a second AppState could consume to build local `Workspace` objects. Building that protocol (list workspaces, subscribe to pane output, forward input) is new server+client surface, not a small addition.
- A smaller, less invasive alternative *within this area's constraints* would be: keep pane data flowing through the existing local `TerminalRuntime` abstraction, but source its I/O from a socket that pipes to/from the remote server's PTY (i.e., make `TerminalRuntime` support a "socket-backed" variant instead of "forked local process" variant). That confines the rewrite to the terminal/runtime layer and lets Workspace/Tab/PaneState stay almost unchanged (just add id namespacing + a host label). That's a **medium**-size change *if* `TerminalRuntime` already abstracts over I/O source (not confirmed — outside this scout's assigned files; recommend a follow-up scout on `src/terminal/` before deciding).

## Unresolved questions
- Does `TerminalRuntime` (src/terminal/) already abstract over I/O transport (PTY fd vs. arbitrary stream), or is it hard-wired to `portable-pty`/fork? This determines whether the "medium" alternative in §5 is realistic — needs a scout pass on `src/terminal/`.
- Does the existing `--remote` SSH bridge protocol (unix.rs) have any structured framing at all inside the byte stream, or is it truly raw terminal bytes end-to-end? Affects whether a new structured protocol can piggyback on the same transport or needs a separate channel.

Status: DONE
