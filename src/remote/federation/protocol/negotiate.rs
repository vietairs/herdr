//! Version + capability negotiation. Pure: no I/O, operates on already
//! decoded `Handshake` values.

use std::collections::BTreeSet;

use super::{Capability, Handshake, RejectReason};

/// Capabilities both peers agreed on: the intersection of their advertised
/// sets. A capability advertised by only one side (e.g. a newer peer's
/// feature the older peer has never heard of) is silently dropped rather
/// than causing a reject — additive evolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgreedCaps(pub BTreeSet<Capability>);

/// Negotiates a handshake between the local and remote peer.
///
/// Fails only on `federation_protocol_version` mismatch. Capability
/// mismatches are never fatal: unknown/unsupported capabilities on either
/// side are simply excluded from the agreed set.
pub fn negotiate(local: &Handshake, remote: &Handshake) -> Result<AgreedCaps, RejectReason> {
    if local.federation_protocol_version != remote.federation_protocol_version {
        return Err(RejectReason::Version {
            local: local.federation_protocol_version,
            remote: remote.federation_protocol_version,
        });
    }

    let agreed = local
        .capabilities
        .intersection(&remote.capabilities)
        .cloned()
        .collect();

    Ok(AgreedCaps(agreed))
}

#[cfg(test)]
mod tests {
    use super::super::ServerInstanceId;
    use super::*;

    fn handshake(version: u32, caps: &[&str]) -> Handshake {
        Handshake {
            federation_protocol_version: version,
            capabilities: caps.iter().map(|c| Capability::new(*c)).collect(),
            server_instance_id: ServerInstanceId("inst".to_string()),
        }
    }

    #[test]
    fn matching_versions_agree_on_shared_capabilities() {
        let local = handshake(1, &[Capability::CLIPBOARD, Capability::SCROLLBACK_REPLAY]);
        let remote = handshake(1, &[Capability::CLIPBOARD, Capability::AGENT_STATUS]);

        let agreed = negotiate(&local, &remote).expect("versions match, should negotiate");

        assert_eq!(agreed.0, [Capability::new(Capability::CLIPBOARD)].into());
    }

    #[test]
    fn mismatched_versions_are_rejected_with_typed_reason() {
        let local = handshake(1, &[Capability::CLIPBOARD]);
        let remote = handshake(2, &[Capability::CLIPBOARD]);

        let err = negotiate(&local, &remote).expect_err("version skew must be rejected");

        assert_eq!(err, RejectReason::Version { local: 1, remote: 2 });
    }

    #[test]
    fn an_unknown_future_capability_is_ignored_gracefully() {
        let local = handshake(1, &[Capability::CLIPBOARD]);
        // Remote advertises a capability this build has never heard of.
        let remote = handshake(1, &[Capability::CLIPBOARD, "future_capability_from_v2"]);

        let agreed = negotiate(&local, &remote).expect("unknown capability must not be fatal");

        assert_eq!(agreed.0, [Capability::new(Capability::CLIPBOARD)].into());
    }
}
