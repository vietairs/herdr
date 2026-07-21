//! Internal app events delivered via channel.
//!
//! Background tasks (PTY child watchers, future hook listeners, etc.) send
//! events to the main loop through this channel. No polling needed.

use std::time::Instant;

use crate::detect::{Agent, AgentState};
use crate::layout::PaneId;
use crate::workspace::{GitStatusCacheEntry, WorkspaceGitStatus};

#[derive(Debug)]
pub struct ApiWorktreeAddRequest {
    pub id: String,
    pub operation_id: u64,
    pub checkout_key: std::path::PathBuf,
    pub source_workspace_id: Option<String>,
    pub source_existing_membership: Option<crate::workspace::WorktreeSpaceMembership>,
    pub source_checkout_path: std::path::PathBuf,
    pub source_repo_root: std::path::PathBuf,
    pub repo_key: String,
    pub repo_name: String,
    pub label: Option<String>,
    pub focus: bool,
    pub respond_to: std::sync::mpsc::Sender<String>,
}

#[derive(Debug)]
pub struct WorktreeAddResult {
    pub path: std::path::PathBuf,
    pub api_request: Option<ApiWorktreeAddRequest>,
    pub result: Result<(), String>,
}

#[derive(Debug)]
pub struct ApiWorktreeRemoveRequest {
    pub id: String,
    pub operation_id: u64,
    pub checkout_key: std::path::PathBuf,
    pub respond_to: std::sync::mpsc::Sender<String>,
}

#[derive(Debug)]
pub struct WorktreeRemoveResult {
    pub workspace_id: String,
    pub path: std::path::PathBuf,
    pub workspace: Option<Box<crate::api::schema::WorkspaceInfo>>,
    pub worktree: Option<Box<crate::api::schema::WorktreeInfo>>,
    pub forced: bool,
    pub api_request: Option<ApiWorktreeRemoveRequest>,
    pub result: Result<(), String>,
}

/// An event from a background task to the main loop.
#[derive(Debug)]
pub enum AppEvent {
    /// A pane's child process exited.
    PaneDied { pane_id: PaneId },
    /// Fallback detector state changed in a pane.
    StateChanged {
        pane_id: PaneId,
        agent: Option<Agent>,
        state: AgentState,
        visible_blocker: bool,
        visible_working: bool,
        process_exited: bool,
        observed_at: Instant,
    },
    /// Hook-authoritative agent state was reported for a pane.
    HookStateReported {
        pane_id: PaneId,
        source: String,
        agent_label: String,
        state: AgentState,
        message: Option<String>,
        seq: Option<u64>,
        session_ref: Option<crate::agent_resume::AgentSessionRef>,
    },
    /// Agent session identity was reported without state authority.
    AgentSessionReported {
        pane_id: PaneId,
        source: String,
        agent_label: String,
        seq: Option<u64>,
        session_ref: Option<crate::agent_resume::AgentSessionRef>,
        session_start_source: Option<String>,
    },
    /// Display-only agent metadata was reported for a pane.
    HookMetadataReported {
        pane_id: PaneId,
        source: String,
        agent_label: Option<String>,
        applies_to_source: Option<String>,
        title: Option<String>,
        display_agent: Option<String>,
        state_labels: std::collections::HashMap<String, String>,
        clear_title: bool,
        clear_display_agent: bool,
        clear_state_labels: bool,
        seq: Option<u64>,
        ttl: Option<std::time::Duration>,
    },
    /// Hook authority was explicitly cleared for a pane.
    HookAuthorityCleared {
        pane_id: PaneId,
        source: Option<String>,
        seq: Option<u64>,
    },
    /// The current detected agent gracefully released this pane back to the shell.
    HookAgentReleased {
        pane_id: PaneId,
        source: String,
        agent_label: String,
        known_agent: Option<Agent>,
        seq: Option<u64>,
    },
    /// A new version is available through the active installation manager.
    UpdateReady {
        version: String,
        install_command: String,
    },
    /// Remote agent detection manifest update check finished.
    AgentDetectionManifestsUpdated {
        updated: Vec<crate::detect::manifest_update::ManifestUpdateCommit>,
        status: crate::detect::manifest_update::ManifestUpdateStatus,
    },
    /// A pane child emitted a valid OSC 52 clipboard write. The main loop
    /// re-emits it through herdr's own clipboard writer.
    ClipboardWrite { content: Vec<u8> },
    /// Prefix-mode ASCII input-source request, emitted on entering/leaving the ASCII input
    /// realm. The foreground process applies the host-local TIS switch (`active = true`) /
    /// restore (`active = false`): the client in server mode (via server forwarding), the
    /// app itself in monolithic mode.
    PrefixInputSource { active: bool },
    /// A pane child reported its shell current directory through terminal
    /// metadata such as OSC 7.
    TerminalCwdReported {
        pane_id: PaneId,
        cwd: std::path::PathBuf,
    },
    /// Background git status refresh completed for workspaces.
    GitStatusRefreshed {
        results: Vec<WorkspaceGitStatus>,
        cache_updates: Vec<(std::path::PathBuf, GitStatusCacheEntry)>,
    },
    /// A plugin action or event command finished.
    PluginCommandFinished {
        log_id: String,
        finished_unix_ms: u64,
        exit_code: Option<i32>,
        stdout: String,
        stderr: String,
        error: Option<String>,
    },
    /// Background `git worktree add` completed.
    WorktreeAddFinished(Box<WorktreeAddResult>),
    /// Background `git worktree remove` completed.
    WorktreeRemoveFinished(Box<WorktreeRemoveResult>),
    /// A server-owned `workspace.mount_remote` dial+mount task succeeded.
    /// `App::run`'s own tick materializes the mirror into real
    /// workspaces/tabs/panes (needs `&mut App`, which the spawned task
    /// itself cannot hold) and spawns the ongoing drive task. Unix-only:
    /// mirrors the `#[cfg(unix)]` gate on `remote::unix`, which owns the
    /// dial/mount primitives this payload carries.
    #[cfg(unix)]
    FederationMountReady(Box<FederationMountReady>),
    /// A server-owned `workspace.mount_remote` dial+mount task failed
    /// (SSH/timeout/unsupported/empty mirror). Local workspace(s) and the
    /// server daemon itself are unaffected; surfaces as a sidebar notice.
    #[cfg(unix)]
    FederationMountFailed { target: String, reason: String },
    /// A federation link's drive task ended (closed, faulted, or errored)
    /// for a specific mount generation. The handler fences on `generation`
    /// so a stale ended-notice from a superseded drive task cannot tear
    /// down a mount that has since been remounted.
    #[cfg(unix)]
    FederationMountEnded {
        host_key: crate::remote::federation::id::HostKey,
        generation: u64,
        target: String,
        reason: String,
    },
    /// A live mount's drive task (`remote::federation::client::
    /// drive_mount_channel`) finished materializing a locally-spawned
    /// `TerminalRuntime` for a remote host's `SplitPaneResponse::Created` —
    /// everything `App::handle_federation_split_pane_ready`
    /// (`app/creation.rs`) needs to splice the new pane into the requesting
    /// pane's own tab layout, without the drive task itself ever touching
    /// `&mut App`.
    #[cfg(unix)]
    FederationSplitPaneReady(Box<FederationSplitPaneReady>),
    /// The remote host rejected (or the local mount could not honor) an
    /// earlier `SplitPaneRequest`. Carries the same `request_id` so the
    /// handler can drop the matching pending-split context.
    #[cfg(unix)]
    FederationSplitPaneFailed { request_id: u64, reason: String },
}

/// Payload for [`AppEvent::FederationSplitPaneReady`] — the fully-built local
/// counterpart of a remote-created pane, assembled by the mount's own drive
/// task (which owns the `TerminalChannelRouter`/mount out-tx a
/// `TerminalRuntime::spawn_remote` call needs) and handed back to `App` for
/// layout insertion.
#[cfg(unix)]
pub struct FederationSplitPaneReady {
    pub request_id: u64,
    pub pane_id: crate::layout::PaneId,
    pub terminal_id: crate::terminal::TerminalId,
    pub terminal: crate::terminal::TerminalState,
    pub runtime: crate::terminal::TerminalRuntime,
    pub pane_state: crate::pane::PaneState,
}

// `TerminalRuntime`/`TerminalState` don't derive `Debug`, matching
// `FederationMountReady`'s existing precedent above.
#[cfg(unix)]
impl std::fmt::Debug for FederationSplitPaneReady {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FederationSplitPaneReady")
            .field("request_id", &self.request_id)
            .field("pane_id", &self.pane_id)
            .finish_non_exhaustive()
    }
}

/// Payload for [`AppEvent::FederationMountReady`] — everything
/// `App::materialize_federation_mount` + the ongoing drive task need, handed
/// back from the server-owned dial+mount task spawned by the
/// `workspace.mount_remote` API handler.
#[cfg(unix)]
pub struct FederationMountReady {
    pub target: String,
    pub mirror: crate::remote::federation::reducer::RemoteMirror,
    pub generation: u64,
    pub tunnel_guard: crate::remote::ChildGuard,
    pub tunnel_reader: tokio::process::ChildStdout,
    pub tunnel_writer: tokio::process::ChildStdin,
}

// `RemoteMirror`/`ChildGuard` don't derive `Debug` (out of this phase's file
// ownership to change), so `AppEvent`'s own `#[derive(Debug)]` needs a manual
// impl here rather than pulling the whole enum off the derive.
#[cfg(unix)]
impl std::fmt::Debug for FederationMountReady {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FederationMountReady")
            .field("target", &self.target)
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}
