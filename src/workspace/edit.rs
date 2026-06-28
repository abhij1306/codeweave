use super::io_helpers::{atomic_write, read_optional, remove_if_exists, render_diff, restore_one};
use super::journal::{
    open_journal, rotate_journal_now, trim_journal, MutationRecord, MAX_JOURNAL_BYTES,
};
use super::util::{
    changes_without_independent_preconditions, line_offset, line_range_bytes,
    normalize_line_endings_for_content, stale_snapshot_for_paths,
};
use super::WorkspaceActor;
use crate::bash::StartRequest;
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
    planned: Vec<PlannedFile>,
    request_id: String,
    apply_result: Vec<MutationRecord>,
    diff: String,
    snapshot_rebased_from: Option<String>,
    commit_ms: u128,
}

enum PreparedEdit {
    Preview(Value),
    Applied(AppliedEdit),
}

impl WorkspaceActor {
    pub async fn code_edit(self: &Arc<Self>, session_id: &str, params: &Value) -> AppResult<Value> {
        let validate = string_list(params, "validate");
        if !validate.is_empty() && !self.policy.bash.enabled {
            return Err(AppError::new(
                "BASH_DISABLED",
                "Bash execution is disabled; enable policy.bash before using validation commands",
            ));
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
            planned,
            request_id,
            apply_result,
            diff,
            snapshot_rebased_from,
            commit_ms,
        } = applied;

        let mut validation = Vec::new();
        let mut validation_failed = false;
        for command in validate {
            match self
                .bash
                .start(
                    &self.root,
                    StartRequest {
                        command: command.clone(),
                        cwd: None,
                        background: Some(false),
                        timeout_ms: None,
                    },
                )
                .await
            {
                Ok(result) => {
                    if result.get("status").and_then(Value::as_str) != Some("succeeded") {
                        validation_failed = true;
                    }
                    validation.push(json!({"command": command, "result": result}));
                }
                Err(error) => {
                    validation_failed = true;
                    validation.push(json!({
                        "command": command,
                        "error": error.0,
                    }));
                }
            }
            if validation_failed {
                break;
            }
        }
        let rollback_on_failure = bool_value(params, "rollback_on_failure", true);
        if validation_failed && rollback_on_failure {
            let write_guard = self.write_lock.clone().lock_owned().await;
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
                    "diff": diff,
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
                "diff": diff,
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

        self.reconcile_pending_async().await?;
        Ok(json!({
            "applied": true,
            "rolled_back": false,
            "snapshot_rebased_from": snapshot_rebased_from,
            "diff": diff,
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
        for item in &planned {
            if let Some(after) = &item.after {
                let absolute = self.root.join(&item.path);
                if parse_has_error(&absolute, after) == Some(true) {
                    return Err(AppError::details(
                        "SYNTAX_ERROR",
                        "Tree-sitter found syntax errors in planned content",
                        json!({"path": item.path}),
                    ));
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
                "files": planned.iter().map(|item| &item.path).collect::<Vec<_>>()
            })));
        }
        self.recheck_preconditions(&planned)?;
        let request_id = format!("req_{}", Uuid::new_v4().simple());
        let commit_started = Instant::now();
        let apply_result = self.commit_plan(session_id, &planned, &request_id)?;
        let commit_ms = commit_started.elapsed().as_millis();
        drop(write_guard);
        Ok(PreparedEdit::Applied(AppliedEdit {
            planned,
            request_id,
            apply_result,
            diff,
            snapshot_rebased_from,
            commit_ms,
        }))
    }

    fn preflight_overlaps(&self, changes: &[Value], index: &CodeIndex) -> AppResult<()> {
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
                    let mut value = current.clone();
                    value.replace_range(
                        start..end,
                        &selected.replacen(old.as_str(), &new, expected),
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
                    current.replacen(old.as_str(), &new, expected)
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
                let mut after = current.clone();
                after.replace_range(start..end, &new);
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
                mutation_id: format!("mut_{}", Uuid::new_v4().simple()),
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
                mutation_id: format!("mut_{}", Uuid::new_v4().simple()),
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
