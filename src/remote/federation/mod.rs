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
