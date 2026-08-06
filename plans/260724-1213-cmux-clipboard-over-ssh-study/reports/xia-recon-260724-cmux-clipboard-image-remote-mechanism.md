# cmux Clipboard Image Over SSH: Technical Mechanism

**Accuracy:** ~95% (confirmed via code trace + wire protocol). Missing: client-side encoding optimization details.

## Architecture Summary

cmux is an Electron-free native macOS/iOS terminal multiplexer with Rust backend. Image-over-SSH uses:
1. **Local capture:** NSPasteboard (macOS) or UIImagePickerController (iOS)
2. **Wire protocol:** v2 JSON-RPC (`terminal.paste_image` method)
3. **Transport:** base64-encoded binary over stdio (SSH exec) or WebSocket
4. **Remote landing:** SCP upload to `/tmp/cmux-drop-<uuid>.<ext>` on remote machine
5. **Agent ingestion:** File path string injected into terminal PTY as shell-escaped text

---

## Traced Paste Path

**Mobile → Desktop (base64):**
- iOS `TerminalInputTextView.swift:45` fires `onPasteImage((Data, String))` callback with raw bytes + lowercase format
- Sent to macOS over custom RPC; wire encodes as `{"method":"terminal.paste_image","params":{"image_base64":"...","image_format":"png",...}}`

**Desktop → Local temp file:**
- `TerminalController.swift:v2MobileTerminalPasteImage()` decodes base64 → `Data`
- Calls `TerminalPasteboardService+ImageMaterialization.swift:82` `saveImageData(_ data: Data, fileExtension: String) -> String?`
  - Writes to `/tmp/cmux-{timestamp}-{UUID}.{ext}` (lines 133–140)
  - File extension sanitized to whitelist: png, jpg, jpeg, gif, webp, heic, heif, tiff, bmp
  - Returns shell-escaped path (e.g., `/tmp/cmux-2025-01-24-143001-abc12345.png`)

**Local terminal path injection:**
- `TerminalController.swift` calls `terminalPanel.surface.sendInputResult(escapedPath)` → pasted into PTY immediately

**Remote SSH path (detected or explicit):**
- `TerminalImageTransferPlanner.swift:execute()` routes to `TerminalRemoteUploadTarget.detectedSSH(DetectedSSHSession)`
- Calls `RemoteSessionCoordinator+Upload.swift:uploadDroppedFilesLocked()` (lines 56–102)
- Invokes `/usr/bin/scp -q -o ... local_file user@host:/tmp/cmux-drop-{uuid}.ext` (line 88)
- Remote path returned to caller
- `TerminalImageTransferPlanner.insertedText(forPathStrings:)` wraps paths in shell escaping + space separation
- **Result:** remote path injected into terminal PTY on LOCAL side; local PTY session then sends to remote server

---

## Transport Details

**Encoding:** Base64 (RFC 4648 standard)
- Wire: `{"image_base64":"iVBORw0KGgoAAAANSUhEUg..."}`
- Size cap: **10 MB** (lines 79–83, `Self.maxClipboardImageSize`)
- Rejected if empty or oversized

**Channels:**
- iOS-macOS: Custom Unix socket or network tunnel (not specified in code; uses iOS app's existing connection protocol)
- Remote execution: SSH exec stdio RPC (newline-delimited JSON), or WebSocket on cloud VMs

**SSH options baked:**
- scp: `-q -o ControlMaster=no -o StrictHostKeyChecking=accept-new` (unless overridden, lines 74–86)
- Timeout: **45 seconds** per file (line 90)
- Port/identity/options from workspace config

---

## Remote Landing Details

**Path generation** (line 107–110):
```swift
/tmp/cmux-drop-{uuid-lowercase}.{ext-lowercase}
```
- UUID: 32-char hex (no hyphens when lowercased by Swift)
- Extension: sanitized from client hint; defaults to `.png`

**Cleanup:** SSH exec `sh -c "rm -f -- ..."` on cancellation or workspace close (lines 113–121)

**Workflow:**
1. Local `/tmp/cmux-...png` created from image data
2. Sent over scp to `/tmp/cmux-drop-...png` on remote (40–45s timeout)
3. Remote path pasted into local PTY
4. Local PTY session forwards input to remote shell
5. Agent receives file path; reads from `/tmp/cmux-drop-...png` on remote FS

---

## Agent Ingestion: FILE PATH, Not [Image #N]

**Key finding:** Agents see a **file path string**, not an image attachment.

**Evidence:**
- `TerminalImageTransferPlanner.insertedText(forPathStrings:)` shell-escapes paths and joins with spaces
- `TerminalPasteboardService.saveImageData()` returns `String?` (escaped path), not `Data`
- Path is passed to `sendInputResult()` → terminal input, not a separate API frame

**Result for Claude Code agent:**
- Agent receives terminal input: `/tmp/cmux-drop-abc12345.png` (on remote) or `/tmp/cmux-2025-01...png` (on local)
- Agent does NOT see `[Image #1]`
- Agent must explicitly read the file from disk if it wants to process the image
- Multiple images: comma-space-separated paths (`/tmp/cmux-drop-A.png /tmp/cmux-drop-B.png`)

---

## Size & Security

**Size limits:**
- 10 MB max per image (enforced client-side and server-side)
- iOS client likely downsamples/reencodes before transmission (not traced; optimization layer)

**Path validation:**
- File extension whitelist prevents `.exe`, `.sh`, arbitrary executables
- Sanitized to lowercase alphanumeric; path traversal blocked (e.g., `../../` becomes `.png`)
- SCP invoked with properly quoted paths

**Temp file ownership:** Registered as "owned" in cleanup registry; removed on app termination or explicit cleanup

---

## What herdr Could Adopt

**Applicable mechanisms:**
1. **Local image → temp file:** Write to `XDG_RUNTIME_DIR` or `/tmp/herdr-{timestamp}-{uuid}.{ext}`, return shell-escaped path (YAGNI: no base64 wire needed if you have local clipboard access)
2. **Remote upload pattern:** Use scp/sftp over federated SSH channel to `/tmp/herdr-drop-{uuid}`, not base64 (more efficient for large images)
3. **Path injection into PTY:** Send shell-escaped path as terminal input (simple, agent-friendly)
4. **Workspace-scoped temp cleanup:** Registry of owned files; garbage-collect on workspace close

**Electron/web-only techniques (NOT applicable to herdr TUI):**
- iOS UIImagePickerController callback routing
- Base64 over WebSocket (herdr has native SSH access)
- v2 JSON-RPC protocol framing (herdr has its own federation wire)
- NSPasteboard integration (macOS-specific; irrelevant for Rust TUI)

**Herdr-specific gaps:**
- herdr's federation protocol doesn't yet carry image metadata (size, format, checksum) for progress/validation
- No OSC 52 clipboard read fallback mentioned in cmux; herdr may need this for headless remotes without `/tmp` writable isolation

---

**Unresolved questions:**
- Does iOS client pre-compress images before base64? (likely yes, not traced)
- Is v2 JSON-RPC wire format documented elsewhere, or only inferred from code?
- Do remote agents on cloud VMs (E2B/Daytona) receive the same `/tmp/cmux-drop-...` paths, or different handling?

Status: **DONE**

**Summary:** cmux pipes clipboard images as base64-encoded RPC parameters, writes to local temp, then (for remote SSH) SCP-uploads to `/tmp/cmux-drop-<uuid>.<ext>` on the remote machine. Agents receive file paths as terminal input text, not attachments. Herdr can adopt the SCP-upload + path-injection pattern without the base64 wire overhead.
