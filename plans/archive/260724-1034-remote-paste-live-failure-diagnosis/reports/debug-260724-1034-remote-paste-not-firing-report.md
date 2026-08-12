# Remote image-paste not firing under --remote-workspace

## Repro (as reported)
`herdr --remote appn-ltu-vm-105 --remote-workspace` from Mac; paste a clipboard
screenshot into a remote pane running Claude Code. VM receives literal text
`/var/folders/ql/.../T/clipboard-2026-07-24-103210-E237739C.png` — no staging,
no toast.

## Root cause (proven): two independent, non-overlapping intercepts, neither
covers this case

There are two *separate* clipboard-image mechanisms in this codebase, and
`--remote-workspace` selects the launch route where **neither applies**.

### Mechanism A — terminal-drop path bridge (classic `--remote`, SSH bridge)
`src/client/mod.rs:1850-1874` (`image_path_from_terminal_drop`) exists
specifically for this exact pattern: a terminal (iTerm2/Terminal.app "paste
image as path" behavior) converts a clipboard image into a local temp file and
delivers the **path as bracketed-paste text**. This function parses that text,
validates it's an absolute path with a recognized image extension, reads the
bytes, and (via `read_image_file_from_terminal_drop`, `mod.rs:1822-1848`)
bridges them onward. But it is gated:
```
src/client/mod.rs:1855  if !is_remote_client { return None; }
```
`is_remote_client_process()` (`mod.rs:667-669`) is only true when
`REMOTE_KEYBINDINGS_ENV_VAR` is set, which only happens on the classic
`herdr --remote <host>` bridge path (`remote::run_remote`, `main.rs:742`).

### Mechanism B — federation FILE_STAGING intercept (f1a11b3c, `--remote-workspace`)
`src/app/input/mod.rs:753-798` (`remote_image_paste_decision`) only fires on a
**raw KeyEvent** matching `config.remote_image_paste` (default `ctrl+v`,
`src/config/model.rs:950`), consumed at the call site `input/mod.rs:95-121`
before mode dispatch. It calls `crate::platform::read_clipboard_image`
(OS-level clipboard read), stages bytes over `FILE_STAGING`
(`remote_clipboard_stage.rs`), and pastes the *remote* staged path. It has no
code path that inspects bracketed-paste **text** content at all — it cannot
recognize "this pasted string is actually a local temp image path."

### Why the launch route matters
`src/main.rs:709-748`: `--remote-workspace` (or `HERDR_REMOTE_FEDERATION=1`)
routes through `remote::decide_launch_route` → `LaunchRoute::Coexistence` →
`server::autodetect::auto_detect_launch_with_mount` (`main.rs:717-737`), which
**returns before** `remote::run_remote` is ever called. So
`REMOTE_KEYBINDINGS_ENV_VAR` is never set, `is_remote_client_process()` is
always `false` in this mode, and Mechanism A (the one actually designed for
"terminal already turned my image into a path") never runs.

Mechanism B is active in this mode, but the user pressed the terminal's native
paste shortcut (Cmd+V), and the terminal (evidence: `clipboard-<timestamp>-
<hex>.png` filename pattern is iTerm2's "paste image as escape sequence /
temp-file path" convention) delivered that as **pasted text**, not as a raw
`ctrl+v` key sequence. `remote_image_paste_decision` never sees a `KeyEvent`
to match against — it silently falls through to normal terminal-key/paste
handling, which just forwards the pasted text bytes to the remote PTY.

### Why there's no toast
Both fallthroughs are silent by design, not a bug in the toast logic:
- Mechanism B: `remote_image_paste_decision` returns `FallThrough` (not
  `Unsupported`) when the event isn't a matching KeyEvent
  (`input/mod.rs:762-767`); `FallThrough` raises nothing.
- Mechanism A: gated out entirely before any parsing is attempted
  (`client/mod.rs:1855`, `1809-1811` in `should_bridge_clipboard_image_paste`).

This matches the report exactly: no feedback, just the raw path text arriving.

## Hypotheses ranked
1. **H2 (launch-route gap) — proven, primary cause.** `--remote-workspace`
   selects `auto_detect_launch_with_mount`, which never sets
   `is_remote_client_process()`, so the one mechanism built for "terminal
   turned the image into a path" (Mechanism A) is unreachable in this mode.
   Evidence: `main.rs:709-748`, `client/mod.rs:667-669`, `1855`.
2. **H1 (trigger mismatch) — proven, compounding cause.** Even disregarding
   H2, Mechanism B (the federation one, f1a11b3c) only recognizes a raw
   `ctrl+v`-style KeyEvent, never bracketed-paste text, so it could not have
   caught this paste even if Mechanism A didn't exist or were active.
   Evidence: `input/mod.rs:753-798, 95-121`.
3. **H3 (capability handshake) — not reached, unconfirmed/moot.**
   `remote_image_paste_decision` returns `FallThrough` before ever consulting
   `mirror.supports(FILE_STAGING)` (`input/mod.rs:791`), because it never gets
   a matching KeyEvent. Capability negotiation is irrelevant to this failure.
   Did not attempt `ssh appn-ltu-vm-105 herdr --version` — out of scope once
   H1/H2 fully explain the symptom without needing a version mismatch.
4. **H4 (clipboard image detection) — not reached, unconfirmed/moot.** The
   clipboard is never read locally in this failure (`begin_remote_clipboard_
   image_capture` is only called from the `Capture` branch, never entered).

## What the fix would be (not applied)
Not a config fix — `keys.remote_image_paste` was already at its default
`ctrl+v` and the workspace *is* a genuine federation mount, so nothing the
user could reconfigure would have helped. This is a code gap: either (a) port
Mechanism A's terminal-drop-path parsing into the `--remote-workspace`
coexistence client so it also runs for federation-mounted panes, teaching it
to stage-and-rewrite the detected local path via the same `FILE_STAGING`
channel Mechanism B already has, or (b) extend `remote_image_paste_decision`'s
call site to also inspect bracketed-paste payloads for a local image path, not
only raw key events. Both are code changes to `src/client/mod.rs` and/or
`src/app/input/mod.rs`; no VM binary/version issue is implicated.

## Secondary ask: can Claude Code show `[Image #1]` instead of a pasted path?
Claude Code only renders `[Image #N]` when *it* performs the clipboard image
read itself (on Linux, via `xclip`/`wl-paste` against the live X11/Wayland
selection). The VM is headless: no X server or Wayland compositor is running,
so `xclip`/`wl-paste` have nothing to attach to regardless of what herdr
stages or pastes. Herdr staging bytes to a remote path does not put anything
into a selection buffer Claude Code's own paste handler would read — that
handler runs independently of the terminal's bracketed-paste text and talks
directly to the (nonexistent) X/Wayland clipboard.
A herdr-side "virtual clipboard" shim (e.g. spin up `Xvfb` + `xclip`/wl-clipboard
mock on the VM and set the selection to the staged image bytes before/at
paste time) could in principle make Claude Code's own clipboard read succeed,
but this is a substantial new surface (headless display server lifecycle,
timing coordination with Claude Code's read, wl-clipboard vs xclip detection)
disproportionate to the ask. The realistic near-term outcome, once the code
gap above is fixed, is that the *remote path* gets pasted correctly (staged,
existing on the VM) — whether Claude Code auto-ingests an image from a pasted
file path in its prompt text is Claude Code's own behavior, not something
herdr controls or can currently verify from this repo.

## Unresolved questions
- Does Claude Code CLI auto-detect and read a pasted absolute path ending in
  `.png` as an image, or does it require the `[Image #N]` clipboard-paste
  flow specifically? Determines whether fixing the path (H2/H1 fix) alone is
  sufficient, or whether a clipboard shim is also needed.
- `ssh appn-ltu-vm-105 herdr --version` was not run (not needed to explain the
  symptom); worth checking anyway to rule out an unrelated staleness issue
  before scoping the fix.

Status: DONE
Summary: The failure is a launch-route gap, not a bug in the FILE_STAGING feature itself — `--remote-workspace` never enables the terminal-drop-path bridge (`is_remote_client_process()` stays false), and the federation intercept only recognizes raw ctrl+v key events, not the bracketed-paste path text iTerm2/Terminal.app actually sends; both fallthroughs are silent by design, matching the "no toast, just the path" report.
Concerns/Blockers: Whether Claude Code ingests a pasted image path as `[Image #N]` on its own is unconfirmed and gates how much of a fix is actually needed.

## Follow-up: ctrl+v fired, read empty

New evidence: pressing `ctrl+v` in the remote pane now shows
`"clipboard has no image (png/jpg/gif/webp/bmp)"`. This is
`TOAST_NO_CLIPBOARD_IMAGE` (`src/app/input/mod.rs:665`), raised from the
`ClipboardImageCapture::NoImage` branch of `handle_remote_clipboard_image_captured`
(`input/mod.rs:850-857`) — i.e. `remote_image_paste_decision` *did* return
`Capture` this time (raw `ctrl+v` KeyEvent matched, unlike the Cmd+V/bracketed-
paste case in the original report), but the OS clipboard-image read itself
came back empty.

### 1. Where the read executes
`begin_remote_clipboard_image_capture` (`input/mod.rs:819-837`) spawns
`crate::platform::read_clipboard_image` on a blocking thread and reports back
via `AppEvent::RemoteClipboardImageCaptured`. On macOS this is
`src/platform/macos.rs:591-630`: shells out to `osascript` running
`the clipboard as «class PNGf»`, writes the coerced PNG to a temp file, reads
it back. A non-zero `osascript` exit (coercion failure) or an empty/oversized
file both fall through to `None` (`macos.rs:611-624`).

### 2. Which process runs this — refutes "wrong machine" theory
`App`/`handle_key`/`handle_raw_input_event` (the code containing
`remote_image_paste_decision` and the clipboard-read spawn) live in the
**server** process (`src/server/headless.rs`; `App::handle_raw_input_event` is
the dispatch point reached from the server's socket-input loop, not from
`src/client/mod.rs`, which is a thin renderer/input-forwarder). For
`--remote-workspace`, `auto_detect_launch_with_mount` (`autodetect.rs:328-340`)
calls `ensure_server_running()` first — this starts/attaches the **local**
herdr server via the local Unix socket (`client_socket_path`,
`autodetect.rs:19,39+`) — then sends a `mount_remote_request` **from** that
local server **to** the VM as an API call, and only then runs
`client::run_client()` as a thin attach. The VM is the passive mount target
answering the federation link; it never owns the `App`/clipboard-read code.
So the server executing `osascript`/`the clipboard as «class PNGf»` is the
**Mac's own local herdr server**, reading the **Mac's own clipboard**. The
user's "wrong machine" theory is refuted by this call chain — there is no
plausible path for this specific read to run on the VM in `--remote-workspace`
mode.

### 3/4. Conclusion: (b) mac clipboard format not recognized — most likely, unconfirmed
`the clipboard as «class PNGf»` only succeeds when the clipboard pasteboard
holds a flavor AppleScript can coerce to real PNG bytes (raw TIFF/PNG image
data). It does **not** succeed against a **file reference / promise**
pasteboard item (e.g. a `public.file-url` entry pointing at a temp PNG on
disk) — which is exactly the shape implied by the original report's `Cmd+V`
behavior (`clipboard-<timestamp>-<hex>.png` path text arriving instead of
bytes): whatever produced that clipboard entry (screenshot tool / iTerm2
"paste image as path" convention) plausibly populated the pasteboard with a
file reference, not raw image data. `read_clipboard_image` has no fallback
that resolves a file-URL/promise pasteboard item to bytes — only the direct
`PNGf` coercion path (`macos.rs:597-598`). This is consistent with, but not
directly proven by, a live pasteboard inspection (`osascript -e "clipboard
info"` was not run against the user's live session). Ranked over "(c) something
else": no other code path between the confirmed `Capture` decision and the
`osascript` call does any filtering that could independently produce `None`.

Status: DONE
Summary: The read runs correctly on the Mac's own local server process against the Mac's own clipboard (wrong-machine theory refuted by the launch-route call chain), and the empty result is most likely because the clipboard holds a file-reference/promise pasteboard item rather than raw PNG/TIFF data that `the clipboard as «class PNGf»` can coerce.
Concerns/Blockers: Format-mismatch conclusion is inferred from the two pieces of evidence (Cmd+V producing a path, ctrl+v producing "no image"), not from a direct live pasteboard-type inspection on the user's Mac.
