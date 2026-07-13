use std::path::PathBuf;

use crate::api::schema::{EventData, EventEnvelope, EventKind};
#[cfg(test)]
use tracing::error;

use super::{
    api_helpers::{pane_agent_status, tab_attention_priority},
    App, Mode,
};
use crate::{config::NewTerminalCwdConfig, workspace::Workspace};

// P9 materialization (mount -> rendered panes).
use crate::api::schema::{PaneInfo as RemotePaneInfo, TabInfo as RemoteTabInfo};
use crate::layout::PaneId;
use crate::pane::PaneState;
use crate::remote::federation::client::TerminalChannelRouter;
use crate::remote::federation::id::{strip_mount_namespace, Mount};
use crate::remote::federation::protocol::{ClipboardMessage, FederationMessage};
use crate::remote::federation::reducer::RemoteMirror;
use crate::terminal::{TerminalId, TerminalRuntime, TerminalState};
use crate::workspace::{MovedPane, WorktreeSpaceMembership};
use tokio::sync::mpsc::UnboundedSender;

pub(crate) fn resolve_new_terminal_cwd(
    policy: &NewTerminalCwdConfig,
    follow_cwd: Option<PathBuf>,
) -> PathBuf {
    match policy {
        NewTerminalCwdConfig::Follow => follow_cwd
            .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("/")),
        NewTerminalCwdConfig::Home => std::env::var_os("HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("/")),
        NewTerminalCwdConfig::Current => {
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))
        }
        NewTerminalCwdConfig::Path(path) => crate::worktree::expand_tilde_path(path),
    }
}

impl App {
    pub(super) fn seed_cwd_from_workspace(&self, ws_idx: usize) -> Option<PathBuf> {
        self.state
            .workspaces
            .get(ws_idx)?
            .resolved_identity_cwd_from(&self.state.terminals, &self.terminal_runtimes)
    }

    pub(super) fn follow_cwd_for_pane_in_workspace(
        &self,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
    ) -> Option<PathBuf> {
        let ws = self.state.workspaces.get(ws_idx)?;
        let tab_idx = ws.find_tab_index_for_pane(pane_id)?;
        ws.tabs.get(tab_idx)?.follow_cwd_for_pane(
            pane_id,
            &self.state.terminals,
            &self.terminal_runtimes,
        )
    }

    pub(super) fn focused_pane_cwd_in_workspace(&self, ws_idx: usize) -> Option<PathBuf> {
        let pane_id = self.state.workspaces.get(ws_idx)?.focused_pane_id()?;
        self.follow_cwd_for_pane_in_workspace(ws_idx, pane_id)
    }

    pub(super) fn resolve_new_terminal_cwd(&self, follow_cwd: Option<PathBuf>) -> PathBuf {
        resolve_new_terminal_cwd(&self.state.new_terminal_cwd, follow_cwd)
    }

    pub(super) fn workspace_creation_source(&self) -> Option<usize> {
        if self.state.mode == Mode::Navigate
            && self.state.workspaces.get(self.state.selected).is_some()
        {
            return Some(self.state.selected);
        }

        self.state.active.or_else(|| {
            self.state
                .workspaces
                .get(self.state.selected)
                .map(|_| self.state.selected)
        })
    }

    /// Create a workspace with a real PTY (needs event_tx).
    #[cfg(test)]
    pub(crate) fn create_workspace(&mut self) {
        let follow_cwd = self.workspace_creation_source().and_then(|ws_idx| {
            self.focused_pane_cwd_in_workspace(ws_idx)
                .or_else(|| self.seed_cwd_from_workspace(ws_idx))
        });
        let initial_cwd = self.resolve_new_terminal_cwd(follow_cwd);
        if let Err(e) = self.create_workspace_with_events(initial_cwd, true) {
            error!(err = %e, "failed to create workspace");
            self.state.mode = Mode::Navigate;
        }
    }

    #[cfg(test)]
    pub(crate) fn create_tab(&mut self) {
        let custom_name = self.state.requested_new_tab_name.take();
        let active_before = self.state.active;
        let follow_cwd = self.state.active.and_then(|ws_idx| {
            self.focused_pane_cwd_in_workspace(ws_idx)
                .or_else(|| self.seed_cwd_from_workspace(ws_idx))
        });
        let initial_cwd = self.resolve_new_terminal_cwd(follow_cwd);
        match self.create_tab_with_options(initial_cwd, true) {
            Ok(created_idx) => {
                let created_workspace = active_before.is_none();
                let ws_idx = if created_workspace {
                    Some(created_idx)
                } else {
                    self.state.active
                };
                let tab_idx = if created_workspace { 0 } else { created_idx };
                if let Some(name) = custom_name {
                    if let Some(ws) =
                        ws_idx.and_then(|ws_idx| self.state.workspaces.get_mut(ws_idx))
                    {
                        if let Some(tab) = ws.tabs.get_mut(tab_idx) {
                            tab.set_custom_name(name);
                        }
                        self.schedule_session_save();
                    }
                }
                if let Some(ws_idx) = ws_idx {
                    if created_workspace {
                        self.emit_workspace_open_events(ws_idx);
                    } else {
                        self.emit_tab_created_events(ws_idx, tab_idx);
                    }
                }
            }
            Err(e) => {
                error!(err = %e, "failed to create tab");
            }
        }
    }

    #[cfg(test)]
    pub(super) fn create_tab_with_options(
        &mut self,
        initial_cwd: PathBuf,
        focus: bool,
    ) -> std::io::Result<usize> {
        let Some(ws_idx) = self.state.active else {
            return self.create_workspace_with_options(initial_cwd, focus);
        };
        let (rows, cols) = self.state.estimate_pane_size();
        let ws = &mut self.state.workspaces[ws_idx];
        let (idx, terminal, runtime) = ws.create_tab(
            rows,
            cols,
            initial_cwd,
            self.state.pane_scrollback_limit_bytes,
            self.state.host_terminal_theme,
            crate::pane::PaneShellConfig::new(&self.state.default_shell, self.state.shell_mode),
            Vec::new(),
        )?;
        let root_pane = ws.tabs[idx].root_pane;
        self.terminal_runtimes.insert(terminal.id.clone(), runtime);
        self.state.terminals.insert(terminal.id.clone(), terminal);
        self.state.remove_alias_shadowed_by_new_pane(root_pane);
        if focus {
            self.state.switch_workspace_tab(ws_idx, idx);
            self.state.mode = Mode::Terminal;
        }
        let workspace_id = self.state.workspaces[ws_idx].id.clone();
        let tab_id = self
            .public_tab_id(ws_idx, idx)
            .unwrap_or_else(|| crate::workspace::public_tab_id_for_number(&workspace_id, idx + 1));
        let root_pane = self.state.workspaces[ws_idx].tabs[idx].root_pane.raw();
        crate::logging::tab_created(&workspace_id, &tab_id, root_pane);
        self.schedule_session_save();
        Ok(idx)
    }

    pub(crate) fn create_workspace_with_options(
        &mut self,
        initial_cwd: PathBuf,
        focus: bool,
    ) -> std::io::Result<usize> {
        self.create_workspace_with_launch_env(initial_cwd, focus, Vec::new())
    }

    #[cfg(test)]
    pub(crate) fn create_workspace_with_events(
        &mut self,
        initial_cwd: PathBuf,
        focus: bool,
    ) -> std::io::Result<()> {
        let ws_idx = self.create_workspace_with_options(initial_cwd, focus)?;
        self.emit_workspace_open_events(ws_idx);
        Ok(())
    }

    pub(crate) fn create_workspace_with_launch_env(
        &mut self,
        initial_cwd: PathBuf,
        focus: bool,
        extra_env: Vec<(String, String)>,
    ) -> std::io::Result<usize> {
        let (rows, cols) = self.state.estimate_pane_size();
        let (ws, terminal, runtime) = Workspace::new_with_extra_env(
            initial_cwd,
            rows,
            cols,
            self.state.pane_scrollback_limit_bytes,
            self.state.host_terminal_theme,
            crate::pane::PaneShellConfig::new(&self.state.default_shell, self.state.shell_mode),
            self.event_tx.clone(),
            self.render_notify.clone(),
            self.render_dirty.clone(),
            extra_env,
        )?;
        self.terminal_runtimes.insert(terminal.id.clone(), runtime);
        self.state.terminals.insert(terminal.id.clone(), terminal);
        self.state.workspaces.push(ws);
        let idx = self.state.workspaces.len() - 1;
        self.state
            .remove_alias_shadowed_by_new_pane(self.state.workspaces[idx].tabs[0].root_pane);
        let workspace_id = self.state.workspaces[idx].id.clone();
        let root_pane = self.state.workspaces[idx].tabs[0].root_pane.raw();
        crate::logging::workspace_created(&workspace_id, root_pane);
        if focus || self.state.active.is_none() {
            self.state.switch_workspace(idx);
            self.state.mode = Mode::Terminal;
        }
        self.schedule_session_save();
        Ok(idx)
    }

    pub(super) fn collect_panes_for_workspace(
        &self,
        workspace_id: Option<&str>,
    ) -> Result<Vec<crate::api::schema::PaneInfo>, (String, String)> {
        if let Some(workspace_id) = workspace_id {
            let Some(ws_idx) = self.parse_workspace_id(workspace_id) else {
                return Err((
                    "workspace_not_found".into(),
                    format!("workspace {workspace_id} not found"),
                ));
            };
            let Some(ws) = self.state.workspaces.get(ws_idx) else {
                return Err((
                    "workspace_not_found".into(),
                    format!("workspace {workspace_id} not found"),
                ));
            };
            Ok(ws
                .tabs
                .iter()
                .flat_map(|tab| tab.layout.pane_ids().into_iter())
                .filter_map(|pane_id| self.pane_info(ws_idx, pane_id))
                .collect())
        } else {
            Ok(self
                .state
                .workspaces
                .iter()
                .enumerate()
                .flat_map(|(ws_idx, ws)| {
                    ws.tabs
                        .iter()
                        .flat_map(|tab| tab.layout.pane_ids().into_iter())
                        .filter_map(move |pane_id| self.pane_info(ws_idx, pane_id))
                })
                .collect())
        }
    }

    pub(super) fn tab_info(
        &self,
        ws_idx: usize,
        tab_idx: usize,
    ) -> Option<crate::api::schema::TabInfo> {
        let ws = self.state.workspaces.get(ws_idx)?;
        let tab = ws.tabs.get(tab_idx)?;
        let (agg_state, seen) = tab
            .panes
            .values()
            .filter_map(|pane| {
                self.state
                    .terminals
                    .get(&pane.attached_terminal_id)
                    .map(|terminal| (terminal.state, pane.seen))
            })
            .max_by_key(|(state, seen)| tab_attention_priority(*state, *seen))
            .unwrap_or((crate::detect::AgentState::Unknown, true));
        Some(crate::api::schema::TabInfo {
            tab_id: self.public_tab_id(ws_idx, tab_idx)?,
            workspace_id: self.public_workspace_id(ws_idx),
            number: tab.number,
            label: ws.tab_display_name(tab_idx)?,
            focused: self.state.active == Some(ws_idx) && ws.active_tab == tab_idx,
            pane_count: tab.panes.len(),
            agent_status: pane_agent_status(agg_state, seen),
        })
    }

    pub(crate) fn emit_workspace_open_events(&mut self, ws_idx: usize) {
        let workspace_info = self.workspace_info(ws_idx);
        let Some(tab) = self.tab_info(ws_idx, 0) else {
            return;
        };
        let Some(root_pane) = self.root_pane_info(ws_idx, 0) else {
            return;
        };
        self.emit_event(EventEnvelope {
            event: EventKind::WorkspaceCreated,
            data: EventData::WorkspaceCreated {
                workspace: workspace_info,
            },
        });
        self.emit_tab_and_pane_created_events(tab, root_pane);
        self.emit_layout_updated_event(ws_idx, 0);
    }

    pub(crate) fn emit_tab_created_events(&mut self, ws_idx: usize, tab_idx: usize) {
        let Some(tab) = self.tab_info(ws_idx, tab_idx) else {
            return;
        };
        let Some(root_pane) = self.root_pane_info(ws_idx, tab_idx) else {
            return;
        };
        self.emit_tab_and_pane_created_events(tab, root_pane);
        self.emit_layout_updated_event(ws_idx, tab_idx);
    }

    fn emit_tab_and_pane_created_events(
        &mut self,
        tab: crate::api::schema::TabInfo,
        root_pane: crate::api::schema::PaneInfo,
    ) {
        self.emit_event(EventEnvelope {
            event: EventKind::TabCreated,
            data: EventData::TabCreated { tab },
        });
        self.emit_event(EventEnvelope {
            event: EventKind::PaneCreated,
            data: EventData::PaneCreated { pane: root_pane },
        });
    }

    pub(super) fn workspace_created_result(
        &self,
        ws_idx: usize,
    ) -> Option<crate::api::schema::ResponseResult> {
        Some(crate::api::schema::ResponseResult::WorkspaceCreated {
            workspace: self.workspace_info(ws_idx),
            tab: self.tab_info(ws_idx, 0)?,
            root_pane: self.root_pane_info(ws_idx, 0)?,
        })
    }

    pub(super) fn tab_created_result(
        &self,
        ws_idx: usize,
        tab_idx: usize,
    ) -> Option<crate::api::schema::ResponseResult> {
        Some(crate::api::schema::ResponseResult::TabCreated {
            tab: self.tab_info(ws_idx, tab_idx)?,
            root_pane: self.root_pane_info(ws_idx, tab_idx)?,
        })
    }

    pub(super) fn root_pane_info(
        &self,
        ws_idx: usize,
        tab_idx: usize,
    ) -> Option<crate::api::schema::PaneInfo> {
        let ws = self.state.workspaces.get(ws_idx)?;
        let tab = ws.tabs.get(tab_idx)?;
        self.pane_info(ws_idx, tab.root_pane)
    }

    pub(super) fn pane_info(
        &self,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
    ) -> Option<crate::api::schema::PaneInfo> {
        let ws = self.state.workspaces.get(ws_idx)?;
        let pane = ws.pane_state(pane_id)?;
        let terminal = self.state.terminals.get(&pane.attached_terminal_id)?;
        let tab_idx = ws.find_tab_index_for_pane(pane_id)?;
        let scroll = self
            .state
            .runtime_for_pane_in_workspace(&self.terminal_runtimes, ws_idx, pane_id)
            .and_then(|runtime| runtime.scroll_metrics())
            .map(|metrics| crate::api::schema::PaneScrollInfo {
                offset_from_bottom: metrics.offset_from_bottom as u64,
                max_offset_from_bottom: metrics.max_offset_from_bottom as u64,
                viewport_rows: metrics.viewport_rows as u64,
            });
        let focused = self.state.active == Some(ws_idx)
            && ws.active_tab == tab_idx
            && ws
                .focused_pane_id()
                .is_some_and(|focused| focused == pane_id);
        let presentation = terminal.effective_presentation();
        Some(crate::api::schema::PaneInfo {
            pane_id: self.public_pane_id(ws_idx, pane_id)?,
            terminal_id: terminal.id.to_string(),
            workspace_id: self.public_workspace_id(ws_idx),
            tab_id: self.public_tab_id(ws_idx, tab_idx)?,
            focused,
            cwd: ws.tabs[tab_idx]
                .cwd_for_pane(pane_id, &self.state.terminals, &self.terminal_runtimes)
                .map(|cwd| cwd.display().to_string()),
            foreground_cwd: ws.tabs[tab_idx]
                .foreground_cwd_for_pane(pane_id, &self.terminal_runtimes)
                .map(|cwd| cwd.display().to_string()),
            label: terminal.manual_label.clone(),
            agent: terminal.effective_agent_label().map(str::to_string),
            title: presentation.title,
            terminal_title: terminal.terminal_title.clone(),
            terminal_title_stripped: terminal.terminal_title_stripped(),
            display_agent: presentation.display_agent,
            agent_status: pane_agent_status(terminal.state, pane.seen),
            state_labels: presentation.state_labels,
            tokens: terminal.metadata_tokens.values(),
            agent_session: terminal_agent_session_info(terminal),
            scroll,
            revision: terminal.revision,
        })
    }

    pub(super) fn lookup_runtime(
        &self,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
    ) -> Option<(&crate::terminal::TerminalRuntime, String)> {
        let runtime =
            self.state
                .runtime_for_pane_in_workspace(&self.terminal_runtimes, ws_idx, pane_id)?;
        Some((runtime, self.public_workspace_id(ws_idx)))
    }

    pub(super) fn lookup_runtime_sender(
        &self,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
    ) -> Option<&crate::terminal::TerminalRuntime> {
        self.state
            .runtime_for_pane_in_workspace(&self.terminal_runtimes, ws_idx, pane_id)
    }

    /// Resolves a `terminal_id` (the same id space `AgentInfo::terminal_id`
    /// and `resolve_terminal_target` use) straight to its `TerminalRuntime`.
    /// Added for `remote::federation::serve` (P3): the federation raw-byte
    /// tap and terminal input/resize routing need the runtime itself, not a
    /// JSON-API-shaped response. Thin combinator over already-existing
    /// `resolve_terminal_target` + `lookup_runtime_sender`; no new
    /// resolution logic.
    pub(crate) fn terminal_runtime_for_terminal_id(
        &self,
        terminal_id: &str,
    ) -> Option<&crate::terminal::TerminalRuntime> {
        let target = self.resolve_terminal_target(terminal_id).ok()?;
        self.lookup_runtime_sender(target.ws_idx, target.pane_id)
    }

    pub(super) fn workspace_info(&self, index: usize) -> crate::api::schema::WorkspaceInfo {
        let ws = &self.state.workspaces[index];
        let (agg_state, seen) = ws.aggregate_state(&self.state.terminals);
        crate::api::schema::WorkspaceInfo {
            workspace_id: self.public_workspace_id(index),
            number: index + 1,
            label: ws.display_name_from(&self.state.terminals, &self.terminal_runtimes),
            focused: self.state.active == Some(index),
            pane_count: ws.public_pane_numbers.len(),
            tab_count: ws.tabs.len(),
            active_tab_id: self.public_tab_id(index, ws.active_tab).unwrap_or_else(|| {
                crate::workspace::public_tab_id_for_number(&ws.id, ws.active_tab + 1)
            }),
            agent_status: pane_agent_status(agg_state, seen),
            tokens: ws.metadata_tokens.values(),
            worktree: ws
                .worktree_space()
                .map(|space| crate::api::schema::WorkspaceWorktreeInfo {
                    repo_key: space.key.clone(),
                    repo_name: space.label.clone(),
                    repo_root: space.repo_root.display().to_string(),
                    checkout_path: space.checkout_path.display().to_string(),
                    is_linked_worktree: space.is_linked_worktree,
                }),
        }
    }

    /// P9 Priority 2 (mount -> rendered panes): materializes a successful
    /// federation mount's already-namespaced `RemoteMirror` snapshot into
    /// real `Workspace`/`Tab`/`PaneState` entries, each remote-backed pane
    /// spawned via `PaneRuntime::spawn_remote` (P5) and fed by the live
    /// `router`/`out_tx`/`clipboard_tx` channels riding the ONE mount
    /// tunnel. v1 scope (logged in implementation-notes.md): eagerly spawns
    /// every mirrored pane at mount time rather than the lazy
    /// hydrate-on-focus design phase-05 anticipated (S12.1) — smaller and
    /// more reversible; lazy hydrate can be layered on later without
    /// reworking this call site. Reuses the SAME construction primitives an
    /// existing "move pane to a new workspace" already uses
    /// (`Workspace::from_existing_pane`, `create_tab_from_existing_pane`,
    /// `Tab::insert_existing_pane`) rather than inventing a parallel
    /// creation path, and the SAME event-emission path
    /// (`emit_workspace_open_events`/`emit_tab_created_events`) every other
    /// workspace/tab creation uses, so the sidebar refreshes exactly as it
    /// would for a locally-created workspace. Returns the indices of the
    /// newly created workspaces in `self.state.workspaces`.
    pub(crate) fn materialize_federation_mount(
        &mut self,
        mirror: &RemoteMirror,
        router: &mut TerminalChannelRouter,
        out_tx: &UnboundedSender<FederationMessage>,
        clipboard_tx: &UnboundedSender<ClipboardMessage>,
    ) -> std::io::Result<Vec<usize>> {
        let mount = mirror.mount().clone();
        let (rows, cols) = self.state.estimate_pane_size();
        let scrollback_limit_bytes = self.state.pane_scrollback_limit_bytes;
        let host_terminal_theme = self.state.host_terminal_theme;

        let mut workspaces: Vec<_> = mirror.workspaces().values().collect();
        workspaces.sort_by_key(|ws| ws.number);

        let mut created_ws_idxs = Vec::new();

        for ws_info in workspaces {
            let mut tabs: Vec<&RemoteTabInfo> = mirror
                .tabs()
                .values()
                .filter(|tab| tab.workspace_id == ws_info.workspace_id)
                .collect();
            tabs.sort_by_key(|tab| tab.number);
            if tabs.is_empty() {
                // A workspace with no tabs is not representable locally
                // (every `Workspace` must have >=1 tab); skip it rather than
                // constructing an invalid entry. Not expected from a real
                // `federation-serve` host (every workspace it reports always
                // has a root tab).
                continue;
            }

            let mut ws_idx: Option<usize> = None;
            for tab_info in tabs {
                let mut panes: Vec<&RemotePaneInfo> = mirror
                    .panes()
                    .values()
                    .filter(|pane| pane.tab_id == tab_info.tab_id)
                    .collect();
                // `PaneInfo` carries no explicit split-order field; sort by
                // the (already-namespaced, stable) public pane id so
                // materialization order is deterministic across runs against
                // the same mirror snapshot.
                panes.sort_by(|a, b| a.pane_id.cmp(&b.pane_id));
                let Some((first_pane, rest_panes)) = panes.split_first() else {
                    // A tab with no panes is similarly not representable;
                    // skip it (not expected from a real host).
                    continue;
                };

                let (root_pane_id, terminal, runtime, pane_state) = self.build_remote_pane(
                    &mount,
                    first_pane,
                    rows,
                    cols,
                    scrollback_limit_bytes,
                    host_terminal_theme,
                    router,
                    out_tx,
                    clipboard_tx,
                )?;
                let terminal_id = terminal.id.clone();
                self.terminal_runtimes.insert(terminal_id.clone(), runtime);
                self.state.terminals.insert(terminal_id, terminal);
                let moved = MovedPane {
                    pane_id: root_pane_id,
                    pane_state,
                };

                let tab_idx = if let Some(existing_ws_idx) = ws_idx {
                    self.state.workspaces[existing_ws_idx].create_tab_from_existing_pane(
                        moved,
                        Some(tab_info.label.clone()),
                        self.event_tx.clone(),
                        self.render_notify.clone(),
                        self.render_dirty.clone(),
                    )
                } else {
                    let mut workspace = Workspace::from_existing_pane(
                        Some(ws_info.label.clone()),
                        Some(tab_info.label.clone()),
                        PathBuf::from(first_pane.cwd.clone().unwrap_or_else(|| "/".to_string())),
                        moved,
                        self.event_tx.clone(),
                        self.render_notify.clone(),
                        self.render_dirty.clone(),
                    );
                    // RT-F8/S11.4: the sidebar badge/grouping
                    // (`ui::sidebar::workspace_federation_origin`) classifies
                    // purely from `Workspace::id`'s `r:<host_key>:` prefix —
                    // never from `custom_name` — so materialized workspaces
                    // must carry the mirror's own namespaced id, not the
                    // fresh local id `from_existing_pane` generates.
                    workspace.id = ws_info.workspace_id.clone();
                    workspace.worktree_space = Some(WorktreeSpaceMembership {
                        key: format!("federation:{}", mount.host_key.as_str()),
                        label: mount.host_key.as_str().to_string(),
                        repo_root: PathBuf::new(),
                        checkout_path: PathBuf::new(),
                        is_linked_worktree: false,
                    });
                    self.state.workspaces.push(workspace);
                    let idx = self.state.workspaces.len() - 1;
                    ws_idx = Some(idx);
                    created_ws_idxs.push(idx);
                    0
                };
                let this_ws_idx = ws_idx.expect("set immediately above on first tab");
                // `Workspace::from_existing_pane` always seeds exactly one
                // tab at index 0; `emit_workspace_open_events` (below, once
                // per newly created workspace) already covers that tab's
                // creation event, so only a *subsequent* tab (index != 0)
                // needs its own `TabCreated`/`PaneCreated` events here.
                let created_this_tab = tab_idx != 0;

                let mut prev_pane_id = root_pane_id;
                for pane_info in rest_panes {
                    let (split_pane_id, split_terminal, split_runtime, split_pane_state) = self
                        .build_remote_pane(
                            &mount,
                            pane_info,
                            rows,
                            cols,
                            scrollback_limit_bytes,
                            host_terminal_theme,
                            router,
                            out_tx,
                            clipboard_tx,
                        )?;
                    let split_terminal_id = split_terminal.id.clone();
                    self.terminal_runtimes
                        .insert(split_terminal_id.clone(), split_runtime);
                    self.state.terminals.insert(split_terminal_id, split_terminal);
                    let split_moved = MovedPane {
                        pane_id: split_pane_id,
                        pane_state: split_pane_state,
                    };
                    // Splits materialize as a simple horizontal chain (v1);
                    // this does not attempt to reproduce the remote's exact
                    // split geometry (not carried by `PaneInfo`).
                    if self.state.workspaces[this_ws_idx].tabs[tab_idx]
                        .insert_existing_pane(
                            prev_pane_id,
                            split_moved,
                            ratatui::layout::Direction::Horizontal,
                            0.5,
                        )
                        .is_ok()
                    {
                        prev_pane_id = split_pane_id;
                    }
                }

                if created_this_tab {
                    self.emit_tab_created_events(this_ws_idx, tab_idx);
                }
            }
        }

        for ws_idx in &created_ws_idxs {
            self.emit_workspace_open_events(*ws_idx);
            self.schedule_session_save();
        }

        Ok(created_ws_idxs)
    }

    /// Builds one remote-backed pane's `TerminalState`/`TerminalRuntime`/
    /// `PaneState` triple for [`App::materialize_federation_mount`]. Opens
    /// this pane's federation `Terminal` channel via `router.open_terminal`
    /// (raw, un-namespaced remote terminal id — `strip_mount_namespace`
    /// reverses the reducer's P7 ingest-time namespacing) and wires the
    /// resulting byte receiver straight into `PaneRuntime::spawn_remote`
    /// (P5), so scrollback replay + live output flow into the pane exactly
    /// as they do for a focused pane in the (currently dormant) lazy-hydrate
    /// design — this call site simply triggers it for every mirrored pane at
    /// mount time instead of on focus (see the v1-scope note above).
    #[allow(clippy::too_many_arguments)]
    fn build_remote_pane(
        &self,
        mount: &Mount,
        pane_info: &RemotePaneInfo,
        rows: u16,
        cols: u16,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        router: &mut TerminalChannelRouter,
        out_tx: &UnboundedSender<FederationMessage>,
        clipboard_tx: &UnboundedSender<ClipboardMessage>,
    ) -> std::io::Result<(PaneId, TerminalState, TerminalRuntime, PaneState)> {
        let raw_terminal_id = strip_mount_namespace(mount, &pane_info.terminal_id);
        let output_rx = router.open_terminal(raw_terminal_id.clone(), mount.mount_generation, out_tx);
        let pane_id = PaneId::alloc();
        let terminal_id = TerminalId::alloc();
        let runtime = TerminalRuntime::spawn_remote(
            pane_id,
            rows,
            cols,
            scrollback_limit_bytes,
            host_terminal_theme,
            None,
            raw_terminal_id,
            mount.mount_generation,
            out_tx.clone(),
            output_rx,
            clipboard_tx.clone(),
            self.event_tx.clone(),
            self.render_notify.clone(),
            self.render_dirty.clone(),
        )?;
        let mut terminal = TerminalState::new(
            terminal_id.clone(),
            pane_info
                .cwd
                .clone()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/")),
        );
        terminal.manual_label = pane_info.label.clone();
        let pane_state = PaneState::new(terminal_id);
        Ok((pane_id, terminal, runtime, pane_state))
    }
}

fn terminal_agent_session_info(
    terminal: &crate::terminal::TerminalState,
) -> Option<crate::api::schema::AgentSessionInfo> {
    if let Some(authority) = terminal.hook_authority.as_ref() {
        if let Some(session_ref) = authority.session_ref.as_ref() {
            return Some(crate::api::schema::AgentSessionInfo {
                source: authority.source.clone(),
                agent: authority.agent_label.clone(),
                kind: session_ref.kind,
                value: session_ref.value.clone(),
            });
        }
    }

    terminal
        .persisted_agent_session
        .as_ref()
        .map(|session| crate::api::schema::AgentSessionInfo {
            source: session.source.clone(),
            agent: session.agent.clone(),
            kind: session.session_ref.kind,
            value: session.session_ref.value.clone(),
        })
}

#[cfg(test)]
mod federation_materialization_tests {
    use super::*;
    use crate::api::schema::common::AgentStatus;
    use crate::api::schema::session::SessionSnapshot;
    use crate::api::schema::{PaneInfo as RemotePaneInfo, TabInfo as RemoteTabInfo, WorkspaceInfo};
    use crate::remote::federation::id::{HostKey, ServerInstanceId};
    use crate::remote::federation::protocol::{EventCursor, TerminalChannelMessage};
    use crate::ui::sidebar::{federation_origin_badge, workspace_federation_origin};

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

    fn mount(generation: u64) -> Mount {
        Mount {
            host_key: HostKey::new("alice@10.0.0.1", "s1"),
            server_instance_id: ServerInstanceId("inst-a".to_string()),
            mount_generation: generation,
        }
    }

    fn workspace_info() -> WorkspaceInfo {
        WorkspaceInfo {
            workspace_id: "w1".to_string(),
            number: 1,
            label: "remote workspace".to_string(),
            focused: false,
            pane_count: 2,
            tab_count: 1,
            active_tab_id: "w1-tab".to_string(),
            agent_status: AgentStatus::Idle,
            tokens: Default::default(),
            worktree: None,
        }
    }

    fn tab_info() -> RemoteTabInfo {
        RemoteTabInfo {
            tab_id: "w1-tab".to_string(),
            workspace_id: "w1".to_string(),
            number: 1,
            label: "remote tab".to_string(),
            focused: false,
            pane_count: 2,
            agent_status: AgentStatus::Idle,
        }
    }

    fn pane_info(pane_id: &str, terminal_id: &str) -> RemotePaneInfo {
        RemotePaneInfo {
            pane_id: pane_id.to_string(),
            terminal_id: terminal_id.to_string(),
            workspace_id: "w1".to_string(),
            tab_id: "w1-tab".to_string(),
            focused: false,
            cwd: Some("/home/alice/project".to_string()),
            foreground_cwd: None,
            label: Some("remote pane".to_string()),
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
            revision: 0,
        }
    }

    fn two_pane_snapshot() -> SessionSnapshot {
        SessionSnapshot {
            version: "0.0.0-test".to_string(),
            protocol: 1,
            focused_workspace_id: None,
            focused_tab_id: None,
            focused_pane_id: None,
            workspaces: vec![workspace_info()],
            tabs: vec![tab_info()],
            panes: vec![pane_info("p1", "t1"), pane_info("p2", "t2")],
            layouts: Vec::new(),
            agents: Vec::new(),
        }
    }

    // Core acceptance criterion: a successful mount materializes into real
    // rendered Workspace/Tab/Pane entries (remote-backed via spawn_remote),
    // and the RT-F8 origin badge / per-host grouping — which classify purely
    // from `Workspace::id`'s `r:<host_key>:` prefix (ui::sidebar) — pick the
    // materialized workspace up correctly.
    #[tokio::test]
    async fn successful_mount_materializes_into_rendered_workspace_tab_and_two_panes() {
        let mut app = test_app();
        let mount = mount(1);
        let mut mirror = RemoteMirror::new(mount.clone());
        mirror.apply_snapshot(&two_pane_snapshot(), EventCursor(0));

        let mut router = TerminalChannelRouter::new();
        let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel();
        let (clipboard_tx, _clipboard_rx) = tokio::sync::mpsc::unbounded_channel();

        let created = app
            .materialize_federation_mount(&mirror, &mut router, &out_tx, &clipboard_tx)
            .expect("materialization must succeed against a loopback-shaped snapshot");

        assert_eq!(created.len(), 1, "exactly one remote workspace was mounted");
        let ws_idx = created[0];
        let ws = &app.state.workspaces[ws_idx];

        // RT-F8/S11.4: badge + grouping key off `Workspace::id`'s namespace,
        // never off `custom_name` — assert both actually fire for the
        // materialized workspace.
        assert!(ws.id.starts_with("r:alice@10.0.0.1#s1:"));
        assert!(workspace_federation_origin(ws).is_some());
        assert!(federation_origin_badge(ws)
            .expect("materialized workspace must carry the remote badge")
            .contains("alice@10.0.0.1#s1"));
        assert_eq!(
            ws.worktree_space.as_ref().map(|space| space.key.as_str()),
            Some("federation:alice@10.0.0.1#s1")
        );

        assert_eq!(ws.tabs.len(), 1, "one remote tab materialized");
        let tab = &ws.tabs[0];
        assert_eq!(tab.panes.len(), 2, "both remote panes materialized (root + split)");

        // Every materialized pane's attached terminal is reachable in both
        // App-level maps a real pane needs (mirrors what
        // create_tab_with_options/create_workspace_with_launch_env do for a
        // local pane).
        for pane in tab.panes.values() {
            let terminal_id = &pane.attached_terminal_id;
            assert!(app.state.terminals.contains_key(terminal_id));
            assert!(app.terminal_runtimes.contains_key(terminal_id));
        }

        // The router opened a federation Terminal channel for each pane
        // under the RAW (un-namespaced) remote terminal id, not the local
        // public one — this is what re-registers correctly when the wire
        // sends `Output` back for it.
        out_rx.close();
        let mut opened_raw_ids = Vec::new();
        while let Ok(msg) = out_rx.try_recv() {
            if let FederationMessage::Terminal(TerminalChannelMessage::Open { terminal_id, .. }) =
                msg
            {
                opened_raw_ids.push(terminal_id);
            }
        }
        opened_raw_ids.sort();
        assert_eq!(opened_raw_ids, vec!["t1".to_string(), "t2".to_string()]);
    }

    #[tokio::test]
    async fn materializing_an_empty_mirror_creates_nothing() {
        let mut app = test_app();
        let mount = mount(1);
        let mirror = RemoteMirror::new(mount);

        let mut router = TerminalChannelRouter::new();
        let (out_tx, _out_rx) = tokio::sync::mpsc::unbounded_channel();
        let (clipboard_tx, _clipboard_rx) = tokio::sync::mpsc::unbounded_channel();

        let created = app
            .materialize_federation_mount(&mirror, &mut router, &out_tx, &clipboard_tx)
            .expect("materializing an empty mirror must not error");

        assert!(created.is_empty());
        assert!(app.state.workspaces.is_empty());
    }
}
