use crate::index::ignored_workspace_path;
use crate::model::{AppError, AppResult};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

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

pub(super) fn char_boundary_at_or_before(value: &str, mut index: usize) -> usize {
    index = index.min(value.len());
    while index > 0 && !value.is_char_boundary(index) {
        index -= 1;
    }
    index
}

pub(super) fn line_ending_label(content: &str) -> &'static str {
    let crlf = content.matches("\r\n").count();
    let lf = content.bytes().filter(|byte| *byte == b'\n').count() - crlf;
    let cr = content.bytes().filter(|byte| *byte == b'\r').count() - crlf;
    match (crlf, lf, cr) {
        (0, 0, 0) => "none",
        (_, 0, 0) if crlf > 0 => "crlf",
        (0, _, 0) if lf > 0 => "lf",
        _ => "mixed",
    }
}

pub(super) fn normalize_line_endings_for_content(content: &str, text: &str) -> String {
    match line_ending_label(content) {
        "crlf" => normalize_line_endings(text, "\r\n"),
        "lf" => normalize_line_endings(text, "\n"),
        _ => text.to_owned(),
    }
}

fn normalize_line_endings(text: &str, replacement: &str) -> String {
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\n', replacement)
}

pub(super) fn preserve_terminal_line_ending(selected: &str, replacement: &str) -> String {
    if replacement.is_empty() || replacement.ends_with('\n') || !selected.ends_with('\n') {
        return replacement.to_owned();
    }
    let mut value = replacement.to_owned();
    if selected.ends_with("\r\n") {
        value.push_str("\r\n");
    } else {
        value.push('\n');
    }
    value
}

pub(super) fn matching_old_text(
    content: &str,
    selected: &str,
    old: &str,
    expected: usize,
) -> (String, usize) {
    let normalized = normalize_line_endings_for_content(content, old);
    if normalized != old {
        let normalized_count = selected.match_indices(&normalized).count();
        if normalized_count == expected {
            return (normalized, normalized_count);
        }
    }
    let count = selected.match_indices(old).count();
    (old.to_owned(), count)
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
pub(super) const MAX_CHANGED_PATH_GROUPS: usize = 20;

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub(super) struct ChangedPathGroup {
    pub path: String,
    pub count: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ChangedPathSummary {
    pub paths: Vec<String>,
    pub count: usize,
    pub truncated: bool,
    pub groups: Vec<ChangedPathGroup>,
}

pub(super) fn summarize_changed_paths(paths: HashSet<String>) -> ChangedPathSummary {
    let mut output: Vec<_> = paths
        .into_iter()
        .filter(|path| !ignored_workspace_path(path))
        .collect();
    output.sort();
    let total = output.len();
    let mut grouped = HashMap::<String, usize>::new();
    for path in &output {
        *grouped.entry(changed_path_group(path)).or_default() += 1;
    }
    let mut groups: Vec<_> = grouped
        .into_iter()
        .map(|(path, count)| ChangedPathGroup { path, count })
        .collect();
    groups.sort_by(|left, right| {
        right
            .count
            .cmp(&left.count)
            .then_with(|| left.path.cmp(&right.path))
    });
    if groups.len() > MAX_CHANGED_PATH_GROUPS {
        let other_index = MAX_CHANGED_PATH_GROUPS.saturating_sub(1);
        let other_count = groups[other_index..].iter().map(|group| group.count).sum();
        groups.truncate(other_index);
        groups.push(ChangedPathGroup {
            path: "(other)".to_owned(),
            count: other_count,
        });
    }
    output.truncate(MAX_OBSERVED_CHANGED_PATHS);
    ChangedPathSummary {
        paths: output,
        count: total,
        truncated: total > MAX_OBSERVED_CHANGED_PATHS,
        groups,
    }
}

fn changed_path_group(path: &str) -> String {
    let components: Vec<_> = path
        .replace('\\', "/")
        .split('/')
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect();
    match components.as_slice() {
        [] | [_] => "(root)".to_owned(),
        [directory, _] => directory.clone(),
        [first, second, ..] => format!("{first}/{second}"),
    }
}
