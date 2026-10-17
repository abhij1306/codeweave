use super::edit::PlannedFile;
use super::events::{MutationRecord, MutationSource};
use super::io_helpers::{atomic_write, remove_if_exists, restore_one};
use super::state::StateWriter;
use super::Workspace;
use crate::error::{AppError, AppResult, ErrorCode};
use crate::index::content_hash;
use chrono::Utc;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::PathBuf;
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
    /// Write a planned edit and publish it as the next generation. The caller holds
    /// the mutation permit and passes the writer it planned under.
    pub(super) fn commit_plan(
        &self,
        writer: &mut StateWriter<'_>,
        plan: &[PlannedFile],
        request_id: &str,
    ) -> AppResult<Vec<MutationRecord>> {
        let ledger = self.state.ledger();
        let generation = writer.read().generation() + 1;
        let timestamp = Utc::now();
        let records: Vec<MutationRecord> = plan
            .iter()
            .map(|item| MutationRecord {
                mutation_id: MutationRecord::new_id(),
                path: item.path.clone(),
                before_hash: item.before_hash.clone(),
                after_hash: item.after.as_deref().map(content_hash),
                source: MutationSource::McpEdit,
                request_id: request_id.to_owned(),
                timestamp,
                generation,
            })
            .collect();
        let mut progress = CommitProgress::new(plan);
        for item in plan {
            let absolute = self.root.join(&item.path);
            ledger.record_internal(absolute.clone(), Instant::now());
            let result = match &item.after {
                Some(content) => atomic_write(&self.root, &item.path, content),
                None => remove_if_exists(&self.root, &item.path),
            };
            if let Err(error) = result {
                ledger.forget_internal(&absolute);
                let report = self.compensate(writer, progress.completed(), false);
                let manual_recovery_required = report.manual_recovery_required();
                return Err(AppError::with_details(
                    if manual_recovery_required {
                        ErrorCode::PartialCommit
                    } else {
                        ErrorCode::AtomicWriteFailed
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
        let status = self.repository.status(&self.root);
        let refreshed = writer.update(|state| {
            state.index_mut().refresh_paths(
                &self.root,
                &paths,
                self.policy.max_file_bytes,
                &self.exclusions,
            )?;
            // A failed `git status` keeps the previous status and marks it stale.
            let _ = state.apply_repo_status(status);
            state.bump_generation();
            state.recompute_snapshot();
            Ok::<(), AppError>(())
        });
        if let Err(error) = refreshed {
            return Err(self.failed_after_apply(
                writer,
                &progress,
                ErrorCode::IndexRefreshFailed,
                error.to_string(),
            ));
        }
        ledger.record(&records);
        Ok(records)
    }

    fn failed_after_apply(
        &self,
        writer: &mut StateWriter<'_>,
        progress: &CommitProgress<'_>,
        failure_code: ErrorCode,
        message: String,
    ) -> AppError {
        let report = self.compensate(writer, progress.completed(), true);
        let manual_recovery_required = report.manual_recovery_required();
        AppError::with_details(
            if manual_recovery_required {
                ErrorCode::PartialCommit
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

    fn compensate(
        &self,
        writer: &mut StateWriter<'_>,
        completed: &[PlannedFile],
        refresh_index: bool,
    ) -> CompensationReport {
        if completed.is_empty() {
            return CompensationReport::default();
        }
        let now = Instant::now();
        for item in completed {
            self.state
                .ledger()
                .record_internal(self.root.join(&item.path), now);
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
            let refreshed = writer.update(|state| {
                state.index_mut().refresh_paths(
                    &self.root,
                    &paths,
                    self.policy.max_file_bytes,
                    &self.exclusions,
                )
            });
            if let Err(error) = refreshed {
                self.state.ledger().mark_pending(paths);
                report.compensation_refresh_error = Some(json!(error));
            }
        }
        report
    }
}
