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
    ///
    /// Dormant outside tests until a live CLI call site wires a real mount
    /// into it (same `#[allow(dead_code)]` precedent as P5's
    /// `PaneRuntime::spawn_remote` itself, P4's `client.rs`/`reducer.rs`, and
    /// P8's sidebar badge helpers before their own live call sites landed).
    #[allow(dead_code)]
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
                    self.state
                        .terminals
                        .insert(split_terminal_id, split_terminal);
                    let split_moved = MovedPane {
                        pane_id: split_pane_id,
                        pane_state: split_pane_state,
                    };
                    // Splits materialize as a simple horizontal chain (v1);
                    // this does not attempt to reproduce the remote's exact
                    // split geometry (not carried by `PaneInfo`). Goes
                    // through `Workspace::insert_moved_pane_into_tab` (not
                    // `Tab::insert_existing_pane` directly) so this
                    // non-root remote pane also gets a `public_pane_numbers`
                    // entry — otherwise it is unreachable through the public
                    // pane-id API (list/focus/close) even though it is a
                    // real live pane.
                    if self.state.workspaces[this_ws_idx]
                        .insert_moved_pane_into_tab(
                            tab_idx,
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
    #[allow(clippy::too_many_arguments, dead_code)]
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
        let output_rx =
            router.open_terminal(raw_terminal_id.clone(), mount.mount_generation, out_tx);
        let pane_id = PaneId::alloc();
        let terminal_id = TerminalId::alloc();
        let runtime = TerminalRuntime::spawn_remote(
            pane_id,
            rows,
            cols,
            scrollback_limit_bytes,
            host_terminal_theme,
            None,
            raw_terminal_id.clone(),
            mount.mount_generation,
            out_tx.clone(),
            output_rx,
            clipboard_tx.clone(),
            self.event_tx.clone(),
            self.render_notify.clone(),
            self.render_dirty.clone(),
        )?;
        // P6/P8/P9 wiring: register this pane's relayed-agent-status sink
        // under the same raw terminal id `router` uses for output routing,
        // so `drive_mount_channel`'s `AgentStatus` handling can forward the
        // remote's real detection status into this pane's own detection
        // loop (`PaneRuntime::relayed_agent_status_sender`).
        if let Some(sender) = runtime.relayed_agent_status_sender() {
            router.register_agent_status_sender(raw_terminal_id, sender);
        }
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

    /// Remembers the local layout context a `SplitPaneRequest` was minted
    /// for, keyed by its `request_id`
    /// (`app/api/panes.rs::dispatch_remote_pane_split`), so
    /// `handle_federation_split_pane_ready`/`_failed` can splice the eventual
    /// response into the right tab once it arrives (fire-and-forget: the
    /// mint site cannot await it inline, see that function's own doc
    /// comment).
    pub(crate) fn register_pending_remote_split(
        &mut self,
        request_id: u64,
        pending: PendingRemoteSplit,
    ) {
        self.pending_remote_splits.insert(request_id, pending);
    }

    fn take_pending_remote_split(&mut self, request_id: u64) -> Option<PendingRemoteSplit> {
        self.pending_remote_splits.remove(&request_id)
    }

    /// Drops every pending remote-split registration targeting one of the
    /// given (closing) workspace ids. Call this from
    /// `handle_federation_mount_ended` before those workspaces are removed,
    /// so a late/never-arriving `SplitPaneResponse` for a torn-down mount
    /// can no longer splice its pane into whatever workspace later reuses
    /// the same index, and the entry doesn't leak in the map forever.
    pub(crate) fn purge_pending_remote_splits_for_workspaces(
        &mut self,
        workspace_ids: &std::collections::HashSet<String>,
    ) {
        self.pending_remote_splits
            .retain(|_, pending| !workspace_ids.contains(&pending.workspace_id));
    }

    /// `AppEvent::FederationSplitPaneReady` handler: the drive task already
    /// built the new pane's real `TerminalRuntime` (it owns the mount's
    /// `TerminalChannelRouter`/out-tx, which this handler does not); this
    /// splices it into the requesting pane's own tab layout, the same
    /// `insert_existing_pane` primitive `materialize_federation_mount` uses
    /// for mount-time split panes.
    #[cfg(unix)]
    pub(crate) fn handle_federation_split_pane_ready(
        &mut self,
        ready: crate::events::FederationSplitPaneReady,
    ) {
        let crate::events::FederationSplitPaneReady {
            request_id,
            origin,
            pane_id,
            terminal_id,
            terminal,
            runtime,
            pane_state,
        } = ready;

        if let Some(pending) = self.pending_remote_splits.get(&request_id) {
            if pending.origin != origin {
                tracing::warn!(
                    request_id,
                    expected_origin = %pending.origin,
                    got_origin = %origin,
                    "dropping a split-pane response from a mount that did not \
                     originate this request"
                );
                return;
            }
        }

        let Some(pending) = self.take_pending_remote_split(request_id) else {
            tracing::warn!(
                request_id,
                "remote split materialized a pane for an unknown/stale request; dropping it"
            );
            return;
        };
        let Some(ws_idx) = self
            .state
            .workspaces
            .iter()
            .position(|ws| ws.id == pending.workspace_id)
        else {
            tracing::warn!(
                request_id,
                workspace_id = %pending.workspace_id,
                "remote split materialized a pane but its workspace no longer exists"
            );
            return;
        };
        let ws = &mut self.state.workspaces[ws_idx];
        let Some(tab_idx) = ws.find_tab_index_for_pane(pending.target_pane_id) else {
            tracing::warn!(
                request_id,
                "remote split materialized a pane but its target pane no longer exists"
            );
            return;
        };

        let moved = crate::workspace::MovedPane {
            pane_id,
            pane_state,
        };
        // `Workspace::insert_moved_pane_into_tab` (not `Tab::
        // insert_existing_pane` directly), so this non-root remote pane
        // also gets a `public_pane_numbers` entry (same reasoning as
        // `materialize_federation_mount`'s split-chain loop above).
        if ws
            .insert_moved_pane_into_tab(
                tab_idx,
                pending.target_pane_id,
                moved,
                pending.direction,
                pending.ratio,
            )
            .is_err()
        {
            tracing::warn!(
                request_id,
                "remote split materialized a pane but it could not be inserted into its \
                 target tab's layout"
            );
            return;
        }

        self.terminal_runtimes.insert(terminal_id.clone(), runtime);
        self.state.terminals.insert(terminal_id, terminal);
        self.state.remove_alias_shadowed_by_new_pane(pane_id);
        self.schedule_session_save();

        if pending.focus {
            let previous_focus = self.state.current_pane_focus_target();
            self.state.switch_workspace_tab(ws_idx, tab_idx);
            self.state
                .record_pane_focus_change(previous_focus, ws_idx, pane_id);
            self.state.settle_terminal_mode_after_focus();
        }

        if let Some(pane) = self.pane_info(ws_idx, pane_id) {
            self.emit_event(EventEnvelope {
                event: EventKind::PaneCreated,
                data: EventData::PaneCreated { pane },
            });
        }
        self.emit_layout_updated_event(ws_idx, tab_idx);

        self.render_dirty
            .store(true, std::sync::atomic::Ordering::Release);
        self.render_notify.notify_one();
    }

    /// `AppEvent::FederationSplitPaneFailed` handler: the remote host
    /// rejected (or the mount could not carry) an earlier split request —
    /// drop the pending context and surface it exactly like a failed local
    /// split, via the same toast mechanism `handle_federation_mount_failed`
    /// uses.
    #[cfg(unix)]
    pub(crate) fn handle_federation_split_pane_failed(
        &mut self,
        request_id: u64,
        reason: String,
        origin: crate::remote::federation::id::HostKey,
    ) {
        if let Some(pending) = self.pending_remote_splits.get(&request_id) {
            if pending.origin != origin {
                tracing::warn!(
                    request_id,
                    expected_origin = %pending.origin,
                    got_origin = %origin,
                    "dropping a split-pane failure from a mount that did not \
                     originate this request"
                );
                return;
            }
        }
        self.take_pending_remote_split(request_id);
        tracing::warn!(request_id, %reason, "remote split failed");
        match self.state.toast_config.delivery {
            crate::config::ToastDelivery::Herdr => {
                self.state.toast = Some(crate::app::state::ToastNotification {
                    kind: super::ToastKind::NeedsAttention,
                    title: "remote split failed".to_string(),
                    context: reason,
                    position: None,
                    target: None,
                });
            }
            crate::config::ToastDelivery::Terminal | crate::config::ToastDelivery::System
                if self.local_terminal_notifications =>
            {
                let notify = match self.state.toast_config.delivery {
                    crate::config::ToastDelivery::Terminal => {
                        crate::terminal_notify::show_notification
                    }
                    crate::config::ToastDelivery::System => {
                        crate::platform::show_desktop_notification
                    }
                    _ => unreachable!("toast delivery was matched above"),
                };
                let _ = notify("remote split failed", Some(&reason));
            }
            _ => {}
        }
        self.render_dirty
            .store(true, std::sync::atomic::Ordering::Release);
        self.render_notify.notify_one();
    }
}

/// Local layout context a `SplitPaneRequest` was minted from, remembered
/// until its `SplitPaneResponse` arrives (or the process ends). See
/// `App::register_pending_remote_split`/`handle_federation_split_pane_ready`.
pub(crate) struct PendingRemoteSplit {
    /// Stable workspace id (`Workspace::id`), not a `Vec` index — indices
    /// shift when workspaces close, so a raw `usize` here could splice a
    /// late/stale response into an unrelated workspace that later occupies
    /// the same slot (see `App::purge_pending_remote_splits_for_workspaces`).
    pub(crate) workspace_id: String,
    pub(crate) target_pane_id: PaneId,
    pub(crate) direction: ratatui::layout::Direction,
    pub(crate) ratio: f32,
    pub(crate) focus: bool,
    /// The mount this split request was actually sent to
    /// (`App::federation_host_key_for_workspace` at mint time). A
    /// `SplitPaneResponse`/`Failed` answering this `request_id` is only
    /// honored if it arrives tagged with this same origin — otherwise a
    /// second, differently-mounted host could splice a pane into this
    /// workspace by predicting/observing the process-global `request_id`
    /// counter.
    pub(crate) origin: crate::remote::federation::id::HostKey,
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
    use crate::remote::federation::id::{classify, HostKey, IdClass, ServerInstanceId};
    use crate::remote::federation::protocol::{EventCursor, TerminalChannelMessage};

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

        // RT-F8/S11.4: `ui::sidebar::workspace_federation_origin`/
        // `federation_origin_badge` classify a workspace as federated purely
        // via `remote::federation::id::classify(&ws.id)` — assert that same
        // classification actually fires for the materialized workspace
        // (`ui::sidebar` is not reachable from this module; `classify` is
        // the exact primitive it is built on).
        assert!(ws.id.starts_with("r:alice@10.0.0.1#s1:"));
        assert!(matches!(classify(&ws.id), IdClass::Remote(_)));
        assert_eq!(
            ws.worktree_space.as_ref().map(|space| space.key.as_str()),
            Some("federation:alice@10.0.0.1#s1")
        );

        assert_eq!(ws.tabs.len(), 1, "one remote tab materialized");
        let tab = &ws.tabs[0];
        assert_eq!(
            tab.panes.len(),
            2,
            "both remote panes materialized (root + split)"
        );

        // Non-root remote panes must go through `Workspace::
        // insert_moved_pane_into_tab` (not `Tab::insert_existing_pane`
        // directly), so every live pane — including the split-materialized
        // one — has a `public_pane_numbers` entry and is reachable through
        // the public pane-id API (list/focus/close), not just internally.
        ws.assert_invariants_for_test();
        for pane_id in tab.panes.keys() {
            assert!(
                ws.public_pane_number(*pane_id).is_some(),
                "every materialized remote pane must have a public pane number"
            );
        }

        // Every materialized pane's attached terminal is reachable in both
        // App-level maps a real pane needs (mirrors what
        // create_tab_with_options/create_workspace_with_launch_env do for a
        // local pane).
        for pane in tab.panes.values() {
            let terminal_id = &pane.attached_terminal_id;
            assert!(app.state.terminals.contains_key(terminal_id));
            assert!(app.terminal_runtimes.get(terminal_id).is_some());
        }

        // The router opened a federation Terminal channel for each pane
        // under the RAW (un-namespaced) remote terminal id, not the local
        // public one — this is what re-registers correctly when the wire
        // sends `Output` back for it.
        out_rx.close();
        let mut opened_raw_ids = Vec::new();
        while let Ok(msg) = out_rx.try_recv() {
            if let FederationMessage::Terminal(TerminalChannelMessage::Open {
                terminal_id, ..
            }) = msg
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

    /// Regression for the public-pane-numbering bypass: before the fix,
    /// `handle_federation_split_pane_ready` spliced the new pane in via
    /// `Tab::insert_existing_pane` directly, never registering it in
    /// `Workspace::public_pane_numbers` — so the pane existed and rendered,
    /// but was unreachable through the public pane-id API (list/focus/
    /// close). Routing through `Workspace::insert_moved_pane_into_tab`
    /// fixes that; `assert_invariants_for_test` independently enforces
    /// "every live pane has a public pane number".
    #[cfg(unix)]
    #[tokio::test]
    async fn split_materialization_assigns_a_public_pane_number_to_the_new_pane() {
        let mut app = test_app();
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("local")];
        app.state.active = Some(0);
        let workspace_id = app.state.workspaces[0].id.clone();
        let target_pane_id = app.state.workspaces[0].tabs[0].root_pane;

        let request_id = 99u64;
        app.register_pending_remote_split(
            request_id,
            PendingRemoteSplit {
                workspace_id,
                target_pane_id,
                direction: ratatui::layout::Direction::Horizontal,
                ratio: 0.5,
                focus: false,
                origin: crate::remote::federation::id::HostKey::new("remote-host", "s1"),
            },
        );

        let (events_tx, _events_rx) = tokio::sync::mpsc::channel::<crate::events::AppEvent>(4);
        let (out_tx, _out_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_output_tx, output_rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(4);
        let (clipboard_tx, _clipboard_rx) = tokio::sync::mpsc::unbounded_channel();
        let render_notify = std::sync::Arc::new(tokio::sync::Notify::new());
        let render_dirty = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        let pane_id = crate::layout::PaneId::alloc();
        let terminal_id = crate::terminal::TerminalId::alloc();
        let runtime = crate::terminal::TerminalRuntime::spawn_remote(
            pane_id,
            24,
            80,
            1 << 16,
            crate::terminal_theme::TerminalTheme::default(),
            None,
            terminal_id.to_string(),
            1,
            out_tx,
            output_rx,
            clipboard_tx,
            events_tx,
            render_notify,
            render_dirty,
        )
        .expect("spawn_remote must succeed for a fresh channel pair");
        let terminal =
            crate::terminal::TerminalState::new(terminal_id.clone(), std::path::PathBuf::from("/"));
        let pane_state = crate::pane::PaneState::new(terminal_id.clone());

        app.handle_federation_split_pane_ready(crate::events::FederationSplitPaneReady {
            request_id,
            origin: crate::remote::federation::id::HostKey::new("remote-host", "s1"),
            pane_id,
            terminal_id,
            terminal,
            runtime,
            pane_state,
        });

        let ws = &app.state.workspaces[0];
        ws.assert_invariants_for_test();
        assert!(
            ws.public_pane_number(pane_id).is_some(),
            "a split-materialized pane must get a public pane number, or it is unreachable \
             through the public pane-id API"
        );
    }

    /// Regression for the cross-mount response-spoofing finding: a
    /// `SplitPaneResponse` (delivered here as `AppEvent::
    /// FederationSplitPaneReady`) whose `request_id` matches a pending
    /// split but whose `origin` `HostKey` does not match the mount the
    /// request was actually sent to must be dropped — the pending entry
    /// stays registered (so the real response can still land later) and no
    /// pane is spliced into any workspace.
    #[cfg(unix)]
    #[tokio::test]
    async fn split_pane_response_from_a_different_mount_than_the_request_is_ignored() {
        let mut app = test_app();
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("local")];
        app.state.active = Some(0);
        let workspace_id = app.state.workspaces[0].id.clone();
        let target_pane_id = app.state.workspaces[0].tabs[0].root_pane;

        let request_id = 100u64;
        let real_origin = crate::remote::federation::id::HostKey::new("real-host", "s1");
        app.register_pending_remote_split(
            request_id,
            PendingRemoteSplit {
                workspace_id,
                target_pane_id,
                direction: ratatui::layout::Direction::Horizontal,
                ratio: 0.5,
                focus: false,
                origin: real_origin,
            },
        );
        assert!(app.pending_remote_splits.contains_key(&request_id));

        let (events_tx, _events_rx) = tokio::sync::mpsc::channel::<crate::events::AppEvent>(4);
        let (out_tx, _out_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_output_tx, output_rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(4);
        let (clipboard_tx, _clipboard_rx) = tokio::sync::mpsc::unbounded_channel();
        let render_notify = std::sync::Arc::new(tokio::sync::Notify::new());
        let render_dirty = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        let pane_id = crate::layout::PaneId::alloc();
        let terminal_id = crate::terminal::TerminalId::alloc();
        let runtime = crate::terminal::TerminalRuntime::spawn_remote(
            pane_id,
            24,
            80,
            1 << 16,
            crate::terminal_theme::TerminalTheme::default(),
            None,
            terminal_id.to_string(),
            1,
            out_tx,
            output_rx,
            clipboard_tx,
            events_tx,
            render_notify,
            render_dirty,
        )
        .expect("spawn_remote must succeed for a fresh channel pair");
        let terminal =
            crate::terminal::TerminalState::new(terminal_id.clone(), std::path::PathBuf::from("/"));
        let pane_state = crate::pane::PaneState::new(terminal_id.clone());

        // A second, attacker/buggy mount answers with the same request_id
        // but its own (different) origin.
        let spoofed_origin = crate::remote::federation::id::HostKey::new("evil-host", "s1");
        app.handle_federation_split_pane_ready(crate::events::FederationSplitPaneReady {
            request_id,
            origin: spoofed_origin,
            pane_id,
            terminal_id,
            terminal,
            runtime,
            pane_state,
        });

        assert!(
            app.pending_remote_splits.contains_key(&request_id),
            "a response from the wrong origin must not consume the pending entry"
        );
        assert!(
            app.state.workspaces[0].pane_state(pane_id).is_none(),
            "a response from the wrong origin must not splice a pane into any workspace"
        );
    }
}
