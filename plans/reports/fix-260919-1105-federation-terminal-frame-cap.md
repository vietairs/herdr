# Federation Terminal frame cap fix

## Root cause (file:line evidence)

`FederationCommand`'s `Open` handler builds the outbound `TerminalChannelMessage::Open`
frame by calling `host.scrollback_replay(&terminal_id)`
(`src/remote/federation/serve.rs:544-556`), which for a live pane resolves to
`AppFederationHost::scrollback_replay` -> `federation_actor::scrollback_replay`
(`src/server/federation_actor.rs:927-940`), returning
`_runtime.handoff_history_ansi()` as raw bytes with **no size cap and no
chunking**. The full ANSI scrollback (with styling) is placed unbounded into
`ScrollbackReplay { bytes }` and sent as one `Open` frame
(`src/remote/federation/protocol/mod.rs:288-292`).

`codec::encode` (`src/remote/federation/protocol/codec.rs:54-73`) never checks
any channel cap at all — only `codec::decode` does, on the receive side
(`codec.rs:106-113`). So there is zero sender-side enforcement; whatever
scrollback exists gets serialized and sent as-is.

`Channel::Terminal`'s cap was `2 * 1024 * 1024` (old
`src/remote/federation/protocol/mod.rs:780`), too small for a real full
scrollback with heavy styling (observed 2.3-2.8 MB on the wire, i.e.
serde_json-encoded, since the codec is JSON not bincode
(`codec.rs:55-58`)). Both the sync and async frame readers treat exceeding
the message's own channel cap as fatal — `io::Error` torn all the way up,
killing the whole federation connection:
- `src/server/federation_accept.rs:159-163` (`read_frame_blocking`)
- `src/remote/federation/serve.rs:198-206` (`read_frame`)

## The fix

Raised `Channel::Terminal::max_len()` from 2 MiB to **8 MiB**
(`src/remote/federation/protocol/mod.rs:780-787`), matching the sibling
`Channel::Mount` cap. That's ~3x the observed maximum wire frame (2.78 MB),
genuine headroom without adopting an arbitrary new number, and it reuses an
already-audited/tested cap tier in this codebase. Added a doc comment
explaining the invariant (unbounded scrollback replay, observed real-world
max) rather than referencing any plan/ticket ID.

## Oversize-is-fatal: can it degrade instead?

Not as a contained change — deferred as a follow-up, not implemented.

Both `read_frame_blocking` (sync, `federation_accept.rs`) and `read_frame`
(async, `serve.rs`) are the single choke point used by every caller,
including one-shot handshake/response reads
(`federation_accept.rs:1820,1879,1906,1967,2540,2547`) as well as the
long-lived per-connection loop
(`federation_accept.rs:527,2751,2774`; `serve.rs:235,303`). Making an
oversize `Terminal` frame "drop and continue" instead of erroring would
require changing these functions' return contract (they currently return
`io::Result<Option<FederationMessage>>` where the cap violation is
indistinguishable, by design, from a hard protocol/IO failure) and touching
every call site to know whether "None but keep looping" is valid there or
not — it is not valid for the one-shot handshake reads, which expect an
immediate typed response. That is materially more than a couple of call
sites, so per KISS/YAGNI this was deferred.

Also: bumping the cap to 8 MiB already fixes every failure observed in the
log (max observed was 2.78 MB, comfortably under 8 MiB), so the fatal path
is no longer reachable for this scenario. Recommended follow-up if oversize
Terminal frames recur despite the larger cap: add a `SnapshotRequest`-style
resync signal specifically for terminal scrollback so a dropped `Open` can
be retried without tearing down the whole mount, rather than reusing the
generic frame reader's error path.

## FEDERATION_PROTOCOL_VERSION determination

No bump made. Evidence:
- Current source value: `FEDERATION_PROTOCOL_VERSION: u32 = 7`
  (`src/remote/federation/protocol/mod.rs:93`).
- Latest released fork tag with this file is `v0.9.0-hvn.2`; at that tag the
  constant is also `7` (`git show v0.9.0-hvn.2:src/remote/federation/protocol/mod.rs`).
  (Upstream tag `v0.9.1` has no federation module at all — herdrdev/herdr
  doesn't carry this fork-only feature — so it isn't the comparison point.)
- Current == released (not already greater), so per the repo rule the
  question is only whether *this* change needs a bump. It does not: the cap
  is a purely local, unilaterally-enforced acceptance threshold, not part of
  the wire format or negotiated in the handshake. It changes no message
  shape, no enum variant, and no serialization. A peer running the old
  binary with the old 2 MiB cap will still reject the same oversize frames
  it always did; that's an orthogonal fleet-rollout concern, not a protocol
  incompatibility requiring version negotiation.

## Tests

Added `a_realistic_scrollback_replay_fits_the_terminal_channel_cap` in
`src/remote/federation/protocol/codec.rs` (new test, ~30 lines after
`channel_cap_enforced_for_clipboard_channel`). It:
1. Back-solves the raw scrollback byte count that produces a serde_json-encoded
   wire frame matching the observed real-world maximum (2,780,438 bytes,
   from the log line `federation frame size 2549229 exceeds its channel's
   cap 2097152` and its siblings up to 2,780,438).
2. Asserts the constructed frame lands within 100 KB of that target (sanity
   check on the back-solve, not the behavior under test).
3. Asserts `decode` accepts it against `Channel::Terminal.max_len()`.

**Verified the test fails against the old cap**: temporarily reverted
`Channel::Terminal`'s cap to `2 * 1024 * 1024` and reran just this test:

```
thread '...a_realistic_scrollback_replay_fits_the_terminal_channel_cap' panicked at codec.rs:371:
a realistic scrollback replay must fit the terminal channel cap: FrameTooLarge { claimed: 2701288, max: 2097152 }
test result: FAILED. 0 passed; 1 failed
```

Then restored the 8 MiB cap and reran — passes. Also checked
`file_staging_channel_cap_is_the_largest_channel_cap` (asserts every
channel's cap `<=` `Channel::largest_max_len()` and that `FileStaging`
(24 MiB) is the largest) — unaffected: `Terminal` now ties `Mount` at 8 MiB,
both still well under `FileStaging`'s 24 MiB, and the test uses `<=` so a tie
is fine. Ran and confirmed green alongside everything else.

## Verification commands run (after final edit)

```
cargo fmt --check                                          # pass (after one `cargo fmt` auto-fix to the new test)
cargo check --all-targets                                  # pass, no warnings
cargo clippy --all-targets --locked -- -D warnings          # pass, zero clippy errors/warnings
cargo test --bin herdr remote::federation::protocol::       # 30 passed, 0 failed
cargo test --bin herdr remote::federation::                 # 129 passed, 0 failed
cargo test --bin herdr server::federation                   # 64 passed, 0 failed
```

(No `just`/`cargo nextest` used for these — `just` isn't on `PATH` here;
`cargo-nextest` binary exists at `~/.cargo/bin/cargo-nextest` but plain
`cargo test --bin herdr` was used directly and is equivalent for this
targeted run. `CARGO_TARGET_DIR` was redirected outside the repo, `ZIG` set
per the environment note.)

## Files changed

- `src/remote/federation/protocol/mod.rs` — `Channel::Terminal::max_len()` 2 MiB -> 8 MiB, doc comment.
- `src/remote/federation/protocol/codec.rs` — new regression test.
- `docs/next/CHANGELOG.md` — `## Unreleased` / `### Fixed` entry.

## Proposed commit message (NOT committed)

```
fix(federation): raise the terminal channel's frame cap to 8 MiB

A live pane's full ANSI scrollback replay is sent unchunked in one Open
frame and has been observed producing 2.3-2.8 MB wire frames, well past
the old 2 MiB Channel::Terminal cap. Exceeding a channel's cap is fatal
to the whole federation connection, so opening such a pane over
federation was tearing down the entire mount. Match the cap to
Channel::Mount's 8 MiB for headroom above the observed maximum.
```

Status: DONE
Summary: Root cause is an unbounded scrollback replay hitting a too-small Terminal channel cap; fixed by raising the cap to 8 MiB (matching Mount) with a regression test that fails on the old cap and passes on the new one. Non-fatal degrade and a protocol version bump were both investigated and are not warranted/deferred, with evidence in this report.
Concerns/Blockers: The "make oversize non-fatal" secondary ask was deferred as a follow-up (not a couple-of-call-sites change); flagged as a recommended future improvement above.
