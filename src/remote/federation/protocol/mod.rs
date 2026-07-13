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
pub const FEDERATION_PROTOCOL_VERSION: u32 = 1;

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
    Gap { from: u64, to: u64 },
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
            Self::Open { mount_generation, .. }
            | Self::Output { mount_generation, .. }
            | Self::Input { mount_generation, .. }
            | Self::Resize { mount_generation, .. }
            | Self::Close { mount_generation, .. } => *mount_generation,
        }
    }

    /// The `terminal_id` tag carried by every variant.
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
}

/// Clipboard-channel message. `origin_tag` identifies which side produced
/// the payload so a receiver can distinguish its own echoed clipboard from
/// a genuinely remote one (consumed by later phases).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardMessage {
    pub origin_tag: String,
    pub payload: Vec<u8>,
}

/// The six federation channel classes, used to select a per-channel frame
/// cap in the codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    Handshake,
    Mount,
    Event,
    Terminal,
    AgentStatus,
    Clipboard,
}

impl Channel {
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
        }
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
