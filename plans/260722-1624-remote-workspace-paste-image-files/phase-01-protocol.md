# Phase 01 — protocol types, channel cap, capability

## Context (verified against source)

- `src/remote/federation/protocol/mod.rs:37` — `FEDERATION_PROTOCOL_VERSION = 3`, with the in-source
  policy comment (`:30-36`) permitting additive variants without a bump while no production call
  site has shipped in a tagged version.
- `src/remote/federation/protocol/mod.rs:48` — `Capability::CLIPBOARD` exists and is referenced
  nowhere. Do not reuse it.
- `src/remote/federation/protocol/mod.rs:314-324` — `enum Channel`; `:332-342` — `max_len()`
  (`Clipboard` 16 MiB at `:339`, `Control` 4 KiB at `:340`).
- `src/remote/federation/protocol/mod.rs:347-364` — `enum FederationMessage`; `:368-384` —
  `channel()`.
- `src/remote/federation/protocol/mod.rs:449` — `snapshot_request_response_roundtrip_through_the_wire_codec`,
  the roundtrip test template (`:407` is the split-pane sibling).
- `src/remote/federation/protocol/codec.rs:54-58` — the codec is **`serde_json`**, not bincode
  (explicit comment). This is load-bearing (see Requirements 2).
- `src/remote/federation/protocol/negotiate.rs:20` — `negotiate()`; `:82` already proves an unknown
  capability is dropped, not fatal.
- `Cargo.toml:22` — `base64 = "0.22.1"` already a direct dependency; used e.g. in
  `src/selection.rs:267`.

### Version verdict — re-verified, holds

- `src/protocol/wire.rs:16` — `PROTOCOL_VERSION = 17`; `git show v0.7.5:src/protocol/wire.rs` also
  16:`= 17`. This design never touches `wire.rs`. **No bump.**
- `git ls-tree v0.7.5 src/remote/federation/` is **empty** — federation does not exist in the latest
  released tag, so no deployed peer can observe an additive variant. **No
  `FEDERATION_PROTOCOL_VERSION` bump.**
- Consequence: skew safety rests entirely on capability gating (phase 03).

## Requirements

1. `ClipboardStageRequest` / `ClipboardStageResponse` variants on `FederationMessage`, correlated by
   a bare `request_id: u64`, mirroring `SplitPaneRequest`/`SplitPaneResponse`'s shape.
2. **The payload is base64, not `Vec<u8>`.** New finding, not in the predict report: the codec is
   `serde_json` (`codec.rs:54-58`), so a `Vec<u8>` serialises as a JSON array of decimal numbers —
   roughly 4x inflation, which would put a 16 MiB image at ~64 MiB on the wire and blow any sane
   cap. base64 is 1.34x. (The existing `ClipboardMessage.payload` / `TerminalChannelMessage::Output.bytes`
   fields have this defect; not in scope to fix — recorded in plan.md Deferred.)
3. A **new** `Channel::FileStaging` arm with its own cap. `Control` (4 KiB) is far too small;
   `Clipboard` (16 MiB) is too small once base64'd and is a different flow. Cap = 24 MiB
   (16 MiB raw → ~21.4 MiB base64 → headroom for the filename and JSON framing).
4. `Channel::largest_max_len()` — a `const fn` returning the max across all arms.
   `serve::global_max_frame` (`serve.rs:150-153`) and
   `federation_accept::global_max_frame` (`federation_accept.rs:100-103`) both hardcode
   `Channel::Clipboard.max_len()` as "the largest cap"; that assumption breaks with a bigger arm and
   would reject every stage frame before decode. Phase 01 provides the helper; **phase 03 owns the
   two call-site swaps** (those files are phase-03-owned).
5. `Capability::FILE_STAGING = "file_staging"` — named for the operation, per predict section D.
   Do not touch `Capability::CLIPBOARD`.
6. A scoped failure enum `ClipboardStageFailure` (not a bare `StageError`), serialisable on the
   response. It must carry a `Busy` variant distinct from `WriteFailed`: phase 03's staging worker
   queue is bounded, and transient backpressure must not be reported to the user as a disk failure.
7. **No separate `extension` field** (two sources of truth, the second one attacker-controlled).
   The extension is re-derived from the sanitised filename in phase 02.
8. **`original_filename` has no OS source — it is synthesised locally.** Verified: the only capture
   type is `crate::platform::ClipboardImage { bytes: Vec<u8>, extension: &'static str }`
   (`src/platform/mod.rs:88-94`); the extension is drawn from a fixed allowlist and magic-byte
   validated (`src/platform/linux.rs:410-416`, `src/platform/macos.rs:626-629`), and no filename
   exists anywhere in the clipboard path. Phase 05 therefore synthesises
   `format!("image.{extension}")` from the validated extension. The phase-02 filename contract is
   consequently **defence against a hostile or buggy mounting client**, not filename preservation;
   the field is still fully untrusted on the receiving side. The remote does no magic-byte check of
   its own, so "images only" is enforced by extension allowlist on the write side, not by content.

## Files

| Action | File | Owner |
|---|---|---|
| modify | `src/remote/federation/protocol/mod.rs` | phase 01 (exclusive) |

No other file is touched in this phase.

## Implementation steps

1. Add `Capability::FILE_STAGING` next to the existing consts (`mod.rs:49-51`), with a doc comment
   saying it gates the stage-then-inject RPC and is explicitly not the unused `CLIPBOARD` const.
2. Add the request/response types near `SplitPaneRequest` (`mod.rs:255-296`):
   - `ClipboardStageRequest { request_id: u64, payload_base64: String, original_filename: String }`.
     Doc-comment that the payload is base64 because the codec is `serde_json`, where a `Vec<u8>`
     serialises as a JSON array of decimal numbers (~4x inflation), and that `original_filename` is
     wholly untrusted peer input — the sanitisation contract lives in `file_staging.rs` (phase 02).
   - `ClipboardStageResponse::Staged { request_id, path: String }` /
     `::Failed { request_id, failure: ClipboardStageFailure }`.
   - `ClipboardStageFailure { InvalidFilename, UnsupportedExtension, InvalidPayload,
     PayloadTooLarge, QuotaExceeded, StagingUnavailable, Busy, WriteFailed }` —
     `Serialize`/`Deserialize`, `Copy`, no free-form string (a free-form remote string would be one
     more thing to sanitise before display; the toast copy is chosen locally in phase 05).
     - `InvalidPayload` is the typed outcome for a `payload_base64` that is frame-legal but not
       decodable base64. It must be produced **before any filesystem access**, so a malformed
       payload can never be misreported as a disk failure and never leaves a partial file.
     - `StagingUnavailable` is the typed outcome for a staging **root** that this contract cannot
       use: not absolute, not losslessly UTF-8, or containing a byte outside the shared path
       allowlist. It is decided before any write, so the remote never creates a file whose path the
       client would then have to reject (phase 02 requirement 4b, phase 04 requirement 8).
3. Add `Channel::FileStaging` to the enum (`mod.rs:314-324`) and to `max_len()` (`:332-342`) at
   `24 * 1024 * 1024`, with a comment deriving the number from the 16 MiB image cap through base64.
   The enum's doc comment at `mod.rs:311` still reads "The six federation channel classes" and is
   already stale at seven arms — fix the wording (drop the count) in the same edit.
4. Add `Channel::largest_max_len()` as a `const fn` that takes the max over every arm explicitly.
   **`Ord::max` / `core::cmp::max` are not `const` on stable**, and the sibling `max_len` is
   `pub const fn` (`mod.rs:331`), so write it as explicit `if a > b { a } else { b }` nesting or a
   plain `match` over the arms. It must not silently go stale — test 5 is the guard.
5. Add both variants to `FederationMessage` (`:347-364`) and to `channel()` (`:368-384`), both
   mapping to `Channel::FileStaging`.

## Tests — TESTS FIRST

Write all four before step 1-5 code; each must fail to compile/assert first. Template:
`snapshot_request_response_roundtrip_through_the_wire_codec` (`mod.rs:449`).

Starred (predict section F):

1. ★`clipboard_stage_request_response_roundtrip_through_the_wire_codec` — encode/decode both
   variants through `codec::encode`/`codec::decode` at `Channel::FileStaging.max_len()`; assert
   `channel()` is `FileStaging` for both and the decoded value equals the original.
2. ★`clipboard_stage_request_response_respect_their_channel_caps` — a request whose encoded frame
   exceeds `Channel::FileStaging.max_len()` decodes as `CodecError::FrameTooLarge`; one just under
   it succeeds.
3. ★`clipboard_stage_capability_absent_on_one_side_is_dropped_not_fatal` — `negotiate()` with
   `FILE_STAGING` advertised on only one side returns `Ok` with the capability absent from the
   agreed set (extends `negotiate.rs:82`'s existing proof to this specific capability).
4. ★`clipboard_stage_capability_present_both_sides_is_agreed` — both sides advertise it, agreed set
   contains it.

Additional (this phase's own finding, requirement 4):

5. `file_staging_channel_cap_is_the_largest_channel_cap` — asserts
   `Channel::largest_max_len() == Channel::FileStaging.max_len()` and that it is `>=` every other
   arm. This is the guard that keeps the two `global_max_frame` call sites honest when a future arm
   grows.
6. `a_base64_encoded_max_size_image_fits_the_file_staging_cap` — build a `ClipboardStageRequest`
   carrying a base64 string sized for a 16 MiB payload plus a 255-byte filename, encode it, assert
   the frame fits `Channel::FileStaging.max_len()`. This is the test that would have caught the
   `Vec<u8>` inflation.

## Risks and rollback

- **Risk:** the 24 MiB cap is picked from arithmetic, not measurement. Test 6 measures it; if it
  fails, raise the cap rather than lowering the image limit (A1 fixes the image limit at 16 MiB).
- **Risk:** a reviewer reads `Capability::CLIPBOARD` as the right gate. Mitigated by the doc comment
  on `FILE_STAGING`.
- **Rollback:** this phase is purely additive to one file with no call sites. Reverting the commit
  removes it cleanly; nothing else compiles against it until phase 03.
