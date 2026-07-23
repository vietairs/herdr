//! Federation protocol message types.
//!
//! Purpose-built wire types for the federation link between two herdr server
//! processes. Compiled into both ends. Independent of `crate::protocol::wire`
//! (client/UI protocol) — this has its own version counter and its own frame
//! caps, even though it reuses the framing *style* (see `codec.rs`).
//!
//! Pure data + pure functions only. No I/O, no call sites yet.

pub mod codec;
pub mod negotiate;

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::api::schema::common::AgentStatus;
use crate::api::schema::events::EventKind;
use crate::api::schema::session::SessionSnapshot;

use super::id::ServerInstanceId;

/// Current federation protocol version. Independent of
/// `crate::protocol::wire::PROTOCOL_VERSION` (the client/UI wire protocol).
/// Bumped 1 -> 2 with the addition of the control-channel `Fault` frame
/// (b0.3-tail): a peer on v1 cannot decode a `Fault`, so the version gates it.
/// Bumped 2 -> 3 with the addition of `SplitPaneRequest`/`SplitPaneResponse`
/// (remote-split protocol scaffolding): a peer on v2 cannot decode either
/// variant.
///
/// `SnapshotRequest`/`SnapshotResponse` (post-mount pane mirroring resync,
/// plans/260722-1327) were added WITHOUT a version bump: v3 has never
/// shipped in a release (no `federation_accept.rs`/`client.rs` production
/// call site existed in any tagged version — see
/// `plans/260713-1217-herdr-remote-workspace-federation/implementation-notes.md`),
/// so there is no deployed peer that could observe the addition as a skew.
pub const FEDERATION_PROTOCOL_VERSION: u32 = 3;

/// An optional feature two federation peers may support. Modeled as an
/// opaque name rather than a closed enum so an older peer can simply not
/// recognize a newer peer's capability string and drop it from the agreed
/// set instead of failing to decode the handshake at all (additive
/// evolution: unknown capabilities are ignored, not fatal).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Capability(pub String);

impl Capability {
    pub const CLIPBOARD: &'static str = "clipboard";
    pub const SCROLLBACK_REPLAY: &'static str = "scrollback_replay";
    pub const AGENT_STATUS: &'static str = "agent_status";
    /// Gates the stage-then-inject file RPC (`ClipboardStageRequest` /
    /// `ClipboardStageResponse`): the mounting client ships bytes over the
    /// tunnel, the serving host writes them into its own staging directory
    /// and answers with the remote path.
    ///
    /// Deliberately NOT `CLIPBOARD`. That const names a surface (an
    /// OSC-52-style clipboard mirror) and has no call site; this one names an
    /// operation, and both peers must advertise it before either side emits a
    /// stage frame. A peer that never advertises it must degrade to a local
    /// failure notice, never a torn-down mount.
    ///
    pub const FILE_STAGING: &'static str = "file_staging";

    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }
}

/// Opening message of the federation handshake.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Handshake {
    pub federation_protocol_version: u32,
    pub capabilities: BTreeSet<Capability>,
    pub server_instance_id: ServerInstanceId,
}

/// Reason a handshake was rejected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RejectReason {
    /// The peers' `federation_protocol_version`s do not match.
    Version { local: u32, remote: u32 },
}

/// Why a federation link is being torn down, carried on the control channel so
/// the peer learns the cause instead of only seeing an EOF. The wire mirror of
/// `server::federation_fault::TunnelExit` (converted at the edge); a versioned
/// closed enum so an unknown future reason is a decode error the version bump
/// guards against, not a silent misinterpretation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FaultReason {
    PeerClosed,
    WriterFailed,
    ChildExited,
    TaskPanicked,
    ServerTerminalClosed,
    Lagged,
    EgressOverflow,
    LocalQueueOverflow,
    GenerationMismatch,
    EventGap,
}

/// Control-channel fault frame: a best-effort "I am closing because <reason>"
/// sent before (or instead of) a bare EOF.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FaultMessage {
    pub reason: FaultReason,
}

/// Response to a `Handshake`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HandshakeResponse {
    Accept {
        agreed_capabilities: BTreeSet<Capability>,
    },
    Reject {
        reason: RejectReason,
    },
}

/// Resumable cursor into a peer's event stream, independent of the local
/// `EventHub` ring (federation owns its own cursor; see phase context).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct EventCursor(pub u64);

/// The atomic mount: a consistent snapshot plus the cursor it is consistent
/// with. The first applied `EventFrame` after mounting must have
/// `source_seq == cursor.0 + 1`; anything else is a gap.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MountSnapshot {
    pub server_instance_id: ServerInstanceId,
    pub snapshot: SessionSnapshot,
    pub cursor: EventCursor,
}

/// A single event, positioned by the source's own monotonic sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventFrame {
    pub source_seq: u64,
    pub kind: EventKind,
}

/// Event-channel message: either a positioned event, or a control marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventChannelMessage {
    Frame(EventFrame),
    /// The source detected (or the reducer detected, from a sequence hole)
    /// that events `from+1..=to` were not delivered.
    Gap {
        from: u64,
        to: u64,
    },
    /// The source is discarding cursor continuity; the receiver must
    /// re-mount (request a fresh `MountSnapshot`) rather than keep applying.
    Reset,
}

/// Scrollback replay payload sent when a `TerminalChannelMessage::Open`
/// primes a freshly-mirrored terminal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScrollbackReplay {
    pub bytes: Vec<u8>,
}

/// Terminal-channel messages. Every variant is tagged with
/// `{terminal_id, mount_generation}` so `id::fence` can reject any message
/// from a stale mount before it is routed — the tag is a required field on
/// every variant, not an optional wrapper, so the type system enforces it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TerminalChannelMessage {
    Open {
        terminal_id: String,
        mount_generation: u64,
        replay: ScrollbackReplay,
    },
    Output {
        terminal_id: String,
        mount_generation: u64,
        bytes: Vec<u8>,
    },
    Input {
        terminal_id: String,
        mount_generation: u64,
        bytes: Vec<u8>,
    },
    Resize {
        terminal_id: String,
        mount_generation: u64,
        cols: u16,
        rows: u16,
    },
    Close {
        terminal_id: String,
        mount_generation: u64,
    },
}

impl TerminalChannelMessage {
    /// The `mount_generation` tag carried by every variant, for fencing.
    pub fn mount_generation(&self) -> u64 {
        match self {
            Self::Open {
                mount_generation, ..
            }
            | Self::Output {
                mount_generation, ..
            }
            | Self::Input {
                mount_generation, ..
            }
            | Self::Resize {
                mount_generation, ..
            }
            | Self::Close {
                mount_generation, ..
            } => *mount_generation,
        }
    }

    /// The `terminal_id` tag carried by every variant. Symmetric with
    /// `mount_generation`; the co-located accept loop's wire router (b0.3-tail /
    /// b0.4 sub-brick 2) reads it to route an inbound terminal message to the
    /// matching `FederationCommand`. Dormant until then.
    #[allow(dead_code)]
    pub fn terminal_id(&self) -> &str {
        match self {
            Self::Open { terminal_id, .. }
            | Self::Output { terminal_id, .. }
            | Self::Input { terminal_id, .. }
            | Self::Resize { terminal_id, .. }
            | Self::Close { terminal_id, .. } => terminal_id,
        }
    }
}

/// Agent-status-channel message, tagged like `TerminalChannelMessage`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentStatusMessage {
    pub terminal_id: String,
    pub mount_generation: u64,
    pub status: AgentStatus,
    /// Canonical agent label (e.g. `"claude"`, `"codex"`) identifying which
    /// agent produced `status`, sourced from the serving host's own
    /// `AgentInfo.agent`. `None` when the serving host has not identified an
    /// agent for this terminal yet. Additive field within
    /// `FEDERATION_PROTOCOL_VERSION` 3: the handshake already rejects any
    /// version skew before either peer decodes a channel frame, so peers
    /// that can reach this type always agree on this field's presence — the
    /// `default`/`skip_serializing_if` below only keep old fixture frames
    /// (recorded before this field existed) decodable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
}

/// Clipboard-channel message. `origin_tag` identifies which side produced
/// the payload so a receiver can distinguish its own echoed clipboard from
/// a genuinely remote one (consumed by later phases).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardMessage {
    pub origin_tag: String,
    pub payload: Vec<u8>,
}

/// Request to split an existing remote pane on the serving host's own
/// workspace, sent by a mounting client so a local "split right"/"split
/// down" action creates the new pane on the mounted host instead of
/// falling back to a local spawn. `request_id` correlates the eventual
/// `SplitPaneResponse` (the control channel carries no other request/
/// response pairing today, so this is deliberately simple: a bare u64
/// minted per outstanding request, not a full RPC framework).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SplitPaneRequest {
    pub request_id: u64,
    /// Raw (un-namespaced) remote pane id to split, as carried by the
    /// mount's own `SessionSnapshot`/event stream — never a locally
    /// namespaced `r:<host>:...` id.
    pub target_pane_id: String,
    pub direction: SplitDirection,
    pub ratio: Option<f32>,
    pub focus: bool,
}

/// A split direction, mirroring `crate::api::schema::common::SplitDirection`
/// but defined locally so the federation wire protocol never depends on the
/// client/UI JSON API's schema types (independent evolution, matching this
/// module's existing "purpose-built wire types" doc comment).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SplitDirection {
    Right,
    Down,
}

/// Response to a `SplitPaneRequest`: either the new pane's raw remote
/// terminal id (the client re-namespaces it under its own mount, the same
/// way `App::build_remote_pane` namespaces mount-time panes), or a reason
/// the split could not be performed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SplitPaneResponse {
    Created {
        request_id: u64,
        new_pane_id: String,
        new_terminal_id: String,
    },
    Failed {
        request_id: u64,
        reason: String,
    },
}

/// Request from a mounting client to write a file into the serving host's own
/// staging directory, so a paste performed against a mirrored remote pane
/// produces a path that resolves on the *remote* host rather than locally.
/// `request_id` correlates the eventual `ClipboardStageResponse`, mirroring
/// `SplitPaneRequest`'s bare-u64 pairing rather than introducing an RPC
/// framework.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardStageRequest {
    pub request_id: u64,
    /// The file's bytes, base64-encoded.
    ///
    /// Base64 rather than `Vec<u8>` because this protocol's codec is
    /// `serde_json` (see `codec::encode`), where a byte vector serialises as
    /// a JSON array of decimal numbers — roughly 4x inflation, which would put
    /// a max-size image far past any sane channel cap. Base64 costs 1.34x.
    pub payload_base64: String,
    /// The sender's proposed file name. **Wholly untrusted peer input**: it is
    /// neither a path nor a validated name at this layer, and the receiving
    /// host must run it through the staging module's sanitisation contract
    /// before it reaches the filesystem. The extension is re-derived from the
    /// sanitised name; there is deliberately no separate extension field,
    /// because a second, independently attacker-controlled source of the same
    /// fact can disagree with the first.
    pub original_filename: String,
}

/// Why a `ClipboardStageRequest` could not be staged.
///
/// A closed enum with no free-form string: a remote-supplied message would be
/// one more untrusted value to sanitise before display, so the user-facing
/// copy is chosen locally from the variant instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClipboardStageFailure {
    /// The proposed file name failed the receiving host's sanitisation
    /// contract (traversal, absolute path, control bytes, over-length, ...).
    InvalidFilename,
    /// The sanitised name's extension is not on the receiving host's
    /// allowlist.
    UnsupportedExtension,
    /// `payload_base64` was frame-legal but is not decodable base64. Decided
    /// before any filesystem access, so a malformed payload can never be
    /// misreported as a disk failure and can never leave a partial file.
    InvalidPayload,
    /// The decoded payload exceeds the receiving host's per-file limit.
    PayloadTooLarge,
    /// Writing this file would push the staging directory past its total-bytes
    /// quota.
    QuotaExceeded,
    /// The receiving host has no staging root this contract can use: not
    /// absolute, not losslessly UTF-8, or containing a byte outside the shared
    /// path allowlist. Decided before any write, so the host never creates a
    /// file whose path the client would then have to reject.
    StagingUnavailable,
    /// The receiving host is already staging its maximum number of concurrent
    /// requests on this connection. Transient backpressure, distinct from
    /// `WriteFailed` so the user is not told a retryable queue limit was a
    /// disk failure.
    Busy,
    /// The write itself failed (permissions, disk full, ...). The receiving
    /// host removes any partially written file before answering.
    WriteFailed,
}

/// Answer to a `ClipboardStageRequest`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClipboardStageResponse {
    Staged {
        request_id: u64,
        /// Absolute path of the staged file **on the serving host**. Still
        /// untrusted from the client's perspective: it is about to be injected
        /// into a PTY, so the client re-validates it before use.
        path: String,
    },
    Failed {
        request_id: u64,
        failure: ClipboardStageFailure,
    },
}

/// Request from the mounting client for a fresh full `MountSnapshot`,
/// carried in-band on the same tunnel rather than requiring a whole new
/// connection. Sent when the client observes a structural `EventFrame`
/// (`PaneCreated`/`PaneClosed`/`TabCreated`/`TabClosed`) it cannot itself
/// turn into a mirror mutation (`reducer.rs`'s module docs: a bare
/// `EventFrame` carries no entity payload) — the response's `MountSnapshot`
/// is diffed against the mirror exactly like a `Gap`/`Reset` remount
/// (`RemoteMirror::reconcile_by_diff`). No fields: the single-controller
/// lease means the server always answers with its own current, unambiguous
/// state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotRequest;

/// The federation channel classes, used to select a per-channel frame cap in
/// the codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    Handshake,
    Mount,
    Event,
    Terminal,
    AgentStatus,
    Clipboard,
    /// Small out-of-band control frames (fault/teardown, split requests).
    /// Kept tiny so a fault can never be mistaken for a bulk channel and its
    /// cap is trivially met.
    Control,
    /// Stage-then-inject file transfer. Its own class rather than a reuse of
    /// `Clipboard`: a different flow with a different cap, and `Control` is
    /// several orders of magnitude too small.
    FileStaging,
}

impl Channel {
    /// Every channel class. A new enum arm must be added here too, or
    /// `largest_max_len` will not see its cap; the channel-cap test names this
    /// list as the place to update.
    pub const ALL: [Channel; 8] = [
        Channel::Handshake,
        Channel::Mount,
        Channel::Event,
        Channel::Terminal,
        Channel::AgentStatus,
        Channel::Clipboard,
        Channel::Control,
        Channel::FileStaging,
    ];

    /// Maximum payload length (bytes, post-header) accepted on this channel.
    /// Mirrors the spirit of `crate::protocol::wire::MAX_FRAME_SIZE` /
    /// `MAX_GRAPHICS_FRAME_SIZE` / `MAX_CLIPBOARD_IMAGE_PAYLOAD`, but these
    /// are federation's own caps, independent of the client/UI wire caps.
    pub const fn max_len(self) -> usize {
        match self {
            Channel::Handshake => 64 * 1024,
            Channel::Mount => 8 * 1024 * 1024,
            Channel::Event => 256 * 1024,
            Channel::Terminal => 2 * 1024 * 1024,
            Channel::AgentStatus => 64 * 1024,
            Channel::Clipboard => 16 * 1024 * 1024,
            Channel::Control => 4 * 1024,
            // Derived from the 16 MiB per-image limit: base64 costs 1.34x
            // (~21.4 MiB), and the rest is headroom for the file name and the
            // JSON envelope around it.
            Channel::FileStaging => 24 * 1024 * 1024,
        }
    }

    /// The largest `max_len()` across every channel.
    ///
    /// The tunnel readers must bound a frame before its true channel is known,
    /// so they need one ceiling that no channel can exceed. Hardcoding a
    /// particular channel's cap there silently rejects every frame on any
    /// channel that later grows past it.
    ///
    /// Written as an explicit `if` ladder because `Ord::max` /
    /// `core::cmp::max` are not `const` on stable and the sibling `max_len` is
    /// a `const fn`.
    pub const fn largest_max_len() -> usize {
        let mut largest = 0usize;
        let mut i = 0;
        while i < Self::ALL.len() {
            let candidate = Self::ALL[i].max_len();
            if candidate > largest {
                largest = candidate;
            }
            i += 1;
        }
        largest
    }
}

/// Top-level envelope for anything sent over the federation link.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum FederationMessage {
    Handshake(Handshake),
    HandshakeResponse(HandshakeResponse),
    MountSnapshot(MountSnapshot),
    Event(EventChannelMessage),
    Terminal(TerminalChannelMessage),
    AgentStatus(AgentStatusMessage),
    Clipboard(ClipboardMessage),
    Fault(FaultMessage),
    SplitPaneRequest(SplitPaneRequest),
    SplitPaneResponse(SplitPaneResponse),
    SnapshotRequest(SnapshotRequest),
    /// Answer to a `SnapshotRequest`: a fresh atomic (snapshot, cursor) pair,
    /// same shape as the mount handshake's own `MountSnapshot` — the
    /// receiver diffs it into the mirror via `RemoteMirror::reconcile_by_diff`
    /// rather than replacing it wholesale (S6.2).
    SnapshotResponse(MountSnapshot),
    ClipboardStageRequest(ClipboardStageRequest),
    ClipboardStageResponse(ClipboardStageResponse),
}

impl FederationMessage {
    /// The channel this message belongs to, used to select the codec cap.
    pub fn channel(&self) -> Channel {
        match self {
            Self::Handshake(_) | Self::HandshakeResponse(_) => Channel::Handshake,
            Self::MountSnapshot(_) => Channel::Mount,
            Self::Event(_) => Channel::Event,
            Self::Terminal(_) => Channel::Terminal,
            Self::AgentStatus(_) => Channel::AgentStatus,
            Self::Clipboard(_) => Channel::Clipboard,
            Self::Fault(_) => Channel::Control,
            Self::SplitPaneRequest(_) | Self::SplitPaneResponse(_) => Channel::Control,
            Self::SnapshotRequest(_) => Channel::Control,
            // Carries a full `SessionSnapshot`, same payload shape/size as
            // the mount handshake's `MountSnapshot` — reuse its channel cap.
            Self::SnapshotResponse(_) => Channel::Mount,
            // Both directions share the channel: the request carries the bulk
            // payload, and pairing the response with it keeps the correlated
            // exchange under one cap.
            Self::ClipboardStageRequest(_) | Self::ClipboardStageResponse(_) => {
                Channel::FileStaging
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::federation::id::{fence, HostKey, Mount, ServerInstanceId};

    fn sample_snapshot() -> SessionSnapshot {
        SessionSnapshot {
            version: "0.0.0-test".to_string(),
            protocol: FEDERATION_PROTOCOL_VERSION,
            focused_workspace_id: None,
            focused_tab_id: None,
            focused_pane_id: None,
            workspaces: Vec::new(),
            tabs: Vec::new(),
            panes: Vec::new(),
            layouts: Vec::new(),
            agents: Vec::new(),
        }
    }

    #[test]
    fn split_pane_request_response_roundtrip_through_the_wire_codec() {
        let request = FederationMessage::SplitPaneRequest(SplitPaneRequest {
            request_id: 7,
            target_pane_id: "term_1".to_string(),
            direction: SplitDirection::Right,
            ratio: Some(0.5),
            focus: true,
        });
        assert_eq!(request.channel(), Channel::Control);
        let encoded = codec::encode(&request).expect("encode must succeed");
        let (decoded, _consumed) =
            codec::decode::<FederationMessage>(&encoded, Channel::Control.max_len())
                .expect("decode must succeed");
        assert_eq!(decoded, request);

        let response = FederationMessage::SplitPaneResponse(SplitPaneResponse::Created {
            request_id: 7,
            new_pane_id: "term_2".to_string(),
            new_terminal_id: "term_2".to_string(),
        });
        assert_eq!(response.channel(), Channel::Control);
        let encoded = codec::encode(&response).expect("encode must succeed");
        let (decoded, _consumed) =
            codec::decode::<FederationMessage>(&encoded, Channel::Control.max_len())
                .expect("decode must succeed");
        assert_eq!(decoded, response);

        let failed = FederationMessage::SplitPaneResponse(SplitPaneResponse::Failed {
            request_id: 7,
            reason: "no such pane".to_string(),
        });
        let encoded = codec::encode(&failed).expect("encode must succeed");
        let (decoded, _consumed) =
            codec::decode::<FederationMessage>(&encoded, Channel::Control.max_len())
                .expect("decode must succeed");
        assert_eq!(decoded, failed);
    }

    // Post-mount pane mirroring fix (plans/260722-1327): `SnapshotRequest`/
    // `SnapshotResponse` round-trip through the wire codec like every other
    // `FederationMessage` variant, on their assigned channels.
    #[test]
    fn snapshot_request_response_roundtrip_through_the_wire_codec() {
        let request = FederationMessage::SnapshotRequest(SnapshotRequest);
        assert_eq!(request.channel(), Channel::Control);
        let encoded = codec::encode(&request).expect("encode must succeed");
        let (decoded, _consumed) =
            codec::decode::<FederationMessage>(&encoded, Channel::Control.max_len())
                .expect("decode must succeed");
        assert_eq!(decoded, request);

        let response = FederationMessage::SnapshotResponse(MountSnapshot {
            server_instance_id: ServerInstanceId("inst-a".to_string()),
            snapshot: sample_snapshot(),
            cursor: EventCursor(9),
        });
        assert_eq!(response.channel(), Channel::Mount);
        let encoded = codec::encode(&response).expect("encode must succeed");
        let (decoded, _consumed) =
            codec::decode::<FederationMessage>(&encoded, Channel::Mount.max_len())
                .expect("decode must succeed");
        assert_eq!(decoded, response);
    }

    fn stage_request(payload_base64: String) -> FederationMessage {
        FederationMessage::ClipboardStageRequest(ClipboardStageRequest {
            request_id: 11,
            payload_base64,
            original_filename: "image.png".to_string(),
        })
    }

    // The stage-then-inject RPC round-trips on its own channel, exactly like
    // every other `FederationMessage` variant.
    #[test]
    fn clipboard_stage_request_response_roundtrip_through_the_wire_codec() {
        let request = stage_request("aGVsbG8=".to_string());
        assert_eq!(request.channel(), Channel::FileStaging);
        let encoded = codec::encode(&request).expect("encode must succeed");
        let (decoded, _consumed) =
            codec::decode::<FederationMessage>(&encoded, Channel::FileStaging.max_len())
                .expect("decode must succeed");
        assert_eq!(decoded, request);

        let staged = FederationMessage::ClipboardStageResponse(ClipboardStageResponse::Staged {
            request_id: 11,
            path: "/tmp/herdr/federation-clipboard-abc/image.png".to_string(),
        });
        assert_eq!(staged.channel(), Channel::FileStaging);
        let encoded = codec::encode(&staged).expect("encode must succeed");
        let (decoded, _consumed) =
            codec::decode::<FederationMessage>(&encoded, Channel::FileStaging.max_len())
                .expect("decode must succeed");
        assert_eq!(decoded, staged);

        // Every failure variant must survive the codec: the client picks its
        // toast copy from the variant, so an undecodable one is a silent
        // failure at the UI.
        for failure in [
            ClipboardStageFailure::InvalidFilename,
            ClipboardStageFailure::UnsupportedExtension,
            ClipboardStageFailure::InvalidPayload,
            ClipboardStageFailure::PayloadTooLarge,
            ClipboardStageFailure::QuotaExceeded,
            ClipboardStageFailure::StagingUnavailable,
            ClipboardStageFailure::Busy,
            ClipboardStageFailure::WriteFailed,
        ] {
            let failed =
                FederationMessage::ClipboardStageResponse(ClipboardStageResponse::Failed {
                    request_id: 11,
                    failure,
                });
            assert_eq!(failed.channel(), Channel::FileStaging);
            let encoded = codec::encode(&failed).expect("encode must succeed");
            let (decoded, _consumed) =
                codec::decode::<FederationMessage>(&encoded, Channel::FileStaging.max_len())
                    .expect("decode must succeed");
            assert_eq!(decoded, failed);
        }
    }

    // Transient backpressure must not be reportable as a disk failure: the
    // two outcomes are distinct values, not aliases.
    #[test]
    fn clipboard_stage_busy_is_distinct_from_write_failed() {
        assert_ne!(
            ClipboardStageFailure::Busy,
            ClipboardStageFailure::WriteFailed
        );
    }

    #[test]
    fn clipboard_stage_request_response_respect_their_channel_caps() {
        let cap = Channel::FileStaging.max_len();

        let just_under = stage_request("A".repeat(cap - 1024));
        let encoded = codec::encode(&just_under).expect("encode must succeed");
        assert!(
            encoded.len() - 8 <= cap,
            "a payload sized just under the cap must produce a frame within it"
        );
        let (decoded, _consumed) = codec::decode::<FederationMessage>(&encoded, cap)
            .expect("a frame within the cap must decode");
        assert_eq!(decoded, just_under);

        let oversized = stage_request("A".repeat(cap + 1));
        let encoded = codec::encode(&oversized).expect("encode must succeed");
        let err = codec::decode::<FederationMessage>(&encoded, cap)
            .expect_err("a frame past the cap must be rejected from the header alone");
        assert!(
            matches!(err, codec::CodecError::FrameTooLarge { .. }),
            "expected FrameTooLarge, got {err:?}"
        );
    }

    // The `file_staging` capability follows the same additive-evolution rule
    // as every other capability: one-sided advertisement is dropped from the
    // agreed set, never fatal to the handshake.
    #[test]
    fn clipboard_stage_capability_absent_on_one_side_is_dropped_not_fatal() {
        let with = Handshake {
            federation_protocol_version: FEDERATION_PROTOCOL_VERSION,
            capabilities: [
                Capability::new(Capability::AGENT_STATUS),
                Capability::new(Capability::FILE_STAGING),
            ]
            .into(),
            server_instance_id: ServerInstanceId("inst-a".to_string()),
        };
        let without = Handshake {
            federation_protocol_version: FEDERATION_PROTOCOL_VERSION,
            capabilities: [Capability::new(Capability::AGENT_STATUS)].into(),
            server_instance_id: ServerInstanceId("inst-b".to_string()),
        };

        let agreed = negotiate::negotiate(&with, &without)
            .expect("a one-sided capability must not reject the handshake");
        assert!(!agreed
            .0
            .contains(&Capability::new(Capability::FILE_STAGING)));
        assert!(agreed
            .0
            .contains(&Capability::new(Capability::AGENT_STATUS)));

        // Symmetric: the same holds when the older peer is the local side.
        let agreed = negotiate::negotiate(&without, &with)
            .expect("a one-sided capability must not reject the handshake");
        assert!(!agreed
            .0
            .contains(&Capability::new(Capability::FILE_STAGING)));
    }

    #[test]
    fn clipboard_stage_capability_present_both_sides_is_agreed() {
        let peer = |id: &str| Handshake {
            federation_protocol_version: FEDERATION_PROTOCOL_VERSION,
            capabilities: [Capability::new(Capability::FILE_STAGING)].into(),
            server_instance_id: ServerInstanceId(id.to_string()),
        };

        let agreed =
            negotiate::negotiate(&peer("inst-a"), &peer("inst-b")).expect("versions match");
        assert!(agreed
            .0
            .contains(&Capability::new(Capability::FILE_STAGING)));
    }

    // `serve::global_max_frame` / `federation_accept::global_max_frame` read a
    // single "largest cap" before a frame's true channel is known. This is the
    // guard that keeps that helper honest when a future arm grows.
    #[test]
    fn file_staging_channel_cap_is_the_largest_channel_cap() {
        let all = [
            Channel::Handshake,
            Channel::Mount,
            Channel::Event,
            Channel::Terminal,
            Channel::AgentStatus,
            Channel::Clipboard,
            Channel::Control,
            Channel::FileStaging,
        ];
        for channel in all {
            assert!(
                channel.max_len() <= Channel::largest_max_len(),
                "{channel:?} cap exceeds largest_max_len()"
            );
        }
        assert_eq!(Channel::largest_max_len(), Channel::FileStaging.max_len());
    }

    // The payload is base64 rather than `Vec<u8>` because the codec is
    // serde_json, where a byte vector serialises as a JSON array of decimal
    // numbers (~4x inflation). A max-size image must fit the channel cap.
    #[test]
    fn a_base64_encoded_max_size_image_fits_the_file_staging_cap() {
        use base64::Engine;

        const MAX_IMAGE_BYTES: usize = 16 * 1024 * 1024;
        let payload_base64 =
            base64::engine::general_purpose::STANDARD.encode(vec![0xABu8; MAX_IMAGE_BYTES]);

        let request = FederationMessage::ClipboardStageRequest(ClipboardStageRequest {
            request_id: u64::MAX,
            payload_base64,
            original_filename: "i".repeat(255),
        });
        let encoded = codec::encode(&request).expect("encode must succeed");

        let (decoded, _consumed) =
            codec::decode::<FederationMessage>(&encoded, Channel::FileStaging.max_len())
                .expect("a max-size image must fit the FileStaging cap");
        assert_eq!(decoded, request);
    }

    #[test]
    fn mount_snapshot_cursor_is_the_predecessor_of_the_first_applied_event() {
        let mount = MountSnapshot {
            server_instance_id: ServerInstanceId("inst-a".to_string()),
            snapshot: sample_snapshot(),
            cursor: EventCursor(41),
        };

        let first_applied = EventFrame {
            source_seq: 42,
            kind: EventKind::LayoutUpdated,
        };

        assert_eq!(first_applied.source_seq, mount.cursor.0 + 1);
    }

    #[test]
    fn mount_snapshot_gap_is_detectable_against_the_cursor() {
        let mount = MountSnapshot {
            server_instance_id: ServerInstanceId("inst-a".to_string()),
            snapshot: sample_snapshot(),
            cursor: EventCursor(41),
        };

        // A frame that skips ahead of cursor+1 signals a gap the reducer
        // must detect rather than silently apply.
        let skipped_ahead = EventFrame {
            source_seq: 44,
            kind: EventKind::LayoutUpdated,
        };

        assert_ne!(skipped_ahead.source_seq, mount.cursor.0 + 1);
    }

    #[test]
    fn terminal_output_tagged_with_stale_generation_is_rejected_by_fence() {
        let live_mount = Mount {
            host_key: HostKey::new("alice@10.0.0.1", "s1"),
            server_instance_id: ServerInstanceId("inst-a".to_string()),
            mount_generation: 3,
        };

        let stale_output = TerminalChannelMessage::Output {
            terminal_id: "term_1".to_string(),
            mount_generation: 2,
            bytes: vec![1, 2, 3],
        };

        let fresh_output = TerminalChannelMessage::Output {
            terminal_id: "term_1".to_string(),
            mount_generation: 3,
            bytes: vec![1, 2, 3],
        };

        assert!(matches!(
            fence(&live_mount, stale_output.mount_generation()),
            crate::remote::federation::id::FenceResult::RejectStale { .. }
        ));
        assert_eq!(
            fence(&live_mount, fresh_output.mount_generation()),
            crate::remote::federation::id::FenceResult::Accept
        );
    }
}
