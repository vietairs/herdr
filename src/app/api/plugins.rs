use ratatui::layout::Direction;

use super::responses::{encode_error, encode_success};
use crate::api::schema::{
    PluginActionInfo, PluginActionInvokeParams, PluginActionListParams, PluginActionRegisterParams,
    PluginInvocationContext, PluginPaneCloseParams, PluginPaneFocusParams, PluginPaneInfo,
    PluginPaneOpenParams, PluginPanePlacement, PluginStorageDeleteParams, PluginStorageEntry,
    PluginStorageGetParams, PluginStorageListParams, PluginStorageScope, PluginStorageSetParams,
    ResponseResult,
};
use crate::app::App;

const PLUGIN_ID_MAX_CHARS: usize = 120;
const PLUGIN_ACTION_ID_MAX_CHARS: usize = 120;
const PLUGIN_STORAGE_KEY_MAX_CHARS: usize = 200;
const PLUGIN_STORAGE_VALUE_MAX_BYTES: usize = 256 * 1024;

type PluginStoragePrefix = (String, PluginStorageScope, Option<String>, Option<String>);

impl App {
    pub(super) fn handle_plugin_action_register(
        &mut self,
        id: String,
        params: PluginActionRegisterParams,
    ) -> String {
        let Some(plugin_id) = normalize_plugin_id(&params.plugin_id) else {
            return invalid_plugin_id(id);
        };
        let Some(action_id) = normalize_action_id(&params.action_id) else {
            return invalid_action_id(id);
        };
        let title = params.title.trim().to_string();
        if title.is_empty() {
            return encode_error(
                id,
                "invalid_plugin_action_title",
                "action title is required",
            );
        }

        let action = PluginActionInfo {
            plugin_id: plugin_id.clone(),
            action_id: action_id.clone(),
            title,
            description: params
                .description
                .map(|description| description.trim().to_string())
                .filter(|description| !description.is_empty()),
            contexts: params.contexts,
            entrypoint: params
                .entrypoint
                .map(|entrypoint| entrypoint.trim().to_string())
                .filter(|entrypoint| !entrypoint.is_empty()),
        };
        self.state
            .plugin_actions
            .insert(plugin_action_key(&plugin_id, &action_id), action.clone());
        self.state.mark_session_dirty();
        encode_success(id, ResponseResult::PluginActionRegistered { action })
    }

    pub(super) fn handle_plugin_action_list(
        &mut self,
        id: String,
        params: PluginActionListParams,
    ) -> String {
        let plugin_id = match params.plugin_id {
            Some(plugin_id) => {
                let Some(plugin_id) = normalize_plugin_id(&plugin_id) else {
                    return invalid_plugin_id(id);
                };
                Some(plugin_id)
            }
            None => None,
        };
        let mut actions = self
            .state
            .plugin_actions
            .values()
            .filter(|action| {
                plugin_id
                    .as_deref()
                    .is_none_or(|plugin_id| action.plugin_id == plugin_id)
            })
            .cloned()
            .collect::<Vec<_>>();
        actions.sort_by_key(|action| action.qualified_id());
        encode_success(id, ResponseResult::PluginActionList { actions })
    }

    pub(super) fn handle_plugin_action_invoke(
        &mut self,
        id: String,
        params: PluginActionInvokeParams,
    ) -> String {
        let action = match self.find_plugin_action(params.plugin_id.as_deref(), &params.action_id) {
            Ok(action) => action,
            Err((code, message)) => return encode_error(id, code, message),
        };
        let context = self.merge_plugin_context(params.context, &id);
        encode_success(id, ResponseResult::PluginActionInvoked { action, context })
    }

    pub(super) fn handle_plugin_storage_get(
        &mut self,
        id: String,
        params: PluginStorageGetParams,
    ) -> String {
        let key = match plugin_storage_key(
            &params.plugin_id,
            params.scope,
            params.workspace_id.as_deref(),
            params.project_id.as_deref(),
            &params.key,
        ) {
            Ok(key) => key,
            Err((code, message)) => return encode_error(id, code, message),
        };
        let entry = self
            .state
            .plugin_storage
            .get(&key)
            .cloned()
            .map(|value| storage_entry(&key, value));
        encode_success(id, ResponseResult::PluginStorageValue { entry })
    }

    pub(super) fn handle_plugin_storage_set(
        &mut self,
        id: String,
        params: PluginStorageSetParams,
    ) -> String {
        let serialized_len = match serde_json::to_vec(&params.value) {
            Ok(bytes) => bytes.len(),
            Err(err) => return encode_error(id, "invalid_plugin_storage_value", err.to_string()),
        };
        if serialized_len > PLUGIN_STORAGE_VALUE_MAX_BYTES {
            return encode_error(
                id,
                "plugin_storage_value_too_large",
                format!("storage value must be <= {PLUGIN_STORAGE_VALUE_MAX_BYTES} bytes"),
            );
        }
        let key = match plugin_storage_key(
            &params.plugin_id,
            params.scope,
            params.workspace_id.as_deref(),
            params.project_id.as_deref(),
            &params.key,
        ) {
            Ok(key) => key,
            Err((code, message)) => return encode_error(id, code, message),
        };
        self.state
            .plugin_storage
            .insert(key.clone(), params.value.clone());
        self.state.mark_session_dirty();
        encode_success(
            id,
            ResponseResult::PluginStorageSet {
                entry: storage_entry(&key, params.value),
            },
        )
    }

    pub(super) fn handle_plugin_storage_delete(
        &mut self,
        id: String,
        params: PluginStorageDeleteParams,
    ) -> String {
        let key = match plugin_storage_key(
            &params.plugin_id,
            params.scope,
            params.workspace_id.as_deref(),
            params.project_id.as_deref(),
            &params.key,
        ) {
            Ok(key) => key,
            Err((code, message)) => return encode_error(id, code, message),
        };
        let deleted = self.state.plugin_storage.remove(&key).is_some();
        if deleted {
            self.state.mark_session_dirty();
        }
        encode_success(id, ResponseResult::PluginStorageDeleted { deleted })
    }

    pub(super) fn handle_plugin_storage_list(
        &mut self,
        id: String,
        params: PluginStorageListParams,
    ) -> String {
        let prefix = match plugin_storage_prefix(
            &params.plugin_id,
            params.scope,
            params.workspace_id.as_deref(),
            params.project_id.as_deref(),
        ) {
            Ok(prefix) => prefix,
            Err((code, message)) => return encode_error(id, code, message),
        };
        let mut entries = self
            .state
            .plugin_storage
            .iter()
            .filter(|(key, _)| storage_key_matches_prefix(key, &prefix))
            .map(|(key, value)| storage_entry(key, value.clone()))
            .collect::<Vec<_>>();
        entries.sort_by(|a, b| a.key.cmp(&b.key));
        encode_success(id, ResponseResult::PluginStorageList { entries })
    }

    pub(super) fn handle_plugin_pane_open(
        &mut self,
        id: String,
        params: PluginPaneOpenParams,
    ) -> String {
        let Some(plugin_id) = normalize_plugin_id(&params.plugin_id) else {
            return invalid_plugin_id(id);
        };
        let entrypoint = params.entrypoint.trim().to_string();
        if entrypoint.is_empty() {
            return encode_error(id, "invalid_plugin_entrypoint", "entrypoint is required");
        }
        if params.argv.is_empty() {
            return encode_error(id, "invalid_plugin_argv", "argv must not be empty");
        }

        match params.placement {
            PluginPanePlacement::Split | PluginPanePlacement::Zoomed => {
                self.open_plugin_split_pane(id, params, plugin_id, entrypoint)
            }
            PluginPanePlacement::Tab => self.open_plugin_tab(id, params, plugin_id, entrypoint),
        }
    }

    pub(super) fn handle_plugin_pane_focus(
        &mut self,
        id: String,
        params: PluginPaneFocusParams,
    ) -> String {
        let Some((ws_idx, pane_id)) = self.parse_pane_id(&params.pane_id) else {
            return encode_error(id, "plugin_pane_not_found", "plugin pane not found");
        };
        if !self.state.plugin_panes.contains_key(&pane_id) {
            return encode_error(id, "plugin_pane_not_found", "plugin pane not found");
        }
        self.state.focus_pane_in_workspace(ws_idx, pane_id);
        self.state.mode = crate::app::Mode::Terminal;
        let Some(record) = self.state.plugin_panes.get(&pane_id).cloned() else {
            return encode_error(id, "plugin_pane_not_found", "plugin pane not found");
        };
        let Some(pane) = self.pane_info(ws_idx, pane_id) else {
            return encode_error(id, "plugin_pane_not_found", "plugin pane not found");
        };
        encode_success(
            id,
            ResponseResult::PluginPaneOpened {
                plugin_pane: PluginPaneInfo {
                    plugin_id: record.plugin_id,
                    entrypoint: record.entrypoint,
                    pane,
                },
            },
        )
    }

    pub(super) fn handle_plugin_pane_close(
        &mut self,
        id: String,
        params: PluginPaneCloseParams,
    ) -> String {
        let Some((_ws_idx, pane_id)) = self.parse_pane_id(&params.pane_id) else {
            return encode_error(id, "plugin_pane_not_found", "plugin pane not found");
        };
        if !self.state.plugin_panes.contains_key(&pane_id) {
            return encode_error(id, "plugin_pane_not_found", "plugin pane not found");
        }
        self.handle_pane_close(
            id,
            crate::api::schema::PaneTarget {
                pane_id: params.pane_id,
            },
        )
    }

    fn open_plugin_split_pane(
        &mut self,
        id: String,
        params: PluginPaneOpenParams,
        plugin_id: String,
        entrypoint: String,
    ) -> String {
        let target_pane_id = params
            .target_pane_id
            .clone()
            .or_else(|| self.current_public_pane_id());
        let Some(target_pane_id) = target_pane_id else {
            return encode_error(id, "no_active_pane", "no active pane");
        };
        let Some((ws_idx, target_pane)) = self.parse_pane_id(&target_pane_id) else {
            return encode_error(
                id,
                "pane_not_found",
                format!("pane {target_pane_id} not found"),
            );
        };
        let direction = match params
            .direction
            .unwrap_or(crate::api::schema::SplitDirection::Right)
        {
            crate::api::schema::SplitDirection::Right => Direction::Horizontal,
            crate::api::schema::SplitDirection::Down => Direction::Vertical,
        };
        let cwd = params
            .cwd
            .map(std::path::PathBuf::from)
            .or_else(|| self.cwd_for_pane(ws_idx, target_pane));
        let (rows, cols) = self.state.estimate_pane_size();
        let previous_focus = self.state.current_pane_focus_target();
        let Some(ws) = self.state.workspaces.get_mut(ws_idx) else {
            return encode_error(id, "workspace_not_found", "workspace not found");
        };
        let result = ws.split_pane_argv_command(
            target_pane,
            direction,
            rows.max(4),
            cols.max(10),
            cwd,
            &params.argv,
            self.state.pane_scrollback_limit_bytes,
            self.state.host_terminal_theme,
            params.focus || params.placement == PluginPanePlacement::Zoomed,
        );
        let (tab_idx, new_pane) = match result {
            Some(Ok(result)) => result,
            Some(Err(err)) => return encode_error(id, "plugin_pane_open_failed", err.to_string()),
            None => {
                return encode_error(
                    id,
                    "pane_not_found",
                    format!("pane {target_pane_id} not found"),
                )
            }
        };
        if params.focus || params.placement == PluginPanePlacement::Zoomed {
            self.state.switch_workspace_tab(ws_idx, tab_idx);
            self.state
                .record_pane_focus_change(previous_focus, ws_idx, new_pane.pane_id);
            self.state.mode = crate::app::Mode::Terminal;
        }
        if params.placement == PluginPanePlacement::Zoomed {
            if let Some(tab) = self
                .state
                .workspaces
                .get_mut(ws_idx)
                .and_then(|ws| ws.tabs.get_mut(tab_idx))
            {
                tab.zoomed = true;
            }
        }
        self.finish_plugin_pane_open(id, ws_idx, new_pane, plugin_id, entrypoint)
    }

    fn open_plugin_tab(
        &mut self,
        id: String,
        params: PluginPaneOpenParams,
        plugin_id: String,
        entrypoint: String,
    ) -> String {
        let ws_idx = match params.workspace_id.as_deref() {
            Some(workspace_id) => match self.parse_workspace_id(workspace_id) {
                Some(ws_idx) => ws_idx,
                None => return encode_error(id, "workspace_not_found", "workspace not found"),
            },
            None => match self.state.active {
                Some(ws_idx) => ws_idx,
                None => return encode_error(id, "no_active_workspace", "no active workspace"),
            },
        };
        let cwd = params
            .cwd
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| self.default_cwd_for_workspace(ws_idx));
        let (rows, cols) = self.state.estimate_pane_size();
        let Some(ws) = self.state.workspaces.get_mut(ws_idx) else {
            return encode_error(id, "workspace_not_found", "workspace not found");
        };
        let (tab_idx, terminal, runtime) = match ws.create_tab_argv_command(
            rows.max(4),
            cols.max(10),
            cwd,
            &params.argv,
            self.state.pane_scrollback_limit_bytes,
            self.state.host_terminal_theme,
        ) {
            Ok(result) => result,
            Err(err) => return encode_error(id, "plugin_pane_open_failed", err.to_string()),
        };
        let pane_id = ws.tabs[tab_idx].root_pane;
        if params.focus {
            self.state.switch_workspace_tab(ws_idx, tab_idx);
            self.state.mode = crate::app::Mode::Terminal;
        }
        let new_pane = crate::workspace::NewPane {
            pane_id,
            terminal,
            runtime,
        };
        self.finish_plugin_pane_open(id, ws_idx, new_pane, plugin_id, entrypoint)
    }

    fn finish_plugin_pane_open(
        &mut self,
        id: String,
        ws_idx: usize,
        new_pane: crate::workspace::NewPane,
        plugin_id: String,
        entrypoint: String,
    ) -> String {
        self.terminal_runtimes
            .insert(new_pane.terminal.id.clone(), new_pane.runtime);
        self.state
            .remove_alias_shadowed_by_new_pane(new_pane.pane_id);
        self.state
            .terminals
            .insert(new_pane.terminal.id.clone(), new_pane.terminal);
        self.state.plugin_panes.insert(
            new_pane.pane_id,
            crate::app::state::PluginPaneRecord {
                plugin_id: plugin_id.clone(),
                entrypoint: entrypoint.clone(),
            },
        );
        self.schedule_session_save();
        let Some(pane) = self.pane_info(ws_idx, new_pane.pane_id) else {
            return encode_error(id, "plugin_pane_open_failed", "plugin pane disappeared");
        };
        self.emit_event(crate::api::schema::EventEnvelope {
            event: crate::api::schema::EventKind::PaneCreated,
            data: crate::api::schema::EventData::PaneCreated { pane: pane.clone() },
        });
        encode_success(
            id,
            ResponseResult::PluginPaneOpened {
                plugin_pane: PluginPaneInfo {
                    plugin_id,
                    entrypoint,
                    pane,
                },
            },
        )
    }

    fn find_plugin_action(
        &self,
        plugin_id: Option<&str>,
        action_id: &str,
    ) -> Result<PluginActionInfo, (&'static str, String)> {
        if let Some(plugin_id) = plugin_id {
            let plugin_id = normalize_plugin_id(plugin_id)
                .ok_or_else(|| ("invalid_plugin_id", "invalid plugin id".to_string()))?;
            let action_id = normalize_action_id(action_id)
                .ok_or_else(|| ("invalid_plugin_action_id", "invalid action id".to_string()))?;
            return self
                .state
                .plugin_actions
                .get(&plugin_action_key(&plugin_id, &action_id))
                .cloned()
                .ok_or_else(|| {
                    (
                        "plugin_action_not_found",
                        "plugin action not found".to_string(),
                    )
                });
        }

        let action_id = action_id.trim();
        let matches = self
            .state
            .plugin_actions
            .values()
            .filter(|action| action.action_id == action_id || action.qualified_id() == action_id)
            .cloned()
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [action] => Ok(action.clone()),
            [] => Err((
                "plugin_action_not_found",
                "plugin action not found".to_string(),
            )),
            _ => Err((
                "ambiguous_plugin_action",
                "plugin action id matches more than one action; include plugin_id".to_string(),
            )),
        }
    }

    fn merge_plugin_context(
        &self,
        provided: Option<PluginInvocationContext>,
        correlation_id: &str,
    ) -> PluginInvocationContext {
        let mut context = self.current_plugin_context(correlation_id);
        if let Some(provided) = provided {
            context.workspace_id = provided.workspace_id.or(context.workspace_id);
            context.workspace_label = provided.workspace_label.or(context.workspace_label);
            context.workspace_cwd = provided.workspace_cwd.or(context.workspace_cwd);
            context.worktree = provided.worktree.or(context.worktree);
            context.tab_id = provided.tab_id.or(context.tab_id);
            context.tab_label = provided.tab_label.or(context.tab_label);
            context.focused_pane_id = provided.focused_pane_id.or(context.focused_pane_id);
            context.focused_pane_cwd = provided.focused_pane_cwd.or(context.focused_pane_cwd);
            context.focused_pane_agent = provided.focused_pane_agent.or(context.focused_pane_agent);
            context.focused_pane_status =
                provided.focused_pane_status.or(context.focused_pane_status);
            context.selected_text = provided.selected_text.or(context.selected_text);
            context.invocation_source = provided.invocation_source.or(context.invocation_source);
            context.correlation_id = provided.correlation_id.or(context.correlation_id);
        }
        context
    }

    fn current_plugin_context(&self, correlation_id: &str) -> PluginInvocationContext {
        let Some(ws_idx) = self.state.active else {
            return PluginInvocationContext {
                workspace_id: None,
                workspace_label: None,
                workspace_cwd: None,
                worktree: None,
                tab_id: None,
                tab_label: None,
                focused_pane_id: None,
                focused_pane_cwd: None,
                focused_pane_agent: None,
                focused_pane_status: None,
                selected_text: None,
                invocation_source: Some("api".to_string()),
                correlation_id: Some(correlation_id.to_string()),
            };
        };
        let ws = &self.state.workspaces[ws_idx];
        let workspace = self.workspace_info(ws_idx);
        let tab_idx = ws.active_tab_index();
        let tab_id = self.public_tab_id(ws_idx, tab_idx);
        let tab_label = ws.tabs.get(tab_idx).map(|tab| tab.display_name());
        let focused_pane = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|ws| ws.focused_pane_id())
            .and_then(|pane_id| self.pane_info(ws_idx, pane_id));
        let workspace_cwd = focused_pane
            .as_ref()
            .and_then(|pane| pane.cwd.clone())
            .or_else(|| Some(self.default_cwd_for_workspace(ws_idx).display().to_string()));
        PluginInvocationContext {
            workspace_id: Some(workspace.workspace_id),
            workspace_label: Some(workspace.label),
            workspace_cwd,
            worktree: workspace.worktree,
            tab_id,
            tab_label,
            focused_pane_id: focused_pane.as_ref().map(|pane| pane.pane_id.clone()),
            focused_pane_cwd: focused_pane.as_ref().and_then(|pane| pane.cwd.clone()),
            focused_pane_agent: focused_pane.as_ref().and_then(|pane| pane.agent.clone()),
            focused_pane_status: focused_pane.as_ref().map(|pane| pane.agent_status),
            selected_text: None,
            invocation_source: Some("api".to_string()),
            correlation_id: Some(correlation_id.to_string()),
        }
    }

    fn current_public_pane_id(&self) -> Option<String> {
        let ws_idx = self.state.active?;
        let pane_id = self.state.workspaces.get(ws_idx)?.focused_pane_id()?;
        self.public_pane_id(ws_idx, pane_id)
    }

    fn cwd_for_pane(
        &self,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
    ) -> Option<std::path::PathBuf> {
        let ws = self.state.workspaces.get(ws_idx)?;
        let tab_idx = ws.find_tab_index_for_pane(pane_id)?;
        ws.tabs
            .get(tab_idx)?
            .cwd_for_pane(pane_id, &self.state.terminals, &self.terminal_runtimes)
    }

    fn default_cwd_for_workspace(&self, ws_idx: usize) -> std::path::PathBuf {
        self.state
            .workspaces
            .get(ws_idx)
            .and_then(|ws| {
                ws.resolved_identity_cwd_from(&self.state.terminals, &self.terminal_runtimes)
            })
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| "/".into()))
    }
}

fn normalize_plugin_id(value: &str) -> Option<String> {
    normalize_identifier(value, PLUGIN_ID_MAX_CHARS)
}

fn normalize_action_id(value: &str) -> Option<String> {
    normalize_identifier(value, PLUGIN_ACTION_ID_MAX_CHARS)
}

fn normalize_storage_key(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty() && value.chars().count() <= PLUGIN_STORAGE_KEY_MAX_CHARS)
        .then(|| value.to_string())
}

fn normalize_identifier(value: &str, max_chars: usize) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()
        && value.chars().count() <= max_chars
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b':' | b'.' | b'_' | b'-')))
    .then(|| value.to_string())
}

fn invalid_plugin_id(id: String) -> String {
    encode_error(
        id,
        "invalid_plugin_id",
        "plugin id must be non-empty, <= 120 characters, and contain only ASCII letters, digits, colon, dot, underscore, or hyphen",
    )
}

fn invalid_action_id(id: String) -> String {
    encode_error(
        id,
        "invalid_plugin_action_id",
        "action id must be non-empty, <= 120 characters, and contain only ASCII letters, digits, colon, dot, underscore, or hyphen",
    )
}

fn plugin_action_key(plugin_id: &str, action_id: &str) -> String {
    format!("{plugin_id}.{action_id}")
}

fn plugin_storage_key(
    plugin_id: &str,
    scope: PluginStorageScope,
    workspace_id: Option<&str>,
    project_id: Option<&str>,
    key: &str,
) -> Result<crate::app::state::PluginStorageKey, (&'static str, String)> {
    let (plugin_id, scope, workspace_id, project_id) =
        plugin_storage_prefix(plugin_id, scope, workspace_id, project_id)?;
    let key = normalize_storage_key(key).ok_or_else(|| {
        (
            "invalid_plugin_storage_key",
            "invalid storage key".to_string(),
        )
    })?;
    Ok(crate::app::state::PluginStorageKey {
        plugin_id,
        scope,
        workspace_id,
        project_id,
        key,
    })
}

fn plugin_storage_prefix(
    plugin_id: &str,
    scope: PluginStorageScope,
    workspace_id: Option<&str>,
    project_id: Option<&str>,
) -> Result<PluginStoragePrefix, (&'static str, String)> {
    let plugin_id = normalize_plugin_id(plugin_id)
        .ok_or_else(|| ("invalid_plugin_id", "invalid plugin id".to_string()))?;
    let workspace_id = workspace_id
        .map(|workspace_id| {
            normalize_identifier(workspace_id, 120)
                .ok_or_else(|| ("invalid_workspace_id", "invalid workspace id".to_string()))
        })
        .transpose()?;
    let project_id = project_id
        .map(|project_id| {
            normalize_identifier(project_id, 200)
                .ok_or_else(|| ("invalid_project_id", "invalid project id".to_string()))
        })
        .transpose()?;
    if scope == PluginStorageScope::Workspace && workspace_id.is_none() {
        return Err((
            "missing_workspace_id",
            "workspace storage requires workspace_id".to_string(),
        ));
    }
    if scope == PluginStorageScope::Project && project_id.is_none() {
        return Err((
            "missing_project_id",
            "project storage requires project_id".to_string(),
        ));
    }
    Ok((plugin_id, scope, workspace_id, project_id))
}

fn storage_key_matches_prefix(
    key: &crate::app::state::PluginStorageKey,
    prefix: &(String, PluginStorageScope, Option<String>, Option<String>),
) -> bool {
    key.plugin_id == prefix.0
        && key.scope == prefix.1
        && key.workspace_id == prefix.2
        && key.project_id == prefix.3
}

fn storage_entry(
    key: &crate::app::state::PluginStorageKey,
    value: serde_json::Value,
) -> PluginStorageEntry {
    PluginStorageEntry {
        plugin_id: key.plugin_id.clone(),
        scope: key.scope,
        key: key.key.clone(),
        value,
        workspace_id: key.workspace_id.clone(),
        project_id: key.project_id.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::{Method, PluginActionContext, Request, SuccessResponse};

    fn test_app() -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        App::new(
            &crate::config::Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        )
    }

    fn response_result(response: &str) -> ResponseResult {
        serde_json::from_str::<SuccessResponse>(response)
            .expect("success response")
            .result
    }

    #[test]
    fn plugin_action_registers_and_invokes_with_context() {
        let mut app = test_app();
        let register = app.handle_api_request(Request {
            id: "register".into(),
            method: Method::PluginActionRegister(PluginActionRegisterParams {
                plugin_id: "example.issue-flow".into(),
                action_id: "assign-issue".into(),
                title: "Assign Issue".into(),
                description: None,
                contexts: vec![PluginActionContext::Workspace],
                entrypoint: Some("assign".into()),
            }),
        });
        assert!(matches!(
            response_result(&register),
            ResponseResult::PluginActionRegistered { .. }
        ));

        let invoke = app.handle_api_request(Request {
            id: "invoke".into(),
            method: Method::PluginActionInvoke(PluginActionInvokeParams {
                plugin_id: Some("example.issue-flow".into()),
                action_id: "assign-issue".into(),
                context: Some(PluginInvocationContext {
                    workspace_id: Some("1".into()),
                    workspace_label: None,
                    workspace_cwd: None,
                    worktree: None,
                    tab_id: None,
                    tab_label: None,
                    focused_pane_id: None,
                    focused_pane_cwd: None,
                    focused_pane_agent: None,
                    focused_pane_status: None,
                    selected_text: None,
                    invocation_source: Some("test".into()),
                    correlation_id: Some("external-correlation".into()),
                }),
            }),
        });
        let ResponseResult::PluginActionInvoked { action, context } = response_result(&invoke)
        else {
            panic!("expected plugin action invocation");
        };
        assert_eq!(action.qualified_id(), "example.issue-flow.assign-issue");
        assert_eq!(context.workspace_id.as_deref(), Some("1"));
        assert_eq!(context.invocation_source.as_deref(), Some("test"));
        assert_eq!(
            context.correlation_id.as_deref(),
            Some("external-correlation")
        );
    }

    #[test]
    fn plugin_action_invoke_builds_default_workspace_tab_and_pane_context() {
        let mut app = test_app();
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("issue")];
        app.state.workspaces[0].identity_cwd = "/tmp/issue".into();
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.workspaces[0].custom_name = Some("Plugin Work".into());
        app.state.workspaces[0].worktree_space = Some(crate::workspace::WorktreeSpaceMembership {
            key: "repo-key".into(),
            label: "herdr".into(),
            repo_root: "/repo/herdr".into(),
            checkout_path: "/repo/herdr-issue".into(),
            is_linked_worktree: true,
        });
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let pane_public = app.public_pane_id(0, pane_id).unwrap();
        let tab_public = app.public_tab_id(0, 0).unwrap();
        let workspace_public = app.public_workspace_id(0);
        let _ = app.handle_pane_report_agent(
            "report".into(),
            crate::api::schema::PaneReportAgentParams {
                pane_id: pane_public.clone(),
                source: "test".into(),
                agent: "codex".into(),
                state: crate::api::schema::PaneAgentState::Working,
                message: None,
                custom_status: None,
                seq: None,
                agent_session_id: None,
                agent_session_path: None,
            },
        );

        let _ = app.handle_api_request(Request {
            id: "register".into(),
            method: Method::PluginActionRegister(PluginActionRegisterParams {
                plugin_id: "example.context".into(),
                action_id: "show".into(),
                title: "Show Context".into(),
                description: None,
                contexts: vec![PluginActionContext::Pane],
                entrypoint: None,
            }),
        });

        let invoke = app.handle_api_request(Request {
            id: "invoke-context".into(),
            method: Method::PluginActionInvoke(PluginActionInvokeParams {
                plugin_id: Some("example.context".into()),
                action_id: "show".into(),
                context: None,
            }),
        });

        let ResponseResult::PluginActionInvoked { context, .. } = response_result(&invoke) else {
            panic!("expected plugin action invocation");
        };
        assert_eq!(
            context.workspace_id.as_deref(),
            Some(workspace_public.as_str())
        );
        assert_eq!(context.workspace_label.as_deref(), Some("Plugin Work"));
        assert_eq!(context.workspace_cwd.as_deref(), Some("/tmp/issue"));
        assert_eq!(context.tab_id.as_deref(), Some(tab_public.as_str()));
        assert_eq!(context.tab_label.as_deref(), Some("1"));
        assert_eq!(
            context.focused_pane_id.as_deref(),
            Some(pane_public.as_str())
        );
        assert_eq!(context.focused_pane_cwd.as_deref(), Some("/tmp/issue"));
        assert_eq!(context.focused_pane_agent.as_deref(), Some("codex"));
        assert_eq!(
            context.focused_pane_status,
            Some(crate::api::schema::AgentStatus::Working)
        );
        assert_eq!(context.invocation_source.as_deref(), Some("api"));
        assert_eq!(context.correlation_id.as_deref(), Some("invoke-context"));
        let worktree = context.worktree.as_ref().unwrap();
        assert_eq!(worktree.repo_key, "repo-key");
        assert_eq!(worktree.repo_name, "herdr");
        assert_eq!(worktree.repo_root, "/repo/herdr");
        assert_eq!(worktree.checkout_path, "/repo/herdr-issue");
        assert!(worktree.is_linked_worktree);
    }

    #[test]
    fn plugin_storage_is_scoped_and_listable() {
        let mut app = test_app();
        let set = app.handle_api_request(Request {
            id: "set".into(),
            method: Method::PluginStorageSet(PluginStorageSetParams {
                plugin_id: "example.notes".into(),
                scope: PluginStorageScope::Workspace,
                key: "pins".into(),
                value: serde_json::json!(["a", "b"]),
                workspace_id: Some("1".into()),
                project_id: None,
            }),
        });
        assert!(matches!(
            response_result(&set),
            ResponseResult::PluginStorageSet { .. }
        ));

        let list = app.handle_api_request(Request {
            id: "list".into(),
            method: Method::PluginStorageList(PluginStorageListParams {
                plugin_id: "example.notes".into(),
                scope: PluginStorageScope::Workspace,
                workspace_id: Some("1".into()),
                project_id: None,
            }),
        });
        let ResponseResult::PluginStorageList { entries } = response_result(&list) else {
            panic!("expected storage list");
        };
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].key, "pins");
        assert_eq!(entries[0].value, serde_json::json!(["a", "b"]));
    }
}
