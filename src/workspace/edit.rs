use super::events::MutationRecord;
use super::io_helpers::{diff_stat, read_optional, render_diff};
use super::state::MutationPermit;
use super::util::{
    changes_without_independent_preconditions, char_boundary_at_or_before, line_offset,
    line_range_bytes, matching_old_text, normalize_line_endings_for_content,
    preserve_terminal_line_ending, stale_snapshot_for_paths,
};
use super::validation::ValidationOutcome;
use super::Workspace;
use crate::index::{content_hash, decode_handle, CodeIndex};
use crate::model::{AppError, AppResult};
use crate::requests::edit::{Change, EditRequest, InsertPosition, ResponseDetail};
use crate::security::validate_relative;
use crate::symbols::{extract_symbols, parse_has_error};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub(super) struct PlannedFile {
    pub(super) path: String,
    pub(super) before: Option<String>,
    pub(super) after: Option<String>,
}

struct AppliedEdit {
    permit: MutationPermit,
    planned: Vec<PlannedFile>,
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
    fn build(diff: &str, planned: &[PlannedFile], detail: ResponseDetail, cap: usize) -> Self {
        let stat = json!(diff_stat(planned)
            .into_iter()
            .map(|(path, added, removed)| json!({
                "path": path,
                "added": added,
                "removed": removed,
            }))
            .collect::<Vec<_>>());
        match detail {
            ResponseDetail::Compact => Self {
                diff: Value::Null,
                stat,
                truncated: false,
                omitted: true,
            },
            ResponseDetail::Debug => Self {
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

impl Workspace {
    pub async fn code_edit(self: &Arc<Self>, request: EditRequest) -> AppResult<Value> {
        let validate = request.validate.clone();
        let detail = request.response_detail;
        if !validate.is_empty() {
            self.bash.ensure_available()?;
        }
        // The permit is held through post-apply validation so a mutating Git
        // action cannot change the edited files while their validation runs.
        let permit = self.state.exclusive().await;
        let actor = Arc::clone(self);
        let prepare_started = Instant::now();
        let prepared = tokio::task::spawn_blocking(move || actor.prepare_edit(&request, permit))
            .await
            .map_err(AppError::internal)??;
        let prepare_ms = prepare_started.elapsed().as_millis();

        let applied = match prepared {
            PreparedEdit::Preview(value) => return Ok(value),
            PreparedEdit::Applied(applied) => applied,
        };
        let AppliedEdit {
            permit,
            planned,
            apply_result,
            diff,
            snapshot_rebased_from,
            commit_ms,
            syntax_checks,
        } = applied;
        let mut permit = Some(permit);

        // Shape the diff payload by response_detail. `standard` (default) caps the
        // unified diff so a large edit cannot balloon the response; the per-file stat
        // is always returned.
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
        } = self.run_edit_validation(&validate).await;

        if let Some(run_id) = validation_pending {
            let mut validation_run_ids = vec![run_id.clone()];
            if let Some(deferred_run_id) = deferred_validation_pending.as_ref() {
                validation_run_ids.push(deferred_run_id.clone());
            }
            let guidance = if deferred_validation_pending.is_some() {
                "Post-apply validation is running in the background. The edit remains applied. Poll bash_status for every ID in validation_run_ids; validation_run_id is the leading validator and deferred_validation_run_id is queued behind it."
            } else {
                "Post-apply validation is running in the background. The edit remains applied. Poll bash_status with validation_run_id."
            };
            drop(permit.take());
            self.reconcile_pending_async().await?;
            let stamp = self.state.read().stamp();
            let mut response = json!({
                "applied": true,
                "validation_failed": validation_failed,
                "validation_pending": true,
                "validation_status": "pending",
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
                "generation": stamp.generation,
                "snapshot_id": stamp.snapshot_id,
                "mutations": apply_result,
                "phase_ms": {
                    "prepare_and_commit": prepare_ms,
                    "commit": commit_ms
                }
            });
            if let Some(deferred_run_id) = deferred_validation_pending {
                response["deferred_validation_run_id"] = Value::String(deferred_run_id);
            }
            if validation_failed {
                response["reason"] = Value::String("validation_failed".to_owned());
            }
            return Ok(response);
        }

        drop(permit.take());
        self.reconcile_pending_async().await?;
        let stamp = self.state.read().stamp();
        let mut response = json!({
            "applied": true,
            "validation_failed": validation_failed,
            "validation_status": if validation_failed { "failed" } else { "passed" },
            "snapshot_rebased_from": snapshot_rebased_from,
            "diff": diff_value,
            "diff_stat": diff_stat_value,
            "diff_truncated": diff_truncated,
            "diff_omitted": diff_omitted,
            "syntax_checks": syntax_checks,
            "validation": validation,
            "generation": stamp.generation,
            "snapshot_id": stamp.snapshot_id,
            "mutations": apply_result,
            "phase_ms": {
                "prepare_and_commit": prepare_ms,
                "commit": commit_ms
            }
        });
        if validation_failed {
            response["reason"] = Value::String("validation_failed".to_owned());
            response["guidance"] = Value::String(
                "Post-apply validation failed, but the edit remains applied so the reported failure can be fixed in a follow-up edit."
                    .to_owned(),
            );
        }
        Ok(response)
    }

    fn prepare_edit(
        &self,
        request: &EditRequest,
        permit: MutationPermit,
    ) -> AppResult<PreparedEdit> {
        let mut writer = self.state.writer();
        self.reconcile_with(&mut writer)?;
        let stamp = writer.read().stamp();
        let current_snapshot = stamp.snapshot_id;
        let requested_snapshot = request.snapshot_id.clone();
        let changes = &request.changes;
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
        let view = writer.read();
        let index = view.index();
        self.preflight_overlaps(changes, index)?;
        let mut plan: HashMap<String, PlannedFile> = HashMap::new();
        for change in changes {
            self.plan_change(
                change,
                requested_snapshot.is_some() && snapshot_matches,
                &mut plan,
                index,
            )?;
        }
        drop(view);
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
        if request.preview {
            return Ok(PreparedEdit::Preview(json!({
                "preview": true,
                "workspace_id": self.id,
                "generation": stamp.generation,
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
        let apply_result = self.commit_plan(&mut writer, &planned, &request_id)?;
        let commit_ms = commit_started.elapsed().as_millis();
        Ok(PreparedEdit::Applied(AppliedEdit {
            permit,
            planned,
            apply_result,
            diff,
            snapshot_rebased_from,
            commit_ms,
            syntax_checks,
        }))
    }

    fn preflight_overlaps(&self, changes: &[Change], index: &CodeIndex) -> AppResult<()> {
        let mut change_counts = HashMap::<String, usize>::new();
        let mut handle_paths = HashSet::new();
        for change in changes {
            let path = change.path().replace('\\', "/");
            *change_counts.entry(path.clone()).or_default() += 1;
            if change.handle().is_some() {
                handle_paths.insert(path);
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
            // Exact replacements carry their expected match; a range replaces
            // the whole handle range.
            let exact = match change {
                Change::Replace {
                    old_text,
                    expected_replacements,
                    ..
                } => Some((old_text.as_str(), *expected_replacements)),
                Change::ReplaceRange { .. } => None,
                _ => continue,
            };
            let path = change.path().replace('\\', "/");
            let content = index
                .get(&path)
                .ok_or_else(|| {
                    AppError::details(
                        "PATH_NOT_INDEXED",
                        "Replace path is not indexed",
                        json!({"path": path}),
                    )
                })?
                .content();
            let handle = change.handle().map(decode_handle).transpose()?;
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
                (0, content)
            };
            let found: Vec<_> = if let Some((old, expected)) = exact {
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
            } else {
                vec![(base_offset, base_offset + selected.len())]
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
        change: &Change,
        has_snapshot: bool,
        plan: &mut HashMap<String, PlannedFile>,
        index: &CodeIndex,
    ) -> AppResult<()> {
        let path = change.path().replace('\\', "/");
        validate_relative(&path)?;
        let expected_hash = change.expected_hash();
        let edit_handle = change.handle().map(decode_handle).transpose()?;
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
            .unwrap_or_else(|| index.get(&path).map(|file| file.content().to_owned()));
        let original_hash = plan
            .get(&path)
            .and_then(|item| item.before.as_ref())
            .map(|content| content_hash(content))
            .or_else(|| before.as_ref().map(|content| content_hash(content)));
        let edits_existing_file = !matches!(change, Change::Create { .. }) || before.is_some();
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
        match change {
            Change::Replace {
                old_text,
                new_text,
                expected_replacements,
                ..
            } => {
                let old = old_text.as_str();
                let new_input = new_text.as_str();
                let expected = *expected_replacements;
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
            Change::ReplaceRange { new_text, .. } => {
                let handle = edit_handle.as_ref().ok_or_else(|| {
                    AppError::new(
                        "MISSING_HANDLE",
                        "replace_range requires a fresh code_retrieve range handle",
                    )
                })?;
                let new_input = new_text.as_str();
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
            Change::Insert {
                content,
                anchor_symbol,
                position,
                ..
            } => {
                let symbol_name = anchor_symbol.as_str();
                let insert_input = content.as_str();
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
                        InsertPosition::Before => symbol.start_line,
                        InsertPosition::After => symbol.end_line + 1,
                        InsertPosition::InsideStart => symbol.start_line + 1,
                        InsertPosition::InsideEnd => symbol.end_line,
                    },
                );
                let mut after = current.clone();
                after.insert_str(offset, &insert);
                put_plan(plan, path, Some(current), Some(after));
            }
            Change::Create {
                content, overwrite, ..
            } => {
                let content_input = content.clone();
                if before.is_some() && !*overwrite {
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
            Change::Delete { .. } => {
                let current = before.ok_or_else(|| {
                    AppError::new("PATH_NOT_FOUND", "Delete target does not exist")
                })?;
                put_plan(plan, path, Some(current), None);
            }
            Change::Rename { to, .. } => {
                let to = to.replace('\\', "/");
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
