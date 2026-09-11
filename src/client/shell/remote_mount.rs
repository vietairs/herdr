//! Client-side collector for `workspace.mount_remote`
//! (`crate::api::schema::WorkspaceMountRemoteParams`). Models the pre-merge
//! `Mode::MountRemoteWorkspace` dialog's shape, but drops its client-owned
//! outcome tracking entirely: per the runtime/client boundary guardrail,
//! "which targets are dialling, mounted, or failed" is a shared runtime fact
//! that lives server-side (`AppState::remote_mount_attempts`,
//! `src/app/state.rs`) and reaches this overlay through
//! `ClientShellSnapshot::remote_mount_attempts`. This module owns only the
//! free-text collector: input editing, recents navigation, and the inline
//! validation/rejection error. Rendering lives in
//! `render_remote_mount_overlay` (`src/client/shell/overlays.rs`) and stays
//! pure -- it reads state and draws, never mutates it.

use super::*;

/// Parses the dialog's free-text input into a whitespace-separated target
/// list. This only rejects "nothing to send" -- target validity (option-like
/// tokens, `localhost`, etc.) is the server's job
/// (`crate::remote::validate_remote_target`, `crate::remote::is_local_target`,
/// `handle_workspace_mount_remote`'s `invalid_request` path); duplicating
/// those rules here risks the client and server disagreeing about what is a
/// valid target.
pub(super) fn parse_remote_mount_targets(input: &str) -> Result<Vec<String>, String> {
    let targets: Vec<String> = input.split_whitespace().map(str::to_string).collect();
    if targets.is_empty() {
        return Err("enter at least one target".to_owned());
    }
    Ok(targets)
}

impl ClientShellState {
    pub(super) fn open_remote_mount_overlay(&mut self) {
        self.overlay = Some(ClientShellOverlay::MountRemote(ClientRemoteMountOverlay {
            input: String::new(),
            error: None,
            recents_highlighted: None,
        }));
    }

    fn remote_mount_recents(&self) -> Vec<String> {
        self.snapshot
            .as_deref()
            .map(|snapshot| snapshot.recent_remote_mount_targets.clone())
            .unwrap_or_default()
    }

    /// Moves the recents highlight by `delta` and copies the newly
    /// highlighted target into the input. From the freshly-opened "nothing
    /// picked" state, both Up and Down land on index 0 (the most recent
    /// target) rather than skipping past it.
    pub(super) fn move_remote_mount_recent_selection(&mut self, delta: isize) {
        let recents = self.remote_mount_recents();
        if recents.is_empty() {
            return;
        }
        let next = match self.overlay.as_ref() {
            Some(ClientShellOverlay::MountRemote(overlay)) => match overlay.recents_highlighted {
                None => 0,
                Some(idx) => (idx as isize + delta).clamp(0, recents.len() as isize - 1) as usize,
            },
            _ => return,
        };
        self.select_remote_mount_recent(next);
    }

    /// Selects recents row `index` directly (used by both keyboard
    /// navigation and a mouse click on a recents row). Selecting a recent
    /// still lets the user edit the input before submitting -- it is not a
    /// lock, just a starting value.
    pub(super) fn select_remote_mount_recent(&mut self, index: usize) {
        let recents = self.remote_mount_recents();
        let Some(target) = recents.get(index).cloned() else {
            return;
        };
        let Some(ClientShellOverlay::MountRemote(overlay)) = self.overlay.as_mut() else {
            return;
        };
        overlay.recents_highlighted = Some(index);
        overlay.input = target;
        overlay.error = None;
    }

    pub(super) fn insert_remote_mount_overlay_text(&mut self, text: &str) -> bool {
        let Some(ClientShellOverlay::MountRemote(overlay)) = self.overlay.as_mut() else {
            return false;
        };
        overlay.input.push_str(text);
        overlay.error = None;
        true
    }

    /// Routes a key event to the mount-remote overlay. Returns whether the
    /// overlay is open and consumed the key, mirroring
    /// `route_worktree_overlay_key`'s "am I even open" gate so the caller
    /// can chain overlay routers with a plain `if ... { return; }`.
    pub(super) fn route_remote_mount_overlay_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) -> bool {
        if !matches!(self.overlay, Some(ClientShellOverlay::MountRemote(_))) {
            return false;
        }
        let (code, modifiers) = crate::config::normalize_key_combo((key.code, key.modifiers));
        match code {
            KeyCode::Esc => {
                self.overlay = None;
                outcome.repaint = true;
            }
            KeyCode::Enter => self.submit_remote_mount(outcome),
            KeyCode::Up => {
                self.move_remote_mount_recent_selection(-1);
                outcome.repaint = true;
            }
            KeyCode::Down => {
                self.move_remote_mount_recent_selection(1);
                outcome.repaint = true;
            }
            KeyCode::Backspace => {
                if let Some(ClientShellOverlay::MountRemote(overlay)) = self.overlay.as_mut() {
                    overlay.input.pop();
                    overlay.error = None;
                }
                outcome.repaint = true;
            }
            KeyCode::Char(character)
                if modifiers
                    .difference(crossterm::event::KeyModifiers::SHIFT)
                    .is_empty() =>
            {
                let text = key
                    .generated_text
                    .clone()
                    .unwrap_or_else(|| character.to_string());
                self.insert_remote_mount_overlay_text(&text);
                outcome.repaint = true;
            }
            _ => {}
        }
        true
    }

    pub(super) fn submit_remote_mount(&mut self, outcome: &mut ClientShellInput) {
        let Some(ClientShellOverlay::MountRemote(overlay)) = self.overlay.as_ref() else {
            return;
        };
        let targets = match parse_remote_mount_targets(&overlay.input) {
            Ok(targets) => targets,
            Err(message) => {
                if let Some(ClientShellOverlay::MountRemote(overlay)) = self.overlay.as_mut() {
                    overlay.error = Some(message);
                }
                outcome.repaint = true;
                return;
            }
        };
        let method = crate::api::schema::Method::WorkspaceMountRemote(
            crate::api::schema::WorkspaceMountRemoteParams {
                targets,
                // No UI exposes this; the pre-merge dialog always sent
                // `false` too (its only reader was never wired to a
                // control -- see the equivalent note in the deleted
                // `src/app/remote_mount.rs`).
                remote_keybindings: false,
            },
        );
        // A server that does not advertise the method would otherwise be
        // reported only through an endpoint notice, which this modal draws
        // over -- leaving the dialog looking inert when the button is
        // pressed. Report it where the user is already looking, the same
        // place a server-side rejection lands.
        if !self.supports_endpoint_method(&method) {
            if let Some(ClientShellOverlay::MountRemote(overlay)) = self.overlay.as_mut() {
                overlay.error = Some(
                    "this server does not support mounting remote workspaces; update and restart it"
                        .to_owned(),
                );
            }
            outcome.repaint = true;
            return;
        }
        self.push_endpoint_method_with_kind(method, PendingEndpointKind::RemoteMount, outcome);
        outcome.repaint = true;
    }

    /// Handles the synchronous ack/rejection for a submitted
    /// `workspace.mount_remote` request. A success ack only means the
    /// dial(s) were accepted and spawned server-side, not that any target is
    /// mounted yet -- live dial state is read from
    /// `snapshot.remote_mount_attempts` at render time instead, so there is
    /// nothing to store here on success. A synchronous error (e.g. an
    /// unparseable target) is the server's own rejection message, surfaced
    /// inline so it cannot disagree with a client-side echo of the same
    /// rule.
    pub(super) fn handle_remote_mount_endpoint_result(
        &mut self,
        result: Result<crate::api::schema::ResponseResult, ClientShellEndpointError>,
    ) -> bool {
        let Err(error) = result else {
            return false;
        };
        if let Some(ClientShellOverlay::MountRemote(overlay)) = self.overlay.as_mut() {
            overlay.error = Some(error.message);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_remote_mount_targets_splits_on_whitespace() {
        let targets = parse_remote_mount_targets("  host-a   alice@host-b:22 ").unwrap();
        assert_eq!(targets, vec!["host-a", "alice@host-b:22"]);
    }

    #[test]
    fn parse_remote_mount_targets_rejects_blank_input() {
        assert!(parse_remote_mount_targets("   ").is_err());
    }

    #[test]
    fn parse_remote_mount_targets_accepts_option_like_and_localhost_tokens() {
        // Target validity (leading-`-`, `localhost`, etc.) is the server's
        // job (`handle_workspace_mount_remote`); the client only rejects
        // "nothing to send".
        assert_eq!(
            parse_remote_mount_targets("host-a -oProxyCommand=x").unwrap(),
            vec!["host-a".to_string(), "-oProxyCommand=x".to_string()]
        );
        assert_eq!(
            parse_remote_mount_targets("localhost").unwrap(),
            vec!["localhost".to_string()]
        );
    }
}
