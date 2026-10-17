//! Typed `code_intelligence` request: parsed and validated once at the boundary.
use crate::model::{AppError, AppResult};
use crate::security::validate_relative;
use serde_json::{json, Value};

pub(crate) const DEFAULT_MAX_RESULTS: usize = 20;
pub(crate) const MAX_RESULTS: usize = 200;

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
pub(crate) struct IntelligenceRequest {
    /// Validated workspace-relative path with `/` separators.
    pub(crate) path: String,
    pub(crate) operation: Operation,
}

impl IntelligenceRequest {
    pub(crate) fn parse(params: &Value) -> AppResult<Self> {
        let operation = string(params, "operation")?
            .ok_or_else(|| AppError::invalid("code_intelligence requires operation"))?;
        let path = string(params, "path")?
            .ok_or_else(|| AppError::invalid("code_intelligence requires path"))?;
        let path = validate_relative(path)?
            .to_string_lossy()
            .replace('\\', "/");
        let operation = match operation {
            "definition" => Operation::Definition {
                position: position(params)?,
            },
            "references" => Operation::References {
                position: position(params)?,
                max_results: max_results(params)?,
            },
            "diagnostics" => Operation::Diagnostics {
                max_results: max_results(params)?,
            },
            "rename_preview" => Operation::RenamePreview {
                position: position(params)?,
                new_name: string(params, "new_name")?
                    .ok_or_else(|| AppError::invalid("rename_preview requires new_name"))?
                    .to_owned(),
            },
            other => {
                return Err(AppError::details(
                    "INVALID_ARGUMENT",
                    "Unknown code_intelligence operation",
                    json!({"operation": other}),
                ))
            }
        };
        Ok(Self { path, operation })
    }
}

/// A field that is absent or JSON `null` is "not provided".
fn field<'a>(params: &'a Value, key: &str) -> Option<&'a Value> {
    params.get(key).filter(|value| !value.is_null())
}

fn string<'a>(params: &'a Value, key: &str) -> AppResult<Option<&'a str>> {
    field(params, key)
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| AppError::invalid(format!("{key} must be a string")))
        })
        .transpose()
}

fn integer(params: &Value, key: &str, min: u64, max: u64) -> AppResult<Option<usize>> {
    let Some(value) = field(params, key) else {
        return Ok(None);
    };
    let number = value
        .as_u64()
        .filter(|number| (min..=max).contains(number))
        .ok_or_else(|| {
            AppError::details(
                "INVALID_ARGUMENT",
                format!("{key} must be an integer from {min} to {max}"),
                json!({"field": key, "value": value}),
            )
        })?;
    usize::try_from(number)
        .map(Some)
        .map_err(|_| AppError::invalid(format!("{key} is too large")))
}

fn position(params: &Value) -> AppResult<Position> {
    let line = integer(params, "line", 1, u32::MAX.into())?
        .ok_or_else(|| AppError::invalid("This code_intelligence operation requires line"))?;
    let column = integer(params, "column", 0, u32::MAX.into())?.unwrap_or(0);
    Ok(Position { line, column })
}

fn max_results(params: &Value) -> AppResult<usize> {
    Ok(integer(params, "max_results", 1, MAX_RESULTS as u64)?.unwrap_or(DEFAULT_MAX_RESULTS))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(params: Value) -> AppResult<IntelligenceRequest> {
        IntelligenceRequest::parse(&params)
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
