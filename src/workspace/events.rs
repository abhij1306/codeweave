use chrono::{DateTime, Utc};
use serde::Serialize;

/// One change to one path, attributed to its source (`mcp_edit` or `external`) and
/// stamped with the generation that published it. Kept in the change ledger's
/// bounded in-memory log.
#[derive(Debug, Clone, Serialize)]
pub struct MutationRecord {
    pub mutation_id: String,
    pub path: String,
    pub before_hash: Option<String>,
    pub after_hash: Option<String>,
    pub source: String,
    pub request_id: String,
    pub timestamp: DateTime<Utc>,
    pub generation: u64,
}

impl MutationRecord {
    pub(super) fn new_id() -> String {
        format!("mut_{}", uuid::Uuid::new_v4().simple())
    }
}
