use crate::error::{AppError, AppResult, ErrorCode};
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

pub fn retrieval_contract(name: &str) -> Option<ContractSpec> {
    RETRIEVAL_CONTRACTS
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

pub fn validate_retrieval_operation(kind: &str, operation: &Map<String, Value>) -> AppResult<()> {
    let contract = retrieval_contract(kind).ok_or_else(|| {
        AppError::with_details(
            ErrorCode::UnsupportedRetrievalOperation,
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
            return Err(AppError::with_details(
                ErrorCode::MissingOperationField,
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
            return Err(AppError::with_details(
                ErrorCode::InvalidOperationField,
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
            return Err(AppError::with_details(
                ErrorCode::InvalidOperationField,
                "response_detail must be compact, standard, or debug",
                json!({"operation": kind, "field": "response_detail"}),
            ));
        }
    }
    Ok(())
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
        return Err(AppError::with_details(
            unknown_code,
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
        return Err(AppError::with_details(
            missing_code,
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
        Err(AppError::with_details(
            code,
            format!("{context} field '{field}' must be a non-empty string"),
            json!({"field": field}),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contract_names_and_fields_are_unique() {
        let names = RETRIEVAL_CONTRACTS
            .iter()
            .map(|contract| contract.name)
            .collect::<BTreeSet<_>>();
        assert_eq!(names.len(), RETRIEVAL_CONTRACTS.len());
        for contract in RETRIEVAL_CONTRACTS {
            let fields = contract.allowed_fields().collect::<Vec<_>>();
            assert_eq!(
                fields.iter().copied().collect::<BTreeSet<_>>().len(),
                fields.len()
            );
        }
    }

    #[test]
    fn retrieval_contract_rejects_missing_and_irrelevant_fields() {
        let missing = json!({"operation": "find_symbol"});
        let error = validate_retrieval_operation(
            "find_symbol",
            missing.as_object().expect("operation object"),
        )
        .unwrap_err();
        assert_eq!(error.code(), ErrorCode::MissingOperationField);
        assert_eq!(error.details().unwrap()["retry_kind"], "fix_request");

        let irrelevant = json!({
            "operation": "find_symbol",
            "symbol": "Workspace",
            "pattern": "Workspace"
        });
        let error = validate_retrieval_operation(
            "find_symbol",
            irrelevant.as_object().expect("operation object"),
        )
        .unwrap_err();
        assert_eq!(error.code(), ErrorCode::UnknownOperationField);
        assert_eq!(error.details().unwrap()["retry_kind"], "remove_field");
    }
}
