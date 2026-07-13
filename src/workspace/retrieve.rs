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

        let fail_fast = bool_value(params, "fail_fast", false);
        let mut seen_ids = HashSet::new();
        let mut results = Vec::new();
        let mut errors = Vec::new();

        for (index, operation) in operations.iter().enumerate() {
            let operation = operation
                .as_object()
                .ok_or_else(|| AppError::invalid("retrieval operations must be objects"))?;
            let kind = operation
                .get("operation")
                .and_then(Value::as_str)
                .ok_or_else(|| AppError::invalid("retrieval operation requires operation"))?;
            let id = operation
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| format!("op_{}", index + 1));
            if !seen_ids.insert(id.clone()) {
                return Err(AppError::details(
                    "DUPLICATE_RETRIEVAL_OPERATION_ID",
                    format!("duplicate retrieval operation id '{id}'"),
                    json!({"id": id}),
                ));
            }

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
            "snapshot_id": self.snapshot(),
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
                self.code_search(&search_params(operation, "filename", name))
            }
            "find_symbol" => {
                let symbol = operation_required_str(operation, "symbol")?;
                self.code_search(&search_params(operation, "symbol", symbol))
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
                self.code_search(&search_params(operation, syntax, pattern))
            }
            "find_references" => {
                let symbol = operation_required_str(operation, "symbol")?;
                self.code_search(&search_params(operation, "references", symbol))
            }
            "symbols_overview" => {
                let paths = operation_paths(operation);
                if paths.is_empty() {
                    return Err(AppError::invalid("symbols_overview requires path or paths"));
                }
                let mut params = search_params(operation, "outline", "");
                params["paths"] = json!(paths);
                self.code_search(&params)
            }
            "repo_map" => self.code_search(&search_params(operation, "repo_map", "")),
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
                let mut params = json!({"items": [item]});
                copy_field(operation, &mut params, "max_chars");
                copy_field(operation, &mut params, "response_detail");
                self.code_fetch_for_session(session_id, &params)
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
