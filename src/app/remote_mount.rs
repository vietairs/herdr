//! Collector for `workspace.mount_remote`
//! (`src/api/schema/workspaces.rs::WorkspaceMountRemoteParams`). The mount
//! request is an existing shared runtime fact reachable end-to-end
//! server-side; this module only owns the TUI-side collector: a free-text
//! input dialog and inline error rendering. Target validity (not just "is
//! the input non-empty") is the server's job (`src/app/api/workspaces.rs`'s
//! `invalid_request` path) — this module does not duplicate it, so the
//! dialog and the server cannot disagree on what is a valid target. No new
//! API method, socket message, or event is added here — see
//! `runtime_workspace_mount_remote` in `runtime_mutations.rs`.

use crossterm::event::{KeyCode, KeyEvent};

use crate::api::schema::{ErrorResponse, SuccessResponse, WorkspaceMountRemoteParams};

use super::state::{Mode, RemoteMountState};
use super::App;

/// Parses the dialog's free-text input into a whitespace-separated target
/// list. This only handles "there is nothing to send" (blank input) —
/// target validity (option-like tokens, `localhost`, etc.) is the server's
/// job (`src/remote/unix.rs::validate_remote_target`, `is_local_target`,
/// `src/app/api/workspaces.rs`'s `invalid_request` path); duplicating those
/// rules client-side risks the two layers disagreeing on what is valid.
pub(crate) fn parse_mount_targets(input: &str) -> Result<Vec<String>, String> {
    let targets: Vec<String> = input.split_whitespace().map(str::to_string).collect();
    if targets.is_empty() {
        return Err("enter at least one target".to_string());
    }
    Ok(targets)
}

/// Builds the request params for a validated target list. `remote_keybindings`
/// has no UI (`src/api/schema/workspaces.rs:31` is never read by the handler,
/// `src/app/api/workspaces.rs:53-126`), so it is always sent `false`.
fn mount_remote_params(targets: Vec<String>) -> WorkspaceMountRemoteParams {
    WorkspaceMountRemoteParams {
        targets,
        remote_keybindings: false,
    }
}

impl App {
    pub(crate) fn open_remote_mount_dialog(&mut self) {
        self.state.remote_mount = Some(RemoteMountState::default());
        self.state.name_input.clear();
        self.state.name_input_replace_on_type = false;
        self.state.mode = Mode::MountRemoteWorkspace;
    }

    pub(crate) fn close_remote_mount_dialog(&mut self) {
        self.state.remote_mount = None;
        self.state.name_input.clear();
        self.state.name_input_replace_on_type = false;
        self.state.mode = if self.state.active.is_some() {
            Mode::Terminal
        } else {
            Mode::Navigate
        };
    }

    pub(crate) fn handle_remote_mount_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.close_remote_mount_dialog();
            }
            KeyCode::Enter => self.submit_remote_mount_via_api(),
            KeyCode::Backspace => {
                if self.state.name_input_replace_on_type {
                    self.state.name_input.clear();
                    self.state.name_input_replace_on_type = false;
                } else {
                    self.state.name_input.pop();
                }
                if let Some(remote_mount) = &mut self.state.remote_mount {
                    remote_mount.error = None;
                }
            }
            KeyCode::Char(c) => {
                self.insert_remote_mount_text(&c.to_string());
            }
            _ => {}
        }
    }

    /// Inserts text into the shared `name_input` field (also used by paste).
    pub(crate) fn insert_remote_mount_text(&mut self, text: &str) {
        if self.state.name_input_replace_on_type {
            self.state.name_input.clear();
            self.state.name_input_replace_on_type = false;
        }
        self.state.name_input.push_str(text);
        if let Some(remote_mount) = &mut self.state.remote_mount {
            remote_mount.error = None;
        }
    }

    pub(crate) fn submit_remote_mount_via_api(&mut self) {
        let targets = match parse_mount_targets(&self.state.name_input) {
            Ok(targets) => targets,
            Err(err) => {
                if let Some(remote_mount) = &mut self.state.remote_mount {
                    remote_mount.error = Some(err);
                }
                return;
            }
        };

        let response = self.runtime_workspace_mount_remote(
            "tui.workspace.mount_remote",
            mount_remote_params(targets),
        );
        if serde_json::from_str::<SuccessResponse>(&response).is_ok() {
            self.close_remote_mount_dialog();
        } else if let Ok(error) = serde_json::from_str::<ErrorResponse>(&response) {
            if let Some(remote_mount) = &mut self.state.remote_mount {
                remote_mount.error = Some(error.error.message);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crossterm::event::KeyModifiers;

    fn test_app() -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        App::new(
            &Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        )
    }

    #[test]
    fn parse_mount_targets_splits_on_whitespace_and_trims() {
        let targets = parse_mount_targets("  host-a   alice@host-b:22 ").unwrap();
        assert_eq!(targets, vec!["host-a", "alice@host-b:22"]);
    }

    #[test]
    fn parse_mount_targets_rejects_blank_input() {
        assert!(parse_mount_targets("   ").is_err());
    }

    #[test]
    fn parse_mount_targets_accepts_option_like_and_localhost_tokens() {
        // Target validity (leading-`-`, `localhost`, etc.) is the server's
        // job now (`src/app/api/workspaces.rs::handle_workspace_mount_remote`);
        // the client only rejects "nothing to send".
        assert_eq!(
            parse_mount_targets("host-a -oProxyCommand=x").unwrap(),
            vec!["host-a".to_string(), "-oProxyCommand=x".to_string()]
        );
        assert_eq!(
            parse_mount_targets("localhost").unwrap(),
            vec!["localhost".to_string()]
        );
    }

    #[test]
    fn open_remote_mount_dialog_sets_mode_and_clears_input() {
        let mut app = test_app();
        app.state.name_input = "leftover".into();
        app.state.mode = Mode::Navigate;

        app.open_remote_mount_dialog();

        assert_eq!(app.state.mode, Mode::MountRemoteWorkspace);
        assert!(app.state.name_input.is_empty());
        assert_eq!(app.state.remote_mount, Some(RemoteMountState::default()));
    }

    #[test]
    fn close_remote_mount_dialog_returns_to_terminal_when_a_workspace_is_active() {
        let mut app = test_app();
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("test")];
        app.state.active = Some(0);
        app.open_remote_mount_dialog();

        app.close_remote_mount_dialog();

        assert_eq!(app.state.mode, Mode::Terminal);
        assert!(app.state.remote_mount.is_none());
        assert!(app.state.name_input.is_empty());
    }

    #[test]
    fn close_remote_mount_dialog_returns_to_navigate_when_no_workspace() {
        let mut app = test_app();
        app.open_remote_mount_dialog();

        app.close_remote_mount_dialog();

        assert_eq!(app.state.mode, Mode::Navigate);
        assert!(app.state.remote_mount.is_none());
    }

    #[test]
    fn remote_mount_key_char_and_backspace_edit_the_input() {
        let mut app = test_app();
        app.open_remote_mount_dialog();

        app.handle_remote_mount_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::empty()));
        app.handle_remote_mount_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::empty()));
        assert_eq!(app.state.name_input, "hi");

        app.handle_remote_mount_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::empty()));
        assert_eq!(app.state.name_input, "h");
    }

    #[test]
    fn remote_mount_esc_closes_the_dialog() {
        let mut app = test_app();
        app.open_remote_mount_dialog();

        app.handle_remote_mount_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));

        assert_ne!(app.state.mode, Mode::MountRemoteWorkspace);
        assert!(app.state.remote_mount.is_none());
    }

    #[test]
    fn remote_mount_mode_keeps_the_ime() {
        assert!(!Mode::MountRemoteWorkspace.wants_ascii_input());
    }

    #[test]
    fn submit_remote_mount_with_invalid_input_keeps_dialog_open_and_sets_inline_error() {
        let mut app = test_app();
        app.open_remote_mount_dialog();
        app.state.name_input = "   ".into();

        app.submit_remote_mount_via_api();

        assert_eq!(app.state.mode, Mode::MountRemoteWorkspace);
        assert!(app
            .state
            .remote_mount
            .as_ref()
            .is_some_and(|remote_mount| remote_mount.error.is_some()));
    }

    #[test]
    fn mount_remote_params_always_send_remote_keybindings_false() {
        let params = mount_remote_params(vec!["host-a".to_string()]);
        assert!(!params.remote_keybindings);
        assert_eq!(params.targets, vec!["host-a".to_string()]);
    }

    // Never submit a dialable target in these tests — it would spawn a real
    // ssh child. Seed a pre-mounted host so the handler acks synchronously
    // without spawning a dial (mirrors
    // `duplicate_host_key_target_is_isolated_and_named_in_failure_event`,
    // `src/app/api/workspaces.rs:1268-1300`). The handler still spawns a
    // fire-and-forget `FederationMountFailed` event for the duplicate host
    // (`src/app/api/workspaces.rs:78-90`), so this asserts the mirror count
    // and dialog state, not "no event".
    #[cfg(unix)]
    #[tokio::test]
    async fn submit_remote_mount_closes_dialog_on_success_ack() {
        let mut app = test_app();
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("local")];
        app.state.active = Some(0);

        let session_name = crate::session::active_name()
            .unwrap_or_else(|| crate::session::DEFAULT_SESSION_NAME.to_string());
        let mirror = crate::remote::federation::reducer::RemoteMirror::new(
            crate::remote::federation::id::Mount {
                host_key: crate::remote::federation::id::HostKey::new(
                    "already-mounted-host",
                    &session_name,
                ),
                server_instance_id: crate::remote::federation::id::ServerInstanceId(
                    "inst-1".to_string(),
                ),
                mount_generation: 1,
            },
        );
        app.state.begin_federation_mount(mirror).unwrap();

        app.open_remote_mount_dialog();
        app.state.name_input = "already-mounted-host".into();
        app.submit_remote_mount_via_api();

        assert!(app.state.remote_mount.is_none());
        assert_eq!(app.state.mode, Mode::Terminal);
        assert_eq!(app.state.remote_mirrors.len(), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn submit_remote_mount_keeps_dialog_open_with_error_from_server() {
        // `parse_mount_targets` no longer rejects "localhost" client-side, so
        // this dispatches to the server and exercises
        // `handle_workspace_mount_remote`'s synchronous `invalid_request`
        // rejection (`src/app/api/workspaces.rs:93-100`). This asserts the
        // dialog surfaces the server's own error message, not a client-side
        // echo of it.
        let mut app = test_app();
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("local")];
        app.state.active = Some(0);
        app.open_remote_mount_dialog();
        app.state.name_input = "localhost".into();

        app.submit_remote_mount_via_api();

        assert_eq!(app.state.mode, Mode::MountRemoteWorkspace);
        let error = app
            .state
            .remote_mount
            .as_ref()
            .and_then(|remote_mount| remote_mount.error.as_ref())
            .expect("server should return an error for a localhost target");
        assert!(
            error.contains("localhost"),
            "expected the server's own error message, got: {error}"
        );
        assert!(app.state.remote_mirrors.is_empty());
    }
}
