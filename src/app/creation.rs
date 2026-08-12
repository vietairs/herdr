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

pub(super) fn launch_cwd_for_terminal(
    terminal_id: &crate::terminal::TerminalId,
    terminals: &std::collections::HashMap<
        crate::terminal::TerminalId,
        crate::terminal::TerminalState,
    >,
    terminal_runtimes: &crate::terminal::TerminalRuntimeRegistry,
) -> Option<PathBuf> {
    terminal_runtimes
        .get(terminal_id)
        .and_then(|runtime| runtime.follow_cwd())
        .or_else(|| {
            terminals
                .get(terminal_id)
                .map(|terminal| terminal.cwd.clone())
        })
}

impl App {
    pub(super) fn seed_cwd_from_workspace(&self, ws_idx: usize) -> Option<PathBuf> {
        self.state
            .workspaces
            .get(ws_idx)?
            .resolved_identity_cwd_from(&self.state.terminals, &self.terminal_runtimes)
    }

    pub(super) fn launch_cwd_for_pane_in_workspace(
        &self,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
    ) -> Option<PathBuf> {
        let workspace = self.state.workspaces.get(ws_idx)?;
        let tab = workspace
            .tabs
            .get(workspace.find_tab_index_for_pane(pane_id)?)?;
        launch_cwd_for_terminal(
            tab.terminal_id(pane_id)?,
            &self.state.terminals,
            &self.terminal_runtimes,
        )
    }

    pub(super) fn focused_pane_cwd_in_workspace(&self, ws_idx: usize) -> Option<PathBuf> {
        let pane_id = self.state.workspaces.get(ws_idx)?.focused_pane_id()?;
        self.launch_cwd_for_pane_in_workspace(ws_idx, pane_id)
    }

    pub(super) fn resolve_new_terminal_cwd(&self, follow_cwd: Option<PathBuf>) -> PathBuf {
        resolve_new_terminal_cwd(&self.state.new_terminal_cwd, follow_cwd)
    }

    pub(super) fn workspace_creation_source(&self) -> Option<usize> {
        // A create confirmed from the name dialog belongs to the workspace
        // that was in focus when the dialog opened. Recomputing it at confirm
        // time would read `Mode::RenameWorkspace` — the modal's own mode — and
        // fall through to `active`, which is not the sidebar-selected
        // workspace the user started the create from. Pinned by id, not index,
        // because a resync can add or remove workspaces while the dialog is
        // open.
        if let Some(pinned) = &self.state.pending_workspace_create_source_workspace {
            if let Some(idx) = self.state.workspaces.iter().position(|ws| &ws.id == pinned) {
                return Some(idx);
            }
        }

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

    pub(super) fn begin_tui_workspace_create(&mut self, request_id: &'static str) {
        if self.state.prompt_new_workspace_name {
            let source_ws_idx = self.workspace_creation_source();
            let follow_cwd = source_ws_idx.and_then(|ws_idx| {
                self.focused_pane_cwd_in_workspace(ws_idx)
                    .or_else(|| self.seed_cwd_from_workspace(ws_idx))
            });
            let cwd = self.resolve_new_terminal_cwd(follow_cwd);
            // Pin the source so confirming the dialog creates from the same
            // workspace the user started on — including deciding whether the
            // create goes out over that workspace's mount.
            let source_workspace_id = source_ws_idx
                .and_then(|ws_idx| self.state.workspaces.get(ws_idx))
                .map(|ws| ws.id.clone());
            super::input::open_new_workspace_dialog(&mut self.state, cwd, source_workspace_id);
            return;
        }

        self.runtime_workspace_create(
            request_id,
            crate::api::schema::WorkspaceCreateParams {
                cwd: None,
                focus: true,
                label: None,
                env: Default::default(),
            },
        );
        self.state.mode = if self.state.active.is_some() {
            Mode::Terminal
        } else {
            Mode::Navigate
        };
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
            self.state.host_terminal_appearance,
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
            self.state.host_terminal_appearance,
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
                // Gap B fix (plans/260724-1536-federation-pane-close-sync):
                // `build_remote_pane` itself never indexes this pane, so
                // without this insert `remote_resync_pane_index` never
                // learns about ANY mount-time pane — only resync/split-
                // created panes did before this fix — and a serving-host
                // close of a mount-time pane silently no-ops in
                // `handle_federation_resync_pane_removed` (the previously
                // undiagnosed cause of "closing an original mount-time pane
                // doesn't tear down live on the client; only a remount
                // does").
                self.remote_resync_pane_index
                    .insert(first_pane.pane_id.clone(), root_pane_id);
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
                // Tab-identity index: without an entry here every pane a
                // later resync reveals for this same remote tab is spliced
                // into whichever local tab happens to be active, which is
                // what collapsed an N-tab remote workspace into one tab with
                // N splits. Mirrors the `remote_resync_pane_index` insert
                // above — mount-time entities need indexing too, not just
                // resync-created ones.
                if let Some(local_ws) = self.state.workspaces.get(this_ws_idx) {
                    let tab_number = local_ws.public_tab_number(tab_idx);
                    self.remote_resync_tab_index.insert(
                        tab_info.tab_id.clone(),
                        RemoteTabRef {
                            workspace_id: local_ws.id.clone(),
                            tab_number,
                            label: Some(tab_info.label.clone()),
                        },
                    );
                }

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
                        // Gap B fix (plans/260724-1536-federation-pane-close-
                        // sync): same reasoning as the root-pane insert
                        // above — index every mount-time pane, not just
                        // resync/split-created ones.
                        self.remote_resync_pane_index
                            .insert(pane_info.pane_id.clone(), split_pane_id);
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

    // Only reached from the `#[cfg(unix)]` federation response handlers below.
    #[cfg(unix)]
    fn take_pending_remote_split(&mut self, request_id: u64) -> Option<PendingRemoteSplit> {
        self.pending_remote_splits.remove(&request_id)
    }

    /// Drops every pending remote-split registration targeting one of the
    /// given (closing) workspace ids. Call this from
    /// `handle_federation_mount_ended` before those workspaces are removed,
    /// so a late/never-arriving `SplitPaneResponse` for a torn-down mount
    /// can no longer splice its pane into whatever workspace later reuses
    /// the same index, and the entry doesn't leak in the map forever.
    // Only reached from the `#[cfg(unix)]` federation response handlers below.
    #[cfg(unix)]
    pub(crate) fn purge_pending_remote_splits_for_workspaces(
        &mut self,
        workspace_ids: &std::collections::HashSet<String>,
    ) {
        self.pending_remote_splits
            .retain(|_, pending| !workspace_ids.contains(&pending.workspace_id));
    }

    /// Remembers the local pane a `ClosePaneRequest` was minted for, keyed by
    /// its `request_id` (`app/api/panes.rs::dispatch_remote_pane_close`, Gap
    /// A — plans/260724-1536-federation-pane-close-sync), so
    /// `handle_federation_close_pane_ready`/`_failed` can act on the right
    /// pane once the eventual response arrives. Mirrors
    /// `register_pending_remote_split`.
    pub(crate) fn register_pending_remote_close(
        &mut self,
        request_id: u64,
        pending: PendingRemoteClose,
    ) {
        self.pending_remote_closes.insert(request_id, pending);
    }

    // Only reached from the `#[cfg(unix)]` federation response handlers below.
    #[cfg(unix)]
    fn take_pending_remote_close(&mut self, request_id: u64) -> Option<PendingRemoteClose> {
        self.pending_remote_closes.remove(&request_id)
    }

    /// Drops every pending remote-close registration targeting one of the
    /// given (closing) workspace ids. Mirrors
    /// `purge_pending_remote_splits_for_workspaces` — call this alongside it
    /// from every site that removes a federated workspace, so a late/never-
    /// arriving `ClosePaneResponse` for a torn-down mount can no longer act
    /// on whatever later reuses the same slot.
    // Only reached from the `#[cfg(unix)]` federation response handlers below.
    #[cfg(unix)]
    pub(crate) fn purge_pending_remote_closes_for_workspaces(
        &mut self,
        workspace_ids: &std::collections::HashSet<String>,
    ) {
        self.pending_remote_closes
            .retain(|_, pending| !workspace_ids.contains(&pending.workspace_id));
    }

    /// Drops `remote_resync_pane_index` entries whose local pane belongs to
    /// one of the given (closing) workspace ids. Every mount-time pane now
    /// gets an entry in this index (`build_remote_pane`), so unmount must
    /// purge it the same way the sibling `purge_pending_remote_*_for_
    /// workspaces` helpers purge their maps — otherwise a stale
    /// `remote_pane_id -> local PaneId` entry leaks forever once the
    /// workspace it pointed into is gone. Unlike the sibling maps, entries
    /// here don't carry a `workspace_id`, so membership is resolved by
    /// walking the still-live workspaces' pane ids before they're removed.
    // Only reached from the `#[cfg(unix)]` federation response handlers below.
    #[cfg(unix)]
    pub(crate) fn purge_remote_resync_pane_index_for_workspaces(
        &mut self,
        workspace_ids: &std::collections::HashSet<String>,
    ) {
        let closing_pane_ids: std::collections::HashSet<PaneId> = self
            .state
            .workspaces
            .iter()
            .filter(|ws| workspace_ids.contains(&ws.id))
            .flat_map(|ws| ws.tabs.iter().flat_map(|tab| tab.layout.pane_ids()))
            .collect();
        self.remote_resync_pane_index
            .retain(|_, local_pane_id| !closing_pane_ids.contains(local_pane_id));
    }

    /// Tab-index sibling of `purge_remote_resync_pane_index_for_workspaces`:
    /// drops `remote_resync_tab_index` entries pointing into one of the
    /// given (closing) workspaces, so a remount to the same host cannot
    /// resolve a resync tab id onto a workspace/tab that no longer exists.
    /// Unlike the pane index these entries carry their workspace id
    /// directly, so no pane walk is needed.
    // Only reached from the `#[cfg(unix)]` federation response handlers below.
    #[cfg(unix)]
    pub(crate) fn purge_remote_resync_tab_index_for_workspaces(
        &mut self,
        workspace_ids: &std::collections::HashSet<String>,
    ) {
        self.remote_resync_tab_index
            .retain(|_, tab_ref| !workspace_ids.contains(&tab_ref.workspace_id));
    }

    /// Workspace-level sibling of the two purge helpers above: drops
    /// `remote_resync_workspace_index` entries for one of the given
    /// (closing) workspaces. Keyed by the namespaced remote workspace id,
    /// which is exactly the local `Workspace::id` those sets carry.
    // Only reached from the `#[cfg(unix)]` federation response handlers below.
    #[cfg(unix)]
    pub(crate) fn purge_remote_resync_workspace_index_for_workspaces(
        &mut self,
        workspace_ids: &std::collections::HashSet<String>,
    ) {
        self.remote_resync_workspace_index
            .retain(|workspace_id, _| !workspace_ids.contains(workspace_id));
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
            remote_pane_id,
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
        // C1/M1 fix (plans/260722-1327 review): reverse-index this
        // split-created pane the same way a resync-created pane is indexed,
        // so a later resync-driven removal of it can find and tear it down
        // (`handle_federation_resync_pane_removed`).
        self.remote_resync_pane_index
            .insert(remote_pane_id, pane_id);
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

        self.render_dirty.request_generic();
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
        self.render_dirty.request_generic();
        self.render_notify.notify_one();
    }

    /// `AppEvent::FederationClosePaneReady` handler (Gap A,
    /// plans/260724-1536-federation-pane-close-sync): the serving host
    /// confirmed it closed the pane this mount asked it to close
    /// (`ClosePaneRequest` -> `ClosePaneResponse::Closed`); tear the local
    /// mirror pane down the same way `handle_federation_resync_pane_removed`
    /// does — the remote already made the real close decision, so unlike
    /// the interactive `pane.close` API path this never asks for close
    /// confirmation. Idempotent (Predict risk 3): if the pane was already
    /// torn down by a resync in the meantime (e.g. the serving host's close
    /// also showed up as a resync-removed pane before this response
    /// arrived), `find_pane` returns `None` and this is a silent no-op
    /// rather than a panic or a double `PaneClosed` emission.
    #[cfg(unix)]
    pub(crate) fn handle_federation_close_pane_ready(
        &mut self,
        request_id: u64,
        origin: crate::remote::federation::id::HostKey,
    ) {
        if let Some(pending) = self.pending_remote_closes.get(&request_id) {
            if pending.origin != origin {
                tracing::warn!(
                    request_id,
                    expected_origin = %pending.origin,
                    got_origin = %origin,
                    "dropping a close-pane response from a mount that did not \
                     originate this request"
                );
                return;
            }
        }

        let Some(pending) = self.take_pending_remote_close(request_id) else {
            tracing::warn!(
                request_id,
                "remote close confirmed for an unknown/stale request; nothing to tear down"
            );
            return;
        };

        let RemoteCloseTarget::Pane(pane_id) = pending.target else {
            tracing::warn!(
                request_id,
                "a ClosePaneResponse answered a pending entry that was not a pane close"
            );
            return;
        };

        let Some((ws_idx, _)) = self.find_pane(pane_id) else {
            // Already gone — e.g. a resync
            // (`handle_federation_resync_pane_removed`) tore it down first
            // while this response was in flight. Idempotent success, not an
            // error.
            return;
        };

        let workspace_id = self.public_workspace_id(ws_idx);
        let public_pane_id = self.public_pane_id(ws_idx, pane_id);
        let layout_update_target = self.layout_update_target_after_pane_removal(ws_idx, pane_id);
        let terminal_id = self.state.terminal_id_for_pane(ws_idx, pane_id);

        let should_close_workspace = {
            let Some(ws) = self.state.workspaces.get_mut(ws_idx) else {
                return;
            };
            ws.close_pane(pane_id)
        };
        self.state.remove_plugin_pane_records([pane_id]);

        if should_close_workspace {
            // One workspace, not the mount's whole federation group — see
            // `close_single_workspace_at`.
            self.close_single_workspace_at(ws_idx);
            self.shutdown_detached_terminal_runtimes();
            if let Some(public_pane_id) = public_pane_id {
                self.emit_event(EventEnvelope {
                    event: EventKind::PaneClosed,
                    data: EventData::PaneClosed {
                        pane_id: public_pane_id,
                        workspace_id,
                    },
                });
            }
        } else {
            if let Some(terminal_id) = terminal_id {
                self.state.remove_unattached_terminal_ids([terminal_id]);
            }
            self.shutdown_detached_terminal_runtimes();
            self.schedule_session_save();
            if let Some(public_pane_id) = public_pane_id {
                self.emit_event(EventEnvelope {
                    event: EventKind::PaneClosed,
                    data: EventData::PaneClosed {
                        pane_id: public_pane_id,
                        workspace_id,
                    },
                });
            }
            if let Some((ws_idx, tab_idx)) = layout_update_target {
                self.emit_layout_updated_event(ws_idx, tab_idx);
            }
        }

        self.render_dirty.request_generic();
        self.render_notify.notify_one();
    }

    /// `AppEvent::FederationClosePaneFailed` handler: the remote host
    /// rejected (or the mount could not carry) an earlier `ClosePaneRequest`
    /// — drop the pending context and surface it exactly like a failed
    /// remote split (`handle_federation_split_pane_failed`), without
    /// touching layout. A "pane not found" style failure is folded into an
    /// idempotent `ClosePaneReady` instead of reaching this handler at all
    /// (see `client.rs`'s `ClosePaneResponse::Failed` handling doc comment)
    /// — Predict risk 3's retry/duplicate-click safety requirement.
    #[cfg(unix)]
    pub(crate) fn handle_federation_close_pane_failed(
        &mut self,
        request_id: u64,
        reason: String,
        origin: crate::remote::federation::id::HostKey,
    ) {
        if let Some(pending) = self.pending_remote_closes.get(&request_id) {
            if pending.origin != origin {
                tracing::warn!(
                    request_id,
                    expected_origin = %pending.origin,
                    got_origin = %origin,
                    "dropping a close-pane failure from a mount that did not \
                     originate this request"
                );
                return;
            }
        }
        self.take_pending_remote_close(request_id);
        tracing::warn!(request_id, %reason, "remote close failed");
        self.raise_remote_close_failed_toast("remote close failed", reason);
        self.render_dirty.request_generic();
        self.render_notify.notify_one();
    }

    /// Raises a "remote close failed" toast/notification, following the
    /// user's configured `ToastDelivery`. Extracted from
    /// `handle_federation_close_pane_failed` so its workspace/tab
    /// counterparts (`handle_federation_workspace_close_failed`,
    /// `handle_federation_tab_close_failed`) share the exact same delivery
    /// dispatch instead of re-deriving it.
    #[cfg(unix)]
    pub(crate) fn raise_remote_close_failed_toast(&mut self, title: &str, reason: String) {
        self.raise_remote_close_toast(super::ToastKind::NeedsAttention, title, reason);
    }

    /// Delivery dispatch shared by every remote-close toast. The only thing
    /// that varies between them is the [`ToastKind`] and the title: the
    /// request-accepted notice is informational, the rejection needs
    /// attention, and both must honour the same configured delivery channel.
    ///
    /// Not gated to Unix even though federation dispatch is: the
    /// request-accepted toast is raised from the platform-neutral TUI close
    /// path, and nothing in this delivery dispatch is Unix-specific.
    pub(crate) fn raise_remote_close_toast(
        &mut self,
        kind: super::ToastKind,
        title: &str,
        reason: String,
    ) {
        match self.state.toast_config.delivery {
            crate::config::ToastDelivery::Herdr => {
                self.state.toast = Some(crate::app::state::ToastNotification {
                    kind,
                    title: title.to_string(),
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
                let _ = notify(title, Some(&reason));
            }
            _ => {}
        }
    }

    /// `AppEvent::FederationWorkspaceCloseReady` handler
    /// (`workspace.close_remote`): the serving host confirmed it closed the
    /// workspace this mount asked it to close (`WorkspaceCloseRequest` ->
    /// `WorkspaceCloseResponse::Closed`); tear the local mirror workspace
    /// down. Idempotent, same reasoning as
    /// `handle_federation_close_pane_ready`: if the workspace was already
    /// retired locally by the time this arrives (e.g. a racing resync, or
    /// `handle_federation_mount_ended`), the id lookup below finds nothing
    /// and this is a silent no-op rather than a panic or a double
    /// `WorkspaceClosed` emission.
    #[cfg(unix)]
    pub(crate) fn handle_federation_workspace_close_ready(
        &mut self,
        request_id: u64,
        origin: crate::remote::federation::id::HostKey,
    ) {
        // Both mismatch checks peek before taking: evicting the pending entry
        // first would drop an unrelated in-flight close, whose genuine
        // response then finds nothing and leaves its mirror behind forever.
        if let Some(pending) = self.pending_remote_closes.get(&request_id) {
            if pending.origin != origin {
                tracing::warn!(
                    request_id,
                    expected_origin = %pending.origin,
                    got_origin = %origin,
                    "dropping a workspace-close response from a mount that did not \
                     originate this request"
                );
                return;
            }
            if !matches!(pending.target, RemoteCloseTarget::Workspace) {
                tracing::warn!(
                    request_id,
                    "a WorkspaceCloseResponse answered a pending entry that was not a workspace \
                     close"
                );
                return;
            }
        }

        let Some(pending) = self.take_pending_remote_close(request_id) else {
            tracing::warn!(
                request_id,
                "remote workspace close confirmed for an unknown/stale request; nothing to \
                 tear down"
            );
            return;
        };

        let workspace_id = pending.workspace_id;
        let mount_origin = pending.origin;
        let Some(ws_idx) = self
            .state
            .workspaces
            .iter()
            .position(|ws| ws.id == workspace_id)
        else {
            // Already gone — idempotent success, not an error. Still settle
            // the mount: whatever removed the workspace first may have been
            // a path that does not end an emptied mount.
            self.end_federation_mount_if_no_mirrors_remain(&mount_origin);
            return;
        };

        let workspace = self.workspace_info(ws_idx);
        let pane_ids: Vec<PaneId> = self.state.workspaces[ws_idx]
            .tabs
            .iter()
            .flat_map(|tab| tab.layout.pane_ids())
            .collect();
        let closing_ids: std::collections::HashSet<String> =
            std::iter::once(workspace_id.clone()).collect();
        self.purge_federation_state_for_workspaces(&closing_ids);
        self.close_single_workspace_at(ws_idx);
        self.state.remove_plugin_pane_records(pane_ids);
        self.shutdown_detached_terminal_runtimes();
        self.end_federation_mount_if_no_mirrors_remain(&mount_origin);
        self.schedule_session_save();
        self.emit_event(EventEnvelope {
            event: EventKind::WorkspaceClosed,
            data: EventData::WorkspaceClosed {
                workspace_id,
                workspace: Some(workspace),
            },
        });

        self.render_dirty.request_generic();
        self.render_notify.notify_one();
    }

    /// `AppEvent::FederationWorkspaceCloseFailed` handler: the remote host
    /// rejected an earlier `WorkspaceCloseRequest` — drop the pending
    /// context and surface it, without touching layout. Same shape/reasoning
    /// as `handle_federation_close_pane_failed`.
    #[cfg(unix)]
    pub(crate) fn handle_federation_workspace_close_failed(
        &mut self,
        request_id: u64,
        reason: String,
        origin: crate::remote::federation::id::HostKey,
    ) {
        if let Some(pending) = self.pending_remote_closes.get(&request_id) {
            if pending.origin != origin {
                tracing::warn!(
                    request_id,
                    expected_origin = %pending.origin,
                    got_origin = %origin,
                    "dropping a workspace-close failure from a mount that did not \
                     originate this request"
                );
                return;
            }
        }
        self.take_pending_remote_close(request_id);
        tracing::warn!(request_id, %reason, "remote workspace close failed");
        self.raise_remote_close_failed_toast("remote workspace close failed", reason);
        self.render_dirty.request_generic();
        self.render_notify.notify_one();
    }

    /// `AppEvent::FederationTabCloseReady` handler (`tab.close_remote`): the
    /// serving host confirmed it closed the tab this mount asked it to close
    /// (`TabCloseRequest` -> `TabCloseResponse::Closed`); tear the local
    /// mirror tab down. If it was the workspace's last tab, closing it
    /// closes the whole workspace — the same fallback `handle_tab_close`
    /// uses for a locally initiated close of a non-federated workspace's
    /// last tab. Idempotent: if the tab was already retired locally by the
    /// time this arrives (a racing resync, or the whole workspace already
    /// gone), `parse_tab_id` finds nothing and this is a silent no-op.
    #[cfg(unix)]
    pub(crate) fn handle_federation_tab_close_ready(
        &mut self,
        request_id: u64,
        origin: crate::remote::federation::id::HostKey,
    ) {
        // Both mismatch checks peek before taking: evicting the pending entry
        // first would drop an unrelated in-flight close, whose genuine
        // response then finds nothing and leaves its mirror behind forever.
        if let Some(pending) = self.pending_remote_closes.get(&request_id) {
            if pending.origin != origin {
                tracing::warn!(
                    request_id,
                    expected_origin = %pending.origin,
                    got_origin = %origin,
                    "dropping a tab-close response from a mount that did not originate this \
                     request"
                );
                return;
            }
            if !matches!(pending.target, RemoteCloseTarget::Tab(_)) {
                tracing::warn!(
                    request_id,
                    "a TabCloseResponse answered a pending entry that was not a tab close"
                );
                return;
            }
        }

        let Some(pending) = self.take_pending_remote_close(request_id) else {
            tracing::warn!(
                request_id,
                "remote tab close confirmed for an unknown/stale request; nothing to tear down"
            );
            return;
        };

        let mount_origin = pending.origin;
        let RemoteCloseTarget::Tab(tab_id) = pending.target else {
            tracing::warn!(
                request_id,
                "a TabCloseResponse answered a pending entry that was not a tab close"
            );
            return;
        };

        let Some((ws_idx, tab_idx)) = self.parse_tab_id(&tab_id) else {
            // Already gone — idempotent success, not an error. Still settle
            // the mount, exactly as the workspace-close twin does: whatever
            // removed the tab (and possibly its workspace) first may have been
            // a path that does not end an emptied mount.
            self.end_federation_mount_if_no_mirrors_remain(&mount_origin);
            return;
        };

        let workspace_id = self.public_workspace_id(ws_idx);
        let pane_ids: Vec<PaneId> = self.state.workspaces[ws_idx]
            .tabs
            .get(tab_idx)
            .map(|tab| tab.layout.pane_ids())
            .unwrap_or_default();
        let terminal_ids = self.state.terminal_ids_for_tab(ws_idx, tab_idx);
        // Prune the mirror-namespaced `remote_resync_tab_index` entry the
        // same way `handle_federation_resync_tab_removed` does, computed
        // before `close_tab` below invalidates `tab_idx` — otherwise a
        // later resync pane for this now-closed remote tab would resolve
        // through the stale entry and rebuild a duplicate local tab.
        if let Some(tab_number) = self.state.workspaces[ws_idx].public_tab_number(tab_idx) {
            self.remote_resync_tab_index.retain(|_, tab_ref| {
                !(tab_ref.workspace_id == workspace_id && tab_ref.tab_number == Some(tab_number))
            });
        }

        let closed = self
            .state
            .workspaces
            .get_mut(ws_idx)
            .is_some_and(|ws| ws.close_tab(tab_idx));

        if closed {
            self.state.remove_plugin_pane_records(pane_ids);
            self.state.remove_unattached_terminal_ids(terminal_ids);
            self.shutdown_detached_terminal_runtimes();
            self.schedule_session_save();
            self.emit_event(EventEnvelope {
                event: EventKind::TabClosed,
                data: EventData::TabClosed {
                    tab_id,
                    workspace_id,
                },
            });
        } else {
            // `close_tab` only refuses on the last tab (the tab_idx above
            // was already validated by `parse_tab_id`) — the workspace-close
            // fallback, mirroring `handle_tab_close`'s own last-tab branch.
            let workspace = self.workspace_info(ws_idx);
            let closing_ids: std::collections::HashSet<String> =
                std::iter::once(workspace_id.clone()).collect();
            self.purge_federation_state_for_workspaces(&closing_ids);
            self.close_single_workspace_at(ws_idx);
            self.state.remove_plugin_pane_records(pane_ids);
            self.shutdown_detached_terminal_runtimes();
            // The last tab took its workspace with it, so this may have been
            // the mount's last mirror.
            self.end_federation_mount_if_no_mirrors_remain(&mount_origin);
            self.schedule_session_save();
            self.emit_event(EventEnvelope {
                event: EventKind::TabClosed,
                data: EventData::TabClosed {
                    tab_id,
                    workspace_id: workspace_id.clone(),
                },
            });
            self.emit_event(EventEnvelope {
                event: EventKind::WorkspaceClosed,
                data: EventData::WorkspaceClosed {
                    workspace_id,
                    workspace: Some(workspace),
                },
            });
        }

        self.render_dirty.request_generic();
        self.render_notify.notify_one();
    }

    /// `AppEvent::FederationTabCloseFailed` handler: the remote host
    /// rejected an earlier `TabCloseRequest` — drop the pending context and
    /// surface it, without touching layout. Same shape/reasoning as
    /// `handle_federation_close_pane_failed`.
    #[cfg(unix)]
    pub(crate) fn handle_federation_tab_close_failed(
        &mut self,
        request_id: u64,
        reason: String,
        origin: crate::remote::federation::id::HostKey,
    ) {
        if let Some(pending) = self.pending_remote_closes.get(&request_id) {
            if pending.origin != origin {
                tracing::warn!(
                    request_id,
                    expected_origin = %pending.origin,
                    got_origin = %origin,
                    "dropping a tab-close failure from a mount that did not originate this \
                     request"
                );
                return;
            }
        }
        self.take_pending_remote_close(request_id);
        tracing::warn!(request_id, %reason, "remote tab close failed");
        self.raise_remote_close_failed_toast("remote tab close failed", reason);
        self.render_dirty.request_generic();
        self.render_notify.notify_one();
    }

    /// Drops a pending remote tab-close registration targeting `tab_id`
    /// (the canonical public tab id stored in `RemoteCloseTarget::Tab`).
    /// Purge-gap fix: `purge_federation_state_for_workspaces` keys on
    /// WORKSPACE id, so a single tab removed WITHOUT its workspace closing
    /// purges no pending tab close there. Call this whenever a tab is
    /// retired that way — `handle_federation_resync_tab_removed` (a racing
    /// resync beat the ack) and `app/api/tabs.rs::handle_tab_close` (a plain
    /// LOCAL close on a mirror beat the ack; `tab.close_remote` is a
    /// separate opt-in verb, so nothing stops both being in flight for the
    /// same tab) — so a late/never-arriving `TabCloseResponse` for a
    /// torn-down tab can no longer act on whatever later reuses the same
    /// slot. `pub(crate)` (not module-private) so `app/api/tabs.rs` can call
    /// it too; `#[cfg(not(unix))]` gets a no-op twin below so that call site
    /// stays ungated, mirroring `purge_federation_state_for_workspaces`.
    #[cfg(unix)]
    pub(crate) fn purge_pending_remote_close_for_tab(&mut self, tab_id: &str) {
        self.pending_remote_closes.retain(|_, pending| {
            !matches!(&pending.target, RemoteCloseTarget::Tab(pending_tab_id) if pending_tab_id == tab_id)
        });
    }

    /// No mount can exist on a target without the federation mount
    /// primitives, so there is never any pending remote close to purge
    /// there. See the `#[cfg(unix)]` twin above.
    #[cfg(not(unix))]
    pub(crate) fn purge_pending_remote_close_for_tab(&mut self, _tab_id: &str) {}

    /// `AppEvent::FederationResyncPaneCreated` handler (post-mount pane
    /// mirroring, part 2 — plans/260722-1327): the drive task already built
    /// the new pane's real `TerminalRuntime`; place it in the already-mounted
    /// workspace under the tab it actually belongs to on the remote.
    ///
    /// The pane's remote `tab_id` (already on the wire as `PaneInfo.tab_id`)
    /// resolves through `remote_resync_tab_index`: a tab this mount already
    /// materialized takes the pane as a split, an unseen tab gets a brand new
    /// local `Tab`. The earlier behavior — always splitting into
    /// `Workspace::active_tab` — is what made an N-tab remote workspace
    /// render as one tab with N splits.
    #[cfg(unix)]
    pub(crate) fn handle_federation_resync_pane_created(
        &mut self,
        ready: crate::events::FederationResyncPaneCreated,
    ) {
        let crate::events::FederationResyncPaneCreated {
            origin,
            workspace_id,
            tab_id,
            pane_id,
            local_pane_id,
            terminal_id,
            terminal,
            runtime,
            pane_state,
        } = ready;

        let Some(ws_idx) = self
            .state
            .workspaces
            .iter()
            .position(|ws| ws.id == workspace_id)
        else {
            // The pane may belong to a remote workspace this mount has never
            // materialized — one created on the serving host after mount, or
            // by this client's own `WorkspaceCreateRequest`. A local
            // `Workspace` needs a pane to exist, so this first pane is what
            // brings it into being (the same staged shape the tab branch
            // below uses one level down).
            self.materialize_resync_workspace_from_pane(
                origin,
                workspace_id,
                tab_id,
                pane_id,
                local_pane_id,
                terminal_id,
                terminal,
                runtime,
                pane_state,
            );
            return;
        };

        if !self.workspace_matches_federation_origin(ws_idx, &origin) {
            tracing::warn!(
                %workspace_id,
                expected_origin = %origin,
                "dropping a resync-created pane whose mount origin does not match its \
                 workspace's federation origin"
            );
            return;
        }

        // Resolve the remote tab to a live local tab index, if this mount
        // already has one. A stale index entry (its tab was closed locally,
        // so its number no longer resolves) degrades to the "unknown tab"
        // branch and is overwritten below, so a closed tab can never leave
        // the index pointing at an unrelated tab.
        let known_tab = self
            .remote_resync_tab_index
            .get(&tab_id)
            .and_then(|tab_ref| tab_ref.tab_number);
        let existing_tab_idx = known_tab.and_then(|number| {
            self.state.workspaces[ws_idx]
                .tabs
                .iter()
                .position(|tab| tab.number == number)
        });

        let moved = crate::workspace::MovedPane {
            pane_id: local_pane_id,
            pane_state,
        };
        let ws = &mut self.state.workspaces[ws_idx];
        let (tab_idx, created_tab) = match existing_tab_idx {
            Some(tab_idx) => {
                // Split off the tab's own last pane, not the workspace's
                // focused pane — the focused pane may well live in a
                // different tab now that panes land in their real tab.
                let Some(target_pane_id) = ws
                    .tabs
                    .get(tab_idx)
                    .and_then(|tab| tab.layout.pane_ids().last().copied())
                else {
                    tracing::warn!(
                        %workspace_id,
                        %tab_id,
                        "resync revealed a new remote pane but its target tab has no pane \
                         to split from"
                    );
                    return;
                };
                if ws
                    .insert_moved_pane_into_tab(
                        tab_idx,
                        target_pane_id,
                        moved,
                        ratatui::layout::Direction::Horizontal,
                        0.5,
                    )
                    .is_err()
                {
                    tracing::warn!(
                        %workspace_id,
                        %tab_id,
                        "resync revealed a new remote pane but it could not be inserted \
                         into its tab"
                    );
                    return;
                }
                (tab_idx, false)
            }
            None => {
                // Same primitive + event path mount-time materialization
                // uses for a non-root remote tab, so a tab discovered after
                // mount is indistinguishable from one present at mount.
                let label = self
                    .remote_resync_tab_index
                    .get(&tab_id)
                    .and_then(|tab_ref| tab_ref.label.clone());
                let ws = &mut self.state.workspaces[ws_idx];
                let tab_idx = ws.create_tab_from_existing_pane(
                    moved,
                    label,
                    self.event_tx.clone(),
                    self.render_notify.clone(),
                    self.render_dirty.clone(),
                );
                (tab_idx, true)
            }
        };

        self.terminal_runtimes.insert(terminal_id.clone(), runtime);
        self.state.terminals.insert(terminal_id, terminal);
        self.state.remove_alias_shadowed_by_new_pane(local_pane_id);
        self.remote_resync_pane_index.insert(pane_id, local_pane_id);
        let tab_number = self.state.workspaces[ws_idx].public_tab_number(tab_idx);
        let local_workspace_id = self.state.workspaces[ws_idx].id.clone();
        let entry = self
            .remote_resync_tab_index
            .entry(tab_id)
            .or_insert_with(|| RemoteTabRef {
                workspace_id: local_workspace_id,
                tab_number: None,
                label: None,
            });
        entry.tab_number = tab_number;
        self.schedule_session_save();

        if created_tab {
            // Emits TabCreated + PaneCreated + LayoutUpdated together.
            self.emit_tab_created_events(ws_idx, tab_idx);
        } else {
            if let Some(pane) = self.pane_info(ws_idx, local_pane_id) {
                self.emit_event(EventEnvelope {
                    event: EventKind::PaneCreated,
                    data: EventData::PaneCreated { pane },
                });
            }
            self.emit_layout_updated_event(ws_idx, tab_idx);
        }

        self.render_dirty.request_generic();
        self.render_notify.notify_one();
    }

    /// Builds a brand new local `Workspace` around the first pane a resync
    /// reported for a remote workspace this mount has never materialized.
    /// Split out of `handle_federation_resync_pane_created` because
    /// `Workspace::from_existing_pane` consumes the `MovedPane`, so the
    /// new-workspace case cannot fall through into the existing-workspace
    /// one.
    ///
    /// Deliberately reuses exactly what `materialize_federation_mount` uses
    /// for a mount-time workspace — `Workspace::from_existing_pane`, the
    /// mirror's own namespaced id as `Workspace::id`, the
    /// `federation:<host_key>` `worktree_space` membership, and
    /// `emit_workspace_open_events` — so a workspace discovered after mount
    /// is indistinguishable from one present at mount, including to the
    /// federation-origin classification that reads `Workspace::id`.
    #[cfg(unix)]
    #[allow(clippy::too_many_arguments)] // the destructured `FederationResyncPaneCreated` payload, minus the fields the caller already consumed
    fn materialize_resync_workspace_from_pane(
        &mut self,
        origin: crate::remote::federation::id::HostKey,
        workspace_id: String,
        tab_id: String,
        pane_id: String,
        local_pane_id: PaneId,
        terminal_id: crate::terminal::TerminalId,
        terminal: crate::terminal::TerminalState,
        runtime: crate::terminal::TerminalRuntime,
        pane_state: crate::pane::PaneState,
    ) {
        let Some(workspace_ref) = self.remote_resync_workspace_index.get(&workspace_id) else {
            tracing::warn!(
                %workspace_id,
                "resync revealed a new remote pane but its workspace is neither materialized \
                 nor announced"
            );
            return;
        };
        if workspace_ref.origin != origin {
            tracing::warn!(
                %workspace_id,
                expected_origin = %workspace_ref.origin,
                got_origin = %origin,
                "dropping a resync-created pane whose mount origin does not match the mount \
                 that announced its workspace"
            );
            return;
        }
        let workspace_label = workspace_ref.label.clone();
        let tab_label = self
            .remote_resync_tab_index
            .get(&tab_id)
            .and_then(|tab_ref| tab_ref.label.clone());

        let moved = crate::workspace::MovedPane {
            pane_id: local_pane_id,
            pane_state,
        };
        let mut workspace = Workspace::from_existing_pane(
            Some(workspace_label),
            tab_label,
            terminal.cwd.clone(),
            moved,
            self.event_tx.clone(),
            self.render_notify.clone(),
            self.render_dirty.clone(),
        );
        // Same reasoning as `materialize_federation_mount`: the federation
        // badge/grouping classifies purely from `Workspace::id`'s
        // `r:<host_key>:` prefix, so the mirror's namespaced id must survive
        // verbatim rather than the fresh local id `from_existing_pane` mints.
        workspace.id = workspace_id.clone();
        workspace.worktree_space = Some(WorktreeSpaceMembership {
            key: format!("federation:{}", origin.as_str()),
            label: origin.as_str().to_string(),
            repo_root: PathBuf::new(),
            checkout_path: PathBuf::new(),
            is_linked_worktree: false,
        });
        self.state.workspaces.push(workspace);
        let ws_idx = self.state.workspaces.len() - 1;

        self.terminal_runtimes.insert(terminal_id.clone(), runtime);
        self.state.terminals.insert(terminal_id, terminal);
        self.state.remove_alias_shadowed_by_new_pane(local_pane_id);
        self.remote_resync_pane_index.insert(pane_id, local_pane_id);
        // `from_existing_pane` always seeds exactly one tab at index 0.
        let tab_number = self.state.workspaces[ws_idx].public_tab_number(0);
        let entry = self
            .remote_resync_tab_index
            .entry(tab_id)
            .or_insert_with(|| RemoteTabRef {
                workspace_id: workspace_id.clone(),
                tab_number: None,
                label: None,
            });
        entry.workspace_id = workspace_id.clone();
        entry.tab_number = tab_number;
        // The announcement did its job; the live `Workspace` is now the
        // record, found by id.
        self.remote_resync_workspace_index.remove(&workspace_id);

        self.emit_workspace_open_events(ws_idx);
        // Focus it only if this client asked the remote host to create it and
        // asked for focus (`App::handle_federation_workspace_create_accepted`
        // is the only writer of this set). A workspace the remote user created
        // out of band arrives through exactly this path and must never pull
        // the local user out of what they were doing.
        if self.pending_remote_workspace_focus.remove(&workspace_id) {
            self.state.switch_workspace(ws_idx);
        }
        self.schedule_session_save();
        self.render_dirty.request_generic();
        self.render_notify.notify_one();
    }

    /// Closes exactly one workspace, never its worktree/federation group.
    ///
    /// `AppState::close_selected_workspace` deliberately closes every
    /// workspace sharing the selected one's `worktree_space` key
    /// (`AppState::close_indices_for`), which is right for a repo's worktree
    /// group and for a whole-mount teardown. But every workspace one
    /// federation mount materializes shares that mount's single
    /// `federation:<host_key>` space key, so retiring one remote workspace —
    /// or its last pane — would otherwise take every other workspace of the
    /// same mount down with it. Detaching the doomed workspace from its space
    /// first makes `close_indices_for` fall back to the single index; the
    /// membership dies with the workspace either way.
    ///
    /// Ungated for the same reason `dispatch_remote_workspace_create` is: the
    /// `tab.close` caller in `api/tabs.rs` is ungated, and on a target without
    /// the mount primitives no workspace ever carries a `federation:` space
    /// key, so that caller never reaches this.
    pub(crate) fn close_single_workspace_at(&mut self, ws_idx: usize) {
        // Validate before moving the selection: an out-of-range index must
        // leave `selected` pointing where it already did, never at a slot that
        // does not exist.
        let Some(ws) = self.state.workspaces.get_mut(ws_idx) else {
            return;
        };
        ws.worktree_space = None;
        self.state.selected = ws_idx;
        self.state.close_selected_workspace();
    }

    /// Ends a federation mount once none of its mirrored workspaces remain.
    ///
    /// The mount is what owns the link, the drive task and the
    /// `remote_mirrors` entry; retiring the last workspace that used it
    /// without ending it leaves all three alive with nothing visible behind
    /// them, and a later remount reports the host as already live. The local
    /// `workspace.close` verb does this inline; the confirmed-close handlers
    /// must do the same, or the two verbs diverge on their last workspace.
    #[cfg(unix)]
    fn end_federation_mount_if_no_mirrors_remain(
        &mut self,
        host_key: &crate::remote::federation::id::HostKey,
    ) {
        let mirrors_remain = (0..self.state.workspaces.len())
            .any(|idx| self.federation_host_key_for_workspace(idx).as_ref() == Some(host_key));
        if !mirrors_remain {
            self.state.end_federation_mount(host_key);
        }
    }

    /// Windows counterparts of the two federation close helpers below.
    /// `server::federation_actor` is compiled on every platform (unlike
    /// `federation_accept`, which is Unix-only), so its command arms must
    /// resolve these names on Windows too. Federation never actually serves
    /// there, so nothing can reach them; they refuse rather than pretend to
    /// close something. Same shape as `nudge_child_redraw`'s cfg pair in that
    /// module.
    #[cfg(not(unix))]
    pub(crate) fn close_federation_target_workspace(
        &mut self,
        _target_workspace_id: &str,
    ) -> Result<(), String> {
        Err("federation is not supported on this platform".to_string())
    }

    #[cfg(not(unix))]
    pub(crate) fn close_federation_target_tab(
        &mut self,
        _target_tab_id: &str,
    ) -> Result<(), String> {
        Err("federation is not supported on this platform".to_string())
    }

    /// Closes exactly one LOCAL workspace on this host in response to a
    /// federated peer's `WorkspaceCloseRequest` — the serving-host half of
    /// close forwarding for the multi-workspace case. Deliberately does NOT
    /// go through the `workspace.close` JSON-API method
    /// (`handle_workspace_close`): on this host the target is always a
    /// workspace it owns locally, so that handler's `federation_host_key_for_workspace`
    /// lookup returns `None`, `retire_one_only` is `false`, and it falls into
    /// `AppState::close_selected_workspace()`'s worktree-group close — which
    /// would take down every sibling workspace sharing the target's
    /// `worktree_space` key for one remote peer's single-workspace request.
    /// Uses `close_single_workspace_at` instead, which detaches the target
    /// from its worktree-space membership before closing so exactly one
    /// workspace ever comes down, mirroring the resync handlers below
    /// (`handle_federation_resync_workspace_removed`) rather than the local
    /// JSON-API close path.
    ///
    /// Stronger than `ClosePane`'s fixed-internal-request-id trust boundary:
    /// this never goes through `handle_api_request` at all, so there is no
    /// request id for a remote peer's close to be mistaken for a local user
    /// gesture, and no confirmation prompt it could trigger on this host's
    /// own session.
    #[cfg(unix)]
    pub(crate) fn close_federation_target_workspace(
        &mut self,
        target_workspace_id: &str,
    ) -> Result<(), String> {
        let Some(ws_idx) = self.parse_federation_workspace_id(target_workspace_id) else {
            return Err("workspace not found".to_string());
        };
        let workspace_id = self.public_workspace_id(ws_idx);
        let workspace = self.workspace_info(ws_idx);
        let pane_ids = self
            .state
            .workspaces
            .get(ws_idx)
            .map(|ws| {
                ws.tabs
                    .iter()
                    .flat_map(|tab| tab.layout.pane_ids())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let closing_ids: std::collections::HashSet<String> =
            std::iter::once(workspace_id.clone()).collect();
        self.purge_federation_state_for_workspaces(&closing_ids);

        self.close_single_workspace_at(ws_idx);
        self.state.remove_plugin_pane_records(pane_ids);
        self.shutdown_detached_terminal_runtimes();
        self.schedule_session_save();
        self.emit_event(EventEnvelope {
            event: EventKind::WorkspaceClosed,
            data: EventData::WorkspaceClosed {
                workspace_id,
                workspace: Some(workspace),
            },
        });
        Ok(())
    }

    /// Closes exactly one LOCAL tab on this host in response to a federated
    /// peer's `TabCloseRequest` — the serving-host half of close forwarding
    /// for the multi-tab case. Deliberately does NOT go through the
    /// `tab.close` JSON-API method (`handle_tab_close`): when the target is
    /// its workspace's last tab and that workspace shares a worktree-group
    /// with a sibling, that handler's `AppState::confirm_implicit_worktree_group_close`
    /// sets `mode = Mode::ConfirmClose` (and `selected`) BEFORE refusing —
    /// mutating this host's own UI state in response to a remote peer's
    /// request, which the fixed internal request id below exists precisely
    /// to prevent. This path always retires exactly the target tab (or, when
    /// it is the last tab, exactly the target workspace via
    /// `close_single_workspace_at`) and never asks for confirmation.
    #[cfg(unix)]
    pub(crate) fn close_federation_target_tab(
        &mut self,
        target_tab_id: &str,
    ) -> Result<(), String> {
        let Some((ws_idx, tab_idx)) = self.parse_federation_tab_id(target_tab_id) else {
            return Err("tab not found".to_string());
        };
        let Some(tab_id) = self.public_tab_id(ws_idx, tab_idx) else {
            return Err("tab not found".to_string());
        };
        let workspace_id = self.public_workspace_id(ws_idx);
        let Some(ws) = self.state.workspaces.get(ws_idx) else {
            return Err("workspace not found".to_string());
        };
        let closes_workspace = ws.tabs.len() <= 1;
        let terminal_ids = self.state.terminal_ids_for_tab(ws_idx, tab_idx);
        let pane_ids = ws
            .tabs
            .get(tab_idx)
            .map(|tab| tab.layout.pane_ids())
            .unwrap_or_default();

        if closes_workspace {
            let closing_ids: std::collections::HashSet<String> =
                std::iter::once(workspace_id.clone()).collect();
            self.purge_federation_state_for_workspaces(&closing_ids);
            let workspace = self.workspace_info(ws_idx);
            self.close_single_workspace_at(ws_idx);
            self.state.remove_plugin_pane_records(pane_ids);
            self.shutdown_detached_terminal_runtimes();
            self.schedule_session_save();
            self.emit_event(EventEnvelope {
                event: EventKind::TabClosed,
                data: EventData::TabClosed {
                    tab_id,
                    workspace_id: workspace_id.clone(),
                },
            });
            self.emit_event(EventEnvelope {
                event: EventKind::WorkspaceClosed,
                data: EventData::WorkspaceClosed {
                    workspace_id,
                    workspace: Some(workspace),
                },
            });
            return Ok(());
        }

        let Some(ws) = self.state.workspaces.get_mut(ws_idx) else {
            return Err("workspace not found".to_string());
        };
        if !ws.close_tab(tab_idx) {
            return Err(format!("tab {target_tab_id} could not be closed"));
        }
        self.state.remove_plugin_pane_records(pane_ids);
        self.state.remove_unattached_terminal_ids(terminal_ids);
        self.shutdown_detached_terminal_runtimes();
        self.schedule_session_save();
        self.emit_event(EventEnvelope {
            event: EventKind::TabClosed,
            data: EventData::TabClosed {
                tab_id,
                workspace_id,
            },
        });
        Ok(())
    }

    /// Drops every per-mount bookkeeping entry belonging to the given
    /// (closing) workspace ids. Grouping the purge helpers behind one ungated
    /// entry point lets the ungated close paths (`api/tabs.rs`'s `tab.close`)
    /// stay free of `#[cfg]` while the helpers keep their Unix gating.
    #[cfg(unix)]
    pub(crate) fn purge_federation_state_for_workspaces(
        &mut self,
        workspace_ids: &std::collections::HashSet<String>,
    ) {
        self.purge_pending_remote_splits_for_workspaces(workspace_ids);
        self.purge_pending_remote_closes_for_workspaces(workspace_ids);
        self.purge_remote_resync_pane_index_for_workspaces(workspace_ids);
        self.purge_remote_resync_tab_index_for_workspaces(workspace_ids);
        self.purge_remote_resync_workspace_index_for_workspaces(workspace_ids);
        self.purge_remote_image_paste_pane_state_for_workspaces(workspace_ids);
        self.purge_pending_remote_clipboard_stages_for_workspaces(workspace_ids);
        self.pending_remote_workspace_focus
            .retain(|workspace_id| !workspace_ids.contains(workspace_id));
    }

    /// No mount can exist on a target without the federation mount
    /// primitives, so there is never any per-mount state to purge there.
    #[cfg(not(unix))]
    pub(crate) fn purge_federation_state_for_workspaces(
        &mut self,
        _workspace_ids: &std::collections::HashSet<String>,
    ) {
    }

    /// `AppEvent::FederationWorkspaceCreateAccepted` handler: the remote host
    /// confirmed the workspace this client asked it to create and named the
    /// id it will materialize under. Nothing is built here — the resync the
    /// same response triggers remains the single materialization path — this
    /// only remembers that *this* client owns the new workspace, so
    /// `materialize_resync_workspace_from_pane` can focus it on arrival.
    #[cfg(unix)]
    pub(crate) fn handle_federation_workspace_create_accepted(
        &mut self,
        request_id: u64,
        origin: crate::remote::federation::id::HostKey,
        workspace_id: String,
    ) {
        // Origin fence, same shape as the resync handlers: the id must be
        // namespaced under the mount that reported it.
        match crate::remote::federation::id::classify(&workspace_id) {
            crate::remote::federation::id::IdClass::Remote(host_key) if host_key == origin => {}
            _ => {
                tracing::warn!(
                    request_id,
                    %workspace_id,
                    expected_origin = %origin,
                    "dropping a workspace-create acceptance whose id is not namespaced under \
                     the mount that reported it"
                );
                return;
            }
        }
        if !self
            .pending_remote_workspace_create_focus
            .remove(&request_id)
        {
            // The request did not ask for focus (or was already answered);
            // the workspace still materializes, it just does not take focus.
            return;
        }
        // Already materialized (a structural event beat the response): focus
        // it now instead of waiting for a create that already happened.
        if let Some(ws_idx) = self
            .state
            .workspaces
            .iter()
            .position(|ws| ws.id == workspace_id)
        {
            self.state.switch_workspace(ws_idx);
            self.render_dirty.request_generic();
            self.render_notify.notify_one();
            return;
        }
        self.pending_remote_workspace_focus.insert(workspace_id);
    }

    /// `AppEvent::FederationResyncWorkspaceCreated` handler: a resync diff
    /// revealed a remote workspace this mount has never seen. A local
    /// `Workspace` cannot exist without a tab, and a `Tab` cannot exist
    /// without a pane, so this only records the workspace's identity and
    /// label — the pane event that follows in the same diff
    /// (`materialize_resync_workspace_from_pane`) builds the real workspace.
    /// Idempotent: an already-materialized workspace keeps its live state.
    #[cfg(unix)]
    pub(crate) fn handle_federation_resync_workspace_created(
        &mut self,
        origin: crate::remote::federation::id::HostKey,
        workspace_id: String,
        label: String,
    ) {
        // Origin fence before anything is recorded. There is no local
        // workspace to check `worktree_space` against yet, but the namespaced
        // id itself names the mount that owns it, so a differently-mounted
        // host still cannot register a workspace under another mount's
        // namespace.
        match crate::remote::federation::id::classify(&workspace_id) {
            crate::remote::federation::id::IdClass::Remote(host_key) if host_key == origin => {}
            _ => {
                tracing::warn!(
                    %workspace_id,
                    expected_origin = %origin,
                    "dropping a resync-created workspace whose id is not namespaced under the \
                     mount that reported it"
                );
                return;
            }
        }
        if self.state.workspaces.iter().any(|ws| ws.id == workspace_id) {
            return;
        }
        let entry = self
            .remote_resync_workspace_index
            .entry(workspace_id)
            .or_insert_with(|| RemoteWorkspaceRef {
                origin,
                label: String::new(),
            });
        entry.label = label;
    }

    /// `AppEvent::FederationResyncWorkspaceRemoved` handler: the remote no
    /// longer reports a workspace this mount materialized. The pane removals
    /// in the same diff usually collapsed it already (`Workspace::close_pane`
    /// returning `true` closes the workspace with its last pane), so this is
    /// most often just an index prune — but it also tears the workspace down
    /// for its own sake when the pane removals did not (e.g. panes this mount
    /// never indexed).
    #[cfg(unix)]
    pub(crate) fn handle_federation_resync_workspace_removed(
        &mut self,
        origin: crate::remote::federation::id::HostKey,
        workspace_id: String,
    ) {
        let Some(ws_idx) = self
            .state
            .workspaces
            .iter()
            .position(|ws| ws.id == workspace_id)
        else {
            // Not materialized. It may still be a pending announcement, which
            // only the mount that made it may retract — a second mount must
            // not be able to wipe another mount's pending workspace and strand
            // the pane event that would have materialized it.
            match self.remote_resync_workspace_index.get(&workspace_id) {
                Some(workspace_ref) if workspace_ref.origin != origin => {
                    tracing::warn!(
                        %workspace_id,
                        expected_origin = %workspace_ref.origin,
                        got_origin = %origin,
                        "dropping a resync workspace removal whose mount origin does not match \
                         the mount that announced its workspace"
                    );
                }
                _ => {
                    self.remote_resync_workspace_index.remove(&workspace_id);
                }
            }
            return;
        };
        // Authorize before mutating: an origin mismatch must leave the index
        // exactly as it found it, not evict a legitimate entry on the way to
        // refusing the removal.
        if !self.workspace_matches_federation_origin(ws_idx, &origin) {
            tracing::warn!(
                %workspace_id,
                expected_origin = %origin,
                "dropping a resync workspace removal whose mount origin does not match the \
                 workspace's federation origin"
            );
            return;
        }
        self.remote_resync_workspace_index.remove(&workspace_id);

        let closing_ids: std::collections::HashSet<String> =
            std::iter::once(workspace_id).collect();
        self.purge_federation_state_for_workspaces(&closing_ids);

        let public_workspace_id = self.public_workspace_id(ws_idx);
        let workspace_info = self.workspace_info(ws_idx);
        self.close_single_workspace_at(ws_idx);
        self.shutdown_detached_terminal_runtimes();
        // The remote-initiated removal is just as capable of taking down a
        // mount's last mirror as the client-initiated close is; without this
        // the link, its drive task and the `remote_mirrors` entry survive with
        // nothing visible behind them and a remount is refused as already live.
        self.end_federation_mount_if_no_mirrors_remain(&origin);
        self.schedule_session_save();

        self.emit_event(EventEnvelope {
            event: EventKind::WorkspaceClosed,
            data: EventData::WorkspaceClosed {
                workspace_id: public_workspace_id,
                workspace: Some(workspace_info),
            },
        });

        self.render_dirty.request_generic();
        self.render_notify.notify_one();
    }

    /// `AppEvent::FederationWorkspaceCreateFailed` handler: the remote host
    /// refused an earlier `WorkspaceCreateRequest`. Nothing was created
    /// remotely, so there is nothing local to reverse — surface the reason
    /// the same way a refused remote pane close does.
    #[cfg(unix)]
    pub(crate) fn handle_federation_workspace_create_failed(
        &mut self,
        request_id: u64,
        reason: String,
        origin: crate::remote::federation::id::HostKey,
    ) {
        tracing::warn!(request_id, %reason, %origin, "remote workspace create failed");
        // Nothing will ever materialize for this request; drop its focus claim
        // so a later create cannot inherit it.
        self.pending_remote_workspace_create_focus
            .remove(&request_id);
        match self.state.toast_config.delivery {
            crate::config::ToastDelivery::Herdr => {
                self.state.toast = Some(crate::app::state::ToastNotification {
                    kind: super::ToastKind::NeedsAttention,
                    title: "remote workspace create failed".to_string(),
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
                    // Unreachable: the two arms above are the only deliveries
                    // this match guard admits.
                    _ => return,
                };
                let _ = notify("remote workspace create failed", Some(&reason));
            }
            _ => {}
        }
        self.render_dirty.request_generic();
        self.render_notify.notify_one();
    }

    /// `AppEvent::FederationResyncTabCreated` handler: a resync diff revealed
    /// a remote tab this mount has never seen. A local `Tab` cannot exist
    /// without a pane, so this only records the tab's identity and label —
    /// the pane events that follow in the same diff
    /// (`handle_federation_resync_pane_created`) materialize the real tab and
    /// fill in its local tab number. Idempotent: a tab already materialized
    /// (by an out-of-order pane event, or at mount time) keeps its local
    /// binding.
    #[cfg(unix)]
    pub(crate) fn handle_federation_resync_tab_created(
        &mut self,
        origin: crate::remote::federation::id::HostKey,
        workspace_id: String,
        tab_id: String,
        label: String,
    ) {
        match self
            .state
            .workspaces
            .iter()
            .position(|ws| ws.id == workspace_id)
        {
            Some(ws_idx) => {
                if !self.workspace_matches_federation_origin(ws_idx, &origin) {
                    tracing::warn!(
                        %workspace_id,
                        %tab_id,
                        expected_origin = %origin,
                        "dropping a resync-created tab whose mount origin does not match its \
                         workspace's federation origin"
                    );
                    return;
                }
            }
            // The workspace itself may still be pending: a diff that creates
            // a whole remote workspace announces the workspace, then its
            // tabs, then the panes that materialize both. Recording the tab's
            // label now is what lets the pane event name the root tab.
            None => match self.remote_resync_workspace_index.get(&workspace_id) {
                Some(workspace_ref) if workspace_ref.origin == origin => {}
                Some(workspace_ref) => {
                    tracing::warn!(
                        %workspace_id,
                        %tab_id,
                        expected_origin = %workspace_ref.origin,
                        got_origin = %origin,
                        "dropping a resync-created tab whose mount origin does not match the \
                         mount that announced its workspace"
                    );
                    return;
                }
                None => {
                    tracing::warn!(
                        %workspace_id,
                        %tab_id,
                        "resync revealed a new remote tab but its workspace is neither \
                         materialized nor announced"
                    );
                    return;
                }
            },
        }

        let entry = self
            .remote_resync_tab_index
            .entry(tab_id)
            .or_insert_with(|| RemoteTabRef {
                workspace_id,
                tab_number: None,
                label: None,
            });
        entry.label = Some(label);
    }

    /// `AppEvent::FederationResyncTabClosed` handler: the remote no longer
    /// reports a tab this mount materialized. Usually the tab's panes are
    /// retired in the same diff and `handle_federation_resync_pane_removed`
    /// has already collapsed the local tab (`Workspace::close_pane` drops a
    /// tab with its last pane), so this is most often just an index prune —
    /// but it also tears the tab down for its own sake when the pane
    /// removals did not (e.g. panes this mount never indexed).
    #[cfg(unix)]
    pub(crate) fn handle_federation_resync_tab_removed(
        &mut self,
        origin: crate::remote::federation::id::HostKey,
        tab_id: String,
    ) {
        // Authorize before mutating. Removing the entry first would let a
        // second mount's stray removal evict a live tab's index entry while
        // the fence below correctly refuses the removal itself — after which
        // the next resync pane for that still-live remote tab would build a
        // duplicate local tab beside the real one.
        let Some(tab_ref) = self.remote_resync_tab_index.get(&tab_id) else {
            return;
        };
        let indexed_workspace_id = tab_ref.workspace_id.clone();
        let tab_number = tab_ref.tab_number;
        let Some(ws_idx) = self
            .state
            .workspaces
            .iter()
            .position(|ws| ws.id == indexed_workspace_id)
        else {
            // The tab's workspace is gone locally, so the entry is dead
            // bookkeeping either way — but only the mount that owns the id's
            // namespace may retire it.
            match crate::remote::federation::id::classify(&indexed_workspace_id) {
                crate::remote::federation::id::IdClass::Remote(host_key) if host_key == origin => {
                    self.remote_resync_tab_index.remove(&tab_id);
                }
                _ => {
                    tracing::warn!(
                        %tab_id,
                        workspace_id = %indexed_workspace_id,
                        expected_origin = %origin,
                        "dropping a resync tab removal whose mount origin does not own its \
                         workspace's id namespace"
                    );
                }
            }
            return;
        };
        if !self.workspace_matches_federation_origin(ws_idx, &origin) {
            tracing::warn!(
                %tab_id,
                expected_origin = %origin,
                "dropping a resync tab removal whose mount origin does not match its \
                 workspace's federation origin"
            );
            return;
        }
        let Some(tab_idx) = tab_number.and_then(|number| {
            self.state.workspaces[ws_idx]
                .tabs
                .iter()
                .position(|tab| tab.number == number)
        }) else {
            // Already gone (pane removals collapsed it) — pruning the index
            // is the whole job.
            self.remote_resync_tab_index.remove(&tab_id);
            return;
        };
        // A workspace must always keep at least one tab; closing the last
        // one is a workspace close, which the pane-removal path already owns
        // (`Workspace::close_pane` returning true). Leave it alone here — and
        // leave its index entry alone too, so the still-live local tab keeps
        // resolving instead of having later panes build a duplicate beside it.
        if self.state.workspaces[ws_idx].tabs.len() <= 1 {
            return;
        }
        self.remote_resync_tab_index.remove(&tab_id);

        let public_tab_id = self.public_tab_id(ws_idx, tab_idx);
        // Purge-gap fix: `public_tab_id` is the same canonical id
        // `dispatch_remote_tab_close` stores in `RemoteCloseTarget::Tab`
        // (never the mirror-namespaced `tab_id` parameter above, a different
        // id scheme) — compute it before `close_tab` below invalidates
        // `tab_idx`, so a pending tab-close response arriving after this
        // resync removal can no longer act on whatever later reuses the
        // same tab slot.
        if let Some(public_tab_id) = public_tab_id.as_deref() {
            self.purge_pending_remote_close_for_tab(public_tab_id);
        }
        let workspace_id = self.public_workspace_id(ws_idx);
        let terminal_ids = self.state.terminal_ids_for_tab(ws_idx, tab_idx);
        let pane_ids: Vec<PaneId> = self.state.workspaces[ws_idx]
            .tabs
            .get(tab_idx)
            .map(|tab| tab.layout.pane_ids())
            .unwrap_or_default();

        if !self.state.workspaces[ws_idx].close_tab(tab_idx) {
            tracing::warn!(
                %tab_id,
                "resync reported a closed remote tab but the local tab could not be closed"
            );
            return;
        }
        let closed_pane_ids: std::collections::HashSet<PaneId> = pane_ids.iter().copied().collect();
        self.remote_resync_pane_index
            .retain(|_, local_pane_id| !closed_pane_ids.contains(local_pane_id));
        self.state.remove_plugin_pane_records(pane_ids);
        self.state.remove_unattached_terminal_ids(terminal_ids);
        self.shutdown_detached_terminal_runtimes();
        self.schedule_session_save();

        if let Some(public_tab_id) = public_tab_id {
            self.emit_event(EventEnvelope {
                event: EventKind::TabClosed,
                data: EventData::TabClosed {
                    tab_id: public_tab_id,
                    workspace_id,
                },
            });
        }

        self.render_dirty.request_generic();
        self.render_notify.notify_one();
    }

    /// Whether `ws_idx`'s workspace was materialized by a mount from
    /// `origin`. Shared origin fence for the federation resync handlers: a
    /// differently-mounted host must not be able to mutate another mount's
    /// workspace by guessing its ids.
    #[cfg(unix)]
    fn workspace_matches_federation_origin(
        &self,
        ws_idx: usize,
        origin: &crate::remote::federation::id::HostKey,
    ) -> bool {
        let space_key = format!("federation:{}", origin.as_str());
        self.state
            .workspaces
            .get(ws_idx)
            .and_then(|ws| ws.worktree_space())
            .is_some_and(|space| space.key == space_key)
    }

    /// `AppEvent::FederationResyncPaneRemoved` handler (post-mount pane
    /// mirroring, part 2 — plans/260722-1327): the remote no longer reports
    /// a pane this mount previously resync-materialized; tear the local
    /// runtime down and remove it from its workspace. Unlike the interactive
    /// `pane.close` API path, this never asks for close confirmation (e.g.
    /// "closing this pane would close a worktree group") — the remote
    /// already made this decision; there is nothing left here to confirm.
    #[cfg(unix)]
    pub(crate) fn handle_federation_resync_pane_removed(
        &mut self,
        origin: crate::remote::federation::id::HostKey,
        pane_id: String,
    ) {
        let Some(local_pane_id) = self.remote_resync_pane_index.remove(&pane_id) else {
            // Not every removed remote pane necessarily went through the
            // resync-created path (e.g. it may have been materialized at
            // mount time, or via a `SplitPaneResponse::Created` this
            // App itself requested) — nothing to reverse-index here means
            // nothing this handler owns to tear down.
            return;
        };
        let Some((ws_idx, _)) = self.find_pane(local_pane_id) else {
            return;
        };

        if !self.workspace_matches_federation_origin(ws_idx, &origin) {
            tracing::warn!(
                %pane_id,
                expected_origin = %origin,
                "dropping a resync pane removal whose mount origin does not match its \
                 workspace's federation origin"
            );
            return;
        }

        let workspace_id = self.public_workspace_id(ws_idx);
        let public_pane_id = self.public_pane_id(ws_idx, local_pane_id);
        let layout_update_target =
            self.layout_update_target_after_pane_removal(ws_idx, local_pane_id);
        let terminal_id = self.state.terminal_id_for_pane(ws_idx, local_pane_id);

        // Captured while the workspace is still present: the last pane's
        // removal below can take the whole workspace with it, and the purge
        // afterwards is keyed by this local id.
        let local_workspace_id = self.state.workspaces.get(ws_idx).map(|ws| ws.id.clone());

        let should_close_workspace = {
            let Some(ws) = self.state.workspaces.get_mut(ws_idx) else {
                return;
            };
            ws.close_pane(local_pane_id)
        };
        self.state.remove_plugin_pane_records([local_pane_id]);

        if should_close_workspace {
            // The last pane took its workspace with it, so this removal is a
            // workspace removal too: drop the federation bookkeeping keyed on
            // it and settle the mount, exactly as the workspace-level handlers
            // do. Otherwise the link, its drive task and the `remote_mirrors`
            // entry outlive the last visible mirror and block a remount.
            if let Some(local_workspace_id) = local_workspace_id {
                let closing_ids: std::collections::HashSet<String> =
                    std::iter::once(local_workspace_id).collect();
                self.purge_federation_state_for_workspaces(&closing_ids);
            }
            // One workspace, not the mount's whole federation group — see
            // `close_single_workspace_at`.
            self.close_single_workspace_at(ws_idx);
            self.shutdown_detached_terminal_runtimes();
            self.end_federation_mount_if_no_mirrors_remain(&origin);
            if let Some(public_pane_id) = public_pane_id {
                self.emit_event(EventEnvelope {
                    event: EventKind::PaneClosed,
                    data: EventData::PaneClosed {
                        pane_id: public_pane_id,
                        workspace_id,
                    },
                });
            }
        } else {
            if let Some(terminal_id) = terminal_id {
                self.state.remove_unattached_terminal_ids([terminal_id]);
            }
            self.shutdown_detached_terminal_runtimes();
            self.schedule_session_save();
            if let Some(public_pane_id) = public_pane_id {
                self.emit_event(EventEnvelope {
                    event: EventKind::PaneClosed,
                    data: EventData::PaneClosed {
                        pane_id: public_pane_id,
                        workspace_id,
                    },
                });
            }
            if let Some((ws_idx, tab_idx)) = layout_update_target {
                self.emit_layout_updated_event(ws_idx, tab_idx);
            }
        }

        self.render_dirty.request_generic();
        self.render_notify.notify_one();
    }
}

/// Local layout context a `SplitPaneRequest` was minted from, remembered
/// until its `SplitPaneResponse` arrives (or the process ends). See
/// `App::register_pending_remote_split`/`handle_federation_split_pane_ready`.
/// Populated by the ungated dispatch path but only read by the
/// `#[cfg(unix)]` federation response handlers, so every field is unread on a
/// target without the federation mount primitives.
#[cfg_attr(not(unix), allow(dead_code))]
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

/// Local layout context a `ClosePaneRequest` was minted from, remembered
/// until its `ClosePaneResponse` arrives (or the process ends). See
/// `App::register_pending_remote_close`/`handle_federation_close_pane_ready`
/// (Gap A, plans/260724-1536-federation-pane-close-sync).
/// Populated by the ungated dispatch path but only read by the
/// `#[cfg(unix)]` federation response handlers, so every field is unread on a
/// target without the federation mount primitives.
/// Where a mirrored remote tab lives locally (`App::remote_resync_tab_index`
/// value). Identifies the local tab by workspace id + *public tab number*
/// rather than a `Vec` index because tab indices shift whenever any earlier
/// tab in the same workspace closes, while `Tab::number` is stable for the
/// life of the tab and never reused (`Workspace::next_public_tab_number` only
/// ever increases) — the same reasoning `PendingRemoteSplit::workspace_id`
/// records for workspaces.
/// Only read by the `#[cfg(unix)]` federation resync handlers.
/// A mirrored remote workspace a resync has announced but whose local
/// `Workspace` does not exist yet (`App::remote_resync_workspace_index`
/// value). Holds only what building that workspace needs once its first pane
/// arrives; a materialized workspace is looked up by `Workspace::id`
/// directly, so entries here are short-lived.
/// Only read by the `#[cfg(unix)]` federation resync handlers.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) struct RemoteWorkspaceRef {
    /// The mount that announced this workspace. Re-checked against the
    /// origin of the pane event that materializes it, so a second mount
    /// cannot complete another mount's pending workspace.
    pub(crate) origin: crate::remote::federation::id::HostKey,
    /// Remote-supplied label, remembered so the local workspace is named
    /// like its remote counterpart.
    pub(crate) label: String,
}

#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) struct RemoteTabRef {
    /// Stable local `Workspace::id` (already the mirror's namespaced id for
    /// a materialized federation workspace).
    pub(crate) workspace_id: String,
    /// `Tab::number` of the local tab, or `None` while the remote tab is
    /// known (a resync reported it) but has no local tab yet because none of
    /// its panes have materialized.
    pub(crate) tab_number: Option<usize>,
    /// Remote-supplied label, remembered so the local tab this remote tab
    /// eventually materializes into is named like its remote counterpart.
    pub(crate) label: Option<String>,
}

/// What a `PendingRemoteClose` entry is waiting to tear down once its
/// response arrives. All three kinds share ONE map and ONE `request_id`
/// counter (`next_remote_close_request_id`, `app/api/panes.rs`) — see
/// `App::pending_remote_closes`'s own doc comment for why a second counter
/// would be unsafe.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) enum RemoteCloseTarget {
    /// `ClosePaneRequest`.
    Pane(PaneId),
    /// `TabCloseRequest` (`tab.close_remote`). Carries the CANONICAL public
    /// tab id (`App::public_tab_id`), never the caller-supplied target
    /// string: `App::parse_tab_id`'s `t_<ws>_<idx>` form is positional, so a
    /// neighbour tab closing and renumbering before the ack arrives could
    /// otherwise redirect the teardown onto a live tab that reused the same
    /// index.
    Tab(String),
    /// `WorkspaceCloseRequest` (`workspace.close_remote`). No extra payload
    /// beyond `workspace_id` below — that already identifies the target.
    Workspace,
}

#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) struct PendingRemoteClose {
    /// Stable workspace id (`Workspace::id`), same reasoning as
    /// `PendingRemoteSplit::workspace_id`.
    pub(crate) workspace_id: String,
    /// The mount this close request was actually sent to
    /// (`App::federation_host_key_for_workspace` at mint time), same
    /// origin-fencing reasoning as `PendingRemoteSplit::origin`.
    pub(crate) origin: crate::remote::federation::id::HostKey,
    pub(crate) target: RemoteCloseTarget,
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

    /// The single mirrored tab's namespaced (public) id — the exact key
    /// `App::remote_resync_tab_index` is keyed on, so a test can address the
    /// already-materialized tab the way a real resync diff would.
    fn only_tab_id(mirror: &RemoteMirror) -> String {
        mirror
            .tabs()
            .keys()
            .next()
            .cloned()
            .expect("the test mirror always holds exactly one tab")
    }

    fn tab_info_for(tab_id: &str, number: usize, label: &str) -> RemoteTabInfo {
        RemoteTabInfo {
            tab_id: tab_id.to_string(),
            workspace_id: "w1".to_string(),
            number,
            label: label.to_string(),
            focused: false,
            pane_count: 1,
            agent_status: AgentStatus::Idle,
        }
    }

    fn pane_info_in_tab(pane_id: &str, terminal_id: &str, tab_id: &str) -> RemotePaneInfo {
        let mut pane = pane_info(pane_id, terminal_id);
        pane.tab_id = tab_id.to_string();
        pane
    }

    /// Two remote tabs, one pane each — the shape that regressed into a
    /// single local tab with two splits.
    fn two_tab_snapshot() -> SessionSnapshot {
        let mut workspace = workspace_info();
        workspace.tab_count = 2;
        SessionSnapshot {
            version: "0.0.0-test".to_string(),
            protocol: 1,
            focused_workspace_id: None,
            focused_tab_id: None,
            focused_pane_id: None,
            workspaces: vec![workspace],
            tabs: vec![
                tab_info_for("w1-tab", 1, "first remote tab"),
                tab_info_for("w1-tab2", 2, "second remote tab"),
            ],
            panes: vec![
                pane_info_in_tab("p1", "t1", "w1-tab"),
                pane_info_in_tab("p2", "t2", "w1-tab2"),
            ],
            layouts: Vec::new(),
            agents: Vec::new(),
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
        let render_dirty = std::sync::Arc::new(crate::render_signal::RenderSignal::new());

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
            remote_pane_id: "remote-pane".to_string(),
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
        let render_dirty = std::sync::Arc::new(crate::render_signal::RenderSignal::new());

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
            remote_pane_id: "remote-pane".to_string(),
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

    /// Phase 4 (plans/260724-1536-federation-pane-close-sync): a
    /// `ClosePaneResponse::Closed` (delivered as `AppEvent::
    /// FederationClosePaneReady`) must tear the pending pane down, the same
    /// way `handle_federation_resync_pane_removed` does — the remote already
    /// made the real close decision.
    #[cfg(unix)]
    #[tokio::test]
    async fn close_pane_ready_tears_down_the_pending_pane() {
        let mut app = test_app();
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("local")];
        app.state.active = Some(0);
        let workspace_id = app.state.workspaces[0].id.clone();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;

        let request_id = 42u64;
        let origin = crate::remote::federation::id::HostKey::new("remote-host", "s1");
        app.register_pending_remote_close(
            request_id,
            PendingRemoteClose {
                workspace_id,
                origin: origin.clone(),
                target: RemoteCloseTarget::Pane(pane_id),
            },
        );

        app.handle_federation_close_pane_ready(request_id, origin);

        assert!(
            app.find_pane(pane_id).is_none(),
            "a ClosePaneResponse::Closed must tear the pending pane down"
        );
        assert!(!app.pending_remote_closes.contains_key(&request_id));
    }

    /// Origin-check counterpart, mirroring `split_pane_response_from_a_
    /// different_mount_than_the_request_is_ignored`: a close response
    /// tagged with a different mount's `HostKey` must be dropped, leaving
    /// the pending entry and the pane both untouched.
    #[cfg(unix)]
    #[tokio::test]
    async fn close_pane_ready_from_a_different_mount_than_the_request_is_ignored() {
        let mut app = test_app();
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("local")];
        app.state.active = Some(0);
        let workspace_id = app.state.workspaces[0].id.clone();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;

        let request_id = 43u64;
        let real_origin = crate::remote::federation::id::HostKey::new("real-host", "s1");
        app.register_pending_remote_close(
            request_id,
            PendingRemoteClose {
                workspace_id,
                origin: real_origin,
                target: RemoteCloseTarget::Pane(pane_id),
            },
        );

        let spoofed_origin = crate::remote::federation::id::HostKey::new("evil-host", "s1");
        app.handle_federation_close_pane_ready(request_id, spoofed_origin);

        assert!(
            app.pending_remote_closes.contains_key(&request_id),
            "a response from the wrong origin must not consume the pending entry"
        );
        assert!(
            app.find_pane(pane_id).is_some(),
            "a response from the wrong origin must not tear any pane down"
        );
    }

    /// Predict risk 3 (double-signal race): a pane that is BOTH registered
    /// in `pending_remote_closes` (in-flight `ClosePaneRequest`) AND, before
    /// the response arrives, gets torn down via the resync path
    /// (`handle_federation_resync_pane_removed`) for the same pane must not
    /// panic or double-emit `PaneClosed` when the `ClosePaneResponse::
    /// Closed` arrives afterward — it is a no-op (pane already gone), not an
    /// error.
    #[cfg(unix)]
    #[tokio::test]
    async fn close_pane_ready_after_the_pane_was_already_resync_removed_is_a_no_op() {
        let mut app = test_app();
        let mount = mount(1);
        let mut mirror = RemoteMirror::new(mount.clone());
        mirror.apply_snapshot(&two_pane_snapshot(), EventCursor(0));

        let mut router = TerminalChannelRouter::new();
        let (out_tx, _out_rx) = tokio::sync::mpsc::unbounded_channel();
        let (clipboard_tx, _clipboard_rx) = tokio::sync::mpsc::unbounded_channel();
        let created = app
            .materialize_federation_mount(&mirror, &mut router, &out_tx, &clipboard_tx)
            .expect("materialization must succeed");
        let ws_idx = created[0];
        let workspace_id = app.state.workspaces[ws_idx].id.clone();
        let root_pane_id = app.state.workspaces[ws_idx].tabs[0].root_pane;

        let request_id = 44u64;
        app.register_pending_remote_close(
            request_id,
            PendingRemoteClose {
                workspace_id,
                origin: mount.host_key.clone(),
                target: RemoteCloseTarget::Pane(root_pane_id),
            },
        );

        // The resync path tears the same pane down first (its own
        // `ClosePaneResponse` has not arrived yet from this test's point of
        // view — a plausible race between a resync poll and a slower
        // response on the same link). The reverse-index key is the
        // mirror's own namespaced id, not the raw snapshot pane id (see the
        // Gap B regression test above), so find it by value.
        let root_remote_pane_id = app
            .remote_resync_pane_index
            .iter()
            .find_map(|(remote_id, local_id)| {
                (*local_id == root_pane_id).then(|| remote_id.clone())
            })
            .expect("the mount-time root pane must be indexed by build_remote_pane");
        app.handle_federation_resync_pane_removed(mount.host_key.clone(), root_remote_pane_id);
        assert!(app.find_pane(root_pane_id).is_none());

        // The (now-late) `ClosePaneResponse::Closed` must not panic and must
        // not emit a second `PaneClosed` for an already-gone pane.
        app.handle_federation_close_pane_ready(request_id, mount.host_key.clone());

        assert!(
            app.find_pane(root_pane_id).is_none(),
            "the pane must remain gone; no re-creation or panic"
        );
    }

    /// `AppEvent::FederationClosePaneFailed` handler: drops the pending
    /// entry and does not touch layout, mirroring
    /// `handle_federation_split_pane_failed`.
    #[cfg(unix)]
    #[tokio::test]
    async fn close_pane_failed_drops_the_pending_entry_without_touching_layout() {
        let mut app = test_app();
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("local")];
        app.state.active = Some(0);
        let workspace_id = app.state.workspaces[0].id.clone();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;

        let request_id = 45u64;
        let origin = crate::remote::federation::id::HostKey::new("remote-host", "s1");
        app.register_pending_remote_close(
            request_id,
            PendingRemoteClose {
                workspace_id,
                origin: origin.clone(),
                target: RemoteCloseTarget::Pane(pane_id),
            },
        );

        app.handle_federation_close_pane_failed(request_id, "refused".to_string(), origin);

        assert!(!app.pending_remote_closes.contains_key(&request_id));
        assert!(
            app.find_pane(pane_id).is_some(),
            "a close failure must not touch the local pane"
        );
    }

    /// `AppEvent::FederationWorkspaceCloseReady` handler
    /// (`workspace.close_remote`): a `WorkspaceCloseResponse::Closed` must
    /// tear the pending workspace down, mirroring
    /// `close_pane_ready_tears_down_the_pending_pane`.
    #[cfg(unix)]
    #[tokio::test]
    async fn workspace_close_ready_tears_down_the_pending_workspace() {
        let mut app = test_app();
        let mount = mount(1);
        let mut mirror = RemoteMirror::new(mount.clone());
        mirror.apply_snapshot(&two_pane_snapshot(), EventCursor(0));
        let mut router = TerminalChannelRouter::new();
        let (out_tx, _out_rx) = tokio::sync::mpsc::unbounded_channel();
        let (clipboard_tx, _clipboard_rx) = tokio::sync::mpsc::unbounded_channel();
        let created = app
            .materialize_federation_mount(&mirror, &mut router, &out_tx, &clipboard_tx)
            .expect("materialization must succeed");
        let ws_idx = created[0];
        let workspace_id = app.state.workspaces[ws_idx].id.clone();

        let request_id = 60u64;
        app.register_pending_remote_close(
            request_id,
            PendingRemoteClose {
                workspace_id: workspace_id.clone(),
                origin: mount.host_key.clone(),
                target: RemoteCloseTarget::Workspace,
            },
        );

        app.handle_federation_workspace_close_ready(request_id, mount.host_key.clone());

        assert!(
            !app.state.workspaces.iter().any(|ws| ws.id == workspace_id),
            "a WorkspaceCloseResponse::Closed must tear the pending workspace down"
        );
        assert!(!app.pending_remote_closes.contains_key(&request_id));
    }

    /// Retiring a mount's LAST mirrored workspace must also end the mount.
    /// The mount owns the link, the drive task and the `remote_mirrors`
    /// entry; leaving it registered with nothing visible behind it strands
    /// all three, and a later remount of the same host reports it as already
    /// live. The local `workspace.close` verb ends it inline, so the
    /// confirmed-close path has to as well or the two verbs disagree.
    #[cfg(unix)]
    #[tokio::test]
    async fn workspace_close_ready_ends_a_mount_whose_last_mirror_it_retired() {
        let mut app = test_app();
        let mount = mount(1);
        let mut mirror = RemoteMirror::new(mount.clone());
        mirror.apply_snapshot(&two_pane_snapshot(), EventCursor(0));
        let mut router = TerminalChannelRouter::new();
        let (out_tx, _out_rx) = tokio::sync::mpsc::unbounded_channel();
        let (clipboard_tx, _clipboard_rx) = tokio::sync::mpsc::unbounded_channel();
        let created = app
            .materialize_federation_mount(&mirror, &mut router, &out_tx, &clipboard_tx)
            .expect("materialization must succeed");
        assert_eq!(
            created.len(),
            1,
            "this fixture mounts exactly one workspace, so closing it empties the mount"
        );
        let ws_idx = created[0];
        let workspace_id = app.state.workspaces[ws_idx].id.clone();
        app.state
            .begin_federation_mount(mirror)
            .expect("registering the mount must succeed");

        let request_id = 61u64;
        app.register_pending_remote_close(
            request_id,
            PendingRemoteClose {
                workspace_id: workspace_id.clone(),
                origin: mount.host_key.clone(),
                target: RemoteCloseTarget::Workspace,
            },
        );
        app.handle_federation_workspace_close_ready(request_id, mount.host_key.clone());

        assert!(
            !app.state.workspaces.iter().any(|ws| ws.id == workspace_id),
            "the confirmed close must still retire the workspace"
        );
        assert!(
            !app.state.remote_mirrors.contains_key(&mount.host_key),
            "retiring the mount's last mirrored workspace must end the mount, not strand it"
        );
    }

    /// The remote-initiated removal must settle the mount exactly as the
    /// client-initiated close does. This is the normal flow when the OTHER
    /// user closes the last mirrored workspace: nothing local was clicked, so
    /// only the resync handler can notice the mount just went empty. Leaving
    /// it registered strands the link, the drive task and the `remote_mirrors`
    /// entry with nothing visible behind them, and a remount of the same host
    /// is refused as already live until the server restarts.
    #[cfg(unix)]
    #[tokio::test]
    async fn resync_workspace_removed_ends_a_mount_whose_last_mirror_it_retired() {
        let mut app = test_app();
        let mount = mount(1);
        let mut mirror = RemoteMirror::new(mount.clone());
        mirror.apply_snapshot(&two_pane_snapshot(), EventCursor(0));
        let mut router = TerminalChannelRouter::new();
        let (out_tx, _out_rx) = tokio::sync::mpsc::unbounded_channel();
        let (clipboard_tx, _clipboard_rx) = tokio::sync::mpsc::unbounded_channel();
        let created = app
            .materialize_federation_mount(&mirror, &mut router, &out_tx, &clipboard_tx)
            .expect("materialization must succeed");
        assert_eq!(
            created.len(),
            1,
            "this fixture mounts exactly one workspace, so removing it empties the mount"
        );
        let workspace_id = app.state.workspaces[created[0]].id.clone();
        app.state
            .begin_federation_mount(mirror)
            .expect("registering the mount must succeed");

        app.handle_federation_resync_workspace_removed(
            mount.host_key.clone(),
            workspace_id.clone(),
        );

        assert!(
            !app.state.workspaces.iter().any(|ws| ws.id == workspace_id),
            "the remote's removal must still retire the workspace locally"
        );
        assert!(
            !app.state.remote_mirrors.contains_key(&mount.host_key),
            "retiring the mount's last mirrored workspace must end the mount, not strand it"
        );
    }

    /// Origin-check counterpart, mirroring
    /// `close_pane_ready_from_a_different_mount_than_the_request_is_ignored`.
    #[cfg(unix)]
    #[tokio::test]
    async fn workspace_close_ready_from_a_different_mount_than_the_request_is_ignored() {
        let mut app = test_app();
        let mount = mount(1);
        let mut mirror = RemoteMirror::new(mount.clone());
        mirror.apply_snapshot(&two_pane_snapshot(), EventCursor(0));
        let mut router = TerminalChannelRouter::new();
        let (out_tx, _out_rx) = tokio::sync::mpsc::unbounded_channel();
        let (clipboard_tx, _clipboard_rx) = tokio::sync::mpsc::unbounded_channel();
        let created = app
            .materialize_federation_mount(&mirror, &mut router, &out_tx, &clipboard_tx)
            .expect("materialization must succeed");
        let ws_idx = created[0];
        let workspace_id = app.state.workspaces[ws_idx].id.clone();

        let request_id = 61u64;
        app.register_pending_remote_close(
            request_id,
            PendingRemoteClose {
                workspace_id: workspace_id.clone(),
                origin: mount.host_key.clone(),
                target: RemoteCloseTarget::Workspace,
            },
        );

        let spoofed_origin = HostKey::new("evil-host", "s1");
        app.handle_federation_workspace_close_ready(request_id, spoofed_origin);

        assert!(
            app.pending_remote_closes.contains_key(&request_id),
            "a response from the wrong origin must not consume the pending entry"
        );
        assert!(
            app.state.workspaces.iter().any(|ws| ws.id == workspace_id),
            "a response from the wrong origin must not tear the workspace down"
        );
    }

    /// Workspace counterpart of the pane retry/duplicate-click safety: an ack that arrives after a
    /// racing resync already removed the workspace must not panic and must
    /// remain an idempotent no-op, mirroring
    /// `close_pane_ready_after_the_pane_was_already_resync_removed_is_a_no_op`.
    #[cfg(unix)]
    #[tokio::test]
    async fn workspace_close_ready_after_a_racing_resync_removal_is_idempotent() {
        let mut app = test_app();
        let mount = mount(1);
        let mut mirror = RemoteMirror::new(mount.clone());
        mirror.apply_snapshot(&two_pane_snapshot(), EventCursor(0));
        let mut router = TerminalChannelRouter::new();
        let (out_tx, _out_rx) = tokio::sync::mpsc::unbounded_channel();
        let (clipboard_tx, _clipboard_rx) = tokio::sync::mpsc::unbounded_channel();
        let created = app
            .materialize_federation_mount(&mirror, &mut router, &out_tx, &clipboard_tx)
            .expect("materialization must succeed");
        let ws_idx = created[0];
        let workspace_id = app.state.workspaces[ws_idx].id.clone();

        let request_id = 62u64;
        app.register_pending_remote_close(
            request_id,
            PendingRemoteClose {
                workspace_id: workspace_id.clone(),
                origin: mount.host_key.clone(),
                target: RemoteCloseTarget::Workspace,
            },
        );

        // A racing resync tears the workspace down first (its own
        // `WorkspaceCloseResponse` has not arrived yet from this test's
        // point of view).
        app.handle_federation_resync_workspace_removed(
            mount.host_key.clone(),
            workspace_id.clone(),
        );
        assert!(!app.state.workspaces.iter().any(|ws| ws.id == workspace_id));

        // The (now-late) `WorkspaceCloseResponse::Closed` must not panic and
        // must not double-tear-down an already-gone workspace.
        app.handle_federation_workspace_close_ready(request_id, mount.host_key.clone());

        assert!(
            !app.state.workspaces.iter().any(|ws| ws.id == workspace_id),
            "the workspace must remain gone; no re-creation or panic"
        );
    }

    /// `AppEvent::FederationWorkspaceCloseFailed` handler: drops the pending
    /// entry and does not touch layout, mirroring
    /// `close_pane_failed_drops_the_pending_entry_without_touching_layout`.
    #[cfg(unix)]
    #[tokio::test]
    async fn workspace_close_failed_drops_the_pending_entry_without_touching_layout() {
        let mut app = test_app();
        let mount = mount(1);
        let mut mirror = RemoteMirror::new(mount.clone());
        mirror.apply_snapshot(&two_pane_snapshot(), EventCursor(0));
        let mut router = TerminalChannelRouter::new();
        let (out_tx, _out_rx) = tokio::sync::mpsc::unbounded_channel();
        let (clipboard_tx, _clipboard_rx) = tokio::sync::mpsc::unbounded_channel();
        let created = app
            .materialize_federation_mount(&mirror, &mut router, &out_tx, &clipboard_tx)
            .expect("materialization must succeed");
        let ws_idx = created[0];
        let workspace_id = app.state.workspaces[ws_idx].id.clone();

        let request_id = 63u64;
        app.register_pending_remote_close(
            request_id,
            PendingRemoteClose {
                workspace_id: workspace_id.clone(),
                origin: mount.host_key.clone(),
                target: RemoteCloseTarget::Workspace,
            },
        );

        app.handle_federation_workspace_close_failed(
            request_id,
            "refused".to_string(),
            mount.host_key.clone(),
        );

        assert!(!app.pending_remote_closes.contains_key(&request_id));
        assert!(
            app.state.workspaces.iter().any(|ws| ws.id == workspace_id),
            "a close failure must not touch the local workspace"
        );
    }

    /// `AppEvent::FederationTabCloseReady` handler (`tab.close_remote`): a
    /// `TabCloseResponse::Closed` for a NON-last tab must close only that
    /// tab, leaving its sibling(s) alive.
    #[cfg(unix)]
    #[tokio::test]
    async fn tab_close_ready_tears_down_the_pending_tab() {
        let (mut app, mount, ws_idx, _workspace_id, tab_ids) = mount_two_tab_mirror();
        let tab_id_to_close = app.public_tab_id(ws_idx, 1).unwrap();

        let request_id = 70u64;
        app.register_pending_remote_close(
            request_id,
            PendingRemoteClose {
                workspace_id: app.state.workspaces[ws_idx].id.clone(),
                origin: mount.host_key.clone(),
                target: RemoteCloseTarget::Tab(tab_id_to_close.clone()),
            },
        );

        app.handle_federation_tab_close_ready(request_id, mount.host_key.clone());

        assert_eq!(
            app.state.workspaces[ws_idx].tabs.len(),
            1,
            "closing the non-last tab must leave the workspace's other tab alive"
        );
        assert!(!app.pending_remote_closes.contains_key(&request_id));
        assert!(
            !app.remote_resync_tab_index.contains_key(&tab_ids[1]),
            "the closed tab's resync index entry must be pruned too"
        );
    }

    /// Origin-check counterpart, mirroring
    /// `close_pane_ready_from_a_different_mount_than_the_request_is_ignored`.
    #[cfg(unix)]
    #[tokio::test]
    async fn tab_close_ready_from_a_different_mount_than_the_request_is_ignored() {
        let (mut app, mount, ws_idx, _workspace_id, _tab_ids) = mount_two_tab_mirror();
        let tab_id_to_close = app.public_tab_id(ws_idx, 1).unwrap();

        let request_id = 71u64;
        app.register_pending_remote_close(
            request_id,
            PendingRemoteClose {
                workspace_id: app.state.workspaces[ws_idx].id.clone(),
                origin: mount.host_key.clone(),
                target: RemoteCloseTarget::Tab(tab_id_to_close),
            },
        );

        let spoofed_origin = HostKey::new("evil-host", "s1");
        app.handle_federation_tab_close_ready(request_id, spoofed_origin);

        assert!(
            app.pending_remote_closes.contains_key(&request_id),
            "a response from the wrong origin must not consume the pending entry"
        );
        assert_eq!(
            app.state.workspaces[ws_idx].tabs.len(),
            2,
            "a response from the wrong origin must not tear any tab down"
        );
    }

    /// `AppEvent::FederationTabCloseFailed` handler: drops the pending entry
    /// and does not touch layout.
    #[cfg(unix)]
    #[tokio::test]
    async fn tab_close_failed_drops_the_pending_entry_without_touching_layout() {
        let (mut app, mount, ws_idx, _workspace_id, _tab_ids) = mount_two_tab_mirror();
        let tab_id_to_close = app.public_tab_id(ws_idx, 1).unwrap();

        let request_id = 72u64;
        app.register_pending_remote_close(
            request_id,
            PendingRemoteClose {
                workspace_id: app.state.workspaces[ws_idx].id.clone(),
                origin: mount.host_key.clone(),
                target: RemoteCloseTarget::Tab(tab_id_to_close),
            },
        );

        app.handle_federation_tab_close_failed(
            request_id,
            "refused".to_string(),
            mount.host_key.clone(),
        );

        assert!(!app.pending_remote_closes.contains_key(&request_id));
        assert_eq!(
            app.state.workspaces[ws_idx].tabs.len(),
            2,
            "a close failure must not touch the local tabs"
        );
    }

    /// Purge-gap fix: a pending tab close must be dropped when its tab is
    /// removed by a resync BEFORE the ack arrives, so a late/never-arriving
    /// `TabCloseResponse` cannot act on whatever later reuses the same slot.
    /// `purge_federation_state_for_workspaces` alone would miss this — it
    /// keys on WORKSPACE id, and this workspace stays up (only one of its
    /// two tabs is removed).
    #[cfg(unix)]
    #[tokio::test]
    async fn pending_tab_close_is_purged_when_the_tab_is_removed_by_resync() {
        let (mut app, mount, ws_idx, _workspace_id, tab_ids) = mount_two_tab_mirror();
        let tab_id_to_close = app.public_tab_id(ws_idx, 1).unwrap();

        let request_id = 73u64;
        app.register_pending_remote_close(
            request_id,
            PendingRemoteClose {
                workspace_id: app.state.workspaces[ws_idx].id.clone(),
                origin: mount.host_key.clone(),
                target: RemoteCloseTarget::Tab(tab_id_to_close),
            },
        );
        assert!(app.pending_remote_closes.contains_key(&request_id));

        // The resync path removes the same tab first — a plausible race
        // between a resync poll and this test's slower `TabCloseResponse`.
        app.handle_federation_resync_tab_removed(mount.host_key.clone(), tab_ids[1].clone());

        assert!(
            !app.pending_remote_closes.contains_key(&request_id),
            "the pending tab close must be purged once its tab is gone, not left to act \
             on a later slot reuse"
        );
    }

    /// Every close kind mints its `request_id`
    /// from the SAME counter (`next_remote_close_request_id`,
    /// `app/api/panes.rs`) precisely because they all correlate through this
    /// ONE `pending_remote_closes` map — see that function's own doc comment
    /// for why a second counter would be unsafe. Pins the actual invariant
    /// that protects: a pane close and a tab close registered back to back
    /// must occupy two DISTINCT entries, and delivering the pane's ack must
    /// tear down only the pane, leaving the tab's own still-pending entry
    /// untouched. The origin check alone cannot catch a collision here — two
    /// closes on the SAME mount share the identical `HostKey`, so only
    /// distinct `request_id`s (and thus distinct map entries) keep them
    /// apart.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_pane_close_and_a_tab_close_registered_together_occupy_distinct_pending_entries() {
        let (mut app, mount, ws_idx, workspace_id, _tab_ids) = mount_two_tab_mirror();
        let pane_id = app.state.workspaces[ws_idx].tabs[0].root_pane;
        let tab_id_to_close = app.public_tab_id(ws_idx, 1).unwrap();

        let pane_request_id = 80u64;
        let tab_request_id = 81u64;
        app.register_pending_remote_close(
            pane_request_id,
            PendingRemoteClose {
                workspace_id: workspace_id.clone(),
                origin: mount.host_key.clone(),
                target: RemoteCloseTarget::Pane(pane_id),
            },
        );
        app.register_pending_remote_close(
            tab_request_id,
            PendingRemoteClose {
                workspace_id: workspace_id.clone(),
                origin: mount.host_key.clone(),
                target: RemoteCloseTarget::Tab(tab_id_to_close),
            },
        );
        assert_eq!(
            app.pending_remote_closes.len(),
            2,
            "a pane close and a tab close registered together must occupy two distinct entries"
        );

        app.handle_federation_close_pane_ready(pane_request_id, mount.host_key.clone());

        assert!(
            app.find_pane(pane_id).is_none(),
            "the pane's own ack must tear the pane down"
        );
        assert!(
            !app.pending_remote_closes.contains_key(&pane_request_id),
            "the pane's pending entry must be consumed by its own ack"
        );
        assert!(
            app.pending_remote_closes.contains_key(&tab_request_id),
            "a colliding-looking id space must not let the pane's ack pop the tab's still-\
             pending entry — they are different entries entirely, not a shared one"
        );
        assert_eq!(
            app.state.workspaces[ws_idx].tabs.len(),
            1,
            "the pane's ack closed its own (now-empty) tab, not the sibling tab whose close \
             is still pending"
        );
    }

    /// Post-mount pane mirroring fix, part 2 (plans/260722-1327): a resync
    /// diff's `AppEvent::FederationResyncPaneCreated` must splice the new
    /// pane into the already-mounted workspace's active tab, register it in
    /// `public_pane_numbers` (reachable through the public pane-id API), and
    /// record the reverse index a later removal needs.
    #[cfg(unix)]
    #[tokio::test]
    async fn resync_pane_created_splices_into_the_mounted_workspace_with_a_public_pane_number() {
        let mut app = test_app();
        let mount = mount(1);
        let mut mirror = RemoteMirror::new(mount.clone());
        mirror.apply_snapshot(&two_pane_snapshot(), EventCursor(0));

        let mut router = TerminalChannelRouter::new();
        let (out_tx, _out_rx) = tokio::sync::mpsc::unbounded_channel();
        let (clipboard_tx, _clipboard_rx) = tokio::sync::mpsc::unbounded_channel();
        let created = app
            .materialize_federation_mount(&mirror, &mut router, &out_tx, &clipboard_tx)
            .expect("materialization must succeed");
        let ws_idx = created[0];
        let workspace_id = app.state.workspaces[ws_idx].id.clone();

        let (events_tx, _events_rx) = tokio::sync::mpsc::channel::<crate::events::AppEvent>(4);
        let (rt_out_tx, _rt_out_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_output_tx, output_rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(4);
        let (rt_clipboard_tx, _rt_clipboard_rx) = tokio::sync::mpsc::unbounded_channel();
        let render_notify = std::sync::Arc::new(tokio::sync::Notify::new());
        let render_dirty = std::sync::Arc::new(crate::render_signal::RenderSignal::new());

        let local_pane_id = crate::layout::PaneId::alloc();
        let terminal_id = crate::terminal::TerminalId::alloc();
        let runtime = crate::terminal::TerminalRuntime::spawn_remote(
            local_pane_id,
            24,
            80,
            1 << 16,
            crate::terminal_theme::TerminalTheme::default(),
            None,
            terminal_id.to_string(),
            1,
            rt_out_tx,
            output_rx,
            rt_clipboard_tx,
            events_tx,
            render_notify,
            render_dirty,
        )
        .expect("spawn_remote must succeed for a fresh channel pair");
        let terminal =
            crate::terminal::TerminalState::new(terminal_id.clone(), std::path::PathBuf::from("/"));
        let pane_state = crate::pane::PaneState::new(terminal_id.clone());
        let remote_pane_id = format!("{workspace_id}:p9");
        let remote_tab_id = only_tab_id(&mirror);

        app.handle_federation_resync_pane_created(crate::events::FederationResyncPaneCreated {
            origin: mount.host_key.clone(),
            workspace_id: workspace_id.clone(),
            tab_id: remote_tab_id.clone(),
            pane_id: remote_pane_id.clone(),
            local_pane_id,
            terminal_id,
            terminal,
            runtime,
            pane_state,
        });

        let ws = &app.state.workspaces[ws_idx];
        ws.assert_invariants_for_test();
        assert!(
            ws.pane_state(local_pane_id).is_some(),
            "the resync-created pane must be spliced into the mounted workspace"
        );
        assert!(
            ws.public_pane_number(local_pane_id).is_some(),
            "a resync-created pane must get a public pane number, or it is unreachable \
             through the public pane-id API"
        );
        assert_eq!(
            app.remote_resync_pane_index.get(&remote_pane_id),
            Some(&local_pane_id),
            "the reverse index must record this pane so a later removal diff can find it"
        );
    }

    /// Origin-check counterpart of `resync_pane_created_splices_into_the_
    /// mounted_workspace_with_a_public_pane_number`: a resync-created pane
    /// whose mount origin does not match the target workspace's federation
    /// origin must be dropped, not spliced in.
    #[cfg(unix)]
    #[tokio::test]
    async fn resync_pane_created_from_the_wrong_origin_is_dropped() {
        let mut app = test_app();
        let mount = mount(1);
        let mut mirror = RemoteMirror::new(mount.clone());
        mirror.apply_snapshot(&two_pane_snapshot(), EventCursor(0));

        let mut router = TerminalChannelRouter::new();
        let (out_tx, _out_rx) = tokio::sync::mpsc::unbounded_channel();
        let (clipboard_tx, _clipboard_rx) = tokio::sync::mpsc::unbounded_channel();
        let created = app
            .materialize_federation_mount(&mirror, &mut router, &out_tx, &clipboard_tx)
            .expect("materialization must succeed");
        let ws_idx = created[0];
        let workspace_id = app.state.workspaces[ws_idx].id.clone();

        let (events_tx, _events_rx) = tokio::sync::mpsc::channel::<crate::events::AppEvent>(4);
        let (rt_out_tx, _rt_out_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_output_tx, output_rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(4);
        let (rt_clipboard_tx, _rt_clipboard_rx) = tokio::sync::mpsc::unbounded_channel();
        let render_notify = std::sync::Arc::new(tokio::sync::Notify::new());
        let render_dirty = std::sync::Arc::new(crate::render_signal::RenderSignal::new());

        let local_pane_id = crate::layout::PaneId::alloc();
        let terminal_id = crate::terminal::TerminalId::alloc();
        let runtime = crate::terminal::TerminalRuntime::spawn_remote(
            local_pane_id,
            24,
            80,
            1 << 16,
            crate::terminal_theme::TerminalTheme::default(),
            None,
            terminal_id.to_string(),
            1,
            rt_out_tx,
            output_rx,
            rt_clipboard_tx,
            events_tx,
            render_notify,
            render_dirty,
        )
        .expect("spawn_remote must succeed for a fresh channel pair");
        let terminal =
            crate::terminal::TerminalState::new(terminal_id.clone(), std::path::PathBuf::from("/"));
        let pane_state = crate::pane::PaneState::new(terminal_id.clone());
        let remote_pane_id = format!("{workspace_id}:p9");
        let remote_tab_id = only_tab_id(&mirror);
        let spoofed_origin = crate::remote::federation::id::HostKey::new("evil-host", "s1");

        app.handle_federation_resync_pane_created(crate::events::FederationResyncPaneCreated {
            origin: spoofed_origin,
            workspace_id: workspace_id.clone(),
            tab_id: remote_tab_id.clone(),
            pane_id: remote_pane_id.clone(),
            local_pane_id,
            terminal_id,
            terminal,
            runtime,
            pane_state,
        });

        assert!(
            app.state.workspaces[ws_idx]
                .pane_state(local_pane_id)
                .is_none(),
            "a resync-created pane from the wrong origin must not be spliced into any workspace"
        );
        // Gap B fix (plans/260724-1536-federation-pane-close-sync):
        // `materialize_federation_mount` now indexes every mount-time pane
        // too, so the index is no longer empty at this point — only the
        // REJECTED pane's own id must be absent from it.
        assert!(
            !app.remote_resync_pane_index.contains_key(&remote_pane_id),
            "a resync-created pane from the wrong origin must not be added to the reverse index"
        );
    }

    /// Post-mount pane mirroring fix, part 2 (plans/260722-1327): a resync
    /// diff's `AppEvent::FederationResyncPaneRemoved` must tear down the
    /// matching local pane (via the reverse index recorded at pane-create
    /// time) and its runtime.
    #[cfg(unix)]
    #[tokio::test]
    async fn resync_pane_removed_tears_down_the_local_pane_and_runtime() {
        let mut app = test_app();
        let mount = mount(1);
        let mut mirror = RemoteMirror::new(mount.clone());
        mirror.apply_snapshot(&two_pane_snapshot(), EventCursor(0));

        let mut router = TerminalChannelRouter::new();
        let (out_tx, _out_rx) = tokio::sync::mpsc::unbounded_channel();
        let (clipboard_tx, _clipboard_rx) = tokio::sync::mpsc::unbounded_channel();
        let created = app
            .materialize_federation_mount(&mirror, &mut router, &out_tx, &clipboard_tx)
            .expect("materialization must succeed");
        let ws_idx = created[0];
        let workspace_id = app.state.workspaces[ws_idx].id.clone();

        let (events_tx, _events_rx) = tokio::sync::mpsc::channel::<crate::events::AppEvent>(4);
        let (rt_out_tx, _rt_out_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_output_tx, output_rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(4);
        let (rt_clipboard_tx, _rt_clipboard_rx) = tokio::sync::mpsc::unbounded_channel();
        let render_notify = std::sync::Arc::new(tokio::sync::Notify::new());
        let render_dirty = std::sync::Arc::new(crate::render_signal::RenderSignal::new());

        let local_pane_id = crate::layout::PaneId::alloc();
        let terminal_id = crate::terminal::TerminalId::alloc();
        let runtime = crate::terminal::TerminalRuntime::spawn_remote(
            local_pane_id,
            24,
            80,
            1 << 16,
            crate::terminal_theme::TerminalTheme::default(),
            None,
            terminal_id.to_string(),
            1,
            rt_out_tx,
            output_rx,
            rt_clipboard_tx,
            events_tx,
            render_notify,
            render_dirty,
        )
        .expect("spawn_remote must succeed for a fresh channel pair");
        let terminal =
            crate::terminal::TerminalState::new(terminal_id.clone(), std::path::PathBuf::from("/"));
        let pane_state = crate::pane::PaneState::new(terminal_id.clone());
        let remote_pane_id = format!("{workspace_id}:p9");
        let remote_tab_id = only_tab_id(&mirror);

        app.handle_federation_resync_pane_created(crate::events::FederationResyncPaneCreated {
            origin: mount.host_key.clone(),
            workspace_id: workspace_id.clone(),
            tab_id: remote_tab_id.clone(),
            pane_id: remote_pane_id.clone(),
            local_pane_id,
            terminal_id: terminal_id.clone(),
            terminal,
            runtime,
            pane_state,
        });
        assert!(app.state.workspaces[ws_idx]
            .pane_state(local_pane_id)
            .is_some());
        assert!(app.terminal_runtimes.get(&terminal_id).is_some());

        app.handle_federation_resync_pane_removed(mount.host_key.clone(), remote_pane_id.clone());

        assert!(
            app.find_pane(local_pane_id).is_none(),
            "the resync-removed pane must no longer exist in any workspace"
        );
        assert!(
            app.terminal_runtimes.get(&terminal_id).is_none(),
            "the resync-removed pane's runtime must be torn down"
        );
        assert!(
            !app.remote_resync_pane_index.contains_key(&remote_pane_id),
            "the reverse index entry must be consumed on removal"
        );
    }

    /// Memory-leak regression (plans/260724-1536-federation-pane-close-sync
    /// pre-merge review): `handle_workspace_close`'s locally-initiated
    /// federated unmount purges `pending_remote_splits`/`_closes`/
    /// `_clipboard_stages` for the closing workspaces but, before this fix,
    /// never purged `remote_resync_pane_index` — and since every mount-time
    /// pane is now indexed there (Gap B fix above), each unmount leaked one
    /// entry per pane forever. Builds a real mount via
    /// `materialize_federation_mount` (so the index is populated the same
    /// way production does it), adds a manually-inserted entry standing in
    /// for a pane that belongs to a *different*, still-live workspace, then
    /// asserts the purge removes only the closing workspace's entries.
    #[cfg(unix)]
    #[tokio::test]
    async fn unmount_purges_remote_resync_pane_index_for_closing_workspace_only() {
        let mut app = test_app();
        let mount = mount(1);
        let mut mirror = RemoteMirror::new(mount.clone());
        mirror.apply_snapshot(&two_pane_snapshot(), EventCursor(0));

        let mut router = TerminalChannelRouter::new();
        let (out_tx, _out_rx) = tokio::sync::mpsc::unbounded_channel();
        let (clipboard_tx, _clipboard_rx) = tokio::sync::mpsc::unbounded_channel();
        let created = app
            .materialize_federation_mount(&mirror, &mut router, &out_tx, &clipboard_tx)
            .expect("materialization must succeed");
        let ws_idx = created[0];
        let closing_workspace_id = app.state.workspaces[ws_idx].id.clone();

        assert!(
            !app.remote_resync_pane_index.is_empty(),
            "materializing the mount must have indexed its mount-time panes, or this test \
             checks nothing"
        );

        // Stand in for an entry belonging to a different, still-live
        // workspace: a `PaneId` deliberately not part of `ws_idx`'s layout.
        let other_remote_pane_id = "other-workspace:p1".to_string();
        let other_local_pane_id = crate::layout::PaneId::alloc();
        app.remote_resync_pane_index
            .insert(other_remote_pane_id.clone(), other_local_pane_id);

        let mut closing_ids = std::collections::HashSet::new();
        closing_ids.insert(closing_workspace_id);
        app.purge_remote_resync_pane_index_for_workspaces(&closing_ids);

        for tab in &app.state.workspaces[ws_idx].tabs {
            for pane_id in tab.layout.pane_ids() {
                assert!(
                    !app.remote_resync_pane_index
                        .values()
                        .any(|local_pane_id| *local_pane_id == pane_id),
                    "the closing workspace's pane must no longer be indexed after purge"
                );
            }
        }
        assert_eq!(
            app.remote_resync_pane_index.get(&other_remote_pane_id),
            Some(&other_local_pane_id),
            "an entry belonging to a different, still-live workspace must survive the purge"
        );
    }

    /// Gap B regression (plans/260724-1536-federation-pane-close-sync):
    /// before the fix, `build_remote_pane` never populated
    /// `remote_resync_pane_index` for a MOUNT-TIME pane (only resync- and
    /// split-created panes did), so a serving-host close of a pane that was
    /// present at initial mount time silently no-op'd in
    /// `handle_federation_resync_pane_removed` — the overwhelming majority
    /// of panes a user actually sees. Builds a real 2-pane mount through
    /// `materialize_federation_mount`/`build_remote_pane` (not a hand-built
    /// fixture that skips it) and asserts a mount-time pane's removal now
    /// tears the local pane down.
    #[cfg(unix)]
    #[tokio::test]
    async fn resync_removed_tears_down_a_mount_time_pane_not_just_resync_created_ones() {
        let mut app = test_app();
        let mount = mount(1);
        let mut mirror = RemoteMirror::new(mount.clone());
        mirror.apply_snapshot(&two_pane_snapshot(), EventCursor(0));

        let mut router = TerminalChannelRouter::new();
        let (out_tx, _out_rx) = tokio::sync::mpsc::unbounded_channel();
        let (clipboard_tx, _clipboard_rx) = tokio::sync::mpsc::unbounded_channel();
        let created = app
            .materialize_federation_mount(&mirror, &mut router, &out_tx, &clipboard_tx)
            .expect("materialization must succeed");
        let ws_idx = created[0];

        // `two_pane_snapshot()` seeds panes "p1" (root) and "p2" (split
        // sibling) into the same tab, sorted by pane id — "p1" materializes
        // as the tab's root pane. The reverse-index key is the mirror's own
        // namespaced (`FedRef`-mapped) id, not the raw snapshot pane id, so
        // find it by value (the local `PaneId` `build_remote_pane` actually
        // produced) rather than assuming its exact wire encoding.
        let root_pane_id = app.state.workspaces[ws_idx].tabs[0].root_pane;
        let root_remote_pane_id = app
            .remote_resync_pane_index
            .iter()
            .find_map(|(remote_id, local_id)| {
                (*local_id == root_pane_id).then(|| remote_id.clone())
            })
            .expect(
                "regression check: a mount-time root pane must be indexed by build_remote_pane, \
                 or this whole test is checking nothing",
            );

        app.handle_federation_resync_pane_removed(
            mount.host_key.clone(),
            root_remote_pane_id.clone(),
        );

        assert!(
            app.find_pane(root_pane_id).is_none(),
            "a mount-time pane's removal must tear the local pane down, not silently no-op"
        );
        assert!(
            !app.remote_resync_pane_index
                .contains_key(&root_remote_pane_id),
            "the reverse index entry must be consumed on removal"
        );
    }

    /// Non-regression counterpart: a resync/split-created pane's removal
    /// (already covered above by `resync_pane_removed_tears_down_the_local_
    /// pane_and_runtime`) must keep working unchanged after the Gap B fix —
    /// this just re-asserts that same behavior survives alongside the new
    /// mount-time indexing.
    #[cfg(unix)]
    #[tokio::test]
    async fn resync_removed_still_tears_down_a_resync_created_pane_after_the_gap_b_fix() {
        let mut app = test_app();
        let mount = mount(1);
        let mut mirror = RemoteMirror::new(mount.clone());
        mirror.apply_snapshot(&two_pane_snapshot(), EventCursor(0));

        let mut router = TerminalChannelRouter::new();
        let (out_tx, _out_rx) = tokio::sync::mpsc::unbounded_channel();
        let (clipboard_tx, _clipboard_rx) = tokio::sync::mpsc::unbounded_channel();
        let created = app
            .materialize_federation_mount(&mirror, &mut router, &out_tx, &clipboard_tx)
            .expect("materialization must succeed");
        let ws_idx = created[0];
        let workspace_id = app.state.workspaces[ws_idx].id.clone();

        let (events_tx, _events_rx) = tokio::sync::mpsc::channel::<crate::events::AppEvent>(4);
        let (rt_out_tx, _rt_out_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_output_tx, output_rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(4);
        let (rt_clipboard_tx, _rt_clipboard_rx) = tokio::sync::mpsc::unbounded_channel();
        let render_notify = std::sync::Arc::new(tokio::sync::Notify::new());
        let render_dirty = std::sync::Arc::new(crate::render_signal::RenderSignal::new());

        let local_pane_id = crate::layout::PaneId::alloc();
        let terminal_id = crate::terminal::TerminalId::alloc();
        let runtime = crate::terminal::TerminalRuntime::spawn_remote(
            local_pane_id,
            24,
            80,
            1 << 16,
            crate::terminal_theme::TerminalTheme::default(),
            None,
            terminal_id.to_string(),
            1,
            rt_out_tx,
            output_rx,
            rt_clipboard_tx,
            events_tx,
            render_notify,
            render_dirty,
        )
        .expect("spawn_remote must succeed for a fresh channel pair");
        let terminal =
            crate::terminal::TerminalState::new(terminal_id.clone(), std::path::PathBuf::from("/"));
        let pane_state = crate::pane::PaneState::new(terminal_id.clone());
        let remote_pane_id = format!("{workspace_id}:p9");
        let remote_tab_id = only_tab_id(&mirror);

        app.handle_federation_resync_pane_created(crate::events::FederationResyncPaneCreated {
            origin: mount.host_key.clone(),
            workspace_id: workspace_id.clone(),
            tab_id: remote_tab_id.clone(),
            pane_id: remote_pane_id.clone(),
            local_pane_id,
            terminal_id: terminal_id.clone(),
            terminal,
            runtime,
            pane_state,
        });
        assert!(app.state.workspaces[ws_idx]
            .pane_state(local_pane_id)
            .is_some());

        app.handle_federation_resync_pane_removed(mount.host_key.clone(), remote_pane_id.clone());

        assert!(
            app.find_pane(local_pane_id).is_none(),
            "a resync-created pane's removal must still work unchanged after the Gap B fix"
        );
    }

    /// Builds the fully-formed local pane payload a mount's drive task hands
    /// back on `AppEvent::FederationResyncPaneCreated`, minus the routing
    /// fields each caller sets. Same construction the older resync tests
    /// spell out inline; factored out because the multi-tab tests below need
    /// it repeatedly.
    #[cfg(unix)]
    fn resync_pane_payload() -> (
        crate::layout::PaneId,
        crate::terminal::TerminalId,
        crate::terminal::TerminalState,
        crate::terminal::TerminalRuntime,
        crate::pane::PaneState,
    ) {
        let (events_tx, _events_rx) = tokio::sync::mpsc::channel::<crate::events::AppEvent>(4);
        let (rt_out_tx, _rt_out_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_output_tx, output_rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(4);
        let (rt_clipboard_tx, _rt_clipboard_rx) = tokio::sync::mpsc::unbounded_channel();
        let render_notify = std::sync::Arc::new(tokio::sync::Notify::new());
        let render_dirty = std::sync::Arc::new(crate::render_signal::RenderSignal::new());

        let local_pane_id = crate::layout::PaneId::alloc();
        let terminal_id = crate::terminal::TerminalId::alloc();
        let runtime = crate::terminal::TerminalRuntime::spawn_remote(
            local_pane_id,
            24,
            80,
            1 << 16,
            crate::terminal_theme::TerminalTheme::default(),
            None,
            terminal_id.to_string(),
            1,
            rt_out_tx,
            output_rx,
            rt_clipboard_tx,
            events_tx,
            render_notify,
            render_dirty,
        )
        .expect("spawn_remote must succeed for a fresh channel pair");
        let terminal =
            crate::terminal::TerminalState::new(terminal_id.clone(), std::path::PathBuf::from("/"));
        let pane_state = crate::pane::PaneState::new(terminal_id.clone());
        (local_pane_id, terminal_id, terminal, runtime, pane_state)
    }

    /// Mounts a two-tab mirror and returns the app plus the materialized
    /// workspace index, its local id, and both namespaced remote tab ids in
    /// remote tab-number order.
    #[cfg(unix)]
    fn mount_two_tab_mirror() -> (App, Mount, usize, String, Vec<String>) {
        let mut app = test_app();
        let mount = mount(1);
        let mut mirror = RemoteMirror::new(mount.clone());
        mirror.apply_snapshot(&two_tab_snapshot(), EventCursor(0));

        let mut router = TerminalChannelRouter::new();
        let (out_tx, _out_rx) = tokio::sync::mpsc::unbounded_channel();
        let (clipboard_tx, _clipboard_rx) = tokio::sync::mpsc::unbounded_channel();
        let created = app
            .materialize_federation_mount(&mirror, &mut router, &out_tx, &clipboard_tx)
            .expect("materialization must succeed");
        let ws_idx = created[0];
        let workspace_id = app.state.workspaces[ws_idx].id.clone();

        let mut tabs: Vec<_> = mirror.tabs().values().collect();
        tabs.sort_by_key(|tab| tab.number);
        let tab_ids = tabs.iter().map(|tab| tab.tab_id.clone()).collect();
        (app, mount, ws_idx, workspace_id, tab_ids)
    }

    /// Regression guard for the reported collapse: a remote workspace with N
    /// tabs must materialize as N local tabs, not one tab with N splits.
    #[cfg(unix)]
    #[tokio::test]
    async fn multi_tab_mount_materializes_one_local_tab_per_remote_tab() {
        let (app, _mount, ws_idx, _workspace_id, tab_ids) = mount_two_tab_mirror();

        let ws = &app.state.workspaces[ws_idx];
        ws.assert_invariants_for_test();
        assert_eq!(ws.tabs.len(), 2, "each remote tab gets its own local tab");
        for tab in &ws.tabs {
            assert_eq!(
                tab.panes.len(),
                1,
                "each remote tab's single pane must stay in its own tab, not become a split"
            );
        }
        assert_eq!(
            ws.tabs
                .iter()
                .map(|tab| tab.custom_name.clone())
                .collect::<Vec<_>>(),
            vec![
                Some("first remote tab".to_string()),
                Some("second remote tab".to_string())
            ],
            "each local tab carries its remote counterpart's label"
        );

        for tab_id in &tab_ids {
            let tab_ref = app
                .remote_resync_tab_index
                .get(tab_id)
                .expect("every mount-time tab must be indexed for later resyncs");
            assert!(tab_ref.tab_number.is_some());
        }
    }

    /// A resync pane whose remote tab this mount already materialized must
    /// land in THAT tab — including when it is not the workspace's active
    /// tab, which is exactly what the old `Workspace::active_tab` fallback
    /// got wrong.
    #[cfg(unix)]
    #[tokio::test]
    async fn resync_pane_created_with_a_known_tab_id_lands_in_that_tab() {
        let (mut app, mount, ws_idx, workspace_id, tab_ids) = mount_two_tab_mirror();
        app.state.workspaces[ws_idx].active_tab = 0;

        let (local_pane_id, terminal_id, terminal, runtime, pane_state) = resync_pane_payload();
        app.handle_federation_resync_pane_created(crate::events::FederationResyncPaneCreated {
            origin: mount.host_key.clone(),
            workspace_id: workspace_id.clone(),
            tab_id: tab_ids[1].clone(),
            pane_id: format!("{workspace_id}:p9"),
            local_pane_id,
            terminal_id,
            terminal,
            runtime,
            pane_state,
        });

        let ws = &app.state.workspaces[ws_idx];
        ws.assert_invariants_for_test();
        assert_eq!(ws.tabs.len(), 2, "a known tab must not spawn another tab");
        assert_eq!(
            ws.find_tab_index_for_pane(local_pane_id),
            Some(1),
            "the pane must be spliced into its own remote tab, not the active one"
        );
        assert!(ws.public_pane_number(local_pane_id).is_some());
    }

    /// The reported bug, at the event level: a resync pane for a remote tab
    /// this mount has never seen must create a new local tab instead of
    /// splitting the active one.
    #[cfg(unix)]
    #[tokio::test]
    async fn resync_pane_created_with_an_unknown_tab_id_creates_a_new_local_tab() {
        let (mut app, mount, ws_idx, workspace_id, _tab_ids) = mount_two_tab_mirror();
        let unknown_tab_id = format!("{workspace_id}:tab-new");
        app.handle_federation_resync_tab_created(
            mount.host_key.clone(),
            workspace_id.clone(),
            unknown_tab_id.clone(),
            "third remote tab".to_string(),
        );

        let (local_pane_id, terminal_id, terminal, runtime, pane_state) = resync_pane_payload();
        app.handle_federation_resync_pane_created(crate::events::FederationResyncPaneCreated {
            origin: mount.host_key.clone(),
            workspace_id: workspace_id.clone(),
            tab_id: unknown_tab_id.clone(),
            pane_id: format!("{workspace_id}:p9"),
            local_pane_id,
            terminal_id,
            terminal,
            runtime,
            pane_state,
        });

        let ws = &app.state.workspaces[ws_idx];
        ws.assert_invariants_for_test();
        assert_eq!(ws.tabs.len(), 3, "an unseen remote tab must create a tab");
        let tab_idx = ws
            .find_tab_index_for_pane(local_pane_id)
            .expect("the new pane must be placed");
        assert_eq!(tab_idx, 2);
        assert_eq!(ws.tabs[tab_idx].panes.len(), 1, "it is not a split");
        assert_eq!(
            ws.tabs[tab_idx].custom_name.as_deref(),
            Some("third remote tab"),
            "the tab-created event's label must reach the materialized tab"
        );
        assert!(ws.public_pane_number(local_pane_id).is_some());
        assert_eq!(
            app.remote_resync_tab_index
                .get(&unknown_tab_id)
                .and_then(|tab_ref| tab_ref.tab_number),
            ws.public_tab_number(tab_idx),
            "the new tab must be indexed so its next pane splits into it"
        );
    }

    /// Tab created then closed over resync: the local tab appears and then
    /// goes away again, leaving no stale tab- or pane-index entries and a
    /// still-valid workspace.
    #[cfg(unix)]
    #[tokio::test]
    async fn resync_tab_created_then_closed_round_trip_leaves_state_consistent() {
        let (mut app, mount, ws_idx, workspace_id, _tab_ids) = mount_two_tab_mirror();
        let new_tab_id = format!("{workspace_id}:tab-new");
        let new_pane_id = format!("{workspace_id}:p9");
        app.handle_federation_resync_tab_created(
            mount.host_key.clone(),
            workspace_id.clone(),
            new_tab_id.clone(),
            "third remote tab".to_string(),
        );

        let (local_pane_id, terminal_id, terminal, runtime, pane_state) = resync_pane_payload();
        app.handle_federation_resync_pane_created(crate::events::FederationResyncPaneCreated {
            origin: mount.host_key.clone(),
            workspace_id: workspace_id.clone(),
            tab_id: new_tab_id.clone(),
            pane_id: new_pane_id.clone(),
            local_pane_id,
            terminal_id,
            terminal,
            runtime,
            pane_state,
        });
        assert_eq!(app.state.workspaces[ws_idx].tabs.len(), 3);

        app.handle_federation_resync_tab_removed(mount.host_key.clone(), new_tab_id.clone());

        // `materialize_federation_mount` itself never focuses the workspace
        // it creates (the mount caller does), so the App-level invariants —
        // which require an active workspace — need that set first.
        app.state.active = Some(ws_idx);
        app.state.assert_invariants_for_test();
        let ws = &app.state.workspaces[ws_idx];
        ws.assert_invariants_for_test();
        assert_eq!(ws.tabs.len(), 2, "the resync-created tab must be gone");
        assert!(ws.pane_state(local_pane_id).is_none());
        assert!(
            !app.remote_resync_tab_index.contains_key(&new_tab_id),
            "a closed remote tab must not leave a stale tab-index entry"
        );
        assert!(
            !app.remote_resync_pane_index.contains_key(&new_pane_id),
            "closing a tab must prune its panes from the pane index too"
        );
    }

    /// A remote tab close from a host other than the one that mounted the
    /// workspace must not tear the local tab down — and must not evict its
    /// index entry on the way to refusing. Authorization has to happen before
    /// the mutation: a wiped entry sends the next resync pane for that live
    /// remote tab down the "unknown tab" branch, building a duplicate local
    /// tab beside the real one.
    #[cfg(unix)]
    #[tokio::test]
    async fn resync_tab_removed_from_the_wrong_origin_is_dropped_and_keeps_the_index() {
        let (mut app, _mount, ws_idx, _workspace_id, tab_ids) = mount_two_tab_mirror();
        let spoofed_origin = crate::remote::federation::id::HostKey::new("evil-host", "s1");
        let before = app
            .remote_resync_tab_index
            .get(&tab_ids[1])
            .map(|tab_ref| (tab_ref.workspace_id.clone(), tab_ref.tab_number))
            .expect("a mounted tab is indexed");

        app.handle_federation_resync_tab_removed(spoofed_origin, tab_ids[1].clone());

        assert_eq!(
            app.state.workspaces[ws_idx].tabs.len(),
            2,
            "a foreign origin must not be able to close another mount's tab"
        );
        let after = app
            .remote_resync_tab_index
            .get(&tab_ids[1])
            .map(|tab_ref| (tab_ref.workspace_id.clone(), tab_ref.tab_number));
        assert_eq!(
            after,
            Some(before),
            "a refused removal must leave the tab index exactly as it found it"
        );
    }

    /// The multi-workspace acceptance criterion: a workspace that appears on
    /// the mounted host after mount — because the serving host created one,
    /// or because this client asked it to with a `WorkspaceCreateRequest` —
    /// must materialize as a real *second* local workspace carrying the
    /// mirror's namespaced id and the mount's federation membership, so the
    /// federation-origin classification groups it with the first.
    #[cfg(unix)]
    #[tokio::test]
    async fn resync_workspace_created_materializes_a_second_federated_workspace() {
        let (mut app, mount, first_ws_idx, first_workspace_id, _tab_ids) = mount_two_tab_mirror();
        let new_workspace_id = format!("r:{}:w2", mount.host_key.as_str());
        let new_tab_id = format!("r:{}:w2-tab", mount.host_key.as_str());
        let new_pane_id = format!("r:{}:w2p1", mount.host_key.as_str());

        app.handle_federation_resync_workspace_created(
            mount.host_key.clone(),
            new_workspace_id.clone(),
            "second remote workspace".to_string(),
        );
        app.handle_federation_resync_tab_created(
            mount.host_key.clone(),
            new_workspace_id.clone(),
            new_tab_id.clone(),
            "w2 root tab".to_string(),
        );
        assert_eq!(
            app.state.workspaces.len(),
            1,
            "the announcement alone must not create a workspace: a Tab needs a pane"
        );

        let (local_pane_id, terminal_id, terminal, runtime, pane_state) = resync_pane_payload();
        app.handle_federation_resync_pane_created(crate::events::FederationResyncPaneCreated {
            origin: mount.host_key.clone(),
            workspace_id: new_workspace_id.clone(),
            tab_id: new_tab_id.clone(),
            pane_id: new_pane_id.clone(),
            local_pane_id,
            terminal_id,
            terminal,
            runtime,
            pane_state,
        });

        assert_eq!(app.state.workspaces.len(), 2, "a second workspace exists");
        let new_ws_idx = app
            .state
            .workspaces
            .iter()
            .position(|ws| ws.id == new_workspace_id)
            .expect("the new workspace carries the mirror's namespaced id verbatim");
        assert_ne!(new_ws_idx, first_ws_idx);
        assert_ne!(new_workspace_id, first_workspace_id);

        let ws = &app.state.workspaces[new_ws_idx];
        ws.assert_invariants_for_test();
        // The exact primitive `ui::sidebar::workspace_federation_origin` is
        // built on — the badge/grouping must classify this as federated.
        assert!(matches!(classify(&ws.id), IdClass::Remote(host) if host == mount.host_key));
        assert_eq!(
            ws.worktree_space().map(|space| space.key.clone()),
            Some(format!("federation:{}", mount.host_key.as_str())),
            "the new workspace joins the same mount's federation group"
        );
        assert_eq!(ws.display_name(), "second remote workspace");
        assert_eq!(ws.tabs.len(), 1);
        assert_eq!(
            ws.tabs[0].custom_name.as_deref(),
            Some("w2 root tab"),
            "the announced tab label reaches the materialized root tab"
        );
        assert!(ws.public_pane_number(local_pane_id).is_some());

        assert_eq!(
            app.remote_resync_pane_index.get(&new_pane_id),
            Some(&local_pane_id),
            "the new workspace's pane is indexed for later resync removals"
        );
        assert!(
            app.remote_resync_tab_index
                .get(&new_tab_id)
                .and_then(|tab_ref| tab_ref.tab_number)
                .is_some(),
            "the new workspace's tab is bound to a live local tab number"
        );
        assert!(
            !app.remote_resync_workspace_index
                .contains_key(&new_workspace_id),
            "the pending announcement is consumed once the workspace is real"
        );
        // `materialize_federation_mount` never focuses what it creates, and
        // the whole-state invariants require an active workspace — set it.
        app.state.active = Some(first_ws_idx);
        app.state.assert_invariants_for_test();
    }

    /// Removal is the mirror image: the workspace disappears and every index
    /// entry pointing into it is pruned, so a later remount cannot resolve a
    /// stale id onto a dead workspace/tab/pane.
    #[cfg(unix)]
    #[tokio::test]
    async fn resync_workspace_removed_prunes_the_workspace_and_its_index_entries() {
        let (mut app, mount, _ws_idx, _first_workspace_id, _tab_ids) = mount_two_tab_mirror();
        let new_workspace_id = format!("r:{}:w2", mount.host_key.as_str());
        let new_tab_id = format!("r:{}:w2-tab", mount.host_key.as_str());
        let new_pane_id = format!("r:{}:w2p1", mount.host_key.as_str());

        app.handle_federation_resync_workspace_created(
            mount.host_key.clone(),
            new_workspace_id.clone(),
            "second remote workspace".to_string(),
        );
        app.handle_federation_resync_tab_created(
            mount.host_key.clone(),
            new_workspace_id.clone(),
            new_tab_id.clone(),
            "w2 root tab".to_string(),
        );
        let (local_pane_id, terminal_id, terminal, runtime, pane_state) = resync_pane_payload();
        app.handle_federation_resync_pane_created(crate::events::FederationResyncPaneCreated {
            origin: mount.host_key.clone(),
            workspace_id: new_workspace_id.clone(),
            tab_id: new_tab_id.clone(),
            pane_id: new_pane_id.clone(),
            local_pane_id,
            terminal_id,
            terminal,
            runtime,
            pane_state,
        });
        assert_eq!(app.state.workspaces.len(), 2);

        app.handle_federation_resync_workspace_removed(
            mount.host_key.clone(),
            new_workspace_id.clone(),
        );

        assert_eq!(
            app.state.workspaces.len(),
            1,
            "the retired remote workspace is gone locally"
        );
        assert!(app
            .state
            .workspaces
            .iter()
            .all(|ws| ws.id != new_workspace_id));
        assert!(!app.remote_resync_pane_index.contains_key(&new_pane_id));
        assert!(!app.remote_resync_tab_index.contains_key(&new_tab_id));
        assert!(!app
            .remote_resync_workspace_index
            .contains_key(&new_workspace_id));
        app.state.active = Some(0);
        app.state.assert_invariants_for_test();
        for ws in &app.state.workspaces {
            ws.assert_invariants_for_test();
        }
    }

    /// Origin fence, both halves: a second mount must be able neither to
    /// announce a workspace under another mount's namespace nor to close a
    /// workspace that mount materialized.
    #[cfg(unix)]
    #[tokio::test]
    async fn resync_workspace_events_from_the_wrong_origin_are_dropped() {
        let (mut app, mount, _ws_idx, first_workspace_id, _tab_ids) = mount_two_tab_mirror();
        let spoofed_origin = crate::remote::federation::id::HostKey::new("evil-host", "s1");
        let victim_workspace_id = format!("r:{}:w2", mount.host_key.as_str());

        app.handle_federation_resync_workspace_created(
            spoofed_origin.clone(),
            victim_workspace_id.clone(),
            "smuggled".to_string(),
        );
        assert!(
            !app.remote_resync_workspace_index
                .contains_key(&victim_workspace_id),
            "a foreign origin must not announce a workspace under another mount's namespace"
        );

        // A pending announcement made by the real mount must survive a foreign
        // mount's removal for the same id: wiping it would strand the pane
        // event that was going to materialize that workspace, with no repair
        // path.
        let pending_id = format!("r:{}:w3", mount.host_key.as_str());
        app.handle_federation_resync_workspace_created(
            mount.host_key.clone(),
            pending_id.clone(),
            "announced".to_string(),
        );
        app.handle_federation_resync_workspace_removed(spoofed_origin.clone(), pending_id.clone());
        assert!(
            app.remote_resync_workspace_index.contains_key(&pending_id),
            "a refused removal must not evict another mount's pending announcement"
        );

        app.handle_federation_resync_workspace_removed(spoofed_origin, first_workspace_id.clone());
        assert!(
            app.state
                .workspaces
                .iter()
                .any(|ws| ws.id == first_workspace_id),
            "a foreign origin must not be able to close another mount's workspace"
        );
        // The materialized workspace's own tab entries must be untouched too:
        // the removal handler purges the tab index for the workspaces it
        // closes, and a refused removal closes nothing.
        assert!(
            app.remote_resync_tab_index
                .values()
                .any(|tab_ref| tab_ref.workspace_id == first_workspace_id),
            "a refused workspace removal must not purge that workspace's index entries"
        );
    }

    /// The client-triggered half: a plain `workspace.create` — the method
    /// every live "new workspace" path funnels into
    /// (`App::begin_tui_workspace_create`, `App::run`'s
    /// `request_new_workspace` drain, and the CLI/JSON API, all via
    /// `runtime_workspace_create`) — must go out over the mount as a
    /// `WorkspaceCreateRequest` when the workspace it is created from is
    /// federation-owned, instead of spawning a local workspace. The
    /// name-prompt dialog's own route is covered separately by
    /// `named_workspace_create_inside_a_federated_workspace_goes_out_over_the_mount`.
    #[cfg(unix)]
    #[tokio::test]
    async fn workspace_create_inside_a_federated_workspace_goes_out_over_the_mount() {
        let mut app = test_app();
        let mount = mount(1);
        let mut mirror = RemoteMirror::new(mount.clone());
        mirror.apply_snapshot(&two_tab_snapshot(), EventCursor(0));

        let mut router = TerminalChannelRouter::new();
        let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel();
        let (clipboard_tx, _clipboard_rx) = tokio::sync::mpsc::unbounded_channel();
        let created = app
            .materialize_federation_mount(&mirror, &mut router, &out_tx, &clipboard_tx)
            .expect("materialization must succeed");
        let ws_idx = created[0];
        // `federation_host_key_for_workspace` resolves the mount through the
        // live mirror registry, the same way the production mount path
        // registers it.
        app.state
            .remote_mirrors
            .insert(mount.host_key.clone(), mirror);
        app.state.active = Some(ws_idx);
        app.state.selected = ws_idx;
        // Drain the frames materialization itself emitted (terminal opens).
        while out_rx.try_recv().is_ok() {}
        let before = app.state.workspaces.len();

        let response =
            app.handle_api_request_after_internal_events_drained(crate::api::schema::Request {
                id: "tui.workspace.create".to_string(),
                method: crate::api::schema::Method::WorkspaceCreate(
                    crate::api::schema::WorkspaceCreateParams {
                        cwd: None,
                        focus: true,
                        label: Some("remote scratch".to_string()),
                        env: Default::default(),
                    },
                ),
            });

        let value: serde_json::Value =
            serde_json::from_str(&response).expect("the API always answers valid JSON");
        // A sent request is a success, not a failure: the create really was
        // accepted and dispatched, it just completes asynchronously.
        assert_eq!(
            value
                .get("result")
                .and_then(|result| result.get("type"))
                .and_then(|kind| kind.as_str()),
            Some("workspace_create_requested"),
            "unexpected response: {response}"
        );
        assert_eq!(
            value
                .get("result")
                .and_then(|result| result.get("origin"))
                .and_then(|origin| origin.as_str()),
            Some(mount.host_key.as_str()),
            "the acknowledgement must name the mount it was sent over: {response}"
        );
        assert_eq!(
            app.state.workspaces.len(),
            before,
            "no local workspace may be spawned for a remote-targeted create"
        );

        let mut sent_label = None;
        while let Ok(frame) = out_rx.try_recv() {
            if let FederationMessage::WorkspaceCreateRequest(request) = frame {
                sent_label = Some(request.label);
            }
        }
        assert_eq!(
            sent_label,
            Some(Some("remote scratch".to_string())),
            "the mount must have received a WorkspaceCreateRequest carrying the label hint"
        );
    }

    /// The fence on that routing: an explicit `cwd` is a deliberate
    /// local-directory choice (the remote host's filesystem is a different
    /// namespace), so it must still create a local workspace even while a
    /// federated workspace is in focus.
    #[cfg(unix)]
    #[tokio::test]
    async fn workspace_create_with_an_explicit_cwd_stays_local_inside_a_federated_workspace() {
        let mut app = test_app();
        let mount = mount(1);
        let mut mirror = RemoteMirror::new(mount.clone());
        mirror.apply_snapshot(&two_tab_snapshot(), EventCursor(0));

        let mut router = TerminalChannelRouter::new();
        let (out_tx, _out_rx) = tokio::sync::mpsc::unbounded_channel();
        let (clipboard_tx, _clipboard_rx) = tokio::sync::mpsc::unbounded_channel();
        let created = app
            .materialize_federation_mount(&mirror, &mut router, &out_tx, &clipboard_tx)
            .expect("materialization must succeed");
        let ws_idx = created[0];
        app.state
            .remote_mirrors
            .insert(mount.host_key.clone(), mirror);
        app.state.active = Some(ws_idx);
        app.state.selected = ws_idx;
        let before = app.state.workspaces.len();

        let temp = std::env::temp_dir();
        let response =
            app.handle_api_request_after_internal_events_drained(crate::api::schema::Request {
                id: "tui.workspace.create_cwd".to_string(),
                method: crate::api::schema::Method::WorkspaceCreate(
                    crate::api::schema::WorkspaceCreateParams {
                        cwd: Some(temp.display().to_string()),
                        focus: true,
                        label: None,
                        env: Default::default(),
                    },
                ),
            });

        assert!(
            !response.contains("workspace_create_requested"),
            "an explicit cwd must not be routed to the remote host: {response}"
        );
        assert_eq!(
            app.state.workspaces.len(),
            before + 1,
            "an explicit cwd creates a real local workspace"
        );
    }

    /// Mounts a two-tab mirror, registers it in the live mirror registry and
    /// focuses it, returning the app, the mount, the mirrored workspace index
    /// and the mount's outbound frame receiver. The registry entry is what
    /// `federation_host_key_for_workspace` resolves a mount through, exactly
    /// as the production mount path registers it.
    #[cfg(unix)]
    fn mounted_and_focused_mirror() -> (
        App,
        Mount,
        usize,
        tokio::sync::mpsc::UnboundedReceiver<FederationMessage>,
    ) {
        let mut app = test_app();
        let mount = mount(1);
        let mut mirror = RemoteMirror::new(mount.clone());
        mirror.apply_snapshot(&two_tab_snapshot(), EventCursor(0));

        let mut router = TerminalChannelRouter::new();
        let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel();
        let (clipboard_tx, _clipboard_rx) = tokio::sync::mpsc::unbounded_channel();
        let created = app
            .materialize_federation_mount(&mirror, &mut router, &out_tx, &clipboard_tx)
            .expect("materialization must succeed");
        let ws_idx = created[0];
        app.state
            .remote_mirrors
            .insert(mount.host_key.clone(), mirror);
        app.state.active = Some(ws_idx);
        app.state.selected = ws_idx;
        // Drain the frames materialization itself emitted (terminal opens).
        while out_rx.try_recv().is_ok() {}
        (app, mount, ws_idx, out_rx)
    }

    /// Regression guard for the configuration that silently disabled the whole
    /// feature: with `ui.prompt_new_workspace_name` on, "new workspace" opens
    /// a name dialog whose confirm used to always send a locally-resolved
    /// `cwd`, which the redirect read as a deliberate local-directory choice.
    /// A user on that config got a local workspace seeded from a path that
    /// only exists on the serving host. The dialog asks for a name, never a
    /// directory, so its create must reach the wire — carrying the typed name
    /// as the label, the only route by which a label reaches the wire at all.
    #[cfg(unix)]
    #[tokio::test]
    async fn named_workspace_create_inside_a_federated_workspace_goes_out_over_the_mount() {
        let (mut app, _mount, _ws_idx, mut out_rx) = mounted_and_focused_mirror();
        app.state.prompt_new_workspace_name = true;
        let before = app.state.workspaces.len();

        app.begin_tui_workspace_create("tui.workspace.create");
        assert_eq!(
            app.state.mode,
            Mode::RenameWorkspace,
            "the prompt config must open the name dialog"
        );
        app.state.name_input = "remote scratch".to_string();
        app.handle_rename_key_via_api(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::empty(),
        ));

        assert_eq!(
            app.state.workspaces.len(),
            before,
            "the named create must not spawn a local workspace from a remote-only path"
        );
        let mut sent_label = None;
        while let Ok(frame) = out_rx.try_recv() {
            if let FederationMessage::WorkspaceCreateRequest(request) = frame {
                sent_label = Some(request.label);
            }
        }
        assert_eq!(
            sent_label,
            Some(Some("remote scratch".to_string())),
            "the mount must have received a WorkspaceCreateRequest carrying the typed name"
        );
    }

    /// The same dialog on a plain local workspace must still create locally,
    /// at the directory it captured when it opened.
    #[tokio::test]
    async fn named_workspace_create_outside_a_mount_still_creates_locally() {
        let mut app = test_app();
        let cwd = std::env::temp_dir();
        app.state.new_terminal_cwd =
            crate::config::NewTerminalCwdConfig::Path(cwd.display().to_string());
        app.state.prompt_new_workspace_name = true;
        let before = app.state.workspaces.len();

        app.begin_tui_workspace_create("tui.workspace.create");
        app.state.name_input = "local scratch".to_string();
        app.handle_rename_key_via_api(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::empty(),
        ));

        assert_eq!(
            app.state.workspaces.len(),
            before + 1,
            "a non-federated named create is still a local create"
        );
        assert_eq!(
            app.state.workspaces[before].custom_name.as_deref(),
            Some("local scratch")
        );
        crate::app::api::test_support::shutdown_test_runtimes(&mut app);
    }

    /// The remote workspace this client asked for must arrive focused —
    /// otherwise the keypress looks like it did nothing — while a workspace
    /// the *remote* user created out of band must never pull the local user
    /// out of what they were doing.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_client_requested_remote_workspace_takes_focus_and_a_remote_originated_one_does_not()
    {
        let (mut app, mount, ws_idx, _out_rx) = mounted_and_focused_mirror();
        let origin = mount.host_key.clone();

        // Out-of-band first: the remote user made this one, so nothing
        // correlates it with a local request.
        let foreign_id = format!("r:{}:w-foreign", origin.as_str());
        app.handle_federation_resync_workspace_created(
            origin.clone(),
            foreign_id.clone(),
            "made by the other user".to_string(),
        );
        let (local_pane_id, terminal_id, terminal, runtime, pane_state) = resync_pane_payload();
        app.handle_federation_resync_pane_created(crate::events::FederationResyncPaneCreated {
            origin: origin.clone(),
            workspace_id: foreign_id.clone(),
            tab_id: format!("r:{}:t-foreign", origin.as_str()),
            pane_id: format!("r:{}:p-foreign", origin.as_str()),
            local_pane_id,
            terminal_id,
            terminal,
            runtime,
            pane_state,
        });
        assert!(
            app.state.workspaces.iter().any(|ws| ws.id == foreign_id),
            "the out-of-band workspace must still materialize"
        );
        assert_eq!(
            app.state.active,
            Some(ws_idx),
            "a workspace the remote user created must not steal focus"
        );

        // Now one this client asked for, with `focus: true`.
        let response =
            app.handle_api_request_after_internal_events_drained(crate::api::schema::Request {
                id: "tui.workspace.create".to_string(),
                method: crate::api::schema::Method::WorkspaceCreate(
                    crate::api::schema::WorkspaceCreateParams {
                        cwd: None,
                        focus: true,
                        label: None,
                        env: Default::default(),
                    },
                ),
            });
        assert!(
            response.contains("workspace_create_requested"),
            "unexpected response: {response}"
        );
        let request_id = *app
            .pending_remote_workspace_create_focus
            .iter()
            .next()
            .expect("a focus-requesting create must claim its request id");

        let requested_id = format!("r:{}:w-requested", origin.as_str());
        app.handle_federation_workspace_create_accepted(
            request_id,
            origin.clone(),
            requested_id.clone(),
        );
        app.handle_federation_resync_workspace_created(
            origin.clone(),
            requested_id.clone(),
            "asked for by this client".to_string(),
        );
        let (local_pane_id, terminal_id, terminal, runtime, pane_state) = resync_pane_payload();
        app.handle_federation_resync_pane_created(crate::events::FederationResyncPaneCreated {
            origin: origin.clone(),
            workspace_id: requested_id.clone(),
            tab_id: format!("r:{}:t-requested", origin.as_str()),
            pane_id: format!("r:{}:p-requested", origin.as_str()),
            local_pane_id,
            terminal_id,
            terminal,
            runtime,
            pane_state,
        });

        let requested_idx = app
            .state
            .workspaces
            .iter()
            .position(|ws| ws.id == requested_id)
            .expect("the requested workspace must materialize");
        assert_eq!(
            app.state.active,
            Some(requested_idx),
            "the workspace this client asked for must arrive focused"
        );
        assert!(
            app.pending_remote_workspace_focus.is_empty()
                && app.pending_remote_workspace_create_focus.is_empty(),
            "the focus claim must be consumed, not left to catch a later create"
        );
        app.state.assert_invariants_for_test();
    }

    /// Closing the last tab of one mirrored workspace must retire exactly that
    /// workspace. Every workspace a mount materializes shares the mount's one
    /// `federation:<host_key>` worktree space, so the ordinary group close
    /// would take the whole mirror down while the mount stayed live.
    #[cfg(unix)]
    #[tokio::test]
    async fn closing_a_mirrored_workspaces_last_tab_leaves_the_mounts_other_workspaces_alive() {
        let (mut app, mount, first_ws_idx, _out_rx) = mounted_and_focused_mirror();
        let origin = mount.host_key.clone();
        let first_workspace_id = app.state.workspaces[first_ws_idx].id.clone();

        let second_id = format!("r:{}:w-second", origin.as_str());
        app.handle_federation_resync_workspace_created(
            origin.clone(),
            second_id.clone(),
            "second remote workspace".to_string(),
        );
        let (local_pane_id, terminal_id, terminal, runtime, pane_state) = resync_pane_payload();
        app.handle_federation_resync_pane_created(crate::events::FederationResyncPaneCreated {
            origin: origin.clone(),
            workspace_id: second_id.clone(),
            tab_id: format!("r:{}:t-second", origin.as_str()),
            pane_id: format!("r:{}:p-second", origin.as_str()),
            local_pane_id,
            terminal_id,
            terminal,
            runtime,
            pane_state,
        });
        let second_ws_idx = app
            .state
            .workspaces
            .iter()
            .position(|ws| ws.id == second_id)
            .expect("the second mirrored workspace must materialize");
        assert_eq!(
            app.state.workspaces[second_ws_idx].tabs.len(),
            1,
            "this test needs the second workspace to have exactly one tab"
        );
        let tab_id = app
            .public_tab_id(second_ws_idx, 0)
            .expect("the mirrored tab has a public id");

        let response =
            app.handle_api_request_after_internal_events_drained(crate::api::schema::Request {
                id: "tab.close".to_string(),
                method: crate::api::schema::Method::TabClose(crate::api::schema::TabTarget {
                    tab_id,
                }),
            });
        assert!(
            response.contains("\"result\""),
            "closing a mirrored last tab must succeed: {response}"
        );

        assert!(
            !app.state.workspaces.iter().any(|ws| ws.id == second_id),
            "the closed workspace must be gone"
        );
        assert!(
            app.state
                .workspaces
                .iter()
                .any(|ws| ws.id == first_workspace_id),
            "the mount's other mirrored workspace must survive its sibling's last-tab close"
        );
        assert!(
            !app.remote_resync_workspace_index.contains_key(&second_id),
            "the closed workspace's federation bookkeeping must be purged"
        );
        app.state.assert_invariants_for_test();
    }

    /// Identity/state guard required for workspace-identity changes: the new
    /// handlers must be inert against adversarial identity state rather than
    /// panicking or corrupting invariants.
    #[cfg(unix)]
    #[tokio::test]
    async fn resync_workspace_handlers_leave_adversarial_identity_state_intact() {
        let mut app = test_app();
        app.state = crate::app::AppState::test_with_adversarial_identity_state();
        let before = app.state.workspaces.len();
        let origin = crate::remote::federation::id::HostKey::new("alice@10.0.0.1", "s1");

        // An announcement for a workspace nothing has materialized, then a
        // removal for one that does not exist: both must no-op.
        app.handle_federation_resync_workspace_created(
            origin.clone(),
            format!("r:{}:ghost", origin.as_str()),
            "ghost".to_string(),
        );
        app.handle_federation_resync_workspace_removed(
            origin.clone(),
            format!("r:{}:ghost", origin.as_str()),
        );
        // A local (non-namespaced) id must be refused outright.
        app.handle_federation_resync_workspace_created(
            origin,
            "w1".to_string(),
            "not namespaced".to_string(),
        );

        assert_eq!(app.state.workspaces.len(), before);
        assert!(app.remote_resync_workspace_index.is_empty());
        app.state.assert_invariants_for_test();
    }
}
