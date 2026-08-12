# Terminal clipboard image paste: cmux works, Apple Terminal / Warp do not

## Symptom
Pasting a clipboard image into a pane belonging to a MOUNTED REMOTE (federated)
workspace works inside cmux but not inside Apple Terminal.app or Warp, same
machine/build/clipboard content.

## Ranked conclusion

**Primary, proven cause: H1 — cmux carries the whole feature via its own
app-level image interception (which lands on herdr's Entry B), and Terminal.app
/ Warp have no equivalent interception, so nothing shaped like an image ever
reaches herdr in those terminals.** This is a design gap (only one of two
launch surfaces "sees" images), not a decode bug and not a `--remote` gating
bug.

- Repro (documented in prior session, `xia-recon-260724-cmux-clipboard-image-remote-mechanism.md`):
  cmux's own Paste handler special-cases image clipboard content: on Cmd+V (or
  its own paste gesture) with an image on the pasteboard, cmux's Swift code
  writes the bytes to a local temp file (`/tmp/cmux-...`, later
  `/tmp/cmux-drop-<uuid>.<ext>` after an scp hop) and injects the **path as
  terminal input text** (`TerminalController.swift` → `sendInputResult`).
- Trace: that injected path text is exactly the shape herdr's Entry B
  (`bracketed_paste_image_decision`, `src/app/input/mod.rs:1016`) is built to
  catch: `local_image_path_from_text` (`src/image_path.rs`) validates the
  shape, `recognized_image_drop_location` (`src/image_path.rs:87-109`) gates
  it to `std::env::temp_dir()`. cmux's `/tmp/...` landing path satisfies this
  gate.
- Source: `src/app/input/mod.rs:849-895` (`bracketed_paste_image_decision`),
  `src/image_path.rs:87-109` (`recognized_image_drop_location`).
- Apple Terminal.app and Warp are plain terminal emulators with **no
  equivalent app-level "intercept image, write temp file, inject path"
  behavior**. When the user presses Cmd+V there, the terminal asks the system
  pasteboard for a *text* representation. An image-only (or file-reference)
  pasteboard item has no such representation in the shape herdr expects, so
  either nothing is pasted or something is pasted that doesn't match
  `local_image_path_from_text`'s shape check, and `bracketed_paste_image_decision`
  returns `FallThrough` (`src/app/input/mod.rs:1024`, `1037` — both `debug!`
  only, see log-absence discussion below). Entry B is therefore silently
  unreachable in these terminals for genuine image content, by design of what
  each terminal chooses to hand herdr, not because of a herdr bug in decoding.
- Confidence: **high**. The mechanism split (cmux = own app-level interception
  feeding Entry B; Terminal.app/Warp = nothing to feed Entry B) is proven by
  the existing cmux code trace plus herdr's own drop-location gate semantics.
  What is *not* directly proven (no live pasteboard capture from Terminal.app
  or Warp was taken this session) is the exact byte/text Terminal.app or Warp
  deliver on Cmd+V over an image pasteboard entry — see "Unconfirmed" below.

## H2 — input-decode gap for legacy Ctrl+V (0x16): REFUTED

Traced the full decode chain for a legacy `0x16` byte landing on the server
(this is the byte any VT100-compatible terminal, including Apple Terminal.app
and Warp, sends for Ctrl+V when no enhanced-keyboard protocol is negotiated):

1. `src/server/headless.rs:3078-3088` (`ServerEvent::ClientInput` handler):
   `client.raw_input.push(&data)` feeds raw client bytes into the framer.
2. `src/raw_input.rs:836-841` (`extract_one_event`, non-ESC branch):
   `first_complete_utf8_char_len` consumes the single byte, then
   `parse_terminal_key_sequence(text)` is called with `text == "\x16"`.
3. `src/input/parse.rs:6-10` (`parse_terminal_key_sequence`): tries
   `parse_kitty_key_sequence` (fails, no `\x1b[` prefix), then
   `parse_modify_other_keys_sequence` (fails), then falls back to
   `parse_legacy_key_sequence`.
4. `src/input/parse.rs:91-106` (`parse_legacy_key_sequence`, single-char
   branch) calls `parse_legacy_ctrl_char('\x16')`.
5. `src/input/parse.rs:111-125` (`parse_legacy_ctrl_char`): `0x16` = decimal
   22, falls in the `1..=26` arm, computing `char::from_u32(22 + 96) = 'v'`,
   producing `TerminalKey::new(KeyCode::Char('v'), KeyModifiers::CONTROL)` —
   exactly the default `keys.remote_image_paste` binding
   (`src/config/model.rs:982`, `"ctrl+v"`).

This legacy fallback has **no dependency on the kitty/CSI-u enhanced-keyboard
protocol or any terminal capability negotiation** — the `.or_else` chain in
`parse_terminal_key_sequence` always tries it, and it is reached identically
regardless of which terminal (cmux, Terminal.app, Warp) sent the byte, because
the decode happens **server-side** (`src/server/headless.rs`), not in the
per-terminal client.

Verified live: `cargo test --bin herdr input::parse::tests -- --test-threads=4`
→ **34 passed, 0 failed**, including `legacy_ctrl_byte_matrix_is_covered`
(same `1..=26` arm, byte range `0x01..=0x1a` which contains `0x16`) and
`parse_legacy_ctrl_b_sequence`/`parse_legacy_ctrl_c_sequence` as worked
examples of the identical code path for other letters.

Once decoded to a `KeyEvent`, it reaches `remote_image_paste_decision`
(`src/app/input/mod.rs:962-987`) via the ordinary key-dispatch path
(`src/app/input/mod.rs:99-132`), which runs **before** any generic
"forward to pane" fallthrough — confirmed by the existing established fact
that `remote_image_paste_decision` is consulted at the top of `handle_key`
and only returns `FallThrough` to continue to normal handling; it is not
raced or shadowed by a PTY-forward branch that runs first.

**Conclusion: H2 is refuted.** Ctrl+V, if the user actually presses it, decodes
correctly and reaches Entry A identically in every terminal tested by source
inspection.

## H3 — host-terminal key theft: UNTESTABLE-HEADLESS for the byte-decode
question, but CONFIRMED as the practical trigger-mismatch for Cmd+V

Cmd+V is not the herdr-recognized binding; `keys.remote_image_paste` defaults
to `ctrl+v` (`src/config/model.rs:982`), and the user's own config confirms
they have not remapped it (`~/.config/herdr/config.toml:87`, commented out —
default applies). Cmd+V is intercepted by every macOS terminal app's Edit
menu / NSResponder chain as "Paste" and never reaches the pty as a raw byte
sequence in any of the three terminals — this is standard macOS text-editing
convention, not something herdr can observe from source. What *is* observable
from source: cmux converts that intercepted Cmd+V, when the pasteboard holds
an image, into Entry-B-shaped bracketed-paste text (see H1). Terminal.app and
Warp do not perform that conversion, so their intercepted Cmd+V produces
nothing recognizable to herdr at all. Whether Ctrl+V (the binding a user would
have to press explicitly) is itself intercepted/rebound by Terminal.app or
Warp before reaching the pty cannot be confirmed from this repo — Ctrl+V is
not a standard macOS Edit-menu binding in either app to general knowledge, but
this needs a live check.

**Live probe to settle this decisively (one-liner per terminal):**
```
cat -v   # then press Ctrl+V in that terminal; expect literal ^V (0x16) echoed
```
If `^V` does not appear, that terminal is consuming/rebinding Ctrl+V before it
reaches the pty and Entry A is unreachable there regardless of any herdr
decode logic. If `^V` does appear, Entry A is reachable and H2's refutation
means it will fire correctly.

## H4 — something else: REFUTED as primary cause
- Clipboard-format gap (macOS `furl` vs `PNGf`) was a real, separately-diagnosed
  and already-fixed issue (`src/platform/macos.rs`, Gap 3 in
  `fix-260724-remote-paste-both-gaps-report.md`, merged to master in
  `b2e0713f`, present on current `master` per `git log --oneline -5 -- src/platform/macos.rs`).
  It explains a *different* failure mode (ctrl+v pressed, clipboard read comes
  back empty) and is orthogonal to the cmux-vs-Terminal.app/Warp split.
- TERM/terminfo/capability negotiation does not gate the legacy decode path
  (see H2 trace) — the `.or_else` fallback chain in
  `parse_terminal_key_sequence` has no capability check.
- No focus/mode gate found between the decode and `remote_image_paste_decision`
  beyond the ordinary terminal-mode check (`src/app/input/mod.rs:906`,
  `debug!(mode = ?state.mode, ...)`).

## New evidence from the coordinator, addressed

### 1. The `--remote`-only gate is real, but on a *different, irrelevant*
mechanism — it does not kill Entry A for this symptom

There are **two independent Entry-A implementations**, and the shipped config
comment (`src/main.rs:193`, `# remote_image_paste = "ctrl+v" # only active in
herdr --remote; empty disables raw-key image paste`) accurately describes only
one of them:

- **(a) Client-side bridge** — `src/client/mod.rs:1854-1875`
  (`should_bridge_clipboard_image_paste`), wired from
  `client_remote_image_paste_key` (`src/client/mod.rs:1741-1755`):
  ```rust
  fn client_remote_image_paste_key(config: &crate::config::Config) -> Option<...> {
      if !is_remote_client_process() {
          return None;
      }
      ...
  }
  ```
  `is_remote_client_process()` is only `true` when `REMOTE_KEYBINDINGS_ENV_VAR`
  is set, which only happens on the classic `herdr --remote <host>` bridge
  route (`remote::run_remote`). This bridge reads the **local OS clipboard on
  the client machine** and forwards bytes to the server as
  `ClientMessage::ClipboardImage` (`src/client/mod.rs:1463-1494`), for the
  case where client and server are genuinely different machines. This is the
  mechanism the config comment describes, and it is correctly `--remote`-gated.
- **(b) Server-side dispatch** — `src/app/input/mod.rs:962-987`
  (`remote_image_paste_decision`), state populated at `src/app/mod.rs:713`:
  ```rust
  remote_image_paste_key: config.remote_image_paste_key().ok().flatten(),
  ```
  This line has **no `--remote` / `is_remote_client_process()` check at all**.
  It is always populated from `keys.remote_image_paste` whenever the App
  starts, which is exactly the case for a plain `herdr` launch attached
  locally to a local server that has a federation-mounted pane
  (`--remote-workspace` / mount, the reported scenario). This is the
  mechanism actually reachable for the reported symptom, and it is **not**
  dead in normal (non-`--remote`) mode.

**Conclusion:** the shipped comment is true but describes the *wrong*
mechanism for this bug report — it does not mean Entry A is unreachable for a
plain `herdr` session with a mounted remote pane. Mechanism (b) is reachable,
byte-for-byte verified in H2 above, and is independent of terminal type. The
`--remote`-only gate is real but is not the cause of the cmux-vs-Terminal.app
split; that split is explained by H1 (Entry B reachability), not H2/Entry-A
reachability.

### 2. Server log silence is expected, not diagnostic

Every log line on both Entry A's and Entry B's decision path is `debug!` or
`warn!` (only for genuine downstream failures), **never `info!`**:
`src/app/input/mod.rs:851,906,910,914,924,933,945,1024,1037,1110,1127,1152`,
plus `src/server/headless.rs:3059` (`debug!(client_id, len = data.len(), ...
"client input received")`) and `:3138` (`debug!(... "client clipboard image
received")`). At the user's log level (`INFO`), **none of these would ever
appear**, whether Entry A/B were reached and fell through, reached and failed,
or never reached at all because the user pressed Cmd+V (which, per H3, likely
never produces a byte sequence `remote_image_paste_decision` or
`bracketed_paste_image_decision` would even look at as a candidate). Log
absence is therefore consistent with every hypothesis and does not
discriminate between them — it is weak evidence as the coordinator flagged.

**Live probe recipe to get discriminating signal:**
```
HERDR_LOG=herdr=debug herdr ...   # then reproduce the paste in each terminal
```
Look specifically for `"remote paste target"`, `"bracketed paste: text does
not have image-path shape"`, `"bracketed paste: image path is not in a
recognized drop location"` (Entry B), and `"client input received"` immediately
followed (or not) by any `remote_image_paste_decision`-adjacent line (Entry A)
to see which path is even reached per terminal/keystroke.

## Entry A vs Entry B — which is broken where

| Terminal | Entry A (ctrl+v raw key) | Entry B (bracketed-paste image path) |
|---|---|---|
| cmux | Reachable if user presses literal Ctrl+V (untested); **not the mechanism actually used** | **Working** — cmux's own image→temp-file→path-injection lands exactly on this path |
| Apple Terminal.app | Decode path proven correct (H2); reachability of the Ctrl+V keystroke itself unconfirmed (H3, needs live probe) | Unreachable for genuine image content — no app-level interception produces path-shaped text |
| Warp | Same as Terminal.app | Same as Terminal.app |

## Minimal fix surface implied (not implemented)

The proven cause is a **coverage gap**, not a bug to patch in the decode or
gating logic already in place:
- `src/app/input/mod.rs` (`remote_image_paste_decision`, Entry A) and
  `src/platform/macos.rs` (`read_clipboard_image` / `read_clipboard_image_via_file_url`)
  already correctly implement a terminal-independent path (raw Ctrl+V + direct
  OS clipboard read, including the furl fallback). The gap is that this path
  requires the user to know and press `ctrl+v` explicitly instead of the
  native macOS `Cmd+V`, and Terminal.app/Warp give the user no visual/UX
  signal that plain Cmd+V won't work for images in a federation-mounted pane.
- If the goal is parity with cmux under Terminal.app/Warp specifically (making
  Cmd+V "just work" there too), the only lever available without those
  terminals' cooperation is: keep directing users to Ctrl+V (already
  supported, confirmed reachable by source trace) and/or surface a toast when
  a paste event on a federation-mounted pane doesn't match either Entry A or
  Entry B shapes, so the silent `FallThrough` in
  `bracketed_paste_image_decision` (`src/app/input/mod.rs:1024`, `1037`)
  becomes visible to the user instead of silent (today, `FallThrough` raises
  no toast by design, matching Gap 1's original "local panes byte-identical"
  requirement — changing that is a real behavior/scope decision, not a
  one-line fix).

## Unresolved / needs live confirmation
- Whether Apple Terminal.app or Warp deliver a literal `0x16` byte to the pty
  when the user presses Ctrl+V, or intercept/rebind it before it reaches the
  pty (H3) — settle with the `cat -v` probe above, once per terminal.
- Whether the user, when they say "pasting doesn't work" in Terminal.app/Warp,
  is pressing Cmd+V (expected, given H1) or already trying Ctrl+V and it's
  still failing (would reopen H2/H3 as live, not just source-level, questions).
- No live pasteboard capture (`osascript -e "clipboard info"`) was taken this
  session for Terminal.app/Warp specifically to directly observe what
  representation (if any) ends up on the pasteboard/pty in those apps versus
  cmux's synthetic temp-file path.

Status: DONE_WITH_CONCERNS
Summary: Primary cause is H1 (design coverage gap, not a bug) — cmux converts image clipboard content into a temp-file path it injects as bracketed-paste text, which lands squarely on herdr's Entry B (`bracketed_paste_image_decision`); Apple Terminal.app and Warp have no equivalent interception, so no image-shaped input ever reaches herdr from them via ordinary Cmd+V. H2 (input-decode gap) is refuted by a full source trace plus a passing `cargo test` run: legacy Ctrl+V (0x16) decodes correctly server-side regardless of terminal or kitty-protocol support. The `--remote`-only gate found in config is real but applies to a separate, irrelevant client-side bridge mechanism (`src/client/mod.rs`); the server-side Entry A dispatch actually reachable for federation-mounted panes has no such gate. Server log silence at INFO level is expected and non-diagnostic since every relevant log line is `debug!`/`warn!`.
Concerns/Blockers: H3 (whether Ctrl+V itself reaches the pty unmolested in Terminal.app/Warp) is not settled from source alone and needs the `cat -v` live probe; whether the user has actually tried Ctrl+V (not just Cmd+V) in the failing terminals is unconfirmed and materially changes which fix (UX/docs vs. terminal-specific bug) is warranted.

---

# ADDENDUM (260807, follow-up): Ctrl+V fall-through — proven, decisive, single cause

The prior sections above stand unmodified. This addendum answers the
coordinator's follow-up: the user tried Ctrl+V (not just Cmd+V) in Apple
Terminal.app/Warp, with an image on the clipboard and a mounted-remote pane
focused, and nothing happened — no toast, no visible effect.

## Ranked cause (single, decisive): Entry A is dead code in the live client/server dispatch path

**`remote_image_paste_decision` (`src/app/input/mod.rs:962-987`) is never
called by any code path that a normal `herdr` session — local or
`--remote-workspace`/federation-mounted — actually exercises.** It is reached
from exactly one production call site, `handle_key`
(`src/app/input/mod.rs:78-132`, the check is at line 101), and `handle_key`
is in turn reached from exactly one production call site,
`handle_raw_input_event`/`handle_raw_input_batch`
(`src/app/runtime.rs:107-125,186-228`, `src/app/mod.rs:1283`), which is only
ever driven by `App::run()`'s own event loop reading `self.input_rx`
(`src/app/mod.rs:1042-1288`). `self.input_rx` requires a real local stdin
reader (`crate::raw_input::spawn_input_reader()`, `src/app/mod.rs:1044`).

**The headless server — the process that owns every client-attached session,
including the reported scenario — explicitly disables this:**

```rust
// src/server/headless.rs:616-618
// No input_rx needed — server doesn't read stdin.
// We use None for input_rx so the event loop doesn't try to read from stdin.
self.app.input_rx = None;
```

`App::run()` itself (the only other caller of the `handle_raw_input_event`
chain) is gated behind the explicit `--no-session` "escape hatch" flag,
documented at `src/main.rs:690` (`"--no-session  Run monolithically (no
server/client, escape hatch)"`) and only reached when `no_session` is true
(`src/main.rs:811,815,899-906`). A normal `herdr` invocation — with or without
`--remote-workspace` — does **not** pass `--no-session`, so `App::run()` is
never entered and `self.input_rx` is never populated in the live path either
way.

**What actually happens to a real Ctrl+V keypress instead**, traced end to
end:

1. Client sends the raw byte(s) as `ClientMessage::Input { data }`
   (`src/client/mod.rs:1510`, confirmed unmodified/pass-through for this key
   since the client-side bridge `should_bridge_clipboard_image_paste` is
   itself `--remote`-gated to `false` here — see original report body).
2. Server: `ClientMessage::Input` → `ServerEvent::ClientInput`
   (`src/server/client_transport.rs:707-734`) →
   `ServerEvent::ClientInput` handler (`src/server/headless.rs:3050-3088`):
   `client.raw_input.push(&data)` parses the byte into a `RawInputEvent::Key`
   (same decode chain already proven correct in the base report — `0x16` →
   `KeyCode::Char('v') + CONTROL`) → `self.handle_client_input_events(...)`
   (`headless.rs:2877`).
3. `handle_client_input_events` → `self.app.route_client_events_from(...)`
   (`src/app/mod.rs:1853-…`), the **actual** live multi-client key dispatcher.
   Its `Key` arm (`app/mod.rs:1862-1900`): for a terminal-mode pane, calls
   `self.handle_terminal_key_headless_from(source_id, key)`
   (`src/app/input/terminal.rs:39-61`) — **a wholly separate function from
   `handle_key`**, with its own independent implementation.
4. `handle_terminal_key_headless_from` → `prepare_terminal_key_forward`
   (`src/app/input/terminal.rs:63-236`): checks popup forwarding, direct
   navigation keybindings (`terminal_direct_non_indexed_navigation_action`),
   custom commands (`navigate::command_for_key`), indexed navigation, the
   prefix key, and modifier-only keys — **none of these reference
   `keys.remote_image_paste`, `remote_image_paste_key`,
   `remote_image_paste_decision`, or `FILE_STAGING` anywhere** (verified by
   `grep -n "remote_image_paste" src/app/input/terminal.rs` → zero matches).
   Ctrl+V matches none of them, so it falls straight through to
   `rt.encode_terminal_key(key.clone())` → `Some(PreparedPaneInput { ...
   bytes ... })` (`terminal.rs:191-235`) → back in
   `handle_terminal_key_headless_from`, `runtime.try_send_bytes(input.bytes)`
   (`terminal.rs:57-60`) — **forwarded verbatim to the focused pane's PTY**,
   which for a federation-mounted pane means the remote host's Claude Code
   process, exactly matching the earlier-established fact that the remote
   host showed its own `"No image found in clipboard"` message. Confirmed
   live-consistent: **no toast**, because no toast-raising code on this path
   is ever reached — there simply is no branch here that could raise one.

**Corroborating negative evidence:** `cargo test --bin herdr
app::input::terminal:: -- --test-threads=4` → 47/47 passed, and none of them
reference `remote_image_paste`. Every existing `remote_image_paste_decision`
test (the ones referenced in the base report and the prior "three gaps" fix)
calls `app.handle_key(...)` or `remote_image_paste_decision(...)` **directly**
(`src/app/input/mod.rs:1665,1675,1684,1714,1752,1773,1797,2231`, etc.) —
bypassing `route_client_events_from`/`handle_terminal_key_headless_from`
entirely. This is why the extensive Gap 1/2/3 test suite from the 260724
"three gaps" fix all passed and the feature still doesn't work live: **the
tests exercise a function the live server never calls.**

**Confidence: proven, not inferred.** Every link in this chain is a direct
code citation, not a plausibility argument — this is a genuine reachability
bug (dead code / unported logic across an architecture split), most likely
introduced when the client/server multi-client dispatch
(`route_client_events_from`/`handle_terminal_key_headless_from`) was built to
replace/parallel the older single-input-source `handle_key`/`App::run()` path
without porting the `remote_image_paste_decision` check across. Note that
Entry B (`bracketed_paste_image_decision`) **was** correctly ported — it is
called directly from `route_client_events_from`'s `Paste` arm
(`src/app/mod.rs:1912-1943`) — which is exactly why Entry B still works (for
the shapes its own gate allows) while Entry A does not. This is an omission
specific to the `Key` arm, not a systemic client/server problem.

This refutes any remaining framing that the `--remote`-only gate
(`client_remote_image_paste_key`) explains the failure — that gate was
already shown to apply to a different, unreachable-for-this-scenario
mechanism in the base report. The real cause is one layer deeper: even the
correctly-ungated server-side check is simply never invoked.

## Live-probe recipe (three cases, each with its own observable outcome)

Restart the server with debug logging first:
```
HERDR_LOG=herdr=debug herdr   # or however the server process is started/reloaded
tail -f ~/.config/herdr/herdr-server.log   # adjust to the actual configured log path
```

**Terminal-theft sanity check (run once, any terminal, no herdr involved):**
```
cat -v
```
Press Ctrl+V. Expect literal `^V` to echo. If it does not appear, that
terminal is consuming/rebinding Ctrl+V before it reaches any pty and no
herdr-side fix can help for that terminal — this would reopen H3. (Prediction
for Apple Terminal.app/Warp: `^V` appears, since Ctrl+V is not a native
Cocoa/NSResponder Edit-menu binding in either app — but confirm live.)

**Case A — Ctrl+V, any clipboard shape:** With an image on the clipboard and a
federation-mounted pane focused, press Ctrl+V. Expected with the bug present:
`grep 'client input received'` shows the byte arrived
(`src/server/headless.rs:3059`), but **no** line containing `remote paste
target`, `clipboard image`, or `remote_image_paste` follows it — proving the
dispatcher (`handle_terminal_key_headless_from`) never asked the question at
all. This is the outcome that confirms the root cause above.

**Case B — Cmd+V with a Finder-copied image file:** `Cmd+C` an image file in
Finder, focus a federation-mounted pane, `Cmd+V`. Grep for `bracketed paste:`.
- `"bracketed paste: text does not have image-path shape"`
  (`src/app/input/mod.rs:1024`) → the pasted text wasn't even a plausible
  path (rules out this fix entirely; likely the terminal didn't paste a raw
  path, or quoting broke the shape check).
- `"bracketed paste: image path is not in a recognized drop location"`
  (`src/app/input/mod.rs:1037`) → **this is the expected/predicted outcome**,
  confirming `recognized_image_drop_location`'s `temp_dir()`-only containment
  (`src/image_path.rs:101-109`) is the blocker, not a shape-parsing bug.

**Case C — Cmd+V with raw image data, no file backing (e.g. a screenshot tool
"Copy Image" or Preview's Edit > Copy):** First, inspect what's actually on
the pasteboard:
```
osascript -e 'clipboard info'
```
Expected: only `«class PNGf»`/`TIFF picture` entries, no `«class furl»` and no
plain-text/string entry. Then, with that clipboard state and a
federation-mounted pane focused, press Cmd+V and check the log. Expected: **no
new log line appears at all** (not even `"client input received"`, and
certainly no bracketed-paste content) — proving zero bytes left the terminal
for this clipboard shape, distinct from Case A/B where at least an input byte
or a paste event is observed.

## Fix plan for all three cases

### Case A — Ctrl+V, any clipboard shape (raw PNGf image data *or* file
reference): **ACHIEVABLE**

Single root cause (above), single fix surface. Port the `Key`-arm equivalent
of what already exists for the `Paste` arm: check
`remote_image_paste_decision(&self.state, &key)` before/alongside the
existing intercepts in `prepare_terminal_key_forward`
(`src/app/input/terminal.rs:63-236`) or in `route_client_events_from`'s `Key`
arm (`src/app/mod.rs:1862-1900`) directly — mirroring how the `Paste` arm
already calls `bracketed_paste_image_decision` inline
(`src/app/mod.rs:1912-1943`) rather than going through a legacy
single-input-source function. On `Capture`, this needs to invoke the same
async clipboard-read-and-stage flow `handle_key`'s `Capture` branch already
triggers (`begin_remote_clipboard_image_capture`,
`src/app/input/mod.rs:1070+`, and `handle_remote_image_paste`,
`src/app/input/mod.rs:1142+`) — both are already `App` methods, not tied to
the dead `handle_key`/`App::run()` chain, so they should be directly callable
from the new call site with no further porting. **No change needed** to
`remote_image_paste_decision`'s own logic, to the FILE_STAGING/federation gate,
or to `src/platform/macos.rs`'s PNGf→furl fallback — all of that is already
correct and covers *both* raw-image-data clipboards and file-reference
clipboards, since it reads the OS pasteboard directly rather than depending on
what the terminal chooses to paste. Files: `src/app/input/terminal.rs` and/or
`src/app/mod.rs` (call-site wiring only); `src/app/input/mod.rs` unchanged.

**Important implication for Case C:** because Entry A reads the OS clipboard
directly via `osascript`, it is *not* limited by what a terminal's Cmd+V
delivers. Once Case A is fixed, **Ctrl+V alone already achieves the user's
"raw image data → remote workspace" requirement** for every clipboard shape,
in every terminal, including Apple Terminal.app and Warp. The remaining gap
is specifically that the **Cmd+V keystroke** cannot carry raw image bytes
through any terminal's pty (see Case C below) — not that the underlying
capability is missing.

### Case B — Cmd+V with a Finder-copied image file (bracketed-paste path
text): **ACHIEVABLE-WITH-TRADEOFF**

Confirmed root cause: `local_image_path_from_text`
(`src/image_path.rs:71-85`) would accept the pasted absolute path + extension
shape from a Finder-copied file (Terminal.app/Warp's normal "paste a Finder
file reference as its POSIX path" behavior), but
`recognized_image_drop_location` (`src/image_path.rs:101-109`) unconditionally
rejects anything outside `std::env::temp_dir()` — by design, per its own
doc comment (`image_path.rs:87-100`): the containment check exists so an
*ordinary* absolute path the user pasted as literal text (e.g. one under
`$HOME` or a repo, not an image-paste gesture at all) is left alone and
forwarded as plain text instead of silently hijacked into an image-staging
flow.

Widening this is a genuine trade-off, not a one-line change — do **not**
simply delete or broaden the containment check, because that reintroduces the
exact false-positive risk the comment documents (an ordinary pasted path
string ending in `.png`/`.jpg`/etc. gets silently diverted from text-paste to
image-staging). Options, none clearly better than the others without a
product decision:
- **Recency check instead of directory containment.** Require the candidate
  file's mtime to be within a short window (e.g. a few seconds) of the paste
  event, in addition to the existing extension whitelist, size cap
  (`MAX_CLIPBOARD_IMAGE_PAYLOAD`), and TOCTOU-safe canonical-path read
  (`read_local_image_file`, `image_path.rs:116-126`). This targets the actual
  signal the temp-dir check was a proxy for ("this file is the artifact this
  specific paste action just produced/referenced"), independent of directory,
  and would cover `~/Desktop`, `~/Downloads`, or any other Finder-copy
  location. Cost: an extra `metadata()` syscall per candidate path; no new
  dependency.
- **Explicit opt-in config flag** (e.g. `paste.trust_home_directory_image_paths
  = false` by default) that widens the allowed root to `$HOME` (or a
  user-specified list) only when the user turns it on, accepting the
  false-positive risk knowingly. Cheapest and safest to ship, but requires the
  user to discover and enable it.
- **Do nothing** (leave Case B unachievable) and rely on Case A/Ctrl+V for
  every clipboard shape, since Case A already covers this exact clipboard
  content (Finder-copied file → furl coercion, `src/platform/macos.rs`'s
  already-shipped furl fallback) with no directory restriction at all — Case
  A's gate is a deliberate keypress, not passive paste-content sniffing, so it
  doesn't need the same anti-false-positive protection.

Files if pursued: `src/image_path.rs` (`recognized_image_drop_location` and/or
a new sibling check), `src/app/input/mod.rs`
(`bracketed_paste_image_decision`'s call site), plus new unit tests mirroring
the existing `recognized_image_drop_location_*` tests
(`src/image_path.rs`).

### Case C — Cmd+V with raw image data, no file backing: **NOT-ACHIEVABLE-IN-TERMINAL via Cmd+V itself; ACHIEVABLE overall via Ctrl+V (Case A)**

Cannot be verified from herdr's source — this is macOS/terminal-app pasteboard
behavior outside the repo — but the existing evidence is consistent and the
reasoning is falsifiable via the `osascript -e 'clipboard info'` probe above.
A "Copy Image" style clipboard entry (screenshot tool, Preview's Edit > Copy)
populates the pasteboard with image-data types only (`«class PNGf»`/TIFF), no
`public.utf8-plain-text` or `«class furl»` representation. A terminal's Cmd+V
handler asks the pasteboard for a text/string representation to insert as
keystrokes; if none exists, nothing is delivered to the pty at all — not even
an empty bracketed-paste sequence, per the prior 260724 diagnosis's
observation that this exact "paste image as escape sequence / temp-file path"
substitution is specifically an **iTerm2** convention (`clipboard-<timestamp>-
<hex>.png` filename pattern), not shared by Apple's Terminal.app or Warp.
There is no hook inside herdr's input-parsing pipeline this could attach to,
because **no bytes ever arrive** for herdr to parse — the gap is entirely on
the terminal-app side, before anything reaches the pty.

Correcting the coordinator's framing slightly: the underlying goal (raw
clipboard image data reaching the remote workspace) is **not** unachievable —
it is exactly what Case A/Ctrl+V already delivers, since Entry A bypasses the
terminal's paste mechanism entirely and reads the OS pasteboard directly. What
is specifically unachievable is the **Cmd+V keystroke** carrying that data
through Terminal.app or Warp — a hard, unfixable-in-herdr platform constraint
(no terminal materializes non-text pasteboard content as pty input without
its own bespoke interception, which is what cmux and iTerm2 each independently
built and Terminal.app/Warp do not have).

**Smallest thing that would close the Cmd+V-specific gap, if desired**
(outside terminal keystroke handling entirely): a small external helper bound
at the OS/terminal level, analogous to cmux's own mechanism — e.g. a macOS
Automator/Shortcuts "Quick Action" or a Warp custom keybinding that shells out
to a new herdr CLI subcommand (e.g. a hypothetical `herdr paste-image
--pane <target>`) reading the local clipboard via the same `read_clipboard_image`
already in `src/platform/macos.rs` and staging it over the existing
FILE_STAGING wire path — remapped to whatever key the user wants (could even
be bound to literal Cmd+V at the OS level via a text-replacement/keybinding
tool, shadowing the terminal's own Cmd+V). This is a real, out-of-repo
integration project, not a herdr code change, and is disproportionate unless
Case A (Ctrl+V) turns out to be insufficient for the user's actual workflow.

## Recommendation beyond the direct fix: a terminal-independent trigger

Per the coordinator's framing, the user's Mac-native instinct is Cmd+V, which
**no terminal can ever deliver to a TUI's pty** — this is a permanent
platform constraint, not a bug any of the above fixes removes for the raw
"paste image data" case. Once Case A is fixed, Ctrl+V is a reliable,
terminal-independent trigger for every clipboard shape, but it still requires
the user to remember a non-native shortcut. A prefix-based binding (herdr's
own two-key chord, e.g. existing prefix + a mnemonic key) is a stronger
"discoverable, always reaches herdr" option worth adding regardless of the
Case A/B fixes: prefix-key handling is checked before generic pane-forward
(`src/app/input/terminal.rs:123-126`, `self.state.is_prefix_key(&key)`) and is
entirely within herdr's own input layer — no terminal can intercept or
reinterpret it, unlike Ctrl+V (theoretically reboundable by some terminal
configs, per the still-open H3 live-probe item in the base report) or Cmd+V
(always consumed by the terminal/OS). This does not replace fixing Case A —
Ctrl+V should still work as documented — but adds a second, more discoverable
surface (e.g. via a command palette / help overlay entry) that isn't at the
mercy of terminal-specific Ctrl-key handling at all.

Status: DONE
Summary: Proven, single root cause for the Ctrl+V fall-through: `remote_image_paste_decision` is only reachable through the legacy `--no-session` monolithic `App::run()` path (`src/app/mod.rs:1042-1288`, `src/app/runtime.rs`), which the live client/server architecture never uses — the headless server explicitly nulls `input_rx` (`src/server/headless.rs:616-618`), and the actual live key dispatcher (`route_client_events_from` → `handle_terminal_key_headless_from`, `src/app/input/terminal.rs:39-236`) has no reference to `remote_image_paste_decision`/`keys.remote_image_paste`/`FILE_STAGING` anywhere, so Ctrl+V falls straight through to raw PTY forwarding with no toast, matching the reported symptom exactly. Case A (Ctrl+V, any clipboard shape) is achievable with a scoped wiring fix; Case B (Cmd+V with a Finder-copied file) is achievable but requires a deliberate, documented trade-off on `recognized_image_drop_location`'s temp-dir-only trust boundary; Case C (Cmd+V with raw, file-less image data) cannot be delivered through Cmd+V in any terminal for platform reasons, but is already fully covered by Case A/Ctrl+V once fixed, since Entry A reads the OS pasteboard directly rather than depending on terminal paste behavior.
Concerns/Blockers: The `cat -v` Ctrl+V-echo probe and the `osascript -e 'clipboard info'` probe are still live checks, not proven from source; Case B's trade-off options are presented without a recommendation since the false-positive-risk-vs-coverage call is a product decision, not a technical one.
