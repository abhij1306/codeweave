use super::util::stale_snapshot;
use super::WorkspaceActor;
use crate::index::{decode_handle, encode_handle, slice_lines, RangeHandle};
use crate::model::{required_str, usize_value, AppError, AppResult};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FetchContinuation {
    workspace_id: String,
    path: String,
    offset: usize,
    content_hash: String,
}

impl WorkspaceActor {
    pub fn code_fetch(&self, params: &Value) -> AppResult<Value> {
        self.reconcile_pending()?;
        if let Some(expected) = params.get("snapshot_id").and_then(Value::as_str) {
            let current = self.snapshot();
            if expected != current {
                return Err(stale_snapshot(expected, &current));
            }
        }
        let items = params
            .get("items")
            .and_then(Value::as_array)
            .ok_or_else(|| AppError::invalid("items must be an array"))?;
        let max_chars = usize_value(params, "max_chars", 30_000).min(200_000);
        let mut remaining = max_chars;
        let mut results = Vec::new();
        let mut errors = Vec::new();
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
        Ok(json!({
            "snapshot_id": self.snapshot(),
            "result_count": results.len(),
            "error_count": errors.len(),
            "partial_success": !results.is_empty() && !errors.is_empty(),
            "truncated": processed_items < items.len() || chars_truncated,
            "items_truncated": processed_items < items.len(),
            "chars_truncated": chars_truncated,
            "results": results,
            "errors": errors,
        }))
    }

    fn fetch_item(&self, item: &Value, remaining: usize) -> AppResult<Value> {
        let kind = required_str(item, "kind")?;
        let value = required_str(item, "value")?;
        match kind {
            "path" => self.fetch_path(
                value,
                item.get("start_line")
                    .and_then(Value::as_u64)
                    .map(|v| v as usize),
                item.get("end_line")
                    .and_then(Value::as_u64)
                    .map(|v| v as usize),
                0,
                remaining,
            ),
            "handle" => {
                let handle = decode_handle(value)?;
                if handle.workspace_id != self.id {
                    return Err(AppError::new(
                        "INVALID_HANDLE",
                        "Handle belongs to another workspace",
                    ));
                }
                let file = self
                    .index
                    .read()
                    .get(&handle.path)
                    .cloned()
                    .ok_or_else(|| AppError::new("STALE_HANDLE", "Handle path no longer exists"))?;
                if file.hash != handle.content_hash {
                    return Err(AppError::details(
                        "STALE_HANDLE",
                        "File changed after handle creation",
                        json!({"path": handle.path, "expected_hash": handle.content_hash, "actual_hash": file.hash}),
                    ));
                }
                self.fetch_path(
                    &handle.path,
                    Some(handle.start_line),
                    Some(handle.end_line),
                    0,
                    remaining,
                )
            }
            "symbol" => {
                let (path, symbol, _) =
                    self.index.read().find_symbol(None, value).ok_or_else(|| {
                        AppError::details(
                            "SYMBOL_NOT_FOUND",
                            "Symbol not found",
                            json!({"symbol": value}),
                        )
                    })?;
                self.fetch_path(
                    &path,
                    Some(symbol.start_line),
                    Some(symbol.end_line),
                    0,
                    remaining,
                )
            }
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
                let file = self
                    .index
                    .read()
                    .get(&continuation.path)
                    .cloned()
                    .ok_or_else(|| {
                        AppError::new("STALE_CONTINUATION", "Continuation path no longer exists")
                    })?;
                if file.hash != continuation.content_hash {
                    return Err(AppError::details(
                        "STALE_CONTINUATION",
                        "File changed after continuation creation",
                        json!({"path": continuation.path, "expected_hash": continuation.content_hash, "actual_hash": file.hash}),
                    ));
                }
                self.fetch_path(
                    &continuation.path,
                    None,
                    None,
                    continuation.offset,
                    remaining,
                )
            }
            _ => Err(AppError::details(
                "INVALID_FETCH_KIND",
                "Unknown fetch kind",
                json!({"kind": kind}),
            )),
        }
    }

    fn fetch_path(
        &self,
        path: &str,
        start: Option<usize>,
        end: Option<usize>,
        offset: usize,
        limit: usize,
    ) -> AppResult<Value> {
        let index = self.index.read();
        let file = index.get(path).ok_or_else(|| {
            AppError::details(
                "PATH_NOT_INDEXED",
                "File is not indexed",
                json!({"path": path}),
            )
        })?;
        let (content, start_line, end_line) = if start.is_some() || end.is_some() {
            let start_line = start.unwrap_or(1);
            let end_line = end.unwrap_or_else(|| file.content.lines().count());
            (
                slice_lines(&file.content, start_line, end_line),
                start_line,
                end_line,
            )
        } else {
            (file.content.clone(), 1, file.content.lines().count().max(1))
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
        let base = json!({"path": file.path, "hash": file.hash, "start_line": start_line, "end_line": end_line, "handle": handle});
        let continuation = |next_offset| {
            encode_fetch_continuation(&FetchContinuation {
                workspace_id: self.id.clone(),
                path: file.path.clone(),
                offset: next_offset,
                content_hash: file.hash.clone(),
            })
            .ok()
        };
        Ok(bounded_content(
            base,
            &content,
            offset,
            limit,
            Some(&continuation),
        ))
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
        .map_err(|e| AppError::new("INVALID_CONTINUATION", e.to_string()))?;
    Ok(serde_json::from_slice(&bytes)?)
}
