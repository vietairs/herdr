use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::common::AgentStatus;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkspaceCreateParams {
    /// Workspace whose focused pane supplies the `follow` cwd policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default)]
    pub focus: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub env: HashMap<String, String>,
}

/// REVISED Phase A (multi-remote federated workspace launch); generalized to
/// N targets in Phase B: mounts one or more federation targets as
/// server-daemon-owned state, alongside the local workspace(s) already
/// running in this session. `main.rs` sends this instead of running the
/// federation driver itself (runtime/client boundary guardrail — mount =
/// shared runtime/session fact). One request carries the full target list
/// (Phase B requirement 9's "one request with a target list" option) so the
/// server-side handler owns the concurrent-dial fan-out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkspaceMountRemoteParams {
    pub targets: Vec<String>,
    #[serde(default)]
    pub remote_keybindings: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkspaceCloseParams {
    pub workspace_id: String,
    #[serde(default, skip_serializing_if = "super::is_false")]
    pub close_group: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkspaceRenameParams {
    pub workspace_id: String,
    /// `None` clears the override and snaps the workspace back to its live
    /// derived name. Same `skip_serializing_if` gating and same reason as
    /// `TabRenameParams::label`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkspaceMoveParams {
    pub workspace_id: String,
    pub insert_index: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkspaceMoveBlockParams {
    pub workspace_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_workspace_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkspaceReportMetadataParams {
    pub workspace_id: String,
    pub source: String,
    #[schemars(schema_with = "super::common::metadata_token_patch_schema")]
    pub tokens: HashMap<String, Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 86_400_000))]
    pub ttl_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkspaceInfo {
    pub workspace_id: String,
    pub number: usize,
    pub label: String,
    /// Which rung of the naming ladder produced `label`
    /// (`docs/next/website/src/content/docs/concepts.mdx`).
    #[serde(default)]
    pub name_source: crate::workspace::naming::NameSource,
    pub focused: bool,
    pub pane_count: usize,
    pub tab_count: usize,
    pub active_tab_id: String,
    pub agent_status: AgentStatus,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[schemars(schema_with = "super::common::metadata_token_values_schema")]
    pub tokens: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<WorkspaceWorktreeInfo>,
    /// Host address (`user@ip`) this workspace was federation-mounted from;
    /// `None` for a local workspace. Runtime/session fact, so it is populated
    /// SERVER-side, not derived by a client from `workspace_id`.
    ///
    /// Security property this preserves: it is set exclusively from
    /// `remote::federation::id::classify(&ws.id)`, and a workspace's `id` is
    /// only ever set to a `FedRef::to_public_id()` value (`r:<host_key>:...`)
    /// by the local mount/materialization path, keyed off the *client's own*
    /// trusted `HostKey` — never anything the remote host sends. No
    /// remote-influenced string (e.g. `custom_name`, `label`) ever feeds this
    /// field, so a crafted remote value can neither spoof nor suppress it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub federation_origin: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkspaceWorktreeInfo {
    pub repo_key: String,
    pub repo_name: String,
    pub repo_root: String,
    pub checkout_path: String,
    pub is_linked_worktree: bool,
}
