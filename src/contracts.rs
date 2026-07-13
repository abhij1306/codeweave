use crate::model::{AppError, AppResult};
use serde_json::{json, Map, Value};
use std::collections::BTreeSet;

pub const MAX_RETRIEVAL_OPERATIONS: usize = 12;
pub const READ_TARGETS: &[&str] = &[
    "path",
    "handle",
    "symbol",
    "metadata",
    "bash_status",
    "bash_log",
    "continuation",
];

#[derive(Debug, Clone, Copy)]
pub struct ContractSpec {
    pub name: &'static str,
    pub required: &'static [&'static str],
    pub optional: &'static [&'static str],
}

impl ContractSpec {
    pub fn allowed_fields(self) -> impl Iterator<Item = &'static str> {
        self.required.iter().chain(self.optional.iter()).copied()
    }
}

pub const RETRIEVAL_CONTRACTS: &[ContractSpec] = &[
    ContractSpec {
        name: "find_file",
        required: &["operation", "name"],
        optional: &[
            "id",
            "path",
            "paths",
            "max_results",
            "context_lines",
            "case_sensitive",
            "response_detail",
        ],
    },
    ContractSpec {
        name: "find_symbol",
        required: &["operation", "symbol"],
        optional: &[
            "id",
            "path",
            "paths",
            "max_results",
            "context_lines",
            "case_sensitive",
            "response_detail",
        ],
    },
    ContractSpec {
        name: "search_text",
        required: &["operation", "pattern"],
        optional: &[
            "id",
            "syntax",
            "path",
            "paths",
            "max_results",
            "context_lines",
            "case_sensitive",
            "response_detail",
        ],
    },
    ContractSpec {
        name: "find_references",
        required: &["operation", "symbol"],
        optional: &[
            "id",
            "path",
            "paths",
            "max_results",
            "context_lines",
            "case_sensitive",
            "reference_scope",
            "reference_kinds",
            "definition_path",
            "definition_line",
            "response_detail",
        ],
    },
    ContractSpec {
        name: "symbols_overview",
        required: &["operation"],
        optional: &["id", "path", "paths", "max_results", "response_detail"],
    },
    ContractSpec {
        name: "repo_map",
        required: &["operation"],
        optional: &["id", "path", "paths", "max_results", "response_detail"],
    },
    ContractSpec {
        name: "read",
        required: &["operation", "target", "value"],
        optional: &[
            "id",
            "path",
            "start_line",
            "end_line",
            "surrounding_lines",
            "include_imports",
            "max_chars",
            "response_detail",
        ],
    },
];

pub const CHANGE_CONTRACTS: &[ContractSpec] = &[
    ContractSpec {
        name: "create",
        required: &["kind", "path", "content"],
        optional: &["overwrite", "expected_hash"],
    },
    ContractSpec {
        name: "replace",
        required: &["kind", "path", "old_text", "new_text"],
        optional: &["expected_replacements", "expected_hash", "handle"],
    },
    ContractSpec {
        name: "replace_range",
        required: &["kind", "path", "handle", "new_text"],
        optional: &[],
    },
    ContractSpec {
        name: "insert",
        required: &["kind", "path", "content", "anchor_symbol", "position"],
        optional: &["expected_hash"],
    },
    ContractSpec {
        name: "delete",
        required: &["kind", "path"],
        optional: &["expected_hash"],
    },
    ContractSpec {
        name: "rename",
        required: &["kind", "path", "to"],
        optional: &["expected_hash"],
    },
];

pub const BASH_CONTRACTS: &[ContractSpec] = &[
    ContractSpec {
        name: "bash",
        required: &["command"],
        optional: &["cwd", "background", "timeout_ms"],
    },
    ContractSpec {
        name: "bash_status",
        required: &["run_id"],
        optional: &[],
    },
    ContractSpec {
        name: "bash_output",
        required: &["run_id"],
        optional: &["stream", "continuation"],
    },
    ContractSpec {
        name: "bash_cancel",
        required: &["run_id"],
        optional: &[],
    },
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    MissingOperationField,
    InvalidOperationField,
    UnknownOperationField,
    UnsupportedRetrievalOperation,
    MissingChangeField,
    InvalidChangeField,
    UnknownChangeField,
    UnsupportedChangeKind,
    InvalidBashRequest,
}

#[derive(Debug, Clone, Copy)]
pub struct ErrorPolicy {
    pub code: &'static str,
    pub retryable: bool,
    pub retry_kind: &'static str,
    pub category: &'static str,
}

impl ErrorCode {
    pub const ALL: [Self; 9] = [
        Self::MissingOperationField,
        Self::InvalidOperationField,
        Self::UnknownOperationField,
        Self::UnsupportedRetrievalOperation,
        Self::MissingChangeField,
        Self::InvalidChangeField,
        Self::UnknownChangeField,
        Self::UnsupportedChangeKind,
        Self::InvalidBashRequest,
    ];

    pub const fn policy(self) -> ErrorPolicy {
        match self {
            Self::MissingOperationField => ErrorPolicy {
                code: "MISSING_OPERATION_FIELD",
                retryable: false,
                retry_kind: "fix_request",
                category: "invalid_request",
            },
            Self::InvalidOperationField => ErrorPolicy {
                code: "INVALID_OPERATION_FIELD",
                retryable: false,
                retry_kind: "fix_request",
                category: "invalid_request",
            },
            Self::UnknownOperationField => ErrorPolicy {
                code: "UNKNOWN_OPERATION_FIELD",
                retryable: false,
                retry_kind: "remove_field",
                category: "invalid_request",
            },
            Self::UnsupportedRetrievalOperation => ErrorPolicy {
                code: "UNSUPPORTED_RETRIEVAL_OPERATION",
                retryable: false,
                retry_kind: "choose_supported_operation",
                category: "unsupported_request",
            },
            Self::MissingChangeField => ErrorPolicy {
                code: "MISSING_CHANGE_FIELD",
                retryable: false,
                retry_kind: "fix_request",
                category: "invalid_request",
            },
            Self::InvalidChangeField => ErrorPolicy {
                code: "INVALID_CHANGE_FIELD",
                retryable: false,
                retry_kind: "fix_request",
                category: "invalid_request",
            },
            Self::UnknownChangeField => ErrorPolicy {
                code: "UNKNOWN_CHANGE_FIELD",
                retryable: false,
                retry_kind: "remove_field",
                category: "invalid_request",
            },
            Self::UnsupportedChangeKind => ErrorPolicy {
                code: "UNSUPPORTED_CHANGE_KIND",
                retryable: false,
                retry_kind: "choose_supported_change",
                category: "unsupported_request",
            },
            Self::InvalidBashRequest => ErrorPolicy {
                code: "INVALID_BASH_REQUEST",
                retryable: false,
                retry_kind: "fix_request",
                category: "invalid_request",
            },
        }
    }

    pub fn error(self, message: impl Into<String>, details: Value) -> AppError {
        let policy = self.policy();
        let mut object = details.as_object().cloned().unwrap_or_default();
        object.insert("retryable".to_owned(), Value::Bool(policy.retryable));
        object.insert(
            "retry_kind".to_owned(),
            Value::String(policy.retry_kind.to_owned()),
        );
        object.insert(
            "category".to_owned(),
            Value::String(policy.category.to_owned()),
        );
        AppError::details(policy.code, message, Value::Object(object))
    }
}

pub fn retrieval_contract(name: &str) -> Option<ContractSpec> {
    RETRIEVAL_CONTRACTS
        .iter()
        .copied()
        .find(|contract| contract.name == name)
}

pub fn change_contract(name: &str) -> Option<ContractSpec> {
    CHANGE_CONTRACTS
        .iter()
        .copied()
        .find(|contract| contract.name == name)
}

pub fn bash_contract(name: &str) -> Option<ContractSpec> {
    BASH_CONTRACTS
        .iter()
        .copied()
        .find(|contract| contract.name == name)
}

pub fn retrieval_operation_names() -> Vec<&'static str> {
    RETRIEVAL_CONTRACTS
        .iter()
        .map(|contract| contract.name)
        .collect()
}

pub fn change_kind_names() -> Vec<&'static str> {
    CHANGE_CONTRACTS
        .iter()
        .map(|contract| contract.name)
        .collect()
}

pub fn validate_retrieval_operation(kind: &str, operation: &Map<String, Value>) -> AppResult<()> {
    let contract = retrieval_contract(kind).ok_or_else(|| {
        ErrorCode::UnsupportedRetrievalOperation.error(
            format!("unsupported retrieval operation '{kind}'"),
            json!({"operation": kind, "supported": retrieval_operation_names()}),
        )
    })?;
    validate_fields(
        contract,
        operation,
        ErrorCode::MissingOperationField,
        ErrorCode::UnknownOperationField,
        "retrieval operation",
    )?;

    for field in contract
        .required
        .iter()
        .copied()
        .filter(|field| *field != "operation")
    {
        require_non_empty_string(
            operation,
            field,
            ErrorCode::InvalidOperationField,
            "retrieval operation",
        )?;
    }
    if kind == "symbols_overview" {
        let has_path = operation
            .get("path")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.trim().is_empty());
        let has_paths = operation
            .get("paths")
            .and_then(Value::as_array)
            .is_some_and(|values| !values.is_empty());
        if !has_path && !has_paths {
            return Err(ErrorCode::MissingOperationField.error(
                "symbols_overview requires path or paths",
                json!({"operation": kind, "required_any": ["path", "paths"]}),
            ));
        }
    }
    if let Some(paths) = operation.get("paths") {
        let valid = paths
            .as_array()
            .is_some_and(|values| values.iter().all(Value::is_string));
        if !valid {
            return Err(ErrorCode::InvalidOperationField.error(
                "retrieval operation paths must be an array of strings",
                json!({"operation": kind, "field": "paths"}),
            ));
        }
    }
    if let Some(detail) = operation.get("response_detail") {
        let valid = detail
            .as_str()
            .is_some_and(|value| matches!(value, "compact" | "standard" | "debug"));
        if !valid {
            return Err(ErrorCode::InvalidOperationField.error(
                "response_detail must be compact, standard, or debug",
                json!({"operation": kind, "field": "response_detail"}),
            ));
        }
    }
    Ok(())
}

pub fn validate_change(change: &Value) -> AppResult<()> {
    let object = change.as_object().ok_or_else(|| {
        ErrorCode::InvalidChangeField.error(
            "transaction changes must be objects",
            json!({"field": "changes"}),
        )
    })?;
    let kind = object
        .get("kind")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            ErrorCode::MissingChangeField.error("change requires kind", json!({"field": "kind"}))
        })?;
    let contract = change_contract(kind).ok_or_else(|| {
        ErrorCode::UnsupportedChangeKind.error(
            format!("unsupported change kind '{kind}'"),
            json!({"kind": kind, "supported": change_kind_names()}),
        )
    })?;
    validate_fields(
        contract,
        object,
        ErrorCode::MissingChangeField,
        ErrorCode::UnknownChangeField,
        "change",
    )?;
    require_non_empty_string(object, "path", ErrorCode::InvalidChangeField, "change")?;
    if kind == "replace_range" {
        require_non_empty_string(object, "handle", ErrorCode::InvalidChangeField, "change")?;
    }
    if kind == "insert" {
        let valid = object
            .get("position")
            .and_then(Value::as_str)
            .is_some_and(|value| {
                matches!(value, "before" | "after" | "inside_start" | "inside_end")
            });
        if !valid {
            return Err(ErrorCode::InvalidChangeField.error(
                "insert position must be before, after, inside_start, or inside_end",
                json!({"kind": kind, "field": "position"}),
            ));
        }
    }
    Ok(())
}

pub fn normalize_bash_request(method: &str, input: &Value) -> AppResult<Value> {
    let contract = bash_contract(method).ok_or_else(|| {
        ErrorCode::InvalidBashRequest.error(
            format!("unsupported Bash method '{method}'"),
            json!({"method": method}),
        )
    })?;
    let object = input.as_object().ok_or_else(|| {
        ErrorCode::InvalidBashRequest.error(
            format!("{method} input must be an object"),
            json!({"method": method}),
        )
    })?;
    validate_fields(
        contract,
        object,
        ErrorCode::InvalidBashRequest,
        ErrorCode::InvalidBashRequest,
        method,
    )?;

    let required_string = if method == "bash" {
        "command"
    } else {
        "run_id"
    };
    require_non_empty_string(
        object,
        required_string,
        ErrorCode::InvalidBashRequest,
        method,
    )?;
    if let Some(stream) = object.get("stream") {
        let valid = stream
            .as_str()
            .is_some_and(|value| matches!(value, "combined" | "stdout" | "stderr"));
        if !valid {
            return Err(ErrorCode::InvalidBashRequest.error(
                "bash_output stream must be combined, stdout, or stderr",
                json!({"method": method, "field": "stream"}),
            ));
        }
    }
    if let Some(background) = object.get("background") {
        if !background.is_boolean() {
            return Err(ErrorCode::InvalidBashRequest.error(
                "bash background must be a boolean",
                json!({"method": method, "field": "background"}),
            ));
        }
    }
    if let Some(timeout) = object.get("timeout_ms") {
        if timeout.as_u64().is_none_or(|value| value == 0) {
            return Err(ErrorCode::InvalidBashRequest.error(
                "bash timeout_ms must be a positive integer",
                json!({"method": method, "field": "timeout_ms"}),
            ));
        }
    }

    let action = match method {
        "bash" => "start",
        "bash_status" => "status",
        "bash_output" => "output",
        "bash_cancel" => "cancel",
        _ => unreachable!("validated Bash contract"),
    };
    let mut normalized = object.clone();
    normalized.insert("action".to_owned(), Value::String(action.to_owned()));
    Ok(Value::Object(normalized))
}

pub fn public_contract_capabilities() -> Value {
    json!({
        "contract_version": 2,
        "retrieval": {
            "tool": "code_retrieve",
            "max_operations": MAX_RETRIEVAL_OPERATIONS,
            "operations": retrieval_operation_names(),
            "contracts": contract_table_json(RETRIEVAL_CONTRACTS),
            "read_targets": READ_TARGETS,
            "text_syntax": ["literal", "regex"],
            "reference_scopes": ["all", "production", "tests"],
            "reference_kinds": ["declaration", "call", "import", "type", "read", "write", "other"],
            "supports_qualified_symbols": true,
            "ambiguous_symbols_return_candidates": true,
            "supports_snapshot_precondition": true,
            "malformed_operations_return_item_errors": true
        },
        "editing": {
            "supports_preview": true,
            "supports_transaction": true,
            "supports_single_file_wrappers": true,
            "supports_handle_range_replace": true,
            "handle_edits_must_be_only_change_for_file": true,
            "full_line_replacements_preserve_terminal_line_ending": true,
            "atomic_file_replace": true,
            "atomic_multi_file_commit": false,
            "compensating_restore": "best_effort",
            "manual_recovery_possible": true,
            "validation_failures_preserve_edits": true,
            "validation_may_run_detached": true,
            "validation_statuses": ["passed", "failed", "pending", "unavailable"]
        },
        "change_contracts": contract_table_json(CHANGE_CONTRACTS),
        "bash_contracts": contract_table_json(BASH_CONTRACTS),
        "error_registry": error_registry_json(),
        "known_limitations": [
            "reference operations share one indexed fallback service; code_intelligence uses semantic locations when an LSP backend succeeds",
            "include_imports returns lexical import prelude only, not inferred dependency usage",
            "hosted connector lazy-loading behavior is outside the server-side MCP list_tools contract"
        ]
    })
}

fn contract_table_json(contracts: &[ContractSpec]) -> Value {
    let mut table = Map::new();
    for contract in contracts {
        table.insert(
            contract.name.to_owned(),
            json!({
                "required": contract.required,
                "optional": contract.optional,
                "allowed": contract.allowed_fields().collect::<Vec<_>>()
            }),
        );
    }
    Value::Object(table)
}

fn error_registry_json() -> Value {
    Value::Array(
        ErrorCode::ALL
            .iter()
            .map(|code| {
                let policy = code.policy();
                json!({
                    "code": policy.code,
                    "retryable": policy.retryable,
                    "retry_kind": policy.retry_kind,
                    "category": policy.category
                })
            })
            .collect(),
    )
}

fn validate_fields(
    contract: ContractSpec,
    object: &Map<String, Value>,
    missing_code: ErrorCode,
    unknown_code: ErrorCode,
    context: &str,
) -> AppResult<()> {
    let allowed = contract.allowed_fields().collect::<BTreeSet<_>>();
    let unknown = object
        .keys()
        .filter(|field| !allowed.contains(field.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    if !unknown.is_empty() {
        return Err(unknown_code.error(
            format!("{context} received unsupported fields"),
            json!({
                "contract": contract.name,
                "unknown": unknown,
                "allowed": allowed
            }),
        ));
    }
    let missing = contract
        .required
        .iter()
        .copied()
        .filter(|field| object.get(*field).is_none_or(Value::is_null))
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(missing_code.error(
            format!("{context} is missing required fields"),
            json!({
                "contract": contract.name,
                "missing": missing,
                "required": contract.required
            }),
        ));
    }
    Ok(())
}

fn require_non_empty_string(
    object: &Map<String, Value>,
    field: &str,
    code: ErrorCode,
    context: &str,
) -> AppResult<()> {
    let valid = object
        .get(field)
        .and_then(Value::as_str)
        .is_some_and(|value| !value.trim().is_empty());
    if valid {
        Ok(())
    } else {
        Err(code.error(
            format!("{context} field '{field}' must be a non-empty string"),
            json!({"field": field}),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_codes_are_unique() {
        let codes = ErrorCode::ALL
            .iter()
            .map(|code| code.policy().code)
            .collect::<BTreeSet<_>>();
        assert_eq!(codes.len(), ErrorCode::ALL.len());
    }

    #[test]
    fn contract_names_and_fields_are_unique() {
        for contracts in [RETRIEVAL_CONTRACTS, CHANGE_CONTRACTS, BASH_CONTRACTS] {
            let names = contracts
                .iter()
                .map(|contract| contract.name)
                .collect::<BTreeSet<_>>();
            assert_eq!(names.len(), contracts.len());
            for contract in contracts {
                let fields = contract.allowed_fields().collect::<Vec<_>>();
                assert_eq!(
                    fields.iter().copied().collect::<BTreeSet<_>>().len(),
                    fields.len()
                );
            }
        }
    }

    #[test]
    fn bash_normalization_rejects_spoofed_fields_and_adds_action() {
        let normalized = normalize_bash_request("bash", &json!({"command": "printf ok"}))
            .expect("valid Bash request");
        assert_eq!(normalized["action"], "start");
        let error = normalize_bash_request(
            "bash_status",
            &json!({"run_id": "run_1", "action": "cancel"}),
        )
        .unwrap_err();
        assert_eq!(error.0.code, "INVALID_BASH_REQUEST");
    }

    #[test]
    fn retrieval_contract_rejects_missing_and_irrelevant_fields() {
        let missing = json!({"operation": "find_symbol"});
        let error = validate_retrieval_operation(
            "find_symbol",
            missing.as_object().expect("operation object"),
        )
        .unwrap_err();
        assert_eq!(error.0.code, "MISSING_OPERATION_FIELD");
        assert_eq!(
            error.0.details.as_ref().unwrap()["retry_kind"],
            "fix_request"
        );

        let irrelevant = json!({
            "operation": "find_symbol",
            "symbol": "WorkspaceActor",
            "pattern": "WorkspaceActor"
        });
        let error = validate_retrieval_operation(
            "find_symbol",
            irrelevant.as_object().expect("operation object"),
        )
        .unwrap_err();
        assert_eq!(error.0.code, "UNKNOWN_OPERATION_FIELD");
        assert_eq!(
            error.0.details.as_ref().unwrap()["retry_kind"],
            "remove_field"
        );
    }

    #[test]
    fn change_contract_rejects_missing_and_unknown_fields() {
        let missing = json!({"kind": "rename", "path": "old.rs"});
        let error = validate_change(&missing).unwrap_err();
        assert_eq!(error.0.code, "MISSING_CHANGE_FIELD");

        let unknown = json!({
            "kind": "delete",
            "path": "old.rs",
            "content": "not allowed for delete"
        });
        let error = validate_change(&unknown).unwrap_err();
        assert_eq!(error.0.code, "UNKNOWN_CHANGE_FIELD");
    }

    #[test]
    fn generated_capability_tables_match_contracts() {
        let capabilities = public_contract_capabilities();
        assert_eq!(
            capabilities["retrieval"]["operations"],
            json!(retrieval_operation_names())
        );
        assert_eq!(
            capabilities["change_contracts"]["replace"]["required"],
            json!(["kind", "path", "old_text", "new_text"])
        );
        assert!(capabilities["editing"]
            .get("supports_rollback_on_failure")
            .is_none());
    }
}
