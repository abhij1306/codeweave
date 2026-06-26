use super::util::stale_snapshot;
use super::WorkspaceActor;
use crate::index::{decode_handle, encode_handle, slice_lines, FileEntry, RangeHandle};
use crate::model::{required_str, usize_value, AppError, AppResult};
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
    missing_code: &'static str,
    missing_message: &'static str,
    stale_code: &'static str,
    stale_message: &'static str,
}

impl WorkspaceActor {
    pub fn code_fetch(&self, params: &Value) -> AppResult<Value> {
        let started = std::time::Instant::now();
        let reconcile_pending = self.read_reconcile_pending();
        if let Some(expected) = params.get("snapshot_id").and_then(Value::as_str) {
            let current = self.snapshot();
            if expected != current {
                return Err(stale_snapshot(expected, &current));
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
        let max_chars = usize_value(params, "max_chars", 30_000).min(200_000);
        let mut remaining = max_chars;
        let mut results = Vec::new();
        let mut errors = Vec::new();
        let fetch_started = std::time::Instant::now();
        for (index, item) in items.iter().enumerate() {
            if remaining == 0 {
                break;
            }
            match self.fetch_item(item, remaining) {
                Ok(result) => {
                    remaining = remaining.saturating_sub(
                        result
                            .get("content")
                            .and_then(Value::as_str)
                            .map(str::len)
                            .unwrap_or(0),
                    );
                    results.push(result);
                }
                Err(error) => errors.push(json!({
                    "index": index,
                    "kind": item.get("kind"),
                    "value": item.get("value"),
                    "error": error.0,
                })),
            }
        }
        let processed_items = results.len() + errors.len();
        let chars_truncated = results.iter().any(|result| {
            result
                .get("continuation")
                .is_some_and(|value| !value.is_null())
                || result
                    .get("content")
                    .and_then(Value::as_str)
                    .zip(result.get("total_chars").and_then(Value::as_u64))
                    .is_some_and(|(content, total)| content.len() < total as usize)
        });
        let mut result = json!({
            "snapshot_id": self.snapshot(),
            "result_count": results.len(),
            "error_count": errors.len(),
            "partial_success": !results.is_empty() && !errors.is_empty(),
            "truncated": processed_items < items.len() || chars_truncated,
            "items_truncated": processed_items < items.len(),
            "chars_truncated": chars_truncated,
            "results": results,
            "errors": errors,
        });
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

    fn fetch_item(&self, item: &Value, remaining: usize) -> AppResult<Value> {
        let kind = required_str(item, "kind")?;
        let value = required_str(item, "value")?;
        match kind {
            "path" => {
                let start = item
                    .get("start_line")
                    .and_then(Value::as_u64)
                    .map(|value| value as usize);
                let end = item
                    .get("end_line")
                    .and_then(Value::as_u64)
                    .map(|value| value as usize);
                self.fetch_indexed_path(value, scope_from_bounds(start, end), 0, remaining, None)
            }
            "handle" => {
                let handle = decode_handle(value)?;
                if handle.workspace_id != self.id {
                    return Err(AppError::new(
                        "INVALID_HANDLE",
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
                        missing_code: "STALE_HANDLE",
                        missing_message: "Handle path no longer exists",
                        stale_code: "STALE_HANDLE",
                        stale_message: "File changed after handle creation",
                    }),
                )
            }
            "symbol" => self.fetch_symbol(value, remaining),
            "task_log" => {
                let task_id = value.strip_prefix("task-log:").unwrap_or(value);
                let content = self.tasks.read_log(task_id)?;
                Ok(bounded_content(
                    json!({"kind": "task_log", "task_id": task_id}),
                    &content,
                    0,
                    remaining,
                    None,
                ))
            }
            "continuation" => {
                let continuation = decode_fetch_continuation(value)?;
                if continuation.workspace_id != self.id {
                    return Err(AppError::new(
                        "INVALID_CONTINUATION",
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
                        missing_code: "STALE_CONTINUATION",
                        missing_message: "Continuation path no longer exists",
                        stale_code: "STALE_CONTINUATION",
                        stale_message: "File changed after continuation creation",
                    }),
                )
            }
            _ => Err(AppError::details(
                "INVALID_FETCH_KIND",
                "Unknown fetch kind",
                json!({"kind": kind}),
            )),
        }
    }

    fn fetch_symbol(&self, symbol_name: &str, limit: usize) -> AppResult<Value> {
        let index = self.index.read();
        let (path, symbol, _) = index.find_symbol(None, symbol_name).ok_or_else(|| {
            AppError::details(
                "SYMBOL_NOT_FOUND",
                "Symbol not found",
                json!({"symbol": symbol_name}),
            )
        })?;
        let file = index.get(&path).ok_or_else(|| {
            AppError::details(
                "PATH_NOT_INDEXED",
                "Symbol path is not indexed",
                json!({"path": path, "symbol": symbol_name}),
            )
        })?;
        self.build_fetch_response(
            file,
            FetchScope::Lines {
                start_line: symbol.start_line,
                end_line: symbol.end_line,
            },
            0,
            limit,
        )
    }

    fn fetch_indexed_path(
        &self,
        path: &str,
        scope: FetchScope,
        offset: usize,
        limit: usize,
        precondition: Option<FetchPrecondition<'_>>,
    ) -> AppResult<Value> {
        let index = self.index.read();
        let file = index.get(path).ok_or_else(|| {
            precondition.map_or_else(
                || {
                    AppError::details(
                        "PATH_NOT_INDEXED",
                        "File is not indexed",
                        json!({"path": path}),
                    )
                },
                |condition| AppError::new(condition.missing_code, condition.missing_message),
            )
        })?;
        if let Some(condition) = precondition {
            if file.hash != condition.expected_hash {
                return Err(AppError::details(
                    condition.stale_code,
                    condition.stale_message,
                    json!({
                        "path": file.path,
                        "expected_hash": condition.expected_hash,
                        "actual_hash": file.hash
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
                Cow::Borrowed(file.content.as_str()),
                1,
                file.content.lines().count().max(1),
            ),
            FetchScope::Lines {
                start_line,
                end_line,
            } => {
                let line_count = file.content.lines().count().max(1);
                let start_line = start_line.max(1).min(line_count);
                let end_line = end_line.max(start_line).min(line_count);
                (
                    Cow::Owned(slice_lines(&file.content, start_line, end_line)),
                    start_line,
                    end_line,
                )
            }
        };
        let handle = encode_handle(&RangeHandle {
            version: 1,
            workspace_id: self.id.clone(),
            snapshot_id: self.snapshot(),
            path: file.path.clone(),
            start_line,
            end_line,
            content_hash: file.hash.clone(),
            symbol: None,
        })?;
        let base = json!({
            "path": file.path,
            "hash": file.hash,
            "start_line": start_line,
            "end_line": end_line,
            "handle": handle
        });
        let continuation_scope = scope;
        let continuation = |next_offset| {
            encode_fetch_continuation(&FetchContinuation {
                workspace_id: self.id.clone(),
                path: file.path.clone(),
                offset: next_offset,
                content_hash: file.hash.clone(),
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

fn bounded_content(
    mut base: Value,
    content: &str,
    offset: usize,
    limit: usize,
    continuation: Option<&dyn Fn(usize) -> Option<String>>,
) -> Value {
    let start = nearest_char_boundary(content, offset.min(content.len()));
    let end = nearest_char_boundary(content, (start + limit).min(content.len()));
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

fn nearest_char_boundary(content: &str, mut index: usize) -> usize {
    while index > 0 && !content.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn encode_fetch_continuation(value: &FetchContinuation) -> AppResult<String> {
    Ok(format!(
        "fetch:v1:{}",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(value)?)
    ))
}

fn decode_fetch_continuation(value: &str) -> AppResult<FetchContinuation> {
    let payload = value
        .strip_prefix("fetch:v1:")
        .ok_or_else(|| AppError::new("INVALID_CONTINUATION", "Unsupported continuation"))?;
    let bytes = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|error| AppError::new("INVALID_CONTINUATION", error.to_string()))?;
    Ok(serde_json::from_slice(&bytes)?)
}
