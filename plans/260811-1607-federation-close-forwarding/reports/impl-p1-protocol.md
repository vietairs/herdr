# P1 — federation close-forwarding protocol layer

Status: DONE

Resolved per team-lead decision (Option 2): file allowlist expanded by one
file, `src/remote/federation/client.rs`, for minimal stub match arms only.

## New types (`src/remote/federation/protocol/mod.rs`)

```rust
pub struct WorkspaceCloseRequest {
    pub request_id: u64,
    pub target_workspace_id: String,
}

pub enum WorkspaceCloseResponse {
    Closed { request_id: u64 },
    Failed { request_id: u64, reason: String },
}

pub struct TabCloseRequest {
    pub request_id: u64,
    pub target_tab_id: String,
}

pub enum TabCloseResponse {
    Closed { request_id: u64 },
    Failed { request_id: u64, reason: String },
}
```

All four derive `#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]`,
placed immediately after `ClosePaneResponse`, doc-commented on the same "raw
un-namespaced id" pattern as `ClosePaneRequest::target_pane_id`.

- Added all four as `FederationMessage` variants, next to the `ClosePane*` pair.
- Mapped all four to `Channel::Control` in `FederationMessage::channel()`.
- Extended the `FEDERATION_PROTOCOL_VERSION` doc comment: these variants ride
  the existing 5 -> 6 bump. Verified v6 (the `WorkspaceCreateRequest`/`Response`
  bump) has not shipped in any release tag yet
  (`git show v0.8.0-hvn.2:src/remote/federation/protocol/mod.rs | grep -c
  WorkspaceCreateRequest` → `0`), so there is no already-deployed peer whose
  decode expectations these four new variants would break — riding the bump
  instead of forcing 6 -> 7 is correct.
- `git diff` confirms `FEDERATION_PROTOCOL_VERSION` (mod.rs) and `PROTOCOL_VERSION`
  (`src/protocol/wire.rs`, untouched entirely) are unchanged — only comment/test
  text references the value `6`, the constant's own line is unmodified.

## Tests added

In `src/remote/federation/protocol/mod.rs` `#[cfg(test)] mod tests`:
- `workspace_close_request_response_roundtrip_through_the_wire_codec`
- `tab_close_request_response_roundtrip_through_the_wire_codec`
- `federation_protocol_version_is_unchanged_for_the_close_forwarding_variants`
  — asserts `FEDERATION_PROTOCOL_VERSION == 6`

In `src/remote/federation/protocol/codec.rs`:
- Extended `every_message_variant()` with all 4 new variants (6 new list
  entries: request + `Closed` + `Failed` for each of Workspace/Tab), so
  `every_message_variant_round_trips_through_encode_decode` covers them.
- Extended the tests-module `use super::super::{...}` import list to bring the
  four new types into scope.

## client.rs stub arms (scope expansion, approved)

`src/remote/federation/client.rs`'s federation-client receive loop
(`match msg { FederationMessage::... }`, line ~591) is exhaustive with no
wildcard arm, so the 4 new `FederationMessage` variants made it fail to
compile (E0004) until handled. Added exactly 4 minimal arms, nothing else:

- `WorkspaceCloseRequest(_)` / `TabCloseRequest(_)`: client->server only
  (this loop drives the client side of a mount, so the peer should never send
  either) — `tracing::debug!(...)` and ignore, wording/shape copied from the
  existing `SplitPaneRequest`/`ClosePaneRequest`/`WorkspaceCreateRequest`
  stubs.
- `WorkspaceCloseResponse(_)` / `TabCloseResponse(_)`: also `tracing::debug!(...)`
  and ignore, with a comment marking them explicitly provisional — real
  handling (origin validation + confirmed teardown) is documented as landing
  with the client close-dispatch work, since it depends on pending-map
  plumbing that doesn't exist yet. No `todo!()`, `unimplemented!()`, `panic!()`,
  or `unwrap()` anywhere — every arm is a safe no-op, so a stray frame of
  these types can never kill a live mount.

## Validation

```
$ ZIG=~/.local/zig-0.15.2/zig cargo test --bin herdr remote::federation::protocol -- --test-threads=4
running 27 tests
... all 27 passed, including:
  workspace_close_request_response_roundtrip_through_the_wire_codec ... ok
  tab_close_request_response_roundtrip_through_the_wire_codec ... ok
  federation_protocol_version_is_unchanged_for_the_close_forwarding_variants ... ok
  every_message_variant_round_trips_through_encode_decode ... ok
test result: ok. 27 passed; 0 failed; 0 ignored; 0 measured; 3396 filtered out

$ ZIG=~/.local/zig-0.15.2/zig cargo build
   Compiling herdr v0.8.0 (...)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 15.66s
```

Both clean. No `unwrap()` in production code. No plan/phase labels ("P1",
"P2", plan ids) in any code comment.

## Diff scope

```
$ git diff --stat
 src/remote/federation/client.rs         |  21 +++++
 src/remote/federation/protocol/codec.rs |  23 +++++-
 src/remote/federation/protocol/mod.rs   | 142 ++++++++++++++++++++++++++++++++
 3 files changed, 185 insertions(+), 1 deletion(-)
```

Exactly the three approved files. Not committed, per instructions.

## History note (superseded)

An earlier pass attempted the original two-file allowlist and hit a real
blocker: adding enum variants can't be isolated from client.rs's exhaustive
match. That was resolved by the team-lead approving a one-file scope
expansion (see above); this report reflects the final, unblocked state.

## Unresolved questions

None outstanding for P1. P2 (host side) and P3 (client dispatch, which fills
in the two provisional response arms in client.rs) are separate phases per
the plan.
