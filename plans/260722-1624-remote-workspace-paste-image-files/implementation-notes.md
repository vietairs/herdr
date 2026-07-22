# Implementation Notes — remote-workspace image paste (federation)

Append-only. 4-line entries: What / Why / Evidence / Reversibility. Log decisions, deviations,
surprises the moment they happen.

Plan: `plan.md` (+ `phase-01`..`phase-05`). Branch: `feat/remote-workspace-paste-image-files`
(worktree `/Users/hvnguyen/Projects/herdr-worktrees/remote-workspace-paste-image-files`, base
`5ec2a10b`).

Design (locked): correlated stage-then-inject RPC mirroring `SplitPaneRequest`/`SplitPaneResponse`.
The client sends image bytes over the federation tunnel; the remote stages the file and returns its
remote path; the client injects that path through the existing local paste path. Images only in v1.
Filename synthesised as `image.{extension}` — the OS clipboard carries no filename.

Before writing code, read `plan.md`'s assumptions section and the phase file you are executing. The
sanitisation, capability-gating, and fencing contracts are load-bearing: they came out of four
adversarial review rounds, not from taste.

## Deviations / Decisions / Surprises

- What: Local build requires BOTH `ZIG=$HOME/.local/zig-0.15.2/zig` and
  `PATH=$HOME/.local/zig-0.15.2/xcrun-shim:$PATH`, not just the first.
  Why: Zig 0.15.2 cannot link against the macOS 27 SDK; the shim points it at the older CLT 15.1 SDK.
  Evidence: baseline `cargo build` in this worktree with ZIG alone → `undefined symbol: _waitpid`,
  `build.rs:78` panic, exit 2. With the shim on PATH: clean `dev` build in 42s, 2 warnings. Both runs
  done at base `5ec2a10b` with no source changes, so that is the pre-implementation baseline.
  Reversibility: N/A (environment). `just`/`cargo-nextest` are not installed locally; the usable proxy
  is `cargo test --bin herdr -- --test-threads=4`. `just check` (incl. `windows-lint`) must be
  discharged where `just` exists — phase 02 requirement 8 and phase 05 step 1 depend on windows-lint.

- What: Clippy baseline in this tree is 3 pre-existing `-D warnings` errors (map_out dead code,
  `Capability::CLIPBOARD` dead code, `pane_source.rs` type_complexity). Diff against these, do not
  treat them as regressions.
  Why: avoids chasing inherited failures as if this feature caused them.
  Evidence: recorded from prior sessions on this repo; re-verify with a clippy run before phase 01.
  Reversibility: N/A. NOTE: this feature adds `Capability::FILE_STAGING` and deliberately leaves the
  decorative unused `Capability::CLIPBOARD` alone (predict section D: naming the new capability for the
  operation, not the surface). Removing the dead one is a separate cleanup, not this feature's job.

### Phase 01

- What: Version verdict re-verified before relying on it. `PROTOCOL_VERSION = 17` in both
  `src/protocol/wire.rs:15` and `git show v0.7.5:src/protocol/wire.rs`; `git ls-tree v0.7.5
  src/remote/federation/` is empty. No bump to either counter.
  Why: the phase file makes skew safety rest entirely on capability gating; that only holds if no
  deployed peer can observe the additive variants.
  Evidence: commands above, run at the start of phase 01.
  Reversibility: N/A (verification only).

- What: Capability-negotiation tests 3 and 4 live in `protocol/mod.rs`'s test module, not
  `protocol/negotiate.rs`.
  Why: phase 01's file table is "modify `src/remote/federation/protocol/mod.rs`, no other file is
  touched"; `negotiate` is reachable from `mod.rs`'s tests as `negotiate::negotiate`, so the tests
  cost nothing to place there and the ownership table stays intact.
  Evidence: both tests green; `Capability::FILE_STAGING` and `negotiate()` are both in scope.
  Reversibility: trivial — move the two `fn`s into `negotiate.rs`'s test module verbatim.

- What: DEVIATION — one arm added to `src/remote/federation/client.rs` (a file phase 01 does not
  own): `ClipboardStageRequest | ClipboardStageResponse => tracing::warn!(...)` in `drive`'s
  exhaustive `match msg`.
  Why: adding the two `FederationMessage` variants breaks that exhaustive match, so the tree does not
  compile without it. plan.md anticipates exactly this ("narrow carve-outs where a type change forces
  a cross-phase compile dependency"). Chose warn-and-drop over `todo!()`/`unreachable!()`: this build
  never advertises `file_staging`, so any such frame is an unnegotiated optional-feature frame, and
  dropping it keeps the mount up instead of panicking a live link.
  Evidence: `error[E0004]: non-exhaustive patterns` at `client.rs:496` before the arm; clean build
  after. No other exhaustive match over `FederationMessage` broke.
  Reversibility: phase 03/04 replace this arm with real dispatch; deleting it is a 6-line revert.

- What: `Channel::largest_max_len()` folds over a new `Channel::ALL` const rather than an inline
  if-ladder over literal arms.
  Why: same effect, but one list instead of two, and the cap test iterates the same list — a new arm
  added to `ALL` is automatically covered by both. `Ord::max` is still not const, so the fold is a
  `while` loop, as the phase file requires.
  Evidence: `file_staging_channel_cap_is_the_largest_channel_cap` green; asserts largest == the
  `FileStaging` cap and `>=` every arm in `ALL`.
  Reversibility: inline the loop back into an if-ladder; no callers outside the test yet.
  KNOWN LIMIT, logged rather than solved: neither `ALL` nor the test is compiler-checked for
  exhaustiveness, so a future arm omitted from `ALL` would be invisible to both. A doc comment on
  `ALL` names it as the place to update. A truly airtight guard needs a match-based construction and
  was judged more machinery than the risk warrants.

- What: `#[allow(dead_code)]` on `Capability::FILE_STAGING` and `Channel::largest_max_len`.
  Why: phase 01 is additive with no production call sites until phase 03, so both are dead today and
  clippy `-D warnings` would report two errors beyond the 3-error baseline. Follows the precedent
  already in this file (`TerminalChannelMessage::terminal_id`, mod.rs:212). Deliberately placed on
  the individual items, NOT the `impl Capability` block, so the pre-existing `CLIPBOARD` dead-code
  error stays visible as part of the baseline.
  Evidence: build warnings back to the baseline 2; `cargo clippy --all-targets -- -D warnings` shows
  exactly the 3 known baseline errors.
  Reversibility: phase 03 removes both attributes when it wires the call sites.

- What: SURPRISE — `Channel::ALL` did not need its own allow. rustc reported "`ALL` and
  `largest_max_len` are never used" as one warning; silencing `largest_max_len` alone made `ALL` live.
  Why: dead-code analysis is transitive.
  Evidence: rebuild after the single attribute — 2 warnings, neither mentioning `ALL`.
  Reversibility: N/A.

- What: Test-quality check per the "would this pass with the feature stubbed" rule.
  Why: the four starred tests plus the two additional ones must each fail against a missing guard.
  Evidence: `a_base64_encoded_max_size_image_fits_the_file_staging_cap` fails against any cap below
  ~22.4 MiB (so it fails against a `Clipboard`-sized 16 MiB cap — it is the real guard against the
  `Vec<u8>` inflation, not a tautology). `file_staging_channel_cap_is_the_largest_channel_cap` fails
  if `largest_max_len` returns any other arm's cap. The cap test drives a real `FrameTooLarge` from
  `codec::decode`, not a manual length comparison. The roundtrip test iterates every
  `ClipboardStageFailure` variant, so a variant that fails to serialise is caught.
  Reversibility: N/A.

### Phase 02

- What: TDD executed literally — the module was first committed to the working tree as a permissive
  stub (accept every filename, extension always `png`, no root/candidate/quota/size guards, bare `?`
  after `create_new`) together with the full 16-test module.
  Why: the phase's test-quality rule ("would this still pass with the guard stubbed out?") is only
  provable by actually stubbing the guard. The stub deliberately reproduces the two behaviours the
  contract exists to forbid: `clipboard_image.rs:70`'s `png` fallback, and a partial file left behind
  by `write_all?`.
  Evidence: first run 15 of 18 red, each on its own guard — `bidi` returned
  `Ok(.../federation-clipboard-...-evil\u{202e}gnp.exe.png)`, `payload.sh` returned `Ok(...png)`,
  `a;b.png` staged successfully, oversized payload staged successfully. After the implementation:
  18/18 green.
  Reversibility: `git diff` on the single new file; no call sites exist yet.

- What: Step 5b's stem allowlist is applied to the whole (already separator-free) file name, not to
  the stem alone.
  Why: the allowlist contains `.`, so applying it to the full name is exactly equivalent for the stem
  and additionally constrains the suffix — one guard instead of a split that could disagree.
  Evidence: `stage_rejects_shell_metacharacters_in_original_filename` (18 hostile names) green, and
  `stage_preserves_the_original_filename_stem_behind_a_collision_proof_prefix` still accepts
  `héllo-wörld.JPEG`, so printable non-ASCII is unaffected.
  Reversibility: split into a stem-only check by slicing at the final `.` before the loop.

- What: Edge case the phase file does not cover — a name that is nothing but an extension (`.png`,
  empty stem after `rsplit_once`). Rejected as `InvalidFilename`.
  Why: conservative + smallest reversible option. The alternative (staging as
  `federation-clipboard-{unique}-{n}-.png`) is legal but produces a trailing-dash name nobody asked
  for, and accepting a name with no content is a strictly larger surface than rejecting it.
  Evidence: guarded by the `has no stem` early return in `sanitize_original_filename`.
  Reversibility: delete the four-line guard; nothing else depends on it.

- What: SURPRISE — test 13 (non-lossless-UTF-8 root) cannot create its directory on macOS. APFS
  rejects a `0xff` path component with `EILSEQ` (`Os { code: 92 }`), so `create_dir_all` panics
  before the assertion.
  Why: the test's original shape assumed a Linux-style byte-transparent filesystem.
  Evidence: first green run failed only on that test with `create test staging child dir: Illegal
  byte sequence`.
  Reversibility: the test now builds the path without creating it and asserts `StagingUnavailable`
  plus an empty parent. It still discriminates: an implementation without the root check reaches
  `create_new` on a missing directory and returns `WriteFailed`, not `StagingUnavailable`.

- What: Test 10 (permissions) is the only test that touches the real shared euid-scoped staging dir;
  it calls the public `stage_remote_clipboard_image` and removes its file afterwards.
  Why: the 0700 directory mode is `ensure_staging_dir`'s guarantee. Asserting it against a
  test-created `tempdir` would only assert the test's own `set_permissions` call — a tautology.
  Requirement 2's no-shared-dir rule exists to stop `TMPDIR` *mutation*; a single prefixed file that
  is read back and deleted races nothing.
  Reversibility: switch to `stage_into` + a self-chmodded dir and drop the dir-mode assertion.

- What: Quota test uses `File::set_len` sparse files (600 MiB apparent, ~0 bytes allocated) rather
  than writing 500 MiB.
  Why: `staging_dir_total_bytes` sums `metadata().len()`, which is the apparent size, so a sparse
  file is a faithful fixture and the test stays sub-millisecond.
  Evidence: `stage_rejects_a_write_that_would_exceed_the_staging_directory_quota` green, including
  the sibling assertion that a 600 MiB *non*-prefixed neighbour does not consume the quota.
  Reversibility: replace with real writes if a future filesystem misreports sparse lengths.

- What: `#[allow(dead_code)]` on `stage_remote_clipboard_image` only.
  Why: same precedent as phase 01. Every other item (including `stage_into`,
  `is_injection_safe_path`, the prefix constant) is transitively reachable from it, so one attribute
  silences the whole module and no item is over-suppressed.
  Evidence: `cargo build` back to the baseline 2 warnings; `cargo clippy --all-targets -- -D warnings`
  shows exactly the 3 baseline errors.
  Reversibility: phase 03 removes the attribute when it wires the call site.

- What: `sync_all()` added after `flush()` in the write sequence, beyond the phase file's
  "write → flush".
  Why: requirement 6b names "write, flush, or sync" as failures that must clean up; a stage whose
  data is still only in the page cache when the path is handed to the peer is a real, if narrow,
  torn read. It goes through the identical `remove_partial` path, so it cannot violate the
  no-artifact contract.
  Evidence: `a_failed_write_leaves_no_partial_file_on_disk` green, and the positive control in the
  same test proves the success path still leaves exactly one file.
  Reversibility: delete the three-line `if let Err` block.

- What: Verification vs baseline — `cargo build` 2 warnings (unchanged); `cargo fmt --check` clean
  (`cargo fmt` reformatted four spots in the new file); `cargo clippy --all-targets -- -D warnings`
  exactly the 3 baseline errors, none in `file_staging.rs`; `cargo test --bin herdr --
  --test-threads=4` 2978 passed / 2 failed, both the known shared-global flakes
  (`api::server::pane_graphics_stream::tests::inactive_owner_cancels_idle_stream_and_dispatches_close`,
  `workspace::tests::generated_workspace_ids_are_short_base32_handles`), both green in isolation.
  Why: phase 02 must not move any of these numbers.
  Evidence: commands and outputs above.
  Reversibility: N/A. NOT discharged locally: `just windows-lint` (requirement 8) — the module is
  `#[cfg(unix)]` at both the declaration and the module body, but only a real Windows cross-compile
  proves it stubs cleanly.

- What: DEVIATION — the non-test body is 387 raw lines / 234 non-comment non-blank lines, over
  requirement 1's "under 200 LOC excluding tests".
  Why: the overage is the ordered contract itself. Requirement 3 mandates eight separate named
  early-returns that must not be collapsed, each carrying a comment stating its invariant, plus a
  `tracing::warn!` per rejection reason. Compressing them back under 200 would mean either merging
  guards (which the phase forbids, and which is exactly how an ordering bug gets reintroduced) or
  deleting the comments that make the ordering legible.
  Evidence: `head -387 file_staging.rs | grep -vE '^\s*(//|$)' | wc -l` = 234; the module doc block
  alone is 36 lines.
  Reversibility: extract `sanitize_original_filename` + its helpers into a sibling
  `file_staging/filename.rs` if the budget is treated as hard; no behaviour changes.

### Phase 03

- What: SURPRISE — phase 03's production code was already present in the working tree when this
  session started (epoch, mirror caps, both gates, worker, `serve.rs` arm), with no tests, no
  `app/**` carve-outs, and no notes entry.
  Why: an earlier interrupted attempt. The tree did not compile: `AppEvent::FederationMountEnded`'s
  new field and the three new variants broke `actions.rs`, `api.rs`, `api/workspaces.rs`.
  Evidence: `cargo build` before any edit → 3 errors (E0004/E0027/E0063) in exactly those files.
  Reversibility: N/A. Consequence for method: TDD could not be run literally forwards. Every guard was
  instead verified by MUTATION — stub the guard, watch the named test go red, revert. Six mutations,
  all recorded below.

- What: Mutation log (each: mutate → run → red → revert; all reverted, tree clean).
  Why: the "would this test still pass with the guard stubbed out?" rule, applied after the fact.
  Evidence: (1) `run_connection`'s capability check → `if true` →
  `a_host_without_the_agreed_file_staging_capability_never_emits_a_stage_frame` FAILS
  ("a stage frame reached the peer: Failed { request_id: 1, failure: WriteFailed }").
  (2) `try_send` error path → `std::mem::forget(err)` →
  `a_request_refused_by_a_full_staging_queue_still_returns_its_permit` FAILS (live count 1, want 0).
  (3) `StagePermit::acquire`'s cap check → `if false` →
  `a_third_concurrent_stage_request_on_one_connection_is_refused_as_busy` FAILS (no Busy frame).
  (4) `serve.rs`'s capability check → `if false` →
  `a_client_that_never_advertised_file_staging_receives_no_stage_frame` FAILS (host answered
  `Staged`). (5) filesystem call inserted before the base64 decode →
  `a_malformed_base64_payload_is_reported_as_invalid_payload_without_touching_the_filesystem` FAILS.
  (6) worker joined at teardown → `a_blocked_staging_operation_does_not_delay_connection_teardown`
  FAILS at its 5s bound.
  Reversibility: N/A.

- What: The gate test's injected op had to be changed from a panicking op to a counting op that
  answers.
  Why: with a panicking op, mutation (1) did NOT turn the test red — the ungated worker panicked
  before writing anything, so the wire stayed as quiet as a correctly gated host and the test passed.
  A test that a stubbed gate survives is exactly the failure mode the rule exists for.
  Evidence: first mutation run "ok"; after the change, the same mutation fails with the frame printed.
  Reversibility: revert to a panicking op only if the assertion is replaced by something else.

- What: `a_blocked_staging_operation_does_not_delay_connection_teardown` runs `finish()` on its own
  thread and waits with `recv_timeout`.
  Why: the joining mutation does not make teardown slow, it makes it never return; an in-line call
  hung the whole test binary instead of reporting a failure (observed: "has been running for over
  60 seconds").
  Evidence: after the change the same mutation reports FAILED in 5.01s.
  Reversibility: inline the call; accept a hang as the failure signal.

- What: DEVIATION — `StagingOp` is `Arc<dyn Fn(..) + Send + Sync>`, not the phase file's bare `fn`
  pointer.
  Why: a `fn` pointer cannot capture, so every parked-op test would have to rendezvous through one
  process-global channel and the three of them would race each other in the same test binary. The
  indirection is still a single field with no trait and no extra layering.
  Evidence: three tests each own their own park/release channels and pass under `--test-threads=4`.
  Reversibility: swap the type back and hoist the rendezvous into a `static`; call sites are two.

- What: DEVIATION from requirement 3b's premise — `server/federation_accept.rs` needs NO internal
  `#[cfg(unix)]`.
  Why: the phase file states `src/server/mod.rs:8` declares it unconditionally. It does not:
  `src/server/mod.rs:7-8` is `#[cfg(unix)] pub(crate) mod federation_accept;`. The whole module is
  therefore already Unix-only, and gating its staging code again would be noise. `serve.rs` IS
  declared unconditionally (`federation/mod.rs:47`), so its arm, and `FixtureHost`'s advertisement,
  are gated as the requirement asks.
  Evidence: `grep -n federation_accept src/server/mod.rs`; Windows cross-compile below.
  Reversibility: add the redundant gates if the module declaration ever changes.

- What: Two Windows-only faults that WERE mine, found and fixed by actually running the cross-compile:
  the `ClipboardStageResponse` arm in `drive_mount_channel` names Unix-only `AppEvent` variants, and
  `serve::handle_inbound`'s `agreed_capabilities` is unread off Unix. Fixed with a `#[cfg(not(unix))]`
  drop-and-log arm, a `#[cfg(unix)]` import, and `#[cfg_attr(not(unix), allow(unused_variables))]`.
  Why: `client.rs` and `serve.rs` both compile on Windows.
  Evidence: `LIBGHOSTTY_VT_SIMD=false cargo clippy --bin herdr --locked --target x86_64-pc-windows-msvc
  -- -D warnings` named all three; after the fix none of the remaining errors mentions a symbol this
  phase introduced.
  Reversibility: each fix is 3-6 lines.

- What: SURPRISE — the Windows target does NOT build at this fork's base commit either. 10 errors at
  `5ec2a10b` (`FederationSplitPaneReady/Failed`, `FederationResyncPaneCreated/Removed`,
  `run_federation_serve_bridge`, `spawn_basic_detection_task`,
  `federation_host_key_for_workspace`).
  Why: pre-existing fork breakage, not this feature's.
  Evidence: `git stash push` → the identical 10 errors → `git stash pop`. So `just windows-lint` cannot
  be green on this branch regardless, and this phase adds zero new Windows errors. NEEDS A HUMAN
  DECISION: the plan's acceptance criterion 6 assumes windows-lint can pass.
  Reversibility: N/A (verification only).

- What: `AppEvent`'s three new variants carry `#[allow(dead_code)]`.
  Why: nothing reads their fields until the pending-stage map exists; without it the build is 5
  warnings against a baseline of 2. Same precedent as phases 01/02.
  Evidence: `cargo build` back to exactly the 2 baseline warnings.
  Reversibility: delete three attributes when the handler lands.

- What: `handle_federation_mount_ended`'s five existing test call sites pass
  `MountConnectionEpoch::UNMOUNTED`.
  Why: those fixtures build their mirror with `RemoteMirror::new`, whose epoch IS `UNMOUNTED`, so the
  value is the truthful one and stays correct when the real epoch fence replaces the debug line.
  Evidence: all five tests still green.
  Reversibility: read the epoch off `app.state.remote_mirrors` instead.

- What: Verification vs baseline — `cargo build` 2 warnings (unchanged); `cargo fmt --check` clean;
  `cargo clippy --all-targets -- -D warnings` exactly the 3 baseline errors (`map_out`,
  `Capability::CLIPBOARD`, `pane_source` type_complexity — none disappeared, since the two allows this
  phase removed were on `FILE_STAGING`/`largest_max_len`, not on these); `cargo test --bin herdr --
  --test-threads=4` 2990 passed / 2 failed, both the known shared-global flakes
  (`pane_graphics_stream::...::inactive_owner_cancels_idle_stream_and_dispatches_close`,
  `workspace::tests::generated_workspace_ids_are_short_base32_handles`), both green in isolation.
  Why: phase 03 must not move these numbers.
  Evidence: commands and outputs above. Windows: cross-compile run locally (see above), not `just`.
  Reversibility: N/A.

## Windows target is already broken at the fork's base commit

**What:** `just windows-lint` / plan acceptance criterion 6 cannot pass on this fork. A Windows
cross-compile fails with 10 errors at base `5ec2a10b`, before any work in this plan dir. This
feature adds zero new ones.

**Why it matters:** criterion 6 ("`just check` green incl. windows-lint") was written assuming a
green baseline. It is unreachable as stated, so it must be restated as "adds no NEW Windows
errors" or fixed by separate work outside this feature's scope.

**Evidence:** `src/remote.rs:1-2` declares `mod unix` under `#[cfg(unix)]`;
`run_federation_serve_bridge` is defined only at `src/remote/unix.rs:749`; `src/main.rs:505` calls
it with no `cfg` guard, so the name is unresolved on Windows. Verified by reading base blobs via
`git show 5ec2a10b:...`, independently of the phase-03 agent's `git stash` cross-compile run
(which reported the same 10 errors at base). Other named breaks: `FederationSplitPane*`,
`FederationResyncPane*`, `spawn_basic_detection_task`, `federation_host_key_for_workspace`.

**Reversibility:** n/a — a finding, not a change. Carry to the before-merge gate as a scope
question for the user: restate the criterion, or fix the fork's Windows build separately.

### Phase 04

- What: DEVIATION — the ~1.5s "saving image to remote host…" affordance ships as constants + a pure
  predicate (`should_raise_slow_stage_toast`) + `App::raise_slow_stage_toast_if_pending`, but NO
  spawned sleep task delivers it.
  Why: a spawned task can only reach `&mut App` through an `AppEvent`, and there is no generic
  "raise a toast" variant; the three `ClipboardStage*` variants all resolve a pending entry. Adding a
  fourth variant means editing `src/events.rs`, which the phase-04 ownership table assigns to phase
  03 and marks as not-to-be-touched. Took the conservative option: everything except the delivery
  hop, so whoever adds the variant wires one call.
  Evidence: `grep -n "Toast" src/events.rs` is empty; `App::event_tx` is the only background→App path.
  Reversibility: add `AppEvent::FederationClipboardStageStillRunning { request_id }`, forward it in
  `app/api.rs` to the existing `raise_slow_stage_toast_if_pending`, and spawn the sibling sleep in
  `begin_remote_clipboard_stage`. ~15 lines. NEEDS A HUMAN DECISION on who owns that edit.

- What: DEVIATION — the named `\n`/`\r` guard is placed ABOVE the general control-byte guard, not
  below it as the requirement enumerates.
  Why: behaviourally identical (both reject), but it makes each guard individually falsifiable. In
  the enumerated order the line-break guard is unreachable for its own inputs, and a mutation
  deleting it left the test green — the exact "would this pass with the guard stubbed out?" failure.
  Evidence: first mutation run, line-break guard neutered → test still `ok`. After the reorder plus
  reason-pinning assertions, the same mutation FAILS.
  Reversibility: swap the two blocks back and drop the two `assert_eq!(..., Err(PathRejection::...))`
  lines.

- What: SURPRISE — guards 3, 4 and 5 of the returned-path contract are all subsumed by guard 6
  (`is_injection_safe_path`), which rejects every control byte and every disallowed character on its
  own. Handler-level "nothing was written" assertions therefore cannot distinguish them.
  Why: matters because four starred tests target those guards specifically.
  Evidence: control-byte guard neutered → the ESC test still passed on the handler assertion alone.
  Reversibility: each affected test now carries a `sanitize_returned_remote_path` assertion pinning
  the exact `PathRejection`, in addition to (never replacing) the handler-driven assertion.

- What: Test 6c and its positive sibling live in `src/app/remote_clipboard_stage.rs`'s test module,
  not in `app/api/workspaces.rs`'s.
  Why: the ownership table gives phase 04 only the handler *bodies* in that file.
  `handle_federation_mount_ended` is `pub(crate)`, so it is drivable from anywhere in the crate and
  the test costs nothing to place in the owned file.
  Evidence: both tests green; `workspaces.rs`'s own test module is untouched.
  Reversibility: move the two `fn`s verbatim.

- What: `test_app()` sets `toast_config.delivery = Herdr`.
  Why: the shipped default is `ToastDelivery::Off` (`config/model.rs:1037`), which drops every
  notification, making "a failure was reported" unobservable and every failure assertion vacuous.
  Evidence: with the default, 11 of the 22 tests failed on "was rejected without telling the user".
  Reversibility: none needed; without it the phase's whole failures-always-toast contract is untested.

- What: The five existing `handle_federation_mount_ended` tests still pass unchanged under the new
  epoch fence.
  Why: their mirrors come from `RemoteMirror::new` (epoch `UNMOUNTED`) and phase 03 already made
  their call sites pass `UNMOUNTED`, so the fence compares equal.
  Evidence: full suite, no `workspaces.rs` test regressed.
  Reversibility: N/A.

- What: Mutation log — every guard stubbed, named test observed red, mutation reverted.
  Why: the "would this test still pass with the guard stubbed out?" rule.
  Evidence: (M1) line-break guard → `if false` → newline test FAILS (`assertion left == right`,
  got `ControlByte`). (M2) control-byte guard → `&& false` → ESC test FAILS (got
  `DisallowedCharacter`). (M3) staging-prefix guard neutered → prefix test FAILS ("/etc/passwd
  reached the pane"). (M4) allowlist guard neutered → metacharacter test FAILS. (M5) `try_send_paste`
  hoisted ABOVE `sanitize_returned_remote_path` (the real unbracketed-PTY hole) → ALL FOUR rejection
  tests FAIL — this is what proves they are handler-driven, not sanitizer-only. (M6) `try_send_paste`
  result discarded → delivery-failure test FAILS. (M7) epoch fence neutered → both stale-response
  tests FAIL (`assertion failed: rx.try_recv().is_err()`). (M8) origin fence neutered → foreign-host
  test FAILS. (M9) claim hoisted above the fence → foreign-host test FAILS on "a foreign answer must
  not evict the legitimate one". (M11) origin purge ignores the epoch → purge test FAILS on the fresh
  remount survivor. (M12) in-flight cap 2 → 4 → cap test FAILS. (M13) budget made fixed →
  proportionality test FAILS. (M14) workspace purge removes everything → purge test FAILS on the
  survivor. (M15) mount-ended epoch fence neutered → delayed-mount-ended test FAILS.
  Reversibility: N/A. All mutations reverted; `grep -n "if false\|&& false"` over both edited files
  is empty.

- What: SURPRISE — the mutation harness crashed mid-run with `OSError: [Errno 28] No space left on
  device` while rewriting a source file, leaving one mutation applied in each of two files.
  Why: the machine's root volume hit 122 MiB free during the campaign.
  Evidence: both were found and hand-reverted; the files were verified intact (`grep -c ""`, tail,
  and a subsequent clean `cargo fmt --check` + full test run).
  Reversibility: N/A. Lesson: a scripted mutate/revert loop should write via a temp file + rename.

- What: Verification vs baseline — `cargo build` 2 warnings (`map_out`, `Capability::CLIPBOARD`);
  `cargo fmt --check` clean; `cargo clippy --all-targets -- -D warnings` exactly the 3 baseline
  errors, none in the new file; `cargo test --bin herdr -- --test-threads=4` 3013 passed / 1 failed
  (`workspace::tests::generated_workspace_ids_are_short_base32_handles`, a known shared-global flake,
  green in isolation); Windows cross-compile `cargo clippy --bin herdr --locked --target
  x86_64-pc-windows-msvc -- -D warnings` exactly the 10 pre-existing errors, none naming a symbol
  this phase introduced. 22 new tests, all green.
  Why: phase 04 must not move these numbers.
  Evidence: commands and outputs above. One earlier full-suite run showed 6 failures including four
  `server::headless` tests; all passed on re-run and are load-sensitive, not caused by this phase.
  Reversibility: N/A.

## Slow-transfer toast: phase 05 gets a narrow events.rs carve-out

**What:** phase 04 shipped `SLOW_STAGE_TOAST_DELAY`, `raise_slow_stage_toast_if_pending`, and its
tests, but no production call site — a background task can only reach `&mut App` through an
`AppEvent`, and phase 04's ownership table forbade touching `events.rs`. Rather than strip the
machinery or leave it dead, phase 05 is granted a carve-out to add the one `AppEvent` variant plus
the delayed emit.

**Why:** without it a multi-MiB image over SSH gives the user no feedback for seconds, and the
natural response — paste again — is then refused by the in-flight cap as `Busy`. The silence is a
failure mode this feature introduces, so the toast is corrective, not decorative. Stripping it
would also discard work already proven non-vacuous by mutation.

**Evidence:** `grep` for both symbols across `src/` returns only the definitions and two test call
sites (`remote_clipboard_stage.rs:1110,1114`). Build stayed at the 2-warning baseline because the
test call sites suppress `dead_code`, so the gap is invisible to the compiler.

**Reversibility:** high — delete the variant, its emit, the constant, the method and its two
tests. Nothing else depends on them.

### Phase 05

- What: DEVIATION — `AppState` gained one field, `remote_image_paste_key: Option<(KeyCode,
  KeyModifiers)>`, touching `src/app/state.rs` (struct + `test_new`) and `src/app/mod.rs`
  (`App::new` + the config-reload keys block), neither of which phase 05 owns.
  Why: requirement 1 mandates a decision function pure over `&AppState`, but the binding lived only
  on `Config`, which `App` does not hold. Reading it from disk on the key path or hardcoding the
  default were both worse. Mirrors the `prefix_code`/`prefix_mods` precedent exactly, reload
  included, so a `herdr server reload-config` picks up a rebind.
  Evidence: `grep -rn remote_image_paste src/` before the change showed the key reachable only
  through `client/mod.rs`; `Keybinds` has no such field.
  Reversibility: delete the field and its four assignment sites; the decision function then needs the
  binding passed in from a caller that has a `Config`.

- What: DEVIATION — four one-word visibility changes in phase 04's `remote_clipboard_stage.rs`
  (`TOAST_TITLE_FAILED`, `TOAST_TITLE_SAVING`, `raise_clipboard_stage_toast`, plus dropping four
  now-false `#[allow(dead_code)]`s), and the `SLOW_STAGE_TOAST_DELAY` spawn inside
  `begin_remote_clipboard_stage`.
  Why: the intercept raises three toasts of its own and the alternative was a third copy of the
  30-line `ToastDelivery` match already duplicated in `creation.rs`. The spawn is the granted
  carve-out's "delayed emit at request-mint time".
  Evidence: build back to the 2-warning baseline with every `allow` removed, so none of them was
  masking anything else.
  Reversibility: re-add the `pub(crate)`s' removal and inline a local toast helper; delete the spawn.

- What: The `events.rs` carve-out also required an arm in `src/app/actions.rs`'s exhaustive
  `match ev` (`AppEvent::FederationClipboardStageStillRunning { .. } => Vec::new()`).
  Why: compile necessity, not scope creep — the same exhaustive match every earlier phase's variants
  had to extend.
  Evidence: `error[E0004]` without it.
  Reversibility: three lines.

- What: Test 4 pins phase 04's shipped copy, not the phase-file draft copy.
  Why: requirement 6's literal strings never shipped — phase 04 declared its own wording, and phase
  05 may not edit that file. The required *mapping* semantics all hold (Busy→busy,
  InvalidFilename→file name, InvalidPayload→could-not-read, StagingUnavailable→no temp folder,
  Quota/WriteFailed→storage). The test asserts each variant's exact shipped words and drives them
  through the real `FederationClipboardStageFailed` handler, so a copy change must be deliberate.
  Evidence: mutating `Busy`'s context to `WriteFailed`'s made the test fail with both strings printed.
  Reversibility: retype the table if the wording is ever aligned to the phase file.

- What: The three phase-05-owned toast strings are shorter than requirement 6's drafts.
  Why: the drafts do not fit. "clipboard has no image herdr can paste (png/jpg/gif/webp/bmp only)" is
  66 display cells against a ~60-cell hard clip with no ellipsis; it ships as "clipboard has no image
  (png/jpg/gif/webp/bmp)" (44). "remote herdr is too old to support image paste; update it" became
  "remote herdr is too old for image paste; update it" (49).
  Evidence: `every_clipboard_stage_toast_string_fits_the_status_line` measures every string with
  `UnicodeWidthStr::width`; padding one past 60 makes it fail with the measured width.
  Reversibility: edit the three constants; the width test keeps the ceiling honest.

- What: SURPRISE — the "no wire send" assertion in the Unsupported test was initially vacuous.
  Why: a remote runtime hands user input to a bounded queue drained by a spawned task
  (`federation/pane_source.rs:133-141`), so an immediate `try_recv` reports "empty" for a key that
  was in fact forwarded a moment later. A mutation that kept the toast but dropped the `return`
  passed.
  Evidence: that mutation ran green; after switching to `assert_no_frame` (a 250 ms
  `timeout(rx.recv())`) the same mutation fails with `the mount received
  Some(Terminal(Input { ..., bytes: [22] }))`.
  Reversibility: N/A — the helper is strictly stronger.

- What: `fall_through_still_reaches_non_terminal_mode_handlers` puts the focus on a live *remote*
  mount and binds the trigger to navigate mode's own `down` key.
  Why: with a local workspace the mode guard is not load-bearing and deleting it left the test green.
  With a remote mount, deleting the guard turns the decision into `Capture`, the key is consumed, and
  the selection never moves.
  Evidence: mode-guard mutation → FAILS (`left: Capture {...}, right: FallThrough`).
  Reversibility: N/A.

- What: `original_filename` is not synthesised in `handle_remote_image_paste`.
  Why: requirement 2b's `format!("image.{}", image.extension)` already exists inside phase 04's
  `begin_remote_clipboard_stage`; doing it twice would mean two sources of truth for the same field.
  The test still asserts `"image.png"` on the outbound `ClipboardStageRequest`.
  Evidence: blanking that format string turns three phase-05 tests red.
  Reversibility: N/A.

- What: `docs/next/.../configuration.mdx` gained prose but no new heading.
  Why: `scripts/docs_translation_parity.py` compares heading outlines, so a new English `##` would
  require writing matching ja and zh-cn headings. Conservative smallest-reversible choice: extend the
  existing keybindings section.
  Evidence: parity on `docs/next` reports only a PRE-EXISTING `persistence-remote.mdx` mismatch
  (English h2=7 vs translated h2=6) in both locales; `configuration.mdx` is not named. Stable-tree
  parity (the script's default root) is clean. `config_reference_check.py` exits 0.
  Reversibility: promote to a `###` and add the two translated headings.

- What: Verification vs baseline — `cargo build` 2 warnings (`map_out`, `Capability::CLIPBOARD`);
  `cargo fmt --check` clean; `cargo clippy --all-targets -- -D warnings` exactly the 3 baseline
  errors, none in phase-05 code; `cargo test --bin herdr -- --test-threads=4` 3022 passed / 2 failed,
  both the known shared-global flakes; Windows cross-compile exactly the 10 pre-existing errors, none
  naming a symbol this phase introduced. 10 new tests, all green.
  Why: phase 05 must not move these numbers.
  Evidence: commands and outputs above.
  Reversibility: N/A.

## Two phase-05 ownership/copy calls resolved by the controller

**What:** (1) the new `AppState.remote_image_paste_key` field crosses phase 05's ownership table
into `state.rs`/`mod.rs` — ACCEPTED. (2) Phase-05 requirement 6's toast strings were shortened —
ACCEPTED.

**Why:** (1) ownership tables are a controller device for keeping parallel phases off each other's
files, not a user-approved contract; phases are sequential here so there is no collision, and the
alternative (threading the binding through the call site) makes the decision function impure and
unassertable without a running event loop. (2) The plan's copy is physically undeliverable: the
status line renders title and context as single Lines with no `.wrap()`, so ratatui hard-clips at
~60 cells with NO ellipsis, and one drafted string measures 66. Shipping it would truncate an
error message mid-word. Shortened copy is the only way to honour the requirement's intent.

**Evidence:** `src/ui/status.rs:108-127` (no `.wrap()`); measured strings now
`remote herdr is too old for image paste; update it` (49), `clipboard has no image
(png/jpg/gif/webp/bmp)` (45), `image is over 16MB, herdr's remote paste limit` (46), all under the
clip. Fall-through verified at `src/app/input/mod.rs:93-124`: only `Unsupported` and `Capture`
return, so a local pane and every non-terminal mode keep byte-identical behaviour.

**Reversibility:** (1) medium — moving the binding back to the call site means rewriting the
decision function's signature and its tests. (2) trivial — the strings are constants; surface the
final wording at the merge gate for the user to overrule.

## Staging worker response routed through enqueue_outbound
- What: `staging_worker_loop` now takes `first_cause` and sends its `ClipboardStageResponse` via `enqueue_outbound` instead of a bare `let _ = tx.try_send(...)`; `spawn_staging_worker` threads the cell in.
- Why: a full egress queue silently discarded a *successful* stage answer — file written on the host, controller's pending entry claimed, nothing able to retry, controller reporting "the remote host did not answer in time".
- Evidence: `a_stage_response_that_cannot_be_queued_tears_the_link_down_instead_of_vanishing` fails (`left: None, right: Some(EgressOverflow)`) when the send is reverted to `try_send`, and its positive control fails when the worker tears down unconditionally.
- Reversibility: local to two functions plus one call site; reverting restores the drop-on-full behaviour.

## Staging root proven owned before it is created, chmod'd, or swept
- What: `ensure_staging_dir` split into `ensure_staging_dir_at(dir)`, which `symlink_metadata`s the root and refuses (never repairs) anything that is a symlink, not a directory, owned by another uid, or group/world accessible; `create_dir_all` replaced by a `DirBuilder` with mode 0700 and re-verified after creation.
- Why: `create_dir_all`/`metadata`/`set_permissions` all follow symlinks, so on a shared Linux `/tmp` another user could pre-create `herdr-clipboard-images-<euid>` pointing at `~/.ssh`; the federated paste path made that reachable without any local user action, and `cleanup_stale` then deletes every entry older than 24h inside the target.
- Evidence: deleting the symlink/not-a-directory guard, the uid guard, or the mode guard each fails a distinct test; swapping `symlink_metadata` back to `metadata` fails `ensure_staging_dir_at_refuses_a_symlinked_root_without_touching_the_target` (target still 0755, its file still present).
- Reversibility: contained in `clipboard_image.rs`; `ensure_staging_dir()` keeps its old signature and the local paste path is unchanged apart from the stricter root.

## Root shape validated before the federated path touches the filesystem at all
- What: `stage_remote_clipboard_image` now calls `staging_dir()` + `validate_staging_root` before `ensure_staging_dir_at`, and the module header's first ordering invariant was rewritten to state what is actually true.
- Why: the header claimed every rejection is decided while the filesystem is untouched, but `ensure_staging_dir` created and chmod'd the root before `stage_into` validated it.
- Evidence: moving `validate_staging_root` after `cleanup_stale` in `stage_into_with` fails `an_unsafe_staging_root_is_rejected_before_its_contents_are_swept`; that test's positive control (a stale file in an acceptable root IS swept) dies if the fixture stops being backdated.
- Reversibility: three lines in `stage_remote_clipboard_image` plus a doc comment.

## One character allowlist covering Cf, Zl, Zp, Zs and the tag block
- What: `is_allowed_name_char` no longer falls back to `!ch.is_control()` for non-ASCII; non-ASCII is accepted unless it is in an explicit table of format, separator, non-ASCII space, and `U+E0000..=U+E007F` tag characters. Both the proposed file name and the returned path go through it.
- Why: `char::is_control` is category Cc only, so ZWSP, BOM, U+2028, NBSP and the tag block passed the returned-path check even though the file-name half rejected bidi overrides. The returned path is rendered in the pane the user eyeballs and read by the agent in it; tag characters encode ASCII invisibly, which is prompt injection the user cannot see.
- Evidence: restoring the old predicate fails all three new tests, first on ZWSP (`left: Ok(".../federation-clipboard-...-evil\u{200b}diagram.png")`); ordinary international names (`日本語-Ünicode-Ωmega.png`) still stage, and the bidi subset is pinned against drift.
- Reversibility: one predicate plus one table in `file_staging.rs`; widening it is a one-line edit.

## Post-creation write, flush and sync each pinned to their own cleanup
- What: the single `write` seam became a `FileWrite` struct carrying `write`/`flush`/`sync`, so each branch's "remove the file before returning" can be exercised alone.
- Why: the previous test only injected a failing `write`, so deleting the cleanup from the flush or sync branch still passed.
- Evidence: dropping `remove_partial` from each branch in turn fails `every_step_after_the_file_exists_removes_it_when_it_fails` with that step named (`a failing flush left a file behind: [...]`).
- Reversibility: production behaviour is unchanged; `FileWrite::production()` is the only value used outside tests.
