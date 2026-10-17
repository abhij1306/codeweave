//! The `code_intelligence` tool request. `IntelligenceRequest` in the
//! intelligence module turns it into a per-operation request.

use schemars::JsonSchema;
use serde::Deserialize;

/// Results returned when `max_results` is omitted.
pub const DEFAULT_MAX_RESULTS: usize = 20;
/// Largest accepted `max_results`.
pub const MAX_RESULTS: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum IntelligenceOperationName {
    Definition,
    References,
    Diagnostics,
    RenamePreview,
}

// Plain comments throughout: schemars publishes doc comments as descriptions.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CodeIntelligenceRequest {
    pub operation: IntelligenceOperationName,
    pub path: Option<String>,
    #[schemars(range(min = 1), description = "One-based line number.")]
    pub line: Option<u32>,
    #[schemars(
        range(min = 0),
        description = "Zero-based UTF-16 code-unit offset within the line; CodeWeave converts it to the server's negotiated position encoding."
    )]
    pub column: Option<u32>,
    pub new_name: Option<String>,
    #[serde(default = "default_max_results")]
    #[schemars(
        range(min = 1, max = MAX_RESULTS),
        description = "Maximum references or diagnostics returned. Diagnostics also report total_count, result_count, and truncated."
    )]
    pub max_results: usize,
}

fn default_max_results() -> usize {
    DEFAULT_MAX_RESULTS
}
