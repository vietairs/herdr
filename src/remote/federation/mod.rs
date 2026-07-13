//! Federation protocol + id-fencing primitives.
//!
//! Pure library: message types, a framed codec, version/capability
//! negotiation, and id-namespacing/generation-fencing primitives shared by
//! both ends of a federated herdr link. No I/O, no live wiring — later
//! phases (P3/P4/P5) consume this to build the actual federation link.

pub mod id;
pub mod protocol;
