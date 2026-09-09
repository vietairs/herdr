pub mod client;
mod event_hub;
pub mod schema;
mod server;
mod status;
mod subscriptions;
mod wait;

pub use event_hub::EventHub;
pub use server::ServerHandle;
pub(crate) use server::{api_method_name, start_server_with_stop_control};
pub use status::{read_runtime_status_at, RuntimeStatus};

use std::path::PathBuf;

use tokio::sync::mpsc;

use crate::api::schema::{Method, Request};

pub const SOCKET_PATH_ENV_VAR: &str = "HERDR_SOCKET_PATH";

pub(crate) fn request_changes_ui(request: &Request) -> bool {
    matches!(
        &request.method,
        Method::ServerReloadConfig(_)
            | Method::ServerReloadAgentManifests(_)
            | Method::NotificationShow(_)
            | Method::ProductAnnouncementDismiss(_)
            | Method::ReleaseNotesDismiss(_)
            | Method::CommandInvoke(_)
            | Method::WorkspaceCreate(_)
            | Method::WorkspaceMountRemote(_)
            | Method::WorkspaceFocus(_)
            | Method::WorkspaceRename(_)
            | Method::WorkspaceMove(_)
            | Method::WorkspaceMoveBlock(_)
            | Method::WorkspaceReportMetadata(_)
            | Method::WorkspaceClose(_)
            | Method::WorktreeCreate(_)
            | Method::WorktreeOpen(_)
            | Method::WorktreeRemove(_)
            | Method::TabCreate(_)
            | Method::TabFocus(_)
            | Method::TabRename(_)
            | Method::TabMove(_)
            | Method::TabClose(_)
            | Method::LayoutApply(_)
            | Method::LayoutSetSplitRatio(_)
            | Method::AgentRename(_)
            | Method::AgentViewSet(_)
            | Method::AgentViewClear(_)
            | Method::AgentFocus(_)
            | Method::AgentStart(_)
            | Method::AgentPrompt(_)
            | Method::AgentSendKeys(_)
            | Method::PaneSplit(_)
            | Method::PaneSwap(_)
            | Method::PaneMove(_)
            | Method::PaneZoom(_)
            | Method::PaneFocusDirection(_)
            | Method::PaneResize(_)
            | Method::PaneScroll(_)
            | Method::PaneEditScrollback(_)
            | Method::PaneFocus(_)
            | Method::PaneInputSet(_)
            | Method::PaneRename(_)
            | Method::PaneGraphicsSet(_)
            | Method::PaneGraphicsClear(_)
            | Method::PaneGraphicsStream(_)
            | Method::PaneGraphicsStreamSet(_)
            | Method::PaneGraphicsStreamDirect(_)
            | Method::PaneGraphicsStreamOpen(_)
            | Method::PaneGraphicsStreamClose(_)
            | Method::PaneReportAgent(_)
            | Method::PaneReportAgentSession(_)
            | Method::PaneReportMetadata(_)
            | Method::PaneClearAgentAuthority(_)
            | Method::PaneReleaseAgent(_)
            | Method::PaneClose(_)
            | Method::PopupClose(_)
            | Method::PluginUnlink(_)
            | Method::PluginDisable(_)
            | Method::PluginActionInvoke(_)
            | Method::PluginPaneOpen(_)
            | Method::PluginPaneFocus(_)
            | Method::PluginPaneClose(_)
    )
}

/// Closed allowlist of API methods a federated (view-only) in-proc session may
/// execute. A federated session displays a REMOTE workspace mounted into the
/// local device's herdr and must never mutate local workspace/tab/pane/plugin/
/// server state, so it may run ONLY read-only queries, presentation/navigation,
/// and remote input forwarded to the remote panes. Every other method is
/// forbidden.
///
/// This is an EXHAUSTIVE match with no wildcard arm: a newly added `Method`
/// forces a compile error here, so a capability can never be silently granted
/// to — or silently withheld from — a federated session without an explicit,
/// reviewed decision. `PaneZoom` and `PaneResize` are FORBIDDEN despite their
/// "zoom/resize" naming (both mutate persisted local layout); the host-driven
/// remote-terminal resize is not an API method and bypasses this gate entirely.
///
/// Dormant until b2.3 wires it at the dispatch entrances behind the federated
/// construction marker.
#[allow(dead_code)]
pub(crate) fn federated_session_allows(method: &Method) -> bool {
    match method {
        // Read-only queries (snapshot / list / get / status / stream).
        Method::Ping(_)
        | Method::SessionSnapshot(_)
        | Method::ServerAgentManifests(_)
        | Method::WorkspaceList(_)
        | Method::WorkspaceGet(_)
        | Method::WorktreeList(_)
        | Method::TabList(_)
        | Method::TabGet(_)
        | Method::AgentList(_)
        | Method::AgentGet(_)
        | Method::AgentRead(_)
        | Method::AgentExplain(_)
        | Method::AgentWait(_)
        | Method::PaneProcessInfo(_)
        | Method::PaneGraphicsInfo(_)
        | Method::PaneGraphicsStream(_)
        | Method::LayoutExport(_)
        | Method::PaneNeighbor(_)
        | Method::PaneEdges(_)
        | Method::PaneList(_)
        | Method::PaneCurrent(_)
        | Method::PaneGet(_)
        | Method::PaneRead(_)
        | Method::EventsSubscribe(_)
        | Method::EventsWait(_)
        | Method::PaneWaitForOutput(_)
        | Method::PluginList(_)
        | Method::PluginActionList(_)
        | Method::PluginLogList(_)
        | Method::IntegrationList(_)
        // pane.selection.read / pane.copy.motion / pane.copy.search compute
        // and return text/cursor targets from the pane's own content; they
        // never write pane, workspace, or persisted state.
        | Method::PaneSelectionRead(_)
        | Method::PaneCopyMotion(_)
        | Method::PaneCopySearch(_)
        // pane.scroll only moves the ephemeral (unpersisted) viewport
        // offset within the terminal scrollback, like a focus change.
        | Method::PaneScroll(_)
        // Presentation / navigation — view-state focus only, no structural change.
        | Method::WorkspaceFocus(_)
        | Method::TabFocus(_)
        | Method::AgentFocus(_)
        | Method::PaneFocus(_)
        | Method::PaneFocusDirection(_)
        | Method::PluginPaneFocus(_)
        // Remote input forwarded to the remote-backed panes.
        | Method::PaneSendText(_)
        | Method::PaneSendKeys(_)
        | Method::PaneSendInput(_)
        | Method::AgentSendKeys(_)
        | Method::AgentPrompt(_) => true,

        // Everything below mutates local workspace/tab/pane/plugin/server state
        // (or is a client-display command) and is forbidden for a view-only
        // federated session.
        Method::ServerStop(_)
        | Method::ServerLiveHandoff(_)
        | Method::ServerReloadConfig(_)
        | Method::ServerReloadAgentManifests(_)
        | Method::NotificationShow(_)
        // product.announcement.dismiss / release_notes.dismiss persist a
        // local seen/dismissed marker in server state, same class as
        // NotificationShow above.
        | Method::ProductAnnouncementDismiss(_)
        | Method::ReleaseNotesDismiss(_)
        // command.invoke runs an arbitrary client-shell-bound command
        // (up to and including plugin actions with local side effects).
        | Method::CommandInvoke(_)
        | Method::ClientWindowTitleSet(_)
        | Method::ClientWindowTitleClear(_)
        // client.shell.surface.set is a client-display signal, same class
        // as the window-title methods above.
        | Method::ClientShellSurfaceSet(_)
        | Method::WorkspaceCreate(_)
        | Method::WorkspaceMountRemote(_)
        | Method::WorkspaceRename(_)
        | Method::WorkspaceMove(_)
        | Method::WorkspaceMoveBlock(_)
        | Method::WorkspaceReportMetadata(_)
        | Method::WorkspaceClose(_)
        | Method::WorkspaceCloseRemote(_)
        | Method::WorktreeCreate(_)
        | Method::WorktreeOpen(_)
        | Method::WorktreeRemove(_)
        | Method::TabCreate(_)
        | Method::TabRename(_)
        | Method::TabMove(_)
        | Method::TabClose(_)
        | Method::TabCloseRemote(_)
        | Method::AgentRename(_)
        | Method::AgentStart(_)
        // agent.view.set/clear pin a server-side view marker (persisted local
        // state), not remote input — forbidden for view-only sessions.
        | Method::AgentViewSet(_)
        | Method::AgentViewClear(_)
        // pane.graphics.set/clear mutate server-side pane graphics state.
        | Method::PaneGraphicsSet(_)
        | Method::PaneGraphicsClear(_)
        | Method::PaneGraphicsStreamSet(_)
        | Method::PaneGraphicsStreamDirect(_)
        | Method::PaneGraphicsStreamOpen(_)
        | Method::PaneGraphicsStreamClose(_)
        | Method::PopupClose(_)
        | Method::PaneSplit(_)
        | Method::PaneSwap(_)
        | Method::PaneMove(_)
        | Method::PaneZoom(_)
        | Method::PaneLayout(_)
        | Method::LayoutApply(_)
        | Method::LayoutSetSplitRatio(_)
        | Method::LayoutBalance(_)
        | Method::PaneResize(_)
        // pane.edit_scrollback spawns a local editor process over the
        // pane's scrollback, a local host side effect, not remote input.
        | Method::PaneEditScrollback(_)
        // pane.link_activate runs a local plugin link handler for the
        // clicked URL, another local host side effect, not remote input.
        | Method::PaneLinkActivate(_)
        // pane.input.set writes persisted local pane input state
        // (right-click passthrough), not remote input forwarding.
        | Method::PaneInputSet(_)
        | Method::PaneRename(_)
        | Method::PaneReportAgent(_)
        | Method::PaneReportAgentSession(_)
        | Method::PaneReportMetadata(_)
        | Method::PaneClearAgentAuthority(_)
        | Method::PaneReleaseAgent(_)
        | Method::PaneClose(_)
        | Method::IntegrationInstall(_)
        | Method::IntegrationUninstall(_)
        | Method::PluginLink(_)
        | Method::PluginUnlink(_)
        | Method::PluginEnable(_)
        | Method::PluginDisable(_)
        | Method::PluginActionInvoke(_)
        | Method::PluginPaneOpen(_)
        | Method::PluginPaneClose(_) => false,
    }
}

/// Serialized error response for an API method rejected because it is not in
/// the federated-session allowlist (`federated_session_allows`). Shared by both
/// dispatch entrances (`app::api` sync funnel + `app::runtime` deferred funnel)
/// so the forbidden shape stays identical. `federated_mode` is only ever set on
/// unix (`App::new_federated`), so on other platforms this simply never fires.
pub(crate) fn federated_forbidden_response(id: String) -> String {
    use crate::api::schema::{ErrorBody, ErrorResponse};
    let response = ErrorResponse {
        id,
        error: ErrorBody {
            code: "forbidden_in_federated_session".into(),
            message: "this operation is not permitted on a federated remote workspace".into(),
        },
    };
    serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string())
}

pub struct ApiRequestMessage {
    pub request: Request,
    pub respond_to: std::sync::mpsc::Sender<String>,
    pub response_write_complete: Option<std::sync::mpsc::Receiver<()>>,
    pub stream_active: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}

pub type ApiRequestSender = mpsc::UnboundedSender<ApiRequestMessage>;

pub fn socket_path() -> PathBuf {
    crate::session::active_api_socket_path()
}

#[cfg(test)]
mod federated_allowlist_tests {
    use super::federated_session_allows;
    use crate::api::schema::{EmptyParams, Method};

    #[test]
    fn allows_read_only_navigation_and_remote_input() {
        assert!(federated_session_allows(&Method::SessionSnapshot(
            EmptyParams {}
        )));
        assert!(federated_session_allows(&Method::WorkspaceList(
            EmptyParams {}
        )));
        assert!(federated_session_allows(&Method::AgentList(EmptyParams {})));
        // Read-only server query stays allowed...
        assert!(federated_session_allows(&Method::ServerAgentManifests(
            EmptyParams {}
        )));
    }

    #[test]
    fn forbids_server_control_and_client_display_methods() {
        assert!(!federated_session_allows(&Method::ServerStop(
            EmptyParams {}
        )));
        assert!(!federated_session_allows(&Method::ServerReloadConfig(
            EmptyParams {}
        )));
        // ...but the reload counterpart of the allowed query is forbidden.
        assert!(!federated_session_allows(
            &Method::ServerReloadAgentManifests(EmptyParams {})
        ));
        assert!(!federated_session_allows(&Method::ClientWindowTitleClear(
            EmptyParams {}
        )));
    }
}
