# Cmd+V screenshot paste into a federated remote pane — option costing

Date: 2026-08-07 · Repo: `/Users/hvnguyen/Projects/herdr` (read-only) · Scope: trigger delivery only (capture + transport already exist)

---

## 0. Findings that change the question

Three repo facts materially reframe the brief. Read these before the option table.

### 0.1 A trigger already exists and already works — it is `Ctrl+V`, not `Cmd+V`

`src/app/input/mod.rs:99-132` intercepts `keys.remote_image_paste` (default `ctrl+v`,
`src/config/model.rs:982`) when the focused pane belongs to a **live federation mount with the
`FILE_STAGING` capability** (`resolve_remote_paste_target`, `src/app/input/mod.rs:901-951`). It then
calls `crate::platform::read_clipboard_image` off-loop and stages to the peer. `Ctrl+V` is not a
menu shortcut in Terminal.app or Warp, so it reaches the pty as `0x16` today.

**So the delivered problem is narrower than stated: it is not "no trigger exists", it is "the user
wants the trigger to be `Cmd+V` specifically".** Every option below must be priced against that.

### 0.2 herdr already treats an EMPTY bracketed paste as a clipboard-image trigger — but only in `herdr --remote`, not in federation mounts

```rust
// src/client/mod.rs:1855-1862
fn should_bridge_clipboard_image_paste(data: &[u8], is_remote_client: bool, ...) -> bool {
    if data == b"\x1b[200~\x1b[201~" {
        return is_remote_client;
    }
```

Added in `16452ee3` ("fix: bridge remote clipboard image paste", refs #205 — Ghostty/macOS→Linux),
scoped down in `36445c29` (refs #986). Upstream shipped this rule because **at least one macOS
terminal emits an empty bracketed paste when the user pastes an image-only clipboard.** That is
exactly the Cmd+V-with-a-screenshot case.

The federation path does **not** have this rule. `bracketed_paste_image_decision`
(`src/app/input/mod.rs:1016+`) only matches *path-shaped* pasted text, so an empty paste falls
through and is forwarded to the remote pty as a no-op
(`src/app/input/mod.rs:278-285`, `src/app/mod.rs:1962-1975`). `src/raw_input.rs:776-782` confirms an
empty bracket pair does produce `RawInputEvent::Paste("")`, so the signal is already reaching the
app and being discarded.

**⇒ If Terminal.app / Warp emit `ESC[200~ESC[201~` on Cmd+V with an image-only clipboard, the whole
feature is a ~20-line change and literal Cmd+V works.** The brief's premise ("the terminal emits
zero bytes") is plausible but, as far as I can tell, an assumption rather than a measurement. It is
the single highest-value thing to verify and it takes five minutes (recipe in §7).

### 0.3 A terminal-side binding does not need a new escape sequence at all

herdr's existing trigger is a single byte, `0x16`. Any terminal "send text" binding can emit `\026`
directly and fire the *existing, tested* code path. Zero herdr changes. Inventing a private
CSI/OSC and wiring a new `RawInputEvent` variant into `src/raw_input.rs:771+` is strictly more work
for no benefit — the byte is already the protocol.

(Related: herdr already parses `cmd`/`super` in keybinding strings, `src/config/keybinds.rs:1156`,
and decodes the kitty-protocol SUPER bit, `src/input/parse.rs:342`. So `remote_image_paste = "cmd+v"`
is *already valid config* — it just needs a terminal that encodes Cmd to the pty. Terminal.app and
Warp do not implement the kitty keyboard protocol; Ghostty and kitty do.)

---

## 1. Option A — Bridge the empty bracketed paste into the federation path *(new; not in the brief)*

**Mechanism.** In `App::handle_paste` (`src/app/input/mod.rs:228`) and the headless
`RawInputEvent::Paste` arm (`src/app/mod.rs:1911`), add before the path-shape check: if
`text.is_empty()` and `resolve_remote_paste_target(&self.state)` yields a mount, call
`begin_remote_clipboard_image_capture(ws_idx, pane_id, crate::platform::read_clipboard_image)` — the
exact call the `ctrl+v` arm already makes. Mirrors `src/client/mod.rs:1860` one layer up. If the
clipboard holds no image, the capture returns `None` and nothing happens.

**Feasibility.** LIKELY — *conditional on the terminal emitting the empty bracket pair*.
The herdr-side plumbing is PROVEN (`src/client/mod.rs:1860` + test
`clipboard_image_paste_bridge_triggers_on_configured_key_and_empty_paste`,
`src/client/mod.rs:2310`). The terminal-side emission is UNVERIFIED for Terminal.app and Warp;
PROVEN for at least one macOS terminal by the existence of upstream #205's fix.

**Effort.** 2 files (`src/app/input/mod.rs`, `src/app/mod.rs`), one shared helper + 2 tests.
~20-40 LOC. No new dependency, artifact, process, or permission. Fits the existing
`RemoteImagePasteDecision` shape.

**Ongoing cost.** None. `#[cfg(unix)]` only, matching the surrounding block — no new
platform fracture, no `src/platform/` change.

**Security.** Neutral-to-good. The signal originates on **local stdin**; a hostile remote writes to
pane *output*, which never reaches `raw_input`. Target pane is chosen by local focus, identically to
the existing `ctrl+v` path. Clipboard read stays client-local. No weakening.

**Failure modes.** (a) Terminal sends nothing → silently nothing happens (same as today; add a
toast only if you can distinguish it, which you cannot). (b) Terminal sends an empty paste for a
genuinely empty clipboard → clipboard read returns `None`, no-op. (c) A pane app that wanted an
empty bracketed paste loses it — negligible, and only on federated panes.

---

## 2. Option B (brief #1) — Terminal-level custom keybinding

### B1. Apple Terminal.app

**Mechanism.** Settings → Profiles → *profile* → Keyboard → **+**. The editor sheet has
`Key:` / `Modifier:` / `Action:` popups (confirmed directly in the app bundle:
`Base.lproj/TTAppPreferences.nib` contains `Key:`, `Modifier:`, `Action:`, and the action title
`Send Text:`). Bindings persist to the profile's `keyMapBoundKeys` dictionary
(string `keyMapBoundKeys` present in `Contents/MacOS/Terminal`). Terminal even validates the sent
text against bracketed-paste markers — `Localizable.loctable` contains: *"Bracketed Paste Mode is
enabled in the terminal and the text to send contains the escape sequence that marks the end of
pasted text…"* — which proves the Send Text action is a real, first-class raw-byte injector.

Bind `Send Text: \026` and the **existing** `keys.remote_image_paste` handler fires. No herdr code.

**Feasibility of ⌘V specifically: UNVERIFIED, and I judge it unlikely to work unmodified.**
- I could not confirm from Apple docs, the app bundle, or community sources whether the `Modifier:`
  popup offers Command, nor whether a Command binding survives menu dispatch. Apple's own page
  ([support.apple.com](https://support.apple.com/guide/terminal/change-profiles-keyboard-settings-trmlkbrd/mac))
  documents the pane but names no modifiers. Do not let anyone tell you this is confirmed.
- The structural obstacle is Cocoa, not Terminal: `NSMenu.performKeyEquivalent:` runs during
  `NSWindow` event dispatch, *before* `keyDown:` reaches the first responder. Edit ▸ Paste owns ⌘V,
  so it should win. Terminal does implement `menuHasKeyEquivalent:forEvent:target:action:` (string
  present in the binary) — it can suppress menu equivalents — but it is known to use that for the
  `Command1Through9SwitchesTabs` preference, not generally.
- **Documented workaround if you want to push it:** macOS System Settings → Keyboard → Keyboard
  Shortcuts → **App Shortcuts** can reassign Terminal's *Paste* menu item to some other combo
  ([Apple](https://support.apple.com/guide/mac-help/keyboard-shortcuts-mchlp2262/mac)), freeing ⌘V
  for the profile binding. Two-step, user-visible, brittle across macOS updates, and still
  UNVERIFIED end-to-end.

**Near-miss combos (LIKELY, and probably good enough):** `⌘⇧V`, `⌥V`, `⌃⇧V` are not Terminal menu
equivalents, so they should bind cleanly. `⌘⇧V` is one extra finger and needs no App Shortcuts hack.

**Effort.** Zero code. One docs paragraph + a copy-pasteable `defaults`/profile snippet.

**Ongoing cost.** Per-profile, per-machine config the user must re-apply on a new Mac. No
permissions, no artifacts. macOS-only doc, no code fracture.

**Security.** None. Terminal injects a byte on a local keypress; identical threat surface to
pressing Ctrl+V.

**Failure mode.** Binding silently doesn't fire, or it fires in a pane app that wanted `⌘⇧V` (it
won't — Terminal consumes it). Worst case: user sees nothing and assumes herdr is broken.

### B2. Warp

**Feasibility: IMPOSSIBLE today. PROVEN.**
Warp keybindings (`~/.warp/keybindings.yaml`, Settings → Keyboard shortcuts) map keys to a **fixed
enum of Warp actions**; there is no send-text / send-hex / send-escape action.
[warpdotdev/Warp#8462](https://github.com/warpdotdev/Warp/issues/8462) ("Feature: Allow custom
keybindings to send raw hex codes", opened 2026-01-17) is **open with no maintainer engagement**.
Warp also does not implement the kitty keyboard protocol, so Cmd cannot reach the pty as an encoded
key either.

⇒ For Warp users, Cmd+V is not reachable by any configuration. Options A, D, or "use Ctrl+V" only.

### B3. Terminals that *do* solve it (for docs)

- **Ghostty:** `keybind = super+v=text:\x16` — the `text:` action is documented
  ([ghostty.org/docs/config/keybind](https://ghostty.org/docs/config/keybind)). PROVEN mechanism.
  Ghostty also supports `unconsumed:` to additionally forward the encoded key.
- **kitty:** `map cmd+v send_text all \x16`. PROVEN.
- **iTerm2:** Keys → Key Bindings → *Send Text*, or *Send Escape Sequence*. PROVEN.
- **cmux:** already works (stages a temp file and pastes the path — hits
  `bracketed_paste_image_decision`).

---

## 3. Option C (brief #2) — macOS global-hotkey helper app

**Mechanism.** A menu-bar app or LaunchAgent registering a system-wide ⌘V via `RegisterEventHotKey`
or a `CGEventTap`. On fire: read `NSPasteboard`, write PNG to a temp file, connect to herdr's
socket (`src/server/socket_paths.rs`) and send a request. herdr would need a new socket message,
which the CLAUDE.md runtime/client guardrail says must be classified — a "paste the local clipboard
image into the focused federated pane" command is a *client-local* action driven by a *desktop*
event, so it does not fit the server API cleanly; it would land in the private TUI client socket,
which the guardrail explicitly discourages deepening.

**Feasibility: LIKELY to build, but the scoping requirement is the killer.**
- `RegisterEventHotKey` is *application-scoped by default*; a **global** ⌘V hook needs a
  `CGEventTap` at `kCGSessionEventTap`, which requires **Accessibility (TCC) approval**.
- A global ⌘V tap intercepts ⌘V **in every app on the machine**. To scope it you must consult
  the frontmost app (`NSWorkspace.frontmostApplication`) and pass the event through otherwise. That
  is racy on every keystroke, and "frontmost app is Terminal.app" does not tell you the front
  *window* is running herdr, let alone that the *focused pane* is federated. There is no reliable
  answer here.
- Swallowing/replaying ⌘V system-wide is the exact behaviour signature of a keylogger.

**Effort.** LARGE. A second Swift/Objective-C artifact; code signing + notarization; a separate
release channel and update story (herdr's `just release` pipeline publishes four Rust binaries —
`.github/workflows/preview.yml`, CLAUDE.md "Release Channels"); a LaunchAgent plist; a first-run
TCC onboarding flow; a new socket message + protocol version bump (`src/protocol/wire.rs`).

**Ongoing cost.** TCC permission that macOS revokes on binary change; notarization on every
release; macOS-only artifact with no Linux/Windows analogue; permanent support burden ("herdr wants
to monitor my keyboard").

**Security.** **Worst of all options.** A privileged process that can read every keystroke and
every clipboard, plus a new local-socket command that performs a clipboard read. Even if the
command is only reachable from the client socket, it converts "clipboard read is a client-local
side effect triggered by a keypress herdr saw" into "clipboard read is an RPC". Any local process
that can reach that socket gains a clipboard-exfiltration primitive. This is a material regression
of the stated threat model.

**Failure modes.** TCC silently revoked after an update → ⌘V does nothing, anywhere, with no
diagnostic. Helper crashes → ⌘V stops working globally until relaunch. Worst case: the tap
misbehaves and ⌘V stops working in unrelated apps.

**Verdict: reject.**

---

## 4. Option D (brief #3) — Clipboard-watcher daemon

**Mechanism.** Poll `NSPasteboard.general.changeCount` (~100-200 ms; there is no KVO/notification
for pasteboard changes on macOS — polling is the only API). On a new image flavor, write a temp
file, so a later trigger completes with zero latency.

**Feasibility: PROVEN mechanism, but it does not solve the stated problem.** It is a *latency*
optimization, not a *trigger*. You still need §1/§2/§5 to know the user wants this image in this
pane. The brief already concedes this.

**Effort.** MEDIUM (a background thread inside herdr, or another process). **Cost/benefit is
terrible**, because the current read is already off-loop
(`spawn_clipboard_image_capture`, `src/app/input/mod.rs:835`) — the latency it removes is barely
perceptible.

**Security.** A process that reads and persists *every* clipboard image the user copies, including
from password managers and private documents, into `/tmp`. See memory
`herdr-clipboard-staging-root-symlink-hole` — herdr's clipboard staging already has a
symlink-follow weakness; widening the volume of staged material makes that worse.

**Verdict: reject.**

---

## 5. Option E (brief #4) — Terminal-specific integrations / a standard

- **Warp plugin API:** does not exist.
  [warpdotdev/warp#435](https://github.com/warpdotdev/warp/discussions/435) is a long-running
  community discussion, not a shipped API. IMPOSSIBLE.
- **Terminal.app:** no extension API. IMPOSSIBLE.
- **OSC 52:** base64 **text** only, and write-oriented. Does not cover images. Confirmed
  ([XTerm](https://invisible-island.net/xterm/xterm-paste64.html)).
- **OSC 5522 — kitty's clipboard protocol — is the real answer, and it is a genuine de-facto
  standard candidate.** It carries arbitrary MIME types (`image/png`) and supports a **read**
  request: `OSC 5522;type=read;<b64 mime list> ST` → `status=OK` / `status=DATA:mime=…` chunks /
  `status=DONE` ([kitty docs](https://sw.kovidgoyal.net/kitty/clipboard/)). Terminals are expected
  to prompt the user before honoring a read.
  - Implemented by: **kitty only**, today. Ghostty has an open request
    ([ghostty-org/ghostty#10099](https://github.com/ghostty-org/ghostty/discussions/10099));
    Claude Code has an open request to speak it
    ([anthropics/claude-code#42712](https://github.com/anthropics/claude-code/issues/42712)).
  - **It still does not solve this problem.** OSC 5522 lets herdr *fetch* an image without
    `osascript` — a nice future replacement for `read_clipboard_image()` when running under a
    remote/thin client — but it needs a trigger just the same, and neither Terminal.app nor Warp
    implements it.

**Verdict: watch, do not build. Worth a tracking note; not on the critical path.**

---

## 6. Option F (brief #5) — herdr-owned in-app trigger (the baseline)

**Mechanism.** Already 90% shipped. `keys.remote_image_paste = "ctrl+v"` works today in Terminal.app
and Warp for federated panes. The only gaps:
1. `remote_image_paste` is parsed by `parse_key_combo` (`src/config.rs:90-97`), **not** as a
   `BindingConfig`, so it cannot express `prefix+v`. Making it a `BindingConfig`/`ActionKeybinds`
   would allow `prefix+v` — a combo no terminal can steal.
2. Docs (`website/src/content/docs/configuration.mdx:206`) say `remote_image_paste` "is only active
   in `herdr --remote`". **That is stale** — `src/app/input/mod.rs:99-132` uses it for federation
   mounts too. Users are being told the feature they want does not exist.
3. No command-palette / global-menu entry for it.

**Feasibility: PROVEN** (`src/app/input/mod.rs:99-132`, tested at
`src/app/input/mod.rs:1690`, `:2231`).

**Effort.** Docs-only for the immediate win (fix the stale sentence, tell macOS users their
options). ~1-2 days if you also convert the field to `BindingConfig` and add a palette action —
`src/config.rs`, `src/config/model.rs`, `src/config/keybinds.rs`, `src/app/state.rs`,
`src/app/input/mod.rs`, plus a config-compat path for existing `"ctrl+v"` strings.

**Ongoing cost / security.** None. Cross-platform clean.

**Failure mode.** The user still doesn't get ⌘V, and says so.

---

## 7. Comparison

| | A. Empty-bracket bridge | B. Terminal keybinding | C. Global-hotkey helper | D. Clipboard watcher | E. OSC 5522 | F. In-app baseline |
|---|---|---|---|---|---|---|
| Delivers literal **⌘V** | **yes, if terminal emits it** | Terminal.app: unverified; Warp: no | yes | no (not a trigger) | no | no |
| Feasibility | LIKELY (herdr side PROVEN) | Terminal.app UNVERIFIED / Warp **IMPOSSIBLE** | LIKELY to build, scoping unsolved | PROVEN but off-target | PROVEN, kitty-only | **PROVEN, shipped** |
| herdr code | ~20-40 LOC, 2 files | **0** | new artifact + protocol bump | new thread/process | new platform backend | 0 (docs) → ~1-2 d (prefix support) |
| New process / artifact | no | no | **yes** | yes | no | no |
| Permissions | none | none | **Accessibility TCC** | none | terminal prompt | none |
| Notarization / release burden | none | none | **yes, ongoing** | some | none | none |
| Cross-platform fracture | none (`#[cfg(unix)]`, existing block) | docs only | macOS-only artifact | macOS-only | terminal-gated | none |
| Security delta | neutral | neutral | **materially negative** | **negative** | improves (drops `osascript`) | neutral |
| Blast radius on failure | silent no-op | silent no-op | ⌘V breaks **system-wide** | temp-file leakage | n/a | n/a |

---

## 8. Recommendation

**#1 — Option A, gated on a five-minute measurement. Then Option F's docs fix. A helper app is not
warranted.**

Reasoning: the brief asked to cost a helper, and the honest answer is that the helper (C) is the
worst option on every axis that matters — it is the only one requiring a TCC permission, a second
signed artifact, and a new socket command that turns a client-local clipboard read into an RPC, and
it *still* cannot reliably scope itself to "herdr is focused and the focused pane is federated". It
buys ⌘V at the price of the threat model.

Meanwhile herdr already contains the exact bridge the user needs, one layer away
(`src/client/mod.rs:1860`), and the docs actively misinform users that the working `ctrl+v` trigger
doesn't apply to their case.

Ordered plan:

1. **Measure** (§7 recipe below) what Terminal.app, Warp, and cmux emit on ⌘V with an image-only
   clipboard. This single data point decides everything.
2. If any of them emits `ESC[200~ESC[201~` → **ship Option A**. ⌘V works, natively, for those
   terminals, for ~30 lines.
3. **Regardless**, ship the Option F docs correction — `configuration.mdx:206` is wrong today.
4. For Warp users and for Terminal.app if the measurement is negative: document `⌘⇧V` via a
   Terminal.app profile binding sending `\026` (Option B), and document `keybind = super+v=text:\x16`
   for Ghostty/kitty/iTerm2. Zero code.
5. Optionally convert `remote_image_paste` to a `BindingConfig` so `prefix+v` becomes expressible.
6. File a tracking note for OSC 5522 as the eventual replacement for `osascript`-based capture.

**If you only do one thing:** run the measurement in §9 — if Terminal.app emits an empty bracketed
paste, ⌘V is a ~30-line change to `src/app/input/mod.rs` mirroring `src/client/mod.rs:1860`, and
every other option on this page is unnecessary.

---

## 9. The decisive test (5 minutes, do this first)

I did not run it: it requires overwriting the user's clipboard and GUI keystroke injection
(Accessibility approval), which is out of scope for a read-only research task.

```sh
# terminal A — capture what the pty receives
cat > /tmp/hexcat.sh <<'EOF'
#!/bin/sh
printf '\033[?2004h'          # enable bracketed paste
stty raw -echo
xxd -c 16                     # ^C to stop
EOF
chmod +x /tmp/hexcat.sh
```

Then, in **each** of Apple Terminal, Warp, and cmux:

1. `Cmd+Ctrl+Shift+4`, select a region (clipboard now has PNG, no text flavor).
2. Run `/tmp/hexcat.sh`.
3. Press **⌘V**.

Record the bytes:

| Observed | Meaning |
|---|---|
| `1b 5b 32 30 30 7e 1b 5b 32 30 31 7e` | empty bracketed paste → **Option A ships ⌘V** |
| nothing at all | brief's premise confirmed → Option B / F only |
| a `/var/folders/.../*.png` path | terminal already stages the file → existing `bracketed_paste_image_decision` should already handle it; if it doesn't, that's a separate bug |

Also worth checking in the same pass: does Terminal.app's Settings → Profiles → Keyboard → **+**
sheet let you record **⌘V** at all, and does it fire? That answers §2's UNVERIFIED verdict directly
and cheaply.

---

## Unresolved questions

1. **What do Terminal.app and Warp actually emit on ⌘V with an image-only clipboard?** Not measured.
   Everything in the ranking hinges on this. (§9)
2. **Does Terminal.app's profile keyboard editor accept the Command modifier**, and does a ⌘V
   binding survive the Edit ▸ Paste menu equivalent? Not confirmed from Apple docs, the app bundle,
   or community sources. If not, does the System Settings → App Shortcuts reassignment of Paste
   actually free it?
3. **Which terminal motivated `src/client/mod.rs:1860`?** Upstream #205 is Ghostty/macOS, but the
   commit message says nothing. Knowing the answer tells you how broad the empty-bracket behaviour
   is. Worth asking upstream or checking #205's comment thread.
4. **Is the user's Warp usage load-bearing?** If Warp must be supported, ⌘V is unreachable by
   configuration there and only Option A (if it measures positive) can deliver it.
5. **Should `remote_image_paste` become a `BindingConfig`** so `prefix+v` is expressible, or does the
   fork prefer to stay close to upstream's plain-combo field to reduce future merge conflicts?
   (Relevant given memory `herdr-354-conflict-merge-methodology`.)
6. **Does the stale doc sentence at `configuration.mdx:206` also exist upstream?** If so, that is a
   clean upstream contribution rather than a fork-local patch.
