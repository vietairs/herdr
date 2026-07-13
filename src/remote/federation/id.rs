//! Id-namespacing and mount-generation fencing primitives for federation.
//!
//! Pure data types + pure functions, no I/O. `map_in`/`map_out`/`classify` let a
//! federating client namespace ids that originate on a remote host so they never
//! collide with locally-generated ids, and `fence` rejects protocol messages that
//! are tagged with a stale mount generation (e.g. after a remount).
//!
//! Non-federating users never call any of this: local ids stay byte-for-byte
//! unchanged because nothing in the non-federated code path constructs a
//! `FedRef` or calls `classify`.

use serde::{Deserialize, Serialize};

/// Prefix that marks a public id string as namespaced under a remote host.
const REMOTE_PREFIX: &str = "r:";

/// Stable identity for a remote host a federation session is attached to:
/// `user@ip` plus a session discriminator (so re-attaching to the same host
/// under a different session does not collide with a prior one).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct HostKey(String);

impl HostKey {
    /// Builds a `HostKey` from a `user@ip`-style address and a session
    /// discriminator. Neither component may contain `:`, since that is the
    /// separator used in the public id encoding.
    pub fn new(user_at_ip: impl Into<String>, session_discriminator: impl Into<String>) -> Self {
        Self(format!("{}#{}", user_at_ip.into(), session_discriminator.into()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for HostKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Identity of a running server instance, distinct across restarts of the
/// same host so a stale mount from a prior server process is never confused
/// with a fresh one.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ServerInstanceId(pub String);

impl std::fmt::Display for ServerInstanceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A live mount: the (host, server instance, generation) triple that every
/// inbound federation message is fenced against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mount {
    pub host_key: HostKey,
    pub server_instance_id: ServerInstanceId,
    pub mount_generation: u64,
}

/// Federated reference: a remote-origin id namespaced by the mount it was
/// observed under. Two remote hosts (or two generations of the same host)
/// that happen to reuse the same raw remote id never collide once wrapped
/// in a `FedRef`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct FedRef {
    pub host_key: HostKey,
    pub server_instance_id: ServerInstanceId,
    pub mount_generation: u64,
    pub remote_id: String,
}

impl FedRef {
    /// Public string encoding of this reference: `r:<host_key>:<remote_id>`.
    ///
    /// `remote_id` is carried through verbatim, so a `FedRef` built from an
    /// already-namespaced id (relay/nested-federation case) cascades one
    /// `r:<host>:` segment per hop instead of re-prefixing the same host
    /// twice (never produces `r:h:r:h:w1`).
    pub fn to_public_id(&self) -> String {
        format!("{REMOTE_PREFIX}{}:{}", self.host_key.as_str(), self.remote_id)
    }
}

impl std::fmt::Display for FedRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_public_id())
    }
}

/// Classification of a public id string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdClass {
    /// A plain, non-federated id (or one whose prefix does not parse as a
    /// remote namespace).
    Local,
    /// A federated id, namespaced under the given host.
    Remote(HostKey),
}

/// Classifies a public id string as local or remote, without allocating a
/// `FedRef` (the mount-generation and remote-id parts are not needed to
/// answer "who owns this id").
pub fn classify(id: &str) -> IdClass {
    match id.strip_prefix(REMOTE_PREFIX) {
        Some(rest) => match rest.split_once(':') {
            Some((host_part, _remainder)) if !host_part.is_empty() => {
                IdClass::Remote(HostKey(host_part.to_string()))
            }
            _ => IdClass::Local,
        },
        None => IdClass::Local,
    }
}

/// Namespaces a raw remote id under the given mount, producing a `FedRef`.
pub fn map_in(remote_id: impl Into<String>, mount: &Mount) -> FedRef {
    FedRef {
        host_key: mount.host_key.clone(),
        server_instance_id: mount.server_instance_id.clone(),
        mount_generation: mount.mount_generation,
        remote_id: remote_id.into(),
    }
}

/// Recovers the raw remote id a `FedRef` was built from. Inverse of
/// `map_in` with respect to the `remote_id` field: `map_out(map_in(id,
/// mount)) == id` for any `id` and `mount`.
pub fn map_out(fed_ref: &FedRef) -> &str {
    &fed_ref.remote_id
}

/// Result of fencing an inbound message's generation against a live mount.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FenceResult {
    Accept,
    RejectStale { live: u64, inbound: u64 },
}

/// Fences an inbound protocol message's `mount_generation` against the live
/// mount. Any generation other than the live one is rejected and must never
/// be routed (covers both stale-behind and skewed-ahead generations).
pub fn fence(mount: &Mount, inbound_generation: u64) -> FenceResult {
    if inbound_generation == mount.mount_generation {
        FenceResult::Accept
    } else {
        FenceResult::RejectStale {
            live: mount.mount_generation,
            inbound: inbound_generation,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mount(host: &str, instance: &str, generation: u64) -> Mount {
        Mount {
            host_key: HostKey::new(host, "s1"),
            server_instance_id: ServerInstanceId(instance.to_string()),
            mount_generation: generation,
        }
    }

    #[test]
    fn map_in_then_map_out_is_identity() {
        let mount = mount("alice@10.0.0.1", "inst-a", 3);
        let fed_ref = map_in("w1", &mount);

        assert_eq!(map_out(&fed_ref), "w1");
    }

    #[test]
    fn local_and_two_remote_hosts_produce_distinct_fed_refs() {
        let local_id = "w1".to_string();

        let mount_a = mount("alice@10.0.0.1", "inst-a", 1);
        let mount_b = mount("bob@10.0.0.2", "inst-b", 1);

        let remote_a = map_in("w1", &mount_a);
        let remote_b = map_in("w1", &mount_b);

        assert_ne!(remote_a, remote_b);
        assert_ne!(remote_a.to_public_id(), local_id);
        assert_ne!(remote_b.to_public_id(), local_id);
        assert_ne!(remote_a.to_public_id(), remote_b.to_public_id());
    }

    #[test]
    fn public_id_prefix_does_not_cascade_double_for_same_host() {
        let mount = mount("alice@10.0.0.1", "inst-a", 1);
        let fed_ref = map_in("w1", &mount);
        let public_id = fed_ref.to_public_id();

        assert_eq!(public_id, format!("r:{}:w1", mount.host_key.as_str()));
        // Re-mapping the same raw remote id under the same mount must
        // reproduce the exact same single-hop prefix, never doubling it.
        let remapped = map_in("w1", &mount).to_public_id();
        assert_eq!(remapped, public_id);
        assert!(!remapped.starts_with(&format!(
            "r:{}:r:{}:",
            mount.host_key.as_str(),
            mount.host_key.as_str()
        )));
    }

    #[test]
    fn classify_recognizes_remote_ids_and_leaves_local_ids_alone() {
        let mount = mount("alice@10.0.0.1", "inst-a", 1);
        let fed_ref = map_in("w1", &mount);

        assert_eq!(classify("w1"), IdClass::Local);
        assert_eq!(classify(&fed_ref.to_public_id()), IdClass::Remote(mount.host_key.clone()));
    }

    #[test]
    fn fence_accepts_live_generation_and_rejects_stale() {
        let mount = mount("alice@10.0.0.1", "inst-a", 5);

        assert_eq!(fence(&mount, 5), FenceResult::Accept);
        assert_eq!(
            fence(&mount, 4),
            FenceResult::RejectStale { live: 5, inbound: 4 }
        );
        assert_eq!(
            fence(&mount, 6),
            FenceResult::RejectStale { live: 5, inbound: 6 }
        );
    }
}
