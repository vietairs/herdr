# Remote-workspace image paste (federation)

**Status:** planned, not started
**Branch:** `feat/remote-workspace-paste-image-files` (worktree
`/Users/hvnguyen/Projects/herdr-worktrees/remote-workspace-paste-image-files`, base `5ec2a10b`)
**Design (locked):** B — correlated stage-then-inject RPC. Client sends image bytes over the
federation tunnel; the remote stages the file and returns its remote path; the client injects that
path through the existing local paste path. Images only in v1. The staged name goes through the full
sanitisation contract.

**Correction to the locked wording (ACKED by the user 2026-07-22; does not change the design):** "original
filename preserved" is not achievable in v1 — verified, there is no filename anywhere in the capture
path (`crate::platform::ClipboardImage { bytes, extension }`, `src/platform/mod.rs:88-94`; both
backends supply only a validated `extension`). Phase 05 synthesises `image.{extension}`; the phase-02
filename contract stands unchanged as defence against a hostile *mounting client*. Nothing else in
design B moves.

**Authoritative input:**
`reports/predict-260722-1624-remote-paste-five-persona-debate.md` (section A corrections override
the blindspot and brainstorm reports; section F is the test plan; section I is reconciled at the
ship gate).

---

## Phases

| # | Phase | Scope | Depends on |
|---|---|---|---|
| 01 | [protocol](phase-01-protocol.md) | `ClipboardStageRequest`/`Response`, `Channel::FileStaging` arm + `largest_max_len()`, `Capability::FILE_STAGING`, codec/cap tests | — |
| 02 | [remote staging](phase-02-remote-staging.md) | New `src/remote/federation/file_staging.rs`: 8-step filename contract, quota check, atomic 0600 write | 01 |
| 03 | [federation wiring](phase-03-federation-wiring.md) | Inbound dispatch in `federation_accept.rs::reader_loop` + detached staging worker with bounded teardown; capability gate on **both** sides; per-mount connection epoch; response arm + `AppEvent`s | 01, 02 |
| 04 | [client correlation + injection](phase-04-client-correlation.md) | Pending map on `App`, proportional timeout, in-flight cap, epoch-fenced purge at the two teardown sites, ordered stale-response guard, returned-path reject-don't-strip, injection, toast copy constants + slow-transfer affordance | 03 |
| 05 | [capture trigger + UX + docs](phase-05-capture-trigger-ux-docs.md) | Three-branch per-pane capture gate replacing the process-level gate, post-capture seam, client-side size precheck, toast copy assertions, config-reference doc | 04 |

Strictly sequential. Each phase owns disjoint files, with narrow carve-outs where a type change
forces a cross-phase compile dependency. Phase 03 owns: the three `actions.rs` match arms; and every
site the new `FederationMountEnded` field breaks — its construction sites in
`app/api/workspaces.rs`, the exhaustive destructure/forward in `app/api.rs:161-168`, and
`handle_federation_mount_ended`'s signature. Phase 04 owns the handler **bodies** in
`app/api/workspaces.rs` and the three new `ClipboardStage*` dispatch arms in `app/api.rs`
(ownership tables are in the phase files).

---

## Acceptance criteria

1. Pasting an image while a mounted-remote-workspace pane is focused writes the file **on the remote
   host** and injects a path that resolves there.
2. No `PROTOCOL_VERSION` bump, no `FEDERATION_PROTOCOL_VERSION` bump (verified against `v0.7.5`,
   see phase 01).
3. The `file_staging` capability is enforced **independently on both sides**: the mounting client
   never sends an ungated `ClipboardStageRequest`, and a newer serving host never emits either
   `ClipboardStage*` frame to a controller that did not negotiate the capability. An older peer in
   either direction degrades to a toast (or a logged drop), never a torn-down mount.
4. A hostile remote cannot execute a command via the returned path, and a hostile client cannot
   write outside the remote staging dir via the filename. Control-byte rejection is **not** enough:
   the paste can reach an unbracketed PTY raw (`src/pane.rs:3076-3086`), so both sides enforce the
   same shell-metacharacter allowlist and the client additionally requires the
   `federation-clipboard-` staging prefix (phase 02 step 5b, phase 04 requirement 8). Every returned-
   path rejection is proven at the **injection boundary** — the real `AppEvent` handler with a live
   pending entry and a real pane runtime — not against the sanitizer in isolation.
5. A pending stage never leaks: it resolves, times out, or is purged at mount-end / workspace-close.
6. `just check` green (`cargo fmt --check`, `cargo clippy -D warnings`, `cargo nextest`,
   `windows-lint`, maintenance script tests). **Discharged on the VM**, not on this workstation: see
   Local build note. Locally the best available proxy is
   `ZIG=~/.local/zig-0.15.2/zig cargo test -- --test-threads=4` plus `cargo clippy` diffed against
   the three known baseline errors; that proxy does **not** cover `windows-lint`, which phase 02
   requirement 8, phase 03 requirement 3b and phase 05 step 1 now depend on. `file_staging` is
   Unix-gated while `server/federation_accept.rs`, `federation/serve.rs` and `app/input/mod.rs` all
   compile on Windows, so every call into it is `#[cfg(unix)]`.
7. **No failed stage leaves an artifact on the remote host.** Every rejection — root, candidate final
   path, filename, quota, payload — is decided before the `create_new` call; nothing is validated
   after `write_all` (phase 02 requirement 4b, test 12b). `create_new` makes only *creation*
   exclusive, so a `write_all`/`flush` failure after it removes the just-created file before
   returning `WriteFailed` — a truncated stage is never left behind either (phase 02 requirement 6b,
   test 12c).
8. **The two-in-flight cap is enforced on both sides.** The client refuses the third concurrent stage
   per mount (phase 04 requirement 5), and the remote admits at most two requests *including* the one
   being staged, answering the third with `Busy` (phase 03 step 3). The serving-side permit is RAII
   and is returned on the `try_send` failure path too, so a refusal can never wedge the connection
   at `Busy` (phase 03 test 10b).
9. **The capture intercept never swallows a key it did not claim.** Only `Unsupported` and `Capture`
   consume; `FallThrough` falls into the existing `match self.state.mode`, so local `ctrl+v` and
   every non-terminal mode keymap are untouched (phase 05 requirement 1, tests 2 and 2b).

---

## Assumptions — CONFIRMED BY USER 2026-07-22

These answer the predict report's four open questions. They are baked into the phases; flagged here
because they are decisions, not derivations. All four confirmed as written at the stage-4 direction
confirm, together with the in-flight cap of 2.

Also confirmed at that gate: **"preserve the original filename" is withdrawn for images.** Verified
at source — `ClipboardImage { bytes, extension: &'static str }` (`platform/mod.rs:90-93`) has no
filename field and both capture paths (`macos.rs:626`, `linux.rs:416`) yield raw pasteboard bytes,
so an OS clipboard image has no name to preserve. Phase 02 synthesises `image.{extension}` behind
the collision-proof prefix and retains the full ordered sanitisation contract as defence against a
hostile *mounting client*, not against the local clipboard. Filename preservation moves to the
generic-file fast-follow, where a copied file does carry a path.

- **A1 — head-of-line blocking (P4 / HIGH-1): no chunking in v1.** The 16 MiB cap stays; a large
  paste stalls every pane on that mount for the transfer duration. Phase 03 ships the loopback test
  that documents the behavior. Chunking requires flow control and is a separate design. Because A1
  serialises transfers anyway, phase 04's client in-flight cap is **2**, not 4 — the client's
  `out_tx` is unbounded (`client.rs:852`), so that cap is the only bound on ~58 MiB-per-stage RSS.
- **A2 — degenerate `mount_generation` (C2): worked around locally.** Federation-wide fencing is
  **not** refactored here (that also affects `SplitPaneResponse` today) — see Deferred.
  **Mechanism revised 2026-07-22 (still inside A2, not a reversal):** the workaround was
  `server_instance_id`; it is now a **locally minted per-mount `MountConnectionEpoch`**. Reason:
  `server_instance_id` is minted per remote server *process* and arrives in `MountSnapshot`
  (`client.rs:190-206`), so a remount to a still-running remote reuses it — it cannot distinguish a
  fresh mount from a superseded one, which is exactly the case the guard exists for. The epoch is a
  process-global `AtomicU64` stamped on each successful `connect_and_mount`, stored on
  `RemoteMirror`, and carried on the two stage events plus the existing `FederationMountEnded`. It is
  still a local mechanism confined to this feature's events and one handler, so A2's "do not refactor
  federation-wide fencing" stands.
- **A3 — trigger key stays `ctrl+v`.** `config/model.rs:948` is a shipped default; changing it is a
  breaking change. Phase 05 documents it, including the readline quoted-insert / vim visual-block
  collision and how to rebind.
- **A4 — staging-dir lifetime keeps the 24h sweep**, plus a new ~500 MiB staging-dir total-bytes
  quota check before write (phase 02). Lifetime is not tied to mount lifetime.

---

## Prediction coverage (predict section I)

| # | Prediction | Phase | Test |
|---|---|---|---|
| P1 | Returned path reaches PTY unsanitised | 04 | `returned_remote_path_with_embedded_newline_is_rejected_before_paste`, `returned_remote_path_with_esc_sequence_is_rejected_before_paste`, `returned_remote_path_with_shell_metacharacters_is_rejected_before_paste`, `returned_remote_path_without_the_staging_prefix_is_rejected` — **all four driven through the real `FederationClipboardStageReady` handler with a live pending entry and a real pane runtime, each paired with an injecting positive control** |
| P2 | Preserved filename enables traversal | 02 | `stage_rejects_path_traversal_in_original_filename`, `stage_rejects_absolute_path_filename`, `stage_rejects_null_byte_in_filename`, `stage_rejects_or_strips_bidi_override_in_filename`, `stage_rejects_unknown_extension_instead_of_png_fallback`, `stage_rejects_oversized_filename`, `stage_rejects_shell_metacharacters_in_original_filename`, `a_rejected_candidate_final_path_leaves_no_file_on_disk`, `a_failed_write_leaves_no_partial_file_on_disk` |
| P3 | Ungated send kills the mount (either direction) | 01 (negotiation), 03 (both gates) | `clipboard_stage_capability_absent_on_one_side_is_dropped_not_fatal`, `clipboard_stage_capability_present_both_sides_is_agreed`, `stage_request_is_not_sent_when_capability_not_agreed`, `a_host_without_the_agreed_file_staging_capability_never_emits_a_stage_frame` |
| P4 | Large paste stalls the mount | 03 (documented, A1) | `a_large_clipboard_frame_does_not_starve_terminal_output_delivery_forever` |
| P5 | Staging inline on the reader thread stalls panes | 03 | `clipboard_stage_request_end_to_end_through_loopback_server` + `reader_loop_hands_a_clipboard_stage_request_to_the_staging_worker_without_blocking` |
| P6 | Pending request hangs forever | 04 | `clipboard_stage_request_times_out_when_remote_never_responds`, `clipboard_stage_timeout_is_proportional_to_payload_size_not_fixed` |
| P7 | Stale response injected after remount; stale mount-end purges a fresh remount | 04 | `clipboard_stage_response_from_a_restarted_remote_is_dropped_not_injected`, `clipboard_stage_response_after_a_remount_to_the_same_running_remote_is_dropped`, `a_delayed_mount_ended_event_does_not_purge_or_end_a_fresh_remount`, `clipboard_stage_response_from_a_different_hostkey_is_rejected`, `clipboard_stage_pending_entries_purged_on_workspace_close_and_mount_end` |
| P8 | Paste lands in the wrong pane | 04 | `remote_paste_injects_staged_path_through_local_paste_command` (target resolved at mint time) |
| P9 | Capture never fires in the mounted TUI | 05 | `image_paste_decision_is_capture_for_a_focused_mounted_remote_pane`, `image_paste_stages_and_consumes_the_key_for_a_supplied_clipboard_image`, `image_paste_decision_is_fall_through_for_a_local_pane` (driven through the real `handle_key` with a live pane receiver), `fall_through_still_reaches_non_terminal_mode_handlers`, `image_paste_decision_is_unsupported_when_the_mount_lacks_the_staging_capability` |
| P10 | Failures stay silent | 04 (mapping), 05 (copy) | `clipboard_stage_toast_copy_is_defined_for_every_failure_variant`, `clipboard_stage_failure_raises_a_toast_with_the_documented_copy` |

Supporting lifecycle tests (`two_pastes_in_quick_succession_resolve_independently_and_in_any_completion_order`,
`a_third_concurrent_stage_request_is_rejected_locally_at_the_in_flight_cap`) are in phase 04; the
serving-side half of the cap is phase 03's
`a_third_concurrent_stage_request_on_one_connection_is_refused_as_busy`.

---

## Manual validation (predict section G gaps, section H script)

Automated tests cannot cover these. Run them on the VM before shipping.

1. Debug build talks to `herdr-dev`, not the installed stable server:
   `env -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH cargo run -- <command>`.
2. Deploy identical source to the VM and build there.
3. Mount via `workspace.mount_remote` over `herdr.sock`.
4. Copy a real PNG locally, focus a pane on the mounted remote workspace, trigger paste; confirm a
   **remote** path arrives and the remote agent can actually read that file. *(Covers: real SSH dial
   path, real OS clipboard read.)*
5. **Repeat against a VM left on an older commit** — the capability gate must degrade to a toast,
   not kill the mount. *(Covers: mixed-binary skew — loopback compiles both ends from one tree.)*
5b. **The other direction: new host, old controller.** Mount from an older-commit controller into a
   VM running this build and drive normal work for a few minutes; the newer host must never emit a
   `ClipboardStage*` frame, and the mount must stay up. *(Covers the serving-side half of the gate;
   only a real mixed-binary pair can prove it.)*
5c. **Stale mount-end after a fast remount.** Kill the link, remount immediately to the same running
   remote, paste; the paste must resolve. *(Covers the connection-epoch fence against a real delayed
   end-notice, which loopback timing rarely reproduces.)*
6. Inspect staged file permissions (0600 in a 0700 dir), the quota rejection path, and 24h cleanup
   on the VM. *(Covers: real remote filesystem permissions/quota/disk-full.)*
6c. **Real disk-full stage.** Point the remote's `TMPDIR` at a tiny full filesystem (a small tmpfs or
   a loop-mounted image) and paste; assert the disk-space toast **and** that no
   `federation-clipboard-`-prefixed file remains in that directory. *(Covers requirement 6b's
   remove-on-write-failure against a real ENOSPC rather than an injected writer.)*
6b. **Hostile/awkward remote `TMPDIR`.** Start the remote server with `TMPDIR` pointing at a path
   containing a space, and again at one containing `;`; the paste must fail with the
   "no usable temp folder" toast and **no file must appear** in that directory. *(Covers phase 02's
   staging-root contract against a real environment rather than a synthetic tempdir.)*
7. `just windows-lint` — federation is `#[cfg(unix)]`, but the Windows target still cross-compiles;
   new code must stub cleanly.

---

## Deferred / follow-up

- **Generic (non-image) file paste.** Explicit fast-follow, out of scope for v1.
- **Chunked payloads + flow control** to remove head-of-line blocking (A1 / P4). Separate design.
- **Federation-wide fencing gap (A2 / C2).** `mount_generation` is degenerate — always 1
  (`client.rs:129,208`; `serve.rs:44` const). `id::fence()` is bare equality (`id.rs:198`). This
  still lets a stale `SplitPaneResponse` through after a fast remount. Not fixed here; this feature
  works around it with a locally minted `MountConnectionEpoch` carried on its own events plus
  `FederationMountEnded`. Note the side effect, deliberate and called out at the ship gate: because
  `FederationMountEnded` now carries the epoch, a stale end-notice can no longer tear down a live
  remount for *any* consumer of that event, split-pane included.
- **Detached staging worker.** The remote-side staging worker is never joined (phase 03 step 3), so
  a filesystem call that hangs forever leaks one thread for the life of the server process. Bounded
  teardown was chosen over joining because joining pins the single-controller lease and blocks every
  later mount to that host. A watchdog/timeout on the staging op is the follow-up.
- **Pre-existing JSON payload inflation.** `TerminalChannelMessage::Output.bytes` and
  `ClipboardMessage.payload` are `Vec<u8>` over a `serde_json` codec (~4x inflation against their
  channel caps). Not touched; the new variant avoids it with base64 (phase 01).
- **TOCTOU on `ensure_staging_dir`** (`clipboard_image.rs:82-94` uses `fs::metadata`, follows
  symlinks). Pre-existing, outside the federation trust model; deliberately not fixed here.
- **Spinner / progress affordance** for slow transfers. v1 shows nothing under ~1.5s and a
  "saving image to remote host…" toast past it.
- **Mount-inside-bridge topology.** If an App running as the far end of `herdr --remote` itself
  mounts a remote workspace, the phase-05 intercept reads the App host's clipboard, not the user's.
  No App-side predicate for that state exists today; documented, not guarded (phase 05 req. 2).
- **Content-type enforcement on the remote.** The remote validates the extension allowlist but does
  no magic-byte check of its own; "images only" is client-enforced (`linux.rs:413`).

---

## Local build note

This workstation has no `just`/`cargo-nextest` and needs `ZIG=~/.local/zig-0.15.2/zig` for the
vendored libghostty-vt build; use `cargo test --test-threads=4` with that env, and treat the three
known baseline clippy errors as pre-existing. `just check` must still be run where available before
the change lands.
