//! `code_retrieve` search operations: typed requests and their execution
//! against the index and the reference service.

mod request;

pub use crate::contracts::{MAX_RETRIEVAL_OPERATIONS, READ_TARGETS};
pub use request::{
    prepare_retrieval_operation, PreparedRetrievalOperation, ReadOperation, ReadTarget,
    RetrievalOperationInput, SearchRequest, SearchTarget, DEFAULT_CONTEXT_LINES,
    DEFAULT_MAX_RESULTS, DEFAULT_READ_MAX_CHARS, MAX_CONTEXT_LINES, MAX_READ_MAX_CHARS,
    MAX_SURROUNDING_LINES, PROTOCOL_MAX_RESULTS,
};

use crate::error::{AppError, AppResult, ErrorCode};
use crate::index::{CodeIndex, SearchScope};
use crate::reference_service::ReferenceService;
use serde_json::{json, Value};
use std::time::Instant;

/// Run one search request against the index.
pub fn execute_index_search(
    index: &CodeIndex,
    workspace_id: &str,
    snapshot_id: &str,
    request: &SearchRequest,
    configured_max_results: usize,
    reconcile_pending: bool,
) -> AppResult<Value> {
    let started = Instant::now();
    let limits = ResultLimits::new(request.max_results, configured_max_results);
    let scope = SearchScope {
        workspace_id,
        snapshot_id,
        paths: &request.paths,
        max_results: limits.applied,
    };
    let search_started = Instant::now();
    let mut result = match &request.target {
        SearchTarget::Text(query) => index.search_text(&scope, query)?,
        SearchTarget::Filename(query) => index.search_filenames(&scope, query)?,
        SearchTarget::Symbol(query) => index.search_symbols(&scope, query)?,
        SearchTarget::References(query) => ReferenceService::new(index).fallback(&scope, query)?,
        SearchTarget::Outline => outlines(index, snapshot_id, &request.paths)?,
        SearchTarget::RepoMap => index.repo_map(&scope),
    };
    if let Some(object) = result.as_object_mut() {
        object.insert("reconcile_pending".to_owned(), json!(reconcile_pending));
    }
    limits.add_to(&mut result);
    add_phase_metrics(
        &mut result,
        &[
            ("index_search", search_started.elapsed().as_millis()),
            ("total_local", started.elapsed().as_millis()),
        ],
    );
    Ok(result)
}

/// One outline for a single path; a batch with per-path errors otherwise.
fn outlines(index: &CodeIndex, snapshot_id: &str, paths: &[String]) -> AppResult<Value> {
    match paths {
        [] => Err(AppError::with_details(
            ErrorCode::InvalidOutlinePath,
            "Outline requires a file path in query or exactly one paths entry",
            json!({
                "paths_count": 0,
                "retryable": true,
                "retry_kind": "retry_with_changes",
                "suggested_calls": []
            }),
        )),
        [path] => index.outline(path),
        _ => {
            let mut results = Vec::new();
            let mut errors = Vec::new();
            for (position, path) in paths.iter().enumerate() {
                match index.outline(path) {
                    Ok(result) => results.push(result),
                    Err(error) => errors.push(json!({
                        "index": position,
                        "path": path,
                        "error": error
                    })),
                }
            }
            Ok(json!({
                "mode": "outline",
                "snapshot_id": snapshot_id,
                "result_count": results.len(),
                "error_count": errors.len(),
                "partial_success": !results.is_empty() && !errors.is_empty(),
                "results": results,
                "errors": errors,
            }))
        }
    }
}

struct ResultLimits {
    requested: usize,
    applied: usize,
    configured_max: usize,
}

impl ResultLimits {
    fn new(requested: usize, configured_max: usize) -> Self {
        Self {
            requested,
            applied: requested.min(PROTOCOL_MAX_RESULTS).min(configured_max),
            configured_max,
        }
    }

    fn add_to(&self, result: &mut Value) {
        let Some(object) = result.as_object_mut() else {
            return;
        };
        object.insert(
            "limits".to_owned(),
            json!({
                "requested_results": self.requested,
                "protocol_max_results": PROTOCOL_MAX_RESULTS,
                "configured_max_results": self.configured_max,
                "applied_results": self.applied,
                "configured_default_results": DEFAULT_MAX_RESULTS,
            }),
        );
        if self.requested > self.applied {
            let message = if self.configured_max < PROTOCOL_MAX_RESULTS {
                format!(
                    "Requested {} results; the effective configured maximum is {}.",
                    self.requested, self.configured_max
                )
            } else {
                format!(
                    "Requested {} results; protocol maximum is {PROTOCOL_MAX_RESULTS}.",
                    self.requested
                )
            };
            object.insert(
                "warnings".to_owned(),
                json!([{"code": "MAX_RESULTS_CLAMPED", "message": message}]),
            );
        }
    }
}

fn add_phase_metrics(value: &mut Value, phases: &[(&str, u128)]) {
    if let Some(object) = value.as_object_mut() {
        object.insert(
            "phase_ms".to_owned(),
            Value::Object(
                phases
                    .iter()
                    .map(|(name, elapsed)| ((*name).to_owned(), json!(elapsed)))
                    .collect(),
            ),
        );
    }
}
