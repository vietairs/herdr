use std::collections::HashMap;
use std::ops::{Deref, DerefMut};
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
#[cfg(not(test))]
use std::sync::atomic::Ordering;
use std::sync::Arc;

use ratatui::layout::Direction;
use tokio::sync::{mpsc, Notify};

use crate::events::AppEvent;
use crate::layout::PaneId;
#[cfg(test)]
use crate::layout::TileLayout;
use crate::pane::{PaneLaunchEnv, PaneState};
use crate::render_signal::RenderSignal;
use crate::terminal::{TerminalId, TerminalRuntime, TerminalRuntimeRegistry, TerminalState};

mod aggregate;
mod git;
pub(crate) mod naming;
mod tab;

use self::git::git_status_cache_key_for_space;
pub(crate) use self::{git::git_status_snapshot_for_cwd_with_demand, tab::MovedPane};
pub use self::{
    git::{
        apply_tab_label_distinctness, derive_label_from_cwd, derive_tab_auto_label,
        fallback_label_from_cwd, git_branch, git_space_metadata, git_status_cache_key,
        GitSpaceMetadata, GitStatusCacheEntry, GitStatusRefreshDemand,
    },
    tab::{NewPane, Tab},
};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WorktreeSpaceMembership {
    pub key: String,
    pub label: String,
    pub repo_root: PathBuf,
    pub checkout_path: PathBuf,
    pub is_linked_worktree: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceGitStatus {
    pub workspace_id: String,
    pub resolved_identity_cwd: PathBuf,
    pub status_cache_key: PathBuf,
    pub demand: GitStatusRefreshDemand,
    pub auto_label: String,
    pub branch: Option<String>,
    pub ahead_behind: Option<(usize, usize)>,
    pub space: Option<GitSpaceMetadata>,
    /// D1 — rung-3 derived label per tab (`tab.number`, not index), computed
    /// on this same ~1.5s pass: `(tab.number, tab_cwd, derived_label)`. The
    /// cwd rides alongside so `apply_workspace_git_statuses` can stamp
    /// `Tab::cached_auto_label_cwd` the same way the workspace stamps
    /// `cached_identity_cwd`, without re-deriving it. Populated by the
    /// caller after `into_workspace_status` returns, since the derivation
    /// needs each tab's own resolved cwd, which this snapshot type does not
    /// carry.
    pub tab_auto_labels: Vec<(usize, PathBuf, Option<String>)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceGitStatusSnapshot {
    pub auto_label: String,
    pub branch: Option<String>,
    pub ahead_behind: Option<(usize, usize)>,
    pub space: Option<GitSpaceMetadata>,
}

pub(crate) fn discover_workspace_git_identity(
    cwd: &std::path::Path,
) -> (Option<GitSpaceMetadata>, String, PathBuf) {
    let space = git_space_metadata(cwd);
    let auto_label = space
        .as_ref()
        .map(|space| self::git::automatic_workspace_label(cwd, &space.repo_root))
        .unwrap_or_else(|| fallback_label_from_cwd(cwd));
    let status_cache_key = space
        .as_ref()
        .map(git_status_cache_key_for_space)
        .unwrap_or_else(|| cwd.to_path_buf());
    (space, auto_label, status_cache_key)
}

impl WorkspaceGitStatusSnapshot {
    pub fn into_workspace_status(
        self,
        workspace_id: String,
        resolved_identity_cwd: PathBuf,
        status_cache_key: PathBuf,
        demand: GitStatusRefreshDemand,
    ) -> WorkspaceGitStatus {
        let auto_label = self
            .space
            .as_ref()
            .map(|space| {
                self::git::automatic_workspace_label(&resolved_identity_cwd, &space.repo_root)
            })
            .unwrap_or_else(|| fallback_label_from_cwd(&resolved_identity_cwd));
        WorkspaceGitStatus {
            workspace_id,
            resolved_identity_cwd,
            status_cache_key,
            demand,
            auto_label,
            branch: self.branch,
            ahead_behind: self.ahead_behind,
            space: self.space,
            tab_auto_labels: Vec::new(),
        }
    }
}

#[cfg_attr(test, allow(dead_code))]
static NEXT_WORKSPACE_ID: AtomicU64 = AtomicU64::new(1);
const PUBLIC_ID_ALPHABET: &[u8; 32] = b"123456789ABCDEFGHJKMNPQRSTVWXYZ0";

// The process-global counter is correct for the real server (one process,
// stable monotonic public ids for its whole lifetime). Under `cargo test`
// every unit test in this crate shares that same static in one binary, so a
// large suite creating many thousands of `Workspace`s across unrelated
// tests can walk the counter past what a single test expects for "a freshly
// generated id is short". Route test builds through a thread-local counter
// instead: each test's assertions only care about ids generated within that
// test, and Rust's test harness gives each test its own thread.
#[cfg(test)]
thread_local! {
    static NEXT_WORKSPACE_ID_TEST: std::cell::Cell<u64> = const { std::cell::Cell::new(1) };
}

pub(crate) fn generate_workspace_id() -> String {
    #[cfg(not(test))]
    let counter = NEXT_WORKSPACE_ID.fetch_add(1, Ordering::Relaxed);
    #[cfg(test)]
    let counter = NEXT_WORKSPACE_ID_TEST.with(|cell| {
        let current = cell.get();
        cell.set(current + 1);
        current
    });
    format!("w{}", encode_public_number(counter as usize))
}

pub(crate) fn encode_public_number(mut value: usize) -> String {
    if value == 0 {
        return "0".to_string();
    }

    let mut encoded = Vec::new();
    while value > 0 {
        let digit = (value - 1) % PUBLIC_ID_ALPHABET.len();
        encoded.push(PUBLIC_ID_ALPHABET[digit] as char);
        value = (value - 1) / PUBLIC_ID_ALPHABET.len();
    }
    encoded.iter().rev().collect()
}

pub(crate) fn decode_public_number(value: &str) -> Option<usize> {
    let mut decoded = 0usize;
    for ch in value.chars() {
        let digit = PUBLIC_ID_ALPHABET
            .iter()
            .position(|candidate| *candidate as char == ch)?;
        decoded = decoded
            .checked_mul(PUBLIC_ID_ALPHABET.len())?
            .checked_add(digit + 1)?;
    }
    Some(decoded)
}

pub(crate) fn public_workspace_number(id: &str) -> Option<usize> {
    id.strip_prefix('w').and_then(decode_public_number)
}

pub(crate) fn public_pane_id_for_number(workspace_id: &str, pane_number: usize) -> String {
    format!("{workspace_id}:p{}", encode_public_number(pane_number))
}

pub(crate) fn public_tab_id_for_number(workspace_id: &str, tab_number: usize) -> String {
    format!("{workspace_id}:t{}", encode_public_number(tab_number))
}

pub(crate) fn reserve_workspace_ids(workspaces: &[Workspace]) {
    let Some(next) = workspaces
        .iter()
        .filter_map(|workspace| public_workspace_number(&workspace.id))
        .max()
        .and_then(|max| u64::try_from(max.checked_add(1)?).ok())
    else {
        return;
    };

    #[cfg(not(test))]
    {
        let mut current = NEXT_WORKSPACE_ID.load(Ordering::Relaxed);
        while current < next {
            match NEXT_WORKSPACE_ID.compare_exchange_weak(
                current,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(observed) => current = observed,
            }
        }
    }
    #[cfg(test)]
    NEXT_WORKSPACE_ID_TEST.with(|cell| {
        if cell.get() < next {
            cell.set(next);
        }
    });
}

/// A named workspace containing tabs.
pub struct Workspace {
    /// Stable public workspace identity, independent of display order.
    pub id: String,
    /// User-provided override. If set, auto-derived identity stops updating.
    pub custom_name: Option<String>,
    /// Remote-resolved label for a federation-mounted workspace. Never
    /// persisted, never a user override; the naming resolver's rung 1.5
    /// reads it and stops before rungs 2-4 run against local data.
    pub mirrored_name: Option<String>,
    /// Fallback workspace identity source for tests, old snapshots, or missing runtimes.
    pub identity_cwd: PathBuf,
    /// CWD from which the cached automatic label and Git metadata were derived.
    pub(crate) cached_identity_cwd: PathBuf,
    /// Automatic workspace label cached outside the render path.
    pub(crate) cached_auto_label: String,
    /// Cache key for periodic Git status associated with `cached_identity_cwd`.
    pub(crate) cached_git_status_key: PathBuf,
    /// Cached current git branch for the workspace repo.
    pub(crate) cached_git_branch: Option<String>,
    /// Cached ahead/behind counts for the workspace repo's current branch upstream.
    pub(crate) cached_git_ahead_behind: Option<(usize, usize)>,
    /// Cached derived Git repo metadata for worktree actions and status display.
    pub(crate) cached_git_space: Option<GitSpaceMetadata>,
    /// Explicit Herdr-managed worktree grouping provenance.
    pub worktree_space: Option<WorktreeSpaceMembership>,
    pub(crate) metadata_tokens: crate::metadata_tokens::MetadataTokens,
    pub(crate) metadata_token_sequences: HashMap<String, u64>,
    /// Public pane numbers within this workspace. Closed pane numbers are not reused.
    pub public_pane_numbers: HashMap<PaneId, usize>,
    pub(crate) next_public_pane_number: usize,
    pub(crate) next_public_tab_number: usize,
    pub tabs: Vec<Tab>,
    pub active_tab: usize,
    #[cfg(test)]
    pub(crate) test_runtimes: HashMap<PaneId, TerminalRuntime>,
}

impl Deref for Workspace {
    type Target = Tab;

    fn deref(&self) -> &Self::Target {
        self.active_tab()
            .expect("workspace must always have at least one active tab")
    }
}

impl DerefMut for Workspace {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.active_tab_mut()
            .expect("workspace must always have at least one active tab")
    }
}

impl Workspace {
    fn adjust_active_tab_after_removal(&mut self, removed_idx: usize) {
        if self.tabs.is_empty() {
            self.active_tab = 0;
        } else if self.active_tab >= self.tabs.len() {
            self.active_tab = self.tabs.len() - 1;
        } else if removed_idx <= self.active_tab && self.active_tab > 0 {
            self.active_tab -= 1;
        }
    }

    pub(crate) fn from_existing_pane(
        label: Option<String>,
        tab_label: Option<String>,
        identity_cwd: PathBuf,
        moved: MovedPane,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<RenderSignal>,
    ) -> Self {
        let id = generate_workspace_id();
        let root_pane = moved.pane_id;
        let tab = Tab::from_existing_pane(1, tab_label, moved, events, render_notify, render_dirty);
        let mut public_pane_numbers = HashMap::new();
        public_pane_numbers.insert(root_pane, 1);
        let (cached_git_space, cached_auto_label, cached_git_status_key) =
            discover_workspace_git_identity(&identity_cwd);
        Self {
            id,
            custom_name: label,
            mirrored_name: None,
            identity_cwd: identity_cwd.clone(),
            cached_identity_cwd: identity_cwd.clone(),
            cached_auto_label,
            cached_git_status_key,
            cached_git_branch: git_branch(&identity_cwd),
            cached_git_ahead_behind: None,
            cached_git_space,
            worktree_space: None,
            metadata_tokens: crate::metadata_tokens::MetadataTokens::default(),
            metadata_token_sequences: HashMap::new(),
            public_pane_numbers,
            next_public_pane_number: 2,
            next_public_tab_number: 2,
            tabs: vec![tab],
            active_tab: 0,
            #[cfg(test)]
            test_runtimes: HashMap::new(),
        }
    }

    pub fn new(
        initial_cwd: PathBuf,
        rows: u16,
        cols: u16,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        host_terminal_appearance: Option<crate::terminal_theme::HostAppearance>,
        shell_config: crate::pane::PaneShellConfig<'_>,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<RenderSignal>,
    ) -> std::io::Result<(Self, TerminalState, TerminalRuntime)> {
        Self::new_with_tab(
            initial_cwd,
            rows,
            cols,
            scrollback_limit_bytes,
            host_terminal_theme,
            host_terminal_appearance,
            shell_config,
            events,
            render_notify,
            render_dirty,
            None,
            Vec::new(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_with_extra_env(
        initial_cwd: PathBuf,
        rows: u16,
        cols: u16,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        host_terminal_appearance: Option<crate::terminal_theme::HostAppearance>,
        shell_config: crate::pane::PaneShellConfig<'_>,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<RenderSignal>,
        extra_env: Vec<(String, String)>,
    ) -> std::io::Result<(Self, TerminalState, TerminalRuntime)> {
        if extra_env.is_empty() {
            return Self::new(
                initial_cwd,
                rows,
                cols,
                scrollback_limit_bytes,
                host_terminal_theme,
                host_terminal_appearance,
                shell_config,
                events,
                render_notify,
                render_dirty,
            );
        }
        Self::new_with_tab(
            initial_cwd,
            rows,
            cols,
            scrollback_limit_bytes,
            host_terminal_theme,
            host_terminal_appearance,
            shell_config,
            events,
            render_notify,
            render_dirty,
            None,
            extra_env,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn new_with_tab(
        initial_cwd: PathBuf,
        rows: u16,
        cols: u16,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        host_terminal_appearance: Option<crate::terminal_theme::HostAppearance>,
        shell_config: crate::pane::PaneShellConfig<'_>,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<RenderSignal>,
        argv: Option<&[String]>,
        extra_env: Vec<(String, String)>,
    ) -> std::io::Result<(Self, TerminalState, TerminalRuntime)> {
        let id = generate_workspace_id();
        let launch_env = PaneLaunchEnv::from_extra(extra_env).with_identity(
            id.clone(),
            public_tab_id_for_number(&id, 1),
            public_pane_id_for_number(&id, 1),
        );
        let (tab, terminal, runtime) = if let Some(argv) = argv {
            Tab::new_argv_command(
                1,
                initial_cwd.clone(),
                rows,
                cols,
                argv,
                scrollback_limit_bytes,
                host_terminal_theme,
                host_terminal_appearance,
                &launch_env,
                events,
                render_notify,
                render_dirty,
            )?
        } else {
            Tab::new(
                1,
                initial_cwd.clone(),
                rows,
                cols,
                scrollback_limit_bytes,
                host_terminal_theme,
                host_terminal_appearance,
                shell_config,
                &launch_env,
                events,
                render_notify,
                render_dirty,
            )?
        };
        let mut public_pane_numbers = HashMap::new();
        public_pane_numbers.insert(tab.root_pane, 1);
        let (cached_git_space, cached_auto_label, cached_git_status_key) =
            discover_workspace_git_identity(&initial_cwd);
        Ok((
            Self {
                id,
                custom_name: None,
                mirrored_name: None,
                identity_cwd: initial_cwd.clone(),
                cached_identity_cwd: initial_cwd.clone(),
                cached_auto_label,
                cached_git_status_key,
                cached_git_branch: git_branch(&initial_cwd),
                cached_git_ahead_behind: None,
                cached_git_space,
                worktree_space: None,
                metadata_tokens: crate::metadata_tokens::MetadataTokens::default(),
                metadata_token_sequences: HashMap::new(),
                public_pane_numbers,
                next_public_pane_number: 2,
                next_public_tab_number: 2,
                tabs: vec![tab],
                active_tab: 0,
                #[cfg(test)]
                test_runtimes: HashMap::new(),
            },
            terminal,
            runtime,
        ))
    }

    pub fn active_tab(&self) -> Option<&Tab> {
        self.tabs.get(self.active_tab)
    }

    pub fn active_tab_index(&self) -> usize {
        self.active_tab
    }

    pub fn active_tab_mut(&mut self) -> Option<&mut Tab> {
        self.tabs.get_mut(self.active_tab)
    }

    pub fn active_tab_display_name(&self) -> Option<String> {
        self.tab_display_name(self.active_tab)
    }

    pub fn tab_display_name(&self, tab_idx: usize) -> Option<String> {
        self.resolve_tab_name(tab_idx)
            .map(|resolved| resolved.text.into_owned())
    }

    /// A caller that needs both the label and its `NameSource` for the
    /// same tab (e.g. `App::tab_info`, once per tab per frame per client)
    /// must call this ONCE instead of `tab_display_name` +
    /// `tab_name_source` back to back, which ran `resolve_tab_name`'s
    /// ladder twice. `resolved_display_from` is the workspace-scope twin of
    /// this method, for the same reason.
    pub fn resolved_tab_display(
        &self,
        tab_idx: usize,
    ) -> Option<(String, self::naming::NameSource)> {
        self.resolve_tab_name(tab_idx)
            .map(|resolved| (resolved.text.into_owned(), resolved.source))
    }

    fn resolve_tab_name(&self, tab_idx: usize) -> Option<self::naming::ResolvedName<'_>> {
        let tab = self.tabs.get(tab_idx)?;
        let is_remote = self.is_federation_materialized();
        let sources = self::naming::NameSources {
            own_override: tab.custom_name.as_deref(),
            mirrored: if is_remote {
                tab.mirrored_name.as_deref()
            } else {
                None
            },
            derived: if is_remote {
                None
            } else {
                tab.cached_auto_label.as_deref()
            },
            // Rung 4 is the STABLE public number, never `tab_idx + 1` — a
            // moved tab's array index shifts while its public number does
            // not (D2), so an index-derived label drifts after a move.
            ordinal: self.public_tab_number(tab_idx),
            ..Default::default()
        };
        self::naming::resolve_name(self::naming::NameScope::Tab, sources)
    }

    /// D3 — the tab's own rung-1 USER override only, for pane inheritance.
    /// Deliberately NOT the tab's full resolved display name, and
    /// deliberately NOT the tab's rung-1.5 mirrored remote label either:
    /// inheriting an auto-derived label, a mirrored remote label, or an
    /// ordinal down onto every unnamed agent pane would shadow that pane's
    /// own mirrored label (rung 1.5) and agent identity (rung 2) with the
    /// TAB's label — exactly the D3 misfire Federation Naming Contract v1
    /// exists to prevent. Only a genuine user rename at this scope
    /// is a "renamed scope" in rung 1's sense for inheritance purposes; a
    /// mirrored tab label is rung 1.5 and stays at the tab, it never
    /// inherits downward.
    pub(crate) fn tab_override_for_pane_inheritance(&self, tab_idx: usize) -> Option<&str> {
        let tab = self.tabs.get(tab_idx)?;
        tab.custom_name.as_deref()
    }

    /// Whether this workspace's id classifies as a federation-mounted
    /// mirror. The same non-spoofable check
    /// `workspace_info()` already uses for `federation_origin`, keyed off
    /// this client's own trusted `HostKey`, never off anything the remote
    /// sends. A remote-classified scope short-circuits the resolver at rung
    /// 1.5 (`mirrored`) and never runs rungs 2-4 against local data.
    pub(crate) fn is_federation_materialized(&self) -> bool {
        matches!(
            crate::remote::federation::id::classify(&self.id),
            crate::remote::federation::id::IdClass::Remote(_)
        )
    }

    pub fn switch_tab(&mut self, idx: usize) {
        if idx < self.tabs.len() {
            self.active_tab = idx;
            if let Some(tab) = self.tabs.get_mut(idx) {
                for pane in tab.panes.values_mut() {
                    pane.seen = true;
                }
            }
        }
    }

    pub fn create_tab(
        &mut self,
        rows: u16,
        cols: u16,
        cwd: PathBuf,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        host_terminal_appearance: Option<crate::terminal_theme::HostAppearance>,
        shell_config: crate::pane::PaneShellConfig<'_>,
        extra_env: Vec<(String, String)>,
    ) -> std::io::Result<(usize, TerminalState, TerminalRuntime)> {
        self.create_tab_with_runtime(
            rows,
            cols,
            cwd,
            scrollback_limit_bytes,
            host_terminal_theme,
            host_terminal_appearance,
            shell_config,
            None,
            extra_env,
        )
    }

    pub fn create_tab_argv_command(
        &mut self,
        rows: u16,
        cols: u16,
        cwd: PathBuf,
        argv: &[String],
        extra_env: Vec<(String, String)>,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        host_terminal_appearance: Option<crate::terminal_theme::HostAppearance>,
    ) -> std::io::Result<(usize, TerminalState, TerminalRuntime)> {
        self.create_tab_with_runtime(
            rows,
            cols,
            cwd,
            scrollback_limit_bytes,
            host_terminal_theme,
            host_terminal_appearance,
            crate::pane::PaneShellConfig::new("", crate::config::ShellModeConfig::NonLogin),
            Some(argv),
            extra_env,
        )
    }

    fn create_tab_with_runtime(
        &mut self,
        rows: u16,
        cols: u16,
        cwd: PathBuf,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        host_terminal_appearance: Option<crate::terminal_theme::HostAppearance>,
        shell_config: crate::pane::PaneShellConfig<'_>,
        argv: Option<&[String]>,
        extra_env: Vec<(String, String)>,
    ) -> std::io::Result<(usize, TerminalState, TerminalRuntime)> {
        let number = self.next_public_tab_number;
        self.next_public_tab_number += 1;
        let pane_number = self.next_public_pane_number;
        let launch_env = self.launch_env_for_new_pane(number, pane_number, extra_env);
        let events = self
            .active_tab()
            .map(|tab| tab.events.clone())
            .expect("workspace must always have at least one tab");
        let render_notify = self
            .active_tab()
            .map(|tab| tab.render_notify.clone())
            .expect("workspace must always have at least one tab");
        let render_dirty = self
            .active_tab()
            .map(|tab| tab.render_dirty.clone())
            .expect("workspace must always have at least one tab");

        let (tab, terminal, runtime) = if let Some(argv) = argv {
            Tab::new_argv_command(
                number,
                cwd,
                rows,
                cols,
                argv,
                scrollback_limit_bytes,
                host_terminal_theme,
                host_terminal_appearance,
                &launch_env,
                events,
                render_notify,
                render_dirty,
            )?
        } else {
            Tab::new(
                number,
                cwd,
                rows,
                cols,
                scrollback_limit_bytes,
                host_terminal_theme,
                host_terminal_appearance,
                shell_config,
                &launch_env,
                events,
                render_notify,
                render_dirty,
            )?
        };
        self.register_new_pane_with_number(tab.root_pane, pane_number);
        self.tabs.push(tab);
        Ok((self.tabs.len() - 1, terminal, runtime))
    }

    pub fn close_tab(&mut self, idx: usize) -> bool {
        if self.tabs.len() <= 1 || idx >= self.tabs.len() {
            return false;
        }
        let tab = self.tabs.remove(idx);
        for pane_id in tab.panes.keys() {
            self.unregister_pane(*pane_id);
        }
        if self.active_tab >= self.tabs.len() {
            self.active_tab = self.tabs.len() - 1;
        } else if idx <= self.active_tab && self.active_tab > 0 {
            self.active_tab -= 1;
        }
        true
    }

    pub fn move_tab(&mut self, source_idx: usize, insert_idx: usize) -> bool {
        if source_idx >= self.tabs.len() || insert_idx > self.tabs.len() {
            return false;
        }

        let target_idx = if source_idx < insert_idx {
            insert_idx.saturating_sub(1)
        } else {
            insert_idx
        }
        .min(self.tabs.len().saturating_sub(1));

        if source_idx == target_idx {
            return false;
        }

        let active_root_pane = self.tabs.get(self.active_tab).map(|tab| tab.root_pane);
        let tab = self.tabs.remove(source_idx);
        self.tabs.insert(target_idx, tab);
        self.active_tab = active_root_pane
            .and_then(|root_pane| self.tabs.iter().position(|tab| tab.root_pane == root_pane))
            .unwrap_or(target_idx);
        true
    }

    #[cfg(test)]
    pub fn close_active_tab(&mut self) -> bool {
        self.close_tab(self.active_tab)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn split_focused_command(
        &mut self,
        direction: Direction,
        rows: u16,
        cols: u16,
        cwd: Option<PathBuf>,
        command: &str,
        extra_env: Vec<(String, String)>,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        host_terminal_appearance: Option<crate::terminal_theme::HostAppearance>,
    ) -> std::io::Result<crate::workspace::tab::NewPane> {
        let pane_number = self.next_public_pane_number;
        let tab_number = self
            .active_tab()
            .map(|tab| tab.number)
            .expect("workspace must always have at least one tab");
        let launch_env = self.launch_env_for_new_pane(tab_number, pane_number, extra_env);
        let new_pane = self
            .active_tab_mut()
            .expect("workspace must always have at least one tab")
            .split_focused_command(
                direction,
                rows,
                cols,
                cwd,
                command,
                &launch_env,
                scrollback_limit_bytes,
                host_terminal_theme,
                host_terminal_appearance,
            )?;
        self.register_new_pane_with_number(new_pane.pane_id, pane_number);
        Ok(new_pane)
    }

    // Workspace split routing carries pane identity, geometry, host context, and focus policy.
    #[allow(clippy::too_many_arguments)]
    pub fn split_pane(
        &mut self,
        pane_id: PaneId,
        direction: Direction,
        rows: u16,
        cols: u16,
        cwd: Option<PathBuf>,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        host_terminal_appearance: Option<crate::terminal_theme::HostAppearance>,
        shell_config: crate::pane::PaneShellConfig<'_>,
        extra_env: Vec<(String, String)>,
        focus_new_pane: bool,
    ) -> Option<std::io::Result<(usize, crate::workspace::tab::NewPane)>> {
        self.split_pane_with_runtime(
            pane_id,
            direction,
            None,
            rows,
            cols,
            cwd,
            scrollback_limit_bytes,
            host_terminal_theme,
            host_terminal_appearance,
            shell_config,
            extra_env,
            focus_new_pane,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn split_pane_with_ratio(
        &mut self,
        pane_id: PaneId,
        direction: Direction,
        ratio: f32,
        rows: u16,
        cols: u16,
        cwd: Option<PathBuf>,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        host_terminal_appearance: Option<crate::terminal_theme::HostAppearance>,
        shell_config: crate::pane::PaneShellConfig<'_>,
        extra_env: Vec<(String, String)>,
        focus_new_pane: bool,
    ) -> Option<std::io::Result<(usize, crate::workspace::tab::NewPane)>> {
        self.split_pane_with_runtime(
            pane_id,
            direction,
            Some(ratio),
            rows,
            cols,
            cwd,
            scrollback_limit_bytes,
            host_terminal_theme,
            host_terminal_appearance,
            shell_config,
            extra_env,
            focus_new_pane,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn split_pane_argv_command(
        &mut self,
        pane_id: PaneId,
        direction: Direction,
        rows: u16,
        cols: u16,
        cwd: Option<PathBuf>,
        argv: &[String],
        extra_env: Vec<(String, String)>,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        host_terminal_appearance: Option<crate::terminal_theme::HostAppearance>,
        focus_new_pane: bool,
    ) -> Option<std::io::Result<(usize, crate::workspace::tab::NewPane)>> {
        self.split_pane_with_runtime(
            pane_id,
            direction,
            None,
            rows,
            cols,
            cwd,
            scrollback_limit_bytes,
            host_terminal_theme,
            host_terminal_appearance,
            crate::pane::PaneShellConfig::new("", crate::config::ShellModeConfig::NonLogin),
            extra_env,
            focus_new_pane,
            Some(argv),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn split_pane_argv_command_with_ratio(
        &mut self,
        pane_id: PaneId,
        direction: Direction,
        ratio: f32,
        rows: u16,
        cols: u16,
        cwd: Option<PathBuf>,
        argv: &[String],
        extra_env: Vec<(String, String)>,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        host_terminal_appearance: Option<crate::terminal_theme::HostAppearance>,
        focus_new_pane: bool,
    ) -> Option<std::io::Result<(usize, crate::workspace::tab::NewPane)>> {
        self.split_pane_with_runtime(
            pane_id,
            direction,
            Some(ratio),
            rows,
            cols,
            cwd,
            scrollback_limit_bytes,
            host_terminal_theme,
            host_terminal_appearance,
            crate::pane::PaneShellConfig::new("", crate::config::ShellModeConfig::NonLogin),
            extra_env,
            focus_new_pane,
            Some(argv),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn split_pane_with_runtime(
        &mut self,
        pane_id: PaneId,
        direction: Direction,
        ratio: Option<f32>,
        rows: u16,
        cols: u16,
        cwd: Option<PathBuf>,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        host_terminal_appearance: Option<crate::terminal_theme::HostAppearance>,
        shell_config: crate::pane::PaneShellConfig<'_>,
        extra_env: Vec<(String, String)>,
        focus_new_pane: bool,
        argv: Option<&[String]>,
    ) -> Option<std::io::Result<(usize, crate::workspace::tab::NewPane)>> {
        let tab_idx = self.find_tab_index_for_pane(pane_id)?;
        let pane_number = self.next_public_pane_number;
        let tab_number = self.tabs[tab_idx].number;
        let launch_env = self.launch_env_for_new_pane(tab_number, pane_number, extra_env);
        let tab = &mut self.tabs[tab_idx];
        let new_pane = match if let Some(argv) = argv {
            tab.split_pane_argv(
                pane_id,
                focus_new_pane,
                direction,
                ratio,
                rows,
                cols,
                cwd,
                argv,
                &launch_env,
                scrollback_limit_bytes,
                host_terminal_theme,
                host_terminal_appearance,
            )
        } else {
            tab.split_pane_shell(
                pane_id,
                focus_new_pane,
                direction,
                ratio,
                rows,
                cols,
                cwd,
                scrollback_limit_bytes,
                host_terminal_theme,
                host_terminal_appearance,
                shell_config,
                &launch_env,
            )
        } {
            Ok(new_pane) => new_pane,
            Err(err) => return Some(Err(err)),
        };
        self.register_new_pane_with_number(new_pane.pane_id, pane_number);
        Some(Ok((tab_idx, new_pane)))
    }

    /// Close the focused pane. Returns true if the workspace should close.
    #[cfg(test)]
    pub fn close_focused(&mut self) -> bool {
        let pane_count = self
            .active_tab()
            .map(|tab| tab.layout.pane_count())
            .unwrap_or(0);
        let tab_count = self.tabs.len();
        if pane_count <= 1 {
            return tab_count <= 1 || self.close_active_tab_and_report();
        }

        if let Some((removed, _terminal_id)) = self.active_tab_mut().and_then(Tab::close_focused) {
            self.unregister_pane(removed);
        }
        false
    }

    /// Remove a specific pane from this workspace without terminating its runtime.
    /// Returns true if the workspace should close.
    pub fn remove_pane(&mut self, pane_id: PaneId) -> bool {
        let Some(tab_idx) = self.find_tab_index_for_pane(pane_id) else {
            return false;
        };
        let pane_count = self.tabs[tab_idx].layout.pane_count();
        let tab_count = self.tabs.len();
        if pane_count <= 1 {
            if tab_count <= 1 {
                return true;
            }
            self.tabs.remove(tab_idx);
            self.unregister_pane(pane_id);
            if self.active_tab >= self.tabs.len() {
                self.active_tab = self.tabs.len() - 1;
            } else if tab_idx <= self.active_tab && self.active_tab > 0 {
                self.active_tab -= 1;
            }
            return false;
        }

        if let Some((removed, _terminal_id)) = self.tabs[tab_idx].remove_pane(pane_id) {
            self.unregister_pane(removed);
        }
        false
    }

    pub(crate) fn take_pane_for_move(&mut self, pane_id: PaneId) -> Option<TakenPane> {
        let tab_idx = self.find_tab_index_for_pane(pane_id)?;
        let pane_count = self.tabs[tab_idx].layout.pane_count();
        if pane_count <= 1 {
            let mut tab = self.tabs.remove(tab_idx);
            let moved = tab.take_pane_for_move(pane_id)?;
            self.adjust_active_tab_after_removal(tab_idx);
            return Some(TakenPane {
                moved,
                removed_tab_idx: Some(tab_idx),
                workspace_empty: self.tabs.is_empty(),
            });
        }

        let moved = self.tabs[tab_idx].take_pane_for_move(pane_id)?;
        Some(TakenPane {
            moved,
            removed_tab_idx: None,
            workspace_empty: false,
        })
    }

    pub(crate) fn insert_moved_pane_into_tab(
        &mut self,
        tab_idx: usize,
        target_pane_id: PaneId,
        moved: MovedPane,
        direction: Direction,
        ratio: f32,
        focus: bool,
    ) -> Result<PaneId, MovedPane> {
        let pane_id = moved.pane_id;
        let Some(tab) = self.tabs.get_mut(tab_idx) else {
            return Err(moved);
        };
        tab.insert_existing_pane(target_pane_id, moved, direction, ratio, focus)?;
        if !self.public_pane_numbers.contains_key(&pane_id) {
            self.register_new_pane_with_number(pane_id, self.next_public_pane_number);
        }
        Ok(pane_id)
    }

    pub(crate) fn create_tab_from_existing_pane(
        &mut self,
        moved: MovedPane,
        label: Option<String>,
        fallback_events: mpsc::Sender<AppEvent>,
        fallback_render_notify: Arc<Notify>,
        fallback_render_dirty: Arc<RenderSignal>,
    ) -> usize {
        let number = self.next_public_tab_number;
        self.next_public_tab_number += 1;
        let pane_id = moved.pane_id;
        let (events, render_notify, render_dirty) = self
            .active_tab()
            .map(|tab| {
                (
                    tab.events.clone(),
                    tab.render_notify.clone(),
                    tab.render_dirty.clone(),
                )
            })
            .unwrap_or((
                fallback_events,
                fallback_render_notify,
                fallback_render_dirty,
            ));
        let tab =
            Tab::from_existing_pane(number, label, moved, events, render_notify, render_dirty);
        if !self.public_pane_numbers.contains_key(&pane_id) {
            self.register_new_pane_with_number(pane_id, self.next_public_pane_number);
        }
        self.tabs.push(tab);
        self.tabs.len() - 1
    }

    pub(crate) fn unregister_moved_pane(&mut self, pane_id: PaneId) {
        self.unregister_pane(pane_id);
    }

    pub fn public_pane_number(&self, pane_id: PaneId) -> Option<usize> {
        self.public_pane_numbers.get(&pane_id).copied()
    }

    fn launch_env_for_new_pane(
        &self,
        tab_number: usize,
        pane_number: usize,
        extra_env: Vec<(String, String)>,
    ) -> PaneLaunchEnv {
        PaneLaunchEnv::from_extra(extra_env).with_identity(
            self.id.clone(),
            public_tab_id_for_number(&self.id, tab_number),
            public_pane_id_for_number(&self.id, pane_number),
        )
    }

    pub fn public_tab_number(&self, tab_idx: usize) -> Option<usize> {
        self.tabs.get(tab_idx).map(|tab| tab.number)
    }

    #[cfg(test)]
    pub fn public_tab_number_for_pane(&self, pane_id: PaneId) -> Option<usize> {
        let tab_idx = self.find_tab_index_for_pane(pane_id)?;
        self.public_tab_number(tab_idx)
    }

    pub fn set_custom_name(&mut self, name: String) {
        self.custom_name = Some(name);
    }

    /// Clears the override — see `Tab::clear_custom_name`'s doc.
    pub fn clear_custom_name(&mut self) {
        self.custom_name = None;
    }

    #[cfg(test)]
    pub fn resolved_identity_cwd(&self) -> Option<PathBuf> {
        Some(self.identity_cwd.clone())
    }

    pub fn resolved_identity_cwd_from(
        &self,
        terminals: &HashMap<TerminalId, TerminalState>,
        terminal_runtimes: &TerminalRuntimeRegistry,
    ) -> Option<PathBuf> {
        self.tabs
            .first()
            .and_then(|tab| tab.cwd_for_pane(tab.root_pane, terminals, terminal_runtimes))
            .or_else(|| Some(self.identity_cwd.clone()))
    }

    #[cfg(test)]
    pub fn display_name(&self) -> String {
        let is_remote = self.is_federation_materialized();
        let derived = (!is_remote).then(|| self.automatic_display_name_for_cwd(&self.identity_cwd));
        self.resolve_workspace_name(is_remote, derived.as_deref())
            .map(|resolved| resolved.text.into_owned())
            .unwrap_or_else(|| "workspace".into())
    }

    pub(crate) fn display_name_from_terminals(
        &self,
        terminals: &HashMap<TerminalId, TerminalState>,
    ) -> String {
        let is_remote = self.is_federation_materialized();
        let derived = (!is_remote).then(|| {
            let cwd = self
                .tabs
                .first()
                .and_then(|tab| tab.terminal_id(tab.root_pane))
                .and_then(|terminal_id| terminals.get(terminal_id))
                .map(|terminal| &terminal.cwd)
                .unwrap_or(&self.identity_cwd);
            self.automatic_display_name_for_cwd(cwd)
        });
        self.resolve_workspace_name(is_remote, derived.as_deref())
            .map(|resolved| resolved.text.into_owned())
            .unwrap_or_else(|| "workspace".into())
    }

    pub fn display_name_from(
        &self,
        terminals: &HashMap<TerminalId, TerminalState>,
        terminal_runtimes: &TerminalRuntimeRegistry,
    ) -> String {
        self.resolved_display_from(terminals, terminal_runtimes).0
    }

    /// Resolves the label AND its `NameSource` in one pass. A
    /// caller that needs both (e.g. `App::workspace_info`, once per
    /// workspace per frame per client — a multiplicative render path per
    /// CLAUDE.md) must call this ONCE. It used to be two separate methods
    /// (`display_name_from` + a since-removed `workspace_name_source_from`)
    /// called back to back, which ran the whole derivation chain
    /// (including `resolved_identity_cwd_from`'s `Pane::cwd()`
    /// mutex-lock-plus-`platform::process_cwd` fallback) twice per call
    /// site.
    pub fn resolved_display_from(
        &self,
        terminals: &HashMap<TerminalId, TerminalState>,
        terminal_runtimes: &TerminalRuntimeRegistry,
    ) -> (String, self::naming::NameSource) {
        let (is_remote, derived) = self.workspace_derived_for(terminals, terminal_runtimes);
        match self.resolve_workspace_name(is_remote, derived.as_deref()) {
            Some(resolved) => (resolved.text.into_owned(), resolved.source),
            None => ("workspace".into(), self::naming::NameSource::Ordinal),
        }
    }

    /// Rung-3 input for the workspace ladder. Restores the rung-1
    /// short-circuit: a workspace with a user override
    /// (`custom_name.is_some()`) never needs a derived cwd label at all —
    /// `resolve_workspace_name` would discard it anyway (rung 1 always
    /// wins) — so skip `resolved_identity_cwd_from`'s cwd resolution
    /// (mutex lock, and on a cache miss `platform::process_cwd`) entirely
    /// rather than paying that cost only to throw the result away.
    fn workspace_derived_for(
        &self,
        terminals: &HashMap<TerminalId, TerminalState>,
        terminal_runtimes: &TerminalRuntimeRegistry,
    ) -> (bool, Option<String>) {
        let is_remote = self.is_federation_materialized();
        if self.custom_name.is_some() {
            return (is_remote, None);
        }
        let derived = if is_remote {
            None
        } else {
            self.resolved_identity_cwd_from(terminals, terminal_runtimes)
                .map(|cwd| self.automatic_display_name_for_cwd(&cwd))
        };
        (is_remote, derived)
    }

    /// Shared tail of the `display_name*` family: rung 1 (`custom_name`),
    /// rung 1.5 (`mirrored_name`, remote scopes only), rung 3
    /// (the caller-supplied `derived` label, local scopes only). Callers
    /// fall back to the literal `"workspace"` when this returns `None` —
    /// no rung 4 here, a workspace has no ordinal.
    fn resolve_workspace_name<'a>(
        &'a self,
        is_remote: bool,
        derived: Option<&'a str>,
    ) -> Option<self::naming::ResolvedName<'a>> {
        let sources = self::naming::NameSources {
            own_override: self.custom_name.as_deref(),
            mirrored: if is_remote {
                self.mirrored_name.as_deref()
            } else {
                None
            },
            derived,
            ..Default::default()
        };
        self::naming::resolve_name(self::naming::NameScope::Workspace, sources)
    }

    fn automatic_display_name_for_cwd(&self, cwd: &std::path::Path) -> String {
        if cwd == self.cached_identity_cwd {
            self.cached_auto_label.clone()
        } else {
            fallback_label_from_cwd(cwd)
        }
    }

    pub fn branch(&self) -> Option<String> {
        self.cached_git_branch.clone()
    }

    pub fn git_ahead_behind(&self) -> Option<(usize, usize)> {
        self.cached_git_ahead_behind
    }

    pub fn git_space(&self) -> Option<&GitSpaceMetadata> {
        self.cached_git_space.as_ref()
    }

    pub fn worktree_space(&self) -> Option<&WorktreeSpaceMembership> {
        self.worktree_space.as_ref()
    }

    pub fn find_tab_index_for_pane(&self, pane_id: PaneId) -> Option<usize> {
        self.tabs
            .iter()
            .position(|tab| tab.panes.contains_key(&pane_id))
    }

    pub fn pane_state(&self, pane_id: PaneId) -> Option<&PaneState> {
        self.tabs.iter().find_map(|tab| tab.panes.get(&pane_id))
    }

    pub fn pane_state_mut(&mut self, pane_id: PaneId) -> Option<&mut PaneState> {
        self.tabs
            .iter_mut()
            .find_map(|tab| tab.panes.get_mut(&pane_id))
    }

    pub fn terminal_id(&self, pane_id: PaneId) -> Option<&TerminalId> {
        self.tabs.iter().find_map(|tab| tab.terminal_id(pane_id))
    }

    pub fn focused_pane_id(&self) -> Option<PaneId> {
        self.active_tab().map(|tab| tab.layout.focused())
    }

    pub fn close_pane(&mut self, pane_id: PaneId) -> bool {
        let tab_idx = match self.find_tab_index_for_pane(pane_id) {
            Some(idx) => idx,
            None => return false,
        };
        let pane_count = self.tabs[tab_idx].layout.pane_count();
        let tab_count = self.tabs.len();
        if pane_count <= 1 {
            if tab_count <= 1 {
                return true;
            }
            self.tabs.remove(tab_idx);
            self.unregister_pane(pane_id);
            if self.active_tab >= self.tabs.len() {
                self.active_tab = self.tabs.len() - 1;
            } else if tab_idx <= self.active_tab && self.active_tab > 0 {
                self.active_tab -= 1;
            }
            return false;
        }

        if let Some((removed, _terminal_id)) = self.tabs[tab_idx].close_pane(pane_id) {
            self.unregister_pane(removed);
        }
        false
    }

    #[cfg(test)]
    fn register_new_pane(&mut self, pane_id: PaneId) {
        self.register_new_pane_with_number(pane_id, self.next_public_pane_number);
    }

    fn register_new_pane_with_number(&mut self, pane_id: PaneId, number: usize) {
        self.public_pane_numbers.insert(pane_id, number);
        self.next_public_pane_number = self.next_public_pane_number.max(number + 1);
    }

    fn unregister_pane(&mut self, pane_id: PaneId) {
        self.public_pane_numbers.remove(&pane_id);
    }

    #[cfg(test)]
    fn close_active_tab_and_report(&mut self) -> bool {
        if self.tabs.len() <= 1 {
            return true;
        }
        self.close_active_tab();
        false
    }
}

pub(crate) struct TakenPane {
    pub moved: MovedPane,
    pub removed_tab_idx: Option<usize>,
    pub workspace_empty: bool,
}

#[cfg(test)]
impl Workspace {
    pub(crate) fn test_new(name: &str) -> Self {
        let (events, _) = mpsc::channel(64);
        let render_notify = Arc::new(Notify::new());
        let render_dirty = Arc::new(RenderSignal::new());
        let identity_cwd = std::env::current_dir().unwrap_or_else(|_| "/".into());
        let (layout, root_id) = TileLayout::new();
        let terminal_id = TerminalId::alloc();
        let mut panes = HashMap::new();
        panes.insert(root_id, PaneState::new(terminal_id));
        let tab = Tab {
            custom_name: None,
            number: 1,
            root_pane: root_id,
            layout,
            panes,
            runtimes: HashMap::new(),
            cached_auto_label: None,
            cached_auto_label_cwd: None,
            mirrored_name: None,
            zoomed: false,
            events,
            render_notify,
            render_dirty,
        };
        let mut public_pane_numbers = HashMap::new();
        public_pane_numbers.insert(tab.root_pane, 1);
        Self {
            id: generate_workspace_id(),
            custom_name: Some(name.to_string()),
            mirrored_name: None,
            identity_cwd: identity_cwd.clone(),
            cached_identity_cwd: identity_cwd.clone(),
            cached_auto_label: fallback_label_from_cwd(&identity_cwd),
            cached_git_status_key: identity_cwd.clone(),
            cached_git_branch: git_branch(&identity_cwd),
            cached_git_ahead_behind: None,
            cached_git_space: None,
            worktree_space: None,
            metadata_tokens: crate::metadata_tokens::MetadataTokens::default(),
            metadata_token_sequences: HashMap::new(),
            public_pane_numbers,
            next_public_pane_number: 2,
            next_public_tab_number: 2,
            tabs: vec![tab],
            active_tab: 0,
            test_runtimes: HashMap::new(),
        }
    }

    pub(crate) fn insert_test_runtime(&mut self, pane_id: PaneId, runtime: TerminalRuntime) {
        self.test_runtimes.insert(pane_id, runtime);
    }

    pub(crate) fn test_split(&mut self, direction: Direction) -> PaneId {
        let tab = self.active_tab_mut().expect("workspace must have tab");
        let new_id = tab.layout.split_focused(direction);
        tab.panes
            .insert(new_id, PaneState::new(TerminalId::alloc()));
        self.register_new_pane(new_id);
        new_id
    }

    pub(crate) fn test_add_tab(&mut self, name: Option<&str>) -> usize {
        let (events, _) = mpsc::channel(64);
        let render_notify = Arc::new(Notify::new());
        let render_dirty = Arc::new(RenderSignal::new());
        let (layout, root_id) = TileLayout::new();
        let mut panes = HashMap::new();
        panes.insert(root_id, PaneState::new(TerminalId::alloc()));
        let tab = Tab {
            custom_name: name.map(str::to_string),
            number: self.next_public_tab_number,
            root_pane: root_id,
            layout,
            panes,
            runtimes: HashMap::new(),
            cached_auto_label: None,
            cached_auto_label_cwd: None,
            mirrored_name: None,
            zoomed: false,
            events,
            render_notify,
            render_dirty,
        };
        self.next_public_tab_number += 1;
        self.register_new_pane(root_id);
        self.tabs.push(tab);
        self.tabs.len() - 1
    }

    pub(crate) fn test_adversarial_identity_state() -> Self {
        let mut ws = Self::test_new("adversarial-identity");
        let removed_pane = ws.test_split(Direction::Horizontal);
        ws.test_split(Direction::Vertical);
        assert!(!ws.close_pane(removed_pane));
        let _unused_raw_id = PaneId::alloc();
        let later_pane = ws.test_split(Direction::Horizontal);

        let removed_tab = ws.test_add_tab(Some("removed"));
        // M5: a *surviving* renamed tab, so the fixture does not trivially
        // pass Policy C by having every surviving tab be unnamed.
        let survivor_tab = ws.test_add_tab(Some("survivor-renamed"));
        let final_tab = ws.test_add_tab(None);
        let survivor_root = ws.tabs[survivor_tab].root_pane;
        let final_root = ws.tabs[final_tab].root_pane;
        assert!(ws.close_tab(removed_tab));
        assert!(ws.move_tab(0, ws.tabs.len()));
        ws.switch_tab(
            ws.find_tab_index_for_pane(survivor_root)
                .expect("survivor tab should still exist"),
        );

        assert_ne!(
            ws.active_tab + 1,
            ws.tabs[ws.active_tab].number,
            "adversarial active tab must distinguish position from public tab number"
        );
        assert_ne!(
            later_pane.raw() as usize,
            ws.public_pane_number(later_pane).unwrap(),
            "adversarial pane must distinguish raw pane id from public pane number"
        );
        assert_eq!(ws.find_tab_index_for_pane(final_root), Some(1));

        // M5: a mirrored-name tab classified `IdClass::Remote`, so the
        // shape invariant's not-remote-but-mirrored branch is actually
        // exercised by adversarial state.
        let mut mirrored_tab = ws.test_add_tab(None);
        // `test_add_tab` returns the tabs-vec index at insertion time, which
        // the moves above may have invalidated as a stable identity — look
        // it up by root pane instead of trusting the returned index.
        let mirrored_root = ws.tabs[mirrored_tab].root_pane;
        mirrored_tab = ws
            .find_tab_index_for_pane(mirrored_root)
            .expect("mirrored tab should still exist");
        ws.tabs[mirrored_tab].mirrored_name = Some("remote-mirrored".to_string());
        ws.id = format!("r:adversarial-host:{}", ws.id);

        ws
    }

    pub(crate) fn assert_invariants_for_test(&self) {
        assert!(
            !self.tabs.is_empty(),
            "workspace {} must contain at least one tab",
            self.id
        );
        assert!(
            self.active_tab < self.tabs.len(),
            "workspace {} active_tab {} out of bounds for {} tabs",
            self.id,
            self.active_tab,
            self.tabs.len()
        );

        let mut tab_numbers = std::collections::HashSet::new();
        let mut max_tab_number = 0usize;
        let mut live_panes = std::collections::HashSet::new();
        let mut terminal_ids = std::collections::HashSet::new();

        for (tab_idx, tab) in self.tabs.iter().enumerate() {
            assert!(
                tab.number > 0,
                "workspace {} tab {} has invalid public tab number 0",
                self.id,
                tab_idx
            );
            assert!(
                tab_numbers.insert(tab.number),
                "workspace {} has duplicate public tab number {}",
                self.id,
                tab.number
            );
            max_tab_number = max_tab_number.max(tab.number);
            assert!(
                tab.panes.contains_key(&tab.root_pane),
                "workspace {} tab {} root pane {:?} is missing from tab panes",
                self.id,
                tab_idx,
                tab.root_pane
            );

            let layout_panes = tab.layout.pane_ids();
            let layout_set: std::collections::HashSet<_> = layout_panes.iter().copied().collect();
            assert_eq!(
                layout_panes.len(),
                layout_set.len(),
                "workspace {} tab {} layout contains duplicate pane ids",
                self.id,
                tab_idx
            );
            assert!(
                layout_set.contains(&tab.layout.focused()),
                "workspace {} tab {} focused pane {:?} is not in layout",
                self.id,
                tab_idx,
                tab.layout.focused()
            );
            let pane_set: std::collections::HashSet<_> = tab.panes.keys().copied().collect();
            assert_eq!(
                layout_set, pane_set,
                "workspace {} tab {} layout panes must exactly match pane states",
                self.id, tab_idx
            );

            for (pane_id, pane) in &tab.panes {
                assert!(
                    live_panes.insert(*pane_id),
                    "workspace {} pane {:?} appears in more than one tab",
                    self.id,
                    pane_id
                );
                assert!(
                    self.public_pane_numbers.contains_key(pane_id),
                    "workspace {} live pane {:?} has no public pane number",
                    self.id,
                    pane_id
                );
                assert!(
                    terminal_ids.insert(pane.attached_terminal_id.clone()),
                    "workspace {} terminal {} is attached to multiple panes",
                    self.id,
                    pane.attached_terminal_id
                );
            }
        }

        assert!(
            self.next_public_tab_number > 0,
            "workspace {} next_public_tab_number must be greater than 0",
            self.id
        );
        assert!(
            self.next_public_tab_number > max_tab_number,
            "workspace {} next_public_tab_number {} must be greater than max live public tab number {}",
            self.id,
            self.next_public_tab_number,
            max_tab_number
        );

        let public_pane_keys: std::collections::HashSet<_> =
            self.public_pane_numbers.keys().copied().collect();
        assert_eq!(
            public_pane_keys, live_panes,
            "workspace {} public pane map must exactly match live panes",
            self.id
        );

        let mut pane_numbers = std::collections::HashSet::new();
        let mut max_pane_number = 0usize;
        for (pane_id, pane_number) in &self.public_pane_numbers {
            assert!(
                *pane_number > 0,
                "workspace {} pane {:?} has invalid public pane number 0",
                self.id,
                pane_id
            );
            assert!(
                pane_numbers.insert(*pane_number),
                "workspace {} duplicate public pane number {} for pane {:?}",
                self.id,
                pane_number,
                pane_id
            );
            max_pane_number = max_pane_number.max(*pane_number);
        }
        assert!(
            self.next_public_pane_number > 0,
            "workspace {} next_public_pane_number must be greater than 0",
            self.id
        );
        assert!(
            self.next_public_pane_number > max_pane_number,
            "workspace {} next_public_pane_number {} must be greater than max live public pane number {}",
            self.id,
            self.next_public_pane_number,
            max_pane_number
        );

        // Shape invariant: a scope carrying `mirrored_name`
        // classifies IdClass::Remote; a scope that does not classify Remote
        // never carries a mirrored_name.
        let ws_is_remote = self.is_federation_materialized();
        assert!(
            ws_is_remote || self.mirrored_name.is_none(),
            "workspace {} is not federation-materialized but carries a mirrored_name",
            self.id
        );
        for (tab_idx, tab) in self.tabs.iter().enumerate() {
            assert!(
                ws_is_remote || tab.mirrored_name.is_none(),
                "workspace {} tab {} is not federation-materialized but carries a mirrored_name",
                self.id,
                tab_idx
            );
        }

        // Resolution invariant: no two auto-named tabs (tabs with no
        // rung-1 override of their own) in one workspace may resolve to the
        // same displayed string — `apply_tab_label_distinctness` exists
        // specifically to guarantee this at the cache-write boundary.
        let mut auto_named_labels = std::collections::HashMap::new();
        for tab_idx in 0..self.tabs.len() {
            if !self.tabs[tab_idx].is_auto_named() {
                continue;
            }
            if let Some(label) = self.tab_display_name(tab_idx) {
                if let Some(previous_idx) = auto_named_labels.insert(label.clone(), tab_idx) {
                    panic!(
                        "workspace {} auto-named tabs {} and {} both resolve to {:?}",
                        self.id, previous_idx, tab_idx, label
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_workspace_ids_are_short_base32_handles() {
        let first = generate_workspace_id();
        let second = generate_workspace_id();

        assert!(first.starts_with('w'));
        assert!(second.starts_with('w'));
        assert_ne!(first, second);
        assert!(first.len() <= 3, "unexpectedly long workspace id: {first}");
        assert!(
            second.len() <= 3,
            "unexpectedly long workspace id: {second}"
        );
    }

    #[test]
    fn public_numbers_round_trip_readable_base32_handles() {
        assert_eq!(encode_public_number(1), "1");
        assert_eq!(encode_public_number(9), "9");
        assert_eq!(encode_public_number(10), "A");
        assert_eq!(encode_public_number(31), "Z");
        assert_eq!(encode_public_number(32), "0");
        assert_eq!(encode_public_number(33), "11");

        for value in [1, 9, 10, 31, 32, 33, 1024, 1025] {
            let encoded = encode_public_number(value);
            assert_eq!(decode_public_number(&encoded), Some(value));
        }
    }

    #[test]
    fn reserving_restored_workspace_ids_prevents_reuse() {
        let mut restored = Workspace::test_new("restored");
        restored.id = "wZ".to_string();

        reserve_workspace_ids(&[restored]);

        let generated = generate_workspace_id();
        assert_ne!(generated, "wZ");
        assert!(public_workspace_number(&generated) > public_workspace_number("wZ"));
    }

    #[test]
    fn pane_public_numbers_are_stable_and_not_reused_after_close() {
        let mut ws = Workspace::test_new("test");
        let root = ws.tabs[0].root_pane;
        let second = ws.test_split(Direction::Horizontal);
        let third = ws.test_split(Direction::Vertical);

        assert_eq!(ws.public_pane_number(root), Some(1));
        assert_eq!(ws.public_pane_number(second), Some(2));
        assert_eq!(ws.public_pane_number(third), Some(3));

        assert!(!ws.close_pane(second));

        assert_eq!(ws.public_pane_number(root), Some(1));
        assert_eq!(ws.public_pane_number(second), None);
        assert_eq!(ws.public_pane_number(third), Some(3));

        let fourth = ws.test_split(Direction::Horizontal);
        assert_eq!(ws.public_pane_number(fourth), Some(4));
    }

    #[test]
    fn tab_public_numbers_are_stable_and_not_reused_after_close() {
        let mut ws = Workspace::test_new("test");
        let first_root = ws.tabs[0].root_pane;
        let second_tab = ws.test_add_tab(None);
        let second_root = ws.tabs[second_tab].root_pane;
        let third_tab = ws.test_add_tab(None);
        let third_root = ws.tabs[third_tab].root_pane;

        assert_eq!(ws.public_tab_number_for_pane(first_root), Some(1));
        assert_eq!(ws.public_tab_number_for_pane(second_root), Some(2));
        assert_eq!(ws.public_tab_number_for_pane(third_root), Some(3));

        assert!(ws.close_tab(second_tab));

        assert_eq!(ws.public_tab_number_for_pane(first_root), Some(1));
        assert_eq!(ws.public_tab_number_for_pane(third_root), Some(3));

        let fourth_tab = ws.test_add_tab(None);
        let fourth_root = ws.tabs[fourth_tab].root_pane;
        assert_eq!(ws.public_tab_number_for_pane(fourth_root), Some(4));
        ws.assert_invariants_for_test();
    }

    #[test]
    fn adversarial_identity_state_satisfies_workspace_invariants_after_mutation() {
        let mut ws = Workspace::test_adversarial_identity_state();
        ws.assert_invariants_for_test();

        let active_public = ws.tabs[ws.active_tab].number;
        assert_ne!(ws.active_tab + 1, active_public);
        let divergent_pane = ws
            .public_pane_numbers
            .iter()
            .find_map(|(pane_id, public_number)| {
                (pane_id.raw() as usize != *public_number).then_some(*pane_id)
            })
            .expect("adversarial state should contain raw/public pane divergence");
        assert_ne!(
            divergent_pane.raw() as usize,
            ws.public_pane_number(divergent_pane).unwrap()
        );

        let new_pane = ws.test_split(Direction::Vertical);
        assert!(ws.public_pane_number(new_pane).is_some());
        assert!(ws.move_tab(ws.active_tab, ws.tabs.len()));
        ws.assert_invariants_for_test();
    }

    #[test]
    fn failed_moved_pane_insert_returns_pane_for_recovery() {
        let mut source = Workspace::test_new("source");
        let source_pane = source.tabs[0].root_pane;
        let taken = source
            .take_pane_for_move(source_pane)
            .expect("source pane should be movable");
        let mut target = Workspace::test_new("target");
        let missing_target = PaneId::alloc();

        let recovered = target
            .insert_moved_pane_into_tab(
                0,
                missing_target,
                taken.moved,
                Direction::Horizontal,
                0.5,
                true,
            )
            .expect_err("invalid target should return the moved pane");

        assert_eq!(recovered.pane_id, source_pane);
        assert!(!target.tabs[0].panes.contains_key(&source_pane));
    }

    #[test]
    fn linked_worktree_auto_label_uses_checkout_name_not_repo_name() {
        let (base, repo, checkout) =
            self::git::test_support::create_repo_with_linked_worktree("linked-auto-label");

        let (space, auto_label, _) = discover_workspace_git_identity(&checkout);

        assert_eq!(
            space.unwrap().repo_name,
            repo.file_name().unwrap().to_str().unwrap()
        );
        assert_eq!(auto_label, checkout.file_name().unwrap().to_str().unwrap());

        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn display_name_reads_cached_identity_without_rechecking_filesystem() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock should be after unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "herdr-workspace-label-cache-{}-{stamp}",
            std::process::id()
        ));
        let cwd = root.join("deep/nested");
        std::fs::create_dir_all(&cwd).expect("create nested cwd");

        let mut ws = Workspace::test_new("ignored");
        ws.custom_name = None;
        ws.identity_cwd = cwd.clone();
        ws.tabs.clear();
        ws.cached_identity_cwd = cwd;
        ws.cached_auto_label = "cached-repo".into();

        std::fs::remove_dir_all(root).expect("remove cwd after cache admission");

        assert_eq!(ws.display_name(), "cached-repo");
    }

    #[test]
    fn terminal_aware_display_name_uses_latest_admitted_identity_cache() {
        let mut ws = Workspace::test_new("ignored");
        let root_pane = ws.tabs[0].root_pane;
        let terminal_id = ws.tabs[0].terminal_id(root_pane).unwrap().clone();
        ws.custom_name = None;
        ws.identity_cwd = PathBuf::from("/old/workspace");
        ws.cached_identity_cwd = PathBuf::from("/new/repo/deep");
        ws.cached_auto_label = "repo".into();
        let terminals = HashMap::from([(
            terminal_id.clone(),
            TerminalState::new(terminal_id, PathBuf::from("/new/repo/deep")),
        )]);

        assert_eq!(ws.display_name_from_terminals(&terminals), "repo");
        assert_eq!(ws.display_name(), "workspace");
    }

    #[test]
    fn workspace_identity_follows_first_tab_root_pane_cwd() {
        let mut ws = Workspace::test_new("ignored");
        ws.custom_name = None;
        let root_pane = ws.tabs[0].root_pane;
        let terminal_id = ws.tabs[0].terminal_id(root_pane).unwrap().clone();
        let mut terminals = HashMap::new();
        terminals.insert(
            terminal_id.clone(),
            TerminalState::new(terminal_id, PathBuf::from("/herdr-test/pion")),
        );
        let terminal_runtimes = TerminalRuntimeRegistry::new();

        assert_eq!(ws.display_name_from(&terminals, &terminal_runtimes), "pion");
        assert_eq!(
            ws.resolved_identity_cwd_from(&terminals, &terminal_runtimes),
            Some(PathBuf::from("/herdr-test/pion"))
        );
    }

    /// R4: labels for auto-named tabs must track the stable public
    /// `tab.number`, not array position — see
    /// `tab_display_name_uses_public_tab_number_not_index` below, which
    /// pins the same rule at the label-resolution level.
    #[test]
    fn moving_tab_keeps_active_identity_and_stable_tab_numbers() {
        let mut ws = Workspace::test_new("test");
        let moved_root = ws.tabs[0].root_pane;
        ws.test_add_tab(Some("foo"));
        let final_auto_idx = ws.test_add_tab(None);
        let active_root = ws.tabs[final_auto_idx].root_pane;
        ws.switch_tab(final_auto_idx);

        assert!(ws.move_tab(0, ws.tabs.len()));

        let labels: Vec<_> = (0..ws.tabs.len())
            .map(|tab_idx| ws.tab_display_name(tab_idx).unwrap())
            .collect();
        assert_eq!(labels, vec!["foo", "3", "1"]);
        assert_eq!(ws.tabs[0].custom_name.as_deref(), Some("foo"));
        assert!(ws.tabs[1].custom_name.is_none());
        assert!(ws.tabs[2].custom_name.is_none());
        assert_eq!(ws.tabs[0].number, 2);
        assert_eq!(ws.tabs[1].number, 3);
        assert_eq!(ws.tabs[2].number, 1);
        assert_eq!(ws.tabs[2].root_pane, moved_root);
        assert_eq!(ws.tabs[ws.active_tab].root_pane, active_root);
        ws.assert_invariants_for_test();
    }

    /// Policy C, rung 4: the fallback ordinal comes from
    /// `public_tab_number(tab_idx)`, the stable public ordinal, never from
    /// `tab_idx + 1`, the array position. After a move the labels must
    /// therefore track the *numbers* `["foo", "3", "1"]`, not the
    /// *positions* `["foo", "2", "3"]`.
    #[test]
    fn tab_display_name_uses_public_tab_number_not_index() {
        let mut ws = Workspace::test_new("test");
        ws.test_add_tab(Some("foo"));
        let final_auto_idx = ws.test_add_tab(None);
        ws.switch_tab(final_auto_idx);

        assert!(ws.move_tab(0, ws.tabs.len()));

        // Numbers are stable identity and diverge from array position:
        // moving tab 0 to the end leaves the public numbers [2, 3, 1] while
        // the array positions are [0, 1, 2].
        assert_eq!(ws.tabs[0].number, 2);
        assert_eq!(ws.tabs[1].number, 3);
        assert_eq!(ws.tabs[2].number, 1);

        let labels: Vec<_> = (0..ws.tabs.len())
            .map(|tab_idx| ws.tab_display_name(tab_idx).unwrap())
            .collect();
        // Auto-named tabs at positions 1 and 2 report their stable public
        // numbers ("3" and "1"), not their 1-based array positions.
        assert_eq!(labels, vec!["foo", "3", "1"]);
        assert_eq!(
            labels[1],
            ws.public_tab_number(1).unwrap().to_string(),
            "the label at position 1 must equal its public number"
        );
        assert_eq!(
            labels[2],
            ws.public_tab_number(2).unwrap().to_string(),
            "the label at position 2 must equal its public number"
        );
    }

    /// The workspace name ladder short-circuits
    /// on `custom_name` (rung 1) at all three entry points
    /// (`display_name`/`display_name_from_terminals`/`display_name_from`,
    /// :1059/:1067/:1085), and only when it is `None` does
    /// `automatic_display_name_for_cwd` (:1099) consult the cache: the
    /// admitted `cached_auto_label` when the queried cwd equals
    /// `cached_identity_cwd`, else `fallback_label_from_cwd` on the raw cwd.
    #[test]
    fn char_workspace_name_ladder_override_then_cached_auto_then_fallback() {
        let mut ws = Workspace::test_new("override-name");
        let root_pane = ws.tabs[0].root_pane;
        let terminal_id = ws.tabs[0].terminal_id(root_pane).unwrap().clone();
        ws.cached_identity_cwd = PathBuf::from("/cached/repo");
        ws.cached_auto_label = "cached-repo".into();
        let terminal_runtimes = TerminalRuntimeRegistry::new();

        // Rung 1: an override wins over everything below it, regardless of
        // what the cache or terminal cwd say.
        let terminals_matching_cache = HashMap::from([(
            terminal_id.clone(),
            TerminalState::new(terminal_id.clone(), PathBuf::from("/cached/repo")),
        )]);
        assert_eq!(ws.display_name(), "override-name");
        assert_eq!(
            ws.display_name_from_terminals(&terminals_matching_cache),
            "override-name"
        );
        assert_eq!(
            ws.display_name_from(&terminals_matching_cache, &terminal_runtimes),
            "override-name"
        );

        // Clear the override: cwd == cached_identity_cwd hits the cached
        // auto label (no filesystem re-check).
        ws.custom_name = None;
        ws.identity_cwd = PathBuf::from("/cached/repo");
        assert_eq!(ws.display_name(), "cached-repo");
        assert_eq!(
            ws.display_name_from_terminals(&terminals_matching_cache),
            "cached-repo"
        );
        assert_eq!(
            ws.display_name_from(&terminals_matching_cache, &terminal_runtimes),
            "cached-repo"
        );

        // A cwd that does NOT match the cached identity cwd falls through
        // to `fallback_label_from_cwd` — the basename, not the cache.
        let terminals_diverging = HashMap::from([(
            terminal_id.clone(),
            TerminalState::new(terminal_id, PathBuf::from("/elsewhere/other-repo")),
        )]);
        assert_eq!(
            ws.display_name_from_terminals(&terminals_diverging),
            "other-repo"
        );
        assert_eq!(
            ws.display_name_from(&terminals_diverging, &terminal_runtimes),
            "other-repo"
        );
    }

    /// A mounted workspace whose remote cwd string
    /// *also* happens to name a real git root on this local machine (the
    /// `cached_auto_label` / cwd-derivation hazard `identity_cwd` and
    /// `automatic_display_name_for_cwd` share) must never let that local
    /// coincidence leak into the resolved name. `is_federation_materialized`
    /// gates rung 3 off entirely for a Remote-classified scope, so only
    /// `custom_name` (rung 1) or `mirrored_name` (rung 1.5) can ever win.
    #[test]
    fn mounted_workspace_with_a_locally_valid_remote_cwd_resolves_to_the_mirrored_label() {
        let mut ws = Workspace::test_new("mounted");
        ws.id = format!("r:mounted-host:{}", ws.id);
        ws.custom_name = None;
        ws.mirrored_name = Some("remote workspace".to_string());
        // The local cache is populated with a real-looking local git root
        // that happens to share a name with something the remote might
        // report — simulating the hazard workspace.rs:283-286 describes.
        ws.cached_identity_cwd = PathBuf::from("/home/alice/project");
        ws.cached_auto_label = "project".to_string();
        ws.identity_cwd = PathBuf::from("/home/alice/project");

        assert!(ws.is_federation_materialized());
        assert_eq!(
            ws.display_name(),
            "remote workspace",
            "a locally-valid cached auto label must never leak into a mounted scope's name"
        );
    }

    /// M3: `Workspace::test_new` unconditionally sets
    /// `custom_name: Some(name)` (:1231), so every test workspace built
    /// with it is already a *renamed* workspace. R1 (inheritance is
    /// tab -> pane only, never workspace -> tab) depends on this fact: if
    /// `test_new` ever stopped setting an override, any test relying on
    /// R1's guard would pass for the wrong reason.
    #[test]
    fn char_test_new_workspace_is_a_renamed_workspace() {
        let ws = Workspace::test_new("my-workspace");
        assert_eq!(ws.custom_name.as_deref(), Some("my-workspace"));
        assert_eq!(
            ws.identity_cwd,
            std::env::current_dir().unwrap_or_else(|_| "/".into())
        );
    }

    /// `move_tab` reassigns array positions but leaves each tab's public
    /// `number` untouched, so after a move the two diverge. This is the
    /// underlying fact that both `tab_display_name` (above) and the
    /// resolver's rung 4 depend on.
    #[test]
    fn char_public_tab_numbers_diverge_from_indexes_after_move() {
        let mut ws = Workspace::test_new("test");
        ws.test_add_tab(Some("foo"));
        ws.test_add_tab(None);

        assert!(ws.move_tab(0, ws.tabs.len()));

        let numbers: Vec<_> = (0..ws.tabs.len())
            .map(|idx| ws.public_tab_number(idx).unwrap())
            .collect();
        let indexes: Vec<_> = (0..ws.tabs.len()).map(|idx| idx + 1).collect();
        assert_eq!(numbers, vec![2, 3, 1]);
        assert_ne!(
            numbers, indexes,
            "public tab numbers must diverge from 1-based array positions after a move"
        );
    }
}
