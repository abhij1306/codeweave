use super::util::stale_snapshot;
use super::*;

const MAX_RETRIEVAL_OPERATIONS: usize = 12;
const READ_TARGETS: &[&str] = &[
    "path",
    "handle",
    "symbol",
    "metadata",
    "bash_status",
    "bash_log",
    "continuation",
];

impl WorkspaceActor {
    /// Execute explicit repository discovery and read operations in one MCP call.
    /// Each entry is an atomic deterministic operation selected by the caller.
    pub fn code_retrieve_for_session(&self, session_id: &str, params: &Value) -> AppResult<Value> {
        let started = Instant::now();
        let operations = params
            .get("operations")
            .and_then(Value::as_array)
            .ok_or_else(|| AppError::invalid("code_retrieve requires operations[]"))?;
        if operations.is_empty() {
            return Err(AppError::invalid(
                "code_retrieve operations[] cannot be empty",
            ));
        }
        if operations.len() > MAX_RETRIEVAL_OPERATIONS {
            return Err(AppError::details(
                "TOO_MANY_RETRIEVAL_OPERATIONS",
                format!("code_retrieve accepts at most {MAX_RETRIEVAL_OPERATIONS} operations"),
                json!({
                    "requested": operations.len(),
                    "maximum": MAX_RETRIEVAL_OPERATIONS
                }),
            ));
        }

        let snapshot = self.snapshot();
        if let Some(expected) = params.get("snapshot_id").and_then(Value::as_str) {
            if expected != snapshot {
                return Err(stale_snapshot(expected, &snapshot));
            }
        }

        let fail_fast = bool_value(params, "fail_fast", false);
        let mut seen_ids = HashSet::new();
        let mut results = Vec::new();
        let mut errors = Vec::new();

        for (index, operation_value) in operations.iter().enumerate() {
            let fallback_id = format!("op_{}", index + 1);
            let Some(operation) = operation_value.as_object() else {
                let error = AppError::invalid("retrieval operations must be objects");
                errors.push(json!({
                    "id": fallback_id,
                    "operation": Value::Null,
                    "error": error.0
                }));
                if fail_fast {
                    break;
                }
                continue;
            };
            let id = operation
                .get("id")
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .map(str::to_owned)
                .unwrap_or(fallback_id);
            let operation_name = operation.get("operation").cloned().unwrap_or(Value::Null);
            if !seen_ids.insert(id.clone()) {
                let error = AppError::details(
                    "DUPLICATE_RETRIEVAL_OPERATION_ID",
                    format!("duplicate retrieval operation id '{id}'"),
                    json!({"id": id}),
                );
                errors.push(json!({
                    "id": id,
                    "operation": operation_name,
                    "error": error.0
                }));
                if fail_fast {
                    break;
                }
                continue;
            }
            let Some(kind) = operation
                .get("operation")
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
            else {
                let error = AppError::invalid("retrieval operation requires operation");
                errors.push(json!({
                    "id": id,
                    "operation": Value::Null,
                    "error": error.0
                }));
                if fail_fast {
                    break;
                }
                continue;
            };

            match self.execute_retrieval_operation(session_id, kind, operation) {
                Ok(result) => results.push(json!({
                    "id": id,
                    "operation": kind,
                    "result": result
                })),
                Err(error) => {
                    errors.push(json!({
                        "id": id,
                        "operation": kind,
                        "error": error.0
                    }));
                    if fail_fast {
                        break;
                    }
                }
            }
        }

        Ok(json!({
            "retrieval_contract_version": 2,
            "snapshot_id": snapshot,
            "operation_count": operations.len(),
            "executed_count": results.len() + errors.len(),
            "result_count": results.len(),
            "error_count": errors.len(),
            "partial_success": !results.is_empty() && !errors.is_empty(),
            "execution": {
                "round_trips": 1,
                "parallel": false,
                "ordering": "request_order"
            },
            "results": results,
            "errors": errors,
            "phase_ms": {
                "total_local": started.elapsed().as_millis()
            }
        }))
    }

    fn execute_retrieval_operation(
        &self,
        session_id: &str,
        kind: &str,
        operation: &serde_json::Map<String, Value>,
    ) -> AppResult<Value> {
        match kind {
            "find_file" => {
                let name = operation_required_str(operation, "name")?;
                self.search_index(&search_params(operation, "filename", name))
            }
            "find_symbol" => {
                let symbol = operation_required_str(operation, "symbol")?;
                self.search_index(&search_params(operation, "symbol", symbol))
            }
            "search_text" => {
                let pattern = operation_required_str(operation, "pattern")?;
                let syntax = operation
                    .get("syntax")
                    .and_then(Value::as_str)
                    .unwrap_or("literal");
                if !matches!(syntax, "literal" | "regex") {
                    return Err(AppError::invalid(
                        "search_text syntax must be literal or regex",
                    ));
                }
                self.search_index(&search_params(operation, syntax, pattern))
            }
            "find_references" => {
                let symbol = operation_required_str(operation, "symbol")?;
                self.search_index(&search_params(operation, "references", symbol))
            }
            "symbols_overview" => {
                let paths = operation_paths(operation);
                if paths.is_empty() {
                    return Err(AppError::invalid("symbols_overview requires path or paths"));
                }
                let mut params = search_params(operation, "outline", "");
                params["paths"] = json!(paths);
                self.search_index(&params)
            }
            "repo_map" => self.search_index(&search_params(operation, "repo_map", "")),
            "read" => {
                let target = operation_required_str(operation, "target")?;
                if !READ_TARGETS.contains(&target) {
                    return Err(AppError::details(
                        "UNSUPPORTED_READ_TARGET",
                        format!("unsupported read target '{target}'"),
                        json!({"target": target, "supported": READ_TARGETS}),
                    ));
                }
                let value = operation_required_str(operation, "value")?;
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
                let result = self.read_target(session_id, &item, max_chars)?;
                if operation.get("response_detail").and_then(Value::as_str) == Some("compact") {
                    Ok(super::fetch::compact_fetch_result(&result))
                } else {
                    Ok(result)
                }
            }
            unsupported => Err(AppError::details(
                "UNSUPPORTED_RETRIEVAL_OPERATION",
                format!("unsupported retrieval operation '{unsupported}'"),
                json!({
                    "operation": unsupported,
                    "supported": [
                        "find_file",
                        "find_symbol",
                        "search_text",
                        "find_references",
                        "symbols_overview",
                        "repo_map",
                        "read"
                    ]
                }),
            )),
        }
    }
}

fn operation_required_str<'a>(
    operation: &'a serde_json::Map<String, Value>,
    field: &str,
) -> AppResult<&'a str> {
    operation
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| AppError::invalid(format!("retrieval operation requires {field}")))
}

fn operation_paths(operation: &serde_json::Map<String, Value>) -> Vec<String> {
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

fn search_params(operation: &serde_json::Map<String, Value>, mode: &str, selector: &str) -> Value {
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

fn copy_field(operation: &serde_json::Map<String, Value>, target: &mut Value, field: &str) {
    if let Some(value) = operation.get(field) {
        target[field] = value.clone();
    }
}
