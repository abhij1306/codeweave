//! Parsing `code_retrieve` operations into typed requests, once.

use crate::contracts::{self, ErrorCode, READ_TARGETS};
use crate::index::{FilenameQuery, SymbolQuery, TextQuery, TextSyntax};
use crate::model::{AppError, AppResult};
use crate::reference_service::{ReferenceQuery, ReferenceScope};
use crate::symbols::ReferenceKind;
use serde_json::{json, Map, Value};
use std::cell::RefCell;
use std::collections::BTreeSet;

/// Results returned when an operation does not set `max_results`.
pub const DEFAULT_MAX_RESULTS: usize = 20;
/// Upper bound on `max_results` regardless of configuration.
pub const PROTOCOL_MAX_RESULTS: usize = 200;
pub const DEFAULT_CONTEXT_LINES: usize = 2;
pub const MAX_CONTEXT_LINES: usize = 20;
pub const DEFAULT_READ_MAX_CHARS: usize = 30_000;
pub const MAX_READ_MAX_CHARS: usize = 200_000;

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

#[derive(Debug, Clone)]
pub struct ReadOperation {
    pub item: Value,
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
    parse_operation(&OperationFields::new(kind, operation))
}

fn parse_operation(fields: &OperationFields<'_>) -> AppResult<PreparedRetrievalOperation> {
    // `operation` and `id` belong to the batch envelope; search operations
    // accept `response_detail` without using it.
    fields.accept_unused(&["operation", "id"]);
    let search = |target| {
        Ok(PreparedRetrievalOperation::Search(SearchRequest {
            target,
            paths: fields.paths(),
            max_results: fields.usize("max_results").unwrap_or(DEFAULT_MAX_RESULTS),
        }))
    };
    match fields.kind {
        "find_file" => {
            fields.accept_unused(&["context_lines", "response_detail"]);
            search(SearchTarget::Filename(FilenameQuery {
                name: fields.required_str("name")?.to_owned(),
                case_sensitive: fields.case_sensitive(),
            }))
        }
        "find_symbol" => {
            fields.accept_unused(&["context_lines", "case_sensitive", "response_detail"]);
            search(SearchTarget::Symbol(SymbolQuery {
                name: fields.required_str("symbol")?.to_owned(),
            }))
        }
        "search_text" => {
            fields.accept_unused(&["response_detail"]);
            let pattern = fields.required_str("pattern")?.to_owned();
            let syntax = match fields.str("syntax") {
                None => TextSyntax::Literal,
                Some(value) => TextSyntax::parse(value).ok_or_else(|| {
                    ErrorCode::InvalidOperationField.error(
                        "search_text syntax must be literal or regex",
                        json!({"operation": fields.kind, "field": "syntax"}),
                    )
                })?,
            };
            search(SearchTarget::Text(TextQuery {
                pattern,
                syntax,
                case_sensitive: fields.case_sensitive(),
                context_lines: fields.context_lines(),
            }))
        }
        "find_references" => {
            // References match exact identifiers, so case folding does not apply.
            fields.accept_unused(&["case_sensitive", "response_detail"]);
            search(SearchTarget::References(reference_query(fields)?))
        }
        "symbols_overview" => {
            fields.accept_unused(&["response_detail"]);
            if fields.paths().is_empty() {
                return Err(AppError::invalid("symbols_overview requires path or paths"));
            }
            search(SearchTarget::Outline)
        }
        "repo_map" => {
            fields.accept_unused(&["response_detail"]);
            search(SearchTarget::RepoMap)
        }
        "read" => read_operation(fields).map(PreparedRetrievalOperation::Read),
        unsupported => Err(ErrorCode::UnsupportedRetrievalOperation.error(
            format!("unsupported retrieval operation '{unsupported}'"),
            json!({
                "operation": unsupported,
                "supported": contracts::retrieval_operation_names()
            }),
        )),
    }
}

fn reference_query(fields: &OperationFields<'_>) -> AppResult<ReferenceQuery> {
    let symbol = fields.required_str("symbol")?.to_owned();
    let scope = match fields.str("reference_scope") {
        None => ReferenceScope::All,
        Some(value) => ReferenceScope::parse(value).ok_or_else(|| {
            AppError::invalid("reference_scope must be all, production, or tests")
        })?,
    };
    let kinds = fields
        .string_list("reference_kinds")
        .into_iter()
        .map(|kind| {
            ReferenceKind::parse(kind)
                .ok_or_else(|| AppError::invalid("reference_kinds contains an unsupported kind"))
        })
        .collect::<AppResult<Vec<_>>>()?;
    Ok(ReferenceQuery {
        symbol,
        scope,
        kinds,
        definition_path: fields.str("definition_path").map(str::to_owned),
        definition_line: fields.usize("definition_line"),
        context_lines: fields.context_lines(),
    })
}

fn read_operation(fields: &OperationFields<'_>) -> AppResult<ReadOperation> {
    let target = fields.required_str("target")?;
    if !READ_TARGETS.contains(&target) {
        return Err(AppError::details(
            "UNSUPPORTED_READ_TARGET",
            format!("unsupported read target '{target}'"),
            json!({"target": target, "supported": READ_TARGETS}),
        ));
    }
    let value = fields.required_str("value")?;
    let mut item = json!({"kind": target, "value": value});
    for field in ["path", "start_line", "end_line", "include_imports"] {
        if let Some(value) = fields.get(field) {
            item[field] = value.clone();
        }
    }
    if let Some(value) = fields.get("surrounding_lines") {
        item["context_lines"] = value.clone();
    }
    Ok(ReadOperation {
        item,
        max_chars: fields
            .usize("max_chars")
            .unwrap_or(DEFAULT_READ_MAX_CHARS)
            .min(MAX_READ_MAX_CHARS),
        compact: fields.str("response_detail") == Some("compact"),
    })
}

/// Field access for one operation. Every field the parser reads, or accepts
/// without using, is recorded so tests can check the parser against the
/// published contract.
struct OperationFields<'a> {
    kind: &'a str,
    object: &'a Map<String, Value>,
    consumed: RefCell<BTreeSet<&'static str>>,
}

impl<'a> OperationFields<'a> {
    fn new(kind: &'a str, object: &'a Map<String, Value>) -> Self {
        Self {
            kind,
            object,
            consumed: RefCell::new(BTreeSet::new()),
        }
    }

    fn get(&self, field: &'static str) -> Option<&'a Value> {
        self.consumed.borrow_mut().insert(field);
        self.object.get(field)
    }

    fn accept_unused(&self, fields: &[&'static str]) {
        self.consumed.borrow_mut().extend(fields);
    }

    /// A string field; a value of another type counts as absent.
    fn str(&self, field: &'static str) -> Option<&'a str> {
        self.get(field).and_then(Value::as_str)
    }

    /// A non-negative integer field; a value of another type counts as absent.
    fn usize(&self, field: &'static str) -> Option<usize> {
        self.get(field)
            .and_then(Value::as_u64)
            .map(|value| usize::try_from(value).unwrap_or(usize::MAX))
    }

    fn required_str(&self, field: &'static str) -> AppResult<&'a str> {
        self.str(field)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                ErrorCode::InvalidOperationField.error(
                    format!("retrieval operation field '{field}' must be a non-empty string"),
                    json!({"field": field}),
                )
            })
    }

    /// The string items of an array field; other items are ignored.
    fn string_list(&self, field: &'static str) -> Vec<&'a str> {
        self.get(field)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect()
    }

    /// `paths`, plus `path` when it is not already listed.
    fn paths(&self) -> Vec<String> {
        let mut paths: Vec<String> = self
            .string_list("paths")
            .into_iter()
            .map(str::to_owned)
            .collect();
        if let Some(path) = self.str("path") {
            if !paths.iter().any(|candidate| candidate == path) {
                paths.push(path.to_owned());
            }
        }
        paths
    }

    fn case_sensitive(&self) -> bool {
        self.get("case_sensitive")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    fn context_lines(&self) -> usize {
        self.usize("context_lines")
            .unwrap_or(DEFAULT_CONTEXT_LINES)
            .min(MAX_CONTEXT_LINES)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::RETRIEVAL_CONTRACTS;

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
    fn parser_consumes_exactly_the_contract_fields() {
        for contract in RETRIEVAL_CONTRACTS {
            let mut operation: Map<String, Value> = contract
                .allowed_fields()
                .map(|field| (field.to_owned(), sample_value(field)))
                .collect();
            operation.insert("operation".to_owned(), json!(contract.name));
            let fields = OperationFields::new(contract.name, &operation);

            parse_operation(&fields).unwrap();

            assert_eq!(
                fields.consumed.into_inner(),
                contract.allowed_fields().collect::<BTreeSet<_>>(),
                "operation: {}",
                contract.name
            );
        }
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
            assert_eq!(error.0.code, "INVALID_ARGUMENT");
            assert_eq!(error.0.message, message);
        }
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
            read.item,
            json!({"kind": "symbol", "value": "run", "context_lines": 4})
        );
        assert_eq!(read.max_chars, 200_000);
        assert!(read.compact);
    }
}
