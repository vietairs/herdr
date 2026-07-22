//! Federation protocol + id-fencing primitives.
//!
//! Pure library: message types, a framed codec, version/capability
//! negotiation, and id-namespacing/generation-fencing primitives shared by
//! both ends of a federated herdr link. No I/O, no live wiring — later
//! phases (P3/P4/P5) consume this to build the actual federation link.
//!
//! # Trust model (P7 — read before adding a new ingestion field)
//! Federation is **TRUSTED-REMOTE, not sandboxed**: we install and control
//! both binaries, and the only boundary between them is SSH (`0o600` local
//! socket, no app-layer auth — verified against `src/api/server.rs`). This
//! is deliberately *not* an authentication/authorization model (out of
//! scope — SSH already is the boundary; adding one would be YAGNI), but it
//! is also not a claim that remote-sourced data is safe to render or apply
//! blindly: a legitimately-authed remote host can be independently
//! compromised after the SSH connection is established, and every byte
//! that host sends afterward is untrusted input to the local process, no
//! different in kind from any other network peer. Concretely:
//! - Every channel decodes exclusively through `protocol::codec`
//!   (`decode`/`encode`) with a per-channel `Channel::max_len` cap — there
//!   is no second, hand-rolled decode path anywhere in this module tree.
//! - Every remote-sourced *chrome* string (labels, cwd, agent name, titles —
//!   never raw PTY bytes, which are legitimate terminal content the ghostty
//!   emulator is the correct sandbox for) is neutralized of control/ANSI/
//!   OSC sequences at the single `reducer` ingest choke point
//!   (`sanitize::sanitize_remote_string`) before it can reach a rendered
//!   buffer.
//! - Every mirrored entity and clipboard write carries its origin
//!   (`id::HostKey` / `ClipboardMessage::origin_tag`) end-to-end, so a
//!   remote's data is never rendered as if it were locally-produced.
//! - A remote-origin clipboard write receives exactly the same trust as a
//!   local one — no elevation, no silent suppression (`pane_source::
//!   apply_remote_clipboard_writes`).
//! - Every per-mount ingestion buffer is bounded (`client::
//!   TERMINAL_OUTPUT_CHANNEL_CAPACITY`, `client::CLIPBOARD_CHANNEL_CAPACITY`)
//!   so a flooding or malicious remote cannot grow local memory without
//!   bound.

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

// P5: `RemoteTerminalSource` — the raw-byte-channel `TerminalSource`/
// `TerminalLifecyclePolicy` counterpart to `terminal::LocalChild`, fed by
// `client`'s per-mount `TerminalChannelRouter`. `pub(crate)` to match the
// visibility pattern of every other module in this file.
pub(crate) mod pane_source;

// P7: control/ANSI/OSC stripping for remote-sourced chrome strings, applied
// at the reducer's ingest choke point (S11.1). `pub(crate)` to match the
// visibility pattern of every other module in this file.
pub(crate) mod sanitize;

// P9.2b b2: the in-proc federated session runner (`run_federated_session`) plus
// the process-global local-spawn backstop it arms. Unix-only (mirrors the
// `#[cfg(unix)]` gate on `remote::unix`, which owns `dial_federation`/
// `LiveTunnel` and `App::new_federated`). Dormant until b3 flips
// `run_remote`'s federated arm onto it.
#[cfg(unix)]
pub(crate) mod session;
