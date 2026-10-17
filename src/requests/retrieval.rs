//! The `code_retrieve` tool request. Each operation stays raw JSON until
//! `retrieval::prepare_retrieval_operation`, which reports a bad operation as a
//! per-operation error instead of failing the whole batch; the schema publishes
//! operations as `RetrievalOperationInput`.

use crate::contracts::MAX_RETRIEVAL_OPERATIONS;
use crate::retrieval::RetrievalOperationInput;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CodeRetrieveRequest {
    #[schemars(
        with = "Vec<RetrievalOperationInput>",
        length(min = 1, max = MAX_RETRIEVAL_OPERATIONS)
    )]
    pub operations: Vec<Value>,
    #[serde(default)]
    pub fail_fast: bool,
    #[schemars(
        description = "Optional workspace snapshot precondition for the complete retrieval batch."
    )]
    pub snapshot_id: Option<String>,
}
