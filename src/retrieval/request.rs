//! Parsing `code_retrieve` operations into typed requests, once.

use crate::contracts::{self, READ_TARGETS};
use crate::error::{AppError, AppResult, ErrorCode};
use crate::index::{FilenameQuery, SymbolQuery, TextQuery, TextSyntax};
use crate::reference_service::{ReferenceQuery, ReferenceScope};
use crate::symbols::ReferenceKind;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Map, Value};

/// Results returned when an operation does not set `max_results`.
pub const DEFAULT_MAX_RESULTS: usize = 20;
/// Upper bound on `max_results` regardless of configuration.
pub const PROTOCOL_MAX_RESULTS: usize = 200;
pub const DEFAULT_CONTEXT_LINES: usize = 2;
pub const MAX_CONTEXT_LINES: usize = 20;
pub const DEFAULT_READ_MAX_CHARS: usize = 30_000;
pub const MAX_READ_MAX_CHARS: usize = 200_000;
/// Context lines an exact symbol read may add around the symbol.
pub const MAX_SURROUNDING_LINES: usize = 200;
/// The largest integer a JSON number carries exactly.
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

// The published shape of one retrieval operation: every field any operation
// accepts. `RETRIEVAL_CONTRACTS` narrows it per operation before parsing.
// Enum-like strings keep their per-field validation (and error codes) in the
// parser, so the schema lists their values with `extend`. Plain comments only:
// schemars publishes doc comments as schema descriptions.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RetrievalOperationInput {
    #[schemars(length(min = 1, max = 80))]
    pub id: Option<String>,
    #[schemars(extend("enum" = contracts::retrieval_operation_names()))]
    pub operation: String,
    #[schemars(
        length(min = 1),
        description = "Filename substring or glob for find_file."
    )]
    pub name: Option<String>,
    #[schemars(
        length(min = 1),
        description = "Symbol selector for find_symbol or find_references."
    )]
    pub symbol: Option<String>,
    #[schemars(
        length(min = 1),
        description = "Literal text or regular expression for search_text."
    )]
    pub pattern: Option<String>,
    #[schemars(extend("enum" = TextSyntax::ALL.map(TextSyntax::as_str), "default" = "literal"))]
    pub syntax: Option<String>,
    #[schemars(
        extend("enum" = READ_TARGETS),
        description = "Exact target kind for read."
    )]
    pub target: Option<String>,
    #[schemars(length(min = 1), description = "Exact target value for read.")]
    pub value: Option<String>,
    #[schemars(
        description = "Single path for symbols_overview or optional symbol owner path for read."
    )]
    pub path: Option<String>,
    #[schemars(description = "Strict workspace-relative search scope.")]
    pub paths: Option<Vec<String>>,
    #[schemars(range(min = 1, max = MAX_SAFE_INTEGER))]
    pub max_results: Option<usize>,
    #[schemars(
        range(min = 0, max = MAX_CONTEXT_LINES),
        description = "Search-result context lines."
    )]
    pub context_lines: Option<usize>,
    pub case_sensitive: Option<bool>,
    #[schemars(extend(
        "enum" = ReferenceScope::ALL.map(ReferenceScope::as_str),
        "default" = "all"
    ))]
    pub reference_scope: Option<String>,
    #[schemars(extend("items" = {
        "type": "string",
        "enum": ReferenceKind::ALL.map(ReferenceKind::as_str)
    }))]
    pub reference_kinds: Option<Vec<String>>,
    pub definition_path: Option<String>,
    #[schemars(range(min = 1))]
    pub definition_line: Option<usize>,
    #[schemars(range(min = 1, max = MAX_SAFE_INTEGER))]
    pub start_line: Option<usize>,
    #[schemars(range(min = 1, max = MAX_SAFE_INTEGER))]
    pub end_line: Option<usize>,
    #[schemars(
        range(min = 0, max = MAX_SURROUNDING_LINES),
        description = "Additional lines around an exact symbol read."
    )]
    pub surrounding_lines: Option<usize>,
    pub include_imports: Option<bool>,
    #[schemars(range(min = 1, max = MAX_READ_MAX_CHARS))]
    pub max_chars: Option<usize>,
    #[schemars(extend(
        "enum" = ["compact", "standard", "debug"],
        "default" = "standard"
    ))]
    pub response_detail: Option<String>,
}

/// A validated search operation.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchRequest {
    pub target: SearchTarget,
    /// Strict workspace-relative scope from `path` and `paths`.
    pub paths: Vec<String>,
    /// Requested result count, before protocol and configured limits.
    pub max_results: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SearchTarget {
    /// `search_text`
    Text(TextQuery),
    /// `find_file`
    Filename(FilenameQuery),
    /// `find_symbol`
    Symbol(SymbolQuery),
    /// `find_references`
    References(ReferenceQuery),
    /// `symbols_overview`: the outline of every file in `paths`.
    Outline,
    /// `repo_map`
    RepoMap,
}

/// What an exact `read` returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadTarget {
    /// A file, optionally narrowed to a line range.
    Path {
        path: String,
        start_line: Option<usize>,
        end_line: Option<usize>,
    },
    /// The range bound to a `code_retrieve` read handle.
    Handle(String),
    /// One symbol declaration, optionally qualified by its file.
    Symbol {
        name: String,
        path: Option<String>,
        context_lines: usize,
        include_imports: bool,
    },
    /// File metadata without content.
    Metadata(String),
    /// A Bash run's status (`bash:` prefix optional).
    BashStatus(String),
    /// A Bash run's retained log (`bash-log:` prefix optional).
    BashLog(String),
    /// The next page of an earlier read.
    Continuation(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadOperation {
    pub target: ReadTarget,
    pub max_chars: usize,
    pub compact: bool,
}

#[derive(Debug, Clone)]
pub enum PreparedRetrievalOperation {
    Search(SearchRequest),
    Read(ReadOperation),
}

/// Validate one `code_retrieve` operation against its contract and parse it.
pub fn prepare_retrieval_operation(
    kind: &str,
    operation: &Map<String, Value>,
) -> AppResult<PreparedRetrievalOperation> {
    contracts::validate_retrieval_operation(kind, operation)?;
    parse_operation(kind, typed_operation(kind, operation)?)
}

/// Deserialize the operation; a field of the wrong type is an error, never a
/// default. JSON `null` counts as omitted.
fn typed_operation(
    kind: &str,
    operation: &Map<String, Value>,
) -> AppResult<RetrievalOperationInput> {
    let present = operation
        .iter()
        .filter(|(_, value)| !value.is_null())
        .map(|(field, value)| (field.clone(), value.clone()))
        .collect::<Map<_, _>>();
    serde_path_to_error::deserialize(Value::Object(present)).map_err(|error| {
        let field = error.path().to_string();
        AppError::with_details(
            ErrorCode::InvalidOperationField,
            format!(
                "retrieval operation field '{field}' is invalid: {}",
                error.inner()
            ),
            json!({"operation": kind, "field": field}),
        )
    })
}

fn parse_operation(
    kind: &str,
    input: RetrievalOperationInput,
) -> AppResult<PreparedRetrievalOperation> {
    let paths = input.scope_paths();
    let max_results = input.max_results.unwrap_or(DEFAULT_MAX_RESULTS);
    let context_lines = input
        .context_lines
        .unwrap_or(DEFAULT_CONTEXT_LINES)
        .min(MAX_CONTEXT_LINES);
    let case_sensitive = input.case_sensitive.unwrap_or(false);
    // `response_detail` is accepted by search operations without being used.
    let target = match kind {
        "find_file" => SearchTarget::Filename(FilenameQuery {
            name: required("name", input.name)?,
            case_sensitive,
        }),
        "find_symbol" => SearchTarget::Symbol(SymbolQuery {
            name: required("symbol", input.symbol)?,
        }),
        "search_text" => {
            let pattern = required("pattern", input.pattern)?;
            let syntax = match input.syntax.as_deref() {
                None => TextSyntax::Literal,
                Some(value) => TextSyntax::parse(value).ok_or_else(|| {
                    AppError::with_details(
                        ErrorCode::InvalidOperationField,
                        "search_text syntax must be literal or regex",
                        json!({"operation": kind, "field": "syntax"}),
                    )
                })?,
            };
            SearchTarget::Text(TextQuery {
                pattern,
                syntax,
                case_sensitive,
                context_lines,
            })
        }
        // References match exact identifiers, so case folding does not apply.
        "find_references" => SearchTarget::References(reference_query(input, context_lines)?),
        "symbols_overview" => {
            if paths.is_empty() {
                return Err(AppError::invalid("symbols_overview requires path or paths"));
            }
            SearchTarget::Outline
        }
        "repo_map" => SearchTarget::RepoMap,
        "read" => return read_operation(input).map(PreparedRetrievalOperation::Read),
        unsupported => {
            return Err(AppError::with_details(
                ErrorCode::UnsupportedRetrievalOperation,
                format!("unsupported retrieval operation '{unsupported}'"),
                json!({
                    "operation": unsupported,
                    "supported": contracts::retrieval_operation_names()
                }),
            ))
        }
    };
    Ok(PreparedRetrievalOperation::Search(SearchRequest {
        target,
        paths,
        max_results,
    }))
}

fn reference_query(
    input: RetrievalOperationInput,
    context_lines: usize,
) -> AppResult<ReferenceQuery> {
    let symbol = required("symbol", input.symbol)?;
    let scope = match input.reference_scope.as_deref() {
        None => ReferenceScope::All,
        Some(value) => ReferenceScope::parse(value).ok_or_else(|| {
            AppError::invalid("reference_scope must be all, production, or tests")
        })?,
    };
    let kinds = input
        .reference_kinds
        .unwrap_or_default()
        .iter()
        .map(|kind| {
            ReferenceKind::parse(kind)
                .ok_or_else(|| AppError::invalid("reference_kinds contains an unsupported kind"))
        })
        .collect::<AppResult<Vec<_>>>()?;
    Ok(ReferenceQuery {
        symbol,
        scope,
        kinds,
        definition_path: input.definition_path,
        definition_line: input.definition_line,
        context_lines,
    })
}

fn read_operation(input: RetrievalOperationInput) -> AppResult<ReadOperation> {
    let target = required("target", input.target)?;
    let value = required("value", input.value)?;
    let target = match target.as_str() {
        "path" => ReadTarget::Path {
            path: value,
            start_line: input.start_line,
            end_line: input.end_line,
        },
        "handle" => ReadTarget::Handle(value),
        "symbol" => ReadTarget::Symbol {
            name: value,
            path: input.path,
            context_lines: input
                .surrounding_lines
                .unwrap_or(0)
                .min(MAX_SURROUNDING_LINES),
            include_imports: input.include_imports.unwrap_or(false),
        },
        "metadata" => ReadTarget::Metadata(value),
        "bash_status" => ReadTarget::BashStatus(value),
        "bash_log" => ReadTarget::BashLog(value),
        "continuation" => ReadTarget::Continuation(value),
        unsupported => {
            return Err(AppError::with_details(
                ErrorCode::UnsupportedReadTarget,
                format!("unsupported read target '{unsupported}'"),
                json!({"target": unsupported, "supported": READ_TARGETS}),
            ))
        }
    };
    Ok(ReadOperation {
        target,
        max_chars: input
            .max_chars
            .unwrap_or(DEFAULT_READ_MAX_CHARS)
            .min(MAX_READ_MAX_CHARS),
        compact: input.response_detail.as_deref() == Some("compact"),
    })
}

/// A required string field that must not be blank.
fn required(field: &'static str, value: Option<String>) -> AppResult<String> {
    value
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            AppError::with_details(
                ErrorCode::InvalidOperationField,
                format!("retrieval operation field '{field}' must be a non-empty string"),
                json!({"field": field}),
            )
        })
}

impl RetrievalOperationInput {
    /// `paths`, plus `path` when it is not already listed.
    fn scope_paths(&self) -> Vec<String> {
        let mut paths = self.paths.clone().unwrap_or_default();
        if let Some(path) = &self.path {
            if !paths.contains(path) {
                paths.push(path.clone());
            }
        }
        paths
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::RETRIEVAL_CONTRACTS;
    use crate::requests::input_schema;
    use std::collections::BTreeSet;

    fn sample_value(field: &str) -> Value {
        match field {
            "operation" | "name" | "symbol" | "pattern" | "value" => json!("x"),
            "id" => json!("op"),
            "syntax" => json!("regex"),
            "target" => json!("path"),
            "path" | "definition_path" => json!("src/a.rs"),
            "paths" => json!(["src"]),
            "response_detail" => json!("compact"),
            "reference_scope" => json!("tests"),
            "reference_kinds" => json!(["call"]),
            "case_sensitive" | "include_imports" => json!(true),
            "max_results" | "context_lines" | "definition_line" | "start_line" | "end_line"
            | "surrounding_lines" | "max_chars" => json!(3),
            other => panic!("add a sample value for retrieval field '{other}'"),
        }
    }

    #[test]
    fn every_operation_parses_with_all_of_its_contract_fields() {
        for contract in RETRIEVAL_CONTRACTS {
            let mut operation: Map<String, Value> = contract
                .allowed_fields()
                .map(|field| (field.to_owned(), sample_value(field)))
                .collect();
            operation.insert("operation".to_owned(), json!(contract.name));

            prepare_retrieval_operation(contract.name, &operation)
                .unwrap_or_else(|error| panic!("{}: {error}", contract.name));
        }
    }

    #[test]
    fn published_operation_fields_are_the_union_of_the_operation_contracts() {
        let schema = input_schema::<RetrievalOperationInput>();
        let published = schema["properties"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        let contracted = RETRIEVAL_CONTRACTS
            .iter()
            .flat_map(|contract| contract.allowed_fields())
            .collect::<BTreeSet<_>>();

        assert_eq!(published, contracted);
    }

    fn prepare(operation: Value) -> AppResult<PreparedRetrievalOperation> {
        let kind = operation["operation"].as_str().unwrap().to_owned();
        prepare_retrieval_operation(&kind, operation.as_object().unwrap())
    }

    fn search_request(operation: Value) -> SearchRequest {
        match prepare(operation).unwrap() {
            PreparedRetrievalOperation::Search(request) => request,
            PreparedRetrievalOperation::Read(_) => panic!("expected a search operation"),
        }
    }

    #[test]
    fn references_parse_into_typed_scope_kinds_and_limits() {
        let request = search_request(json!({
            "operation": "find_references",
            "symbol": "run",
            "path": "src",
            "reference_scope": "production",
            "reference_kinds": ["call", "import"],
            "definition_line": 4,
            "context_lines": 99
        }));

        assert_eq!(
            request,
            SearchRequest {
                target: SearchTarget::References(ReferenceQuery {
                    symbol: "run".to_owned(),
                    scope: ReferenceScope::Production,
                    kinds: vec![ReferenceKind::Call, ReferenceKind::Import],
                    definition_path: None,
                    definition_line: Some(4),
                    context_lines: 20,
                }),
                paths: vec!["src".to_owned()],
                max_results: 20,
            }
        );
    }

    #[test]
    fn search_text_defaults_to_case_insensitive_literal() {
        let request = search_request(json!({"operation": "search_text", "pattern": "needle"}));

        assert_eq!(
            request.target,
            SearchTarget::Text(TextQuery {
                pattern: "needle".to_owned(),
                syntax: TextSyntax::Literal,
                case_sensitive: false,
                context_lines: 2,
            })
        );
    }

    #[test]
    fn invalid_reference_values_keep_their_error_codes_and_messages() {
        for (operation, message) in [
            (
                json!({"operation": "find_references", "symbol": "x", "reference_scope": "docs"}),
                "reference_scope must be all, production, or tests",
            ),
            (
                json!({"operation": "find_references", "symbol": "x", "reference_kinds": ["call", "alias"]}),
                "reference_kinds contains an unsupported kind",
            ),
        ] {
            let error = prepare(operation).unwrap_err();
            assert_eq!(error.code(), ErrorCode::InvalidArgument);
            assert_eq!(error.message(), message);
        }
    }

    #[test]
    fn a_field_of_the_wrong_type_is_an_operation_error_not_a_default() {
        let error = prepare(json!({
            "operation": "search_text",
            "pattern": "needle",
            "max_results": "5"
        }))
        .unwrap_err();

        assert_eq!(error.code(), ErrorCode::InvalidOperationField);
        assert_eq!(error.details().unwrap()["field"], "max_results");
    }

    #[test]
    fn read_clamps_max_chars_and_maps_surrounding_lines() {
        let PreparedRetrievalOperation::Read(read) = prepare(json!({
            "operation": "read",
            "target": "symbol",
            "value": "run",
            "surrounding_lines": 4,
            "max_chars": 900_000,
            "response_detail": "compact"
        }))
        .unwrap() else {
            panic!("expected a read operation")
        };

        assert_eq!(
            read,
            ReadOperation {
                target: ReadTarget::Symbol {
                    name: "run".to_owned(),
                    path: None,
                    context_lines: 4,
                    include_imports: false,
                },
                max_chars: 200_000,
                compact: true,
            }
        );
    }
}
