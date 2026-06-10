use ratatui::layout::Direction;

use super::responses::{encode_error, encode_success};
use crate::api::schema::{
    InstalledPluginInfo, PluginActionInfo, PluginActionInvokeParams, PluginActionListParams,
    PluginInvocationContext, PluginLinkParams, PluginListParams, PluginManifestAction,
    PluginManifestEventHook, PluginPaneCloseParams, PluginPaneFocusParams, PluginPaneInfo,
    PluginPaneOpenParams, PluginPanePlacement, PluginStorageDeleteParams, PluginStorageEntry,
    PluginStorageGetParams, PluginStorageListParams, PluginStorageScope, PluginStorageSetParams,
    PluginUnlinkParams, ResponseResult,
};
use crate::app::App;

const PLUGIN_ID_MAX_CHARS: usize = 120;
const PLUGIN_ACTION_ID_MAX_CHARS: usize = 120;
const PLUGIN_STORAGE_KEY_MAX_CHARS: usize = 200;
const PLUGIN_STORAGE_VALUE_MAX_BYTES: usize = 256 * 1024;

type PluginStoragePrefix = (String, PluginStorageScope, Option<String>, Option<String>);

impl App {
    pub(super) fn handle_plugin_link(&mut self, id: String, params: PluginLinkParams) -> String {
        let plugin = match load_plugin_manifest(&params.path, params.enabled) {
            Ok(plugin) => plugin,
            Err((code, message)) => return encode_error(id, code, message),
        };
        self.state
            .installed_plugins
            .insert(plugin.plugin_id.clone(), plugin.clone());
        self.state.mark_session_dirty();
        encode_success(id, ResponseResult::PluginLinked { plugin })
    }

    pub(super) fn handle_plugin_list(&mut self, id: String, params: PluginListParams) -> String {
        let plugin_id = match params.plugin_id {
            Some(plugin_id) => {
                let Some(plugin_id) = normalize_plugin_id(&plugin_id) else {
                    return invalid_plugin_id(id);
                };
                Some(plugin_id)
            }
            None => None,
        };
        let mut plugins = self
            .state
            .installed_plugins
            .values()
            .filter(|plugin| {
                plugin_id
                    .as_deref()
                    .is_none_or(|plugin_id| plugin.plugin_id == plugin_id)
            })
            .cloned()
            .collect::<Vec<_>>();
        plugins.sort_by(|a, b| a.plugin_id.cmp(&b.plugin_id));
        encode_success(id, ResponseResult::PluginList { plugins })
    }

    pub(super) fn handle_plugin_unlink(
        &mut self,
        id: String,
        params: PluginUnlinkParams,
    ) -> String {
        let Some(plugin_id) = normalize_plugin_id(&params.plugin_id) else {
            return invalid_plugin_id(id);
        };
        let removed = self.state.installed_plugins.remove(&plugin_id).is_some();
        if removed {
            self.state.mark_session_dirty();
        }
        encode_success(id, ResponseResult::PluginUnlinked { plugin_id, removed })
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
        let mut actions = manifest_actions(&self.state.installed_plugins)
            .filter(|action| {
                plugin_id
                    .as_deref()
                    .is_none_or(|plugin_id| action.plugin_id == plugin_id)
            })
            .collect::<Vec<_>>();
        actions.sort_by_key(|action| action.qualified_id());
        encode_success(id, ResponseResult::PluginActionList { actions })
    }

    pub(super) fn handle_plugin_action_invoke(
        &mut self,
        id: String,
        params: PluginActionInvokeParams,
    ) -> String {
        let (plugin, action) =
            match self.find_plugin_action(params.plugin_id.as_deref(), &params.action_id) {
                Ok(pair) => pair,
                Err((code, message)) => return encode_error(id, code, message),
            };
        if !plugin.enabled {
            return encode_error(
                id,
                "plugin_disabled",
                format!("plugin {} is disabled", plugin.plugin_id),
            );
        }
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
        let extra_env = match super::env::normalize_launch_env(params.env.clone()) {
            Ok(env) => env,
            Err((code, message)) => return encode_error(id, &code, message),
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
            extra_env,
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
        let extra_env = match super::env::normalize_launch_env(params.env.clone()) {
            Ok(env) => env,
            Err((code, message)) => return encode_error(id, &code, message),
        };
        let (rows, cols) = self.state.estimate_pane_size();
        let Some(ws) = self.state.workspaces.get_mut(ws_idx) else {
            return encode_error(id, "workspace_not_found", "workspace not found");
        };
        let (tab_idx, terminal, runtime) = match ws.create_tab_argv_command(
            rows.max(4),
            cols.max(10),
            cwd,
            &params.argv,
            extra_env,
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
    ) -> Result<(crate::api::schema::InstalledPluginInfo, PluginActionInfo), (&'static str, String)>
    {
        if let Some(plugin_id) = plugin_id {
            let plugin_id = normalize_plugin_id(plugin_id)
                .ok_or_else(|| ("invalid_plugin_id", "invalid plugin id".to_string()))?;
            let action_id = normalize_action_id(action_id)
                .ok_or_else(|| ("invalid_plugin_action_id", "invalid action id".to_string()))?;
            let plugin = self
                .state
                .installed_plugins
                .get(&plugin_id)
                .ok_or_else(|| ("plugin_not_found", "plugin not found".to_string()))?
                .clone();
            let action_info = plugin
                .actions
                .iter()
                .find(|a| a.id == action_id)
                .map(|a| manifest_action_info(&plugin_id, a))
                .ok_or_else(|| {
                    (
                        "plugin_action_not_found",
                        "plugin action not found".to_string(),
                    )
                })?;
            return Ok((plugin, action_info));
        }

        let action_id = action_id.trim();
        let matches = manifest_actions(&self.state.installed_plugins)
            .filter(|action| action.action_id == action_id || action.qualified_id() == action_id)
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [action] => {
                let plugin = self
                    .state
                    .installed_plugins
                    .get(&action.plugin_id)
                    .cloned()
                    .ok_or_else(|| ("plugin_not_found", "plugin not found".to_string()))?;
                Ok((plugin, action.clone()))
            }
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

#[derive(serde::Deserialize)]
struct RawPluginManifest {
    id: String,
    name: String,
    version: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    actions: Vec<RawPluginManifestAction>,
    #[serde(default)]
    events: Vec<RawPluginManifestEventHook>,
}

#[derive(serde::Deserialize)]
struct RawPluginManifestAction {
    id: String,
    title: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    contexts: Vec<crate::api::schema::PluginActionContext>,
    command: Vec<String>,
}

#[derive(serde::Deserialize)]
struct RawPluginManifestEventHook {
    on: String,
    command: Vec<String>,
}

fn load_plugin_manifest(
    path: &str,
    enabled: bool,
) -> Result<InstalledPluginInfo, (&'static str, String)> {
    let path = std::path::PathBuf::from(path);
    let manifest_path = if path.is_dir() {
        path.join("herdr-plugin.toml")
    } else {
        path
    };
    let manifest_path = manifest_path
        .canonicalize()
        .map_err(|err| ("plugin_manifest_not_found", err.to_string()))?;
    let plugin_root = manifest_path
        .parent()
        .ok_or_else(|| {
            (
                "invalid_plugin_manifest_path",
                "manifest path has no parent directory".to_string(),
            )
        })?
        .to_path_buf();
    let content = std::fs::read_to_string(&manifest_path)
        .map_err(|err| ("plugin_manifest_read_failed", err.to_string()))?;
    let raw: RawPluginManifest = toml::from_str(&content)
        .map_err(|err| ("plugin_manifest_parse_failed", err.to_string()))?;
    let plugin_id = normalize_plugin_id(&raw.id)
        .ok_or_else(|| ("invalid_plugin_id", "invalid plugin id".to_string()))?;
    let name = non_empty_trimmed(&raw.name, "invalid_plugin_name", "plugin name is required")?;
    let version = non_empty_trimmed(
        &raw.version,
        "invalid_plugin_version",
        "plugin version is required",
    )?;
    let description = raw
        .description
        .map(|description| description.trim().to_string())
        .filter(|description| !description.is_empty());
    let mut actions = raw
        .actions
        .into_iter()
        .map(normalize_manifest_action)
        .collect::<Result<Vec<_>, _>>()?;
    actions.sort_by(|a, b| a.id.cmp(&b.id));
    let mut events = raw
        .events
        .into_iter()
        .map(normalize_manifest_event)
        .collect::<Result<Vec<_>, _>>()?;
    events.sort_by(|a, b| a.on.cmp(&b.on).then_with(|| a.command.cmp(&b.command)));

    Ok(InstalledPluginInfo {
        plugin_id,
        name,
        version,
        description,
        manifest_path: manifest_path.display().to_string(),
        plugin_root: plugin_root.display().to_string(),
        enabled,
        actions,
        events,
    })
}

fn normalize_manifest_action(
    action: RawPluginManifestAction,
) -> Result<PluginManifestAction, (&'static str, String)> {
    let id = normalize_action_id(&action.id)
        .ok_or_else(|| ("invalid_plugin_action_id", "invalid action id".to_string()))?;
    let title = non_empty_trimmed(
        &action.title,
        "invalid_plugin_action_title",
        "action title is required",
    )?;
    let description = action
        .description
        .map(|description| description.trim().to_string())
        .filter(|description| !description.is_empty());
    let command = normalize_command(action.command)?;
    Ok(PluginManifestAction {
        id,
        title,
        description,
        contexts: action.contexts,
        command,
    })
}

fn normalize_manifest_event(
    event: RawPluginManifestEventHook,
) -> Result<PluginManifestEventHook, (&'static str, String)> {
    let on = non_empty_trimmed(&event.on, "invalid_plugin_event", "event name is required")?;
    let command = normalize_command(event.command)?;
    Ok(PluginManifestEventHook { on, command })
}

fn normalize_command(command: Vec<String>) -> Result<Vec<String>, (&'static str, String)> {
    let command = command
        .into_iter()
        .map(|arg| arg.trim().to_string())
        .collect::<Vec<_>>();
    if command.is_empty() || command.iter().any(|arg| arg.is_empty()) {
        return Err((
            "invalid_plugin_command",
            "command must contain non-empty argv strings".to_string(),
        ));
    }
    Ok(command)
}

fn non_empty_trimmed(
    value: &str,
    code: &'static str,
    message: &'static str,
) -> Result<String, (&'static str, String)> {
    let value = value.trim().to_string();
    if value.is_empty() {
        Err((code, message.to_string()))
    } else {
        Ok(value)
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

fn manifest_action_info(plugin_id: &str, action: &PluginManifestAction) -> PluginActionInfo {
    PluginActionInfo {
        plugin_id: plugin_id.to_string(),
        action_id: action.id.clone(),
        title: action.title.clone(),
        description: action.description.clone(),
        contexts: action.contexts.clone(),
        command: action.command.clone(),
    }
}

fn manifest_actions(
    plugins: &crate::app::state::InstalledPluginRegistry,
) -> impl Iterator<Item = PluginActionInfo> + '_ {
    plugins.values().flat_map(|plugin| {
        plugin
            .actions
            .iter()
            .map(|action| manifest_action_info(&plugin.plugin_id, action))
    })
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
    use crate::api::schema::{Method, Request, SuccessResponse};
    use std::time::{SystemTime, UNIX_EPOCH};

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

    fn unique_temp_path(name: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("herdr-{name}-{}-{nanos}", std::process::id()))
    }

    fn write_manifest(root: &std::path::Path) -> std::path::PathBuf {
        std::fs::create_dir_all(root).unwrap();
        let manifest = root.join("herdr-plugin.toml");
        std::fs::write(
            &manifest,
            r#"
id = "example.worktree-bootstrap"
name = "Worktree Bootstrap"
version = "0.1.0"
description = "Prepare new worktrees"

[[actions]]
id = "bootstrap"
title = "Bootstrap worktree"
contexts = ["workspace"]
command = ["bun", "run", "bootstrap.ts"]

[[events]]
on = "worktree.created"
command = ["bun", "run", "bootstrap.ts"]
"#,
        )
        .unwrap();
        manifest
    }

    fn link_manifest(app: &mut App, root: &std::path::Path) {
        let result = app.handle_api_request(Request {
            id: "link".into(),
            method: Method::PluginLink(PluginLinkParams {
                path: root.display().to_string(),
                enabled: true,
            }),
        });
        assert!(
            result.contains("plugin_linked"),
            "expected plugin_linked: {result}"
        );
    }

    #[test]
    fn plugin_link_lists_and_unlinks_manifest() {
        let mut app = test_app();
        let root = unique_temp_path("plugin-link");
        write_manifest(&root);

        let link = app.handle_api_request(Request {
            id: "link".into(),
            method: Method::PluginLink(PluginLinkParams {
                path: root.display().to_string(),
                enabled: true,
            }),
        });
        let ResponseResult::PluginLinked { plugin } = response_result(&link) else {
            panic!("expected plugin linked response: {link}");
        };
        assert_eq!(plugin.plugin_id, "example.worktree-bootstrap");
        assert_eq!(plugin.name, "Worktree Bootstrap");
        assert_eq!(plugin.version, "0.1.0");
        assert_eq!(plugin.plugin_root, root.display().to_string());
        assert!(plugin.enabled);
        assert_eq!(plugin.actions.len(), 1);
        assert_eq!(plugin.actions[0].id, "bootstrap");
        assert_eq!(plugin.actions[0].command, ["bun", "run", "bootstrap.ts"]);
        assert_eq!(plugin.events.len(), 1);
        assert_eq!(plugin.events[0].on, "worktree.created");

        let list = app.handle_api_request(Request {
            id: "list".into(),
            method: Method::PluginList(PluginListParams { plugin_id: None }),
        });
        let ResponseResult::PluginList { plugins } = response_result(&list) else {
            panic!("expected plugin list response: {list}");
        };
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].plugin_id, "example.worktree-bootstrap");

        let unlink = app.handle_api_request(Request {
            id: "unlink".into(),
            method: Method::PluginUnlink(PluginUnlinkParams {
                plugin_id: "example.worktree-bootstrap".into(),
            }),
        });
        assert!(matches!(
            response_result(&unlink),
            ResponseResult::PluginUnlinked {
                plugin_id,
                removed: true
            } if plugin_id == "example.worktree-bootstrap"
        ));

        let list = app.handle_api_request(Request {
            id: "list-empty".into(),
            method: Method::PluginList(PluginListParams { plugin_id: None }),
        });
        let ResponseResult::PluginList { plugins } = response_result(&list) else {
            panic!("expected plugin list response: {list}");
        };
        assert!(plugins.is_empty());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn manifest_action_list_and_invoke_with_context() {
        let mut app = test_app();
        let root = unique_temp_path("plugin-action-list");
        write_manifest(&root);
        link_manifest(&mut app, &root);

        let list = app.handle_api_request(Request {
            id: "list".into(),
            method: Method::PluginActionList(PluginActionListParams { plugin_id: None }),
        });
        let ResponseResult::PluginActionList { actions } = response_result(&list) else {
            panic!("expected plugin action list: {list}");
        };
        assert_eq!(actions.len(), 1);
        assert_eq!(
            actions[0].qualified_id(),
            "example.worktree-bootstrap.bootstrap"
        );
        assert_eq!(actions[0].command, ["bun", "run", "bootstrap.ts"]);

        let invoke = app.handle_api_request(Request {
            id: "invoke".into(),
            method: Method::PluginActionInvoke(PluginActionInvokeParams {
                plugin_id: Some("example.worktree-bootstrap".into()),
                action_id: "bootstrap".into(),
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
            panic!("expected plugin action invocation: {invoke}");
        };
        assert_eq!(
            action.qualified_id(),
            "example.worktree-bootstrap.bootstrap"
        );
        assert_eq!(action.command, ["bun", "run", "bootstrap.ts"]);
        assert_eq!(context.workspace_id.as_deref(), Some("1"));
        assert_eq!(context.invocation_source.as_deref(), Some("test"));
        assert_eq!(
            context.correlation_id.as_deref(),
            Some("external-correlation")
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn manifest_action_invoke_builds_default_workspace_context() {
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

        let root = unique_temp_path("plugin-action-context");
        // write a manifest with a "show" action in pane context
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("herdr-plugin.toml"),
            r#"
id = "example.context"
name = "Context"
version = "0.1.0"

[[actions]]
id = "show"
title = "Show Context"
contexts = ["pane"]
command = ["show-ctx"]
"#,
        )
        .unwrap();
        link_manifest(&mut app, &root);

        let invoke = app.handle_api_request(Request {
            id: "invoke-context".into(),
            method: Method::PluginActionInvoke(PluginActionInvokeParams {
                plugin_id: Some("example.context".into()),
                action_id: "show".into(),
                context: None,
            }),
        });

        let ResponseResult::PluginActionInvoked { context, .. } = response_result(&invoke) else {
            panic!("expected plugin action invocation: {invoke}");
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

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn manifest_action_invoke_returns_plugin_disabled_when_disabled() {
        let mut app = test_app();
        let root = unique_temp_path("plugin-disabled");
        write_manifest(&root);

        // link as disabled
        let link_result = app.handle_api_request(Request {
            id: "link-disabled".into(),
            method: Method::PluginLink(PluginLinkParams {
                path: root.display().to_string(),
                enabled: false,
            }),
        });
        assert!(
            link_result.contains("plugin_linked"),
            "expected plugin_linked: {link_result}"
        );

        let invoke = app.handle_api_request(Request {
            id: "invoke-disabled".into(),
            method: Method::PluginActionInvoke(PluginActionInvokeParams {
                plugin_id: Some("example.worktree-bootstrap".into()),
                action_id: "bootstrap".into(),
                context: None,
            }),
        });
        let value: serde_json::Value = serde_json::from_str(&invoke).unwrap();
        assert_eq!(value["error"]["code"], "plugin_disabled");

        let _ = std::fs::remove_dir_all(root);
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
