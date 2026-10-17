//! Typed `code_intelligence` request: the per-operation form of the tool's
//! `CodeIntelligenceRequest`, built once at the boundary.
use crate::model::{AppError, AppResult};
use crate::requests::intelligence::{CodeIntelligenceRequest, IntelligenceOperationName};
use crate::security::validate_relative;

/// A public position: one-based line, zero-based UTF-16 column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Position {
    pub(crate) line: usize,
    pub(crate) column: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Operation {
    Definition {
        position: Position,
    },
    References {
        position: Position,
        max_results: usize,
    },
    Diagnostics {
        max_results: usize,
    },
    RenamePreview {
        position: Position,
        new_name: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntelligenceRequest {
    /// Validated workspace-relative path with `/` separators.
    pub(crate) path: String,
    pub(crate) operation: Operation,
}

impl TryFrom<CodeIntelligenceRequest> for IntelligenceRequest {
    type Error = AppError;

    fn try_from(request: CodeIntelligenceRequest) -> AppResult<Self> {
        let path = request
            .path
            .as_deref()
            .ok_or_else(|| AppError::invalid("code_intelligence requires path"))?;
        let path = validate_relative(path)?
            .to_string_lossy()
            .replace('\\', "/");
        let position = || {
            let line = request.line.ok_or_else(|| {
                AppError::invalid("This code_intelligence operation requires line")
            })?;
            Ok::<_, AppError>(Position {
                line: line as usize,
                column: request.column.unwrap_or(0) as usize,
            })
        };
        let operation = match request.operation {
            IntelligenceOperationName::Definition => Operation::Definition {
                position: position()?,
            },
            IntelligenceOperationName::References => Operation::References {
                position: position()?,
                max_results: request.max_results,
            },
            IntelligenceOperationName::Diagnostics => Operation::Diagnostics {
                max_results: request.max_results,
            },
            IntelligenceOperationName::RenamePreview => Operation::RenamePreview {
                position: position()?,
                new_name: request
                    .new_name
                    .clone()
                    .ok_or_else(|| AppError::invalid("rename_preview requires new_name"))?,
            },
        };
        Ok(Self { path, operation })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::requests::{Contract, Rejection};
    use serde_json::{json, Value};

    fn parse(params: Value) -> AppResult<IntelligenceRequest> {
        Contract::of::<CodeIntelligenceRequest>("code_intelligence")
            .parse::<CodeIntelligenceRequest>(params, Rejection::Argument)?
            .try_into()
    }

    #[test]
    fn requests_parse_into_typed_operations_with_defaults() {
        let references =
            parse(json!({"operation":"references","path":"src\\a.rs","line":3})).unwrap();
        let rename = parse(json!({
            "operation":"rename_preview","path":"a.rs","line":1,"column":4,
            "new_name":"next","max_results":null
        }))
        .unwrap();

        assert_eq!(references.path, "src/a.rs");
        assert_eq!(
            references.operation,
            Operation::References {
                position: Position { line: 3, column: 0 },
                max_results: 20
            }
        );
        assert_eq!(
            rename.operation,
            Operation::RenamePreview {
                position: Position { line: 1, column: 4 },
                new_name: "next".to_owned()
            }
        );
    }

    #[test]
    fn invalid_numbers_are_errors_not_defaults() {
        let rejected = [
            json!({"operation":"definition","path":"a.rs","line":1,"column":"3"}),
            json!({"operation":"definition","path":"a.rs","line":1,"column":-1}),
            json!({"operation":"definition","path":"a.rs","line":0}),
            json!({"operation":"definition","path":"a.rs","line":1.5}),
            json!({"operation":"references","path":"a.rs","line":1,"max_results":0}),
            json!({"operation":"diagnostics","path":"a.rs","max_results":201}),
            json!({"operation":"diagnostics","path":"a.rs","max_results":"20"}),
            json!({"operation":"definition","path":"a.rs"}),
            json!({"operation":"definition","path":"../a.rs","line":1}),
            json!({"operation":"hover","path":"a.rs","line":1}),
        ]
        .map(|params| parse(params).unwrap_err().0.code);

        assert_eq!(
            rejected,
            [
                "INVALID_ARGUMENT",
                "INVALID_ARGUMENT",
                "INVALID_ARGUMENT",
                "INVALID_ARGUMENT",
                "INVALID_ARGUMENT",
                "INVALID_ARGUMENT",
                "INVALID_ARGUMENT",
                "INVALID_ARGUMENT",
                "OUTSIDE_ROOT",
                "INVALID_ARGUMENT",
            ]
        );
    }

    #[test]
    fn diagnostics_and_references_share_the_same_result_limit() {
        let diagnostics =
            parse(json!({"operation":"diagnostics","path":"a.py","max_results":200})).unwrap();
        let references =
            parse(json!({"operation":"references","path":"a.py","line":1,"max_results":200}))
                .unwrap();
        assert_eq!(
            diagnostics.operation,
            Operation::Diagnostics { max_results: 200 }
        );
        assert_eq!(
            references.operation,
            Operation::References {
                position: Position { line: 1, column: 0 },
                max_results: 200
            }
        );
    }
}
