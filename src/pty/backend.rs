#[cfg(unix)]
mod unix;

#[cfg(unix)]
pub(crate) use unix::*;

#[cfg(windows)]
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

#[cfg(windows)]
pub(crate) struct SpawnedPty {
    pub master: Box<dyn MasterPty + Send>,
    pub child: Box<dyn Child + Send + Sync>,
}

#[cfg(windows)]
pub(crate) fn spawn_with_portable_pty(
    rows: u16,
    cols: u16,
    cmd: CommandBuilder,
) -> std::io::Result<SpawnedPty> {
    // Last-resort backstop: no local PTY may spawn while a federated session is
    // active. The API mutation allowlist blocks pane-creating methods, but any
    // non-API local-pane path funnels through here too (defense-in-depth).
    // Mirrors the same guard in the unix backend.
    if crate::remote::federation::session::federated_session_active() {
        return Err(std::io::Error::other(
            "local pty spawn is forbidden during a federated session",
        ));
    }
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|err| std::io::Error::other(err.to_string()))?;
    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|err| std::io::Error::other(err.to_string()))?;

    Ok(SpawnedPty {
        master: pair.master,
        child,
    })
}
