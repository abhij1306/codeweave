use super::edit::PlannedFile;
use super::io_helpers::{atomic_write, remove_if_exists, restore_one};
use super::journal::{
    open_journal, rotate_journal_now, trim_journal, MutationRecord, MAX_JOURNAL_BYTES,
};
use super::WorkspaceActor;
use crate::index::content_hash;
use crate::model::{AppError, AppResult};
use chrono::Utc;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::fs;
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::Instant;

struct CommitProgress<'a> {
    plan: &'a [PlannedFile],
    completed_count: usize,
}

impl<'a> CommitProgress<'a> {
    fn new(plan: &'a [PlannedFile]) -> Self {
        Self {
            plan,
            completed_count: 0,
        }
    }

    fn advance(&mut self) {
        self.completed_count += 1;
    }

    fn completed(&self) -> &'a [PlannedFile] {
        &self.plan[..self.completed_count.min(self.plan.len())]
    }

    fn completed_paths(&self) -> Vec<&'a String> {
        self.completed().iter().map(|item| &item.path).collect()
    }
}

#[derive(Default)]
struct CompensationReport {
    restored_paths: Vec<String>,
    rollback_failures: Vec<Value>,
    rollback_refresh_error: Option<Value>,
}

impl CompensationReport {
    fn manual_recovery_required(&self) -> bool {
        !self.rollback_failures.is_empty()
    }
}

impl WorkspaceActor {
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
        let mut progress = CommitProgress::new(plan);
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
                let report = self.compensate(progress.completed(), false);
                let manual_recovery_required = report.manual_recovery_required();
                return Err(AppError::details(
                    if manual_recovery_required {
                        "PARTIAL_COMMIT"
                    } else {
                        "ATOMIC_WRITE_FAILED"
                    },
                    error.to_string(),
                    json!({
                        "failed_path": item.path,
                        "completed_before_failure": progress.completed_paths(),
                        "restored_paths": report.restored_paths,
                        "rollback_failures": report.rollback_failures,
                        "manual_recovery_required": manual_recovery_required
                    }),
                ));
            }
            progress.advance();
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
                self.failed_after_apply(&progress, "INDEX_REFRESH_FAILED", error.to_string())
            })?;
        self.persist_mutations(&records).map_err(|error| {
            self.failed_after_apply(&progress, "JOURNAL_COMMIT_FAILED", error.to_string())
        })?;
        self.refresh_repo_status();
        self.generation.store(generation, Ordering::Release);
        self.recompute_snapshot();
        self.publish_mutations(&records);
        Ok(records)
    }

    fn failed_after_apply(
        &self,
        progress: &CommitProgress<'_>,
        failure_code: &'static str,
        message: String,
    ) -> AppError {
        let report = self.compensate(progress.completed(), true);
        let manual_recovery_required = report.manual_recovery_required();
        AppError::details(
            if manual_recovery_required {
                "PARTIAL_COMMIT"
            } else {
                failure_code
            },
            message,
            json!({
                "completed_before_failure": progress.completed_paths(),
                "restored_paths": report.restored_paths,
                "rollback_failures": report.rollback_failures,
                "manual_recovery_required": manual_recovery_required,
                "rollback_refresh_error": report.rollback_refresh_error
            }),
        )
    }

    fn compensate(&self, completed: &[PlannedFile], refresh_index: bool) -> CompensationReport {
        if completed.is_empty() {
            return CompensationReport::default();
        }
        {
            let now = Instant::now();
            let mut internal = self.internal_writes.lock();
            for item in completed {
                internal.insert(self.root.join(&item.path), now);
            }
        }

        let mut report = CompensationReport::default();
        for item in completed.iter().rev() {
            match restore_one(&self.root, item) {
                Ok(()) => report.restored_paths.push(item.path.clone()),
                Err(error) => report.rollback_failures.push(json!({
                    "path": item.path,
                    "error": error.to_string()
                })),
            }
        }

        if refresh_index {
            let paths: HashSet<PathBuf> = completed
                .iter()
                .map(|item| self.root.join(&item.path))
                .collect();
            if let Err(error) = self.index.write().refresh_paths(
                &self.root,
                &paths,
                self.policy.max_file_bytes,
                &self.exclusions,
            ) {
                self.pending_paths.lock().extend(paths);
                self.needs_reconcile.store(true, Ordering::Release);
                report.rollback_refresh_error = Some(json!(error.0));
            }
        }
        report
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

    pub(super) fn record_mutations(&self, records: &[MutationRecord]) -> AppResult<()> {
        self.persist_mutations(records)?;
        self.publish_mutations(records);
        Ok(())
    }
}
