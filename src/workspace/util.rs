use crate::index::ignored_workspace_path;
use crate::model::{AppError, AppResult};
use serde_json::{json, Value};
use std::collections::HashSet;

pub(super) fn line_range_bytes(
    content: &str,
    start_line: usize,
    end_line: usize,
) -> AppResult<(usize, usize)> {
    if start_line > end_line {
        return Err(AppError::details(
            "INVALID_HANDLE_RANGE",
            "Handle start_line must be less than or equal to end_line",
            json!({"start_line": start_line, "end_line": end_line}),
        ));
    }
    let start = line_offset(content, start_line).min(content.len());
    let end = line_offset(content, end_line.saturating_add(1)).min(content.len());
    Ok((start, end))
}

pub(super) fn line_offset(content: &str, line: usize) -> usize {
    if line <= 1 {
        return 0;
    }
    let mut current = 1;
    for (index, byte) in content.bytes().enumerate() {
        if byte == b'\n' {
            current += 1;
            if current == line {
                return index + 1;
            }
        }
    }
    content.len()
}

pub(super) fn stale_snapshot(expected: &str, actual: &str) -> AppError {
    stale_snapshot_for_paths(expected, actual, &[])
}

pub(super) fn stale_snapshot_for_paths(expected: &str, actual: &str, paths: &[String]) -> AppError {
    AppError::details(
        "STALE_SNAPSHOT",
        "Workspace changed after the requested snapshot",
        json!({
            "expected_snapshot": expected,
            "actual_snapshot": actual,
            "paths_requiring_refetch": paths,
            "retryable": true,
            "suggested_action": "Fetch only the affected handles/files again; edits with current expected_hash or handle preconditions can be retried without reopening the workspace."
        }),
    )
}

pub(super) fn changes_without_independent_preconditions(changes: &[Value]) -> Vec<String> {
    changes
        .iter()
        .filter_map(|change| {
            let kind = change
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let path = change
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or("<unknown>");
            let new_create = kind == "create"
                && !change
                    .get("overwrite")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
            let has_precondition = change
                .get("expected_hash")
                .and_then(Value::as_str)
                .is_some()
                || change.get("handle").and_then(Value::as_str).is_some();
            (!new_create && !has_precondition).then(|| path.to_owned())
        })
        .collect()
}

pub(super) const MAX_OBSERVED_CHANGED_PATHS: usize = 30;

pub(super) fn summarize_changed_paths(paths: HashSet<String>) -> (Vec<String>, usize, bool) {
    let mut output: Vec<_> = paths
        .into_iter()
        .filter(|path| !ignored_workspace_path(path))
        .collect();
    output.sort();
    let total = output.len();
    output.truncate(MAX_OBSERVED_CHANGED_PATHS);
    (output, total, total > MAX_OBSERVED_CHANGED_PATHS)
}
