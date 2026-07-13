use crate::contracts::{self, ErrorCode};
pub use crate::contracts::{MAX_RETRIEVAL_OPERATIONS, READ_TARGETS};
use crate::index::{CodeIndex, SearchParams};
use crate::model::{bool_value, string_list, usize_value, AppError, AppResult};
use serde_json::{json, Map, Value};
use std::time::Instant;

pub const PROTOCOL_MAX_RESULTS: usize = 200;

#[derive(Debug, Clone)]
pub struct ReadOperation {
    pub item: Value,
    pub max_chars: usize,
    pub compact: bool,
}

#[derive(Debug, Clone)]
pub enum PreparedRetrievalOperation {
    Search(Value),
    Read(ReadOperation),
}

pub fn prepare_retrieval_operation(
    kind: &str,
    operation: &Map<String, Value>,
) -> AppResult<PreparedRetrievalOperation> {
    contracts::validate_retrieval_operation(kind, operation)?;
    let prepared = match kind {
        "find_file" => {
            let name = required_operation_str(operation, "name")?;
            PreparedRetrievalOperation::Search(search_params(operation, "filename", name))
        }
        "find_symbol" => {
            let symbol = required_operation_str(operation, "symbol")?;
            PreparedRetrievalOperation::Search(search_params(operation, "symbol", symbol))
        }
        "search_text" => {
            let pattern = required_operation_str(operation, "pattern")?;
            let syntax = operation
                .get("syntax")
                .and_then(Value::as_str)
                .unwrap_or("literal");
            if !matches!(syntax, "literal" | "regex") {
                return Err(ErrorCode::InvalidOperationField.error(
                    "search_text syntax must be literal or regex",
                    json!({"operation": kind, "field": "syntax"}),
                ));
            }
            PreparedRetrievalOperation::Search(search_params(operation, syntax, pattern))
        }
        "find_references" => {
            let symbol = required_operation_str(operation, "symbol")?;
            PreparedRetrievalOperation::Search(search_params(operation, "references", symbol))
        }
        "symbols_overview" => {
            let paths = operation_paths(operation);
            if paths.is_empty() {
                return Err(AppError::invalid("symbols_overview requires path or paths"));
            }
            let mut params = search_params(operation, "outline", "");
            params["paths"] = json!(paths);
            PreparedRetrievalOperation::Search(params)
        }
        "repo_map" => PreparedRetrievalOperation::Search(search_params(operation, "repo_map", "")),
        "read" => {
            let target = required_operation_str(operation, "target")?;
            if !READ_TARGETS.contains(&target) {
                return Err(AppError::details(
                    "UNSUPPORTED_READ_TARGET",
                    format!("unsupported read target '{target}'"),
                    json!({"target": target, "supported": READ_TARGETS}),
                ));
            }
            let value = required_operation_str(operation, "value")?;
            let mut item = json!({"kind": target, "value": value});
            for field in ["path", "start_line", "end_line", "include_imports"] {
                copy_field(operation, &mut item, field);
            }
            if let Some(value) = operation.get("surrounding_lines") {
                item["context_lines"] = value.clone();
            }
            let max_chars = operation
                .get("max_chars")
                .and_then(Value::as_u64)
                .map(|value| value as usize)
                .unwrap_or(30_000)
                .min(200_000);
            PreparedRetrievalOperation::Read(ReadOperation {
                item,
                max_chars,
                compact: operation.get("response_detail").and_then(Value::as_str)
                    == Some("compact"),
            })
        }
        unsupported => {
            return Err(ErrorCode::UnsupportedRetrievalOperation.error(
                format!("unsupported retrieval operation '{unsupported}'"),
                json!({
                    "operation": unsupported,
                    "supported": contracts::retrieval_operation_names()
                }),
            ))
        }
    };
    Ok(prepared)
}

pub fn execute_index_search(
    index: &CodeIndex,
    workspace_id: &str,
    snapshot_id: &str,
    params: &Value,
    configured_max_results: usize,
    reconcile_pending: bool,
) -> AppResult<Value> {
    let started = Instant::now();
    let mode = params
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or("literal");
    let queries = if let Some(values) = params.get("queries").and_then(Value::as_array) {
        values
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .ok_or_else(|| AppError::invalid("queries must contain strings"))
            })
            .collect::<AppResult<Vec<_>>>()?
    } else {
        vec![params
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or_default()]
    };
    if queries.is_empty() {
        return Err(AppError::invalid("queries cannot be empty"));
    }
    let paths = string_list(params, "paths");
    let reference_scope = params
        .get("reference_scope")
        .and_then(Value::as_str)
        .unwrap_or("all");
    if !matches!(reference_scope, "all" | "production" | "tests") {
        return Err(AppError::invalid(
            "reference_scope must be all, production, or tests",
        ));
    }
    let reference_kinds = string_list(params, "reference_kinds");
    if reference_kinds.iter().any(|kind| {
        !matches!(
            kind.as_str(),
            "declaration" | "call" | "import" | "type" | "read" | "write" | "other"
        )
    }) {
        return Err(AppError::invalid(
            "reference_kinds contains an unsupported kind",
        ));
    }
    let definition_path = params.get("definition_path").and_then(Value::as_str);
    let definition_line = params
        .get("definition_line")
        .and_then(Value::as_u64)
        .map(|value| value as usize);
    let (requested_results, applied_results, limit_warnings) =
        result_limit(params, 20, configured_max_results);

    if mode == "outline" && queries.len() == 1 && queries[0].is_empty() && paths.len() > 1 {
        let search_started = Instant::now();
        let mut results = Vec::new();
        let mut errors = Vec::new();
        for (index_number, path) in paths.iter().enumerate() {
            match index.search(SearchParams {
                workspace_id,
                snapshot_id,
                mode,
                query: path,
                path_filters: &[],
                case_sensitive: bool_value(params, "case_sensitive", false),
                max_results: applied_results,
                context_lines: usize_value(params, "context_lines", 2).min(20),
                reference_scope,
                reference_kinds: &reference_kinds,
                definition_path,
                definition_line,
            }) {
                Ok(result) => results.push(result),
                Err(error) => errors.push(json!({
                    "index": index_number,
                    "path": path,
                    "error": error.0
                })),
            }
        }
        let mut result = json!({
            "mode": mode,
            "snapshot_id": snapshot_id,
            "result_count": results.len(),
            "error_count": errors.len(),
            "partial_success": !results.is_empty() && !errors.is_empty(),
            "results": results,
            "errors": errors,
        });
        add_reconcile_pending(&mut result, reconcile_pending);
        add_result_limits(
            &mut result,
            requested_results,
            applied_results,
            configured_max_results,
            20,
            limit_warnings,
        );
        add_phase_metrics(
            &mut result,
            &[
                ("index_search", search_started.elapsed().as_millis()),
                ("total_local", started.elapsed().as_millis()),
            ],
        );
        return Ok(result);
    }

    let run_search = |query: &str| {
        let effective_query = if mode == "outline" && query.is_empty() {
            if paths.len() == 1 {
                paths[0].as_str()
            } else {
                return Err(AppError::details(
                    "INVALID_OUTLINE_PATH",
                    "Outline requires a file path in query or exactly one paths entry",
                    json!({
                        "paths_count": paths.len(),
                        "retryable": true,
                        "retry_kind": "retry_with_changes",
                        "suggested_calls": paths.iter().map(|path| json!({
                            "mode": "outline",
                            "paths": [path]
                        })).collect::<Vec<_>>()
                    }),
                ));
            }
        } else {
            query
        };
        index.search(SearchParams {
            workspace_id,
            snapshot_id,
            mode,
            query: effective_query,
            path_filters: &paths,
            case_sensitive: bool_value(params, "case_sensitive", false),
            max_results: applied_results,
            context_lines: usize_value(params, "context_lines", 2).min(20),
            reference_scope,
            reference_kinds: &reference_kinds,
            definition_path,
            definition_line,
        })
    };

    if queries.len() == 1 {
        let search_started = Instant::now();
        let mut result = run_search(queries[0])?;
        add_reconcile_pending(&mut result, reconcile_pending);
        add_result_limits(
            &mut result,
            requested_results,
            applied_results,
            configured_max_results,
            20,
            limit_warnings,
        );
        add_phase_metrics(
            &mut result,
            &[
                ("index_search", search_started.elapsed().as_millis()),
                ("total_local", started.elapsed().as_millis()),
            ],
        );
        return Ok(result);
    }

    let mut results = Vec::new();
    let mut errors = Vec::new();
    let search_started = Instant::now();
    for query in &queries {
        match run_search(query) {
            Ok(result) => results.push(json!({"query": query, "result": result})),
            Err(error) => errors.push(json!({"query": query, "error": error.0})),
        }
    }
    let mut result = json!({
        "mode": mode,
        "snapshot_id": snapshot_id,
        "query_count": queries.len(),
        "result_count": results.len(),
        "error_count": errors.len(),
        "partial_success": !results.is_empty() && !errors.is_empty(),
        "results": results,
        "errors": errors,
    });
    add_reconcile_pending(&mut result, reconcile_pending);
    add_result_limits(
        &mut result,
        requested_results,
        applied_results,
        configured_max_results,
        20,
        limit_warnings,
    );
    add_phase_metrics(
        &mut result,
        &[
            ("index_search", search_started.elapsed().as_millis()),
            ("total_local", started.elapsed().as_millis()),
        ],
    );
    Ok(result)
}

fn required_operation_str<'a>(
    operation: &'a Map<String, Value>,
    field: &str,
) -> AppResult<&'a str> {
    operation
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            ErrorCode::InvalidOperationField.error(
                format!("retrieval operation field '{field}' must be a non-empty string"),
                json!({"field": field}),
            )
        })
}

fn operation_paths(operation: &Map<String, Value>) -> Vec<String> {
    let mut paths = operation
        .get("paths")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if let Some(path) = operation.get("path").and_then(Value::as_str) {
        if !paths.iter().any(|candidate| candidate == path) {
            paths.push(path.to_owned());
        }
    }
    paths
}

fn search_params(operation: &Map<String, Value>, mode: &str, selector: &str) -> Value {
    let mut params = json!({
        "mode": mode,
        "query": selector,
    });
    let paths = operation_paths(operation);
    if !paths.is_empty() {
        params["paths"] = json!(paths);
    }
    for field in [
        "max_results",
        "context_lines",
        "case_sensitive",
        "reference_scope",
        "reference_kinds",
        "definition_path",
        "definition_line",
    ] {
        copy_field(operation, &mut params, field);
    }
    params
}

fn copy_field(operation: &Map<String, Value>, target: &mut Value, field: &str) {
    if let Some(value) = operation.get(field) {
        target[field] = value.clone();
    }
}

fn result_limit(
    params: &Value,
    default: usize,
    configured_max: usize,
) -> (usize, usize, Vec<Value>) {
    let requested = usize_value(params, "max_results", default);
    let applied = requested.min(PROTOCOL_MAX_RESULTS).min(configured_max);
    let mut warnings = Vec::new();
    if requested > applied {
        let limit_message = if configured_max < PROTOCOL_MAX_RESULTS {
            format!("Requested {requested} results; the effective configured maximum is {configured_max}.")
        } else {
            format!("Requested {requested} results; protocol maximum is {PROTOCOL_MAX_RESULTS}.")
        };
        warnings.push(json!({"code": "MAX_RESULTS_CLAMPED", "message": limit_message}));
    }
    (requested, applied, warnings)
}

fn add_result_limits(
    result: &mut Value,
    requested: usize,
    applied: usize,
    configured_max: usize,
    configured_default: usize,
    warnings: Vec<Value>,
) {
    if let Some(object) = result.as_object_mut() {
        object.insert(
            "limits".to_owned(),
            json!({
                "requested_results": requested,
                "protocol_max_results": PROTOCOL_MAX_RESULTS,
                "configured_max_results": configured_max,
                "applied_results": applied,
                "configured_default_results": configured_default,
            }),
        );
        if !warnings.is_empty() {
            object.insert("warnings".to_owned(), Value::Array(warnings));
        }
    }
}

fn add_reconcile_pending(value: &mut Value, pending: bool) {
    if let Some(object) = value.as_object_mut() {
        object.insert("reconcile_pending".to_owned(), json!(pending));
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
