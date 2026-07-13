//! Federation protocol + id-fencing primitives.
//!
//! Pure library: message types, a framed codec, version/capability
//! negotiation, and id-namespacing/generation-fencing primitives shared by
//! both ends of a federated herdr link. No I/O, no live wiring — later
//! phases (P3/P4/P5) consume this to build the actual federation link.

pub mod id;
pub mod protocol;

// `LoopbackFederationServer`/`FixtureHost` are test-only substrate (this
// phase's own tests plus P4-P9's, per the phase file) — gated so a release
// build never carries never-constructed fixture types as dead code.
#[cfg(test)]
pub(crate) mod loopback;
pub(crate) mod serve;
pub(crate) mod tee;

// P4: local federation client + per-mount replica reducer. `pub(crate)`
// (not `pub`) to match every type these modules expose (`FederationClient`,
// `RemoteMirror`, ...), which are all `pub(crate)` — matches the existing
// `serve`/`tee` visibility pattern in this file rather than the phase file's
// literal `pub mod` wording (logged in implementation-notes.md).
pub(crate) mod client;
pub(crate) mod reducer;
