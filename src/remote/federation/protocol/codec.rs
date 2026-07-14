//! Framed codec for federation messages.
//!
//! Reuses the framing *style* of `crate::protocol::wire` (a `u32` LE length
//! prefix ahead of a bincode payload), but is otherwise independent: this
//! codec carries its own version tag and its own per-channel caps, and is
//! pure (operates on in-memory byte slices, no `Read`/`Write`). Downstream
//! code must go through `encode`/`decode` here rather than hand-rolling a
//! per-call-site decode, so the cap and version checks can never be bypassed.

use serde::de::DeserializeOwned;
use serde::Serialize;

use super::FEDERATION_PROTOCOL_VERSION;

/// `[u32 LE federation protocol version][u32 LE payload length]`.
const FRAME_HEADER_LEN: usize = 8;

/// Typed decode/encode failures.
#[derive(Debug)]
pub enum CodecError {
    /// The frame's declared payload length exceeds the caller-supplied cap
    /// for the channel it was read from. Detected from the header alone,
    /// before the payload bytes are touched — never allocates a
    /// cap-exceeding buffer.
    FrameTooLarge { claimed: usize, max: usize },
    /// The frame's header, length prefix, or bincode payload is invalid.
    Malformed(String),
    /// The frame's federation protocol version does not match this build's.
    VersionSkew { local: u32, remote: u32 },
}

impl std::fmt::Display for CodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CodecError::FrameTooLarge { claimed, max } => {
                write!(f, "federation frame size {claimed} exceeds channel cap {max}")
            }
            CodecError::Malformed(reason) => write!(f, "malformed federation frame: {reason}"),
            CodecError::VersionSkew { local, remote } => write!(
                f,
                "federation protocol version skew: local={local} remote={remote}"
            ),
        }
    }
}

impl std::error::Error for CodecError {}

/// Encodes a message as a length-prefixed, version-tagged frame:
/// `[u32LE federation_protocol_version][u32LE payload length][bincode payload]`.
pub fn encode<M: Serialize>(msg: &M) -> Result<Vec<u8>, CodecError> {
    // serde_json (not bincode): federation frames carry api-schema types
    // (SessionSnapshot, EventKind) that use `#[serde(skip_serializing_if)]` /
    // `default`, which bincode's non-self-describing format cannot round-trip.
    // JSON is self-describing and is exactly what these types are designed for.
    let payload = serde_json::to_vec(msg).map_err(|e| CodecError::Malformed(e.to_string()))?;

    if payload.len() > u32::MAX as usize {
        return Err(CodecError::Malformed(format!(
            "payload length {} exceeds u32::MAX",
            payload.len()
        )));
    }

    let mut frame = Vec::with_capacity(FRAME_HEADER_LEN + payload.len());
    frame.extend_from_slice(&FEDERATION_PROTOCOL_VERSION.to_le_bytes());
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

/// Decodes a frame previously produced by `encode`.
///
/// `max_len` is the caller-selected cap for the channel the frame was read
/// from (see `super::Channel::max_len`). Returns the decoded message and the
/// number of bytes consumed from `buf` (header + payload), so callers reading
/// from a longer buffer (e.g. multiple frames concatenated) can advance past
/// exactly the consumed bytes.
pub fn decode<M: DeserializeOwned>(buf: &[u8], max_len: usize) -> Result<(M, usize), CodecError> {
    if buf.len() < FRAME_HEADER_LEN {
        return Err(CodecError::Malformed(
            "buffer shorter than frame header".to_string(),
        ));
    }

    let remote_version = u32::from_le_bytes(buf[0..4].try_into().unwrap());
    if remote_version != FEDERATION_PROTOCOL_VERSION {
        return Err(CodecError::VersionSkew {
            local: FEDERATION_PROTOCOL_VERSION,
            remote: remote_version,
        });
    }

    let claimed_len = u32::from_le_bytes(buf[4..8].try_into().unwrap()) as usize;
    if claimed_len > max_len {
        // Header alone is enough to reject; the payload bytes are never
        // sliced or copied.
        return Err(CodecError::FrameTooLarge {
            claimed: claimed_len,
            max: max_len,
        });
    }

    let payload_end = FRAME_HEADER_LEN + claimed_len;
    if buf.len() < payload_end {
        return Err(CodecError::Malformed("truncated frame payload".to_string()));
    }

    let payload = &buf[FRAME_HEADER_LEN..payload_end];
    // serde_json::from_slice consumes the whole payload or errors on trailing
    // non-whitespace, so an explicit trailing-bytes check is unnecessary.
    let msg =
        serde_json::from_slice(payload).map_err(|e| CodecError::Malformed(e.to_string()))?;

    Ok((msg, payload_end))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::super::{
        AgentStatusMessage, Capability, Channel, ClipboardMessage, EventChannelMessage,
        EventCursor, EventFrame, FaultMessage, FaultReason, FederationMessage, Handshake,
        HandshakeResponse, MountSnapshot, RejectReason, ScrollbackReplay, TerminalChannelMessage,
    };
    use super::*;
    use crate::api::schema::common::AgentStatus;
    use crate::api::schema::events::EventKind;
    use crate::api::schema::session::SessionSnapshot;
    use crate::remote::federation::id::ServerInstanceId;

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

    fn every_message_variant() -> Vec<FederationMessage> {
        let mut caps = BTreeSet::new();
        caps.insert(Capability::new(Capability::CLIPBOARD));

        vec![
            FederationMessage::Handshake(Handshake {
                federation_protocol_version: FEDERATION_PROTOCOL_VERSION,
                capabilities: caps.clone(),
                server_instance_id: ServerInstanceId("inst-a".to_string()),
            }),
            FederationMessage::HandshakeResponse(HandshakeResponse::Accept {
                agreed_capabilities: caps.clone(),
            }),
            FederationMessage::HandshakeResponse(HandshakeResponse::Reject {
                reason: RejectReason::Version { local: 1, remote: 2 },
            }),
            FederationMessage::MountSnapshot(MountSnapshot {
                server_instance_id: ServerInstanceId("inst-a".to_string()),
                snapshot: sample_snapshot(),
                cursor: EventCursor(41),
            }),
            FederationMessage::Event(EventChannelMessage::Frame(EventFrame {
                source_seq: 42,
                kind: EventKind::LayoutUpdated,
            })),
            FederationMessage::Event(EventChannelMessage::Gap { from: 40, to: 42 }),
            FederationMessage::Event(EventChannelMessage::Reset),
            FederationMessage::Terminal(TerminalChannelMessage::Open {
                terminal_id: "term_1".to_string(),
                mount_generation: 3,
                replay: ScrollbackReplay {
                    bytes: vec![1, 2, 3],
                },
            }),
            FederationMessage::Terminal(TerminalChannelMessage::Output {
                terminal_id: "term_1".to_string(),
                mount_generation: 3,
                bytes: vec![4, 5, 6],
            }),
            FederationMessage::Terminal(TerminalChannelMessage::Input {
                terminal_id: "term_1".to_string(),
                mount_generation: 3,
                bytes: vec![7, 8, 9],
            }),
            FederationMessage::Terminal(TerminalChannelMessage::Resize {
                terminal_id: "term_1".to_string(),
                mount_generation: 3,
                cols: 80,
                rows: 24,
            }),
            FederationMessage::Terminal(TerminalChannelMessage::Close {
                terminal_id: "term_1".to_string(),
                mount_generation: 3,
            }),
            FederationMessage::AgentStatus(AgentStatusMessage {
                terminal_id: "term_1".to_string(),
                mount_generation: 3,
                status: AgentStatus::Working,
            }),
            FederationMessage::Clipboard(ClipboardMessage {
                origin_tag: "local".to_string(),
                payload: vec![10, 11, 12],
            }),
            FederationMessage::Fault(FaultMessage {
                reason: FaultReason::EgressOverflow,
            }),
        ]
    }

    #[test]
    fn every_message_variant_round_trips_through_encode_decode() {
        for msg in every_message_variant() {
            let channel = msg.channel();
            let frame = encode(&msg).expect("encode should succeed");
            let (decoded, consumed): (FederationMessage, usize) =
                decode(&frame, channel.max_len()).expect("decode should succeed");

            assert_eq!(decoded, msg, "round trip mismatch for channel {channel:?}");
            assert_eq!(consumed, frame.len());
        }
    }

    #[test]
    fn oversized_frame_is_rejected_without_reading_payload() {
        let msg = FederationMessage::Clipboard(ClipboardMessage {
            origin_tag: "local".to_string(),
            payload: vec![0u8; 1024],
        });
        let frame = encode(&msg).expect("encode should succeed");

        let tiny_cap = 4; // smaller than the actual payload
        let err = decode::<FederationMessage>(&frame, tiny_cap)
            .expect_err("oversized frame must be rejected");

        match err {
            CodecError::FrameTooLarge { claimed, max } => {
                assert!(claimed > tiny_cap);
                assert_eq!(max, tiny_cap);
            }
            other => panic!("expected FrameTooLarge, got {other:?}"),
        }
    }

    #[test]
    fn channel_cap_enforced_for_clipboard_channel() {
        let msg = FederationMessage::Clipboard(ClipboardMessage {
            origin_tag: "local".to_string(),
            payload: vec![0u8; Channel::Clipboard.max_len() + 1],
        });
        let frame = encode(&msg).expect("encode should succeed");

        let err = decode::<FederationMessage>(&frame, Channel::Clipboard.max_len())
            .expect_err("payload exceeding the clipboard channel cap must be rejected");

        assert!(matches!(err, CodecError::FrameTooLarge { .. }));
    }

    #[test]
    fn decode_rejects_mismatched_federation_protocol_version() {
        let msg = FederationMessage::Event(EventChannelMessage::Reset);
        let mut frame = encode(&msg).expect("encode should succeed");
        // Corrupt the version tag in the header.
        frame[0..4].copy_from_slice(&(FEDERATION_PROTOCOL_VERSION + 1).to_le_bytes());

        let err = decode::<FederationMessage>(&frame, Channel::Event.max_len())
            .expect_err("version-skewed frame must be rejected");

        assert!(matches!(
            err,
            CodecError::VersionSkew { local, remote }
                if local == FEDERATION_PROTOCOL_VERSION && remote == FEDERATION_PROTOCOL_VERSION + 1
        ));
    }

    #[test]
    fn decode_rejects_truncated_frame_as_malformed() {
        let msg = FederationMessage::Event(EventChannelMessage::Reset);
        let frame = encode(&msg).expect("encode should succeed");

        let truncated = &frame[..frame.len() - 1];
        let err = decode::<FederationMessage>(truncated, Channel::Event.max_len())
            .expect_err("truncated frame must be rejected");

        assert!(matches!(err, CodecError::Malformed(_)));
    }
}
