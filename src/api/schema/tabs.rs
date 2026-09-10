use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::common::AgentStatus;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TabCreateParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default)]
    pub focus: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub env: HashMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct TabListParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TabRenameParams {
    pub tab_id: String,
    /// `None` clears the override and snaps the tab back to its live
    /// derived name (rung 1.5/3/4), mirroring `AgentRenameParams::name`
    /// (`herdr agent rename <target> --clear`). A bare JSON string still
    /// deserializes into `Some(..)` for a caller that never omits this.
    ///
    /// Gated with `skip_serializing_if` so a clear serializes as an omitted
    /// key, never as a literal `null`: a server built before this field was
    /// nullable rejects `null` with a serde type error, where an absent key
    /// is the harmless no-op it always was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TabMoveParams {
    pub tab_id: String,
    pub insert_index: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TabInfo {
    pub tab_id: String,
    pub workspace_id: String,
    pub number: usize,
    pub label: String,
    /// Which rung of the naming ladder produced `label`
    /// (`docs/next/website/src/content/docs/concepts.mdx`). Clients switch
    /// on this instead of re-deriving what an auto-named tab "means" —
    /// e.g. only `Ordinal` should ever be styled as a bare position/dimmed.
    #[serde(default)]
    pub name_source: crate::workspace::naming::NameSource,
    pub focused: bool,
    pub pane_count: usize,
    pub agent_status: AgentStatus,
}
