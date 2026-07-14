use super::edit::PlannedFile;
use super::events::{append_events, MutationRecord};
use super::io_helpers::{atomic_write, remove_if_exists, restore_one};
use super::Workspace;
use crate::index::content_hash;
use crate::model::{AppError, AppResult};
use chrono::Utc;
use serde_json::{json, Value};
use std::collections::HashSet;
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
    compensation_failures: Vec<Value>,
    compensation_refresh_error: Option<Value>,
}

impl CompensationReport {
    fn manual_recovery_required(&self) -> bool {
        !self.compensation_failures.is_empty()
    }
}

impl Workspace {
    pub(super) fn commit_plan(
        &self,
        plan: &[PlannedFile],
        request_id: &str,
    ) -> AppResult<Vec<MutationRecord>> {
        let generation = self.generation() + 1;
        let timestamp = Utc::now();
        let records: Vec<MutationRecord> = plan
            .iter()
            .map(|item| MutationRecord {
                mutation_id: MutationRecord::new_id(),
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
                        "compensation_failures": report.compensation_failures,
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
                "compensation_failures": report.compensation_failures,
                "manual_recovery_required": manual_recovery_required,
                "compensation_refresh_error": report.compensation_refresh_error
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
                Err(error) => report.compensation_failures.push(json!({
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
                report.compensation_refresh_error = Some(json!(error.0));
            }
        }
        report
    }

    fn publish_mutations(&self, records: &[MutationRecord]) {
        append_events(&mut self.mutations.lock(), records);
    }

    pub(super) fn record_mutations(&self, records: &[MutationRecord]) -> AppResult<()> {
        self.publish_mutations(records);
        Ok(())
    }
}
