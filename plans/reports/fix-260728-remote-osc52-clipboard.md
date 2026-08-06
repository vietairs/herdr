# Remote OSC 52 clipboard not reaching mac — root cause + fix

Date: 2026-07-28. Branch: `master` (fork `vietairs/herdr`, base commit `00b86618`).
Companion root-cause report: `root-cause-260728-remote-osc52-clipboard.md`.

## Symptom

Claude Code (TUI) running in a pane on `appn-ltu-vm-100` / `appn-ltu-vm-105`
mounted as a federated remote workspace shows "Copied", but nothing lands on the
mac clipboard. Mouse-drag selection copy from the same remote pane works.

## Why selection copy worked and OSC 52 did not

Two unrelated paths:

- Selection copy — `src/selection.rs:327` calls `platform::write_clipboard`
  directly on the locally-rendered mirrored grid text. Never touches federation.
- OSC 52 from the remote app — travels the federation clipboard channel.

## Root cause (confirmed)

Detection was never the problem. The mirror emulator parses the remote pane's
OSC 52 correctly (`Osc52Forwarder`, `src/pane/osc.rs:397`), no alt-screen gating,
and `src/remote/federation/sanitize.rs:10-13` does not strip it. `src/pane.rs`
(RT-F7 comment) then sends a `ClipboardMessage { origin_tag: "remote", payload }`.

The receiver was discarded. At the one live production mount site:

```rust
// src/app/api/workspaces.rs:199-201 (before)
let (outbound_clip_tx, _outbound_clip_rx) = tokio::sync::mpsc::unbounded_channel::<
    crate::remote::federation::protocol::ClipboardMessage,
>();
```

The consumer that would apply it — `apply_remote_clipboard_writes`
(`src/remote/federation/pane_source.rs:251`) — is fully implemented but had
**zero non-test callers**. Its own module header states the intent plainly:

> "…of this module is dead code outside `#[cfg(test)]` until P8 wires a real…"
> — `src/remote/federation/pane_source.rs:24`

So this is an **unfinished feature (staged P7/P8 scope), not a regression**.
Every remote clipboard write was silently dropped on the floor.

## Fix

`src/app/api/workspaces.rs` — keep the receiver and drain it into the same
`AppEvent::ClipboardWrite` path local panes use.

Routing through `AppEvent::ClipboardWrite` rather than calling
`platform::write_clipboard` directly is the load-bearing detail: clipboard is a
**client-local side effect**. In server/client mode the headless server does not
write the clipboard itself — it forwards to the foreground client as
`ServerMessage::Clipboard` (`src/server/headless.rs:2168`). Writing the clipboard
inside the mount task would have targeted the *server host*, not the operator's
mac — i.e. it would have looked correct in monolithic mode and silently failed in
the exact server/client mode the user actually runs.

Task lifetime: ends when the mount drops its senders. No leak.
`try_send` + `warn!` on failure, so a full/closed channel is logged, never silent.

Respects the runtime/client boundary guardrail in `CLAUDE.md`: no new
TUI-socket-only behavior; it reuses the existing server→client clipboard message.

## Verification

Local mac build is **blocked, unrelated to this change**: the vendored
libghostty-vt Zig/C++ step fails against the Xcode-beta `MacOSX27.0.sdk`
(`libcxx/include/__random/...`), before any Rust compiles. `SDKROOT` and
`DEVELOPER_DIR=/Library/Developer/CommandLineTools` both fail to redirect it —
Zig resolves the SDK via `xcrun` against the selected Xcode.

Verified on `appn-ltu-vm-100` (Linux, zig 0.15.2), synced to the identical base
commit `00b86618` with only this change applied:

- `cargo check --tests` — clean. Two unused-import warnings are pre-existing
  (`EventData`/`EventsWaitParams`/`Ordering`); this change adds no imports.
- `cargo test --bin herdr clipboard` — all clipboard tests pass, including the
  full `app::remote_clipboard_stage::tests` and `pane::osc::tests` sets.
- Full `cargo test --bin herdr`, patched: 3160 passed / 20 failed — all 20 in
  `session::tests`.
- Full `cargo test --bin herdr`, **baseline (patch stashed)**: 3159 passed /
  **21 failed** — same `session::tests` family, 3180 total both runs.

  Failures are therefore **pre-existing and flaky**, not introduced here: the
  count moves run-to-run (21 -> 20), and the same tests pass 31/31 in isolation
  (`cargo test --bin herdr session::`). They mutate process-global env/socket
  vars and contaminate each other under parallel execution. Worth a separate
  fix; out of scope for this change.

**Not yet done: no live end-to-end test.** The fix has not been exercised against
a real federated mount with Claude Code copying in a remote pane. That requires
rebuilding and restarting BOTH the local and remote herdr servers (stale server
images cause `MountSnapshot` mismatch).

## Follow-up shipped in the same change: attribution + opt-out

Both land in the two clipboard event handlers, not at the mount, so
`reload_config` applies live without remounting.

- `AppEvent::ClipboardWrite` gains `origin: Option<String>` (`src/events.rs`).
  `None` = local pane, `Some(address)` = federated remote. All local producers
  pass `None`; only the federation drain sets it.
- **Attribution** — copy toast reads `copied to clipboard from <user@ip>` for a
  remote write, unchanged `copied to clipboard` for local
  (`App::show_clipboard_feedback`, `src/app/api.rs`). The mount's `HostKey` is
  `user@ip#session-discriminator`; only the address is shown.
- **Opt-out** — `[remote] accept_clipboard_writes` (default `true`, preserving
  parity). When `false`, remote-origin writes are dropped with a `debug!` line
  and no toast; local writes are unaffected. Documented in the shipped default
  config (`src/main.rs`).
- Policy is enforced on BOTH paths: monolithic (`src/app/api.rs`) and headless
  server forwarding (`src/server/headless.rs`).

Not implemented, deliberately: per-write confirmation prompt (rejected — copy is
high-frequency in a TUI, the friction would negate the fix) and per-host
allow/deny lists (YAGNI until a global toggle proves insufficient).

### Tests added

- `app::tests::remote_clipboard_write_names_the_host_that_wrote_it`
- `app::tests::remote_clipboard_write_is_ignored_when_the_operator_refuses_remote_writes`
- `app::tests::refusing_remote_clipboard_writes_does_not_affect_local_panes`
- `config::model::tests::remote_clipboard_writes_are_accepted_by_default_and_can_be_refused`

All 4 pass. Full suite after the change: 3181 passed / 3 failed (3184 total,
+4 new). The 3 failures were `app::api::plugins::*` and
`pty::backend::unix::*` — a *different* set than the baseline's 20-21
`session::tests`, and all pass under `--test-threads=1`. Suite-wide parallel
flakiness, confirmed independent of this change.

## Unresolved questions

1. **Live verification outstanding** — needs both servers rebuilt/restarted, then
   copy inside Claude Code on vm-100 and check the mac clipboard.
2. ~~Consent gate for remote-origin clipboard writes.~~ **Addressed** — see the
   attribution + opt-out section above. Note the exfiltration direction was
   already closed independently: `parse_osc52_clipboard_write`
   (`src/pane/osc.rs:886-897`) rejects OSC 52 read queries (`data == b"?"`) and
   accepts only the `c` selector, so a mounted host can overwrite the clipboard
   but can never read it. Payloads are capped at 256 KiB.
3. ~~`origin_tag` discarded.~~ **Addressed** — origin now drives both the toast
   and the policy check. The channel's own `origin_tag` is still the generic
   literal `"remote"`; the mount's `HostKey` is used instead as the richer
   source. Collapsing the two would be a tidy-up.
4. The `inbound_clip_tx` receiver (`_inbound_clip_rx`, same function) is also
   dropped — the opposite direction (local→remote clipboard). Not in scope here;
   flagged as a likely sibling gap.
5. Whether the local mac toolchain break should be fixed separately (pin a
   non-beta Xcode, or bump vendored zig).
