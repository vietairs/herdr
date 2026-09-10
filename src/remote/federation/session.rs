//! Dial + mount half of a federation session.
//!
//! `dial_and_mount` opens a live federation tunnel to a remote herdr server,
//! performs the handshake and mount under the P4 connect/mount timeouts, and
//! hands the caller a `RemoteMirror` plus the raw tunnel halves. It owns no
//! terminal and no `App`: it is pure async I/O, `Send`-shaped, and safe to
//! `tokio::spawn` from any tokio context.
//!
//! The only caller is `remote::attach`'s mount path, which materializes the
//! returned mirror into the running server's workspace set. There is no
//! standalone federated TUI runner: mounting a remote into an existing session
//! (`workspace.mount_remote`) and attaching to a remote server are the two
//! supported shapes.

use std::io;

use crate::remote::federation::client::FederationClient;
use crate::remote::federation::id::HostKey;
use crate::remote::federation::protocol::Capability;
use crate::remote::{
    dial_federation, ManagedSshOptions, RemoteHerdr, FEDERATION_CONNECT_TIMEOUT,
    FEDERATION_MOUNT_TIMEOUT,
};

/// Local-capability set advertised to the federation host. It is a strict
/// superset of the one-shot `attempt_federation_mount` snapshot dial, which
/// advertises only `SCROLLBACK_REPLAY` and `AGENT_STATUS`: a live mount also
/// advertises `FILE_STAGING`, `WORKSPACE_TAB_CLOSE` and `PANE_NAME_SOURCE`.
/// A capability added here is therefore not automatically advertised by the
/// snapshot dial — update both when a new one must apply to each.
fn local_capabilities() -> std::collections::BTreeSet<Capability> {
    [
        Capability::new(Capability::SCROLLBACK_REPLAY),
        Capability::new(Capability::AGENT_STATUS),
        // Advertised unconditionally on the mounting side: this side only
        // sends bytes and reads back a path, it touches no filesystem, so it
        // has nothing to gate on the local platform. Whether a stage request
        // is ever sent is decided by what the *host* also advertised.
        //
        // On a non-Unix client the advert is true only by omission: the sole
        // initiator, `app::remote_clipboard_stage`, is `#[cfg(unix)]`, so no
        // stage frame can be built at all. Un-gating that module means
        // un-gating this promise with it.
        Capability::new(Capability::FILE_STAGING),
        // Gates `workspace.close_remote`/`tab.close_remote` forwarding
        // (federation close forwarding for the multi-workspace/tab case).
        // Advertised unconditionally: like `FILE_STAGING`, this side only
        // ever sends the request and reads back a response, so there is
        // nothing local to gate on.
        Capability::new(Capability::WORKSPACE_TAB_CLOSE),
        // States that this build reports `PaneInfo::name_source`, so a peer
        // may read that field's absence as absent rather than as the
        // `Ordinal` its `serde` default would otherwise manufacture.
        // Advertised unconditionally: it describes what this build's own
        // `PaneInfo` carries, which no local platform fact can change.
        Capability::new(Capability::PANE_NAME_SOURCE),
    ]
    .into_iter()
    .collect()
}

/// Live outcome of dialing + mounting a federation target: everything the
/// caller needs to materialize the mount into an `App` and drive it, with no
/// terminal/TTY ownership assumed. Extracted out of `run_federated_session`
/// (REVISED Phase A step 3) so the server daemon's own async task can reuse
/// exactly this dial+mount sequence without pulling in any of
/// `run_federated_session`'s terminal-mode setup.
pub(crate) struct DialAndMountOutcome {
    pub(crate) mirror: crate::remote::federation::reducer::RemoteMirror,
    pub(crate) generation: u64,
    pub(crate) tunnel_guard: crate::remote::ChildGuard,
    pub(crate) tunnel_reader: tokio::process::ChildStdout,
    pub(crate) tunnel_writer: tokio::process::ChildStdin,
}

/// Dials `target`'s live herdr server and mounts a federation session over
/// it, applying the same connect/mount timeouts and empty-mirror rejection
/// `run_federated_session` always has. No `App`, no TTY — pure async I/O,
/// `Send`-shaped, safe to `tokio::spawn` from any tokio context (server
/// daemon or CLI process alike).
pub(crate) async fn dial_and_mount(
    target: &str,
    remote_herdr: &RemoteHerdr,
    session_name: &str,
    ssh_options: Option<&ManagedSshOptions>,
) -> io::Result<DialAndMountOutcome> {
    let tunnel = dial_federation(target, remote_herdr, session_name, ssh_options)
        .await
        .map_err(|err| io::Error::other(format!("federation dial failed: {err:?}")))?;
    let crate::remote::LiveTunnel {
        guard: tunnel_guard,
        reader,
        writer,
    } = tunnel;

    let client = FederationClient::new(
        HostKey::new(target, session_name),
        local_capabilities(),
        std::collections::BTreeSet::new(),
    );

    // Wrap `connect_and_mount`'s otherwise-unbounded reads: a live tunnel
    // (unlike the one-shot snapshot dial) can hang forever on a stalled
    // link. Use the connect budget for the whole handshake+mount round — it
    // is the tighter of the two; mount is bounded by the same await.
    let mount_budget = FEDERATION_CONNECT_TIMEOUT + FEDERATION_MOUNT_TIMEOUT;
    let mounted =
        match tokio::time::timeout(mount_budget, client.connect_and_mount(reader, writer)).await {
            Ok(Ok(mounted)) => mounted,
            Ok(Err(err)) => {
                return Err(io::Error::other(format!("federation mount failed: {err}")));
            }
            Err(_elapsed) => {
                return Err(io::Error::other(
                    "federation mount timed out before a live workspace was ready",
                ));
            }
        };

    // Non-empty required: an empty mount has nothing to render — abort so
    // the caller falls back to classic rather than entering an empty TUI.
    if mounted.mirror.workspaces().is_empty() {
        return Err(io::Error::other(
            "federation mount returned no remote workspaces",
        ));
    }

    let mirror = mounted.mirror;
    let generation = mirror.mount().mount_generation;
    Ok(DialAndMountOutcome {
        mirror,
        generation,
        tunnel_guard,
        tunnel_reader: mounted.reader,
        tunnel_writer: mounted.writer,
    })
}
