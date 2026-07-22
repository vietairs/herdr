# Blindspot --deep — remote-workspace paste (images + files)

Date: 2026-07-22 · Route R7 stage 1 · 4 parallel scouts

## The one-line root cause

A federated pane's PTY runs on the REMOTE host. Today paste stages bytes to a **local** temp file
(`src/server/clipboard_image.rs:13`) and injects that **local absolute path** as pane text. For a
remote pane the path names a file the remote process cannot see. `cmux ssh` works because agent and
file share a host.

## Biggest finding — the rails already exist, dormant

The fork's own federation design anticipated this and left it unfinished:

| Piece | Location | State |
|---|---|---|
| `ClipboardMessage { origin_tag: String, payload: Vec<u8> }` | `protocol/mod.rs:243-247` | shipped in v3 |
| `FederationMessage::Clipboard(..)` variant | `protocol/mod.rs:354` | shipped in v3 |
| `Channel::Clipboard` cap = 16 MiB | `protocol/mod.rs:320,339` | shipped |
| `send_local_clipboard_to_remote()` | `client.rs:452-462` | **dormant** — doc: "no production call site until P8 wires a real paste keybinding to a live mount" |
| serve-side inbound handling | `serve.rs:441-446` | **deferred** — `_ => return`, comment "Clipboard forwarding is deferred (M9)" |
| `handle_inbound` itself | `serve.rs:415` | `#[allow(dead_code)]` |

=> **No `FEDERATION_PROTOCOL_VERSION` bump required** (stays 3). Both peers already decode the
variant. This is finishing P8 + M9, not inventing a protocol.

## Critical gap in the existing message shape

`ClipboardMessage` has ONLY `origin_tag` + `payload`. Missing for this feature:
- file **extension** (staging needs it; `sanitize_extension` allowlist is local-only today)
- original **filename** (for non-image files, where extension allowlist is wrong)
- **target pane/terminal id** — serve-side has no idea which pane to paste into
- a **response** carrying the staged remote path back (Clipboard is fire-and-forget, no
  `request_id`; contrast `SplitPaneRequest/Response` which do correlate — `protocol/mod.rs:260-269`)

Adding fields to the struct is a serde wire change. `codec.rs:94-98` rejects on version **inequality**,
so old-peer tolerance is NOT provided by capability negotiation. Mitigation options: `#[serde(default)]`
fields, encode metadata in `origin_tag`, or bump to v4. **This is the central design fork.**

## Existing paste path (client -> server -> pane)

1. Capture: `client/mod.rs:1805` `should_bridge_clipboard_image_paste()` on empty bracketed paste
   `\x1b[200~\x1b[201~`, or configurable `remote_image_paste_key` (`client/mod.rs:1427`).
   Unix drag-drop of a file: `client/mod.rs:1823-1874` `read_image_file_from_terminal_drop()` —
   **image-only**; no generic file paste exists anywhere.
2. Guard: 16 MB client-side (`client/mod.rs:1430`), `MAX_CLIPBOARD_IMAGE_PAYLOAD` (`wire.rs:28`);
   server closes connection on oversize (`client_transport.rs:737-752`).
3. Transport: `ClientMessage::ClipboardImage { extension, data }` (`wire.rs:336`), one message, no chunking.
4. Stage: `headless.rs:1721` -> `clipboard_image::stage()` -> `$TMPDIR/herdr-clipboard-images-<uid>/`.
5. Inject: `RawInputEvent::Paste(path)` -> `app/mod.rs:1711` -> `runtime.try_send_paste` ->
   `pane.rs:3072` (bracketed-paste wrap if `input_state.bracketed_paste`).

Platform: macOS PNG via osascript (`platform/macos.rs:591`); Linux PNG/JPEG/GIF/WebP/BMP via
wl-paste/xclip with signature validation (`platform/linux.rs:376`); **Windows + fallback return None**
(`platform/windows.rs:665`). Federation is `#[cfg(unix)]` only (`federation/mod.rs:74`).

## Routing a payload to the right mount

- Remote pane ids are namespaced `r:<host_key>:<remote_id>`; canonical predicate `classify()`
  (`federation/id.rs:137-147`). `HostKey = user@ip#session_discriminator`.
- Registry: `AppState::remote_mirrors: HashMap<HostKey, RemoteMirror>` (`app/state.rs:1594`).
  Pane -> mount is derived by parsing the id prefix; there is **no reverse index**.
- Send surface: the mount's `out_tx: mpsc::UnboundedSender<FederationMessage>`
  (`pane_source.rs:101`) — non-blocking, fire-and-forget, returns only `SendError`.
- Teardown: `end_federation_mount()` (`app/state.rs:1652`) aborts tasks; **in-flight pastes are
  dropped with no retry/stash** (`pane_source.rs:162-167`).

## Security / limits precedent

- Trust model: federation is **TRUSTED-REMOTE, SSH is the only auth boundary**
  (`federation/mod.rs:8-37`); socket is `0o600`, no app-layer auth, no per-workspace isolation.
  But remote-sourced input is still treated as untrusted after the session.
- Hardening to mirror on the remote side: extension allowlist (`clipboard_image.rs:58`), atomic
  `create_new(true)` + 100-attempt retry (`:32`), file `0o600` (`:100`), dir `0o700` (`:110`),
  24h stale sweep (`:118`).
- Control-byte sanitization of remote strings: `sanitize.rs:38-45` (C0/DEL/C1) at the reducer
  ingest choke point. **A staged remote path pasted back is remote-sourced text -> must be sanitized
  before it reaches a PTY.**
- Buffers: `CLIPBOARD_CHANNEL_CAPACITY = 64` (`client.rs:314`), terminal output 4096 (`client.rs:302`).
- Deps present: `base64 0.22.1`. **Absent: `image`, `infer`, `mime_guess`, `tempfile`.** No
  content/magic-byte validation anywhere — extension-only.

## Test harness

`LoopbackFederationServer::spawn()` (`loopback.rs:235-256`) runs `serve::run()` over in-memory
`tokio::io::duplex`. Copyable example: `handshake_advertises_capability_and_a_fresh_instance_id_each_boot()`
(`loopback.rs:314-355`). Recipes: `just test`, `just test-one <filter>`, `just check`.

## Unknown unknowns / risks to carry into design

1. **Direction is one-way by default.** Paste needs the staged REMOTE path echoed back to decide what
   text to inject. Either a new correlated response, or the remote side pastes into the pane itself
   (server-side injection) and the client injects nothing.
2. **Which pane?** `ClipboardMessage` has no pane target; server-side injection needs one.
3. **Non-image files** break every existing assumption: the extension allowlist, the
   `read_image_file_from_terminal_drop` capture, and `sanitize_extension`'s png-default fallback
   (an unknown extension silently becomes `.png` — actively wrong for `report.pdf`).
4. **No filename preservation** — agents often need a meaningful name, not `client-3-clipboard-<nanos>.png`.
5. **Windows**: federation is Unix-only; the feature is inherently Unix-scoped for now.
6. **Mount-drop mid-transfer** silently drops the payload with no user feedback.

## Unresolved questions

- Should non-image arbitrary files be in scope for v1, or images first? (user asked for both)
- Should the staged remote file live in the remote pane's cwd or a temp staging dir?
- Is a filename allowlist/sanitizer needed on the remote side for user-supplied names?
