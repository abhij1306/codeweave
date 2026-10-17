use super::util::stale_snapshot;
use super::*;
use crate::error::ErrorCode;
use crate::requests::retrieval::CodeRetrieveRequest;
use crate::retrieval::{
    prepare_retrieval_operation, PreparedRetrievalOperation, MAX_RETRIEVAL_OPERATIONS,
};

impl Workspace {
    /// Execute explicit repository discovery and read operations in one MCP call.
    /// Each entry is an atomic deterministic operation selected by the caller.
    pub fn code_retrieve(&self, request: &CodeRetrieveRequest) -> AppResult<Value> {
        let started = Instant::now();
        let operations = &request.operations;
        if operations.is_empty() {
            return Err(AppError::invalid(
                "code_retrieve operations[] cannot be empty",
            ));
        }
        if operations.len() > MAX_RETRIEVAL_OPERATIONS {
            return Err(AppError::with_details(
                ErrorCode::TooManyRetrievalOperations,
                format!("code_retrieve accepts at most {MAX_RETRIEVAL_OPERATIONS} operations"),
                json!({
                    "requested": operations.len(),
                    "maximum": MAX_RETRIEVAL_OPERATIONS
                }),
            ));
        }

        let snapshot = self.state.read().stamp().snapshot_id;
        if let Some(expected) = request.snapshot_id.as_deref() {
            if expected != snapshot {
                return Err(stale_snapshot(expected, &snapshot));
            }
        }

        let fail_fast = request.fail_fast;
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
                    "error": error
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
                let error = AppError::with_details(
                    ErrorCode::DuplicateRetrievalOperationId,
                    format!("duplicate retrieval operation id '{id}'"),
                    json!({"id": id}),
                );
                errors.push(json!({
                    "id": id,
                    "operation": operation_name,
                    "error": error
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
                    "error": error
                }));
                if fail_fast {
                    break;
                }
                continue;
            };

            match self.execute_retrieval_operation(kind, operation) {
                Ok(result) => results.push(json!({
                    "id": id,
                    "operation": kind,
                    "result": result
                })),
                Err(error) => {
                    errors.push(json!({
                        "id": id,
                        "operation": kind,
                        "error": error
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
        kind: &str,
        operation: &serde_json::Map<String, Value>,
    ) -> AppResult<Value> {
        match prepare_retrieval_operation(kind, operation)? {
            PreparedRetrievalOperation::Search(request) => self.search_index(&request),
            PreparedRetrievalOperation::Read(read) => {
                let result = self.read_target(&read.target, read.max_chars)?;
                if read.compact {
                    Ok(super::fetch::compact_fetch_result(&result))
                } else {
                    Ok(result)
                }
            }
        }
    }
}
