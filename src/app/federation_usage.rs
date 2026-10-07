//! Per-pane usage facts (prompt cache, context usage) forwarded by the
//! serving host of a federation mount, stored on the local mirror terminal so
//! a mounted pane exposes them through the same pane/agent info, events and
//! client snapshots as a local pane.

use crate::api::schema::{ContextUsageInfo, PromptCacheInfo};
use crate::app::App;
use crate::terminal::TerminalId;

impl App {
    /// Stores the serving host's facts on `terminal_id`, the local terminal
    /// the mount's own router resolved for them. Unknown terminal → no-op.
    /// Changed → revision bump + pane.updated for the pane holding it,
    /// whichever workspace that pane has been moved to.
    ///
    /// Runs once per received usage frame, which the serving host sends only
    /// when a terminal's facts change, so the walk is bounded by agent churn
    /// and never runs per render.
    pub(crate) fn handle_federation_pane_usage(
        &mut self,
        terminal_id: &TerminalId,
        prompt_cache: Option<PromptCacheInfo>,
        context_usage: Option<ContextUsageInfo>,
    ) {
        let Some(terminal) = self.state.terminals.get_mut(terminal_id) else {
            return;
        };
        if terminal.prompt_cache == prompt_cache && terminal.context_usage == context_usage {
            return;
        }
        terminal.prompt_cache = prompt_cache;
        terminal.context_usage = context_usage;
        terminal.revision = terminal.revision.saturating_add(1);
        if let Some((ws_idx, pane_id)) = self.pane_holding_terminal(terminal_id) {
            self.emit_pane_updated(ws_idx, pane_id);
        }
        self.render_dirty.request_generic();
    }

    /// The workspace index and pane currently showing `terminal_id`.
    fn pane_holding_terminal(
        &self,
        terminal_id: &TerminalId,
    ) -> Option<(usize, crate::layout::PaneId)> {
        self.state
            .workspaces
            .iter()
            .enumerate()
            .find_map(|(ws_idx, workspace)| {
                workspace
                    .tabs
                    .iter()
                    .flat_map(|tab| tab.panes.iter())
                    .find(|(_, pane)| &pane.attached_terminal_id == terminal_id)
                    .map(|(pane_id, _)| (ws_idx, *pane_id))
            })
    }
}

#[cfg(test)]
mod tests {
    use crate::api::schema::session::SessionSnapshot;
    use crate::api::schema::{
        AgentStatus, ContextUsageInfo, EventData, EventKind, PaneInfo, PromptCacheInfo, TabInfo,
        WorkspaceInfo,
    };
    use crate::app::App;
    use crate::remote::federation::client::TerminalChannelRouter;
    use crate::remote::federation::id::{HostKey, Mount, ServerInstanceId};
    use crate::remote::federation::protocol::EventCursor;
    use crate::remote::federation::reducer::RemoteMirror;
    use crate::terminal::TerminalId;

    fn origin() -> HostKey {
        HostKey::new("alice@10.0.0.1", "s1")
    }

    fn mounted_snapshot() -> SessionSnapshot {
        SessionSnapshot {
            version: "0.0.0-test".to_string(),
            protocol: 1,
            focused_workspace_id: None,
            focused_tab_id: None,
            focused_pane_id: None,
            workspaces: vec![WorkspaceInfo {
                workspace_id: "w1".to_string(),
                number: 1,
                label: "remote workspace".to_string(),
                name_source: crate::workspace::naming::NameSource::Mirrored,
                focused: false,
                pane_count: 1,
                tab_count: 1,
                active_tab_id: "w1-tab".to_string(),
                agent_status: AgentStatus::Idle,
                tokens: Default::default(),
                worktree: None,
                federation_origin: None,
            }],
            tabs: vec![TabInfo {
                tab_id: "w1-tab".to_string(),
                workspace_id: "w1".to_string(),
                number: 1,
                label: "remote tab".to_string(),
                name_source: crate::workspace::naming::NameSource::Mirrored,
                focused: false,
                pane_count: 1,
                agent_status: AgentStatus::Idle,
            }],
            panes: vec![PaneInfo {
                restore_error: None,
                pane_id: "p1".to_string(),
                terminal_id: "term_1".to_string(),
                workspace_id: "w1".to_string(),
                tab_id: "w1-tab".to_string(),
                focused: false,
                cwd: None,
                foreground_cwd: None,
                label: None,
                name_source: crate::workspace::naming::NameSource::default(),
                agent: None,
                title: None,
                terminal_title: None,
                terminal_title_stripped: None,
                display_agent: None,
                agent_status: AgentStatus::Idle,
                state_labels: Default::default(),
                tokens: Default::default(),
                agent_session: None,
                scroll: None,
                prompt_cache: None,
                context_usage: None,
                revision: 0,
            }],
            layouts: Vec::new(),
            agents: Vec::new(),
        }
    }

    /// An App holding one workspace materialized from a mount of `origin()`,
    /// whose single pane mirrors the serving host's `term_1`, and that
    /// mount's router.
    fn mounted_app_with_router(
        snapshot: SessionSnapshot,
    ) -> (App, usize, crate::layout::PaneId, TerminalChannelRouter) {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        let mut mirror = RemoteMirror::new(Mount {
            host_key: origin(),
            server_instance_id: ServerInstanceId("inst-a".to_string()),
            mount_generation: 1,
        });
        mirror.apply_snapshot(&snapshot, EventCursor(0));
        let mut router = TerminalChannelRouter::new();
        let (out_tx, _out_rx) = tokio::sync::mpsc::unbounded_channel();
        let (clipboard_tx, _clipboard_rx) = tokio::sync::mpsc::unbounded_channel();
        let created = app
            .materialize_federation_mount(&mirror, &mut router, &out_tx, &clipboard_tx)
            .expect("materialization succeeds against a loopback-shaped snapshot");
        let ws_idx = created[0];
        let pane_id = app.state.workspaces[ws_idx].tabs[0].root_pane;
        (app, ws_idx, pane_id, router)
    }

    fn mounted_app() -> (App, usize, crate::layout::PaneId) {
        let (app, ws_idx, pane_id, _router) = mounted_app_with_router(mounted_snapshot());
        (app, ws_idx, pane_id)
    }

    /// The local terminal the mounted pane is attached to.
    fn mirror_terminal(app: &App, ws_idx: usize, pane_id: crate::layout::PaneId) -> TerminalId {
        app.state.workspaces[ws_idx]
            .pane_state(pane_id)
            .expect("the mounted pane exists")
            .attached_terminal_id
            .clone()
    }

    fn cache() -> PromptCacheInfo {
        PromptCacheInfo {
            source: "herdr:claude".to_string(),
            last_request_at_ms: 1_700_000_000_000,
            ttl_secs: 300,
        }
    }

    fn context() -> ContextUsageInfo {
        ContextUsageInfo {
            source: "herdr:claude".to_string(),
            used_tokens: 42_000,
            window_tokens: Some(200_000),
            observed_at_ms: 1_700_000_000_500,
        }
    }

    fn info(app: &App, ws_idx: usize, pane_id: crate::layout::PaneId) -> PaneInfo {
        app.pane_info(ws_idx, pane_id)
            .expect("the mounted pane has pane info")
    }

    fn pane_updated_count(app: &App, after: u64) -> usize {
        app.event_hub
            .events_after(after)
            .iter()
            .filter(|(_, envelope)| {
                envelope.event == EventKind::PaneUpdated
                    && matches!(envelope.data, EventData::PaneUpdated { .. })
            })
            .count()
    }

    #[tokio::test]
    async fn federation_pane_usage_sets_both_facts_on_the_mirror_pane() {
        let (mut app, ws_idx, pane_id) = mounted_app();
        let terminal = mirror_terminal(&app, ws_idx, pane_id);
        let revision = info(&app, ws_idx, pane_id).revision;
        let sequence = app.event_hub.current_sequence();

        app.handle_federation_pane_usage(&terminal, Some(cache()), Some(context()));

        let pane = info(&app, ws_idx, pane_id);
        assert_eq!(pane.prompt_cache, Some(cache()));
        assert_eq!(pane.context_usage, Some(context()));
        assert_eq!(pane.revision, revision + 1);
        assert_eq!(pane_updated_count(&app, sequence), 1);
    }

    #[tokio::test]
    async fn federation_pane_usage_with_none_clears_the_facts() {
        let (mut app, ws_idx, pane_id) = mounted_app();
        let terminal = mirror_terminal(&app, ws_idx, pane_id);
        app.handle_federation_pane_usage(&terminal, Some(cache()), Some(context()));
        let revision = info(&app, ws_idx, pane_id).revision;
        let sequence = app.event_hub.current_sequence();

        app.handle_federation_pane_usage(&terminal, None, None);

        let pane = info(&app, ws_idx, pane_id);
        assert_eq!(pane.prompt_cache, None);
        assert_eq!(pane.context_usage, None);
        assert_eq!(pane.revision, revision + 1);
        assert_eq!(pane_updated_count(&app, sequence), 1);
    }

    #[tokio::test]
    async fn federation_pane_usage_ignores_unknown_terminals() {
        let (mut app, ws_idx, pane_id) = mounted_app();
        let revision = info(&app, ws_idx, pane_id).revision;
        let sequence = app.event_hub.current_sequence();

        app.handle_federation_pane_usage(&TerminalId::alloc(), Some(cache()), Some(context()));

        let pane = info(&app, ws_idx, pane_id);
        assert_eq!(pane.prompt_cache, None);
        assert_eq!(pane.context_usage, None);
        assert_eq!(pane.revision, revision);
        assert_eq!(pane_updated_count(&app, sequence), 0);
    }

    #[tokio::test]
    async fn federation_pane_usage_follows_a_mirror_pane_moved_out_of_its_mount() {
        let (mut app, ws_idx, pane_id, router) = mounted_app_with_router(mounted_snapshot());
        let terminal = router
            .local_terminal_id("term_1")
            .expect("the mount maps the serving host's terminal to its mirror")
            .clone();
        assert_eq!(terminal, mirror_terminal(&app, ws_idx, pane_id));
        let public_pane_id = app.public_pane_id(ws_idx, pane_id).unwrap();
        let response = app.handle_api_request(crate::api::schema::Request {
            id: "move".into(),
            method: crate::api::schema::Method::PaneMove(crate::api::schema::PaneMoveParams {
                pane_id: public_pane_id,
                destination: crate::api::schema::PaneMoveDestination::NewWorkspace {
                    label: Some("local".into()),
                    tab_label: None,
                },
                focus: false,
            }),
        });
        let _: crate::api::schema::SuccessResponse =
            serde_json::from_str(&response).expect("the move succeeds");
        let (moved_ws_idx, moved_pane_id) = app
            .pane_holding_terminal(&terminal)
            .expect("the moved pane still shows the mirror terminal");
        assert!(
            app.state.workspaces[moved_ws_idx]
                .worktree_space()
                .is_none_or(|space| space.key != format!("federation:{}", origin().as_str())),
            "the pane now lives outside the mounted workspace"
        );
        let sequence = app.event_hub.current_sequence();

        app.handle_federation_pane_usage(&terminal, Some(cache()), Some(context()));

        let pane = info(&app, moved_ws_idx, moved_pane_id);
        assert_eq!(pane.prompt_cache, Some(cache()));
        assert_eq!(pane.context_usage, Some(context()));
        assert_eq!(pane_updated_count(&app, sequence), 1);
    }

    #[tokio::test]
    async fn mounted_pane_starts_with_the_facts_in_the_mount_snapshot() {
        let mut snapshot = mounted_snapshot();
        snapshot.panes[0].prompt_cache = Some(cache());
        snapshot.panes[0].context_usage = Some(context());

        let (app, ws_idx, pane_id, _router) = mounted_app_with_router(snapshot);

        let pane = info(&app, ws_idx, pane_id);
        assert_eq!(pane.prompt_cache, Some(cache()));
        assert_eq!(pane.context_usage, Some(context()));
    }

    #[tokio::test]
    async fn federation_pane_usage_unchanged_value_does_not_bump_revision() {
        let (mut app, ws_idx, pane_id) = mounted_app();
        let terminal = mirror_terminal(&app, ws_idx, pane_id);
        app.handle_federation_pane_usage(&terminal, Some(cache()), Some(context()));
        let revision = info(&app, ws_idx, pane_id).revision;
        let sequence = app.event_hub.current_sequence();

        app.handle_federation_pane_usage(&terminal, Some(cache()), Some(context()));

        assert_eq!(info(&app, ws_idx, pane_id).revision, revision);
        assert_eq!(pane_updated_count(&app, sequence), 0);
    }
}
