//! The `workspace` tool request.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Mutations returned by `action=changes` when `limit` is omitted.
pub const DEFAULT_CHANGES_LIMIT: usize = 200;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceAction {
    #[default]
    Summary,
    Refresh,
    Changes,
    Diagnostics,
}

// The schema stays open (no `additionalProperties: false`) as published;
// unknown fields are still rejected by the request contract.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct WorkspaceRequest {
    #[serde(default)]
    pub action: WorkspaceAction,
    pub force: Option<bool>,
    #[schemars(
        description = "For action=changes, return only mutations with a generation greater than this value."
    )]
    pub since_generation: Option<u64>,
    #[schemars(description = "For action=changes, return only mutations from this source.")]
    pub source: Option<String>,
    #[serde(default = "default_changes_limit")]
    #[schemars(
        range(min = 1, max = 2000),
        description = "Maximum number of mutations returned by action=changes."
    )]
    pub limit: usize,
}

fn default_changes_limit() -> usize {
    DEFAULT_CHANGES_LIMIT
}
