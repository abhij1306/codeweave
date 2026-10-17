use super::util::line_ending_label;
use super::Workspace;
use crate::error::{AppError, AppResult, ErrorCode};
use crate::index::{
    decode_handle, encode_handle, slice_lines, FileEntry, RangeHandle, SymbolDefinition,
};
use crate::retrieval::ReadTarget;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::borrow::Cow;

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum FetchScope {
    #[default]
    Full,
    Lines {
        start_line: usize,
        end_line: usize,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FetchContinuation {
    workspace_id: String,
    path: String,
    offset: usize,
    content_hash: String,
    #[serde(default)]
    scope: FetchScope,
}

#[derive(Clone, Copy)]
struct FetchPrecondition<'a> {
    expected_hash: &'a str,
    missing_code: ErrorCode,
    missing_message: &'static str,
    stale_code: ErrorCode,
    stale_message: &'static str,
}

impl Workspace {
    #[cfg(test)]
    pub(super) fn read_targets(&self, params: &Value) -> AppResult<Value> {
        self.read_targets_batch(params)
    }

    #[cfg(test)]
    pub(super) fn read_targets_batch(&self, params: &Value) -> AppResult<Value> {
        let started = std::time::Instant::now();
        let reconcile_pending = self.state.ledger().needs_reconcile();
        let snapshot = self.state.read().stamp().snapshot_id;
        if let Some(expected) = params.get("snapshot_id").and_then(Value::as_str) {
            if expected != snapshot {
                return Err(super::util::stale_snapshot(expected, &snapshot));
            }
        }
        let items: Vec<Value> = if let Some(path) = params.get("path").and_then(Value::as_str) {
            vec![json!({
                "kind": "path",
                "value": path,
                "start_line": params.get("start_line"),
                "end_line": params.get("end_line")
            })]
        } else {
            params
                .get("items")
                .and_then(Value::as_array)
                .cloned()
                .ok_or_else(|| AppError::invalid("Provide 'path' or an 'items' array"))?
        };
        let max_chars = params
            .get("max_chars")
            .and_then(Value::as_u64)
            .map_or(crate::retrieval::DEFAULT_READ_MAX_CHARS, saturating_usize)
            .min(crate::retrieval::MAX_READ_MAX_CHARS);
        let response_detail = params
            .get("response_detail")
            .and_then(Value::as_str)
            .unwrap_or("standard");
        let mut remaining = max_chars;
        let mut results = Vec::new();
        let mut errors = Vec::new();
        let fetch_started = std::time::Instant::now();
        for (index, item) in items.iter().enumerate() {
            if remaining == 0 {
                break;
            }
            match read_target_from_item(item)
                .and_then(|target| self.read_target(&target, remaining))
            {
                Ok(result) => {
                    remaining = remaining.saturating_sub(result_text_len(&result));
                    results.push(result);
                }
                Err(error) => errors.push(json!({
                    "index": index,
                    "kind": item.get("kind"),
                    "value": item.get("value"),
                    "error": error,
                })),
            }
        }
        let processed_items = results.len() + errors.len();
        let chars_truncated = results.iter().any(|result| {
            result
                .get("continuation")
                .is_some_and(|value| !value.is_null())
                || result
                    .get("output_truncated")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                || result
                    .get("content")
                    .and_then(Value::as_str)
                    .zip(result.get("total_chars").and_then(Value::as_u64))
                    .is_some_and(|(content, total)| content.len() < saturating_usize(total))
        });
        let mut result = json!({
            "snapshot_id": snapshot,
            "result_count": results.len(),
            "error_count": errors.len(),
            "partial_success": !results.is_empty() && !errors.is_empty(),
            "truncated": processed_items < items.len() || chars_truncated,
            "items_truncated": processed_items < items.len(),
            "chars_truncated": chars_truncated,
            "results": results,
            "errors": errors,
        });
        if response_detail == "compact" {
            if let Some(object) = result.as_object_mut() {
                object.insert(
                    "results".to_owned(),
                    Value::Array(results.iter().map(compact_fetch_result).collect::<Vec<_>>()),
                );
                object.insert("response_detail".to_owned(), json!("compact"));
            }
        } else if response_detail == "debug" {
            if let Some(object) = result.as_object_mut() {
                object.insert("response_detail".to_owned(), json!("debug"));
            }
        }
        super::add_reconcile_pending(&mut result, reconcile_pending);
        super::add_phase_metrics(
            &mut result,
            &[
                ("fetch_items", fetch_started.elapsed().as_millis()),
                ("total_local", started.elapsed().as_millis()),
            ],
        );
        Ok(result)
    }

    pub(super) fn read_target(&self, target: &ReadTarget, remaining: usize) -> AppResult<Value> {
        match target {
            ReadTarget::Path {
                path,
                start_line,
                end_line,
            } => self.fetch_indexed_path(
                path,
                scope_from_bounds(*start_line, *end_line),
                0,
                remaining,
                None,
            ),
            ReadTarget::Handle(value) => {
                let handle = decode_handle(value)?;
                if handle.workspace_id != self.id {
                    return Err(AppError::new(
                        ErrorCode::InvalidHandle,
                        "Handle belongs to another workspace",
                    ));
                }
                self.fetch_indexed_path(
                    &handle.path,
                    FetchScope::Lines {
                        start_line: handle.start_line,
                        end_line: handle.end_line,
                    },
                    0,
                    remaining,
                    Some(FetchPrecondition {
                        expected_hash: &handle.content_hash,
                        missing_code: ErrorCode::StaleHandle,
                        missing_message: "Handle path no longer exists",
                        stale_code: ErrorCode::StaleHandle,
                        stale_message: "File changed after handle creation",
                    }),
                )
            }
            ReadTarget::Symbol {
                name,
                path,
                context_lines,
                include_imports,
            } => self.fetch_symbol(
                name,
                path.as_deref(),
                remaining,
                *context_lines,
                *include_imports,
            ),
            ReadTarget::Metadata(path) => self.fetch_metadata(path),
            ReadTarget::BashStatus(value) => {
                let run_id = value.strip_prefix("bash:").unwrap_or(value.as_str());
                self.bash.status_with_limit(run_id, remaining)
            }
            ReadTarget::BashLog(value) => {
                let run_id = value.strip_prefix("bash-log:").unwrap_or(value.as_str());
                let content = self.bash.read_output(run_id)?;
                Ok(bounded_content(
                    json!({"kind": "bash_log", "run_id": run_id}),
                    &content,
                    0,
                    remaining,
                    None,
                ))
            }
            ReadTarget::Continuation(value) => {
                let continuation = decode_fetch_continuation(value)?;
                if continuation.workspace_id != self.id {
                    return Err(AppError::new(
                        ErrorCode::InvalidContinuation,
                        "Continuation belongs to another workspace",
                    ));
                }
                self.fetch_indexed_path(
                    &continuation.path,
                    continuation.scope,
                    continuation.offset,
                    remaining,
                    Some(FetchPrecondition {
                        expected_hash: &continuation.content_hash,
                        missing_code: ErrorCode::StaleContinuation,
                        missing_message: "Continuation path no longer exists",
                        stale_code: ErrorCode::StaleContinuation,
                        stale_message: "File changed after continuation creation",
                    }),
                )
            }
        }
    }

    fn fetch_symbol(
        &self,
        symbol_name: &str,
        requested_path: Option<&str>,
        limit: usize,
        context_lines: usize,
        include_imports: bool,
    ) -> AppResult<Value> {
        let view = self.state.read();
        let index = view.index();
        let (qualified_path, symbol_name) = if requested_path.is_none() {
            symbol_name
                .split_once("::")
                .filter(|(path, name)| !name.is_empty() && index.get(path).is_some())
                .map_or((None, symbol_name), |(path, name)| (Some(path), name))
        } else {
            (None, symbol_name)
        };
        let requested_path = requested_path.or(qualified_path);
        let candidates = index.find_symbols(requested_path, symbol_name);
        if candidates.is_empty() {
            return Err(AppError::with_details(
                ErrorCode::SymbolNotFound,
                "Symbol not found",
                json!({"symbol": symbol_name, "path": requested_path}),
            ));
        }
        if candidates.len() > 1 {
            let candidates = candidates
                .into_iter()
                .take(20)
                .map(|SymbolDefinition { path, symbol, hash }| {
                    let handle = encode_handle(&RangeHandle {
                        workspace_id: self.id.clone(),
                        path: path.clone(),
                        start_line: symbol.start_line,
                        end_line: symbol.end_line,
                        content_hash: hash,
                    })
                    .ok();
                    json!({
                        "path": path,
                        "symbol": symbol.name,
                        "kind": symbol.kind,
                        "start_line": symbol.start_line,
                        "end_line": symbol.end_line,
                        "signature": symbol.signature,
                        "handle": handle,
                    })
                })
                .collect::<Vec<_>>();
            return Err(AppError::with_details(
                ErrorCode::AmbiguousSymbol,
                "Symbol name matches multiple declarations; specify path or fetch a candidate handle",
                json!({"symbol": symbol_name, "candidates": candidates}),
            ));
        }
        let SymbolDefinition { path, symbol, .. } =
            candidates.into_iter().next().expect("checked non-empty");
        let file = index.get(&path).ok_or_else(|| {
            AppError::with_details(
                ErrorCode::PathNotIndexed,
                "Symbol path is not indexed",
                json!({"path": path, "symbol": symbol_name}),
            )
        })?;
        let line_count = file.content().lines().count().max(1);
        let start_line = symbol.start_line.saturating_sub(context_lines).max(1);
        let end_line = symbol
            .end_line
            .saturating_add(context_lines)
            .min(line_count);
        let mut result = self.build_fetch_response(
            file,
            FetchScope::Lines {
                start_line,
                end_line,
            },
            0,
            limit,
        )?;
        if let Some(object) = result.as_object_mut() {
            object.insert("symbol".to_owned(), json!(symbol));
            if include_imports {
                object.insert(
                    "imports".to_owned(),
                    json!(lexical_import_prelude(file, start_line)),
                );
            }
        }
        Ok(result)
    }

    fn fetch_metadata(&self, path: &str) -> AppResult<Value> {
        let view = self.state.read();
        let index = view.index();
        let file = index.get(path).ok_or_else(|| {
            AppError::with_details(
                ErrorCode::PathNotIndexed,
                "File is not indexed",
                json!({"path": path}),
            )
        })?;
        Ok(json!({
            "kind": "metadata",
            "path": file.path(),
            "hash": file.hash(),
            "size": file.size(),
            "language": file.language(),
            "document_type": file.document_type(),
            "line_count": file.content().lines().count().max(1),
            "modified_ns": file.modified_ns()
        }))
    }

    fn fetch_indexed_path(
        &self,
        path: &str,
        scope: FetchScope,
        offset: usize,
        limit: usize,
        precondition: Option<FetchPrecondition<'_>>,
    ) -> AppResult<Value> {
        let view = self.state.read();
        let index = view.index();
        let file = index.get(path).ok_or_else(|| {
            precondition.map_or_else(
                || {
                    AppError::with_details(
                        ErrorCode::PathNotIndexed,
                        "File is not indexed",
                        json!({"path": path}),
                    )
                },
                |condition| AppError::new(condition.missing_code, condition.missing_message),
            )
        })?;
        if let Some(condition) = precondition {
            if file.hash() != condition.expected_hash {
                return Err(AppError::with_details(
                    condition.stale_code,
                    condition.stale_message,
                    json!({
                        "path": file.path(),
                        "expected_hash": condition.expected_hash,
                        "actual_hash": file.hash()
                    }),
                ));
            }
        }
        self.build_fetch_response(file, scope, offset, limit)
    }

    fn build_fetch_response(
        &self,
        file: &FileEntry,
        scope: FetchScope,
        offset: usize,
        limit: usize,
    ) -> AppResult<Value> {
        let (content, start_line, end_line): (Cow<'_, str>, usize, usize) = match scope {
            FetchScope::Full => (
                Cow::Borrowed(file.content()),
                1,
                file.content().lines().count().max(1),
            ),
            FetchScope::Lines {
                start_line,
                end_line,
            } => {
                let line_count = file.content().lines().count().max(1);
                let start_line = start_line.max(1).min(line_count);
                let end_line = end_line.max(start_line).min(line_count);
                (
                    Cow::Owned(slice_lines(file.content(), start_line, end_line)),
                    start_line,
                    end_line,
                )
            }
        };
        let handle = encode_handle(&RangeHandle {
            workspace_id: self.id.clone(),
            path: file.path().to_owned(),
            start_line,
            end_line,
            content_hash: file.hash().to_owned(),
        })?;
        let base = json!({
            "path": file.path(),
            "hash": file.hash(),
            "line_ending": line_ending_label(file.content()),
            "start_line": start_line,
            "end_line": end_line,
            "handle": handle
        });
        let continuation_scope = scope;
        let continuation = |next_offset| {
            encode_fetch_continuation(&FetchContinuation {
                workspace_id: self.id.clone(),
                path: file.path().to_owned(),
                offset: next_offset,
                content_hash: file.hash().to_owned(),
                scope: continuation_scope,
            })
            .ok()
        };
        Ok(bounded_content(
            base,
            content.as_ref(),
            offset,
            limit,
            Some(&continuation),
        ))
    }
}

/// The typed read target of a test `items` entry (`kind`, `value`, and the
/// optional `path`, `start_line`, `end_line`, `context_lines`,
/// `include_imports`).
#[cfg(test)]
fn read_target_from_item(item: &Value) -> AppResult<ReadTarget> {
    let text = |field: &str| item.get(field).and_then(Value::as_str).map(str::to_owned);
    let number = |field: &str| {
        item.get(field)
            .and_then(Value::as_u64)
            .map(saturating_usize)
    };
    let value = text("value").ok_or_else(|| AppError::invalid("Missing or invalid 'value'"))?;
    Ok(
        match text("kind")
            .ok_or_else(|| AppError::invalid("Missing or invalid 'kind'"))?
            .as_str()
        {
            "path" => ReadTarget::Path {
                path: value,
                start_line: number("start_line"),
                end_line: number("end_line"),
            },
            "handle" => ReadTarget::Handle(value),
            "symbol" => ReadTarget::Symbol {
                name: value,
                path: text("path"),
                context_lines: number("context_lines").unwrap_or(0).min(200),
                include_imports: item
                    .get("include_imports")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            },
            "metadata" => ReadTarget::Metadata(value),
            "bash_status" => ReadTarget::BashStatus(value),
            "bash_log" => ReadTarget::BashLog(value),
            "continuation" => ReadTarget::Continuation(value),
            kind => {
                return Err(AppError::with_details(
                    ErrorCode::InvalidFetchKind,
                    "Unknown fetch kind",
                    json!({"kind": kind}),
                ))
            }
        },
    )
}

#[cfg(test)]
fn result_text_len(result: &Value) -> usize {
    ["content", "output"]
        .into_iter()
        .filter_map(|field| result.get(field).and_then(Value::as_str))
        .map(str::len)
        .sum()
}

pub(super) fn compact_fetch_result(result: &Value) -> Value {
    let mut compact = serde_json::Map::new();
    for field in [
        "kind",
        "path",
        "hash",
        "content",
        "line_ending",
        "run_id",
        "status",
        "exit_code",
        "output",
        "output_truncated",
        "language",
        "document_type",
        "line_count",
        "size",
        "modified_ns",
        "imports",
    ] {
        if let Some(value) = result.get(field) {
            compact.insert(field.to_owned(), value.clone());
        }
    }
    Value::Object(compact)
}

fn lexical_import_prelude(file: &FileEntry, before_line: usize) -> Vec<Value> {
    file.content()
        .lines()
        .take(before_line.saturating_sub(1))
        .enumerate()
        .filter_map(|(index, line)| {
            let trimmed = line.trim_start();
            let is_import = trimmed.starts_with("use ")
                || trimmed.starts_with("pub use ")
                || trimmed.starts_with("extern crate ")
                || trimmed.starts_with("import ")
                || trimmed.starts_with("from ")
                || trimmed.starts_with("package ");
            is_import.then(|| {
                json!({
                    "line": index + 1,
                    "text": line
                })
            })
        })
        .collect()
}

fn scope_from_bounds(start: Option<usize>, end: Option<usize>) -> FetchScope {
    if start.is_none() && end.is_none() {
        FetchScope::Full
    } else {
        let start_line = start.unwrap_or(1).max(1);
        FetchScope::Lines {
            start_line,
            end_line: end.unwrap_or(usize::MAX).max(start_line),
        }
    }
}

/// A client-supplied count as `usize`, saturating where `usize` is narrower.
#[cfg(test)]
fn saturating_usize(value: u64) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

fn bounded_content(
    mut base: Value,
    content: &str,
    offset: usize,
    limit: usize,
    continuation: Option<&dyn Fn(usize) -> Option<String>>,
) -> Value {
    let start = content.floor_char_boundary(offset);
    let end = content.floor_char_boundary(start.saturating_add(limit));
    let next = if end < content.len() {
        continuation.and_then(|builder| builder(end))
    } else {
        None
    };
    if let Some(object) = base.as_object_mut() {
        object.insert(
            "content".to_owned(),
            Value::String(content[start..end].to_owned()),
        );
        object.insert("offset".to_owned(), json!(start));
        object.insert("total_chars".to_owned(), json!(content.len()));
        object.insert("continuation".to_owned(), json!(next));
    }
    base
}

fn encode_fetch_continuation(value: &FetchContinuation) -> AppResult<String> {
    Ok(format!(
        "fetch:{}",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(value)?)
    ))
}

fn decode_fetch_continuation(value: &str) -> AppResult<FetchContinuation> {
    let payload = value
        .strip_prefix("fetch:")
        .ok_or_else(|| AppError::new(ErrorCode::InvalidContinuation, "Unsupported continuation"))?;
    let bytes = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|error| AppError::new(ErrorCode::InvalidContinuation, error.to_string()))?;
    Ok(serde_json::from_slice(&bytes)?)
}
