use super::io_helpers::{
    atomic_write, diff_stat, read_optional, remove_if_exists, render_diff, restore_one,
};
use super::journal::{
    open_journal, rotate_journal_now, trim_journal, MutationRecord, MAX_JOURNAL_BYTES,
};
use super::util::{
    changes_without_independent_preconditions, line_offset, line_range_bytes,
    normalize_line_endings_for_content, stale_snapshot_for_paths,
};
use super::validation::ValidationOutcome;
use super::WorkspaceActor;
use crate::index::{content_hash, decode_handle, CodeIndex};
use crate::model::{bool_value, required_str, string_list, usize_value, AppError, AppResult};
use crate::security::validate_relative;
use crate::symbols::{extract_symbols, parse_has_error};
use chrono::Utc;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::{atomic::Ordering, Arc};
use std::time::Instant;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub(super) struct PlannedFile {
    pub(super) path: String,
    pub(super) before: Option<String>,
    pub(super) after: Option<String>,
}

struct AppliedEdit {
    write_guard: tokio::sync::OwnedMutexGuard<()>,
    planned: Vec<PlannedFile>,
    request_id: String,
    apply_result: Vec<MutationRecord>,
    diff: String,
    snapshot_rebased_from: Option<String>,
    commit_ms: u128,
    /// Per-file syntax preflight outcome ("checked" | "skipped"); see D5.
    syntax_checks: Vec<Value>,
}

enum PreparedEdit {
    Preview(Value),
    Applied(AppliedEdit),
}

/// The diff-related fields returned by `code_edit`, shaped by `response_detail`.
/// `stat` is always present (cheap and useful); `diff` may be the full unified diff,
/// a size-capped prefix, or null when the caller asked for a compact response.
struct DiffView {
    diff: Value,
    stat: Value,
    truncated: bool,
    omitted: bool,
}

impl DiffView {
    /// Build the view for the successful/applied response paths. `compact` drops the
    /// unified diff and keeps only the per-file stat; `debug` returns it in full;
    /// `standard` (the default) caps it at `max_context_chars` to bound payload size.
    fn build(diff: &str, planned: &[PlannedFile], detail: &str, cap: usize) -> Self {
        let stat = json!(diff_stat(planned)
            .into_iter()
            .map(|(path, added, removed)| json!({
                "path": path,
                "added": added,
                "removed": removed,
            }))
            .collect::<Vec<_>>());
        match detail {
            "compact" => Self {
                diff: Value::Null,
                stat,
                truncated: false,
                omitted: true,
            },
            "debug" => Self {
                diff: json!(diff),
                stat,
                truncated: false,
                omitted: false,
            },
            _ if cap > 0 && diff.len() > cap => {
                // Cut on a line boundary so the prefix stays a readable diff.
                let safe_cap = char_boundary_at_or_before(diff, cap);
                let end = diff[..safe_cap]
                    .rfind('\n')
                    .map(|idx| idx + 1)
                    .unwrap_or(safe_cap);
                Self {
                    diff: json!(&diff[..end]),
                    stat,
                    truncated: true,
                    omitted: false,
                }
            }
            _ => Self {
                diff: json!(diff),
                stat,
                truncated: false,
                omitted: false,
            },
        }
    }
}

impl WorkspaceActor {
    pub async fn code_edit(self: &Arc<Self>, session_id: &str, params: &Value) -> AppResult<Value> {
        let validate = string_list(params, "validate");
        if !validate.is_empty() {
            self.bash.ensure_available()?;
        }
        let write_guard = self.write_lock.clone().lock_owned().await;
        let actor = Arc::clone(self);
        let params_owned = params.clone();
        let session_id_owned = session_id.to_owned();
        let prepare_started = Instant::now();
        let prepared = tokio::task::spawn_blocking(move || {
            actor.prepare_edit(&session_id_owned, &params_owned, write_guard)
        })
        .await
        .map_err(AppError::internal)??;
        let prepare_ms = prepare_started.elapsed().as_millis();

        let applied = match prepared {
            PreparedEdit::Preview(value) => return Ok(value),
            PreparedEdit::Applied(applied) => applied,
        };
        let AppliedEdit {
            write_guard,
            planned,
            request_id,
            apply_result,
            diff,
            snapshot_rebased_from,
            commit_ms,
            syntax_checks,
        } = applied;
        let mut write_guard = Some(write_guard);
        let rollback_on_failure = bool_value(params, "rollback_on_failure", true);

        // Shape the diff payload by response_detail before `planned` is moved into any
        // rollback closure below. `standard` (default) caps the unified diff so a large
        // edit cannot balloon the response; the per-file stat is always returned.
        let detail = params
            .get("response_detail")
            .and_then(Value::as_str)
            .unwrap_or("standard");
        let diff_view = DiffView::build(&diff, &planned, detail, self.policy.max_context_chars);
        let DiffView {
            diff: diff_value,
            stat: diff_stat_value,
            truncated: diff_truncated,
            omitted: diff_omitted,
        } = diff_view;

        let ValidationOutcome {
            validation,
            failed: validation_failed,
            pending_run_id: validation_pending,
            deferred_run_id: deferred_validation_pending,
            cancellation_error: validation_cancellation_error,
        } = self
            .run_edit_validation(session_id, &validate, rollback_on_failure)
            .await;

        if let Some(error) = validation_cancellation_error {
            drop(write_guard.take());
            self.reconcile_pending_async().await?;
            return Ok(json!({
                "applied": true,
                "rolled_back": false,
                "reason": "validation_cancellation_unconfirmed",
                "validation_cancellation_error": error,
                "guidance": "Validation cancellation could not be confirmed, so CodeWeave left the edit applied instead of risking rollback while the validator might still be running.",
                "snapshot_rebased_from": snapshot_rebased_from,
                "diff": diff_value,
                "diff_stat": diff_stat_value,
                "diff_truncated": diff_truncated,
                "diff_omitted": diff_omitted,
                "syntax_checks": syntax_checks,
                "validation": validation,
                "generation": self.generation(),
                "snapshot_id": self.snapshot(),
                "mutations": apply_result,
                "phase_ms": {
                    "prepare_and_commit": prepare_ms,
                    "commit": commit_ms
                }
            }));
        }

        if let Some(run_id) = validation_pending {
            debug_assert!(!rollback_on_failure);
            let mut validation_run_ids = vec![run_id.clone()];
            if let Some(deferred_run_id) = deferred_validation_pending.as_ref() {
                validation_run_ids.push(deferred_run_id.clone());
            }
            let guidance = if deferred_validation_pending.is_some() {
                "Validation is running detached because rollback_on_failure is false. Poll bash_status for every ID in validation_run_ids; validation_run_id is the leading validator and deferred_validation_run_id is queued behind it."
            } else {
                "Validation is running detached because rollback_on_failure is false. Poll bash_status with validation_run_id."
            };
            drop(write_guard.take());
            self.reconcile_pending_async().await?;
            let mut response = json!({
                "applied": true,
                "rolled_back": false,
                "validation_pending": true,
                "validation_run_id": run_id,
                "validation_run_ids": validation_run_ids,
                "guidance": guidance,
                "snapshot_rebased_from": snapshot_rebased_from,
                "diff": diff_value,
                "diff_stat": diff_stat_value,
                "diff_truncated": diff_truncated,
                "diff_omitted": diff_omitted,
                "syntax_checks": syntax_checks,
                "validation": validation,
                "generation": self.generation(),
                "snapshot_id": self.snapshot(),
                "mutations": apply_result,
                "phase_ms": {
                    "prepare_and_commit": prepare_ms,
                    "commit": commit_ms
                }
            });
            if let Some(deferred_run_id) = deferred_validation_pending {
                response["deferred_validation_run_id"] = Value::String(deferred_run_id);
            }
            return Ok(response);
        }

        if validation_failed && rollback_on_failure {
            let write_guard = write_guard
                .take()
                .expect("applied edit must retain its write guard");
            let actor = Arc::clone(self);
            let rollback_request_id = request_id.clone();
            let rollback_session_id = session_id.to_owned();
            let rollback_result = tokio::task::spawn_blocking(move || {
                let _write_guard = write_guard;
                let _reconcile_guard = actor.reconcile_lock.lock();
                actor.recheck_applied_state(&planned)?;
                actor.restore_plan(&rollback_session_id, &planned, &rollback_request_id)
            })
            .await
            .map_err(AppError::internal)?;
            if let Err(error) = rollback_result {
                return Ok(json!({
                    "applied": true,
                    "rolled_back": false,
                    "reason": "validation_failed_rollback_conflict",
                    "rollback_error": error.0,
                    "snapshot_rebased_from": snapshot_rebased_from,
                    "diff": diff_value,
                    "diff_stat": diff_stat_value,
                    "diff_truncated": diff_truncated,
                    "diff_omitted": diff_omitted,
                    "syntax_checks": syntax_checks,
                    "validation": validation,
                    "generation": self.generation(),
                    "snapshot_id": self.snapshot(),
                    "mutations": apply_result,
                    "phase_ms": {
                        "prepare_and_commit": prepare_ms,
                        "commit": commit_ms
                    }
                }));
            }
            return Ok(json!({
                "applied": false,
                "rolled_back": true,
                "reason": "validation_failed",
                "snapshot_rebased_from": snapshot_rebased_from,
                "diff": diff_value,
                "diff_stat": diff_stat_value,
                "diff_truncated": diff_truncated,
                "diff_omitted": diff_omitted,
                "syntax_checks": syntax_checks,
                "validation": validation,
                "generation": self.generation(),
                "snapshot_id": self.snapshot(),
                "mutations": apply_result,
                "phase_ms": {
                    "prepare_and_commit": prepare_ms,
                    "commit": commit_ms
                }
            }));
        }

        drop(write_guard.take());
        self.reconcile_pending_async().await?;
        Ok(json!({
            "applied": true,
            "rolled_back": false,
            "snapshot_rebased_from": snapshot_rebased_from,
            "diff": diff_value,
            "diff_stat": diff_stat_value,
            "diff_truncated": diff_truncated,
            "diff_omitted": diff_omitted,
            "syntax_checks": syntax_checks,
            "validation": validation,
            "generation": self.generation(),
            "snapshot_id": self.snapshot(),
            "mutations": apply_result,
            "phase_ms": {
                "prepare_and_commit": prepare_ms,
                "commit": commit_ms
            }
        }))
    }

    fn prepare_edit(
        &self,
        session_id: &str,
        params: &Value,
        write_guard: tokio::sync::OwnedMutexGuard<()>,
    ) -> AppResult<PreparedEdit> {
        self.reconcile_pending()?;
        let _reconcile_guard = self.reconcile_lock.lock();
        let current_snapshot = self.snapshot();
        let requested_snapshot = params
            .get("snapshot_id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let changes = params
            .get("changes")
            .and_then(Value::as_array)
            .ok_or_else(|| AppError::invalid("changes must be an array"))?;
        if changes.is_empty() {
            return Err(AppError::invalid("changes cannot be empty"));
        }
        let snapshot_matches = requested_snapshot
            .as_deref()
            .map(|expected| expected == current_snapshot)
            .unwrap_or(true);
        if !snapshot_matches {
            let unsafe_paths = changes_without_independent_preconditions(changes);
            if !unsafe_paths.is_empty() {
                return Err(stale_snapshot_for_paths(
                    requested_snapshot.as_deref().unwrap_or_default(),
                    &current_snapshot,
                    &unsafe_paths,
                ));
            }
        }
        let index = self.index.read();
        self.preflight_overlaps(changes, &index)?;
        let mut plan: HashMap<String, PlannedFile> = HashMap::new();
        for change in changes {
            self.plan_change(
                change,
                requested_snapshot.is_some() && snapshot_matches,
                &mut plan,
                &index,
            )?;
        }
        drop(index);
        let mut planned: Vec<PlannedFile> = plan.into_values().collect();
        planned.sort_by(|a, b| a.path.cmp(&b.path));
        // Per-file syntax preflight result. Tree-sitter validates languages with a
        // bundled grammar; for formats without one (YAML/TOML/Markdown/HTML/plain
        // text) `parse_has_error` returns None and the SYNTAX_ERROR gate cannot
        // fire — surface that as "skipped" so the bypass is visible instead of
        // silently passing. Deletions carry no content to check.
        let mut syntax_checks: Vec<Value> = Vec::new();
        for item in &planned {
            if let Some(after) = &item.after {
                let absolute = self.root.join(&item.path);
                match parse_has_error(&absolute, after) {
                    Some(true) => {
                        return Err(AppError::details(
                            "SYNTAX_ERROR",
                            "Tree-sitter found syntax errors in planned content",
                            json!({"path": item.path}),
                        ));
                    }
                    Some(false) => {
                        syntax_checks.push(json!({"path": item.path, "syntax_check": "checked"}))
                    }
                    None => {
                        syntax_checks.push(json!({"path": item.path, "syntax_check": "skipped"}))
                    }
                }
            }
        }
        let diff = render_diff(&planned);
        let snapshot_rebased_from = requested_snapshot.filter(|_| !snapshot_matches);
        if bool_value(params, "preview", false) {
            return Ok(PreparedEdit::Preview(json!({
                "preview": true,
                "workspace_id": self.id,
                "generation": self.generation(),
                "snapshot_id": current_snapshot,
                "snapshot_rebased_from": snapshot_rebased_from,
                "diff": diff,
                "syntax_checks": syntax_checks,
                "files": planned.iter().map(|item| &item.path).collect::<Vec<_>>()
            })));
        }
        self.recheck_preconditions(&planned)?;
        let request_id = format!("req_{}", Uuid::new_v4().simple());
        let commit_started = Instant::now();
        let apply_result = self.commit_plan(session_id, &planned, &request_id)?;
        let commit_ms = commit_started.elapsed().as_millis();
        Ok(PreparedEdit::Applied(AppliedEdit {
            write_guard,
            planned,
            request_id,
            apply_result,
            diff,
            snapshot_rebased_from,
            commit_ms,
            syntax_checks,
        }))
    }

    fn preflight_overlaps(&self, changes: &[Value], index: &CodeIndex) -> AppResult<()> {
        let mut change_counts = HashMap::<String, usize>::new();
        let mut handle_paths = HashSet::new();
        for change in changes {
            if let Some(path) = change.get("path").and_then(Value::as_str) {
                let path = path.replace('\\', "/");
                *change_counts.entry(path.clone()).or_default() += 1;
                if change.get("handle").and_then(Value::as_str).is_some() {
                    handle_paths.insert(path);
                }
            }
        }
        if let Some(path) = handle_paths
            .into_iter()
            .find(|path| change_counts.get(path).copied().unwrap_or_default() > 1)
        {
            return Err(AppError::details(
                "AMBIGUOUS_HANDLE_EDIT_ORDER",
                "A handle-based edit must be the only change for its file in one transaction",
                json!({
                    "path": path,
                    "guidance": "Use one replace_range for the complete fetched range, or use exact replacements without handles."
                }),
            ));
        }

        let mut ranges: HashMap<String, Vec<(usize, usize)>> = HashMap::new();
        for change in changes {
            let kind = change
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !matches!(kind, "replace" | "replace_range") {
                continue;
            }
            let path = required_str(change, "path")?.replace('\\', "/");
            let content = &index
                .get(&path)
                .ok_or_else(|| {
                    AppError::details(
                        "PATH_NOT_INDEXED",
                        "Replace path is not indexed",
                        json!({"path": path}),
                    )
                })?
                .content;
            let handle = change
                .get("handle")
                .and_then(Value::as_str)
                .map(decode_handle)
                .transpose()?;
            let (base_offset, selected) = if let Some(handle) = &handle {
                if handle.workspace_id != self.id || handle.path != path {
                    return Err(AppError::new(
                        "INVALID_HANDLE",
                        "Edit handle does not match workspace/path",
                    ));
                }
                let (start, end) = line_range_bytes(content, handle.start_line, handle.end_line)?;
                (start, &content[start..end])
            } else {
                (0, content.as_str())
            };
            let found: Vec<_> = if kind == "replace_range" {
                if handle.is_none() {
                    return Err(AppError::new(
                        "MISSING_HANDLE",
                        "replace_range requires a fetch handle",
                    ));
                }
                vec![(base_offset, base_offset + selected.len())]
            } else {
                let old = required_str(change, "old_text")?;
                let expected = usize_value(change, "expected_replacements", 1);
                let (old, actual) = matching_old_text(content, selected, old, expected);
                if actual != expected {
                    return Err(AppError::details(
                        "EXACT_MATCH_COUNT",
                        "Exact replacement count did not match",
                        json!({"path": path, "expected": expected, "actual": actual}),
                    ));
                }
                selected
                    .match_indices(old.as_str())
                    .map(|(start, text)| {
                        let start = base_offset + start;
                        (start, start + text.len())
                    })
                    .collect()
            };
            let existing = ranges.entry(path.clone()).or_default();
            for range in found {
                if existing
                    .iter()
                    .any(|other| range.0 < other.1 && other.0 < range.1)
                {
                    return Err(AppError::details(
                        "OVERLAPPING_EDITS",
                        "Two exact edits overlap",
                        json!({"path": path}),
                    ));
                }
                existing.push(range);
            }
        }
        Ok(())
    }

    fn plan_change(
        &self,
        change: &Value,
        has_snapshot: bool,
        plan: &mut HashMap<String, PlannedFile>,
        index: &CodeIndex,
    ) -> AppResult<()> {
        let kind = required_str(change, "kind")?;
        let path = required_str(change, "path")?.replace('\\', "/");
        validate_relative(&path)?;
        let expected_hash = change.get("expected_hash").and_then(Value::as_str);
        let edit_handle = change
            .get("handle")
            .and_then(Value::as_str)
            .map(decode_handle)
            .transpose()?;
        if let Some(handle) = &edit_handle {
            if handle.workspace_id != self.id || handle.path != path {
                return Err(AppError::new(
                    "INVALID_HANDLE",
                    "Edit handle does not match workspace/path",
                ));
            }
        }
        let handle_hash = edit_handle
            .as_ref()
            .map(|handle| handle.content_hash.as_str());
        let before = plan
            .get(&path)
            .map(|item| item.after.clone())
            .unwrap_or_else(|| index.get(&path).map(|file| file.content.clone()));
        let original_hash = plan
            .get(&path)
            .and_then(|item| item.before.as_ref())
            .map(|content| content_hash(content))
            .or_else(|| before.as_ref().map(|content| content_hash(content)));
        let edits_existing_file = kind != "create" || before.is_some();
        if edits_existing_file && !has_snapshot && expected_hash.is_none() && handle_hash.is_none()
        {
            return Err(AppError::details(
                "MISSING_PRECONDITION",
                "Existing-file edits require snapshot_id, expected_hash, or handle",
                json!({"path": path}),
            ));
        }
        if let Some(expected) = expected_hash.or(handle_hash) {
            if original_hash.as_deref() != Some(expected) {
                return Err(AppError::details(
                    "STALE_FILE",
                    "File hash precondition failed",
                    json!({"path": path, "expected_hash": expected, "actual_hash": original_hash}),
                ));
            }
        }
        match kind {
            "replace" => {
                let old = required_str(change, "old_text")?;
                let new_input = change
                    .get("new_text")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let expected = usize_value(change, "expected_replacements", 1);
                let current = before.ok_or_else(|| {
                    AppError::details(
                        "PATH_NOT_FOUND",
                        "Replace path does not exist",
                        json!({"path": path}),
                    )
                })?;
                let new = normalize_line_endings_for_content(&current, new_input);
                let after = if let Some(handle) = &edit_handle {
                    let (start, end) =
                        line_range_bytes(&current, handle.start_line, handle.end_line)?;
                    let selected = &current[start..end];
                    let (old, count) = matching_old_text(&current, selected, old, expected);
                    if count != expected {
                        return Err(AppError::details(
                            "EXACT_MATCH_COUNT",
                            "Exact replacement count did not match the provenance range",
                            json!({"path": path, "expected": expected, "actual": count, "start_line": handle.start_line, "end_line": handle.end_line}),
                        ));
                    }
                    let replacement = preserve_terminal_line_ending(old.as_str(), &new);
                    let mut value = current.clone();
                    value.replace_range(
                        start..end,
                        &selected.replacen(old.as_str(), &replacement, expected),
                    );
                    value
                } else {
                    let (old, count) = matching_old_text(&current, &current, old, expected);
                    if count != expected {
                        return Err(AppError::details(
                            "EXACT_MATCH_COUNT",
                            "Exact replacement count did not match current planned content",
                            json!({"path": path, "expected": expected, "actual": count}),
                        ));
                    }
                    let replacement = preserve_terminal_line_ending(old.as_str(), &new);
                    current.replacen(old.as_str(), &replacement, expected)
                };
                put_plan(plan, path, Some(current), Some(after));
            }
            "replace_range" => {
                let handle = edit_handle.as_ref().ok_or_else(|| {
                    AppError::new("MISSING_HANDLE", "replace_range requires a fetch handle")
                })?;
                let new_input = change
                    .get("new_text")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let current = before.ok_or_else(|| {
                    AppError::details(
                        "PATH_NOT_FOUND",
                        "Replace path does not exist",
                        json!({"path": path}),
                    )
                })?;
                let new = normalize_line_endings_for_content(&current, new_input);
                let (start, end) = line_range_bytes(&current, handle.start_line, handle.end_line)?;
                let replacement = preserve_terminal_line_ending(&current[start..end], &new);
                let mut after = current.clone();
                after.replace_range(start..end, &replacement);
                put_plan(plan, path, Some(current), Some(after));
            }
            "insert" => {
                let symbol_name = required_str(change, "anchor_symbol")?;
                let position = required_str(change, "position")?;
                let insert_input = required_str(change, "content")?;
                let current = before
                    .ok_or_else(|| AppError::new("PATH_NOT_FOUND", "Insert path does not exist"))?;
                let insert = normalize_line_endings_for_content(&current, insert_input);
                let absolute = self.root.join(&path);
                let symbol = extract_symbols(&absolute, &current)
                    .into_iter()
                    .find(|item| item.name == symbol_name)
                    .ok_or_else(|| {
                        AppError::details(
                            "SYMBOL_NOT_FOUND",
                            "Anchor symbol not found in current planned content",
                            json!({"path": path, "symbol": symbol_name}),
                        )
                    })?;
                let offset = line_offset(
                    &current,
                    match position {
                        "before" => symbol.start_line,
                        "after" => symbol.end_line + 1,
                        "inside_start" => symbol.start_line + 1,
                        "inside_end" => symbol.end_line,
                        _ => return Err(AppError::invalid("Invalid insert position")),
                    },
                );
                let mut after = current.clone();
                after.insert_str(offset, &insert);
                put_plan(plan, path, Some(current), Some(after));
            }
            "create" => {
                let content_input = change
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let overwrite = bool_value(change, "overwrite", false);
                if before.is_some() && !overwrite {
                    return Err(AppError::details(
                        "PATH_EXISTS",
                        "Create target already exists",
                        json!({"path": path}),
                    ));
                }
                let content = before
                    .as_ref()
                    .map(|current| normalize_line_endings_for_content(current, &content_input))
                    .unwrap_or(content_input);
                put_plan(plan, path, before, Some(content));
            }
            "delete" => {
                let current = before.ok_or_else(|| {
                    AppError::new("PATH_NOT_FOUND", "Delete target does not exist")
                })?;
                put_plan(plan, path, Some(current), None);
            }
            "rename" => {
                let to = required_str(change, "to")?.replace('\\', "/");
                validate_relative(&to)?;
                let current = before.ok_or_else(|| {
                    AppError::new("PATH_NOT_FOUND", "Rename source does not exist")
                })?;
                if plan.get(&to).and_then(|item| item.after.as_ref()).is_some()
                    || index.get(&to).is_some()
                {
                    return Err(AppError::details(
                        "PATH_EXISTS",
                        "Rename target already exists",
                        json!({"path": to}),
                    ));
                }
                put_plan(plan, path, Some(current.clone()), None);
                put_plan(plan, to, None, Some(current));
            }
            _ => {
                return Err(AppError::details(
                    "INVALID_CHANGE_KIND",
                    "Unknown change kind",
                    json!({"kind": kind}),
                ))
            }
        }
        Ok(())
    }

    fn recheck_preconditions(&self, plan: &[PlannedFile]) -> AppResult<()> {
        for item in plan {
            let actual = read_optional(&self.root, &item.path).map_err(|error| {
                AppError::details("READ_FAILED", error.to_string(), json!({"path": item.path}))
            })?;
            let actual_hash = actual.as_ref().map(|value| content_hash(value));
            let expected_hash = item.before.as_ref().map(|value| content_hash(value));
            if actual_hash != expected_hash {
                return Err(AppError::details(
                    "STALE_FILE",
                    "File changed after planning and before commit",
                    json!({"path": item.path, "expected_hash": expected_hash, "actual_hash": actual_hash}),
                ));
            }
        }
        Ok(())
    }

    pub(super) fn commit_plan(
        &self,
        session_id: &str,
        plan: &[PlannedFile],
        request_id: &str,
    ) -> AppResult<Vec<MutationRecord>> {
        let generation = self.generation() + 1;
        let timestamp = Utc::now();
        let records: Vec<MutationRecord> = plan
            .iter()
            .map(|item| MutationRecord {
                mutation_id: MutationRecord::new_id(),
                session_id: session_id.to_owned(),
                path: item.path.clone(),
                before_hash: item.before.as_ref().map(|value| content_hash(value)),
                after_hash: item.after.as_ref().map(|value| content_hash(value)),
                source: "mcp_edit".to_owned(),
                request_id: request_id.to_owned(),
                timestamp,
                generation,
            })
            .collect();
        let mut completed_count = 0usize;
        for item in plan {
            let absolute = self.root.join(&item.path);
            self.internal_writes
                .lock()
                .insert(absolute.clone(), Instant::now());
            let result = match &item.after {
                Some(content) => atomic_write(&self.root, &item.path, content),
                None => remove_if_exists(&self.root, &item.path),
            };
            if let Err(error) = result {
                self.internal_writes.lock().remove(&absolute);
                let mut restored_paths = Vec::new();
                let mut rollback_failures = Vec::new();
                for rollback in plan[..completed_count].iter().rev() {
                    self.internal_writes
                        .lock()
                        .insert(self.root.join(&rollback.path), Instant::now());
                    match restore_one(&self.root, rollback) {
                        Ok(()) => restored_paths.push(rollback.path.clone()),
                        Err(rollback_error) => rollback_failures.push(json!({
                            "path": rollback.path,
                            "error": rollback_error.to_string()
                        })),
                    }
                }
                let manual_recovery_required = !rollback_failures.is_empty();
                let code = if manual_recovery_required {
                    "PARTIAL_COMMIT"
                } else {
                    "ATOMIC_WRITE_FAILED"
                };
                return Err(AppError::details(
                    code,
                    error.to_string(),
                    json!({
                        "failed_path": item.path,
                        "completed_before_failure": plan[..completed_count].iter().map(|value| &value.path).collect::<Vec<_>>(),
                        "restored_paths": restored_paths,
                        "rollback_failures": rollback_failures,
                        "manual_recovery_required": manual_recovery_required
                    }),
                ));
            }
            completed_count += 1;
        }
        let paths: HashSet<PathBuf> = plan.iter().map(|item| self.root.join(&item.path)).collect();
        self.index
            .write()
            .refresh_paths(
                &self.root,
                &paths,
                self.policy.max_file_bytes,
                &self.exclusions,
            )
            .map_err(|error| {
                self.failed_after_apply(
                    plan,
                    completed_count,
                    "INDEX_REFRESH_FAILED",
                    error.to_string(),
                )
            })?;
        self.persist_mutations(&records).map_err(|error| {
            self.failed_after_apply(
                plan,
                completed_count,
                "JOURNAL_COMMIT_FAILED",
                error.to_string(),
            )
        })?;
        self.refresh_repo_status();
        self.generation.store(generation, Ordering::Release);
        self.recompute_snapshot();
        self.publish_mutations(&records);
        Ok(records)
    }

    fn failed_after_apply(
        &self,
        plan: &[PlannedFile],
        completed_count: usize,
        failure_code: &'static str,
        message: String,
    ) -> AppError {
        let completed = &plan[..completed_count.min(plan.len())];
        let mut restored_paths = Vec::new();
        let mut rollback_failures = Vec::new();
        for item in completed.iter().rev() {
            self.internal_writes
                .lock()
                .insert(self.root.join(&item.path), Instant::now());
            match restore_one(&self.root, item) {
                Ok(()) => restored_paths.push(item.path.clone()),
                Err(error) => rollback_failures.push(json!({
                    "path": item.path,
                    "error": error.to_string()
                })),
            }
        }

        let paths: HashSet<PathBuf> = completed
            .iter()
            .map(|item| self.root.join(&item.path))
            .collect();
        let rollback_refresh_error = if paths.is_empty() {
            None
        } else {
            match self.index.write().refresh_paths(
                &self.root,
                &paths,
                self.policy.max_file_bytes,
                &self.exclusions,
            ) {
                Ok(_) => None,
                Err(error) => {
                    self.pending_paths.lock().extend(paths);
                    self.needs_reconcile.store(true, Ordering::Release);
                    Some(error.0)
                }
            }
        };

        let manual_recovery_required = !rollback_failures.is_empty();
        AppError::details(
            if manual_recovery_required {
                "PARTIAL_COMMIT"
            } else {
                failure_code
            },
            message,
            json!({
                "completed_before_failure": completed.iter().map(|item| &item.path).collect::<Vec<_>>(),
                "restored_paths": restored_paths,
                "rollback_failures": rollback_failures,
                "manual_recovery_required": manual_recovery_required,
                "rollback_refresh_error": rollback_refresh_error
            }),
        )
    }

    fn persist_mutations(&self, records: &[MutationRecord]) -> AppResult<()> {
        if records.is_empty() {
            return Ok(());
        }
        let mut encoded = Vec::new();
        for record in records {
            serde_json::to_writer(&mut encoded, record).map_err(|error| {
                AppError::details(
                    "JOURNAL_SERIALIZATION_FAILED",
                    error.to_string(),
                    json!({"mutation_id": record.mutation_id}),
                )
            })?;
            encoded.push(b'\n');
        }

        let mut slot = self.journal_file.lock();
        let mut file = slot
            .take()
            .ok_or_else(|| AppError::new("JOURNAL_UNAVAILABLE", "Mutation journal is not open"))?;
        let current_len = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
        if current_len > 0 && current_len.saturating_add(encoded.len() as u64) > MAX_JOURNAL_BYTES {
            if let Err(error) = file.flush() {
                *slot = Some(file);
                return Err(AppError::details(
                    "JOURNAL_FLUSH_FAILED",
                    error.to_string(),
                    json!({}),
                ));
            }
            drop(file);
            if let Err(error) = rotate_journal_now(&self.journal_path) {
                *slot = open_journal(&self.journal_path).ok();
                return Err(error);
            }
            file = match open_journal(&self.journal_path) {
                Ok(file) => file,
                Err(error) => {
                    let archive = self.journal_path.with_file_name("mutations.previous.jsonl");
                    if self.journal_path.exists()
                        || fs::rename(&archive, &self.journal_path).is_ok()
                    {
                        *slot = open_journal(&self.journal_path).ok();
                    }
                    return Err(AppError::details(
                        "JOURNAL_OPEN_FAILED",
                        error.to_string(),
                        json!({"path": self.journal_path}),
                    ));
                }
            };
        }

        let original_len = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
        if let Err(error) = file
            .seek(SeekFrom::End(0))
            .and_then(|_| file.write_all(&encoded))
            .and_then(|_| file.flush())
        {
            let recovery_error = file
                .set_len(original_len)
                .and_then(|_| file.flush())
                .err()
                .map(|recovery| recovery.to_string());
            *slot = Some(file);
            return Err(AppError::details(
                if recovery_error.is_some() {
                    "JOURNAL_RECOVERY_FAILED"
                } else {
                    "JOURNAL_WRITE_FAILED"
                },
                error.to_string(),
                json!({
                    "original_len": original_len,
                    "recovery_error": recovery_error
                }),
            ));
        }
        *slot = Some(file);
        Ok(())
    }

    fn publish_mutations(&self, records: &[MutationRecord]) {
        let mut journal = self.mutations.lock();
        journal.extend(records.iter().cloned());
        trim_journal(&mut journal);
    }

    pub(super) fn recheck_applied_state(&self, plan: &[PlannedFile]) -> AppResult<()> {
        for item in plan {
            let actual = read_optional(&self.root, &item.path).map_err(|error| {
                AppError::details("READ_FAILED", error.to_string(), json!({"path": item.path}))
            })?;
            let actual_hash = actual.as_ref().map(|value| content_hash(value));
            let expected_hash = item.after.as_ref().map(|value| content_hash(value));
            if actual_hash != expected_hash {
                return Err(AppError::details(
                    "ROLLBACK_CONFLICT",
                    "Validation failed, but rollback was skipped because a file changed after the edit",
                    json!({"path": item.path, "expected_hash": expected_hash, "actual_hash": actual_hash}),
                ));
            }
        }
        Ok(())
    }

    fn restore_plan(
        &self,
        session_id: &str,
        plan: &[PlannedFile],
        request_id: &str,
    ) -> AppResult<()> {
        for item in plan.iter().rev() {
            self.internal_writes
                .lock()
                .insert(self.root.join(&item.path), Instant::now());
            restore_one(&self.root, item)?;
        }
        let paths: HashSet<PathBuf> = plan.iter().map(|item| self.root.join(&item.path)).collect();
        self.index.write().refresh_paths(
            &self.root,
            &paths,
            self.policy.max_file_bytes,
            &self.exclusions,
        )?;
        let generation = self.generation() + 1;
        let records: Vec<_> = plan
            .iter()
            .map(|item| MutationRecord {
                mutation_id: MutationRecord::new_id(),
                session_id: session_id.to_owned(),
                path: item.path.clone(),
                before_hash: item.after.as_ref().map(|value| content_hash(value)),
                after_hash: item.before.as_ref().map(|value| content_hash(value)),
                source: "rollback".to_owned(),
                request_id: request_id.to_owned(),
                timestamp: Utc::now(),
                generation,
            })
            .collect();
        self.persist_mutations(&records)?;
        self.refresh_repo_status();
        self.generation.store(generation, Ordering::Release);
        self.recompute_snapshot();
        self.publish_mutations(&records);
        Ok(())
    }

    pub(super) fn record_mutations(&self, records: &[MutationRecord]) -> AppResult<()> {
        self.persist_mutations(records)?;
        self.publish_mutations(records);
        Ok(())
    }
}

fn put_plan(
    plan: &mut HashMap<String, PlannedFile>,
    path: String,
    before: Option<String>,
    after: Option<String>,
) {
    if let Some(existing) = plan.get_mut(&path) {
        existing.after = after;
    } else {
        plan.insert(
            path.clone(),
            PlannedFile {
                path,
                before,
                after,
            },
        );
    }
}

fn char_boundary_at_or_before(value: &str, mut index: usize) -> usize {
    index = index.min(value.len());
    while index > 0 && !value.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn preserve_terminal_line_ending(selected: &str, replacement: &str) -> String {
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

fn matching_old_text(content: &str, selected: &str, old: &str, expected: usize) -> (String, usize) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_view_truncates_on_utf8_boundary() {
        let diff = format!("{}é\nnext\n", "a".repeat(10));

        let view = DiffView::build(&diff, &[], "standard", 11);

        assert!(view.truncated);
        assert_eq!(view.diff.as_str().unwrap(), "aaaaaaaaaa");
    }
}
