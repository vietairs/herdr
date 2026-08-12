# Code review — round 4 delta

Branch `fix/terminal-clipboard-image-paste` @ base `f934965b`, uncommitted. Scope: M1, M2, M3, Q5 only.

## Verdict

**APPROVE_WITH_NITS** — with one Medium finding that is a *documentation* defect, not a code defect: the round-4 docs assert an absolute ("turns clipboard image paste off entirely") that is false on the `herdr --remote` path. Fix the sentence or the code; either is a one-liner. Everything M1/M2/M3/Q5 claimed about the *federation* path verified true.

---

## Item-by-item verification

### M1 — off switch. VERIFIED (federation path), HOLE (remote-client path)

Gate at `src/app/input/mod.rs:1186-1188` is the first statement of `dispatch_empty_bracketed_paste`, before `resolve_remote_paste_target`, before any toast, before any capture. Holds.

Bypass audit — enumerated every production entry into the two dispatchers:

| entry | file:line | gated by binding? |
|---|---|---|
| legacy `handle_key` | `src/app/input/mod.rs:100` | yes (`remote_image_paste_decision` line 917) |
| legacy `handle_paste` | `src/app/input/mod.rs:219` | empty→yes; non-empty→no (intentional) |
| live key `prepare_terminal_key_forward` | `src/app/input/terminal.rs:92` | yes |
| live paste arm | `src/app/mod.rs:1961` | empty→yes; non-empty→no (intentional) |

No in-process bypass. The non-empty cmux temp-path bridge being ungated is fine as decided — it is a file read of a path the terminal itself produced, not a clipboard read; agreed, not flagging.

**Finding M1-1 (Medium) — the off switch does not reach `herdr --remote`.**
`src/client/mod.rs:1888-1890`:

```rust
if data == EMPTY_BRACKETED_PASTE {
    return is_remote_client;
}
```

Returns before the `remote_image_paste_key` is even looked at. So on a `herdr --remote` client with `keys.remote_image_paste = ""`, Cmd+V with an image on the clipboard still calls `crate::platform::read_clipboard_image()` (line 1467) and still ships a `ClientMessage::ClipboardImage` to the server. With no image, `suppress_unbridged_clipboard_image_trigger` (line 1494) swallows the keystroke too — so with the feature "off", the remote client both reads the clipboard *and* eats the paste.

This behaviour is pre-existing on master. What is new in round 4 is the claim about it:

- `docs/next/CHANGELOG.md`: "Setting `keys.remote_image_paste` to an empty string turns the whole feature off, including this trigger."
- `docs/next/website/src/data/config-reference.json`: "Set it to an empty string to turn clipboard image paste off entirely, including the empty-bracketed-paste trigger some terminals use for `Cmd+V`."
- `src/main.rs:193`: "empty disables clipboard image paste entirely"
- `src/app/input/mod.rs:1181-1185` doc comment: "One setting governs the whole clipboard-image-paste feature… would otherwise leave the feature with no off switch at all."

Failure scenario: privacy-conscious user sets `keys.remote_image_paste = ""` on their laptop after reading the config reference, `ssh`es in with `herdr --remote`, hits Cmd+V in Warp intending to paste text from an empty clipboard — herdr reads their laptop clipboard anyway and uploads whatever screenshot is on it to the server host. The user has done exactly what the docs told them to do to prevent that.

Fix is either: thread the binding into the empty branch (`if data == EMPTY_BRACKETED_PASTE { return is_remote_client && remote_image_paste_key.is_some(); }`, plus the same guard in `suppress_unbridged_clipboard_image_trigger`), or soften all four doc strings to scope the claim to mounted-remote panes. I'd take the code fix — it makes the absolute true and is consistent with the stated intent of M1.

### M2 — remote client must not read the HOST clipboard. VERIFIED, with the documented residue confirmed real

`src/client/mod.rs:1878-1879`, called at `:1494`.

Cannot break local clients — two independent reasons, both checked:
1. `should_bridge_clipboard_image_paste` (`:1888`) returns `is_remote_client` for the empty shape, so a local client never enters the enclosing block at all and the suppression is unreachable for it.
2. `client_remote_image_paste_key` (`:1744-1748`) returns `None` unconditionally when `!is_remote_client_process()`, so the key branch is also dead locally. A local client forwards raw bytes and the server (same host, same user's clipboard) does the work. Correct.

`is_remote_client_process()` (`:671`) = `REMOTE_KEYBINDINGS_ENV_VAR` present. Only `herdr --remote` sets it. Test at `:2377` covers all four shape/origin combinations. Not a phantom test.

**Finding M2-1 (Medium, documented, accept-or-fix) — the configured-key residue is real and is *widened* by this branch.**
Remote client presses `Ctrl+V`, its own clipboard has no image → `should_bridge` true → read returns `None` → not suppressed (by design) → raw `0x16` forwarded → server's `prepare_terminal_key_forward` (`src/app/input/terminal.rs:92`) → `dispatch_remote_image_paste_key` → `clipboard_image_reader()` = `crate::platform::read_clipboard_image` **in the server process** → host's clipboard image staged on the mounted third host and its path pasted into the pane the remote operator is watching. The agent in that pane then reads and describes the image. That is an exfiltration path for a screenshot the remote operator never saw.

Rounds 1-3 are what made this live: on master the key intercept sat only in the dead `handle_key`, so in a normal session the server never read the host clipboard for a forwarded `0x16`. Round 4 closed the empty-paste half of the hole and left the key half open.

Threat model caveat: `herdr --remote` is normally a self-attach (same human, own machine), which is why I am not calling this a blocker. But the doc comment at `:1866-1873` frames it as "an image the person at this keyboard never saw" — i.e. it asserts the adversarial model — and then leaves the larger of the two doors open. Either the comment should say plainly that the key trigger is *not* covered and why that is acceptable, or the key should be suppressed too when the remote client found no image (it is symmetric: the client already proved there is nothing to bridge, and the cost is only that a remote-client `Ctrl+V` on a federated pane no longer reaches the pane app — which it already does not, since a *successful* read swallows it as well).

Informational: for a remote client on a box with no clipboard tooling, `read_clipboard_image()` always returns `None`, so *every* empty bracketed paste is now swallowed and never reaches the pane app. Payload-free, so harmless; noting it because it is a behaviour change vs master for that configuration.

### M3 — `&mut self` + per-pane in-flight flag. VERIFIED

Signature change `src/app/input/mod.rs:1281-1287`. Both callers (`:1094` key path, `:1156` cmux path, `:1211` empty path) already hold `&mut self`. No orphaned call sites (`grep` shows none outside `impl App`).

Insert ordering is correct: workspace-id lookup first (`:1289`, early return before any insert), then `insert` (`:1291`), then `spawn` (`:1300`). No path inserts without spawning.

**Release paths enumerated by reading, not by trusting the comment.** `handle_remote_clipboard_image_captured` (`:1313`): the `remove(&target_pane_id)` at `:1325-1326` is the **first statement in the body**, ahead of the `match`, ahead of every `return`. There is no `?`, no `let … else`, no panic-capable expression before it. Every one of the four exits — `NoImage` (`:1337`), `ReadTimedOut` (`:1344`), workspace-gone (`:1357`), success (`:1362`) — is downstream of the release. Cannot wedge.

The other half of the claim, "every read resolves into exactly one `RemoteClipboardImageCaptured`", also verified rather than assumed:
- `spawn_clipboard_image_capture` (`:793-810`): all four match arms fall through to a single `events.send(...)`. Includes `Ok(Err(JoinError))` → `NoImage`, so a panic inside the read still emits. Includes `Err(_)` timeout → `ReadTimedOut`.
- Dispatch is unconditional: `src/app/api.rs:307-314` is a plain `if let … { handle…; return; }` in `handle_internal_event` with no mode/focus/suspend gate ahead of it, and `handle_internal_event_with_render_impact` (`:65`) routes everything but `GitStatusRefreshed` into it.
- Only unhandled loss is `event_tx` receiver dropped = app shutdown. Don't care.

Nit M3-a: on timeout the `spawn_blocking` child is *not* cancelled (`spawn_blocking` is uncancellable; the outer task just drops the handle). The flag is released, so a new trigger can start a second child while the first is still alive. Bounded at one new child per 5s per pane, so the flood bound still holds in the sense that matters; the "one read per pane at a time" phrasing in the `App` field doc (`src/app/mod.rs:195-207`) is slightly stronger than what the code guarantees.

Nit M3-b: a trigger dropped by the in-flight guard produces only a `debug!` (`:1294`). Copy image A, Ctrl+V, then within the read window copy image B and Ctrl+V — B's press is silently eaten, no toast, and the staged image is A. Rare (read is normally sub-100ms) but the user gets no signal.

### Q5 — purge. VERIFIED, both sites, both sets

Two production call sites, and they are the only two workspace-removal paths that also purge the sibling federation indexes:
- `src/app/api/workspaces.rs:552` in `handle_federation_mount_ended` (fn at `:462`) — remote teardown.
- `src/app/api/workspaces.rs:906` in `handle_workspace_close` (fn at `:862`) — local close.

Ordering correct at both: the purge derives `closing_pane_ids` from `self.state.workspaces`, and at both sites it runs *before* `self.state.close_selected_workspace()` (`:554` / `:911`). Had it run after, the id set would be empty and the purge a no-op — worth stating because that is the way this class of fix usually gets shipped broken.

Both sets cleared: `remote_image_paste_unsupported_notices` (`:1266`) and `remote_clipboard_image_reads_in_flight` (`:1268`).

Site 2 is inside `if let Some(host_key) = self.federation_host_key_for_workspace(index)`. Correct scoping — both sets are only ever keyed by federated remote pane ids.

Nit Q5-a: the test `remote_image_paste_pane_state_is_purged_when_the_workspace_closes` (`:3111`) calls `purge_remote_image_paste_pane_state_for_workspaces` directly. It proves the function, not the wiring. If either call site is later dropped in a refactor, the suite stays green. Cheap improvement: drive one of the two through `handle_workspace_close`.

Nit Q5-b (pre-existing, out of scope): `handle_federation_mount_ended` (`:544-552`) still does not call `purge_pending_remote_clipboard_stages_for_workspaces`, which `handle_workspace_close` does (`:905`). Asymmetry predates this branch.

Nit Q5-c: closing a single *pane* (not the workspace) leaks its entry in both sets for the process lifetime. `PaneId`s are never reused so it cannot mis-target, and it is one `u64`-ish entry per pane the user tried the feature on. Fine as-is; flagging only because the field docs claim the purge covers the leak completely.

---

## Rounds 1-3 regression sanity

- **Ctrl+V on a local non-remote pane still reaches the pane app.** `remote_image_paste_decision` (`:912`) → `resolve_remote_paste_target` returns `None` → `FallThrough` → `Forward`, no side effect in that arm. Covered by `live_key_dispatch_forwards_the_image_paste_key_on_a_local_pane` (`:2673`), which asserts the literal `0x16` arrives and no toast fires. Passes.
- **Cmd+V on cmux still works.** Non-empty text short-circuits *before* the new empty branch (`:1131-1133`), so round 4 cannot have touched it. Path unchanged.
- **Empty paste on a local pane still forwarded.** `empty_bracketed_paste_on_an_ordinary_local_pane_is_forwarded_unchanged` (`:2970`). Passes.
- **Key-repeat gate intact.** `:1085-1088` unchanged by round 4.
- Test quality: the round-4 tests drive `route_client_input` / `handle_terminal_key_headless` and count real `AppEvent`s off the App's own channel. Not phantom.

---

## Verbatim test output

Branch, `cargo test --bin herdr -- --test-threads=2`:

```
failures:

---- api::server::pane_graphics_stream::tests::inactive_owner_cancels_idle_stream_and_dispatches_close stdout ----

thread 'api::server::pane_graphics_stream::tests::inactive_owner_cancels_idle_stream_and_dispatches_close' (17221331) panicked at src/api/server/pane_graphics_stream.rs:991:14:
canceled idle stream should dispatch a close: Timeout
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace


failures:
    api::server::pane_graphics_stream::tests::inactive_owner_cancels_idle_stream_and_dispatches_close

test result: FAILED. 3393 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 41.96s
```

3393 passed / 1 failed. The single failure is the acknowledged pre-existing `pane_graphics_stream` flake. No new failures.

## Verbatim clippy output

Baseline established in a throwaway detached worktree at `f934965b` (since nothing is committed on the branch), `touch src/main.rs` before each run to defeat the cached-no-op trap. Both runs `cargo clippy --bin herdr --all-targets`.

Branch:

```
warning: `herdr` (bin "herdr" test) generated 12 warnings (run `cargo clippy --fix --bin "herdr" -p herdr --tests -- ` to apply 6 suggestions)
warning: `herdr` (bin "herdr") generated 10 warnings (7 duplicates)
```

Master `f934965b`:

```
warning: `herdr` (bin "herdr" test) generated 12 warnings (7 duplicates) (run `cargo clippy --fix --bin "herdr" -p herdr --tests -- ` to apply 4 suggestions)
```

Per-lint inventory is byte-identical between the two (14 distinct warning texts, same counts: 2× `redundant pattern matching`, plus 12 singletons — `type_complexity`, `contains()` vs `iter().any()`, unused import `Ordering`, needless borrow, and 9 dead-code items). **0 errors, 10 bin warnings, 12 bin-test warnings on both. Branch adds 0 new clippy warnings.**

Baseline worktree and its target dir were removed after the run; `git worktree list` is back to two entries.

---

## Recommended actions

1. **M1-1** — either gate the empty-paste branch in `src/client/mod.rs:1888` on `remote_image_paste_key.is_some()`, or drop the word "entirely"/"whole feature" from the four doc strings. Do not ship the current absolute.
2. **M2-1** — decide explicitly on the key-trigger residue. If accepting it, amend the `suppress_unbridged_clipboard_image_trigger` doc comment to state that the configured key can still cause a host-clipboard read, rather than implying the boundary is closed.
3. **Q5-a** — route one purge assertion through `handle_workspace_close` so the wiring is regression-protected.
4. M3-a / M3-b / Q5-c — optional comment tightening; no code change needed.

---

## Unresolved questions

1. Is `herdr --remote` ever used cross-user (operator A attaching to operator B's herdr), or is it strictly self-attach? The answer decides whether M2-1 is a doc nit or a real blocker.
2. Does anything in the fleet actually depend on an empty bracketed paste reaching a pane app? If yes, the round-4 consume-on-federated-pane behaviour needs an escape hatch beyond unsetting the binding.
3. Should the in-flight guard drop (M3-b) surface a toast, or is silence preferred for a case that only appears under sub-100ms double-triggers?
