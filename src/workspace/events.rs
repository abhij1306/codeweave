use chrono::{DateTime, Utc};
use serde::Serialize;

/// Who made a recorded change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MutationSource {
    /// A CodeWeave edit (`mcp_edit`).
    McpEdit,
    /// A change made outside CodeWeave and found by reconcile (`external`).
    External,
}

impl MutationSource {
    /// The wire name, as clients filter by it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::McpEdit => "mcp_edit",
            Self::External => "external",
        }
    }
}

/// One change to one path, attributed to its source (`mcp_edit` or `external`) and
/// stamped with the generation that published it. Kept in the change ledger's
/// bounded in-memory log.
#[derive(Debug, Clone, Serialize)]
pub struct MutationRecord {
    pub mutation_id: String,
    pub path: String,
    pub before_hash: Option<String>,
    pub after_hash: Option<String>,
    pub source: MutationSource,
    pub request_id: String,
    pub timestamp: DateTime<Utc>,
    pub generation: u64,
}

impl MutationRecord {
    pub(super) fn new_id() -> String {
        format!("mut_{}", uuid::Uuid::new_v4().simple())
    }
}
