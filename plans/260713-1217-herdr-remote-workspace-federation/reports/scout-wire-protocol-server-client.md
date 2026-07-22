# Scout: wire protocol + server/client architecture for remote-workspace federation

Scope: read-only. herdr fork, 178K LOC. Question: can the local server relay a
second (remote, ssh-reached) server's workspaces as a local workspace?

## 1. Client↔server protocol

Two distinct, unrelated protocol layers coexist:

**A. JSON-RPC-like control API** (`src/api/`, `src/api/schema.rs` + `src/api/schema/*.rs`)
- Transport: Unix domain socket (`interprocess::local_socket`), one JSON `Request`
  per line/frame. Socket at `crate::api::socket_path()`, perms `0o600`
  (`src/api/server.rs:25,80,132`).
- Shape: `Request { id: String, #[serde(flatten)] method: Method }`
  (`src/api/schema.rs:32-35`). `Method` is a giant enum tagged
  `#[serde(tag = "method", content = "params")]` (`src/api/schema.rs:39-42`,
  ~80 variants: `workspace.*`, `tab.*`, `pane.*`, `agent.*`, `plugin.*`,
  `events.*`, `session.snapshot`, …).
- Responses: `SuccessResponse { id, result: ResponseResult }` or
  `ErrorResponse { id, error: ErrorBody }` (`src/api/schema/response.rs:23-36`).
  `ResponseResult` is tagged `#[serde(tag = "type", rename_all = "snake_case")]`
  with one variant per method family (`src/api/schema/response.rs:41-45` onward).
- Versioned: `Pong { version, protocol: u32, capabilities }` — protocol is a
  `u32` returned on ping/handshake; server rejects mismatched clients
  (`ensure_remote_server_running` in `src/remote/unix.rs:221-241` checks
  `status.protocol == Some(CURRENT_PROTOCOL)` and refuses to bridge otherwise).

**B. Binary render/attach protocol** (`src/protocol/wire.rs`, 2048 lines)
- `pub const PROTOCOL_VERSION: u32 = 16` (`wire.rs:16`) — separate version
  counter from the JSON API's `protocol` field, bumped on wire-format changes.
- Framing: u32 little-endian length prefix + payload, `MAX_FRAME_SIZE = 2MB`,
  `MAX_GRAPHICS_FRAME_SIZE = 32MB` for Kitty images (`wire.rs:20-29`).
- `ClientMessage` enum (`wire.rs:308-401`): `Hello{version, cols, rows,
  requested_encoding, keybindings, launch_mode}`, `Input`, `Resize`, `Detach`,
  `AttachTerminal{terminal_id, takeover}`, `ObserveTerminal{target}`,
  `ControlTerminal{target, takeover}`, `AttachScroll`, `ClipboardImage`,
  `InputEvents`.
- `ServerMessage` enum (`wire.rs:599-`): `Welcome{version, encoding, error}`,
  `Frame(FrameData)` (semantic cell grid), `Terminal(TerminalFrame)` (raw ANSI
  bytes), `Graphics`, `ServerShutdown`, `Notify`, `Clipboard`, `WindowTitle`,
  `MouseCapture`, `ReloadSoundConfig`.
- Runs over the **client socket** (`src/server/socket_paths.rs::client_socket_path`),
  separate Unix socket from the API socket, also `0o600`.

**Event/subscription model** (`src/api/schema/events.rs`, `src/api/event_hub.rs`,
`src/api/subscriptions.rs`, 825 lines):
- Client sends `events.subscribe` with `Vec<Subscription>` — tagged enum
  (`events.rs:17-83`) covering `workspace.*`, `tab.*`, `pane.*`,
  `pane.output_matched` (substring/regex log-tail match), `pane.agent_status_changed`,
  `layout.updated`.
- `EventHub` (`event_hub.rs:1-46`) is an in-process `Arc<Mutex<Vec<(u64 seq,
  EventEnvelope)>>>` ring buffer (max 512), monotonic `next_sequence`. Clients
  poll `events_after(sequence)` — this is in-process pub/sub, not networked.
- Also a synchronous long-poll variant: `events.wait` → blocks server-side
  until `EventMatch` fires or timeout (`src/api/wait.rs`, `Method::EventsWait`).

**Auth**: none at the application layer. Security is entirely filesystem
socket permission (`0o600`, owner-only) — confirmed via grep, no token/auth
field anywhere in `Request`/`Hello`/`ClientMessage`.

## 2. Full-session structured representation — `SessionSnapshot`

Yes. `src/api/schema/session.rs`:
```rust
pub struct SessionSnapshot {
    pub version: String,
    pub protocol: u32,
    pub focused_workspace_id: Option<String>,
    pub focused_tab_id: Option<String>,
    pub focused_pane_id: Option<String>,
    pub workspaces: Vec<WorkspaceInfo>,
    pub tabs: Vec<TabInfo>,
    pub panes: Vec<PaneInfo>,
    pub layouts: Vec<PaneLayoutSnapshot>,
    pub agents: Vec<AgentInfo>,
}
```
Retrieved via `Method::SessionSnapshot(EmptyParams)` →
`ResponseResult::SessionSnapshot { snapshot: Box<SessionSnapshot> }`
(`src/api/schema/response.rs:48-50`, handler `src/app/api/session.rs`).
This is a flat, fully structured, serializable description of every
workspace/tab/pane/agent — everything federation would need to mirror.

Could a local server SUBSCRIBE to a remote server's events over ssh and
re-emit them? Mechanically yes at the transport level — the existing
`--remote` feature already proves an herdr Unix socket can be tunneled
transparently through `ssh` stdio (see §4). But nothing today does the
"subscribe as a client, translate, and re-publish to my own EventHub"
step — `EventHub::push` is only ever called from in-process app-state
mutation call sites (e.g. `src/app/creation.rs:306`), not from a generic
"ingest an EventEnvelope from an external source" entry point.

## 3. `ClientConnectionMode`

`src/server/clients.rs:9`:
```rust
pub(crate) enum ClientConnectionMode {
    App,
    TerminalAttach { terminal_id: String },
    TerminalObserve { terminal_id: String },
}
```
- `App`: full app client — receives `Frame`/`Terminal` render stream for the
  *entire* multiplexed UI (all workspaces the user can see) plus can issue
  JSON API calls over the second socket. This is the mode that gets
  structured session data (via `session.snapshot` + `events.subscribe`).
- `TerminalAttach{terminal_id}` / `TerminalObserve{terminal_id}`: a client
  attaches directly to ONE pane's raw PTY stream (read-write / read-only),
  bypassing the app chrome — set via `ClientMessage::AttachTerminal` /
  `ObserveTerminal` (`wire.rs`), tracked in `headless.rs` (e.g. lines
  1438, 1513, 2378, 2578, 2670). This is the raw-terminal-stream path used by
  `herdr attach <target>` and by `run_remote_client_bridge`.

So: yes, there is exactly the split asked about — `App` mode = structured
session data + JSON API; `TerminalAttach`/`TerminalObserve` = raw terminal
byte stream to one pane, no session structure at all.

## 4. Federation obstacles at the protocol level

**a. ID namespacing/collisions — confirmed real, not hypothetical.**
Workspace/tab/pane IDs are per-process monotonic counters, not UUIDs:
```rust
// src/workspace.rs:73-79
static NEXT_WORKSPACE_ID: AtomicU64 = AtomicU64::new(1);
const PUBLIC_ID_ALPHABET: &[u8; 32] = b"123456789ABCDEFGHJKMNPQRSTVWXYZ0";
pub(crate) fn generate_workspace_id() -> String {
    let counter = NEXT_WORKSPACE_ID.fetch_add(1, Ordering::Relaxed);
    format!("w{}", encode_public_number(counter as usize))
}
```
Every fresh herdr server starts this counter at 1, so a local server's `w1`
and a remote server's `w1` are guaranteed to collide the moment both are
visible in one namespace. Same pattern is implied for tab/pane IDs (not
individually re-verified but same module family). Federation requires an ID
remapping/prefixing layer (e.g. `remote:<host>:w1`) at every boundary: API
responses, `ResponseResult` payloads, `EventEnvelope`/`Subscription` targets,
and the render/attach `terminal_id` in `ClientMessage::AttachTerminal`.

**b. Routing input to the right backend.** The JSON API and the binary
attach protocol both address panes/terminals by bare `String` id
(`PaneTarget`, `terminal_id: String` in `wire.rs`). There is no concept of
"which server owns this id" anywhere in the `Method`/`ClientMessage` shapes —
routing would have to be reconstructed entirely in a new proxy layer using
the ID-prefix scheme from (a), since the wire format itself carries no
origin/backend field.

**c. Event fan-in.** `EventHub` (§1/§2) is a single in-process
`Arc<Mutex<Vec<...>>>` sequence, `push()`-only from local app-state mutation.
There's no "ingest event from elsewhere" API, no per-source sequence
namespacing, and downstream consumers (`events.wait`, `SubscriptionEventEnvelope`)
assume a single global monotonic `sequence`. Merging a second event stream
means either interleaving into the same `next_sequence` counter (races,
loses per-source ordering guarantees) or redesigning `EventHub` to be
multi-source aware.

**d. Auth.** None at the app layer (§1) — trust is 100% "you can open this
Unix socket," and for remote today that trust is delegated entirely to SSH
(see below). A federating local server proxying a remote server inherits
whatever trust the SSH session it depends on provides; nothing new to build
here per se, but nothing to reuse either — there's no token/capability model
to extend for scoping what a mounted remote workspace can do locally.

**e. Does anything today mux multiple backends?** No — and there's a
directly relevant precedent that clarifies why this is genuinely new
territory. `herdr --remote ssh user@ip` already exists
(`src/remote.rs`, `src/remote/unix.rs`, 3099 lines) but it is **not**
federation — it's session *replacement*:
1. `RemoteSsh` bootstraps/verifies the herdr binary on the remote host over
   SSH (`prepare_remote_herdr`, `src/remote/unix.rs:672`).
2. `ensure_remote_server_ready` starts a herdr server on the remote host.
3. `SshStdioBridge::start` spawns `ssh ... herdr --remote-client-bridge`,
   which on the remote side opens `run_remote_client_bridge()`
   (`src/remote/unix.rs:194-217`): connects to the **remote's own**
   `client_socket_path()` and pipes stdin/stdout raw — i.e. ssh's stdio pipe
   *becomes* a transparent forwarded Unix-socket byte tunnel to the remote
   server's client socket.
4. `run_client_process(&local_socket, ...)` then runs the **local** herdr
   client directly against that tunneled socket.

The net effect: the local machine's *client process* attaches wholesale to
the *remote* server as if it were local — the local herdr server (if
running at all) is never involved, and the entire local session becomes the
remote one. This proves the wire protocol is transport-agnostic (a plain
byte pipe over ssh stdio suffices, no protocol change needed for tunneling),
but it is the opposite shape from federation: today's model is
"replace whole client session with remote," not "local server mounts remote
as one workspace among locally-owned ones." Nothing in this code path
touches `EventHub`, `SessionSnapshot` merging, or ID remapping, because none
of that is needed when there's only ever one server in play at a time.

## 5. Feasibility rating

**Rating: large, bordering on a new protocol layer** (not a rewrite of the
existing wire protocol, but a substantial new subsystem sitting on top of it).

What's reusable as-is:
- SSH byte-stream tunneling pattern from `RemoteSsh`/`SshStdioBridge`
  (transport-level, proven).
- `SessionSnapshot` already gives a complete structured description to seed
  a mirrored view.
- `App`-mode client + `events.subscribe`/`events.wait` already gives a
  polling/long-poll change feed to mirror from.
- Socket-permission-based trust model needs no changes if federation stays
  SSH-tunnel-scoped.

What must be newly built (this is the "large" part):
1. **ID remapping layer** — prefix/translate every `workspace_id`/`tab_id`/
   `pane_id`/`terminal_id` crossing the federation boundary, in both
   directions, across `Method`, `ResponseResult`, `Subscription`,
   `EventEnvelope`, and `wire.rs::ClientMessage::AttachTerminal` /
   `ObserveTerminal` / `ControlTerminal` target strings. Touches
   `src/api/schema/*.rs` call sites broadly (every `*Target` struct) or
   requires an intercepting proxy that never lets raw remote IDs reach local
   clients.
2. **Multi-source `EventHub`** — extend `src/api/event_hub.rs` (currently a
   46-line single-sequence ring buffer) to accept externally-sourced events
   with correct provenance/sequencing, or run a second parallel hub keyed by
   remote-origin and merge at the subscription-dispatch layer
   (`src/api/subscriptions.rs`, 825 lines — would need real changes here).
3. **A remote-server API client inside the local server process** — today
   the API socket only has one implementation direction: server
   (`src/api/server.rs`) accepting client connections. Nothing acts as an
   API *client* long-term inside another herdr server process today (the
   `--remote` bridge is a dumb byte pipe, not a JSON-API client). This is a
   new component: connect out over ssh-tunneled socket, issue
   `session.snapshot` + `events.subscribe`, and keep a live mirrored
   `WorkspaceInfo`/`TabInfo`/`PaneInfo` set inside local `AppState`.
   `src/app/mod.rs` (`struct App`, `async fn run`) is the existing single
   in-process state owner and event loop — folding in a second, remote-backed
   workspace source is a structural change to the render/tick loop, not a
   local patch.
4. **Input/render routing** — a mounted remote workspace's pane
   `TerminalAttach`/render traffic must be relayed pane-by-pane (raw
   `Frame`/`Terminal`/`Input` bytes) between the local client-socket
   connection and the tunneled remote client-socket connection, keeping
   `terminal_id` remapping consistent with (1). No existing code proxies
   `ServerMessage`/`ClientMessage` frames between two live socket
   connections — `client_transport.rs` (1343 lines) is written for
   "one physical socket ↔ this server's own `AppState`," not
   "pass-through between two sockets."

Concrete enums/structs needing extension or a wrapping layer:
`ClientConnectionMode` (§3), `EventHub`/`EventEnvelope` (§1,§4c),
`Method`/`ResponseResult` `*Target` structs (§4a), `wire.rs::ClientMessage`
`terminal_id`/`target` fields (§4a/e), `SessionSnapshot` (needs a "merge
external snapshot" concept, §2), and `App`/`AppState` in `src/app/mod.rs`
(needs a remote-workspace-source concept alongside local `Workspace`s).

Nothing here is protocol-format-breaking (JSON `Method`/`ResponseResult` and
the binary `wire.rs` frames don't need new wire bytes, version bump would be
additive at most), so this is not a rewrite of the wire protocol itself —
but the amount of new stateful plumbing (ID translation, event merge, a
second API-client role, cross-socket frame relay) is comfortably a "large"
scope, closer to a new subsystem than a medium-sized feature.

## Unresolved questions
- Tab/pane ID generation was located structurally alongside workspace IDs
  (`src/workspace.rs`) but not individually re-verified counter-by-counter;
  strongly likely to follow the same `AtomicU64` monotonic pattern given the
  shared `encode_public_number` helper, but worth a quick confirm before
  design.
- Whether `--remote`'s `SshStdioBridge` local-forward mechanism
  (`local_forward_socket_path`) could be reused directly as the transport
  for a federation API-client connection, or whether federation needs its
  own ssh tunnel management independent of `RemoteSsh`, wasn't traced to
  the byte level — flagged for the design phase, not resolved here.
