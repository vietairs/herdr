//! Per-pane usage facts (prompt cache, context usage) forwarded by the
//! serving host of a federation mount, stored on the local mirror terminal so
//! a mounted pane exposes them through the same pane/agent info, events and
//! client snapshots as a local pane.

use crate::api::schema::{ContextUsageInfo, PromptCacheInfo};
use crate::app::App;
use crate::remote::federation::id::HostKey;

impl App {
    /// Stores the serving host's facts on the matching mirror terminal of a
    /// workspace mounted from `origin`. Unknown terminal or origin mismatch →
    /// no-op. Changed → revision bump + pane.updated.
    ///
    /// Runs once per received usage frame, which the serving host sends only
    /// when a terminal's facts change, so the walk is bounded by agent churn
    /// and never runs per render.
    pub(crate) fn handle_federation_pane_usage(
        &mut self,
        origin: HostKey,
        terminal_id: String,
        prompt_cache: Option<PromptCacheInfo>,
        context_usage: Option<ContextUsageInfo>,
    ) {
        let Some((ws_idx, pane_id, local_terminal_id)) =
            self.federation_mirror_pane(&origin, &terminal_id)
        else {
            return;
        };
        let Some(terminal) = self.state.terminals.get_mut(&local_terminal_id) else {
            return;
        };
        if terminal.prompt_cache == prompt_cache && terminal.context_usage == context_usage {
            return;
        }
        terminal.prompt_cache = prompt_cache;
        terminal.context_usage = context_usage;
        terminal.revision = terminal.revision.saturating_add(1);
        self.emit_pane_updated(ws_idx, pane_id);
        self.render_dirty.request_generic();
    }

    /// The first pane of a workspace mounted from `origin` whose runtime
    /// mirrors the serving host's raw `remote_terminal_id`, with its local
    /// terminal id.
    fn federation_mirror_pane(
        &self,
        origin: &HostKey,
        remote_terminal_id: &str,
    ) -> Option<(usize, crate::layout::PaneId, crate::terminal::TerminalId)> {
        (0..self.state.workspaces.len())
            .filter(|ws_idx| self.workspace_matches_federation_origin(*ws_idx, origin))
            .find_map(|ws_idx| {
                self.state.workspaces[ws_idx]
                    .tabs
                    .iter()
                    .flat_map(|tab| tab.panes.iter())
                    .find(|(_, pane)| {
                        self.terminal_runtimes
                            .get(&pane.attached_terminal_id)
                            .and_then(|runtime| runtime.remote_terminal_id())
                            == Some(remote_terminal_id)
                    })
                    .map(|(pane_id, pane)| (ws_idx, *pane_id, pane.attached_terminal_id.clone()))
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
    /// whose single pane mirrors the serving host's `term_1`.
    fn mounted_app() -> (App, usize, crate::layout::PaneId) {
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
        mirror.apply_snapshot(&mounted_snapshot(), EventCursor(0));
        let mut router = TerminalChannelRouter::new();
        let (out_tx, _out_rx) = tokio::sync::mpsc::unbounded_channel();
        let (clipboard_tx, _clipboard_rx) = tokio::sync::mpsc::unbounded_channel();
        let created = app
            .materialize_federation_mount(&mirror, &mut router, &out_tx, &clipboard_tx)
            .expect("materialization succeeds against a loopback-shaped snapshot");
        let ws_idx = created[0];
        let pane_id = app.state.workspaces[ws_idx].tabs[0].root_pane;
        (app, ws_idx, pane_id)
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
        let revision = info(&app, ws_idx, pane_id).revision;
        let sequence = app.event_hub.current_sequence();

        app.handle_federation_pane_usage(
            origin(),
            "term_1".to_string(),
            Some(cache()),
            Some(context()),
        );

        let pane = info(&app, ws_idx, pane_id);
        assert_eq!(pane.prompt_cache, Some(cache()));
        assert_eq!(pane.context_usage, Some(context()));
        assert_eq!(pane.revision, revision + 1);
        assert_eq!(pane_updated_count(&app, sequence), 1);
    }

    #[tokio::test]
    async fn federation_pane_usage_with_none_clears_the_facts() {
        let (mut app, ws_idx, pane_id) = mounted_app();
        app.handle_federation_pane_usage(
            origin(),
            "term_1".to_string(),
            Some(cache()),
            Some(context()),
        );
        let revision = info(&app, ws_idx, pane_id).revision;
        let sequence = app.event_hub.current_sequence();

        app.handle_federation_pane_usage(origin(), "term_1".to_string(), None, None);

        let pane = info(&app, ws_idx, pane_id);
        assert_eq!(pane.prompt_cache, None);
        assert_eq!(pane.context_usage, None);
        assert_eq!(pane.revision, revision + 1);
        assert_eq!(pane_updated_count(&app, sequence), 1);
    }

    #[tokio::test]
    async fn federation_pane_usage_ignores_other_origins_and_unknown_terminals() {
        let (mut app, ws_idx, pane_id) = mounted_app();
        let revision = info(&app, ws_idx, pane_id).revision;
        let sequence = app.event_hub.current_sequence();

        app.handle_federation_pane_usage(
            HostKey::new("bob@10.0.0.2", "s1"),
            "term_1".to_string(),
            Some(cache()),
            Some(context()),
        );
        app.handle_federation_pane_usage(
            origin(),
            "term_404".to_string(),
            Some(cache()),
            Some(context()),
        );

        let pane = info(&app, ws_idx, pane_id);
        assert_eq!(pane.prompt_cache, None);
        assert_eq!(pane.context_usage, None);
        assert_eq!(pane.revision, revision);
        assert_eq!(pane_updated_count(&app, sequence), 0);
    }

    #[tokio::test]
    async fn federation_pane_usage_unchanged_value_does_not_bump_revision() {
        let (mut app, ws_idx, pane_id) = mounted_app();
        app.handle_federation_pane_usage(
            origin(),
            "term_1".to_string(),
            Some(cache()),
            Some(context()),
        );
        let revision = info(&app, ws_idx, pane_id).revision;
        let sequence = app.event_hub.current_sequence();

        app.handle_federation_pane_usage(
            origin(),
            "term_1".to_string(),
            Some(cache()),
            Some(context()),
        );

        assert_eq!(info(&app, ws_idx, pane_id).revision, revision);
        assert_eq!(pane_updated_count(&app, sequence), 0);
    }
}
