//! The Bash supervisor: starts runs one at a time, tracks each run's record, and
//! serves status, output and cancellation for them.
mod execution;
mod launch;
mod lifecycle;
mod output;
mod readiness;

use crate::model::{AppError, AppResult, PolicyConfig};
use crate::security::resolve_existing;
use chrono::{DateTime, Utc};
use execution::{execute, finalize_run_error, AbandonGuard, ExecutionRequest, TERMINATE_GRACE};
use launch::{Launcher, ShellLauncher};
pub use lifecycle::RunStatus;
use lifecycle::{CancelRequest, RunRecord};
pub(crate) use lifecycle::{ChangeBaseline, FrozenChanges, RunChangeWindow};
pub use output::OutputStream;
use output::{clamp_chars, continuation_offset, continuation_token};
use parking_lot::Mutex;
use readiness::resolve_bash;
pub use readiness::BashReadiness;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::{timeout, Duration};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize)]
pub struct BashRunView {
    pub run_id: String,
    pub status: RunStatus,
    pub command: String,
    pub cwd: String,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub exit_code: Option<i32>,
    pub output: String,
    pub output_truncated: bool,
    pub retention_policy: &'static str,
    pub dropped_prefix_chars: usize,
    pub retained_start_offset: usize,
    pub log_handle: String,
    pub status_fetch: serde_json::Value,
    pub pid: Option<u32>,
}

type SharedRecord = Arc<Mutex<RunRecord>>;

/// Retained runs by id.
///
/// Lock order: the table lock is never held while a record lock is taken.
/// Callers copy the record handles out, release the table, then lock records.
/// (Record locks may be held while taking the table lock; nothing does that.)
#[derive(Debug, Default)]
struct RunTable {
    runs: Mutex<HashMap<String, SharedRecord>>,
}

impl RunTable {
    fn get(&self, run_id: &str) -> AppResult<SharedRecord> {
        self.runs.lock().get(run_id).cloned().ok_or_else(|| {
            AppError::details(
                "RUN_NOT_FOUND",
                "Bash run not found",
                json!({"run_id": run_id}),
            )
        })
    }

    fn records(&self) -> Vec<SharedRecord> {
        self.runs.lock().values().cloned().collect()
    }

    /// Insert a run, then evict the oldest terminal runs beyond `retain`.
    /// Active runs are never evicted.
    fn insert(&self, run_id: String, record: SharedRecord, retain: usize) {
        let snapshot: Vec<(String, SharedRecord)> = {
            let mut runs = self.runs.lock();
            runs.insert(run_id, record);
            if runs.len() <= retain {
                return;
            }
            runs.iter()
                .map(|(id, record)| (id.clone(), record.clone()))
                .collect()
        };
        let excess = snapshot.len().saturating_sub(retain);
        let mut ended: Vec<(DateTime<Utc>, u64, String)> = snapshot
            .into_iter()
            .filter_map(|(id, record)| {
                let record = record.lock();
                record
                    .ended_at()
                    .map(|ended| (ended, record.sequence(), id))
            })
            .collect();
        ended.sort();
        // A terminal run never becomes active again, so these stay evictable.
        let mut runs = self.runs.lock();
        for (_, _, run_id) in ended.into_iter().take(excess) {
            runs.remove(&run_id);
        }
    }
}

#[derive(Clone)]
pub struct BashSupervisor {
    runs: Arc<RunTable>,
    next_run_sequence: Arc<AtomicU64>,
    run_permit: Arc<Semaphore>,
    policy: PolicyConfig,
    readiness: BashReadiness,
    /// Present exactly when Bash is ready.
    launcher: Option<Arc<dyn Launcher>>,
}

impl std::fmt::Debug for BashSupervisor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BashSupervisor")
            .field("runs", &self.runs)
            .field("policy", &self.policy)
            .field("readiness", &self.readiness)
            .finish_non_exhaustive()
    }
}

const MAX_RETAINED_RUNS: usize = 128;

#[derive(Debug, Clone)]
pub struct StartRequest {
    pub command: String,
    pub cwd: Option<String>,
    pub background: Option<bool>,
    pub timeout_ms: Option<u64>,
}

struct PreparedStartRequest {
    launcher: Arc<dyn Launcher>,
    command: String,
    cwd: PathBuf,
    background: bool,
    timeout_ms: u64,
}

fn validate_policy(policy: &PolicyConfig) -> AppResult<()> {
    if policy.bash.executable.trim().is_empty() {
        return Err(AppError::details(
            "INVALID_POLICY",
            "policy.bash.executable must not be empty",
            json!({}),
        ));
    }
    if policy.bash.max_output_chars == 0 {
        return Err(AppError::details(
            "INVALID_POLICY",
            "policy.bash.maxOutputChars must be greater than zero",
            json!({"max_output_chars": policy.bash.max_output_chars}),
        ));
    }
    if policy.bash.default_timeout_ms == 0 || policy.bash.max_timeout_ms == 0 {
        return Err(AppError::details(
            "INVALID_POLICY",
            "policy.bash timeout values must be greater than zero",
            json!({
                "default_timeout_ms": policy.bash.default_timeout_ms,
                "max_timeout_ms": policy.bash.max_timeout_ms
            }),
        ));
    }
    if policy.bash.default_timeout_ms > policy.bash.max_timeout_ms {
        return Err(AppError::details(
            "INVALID_POLICY",
            "policy.bash.defaultTimeoutMs must be less than or equal to maxTimeoutMs",
            json!({
                "default_timeout_ms": policy.bash.default_timeout_ms,
                "max_timeout_ms": policy.bash.max_timeout_ms
            }),
        ));
    }
    Ok(())
}

impl BashSupervisor {
    pub fn new(policy: PolicyConfig) -> AppResult<Self> {
        validate_policy(&policy)?;
        let readiness = resolve_bash(&policy);
        let launcher = readiness
            .executable()
            .filter(|_| readiness.is_ready())
            .map(|executable| Arc::new(ShellLauncher::new(executable)) as Arc<dyn Launcher>);
        Ok(Self::with_parts(policy, readiness, launcher))
    }

    fn with_parts(
        policy: PolicyConfig,
        readiness: BashReadiness,
        launcher: Option<Arc<dyn Launcher>>,
    ) -> Self {
        Self {
            runs: Arc::default(),
            next_run_sequence: Arc::new(AtomicU64::new(0)),
            run_permit: Arc::new(Semaphore::new(1)),
            policy,
            readiness,
            launcher,
        }
    }

    pub fn readiness(&self) -> BashReadiness {
        self.readiness.clone()
    }

    pub fn ensure_available(&self) -> AppResult<()> {
        self.launcher().map(|_| ())
    }

    fn launcher(&self) -> AppResult<Arc<dyn Launcher>> {
        self.launcher.clone().ok_or_else(|| {
            AppError::details(
                "BASH_UNAVAILABLE",
                self.readiness
                    .failure_reason
                    .clone()
                    .unwrap_or_else(|| "No usable Bash implementation found".to_owned()),
                json!({"execution": self.readiness()}),
            )
        })
    }

    pub fn running_count(&self) -> usize {
        self.runs
            .records()
            .into_iter()
            .filter(|record| !record.lock().status().is_terminal())
            .count()
    }

    /// Set the workspace stamp a run's change attribution starts from.
    pub(crate) fn set_change_baseline(&self, run_id: &str, baseline: ChangeBaseline) {
        if let Ok(record) = self.runs.get(run_id) {
            record.lock().set_baseline(baseline);
        }
    }

    /// What the workspace needs to attribute changes to a run. A run without a
    /// baseline adopts `fallback`.
    pub(crate) fn change_window(
        &self,
        run_id: &str,
        fallback: ChangeBaseline,
    ) -> AppResult<RunChangeWindow> {
        Ok(self.runs.get(run_id)?.lock().change_window(fallback))
    }

    /// Fix a terminal run's changed paths; returns the frozen result, which is
    /// the earlier one if the run was already frozen.
    pub(crate) fn freeze_changes(
        &self,
        run_id: &str,
        changes: FrozenChanges,
    ) -> AppResult<FrozenChanges> {
        Ok(self.runs.get(run_id)?.lock().freeze_changes(changes))
    }

    fn prepare_start_request(
        &self,
        root: &Path,
        request: StartRequest,
    ) -> AppResult<PreparedStartRequest> {
        let launcher = self.launcher()?;
        let command = request.command.trim().to_owned();
        if command.is_empty() {
            return Err(AppError::invalid("Bash command cannot be empty"));
        }
        let cwd_relative = request.cwd.unwrap_or_else(|| ".".to_owned());
        let cwd = if cwd_relative == "." {
            root.to_path_buf()
        } else {
            resolve_existing(root, &cwd_relative)?
        };
        if !cwd.is_dir() {
            return Err(AppError::new("INVALID_CWD", "Bash cwd is not a directory"));
        }
        let timeout_ms = request
            .timeout_ms
            .unwrap_or(self.policy.bash.default_timeout_ms);
        if timeout_ms == 0 || timeout_ms > self.policy.bash.max_timeout_ms {
            return Err(AppError::details(
                "INVALID_TIMEOUT",
                "timeout_ms must be greater than zero and no larger than policy.bash.maxTimeoutMs",
                json!({
                    "timeout_ms": timeout_ms,
                    "max_timeout_ms": self.policy.bash.max_timeout_ms
                }),
            ));
        }
        Ok(PreparedStartRequest {
            launcher,
            command,
            cwd,
            background: request.background.unwrap_or(false),
            timeout_ms,
        })
    }

    pub async fn start(&self, root: &Path, request: StartRequest) -> AppResult<serde_json::Value> {
        let prepared = self.prepare_start_request(root, request)?;
        let permit = match self.run_permit.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                let active = self.active_run_view();
                return Err(AppError::details(
                    "RUN_BUSY",
                    "Another command is already running",
                    json!({
                        "retryable": true,
                        "active_run_limit": 1,
                        "active_run": active,
                        "suggested_action": "Poll bash_status with the active run_id, or cancel it before starting a different command."
                    }),
                ));
            }
        };
        self.start_prepared(prepared, Some(permit)).await
    }

    pub(crate) async fn queue(
        &self,
        root: &Path,
        request: StartRequest,
    ) -> AppResult<serde_json::Value> {
        let mut prepared = self.prepare_start_request(root, request)?;
        prepared.background = true;
        self.start_prepared(prepared, None).await
    }

    async fn start_prepared(
        &self,
        prepared: PreparedStartRequest,
        immediate_permit: Option<OwnedSemaphorePermit>,
    ) -> AppResult<serde_json::Value> {
        let PreparedStartRequest {
            launcher,
            command,
            cwd,
            background,
            timeout_ms,
        } = prepared;
        let queued = immediate_permit.is_none();
        let run_id = format!("run_{}", Uuid::new_v4().simple());
        let sequence = self.next_run_sequence.fetch_add(1, Ordering::Relaxed);
        let record = Arc::new(Mutex::new(RunRecord::queued(
            run_id.clone(),
            sequence,
            command.clone(),
            cwd.clone(),
            self.policy.bash.max_output_chars,
        )));
        self.runs
            .insert(run_id.clone(), record.clone(), MAX_RETAINED_RUNS);

        // Execution always runs on a detached task, even for foreground calls.
        // If the client aborts the request, the command keeps running and owns
        // the permit until completion. Queued internal runs wait for that permit
        // inside their detached task, so they are immediately pollable without
        // violating the single-slot execution order. The guard is moved in
        // before the task first runs, so even a never-polled task ends its run.
        let semaphore = self.run_permit.clone();
        let guard = AbandonGuard(record);
        let mut handle = tokio::spawn(async move {
            let record = &guard.0;
            let _run_permit = match immediate_permit {
                Some(permit) => permit,
                None => match semaphore.acquire_owned().await {
                    Ok(permit) => permit,
                    Err(_) => {
                        let error = AppError::new(
                            "BASH_QUEUE_CLOSED",
                            "Bash execution queue is unavailable",
                        );
                        finalize_run_error(record, &error);
                        return Err(error);
                    }
                },
            };
            let request = ExecutionRequest {
                command,
                cwd,
                timeout_ms,
            };
            let result = execute(record, launcher.as_ref(), request).await;
            if let Err(error) = &result {
                finalize_run_error(record, error);
            }
            result
        });

        if queued {
            let mut result = self.status(&run_id)?;
            result["background"] = Value::Bool(true);
            result["queued"] = Value::Bool(true);
            result["guidance"] = Value::String(
                "Command is queued behind the active run; poll bash_status with this run_id."
                    .to_owned(),
            );
            return Ok(result);
        }

        if background {
            let mut result = self.status(&run_id)?;
            result["background"] = Value::Bool(true);
            return Ok(result);
        }

        // Foreground: return as soon as the command finishes, but never block
        // the MCP request past the foreground budget. Exceeding it does NOT
        // kill the command — the detached task keeps running and the client is
        // told to poll. A budget of 0 disables auto-promotion.
        // Only race the budget when the command could plausibly outlast it.
        // If its own timeout is within the budget it will finish (or self-kill)
        // in time, so we await it fully and avoid a promotion/completion race.
        let budget_ms = self.policy.bash.foreground_budget_ms;
        let outcome = if budget_ms == 0 || timeout_ms <= budget_ms {
            Some(handle.await)
        } else {
            timeout(Duration::from_millis(budget_ms), &mut handle)
                .await
                .ok()
        };

        match outcome {
            Some(Ok(result)) => {
                result?;
                self.status(&run_id)
            }
            Some(Err(join_error)) => Err(AppError::details(
                "BASH_EXECUTION_FAILED",
                "Bash execution task terminated unexpectedly",
                json!({"run_id": run_id, "detail": join_error.to_string()}),
            )),
            None => {
                // Budget exceeded; command continues in the background.
                tracing::info!(
                    run_id = %run_id,
                    budget_ms,
                    "bash foreground run auto-promoted to background"
                );
                let mut view = self.status(&run_id)?;
                view["detached"] = Value::Bool(true);
                view["reason"] = Value::String("foreground_budget_exceeded".to_owned());
                view["poll_hint_ms"] = json!(3000);
                view["guidance"] = Value::String(
                    "Command is still running after the foreground budget; do not re-issue it. \
                     Poll bash_status with the returned run_id until status is terminal."
                        .to_owned(),
                );
                Ok(view)
            }
        }
    }

    /// Summary of the currently active run, attached to RUN_BUSY so the model
    /// can poll or cancel instead of retrying blindly.
    fn active_run_view(&self) -> Option<serde_json::Value> {
        self.runs
            .records()
            .into_iter()
            .filter_map(|record| {
                let record = record.lock();
                let priority = match record.status() {
                    RunStatus::Running | RunStatus::Cancelling => 0,
                    RunStatus::Queued => 1,
                    _ => return None,
                };
                let elapsed_ms = (Utc::now() - record.started_at()).num_milliseconds().max(0);
                Some((
                    priority,
                    record.sequence(),
                    json!({
                        "run_id": record.run_id(),
                        "status": record.status(),
                        "command": record.command(),
                        "elapsed_ms": elapsed_ms,
                        "status_fetch": {"kind": "bash_status", "value": record.run_id()}
                    }),
                ))
            })
            .min_by_key(|(priority, sequence, _)| (*priority, *sequence))
            .map(|(_, _, view)| view)
    }

    pub fn status(&self, run_id: &str) -> AppResult<serde_json::Value> {
        self.status_with_limit(run_id, self.policy.bash.max_output_chars)
    }

    pub fn status_with_limit(
        &self,
        run_id: &str,
        max_output: usize,
    ) -> AppResult<serde_json::Value> {
        let record = self.runs.get(run_id)?;
        let record = record.lock();
        let limit = max_output.min(self.policy.bash.max_output_chars);
        Ok(serde_json::to_value(view(&record, limit))?)
    }

    pub fn output_stream(
        &self,
        run_id: &str,
        continuation: Option<&str>,
        requested_stream: Option<&str>,
    ) -> AppResult<serde_json::Value> {
        let stream = OutputStream::parse(requested_stream)
            .map_err(|error| AppError::invalid(error.to_string()))?;
        let record = self.runs.get(run_id)?;
        let record = record.lock();
        let buffer = record.output().stream(stream);
        let dropped = buffer.dropped_chars();
        let from = continuation_offset(continuation, run_id, stream).unwrap_or(dropped);
        let page = buffer.page(from, self.policy.bash.max_output_chars);
        Ok(json!({
            "run_id": run_id,
            "status": record.status(),
            "exit_code": record.exit_code(),
            "stream": stream.as_str(),
            "output": page.text,
            "continuation": page.next.map(|offset| continuation_token(run_id, stream, offset)),
            "total_chars": buffer.total_chars(),
            "retained_chars": buffer.retained_chars(),
            "output_truncated": dropped > 0,
            "retention_policy": "tail",
            "dropped_prefix_chars": dropped,
            "retained_start_offset": dropped,
            "continuation_scope": "retained_buffer"
        }))
    }

    /// Request cancellation. Never blocks: the process group gets a stop request
    /// and a tracked escalation, and the execution task records the end.
    pub fn cancel(&self, run_id: &str) -> AppResult<serde_json::Value> {
        let record = self.runs.get(run_id)?;
        let group = {
            let mut item = record.lock();
            match item.request_cancel() {
                CancelRequest::AlreadyTerminal => {
                    return Ok(serde_json::to_value(view(
                        &item,
                        self.policy.bash.max_output_chars,
                    ))?);
                }
                CancelRequest::Requested(group) => group,
            }
        };
        if let Some(group) = group {
            group.terminate(TERMINATE_GRACE);
        }
        Ok(json!({"run_id": run_id, "status": RunStatus::Cancelling}))
    }

    /// The retained combined output, for `bash_log` fetches.
    pub fn read_output(&self, run_id: &str) -> AppResult<String> {
        let record = self.runs.get(run_id)?;
        let record = record.lock();
        Ok(record
            .output()
            .stream(OutputStream::Combined)
            .retained()
            .to_owned())
    }
}

/// The client view: retained combined output followed by supervisor notes,
/// clamped to `max_output` characters (the head for a success, else the tail).
fn view(record: &RunRecord, max_output: usize) -> BashRunView {
    let combined = record.output().stream(OutputStream::Combined);
    let mut output = combined.retained().to_owned();
    for note in record.notes() {
        if !output.is_empty() {
            output.push_str("\n\n");
        }
        output.push_str(note);
    }
    let clamped =
        clamp_chars(&output, max_output, record.status().keeps_output_tail()).map(str::to_owned);
    let output_truncated = record.output().truncated() || clamped.is_some();
    let run_id = record.run_id();
    BashRunView {
        run_id: run_id.to_owned(),
        status: record.status(),
        command: record.command().to_owned(),
        cwd: record.cwd().to_string_lossy().into_owned(),
        started_at: record.started_at(),
        ended_at: record.ended_at(),
        exit_code: record.exit_code(),
        output: clamped.unwrap_or(output),
        output_truncated,
        retention_policy: "tail",
        dropped_prefix_chars: combined.dropped_chars(),
        retained_start_offset: combined.dropped_chars(),
        log_handle: format!("bash-log:{run_id}"),
        status_fetch: json!({"kind": "bash_status", "value": run_id}),
        pid: record.pid(),
    }
}

#[cfg(test)]
mod tests;
