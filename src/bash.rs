mod execution;
mod readiness;

use crate::model::{AppError, AppResult, PolicyConfig};
use crate::process_runtime::{strip_ansi, terminate_process_tree, OutputStream, WindowsJob};
use crate::security::resolve_existing;
use chrono::{DateTime, Utc};
use execution::{execute, finalize_run_error};
use parking_lot::Mutex;
use readiness::resolve_bash;
pub use readiness::BashReadiness;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
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
    pub status: String,
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

#[derive(Debug)]
pub(crate) struct RunRecord {
    run_id: String,
    sequence: u64,
    status: String,
    command: String,
    cwd: PathBuf,
    started_at: DateTime<Utc>,
    ended_at: Option<DateTime<Utc>>,
    exit_code: Option<i32>,
    pub(crate) output: String,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
    pub(crate) combined: String,
    pub(crate) output_truncated: bool,
    pub(crate) stdout_dropped_chars: usize,
    pub(crate) stderr_dropped_chars: usize,
    pub(crate) combined_dropped_chars: usize,
    pid: Option<u32>,
    cancel_requested: bool,
    job: Option<Arc<WindowsJob>>,
    baseline_generation: Option<u64>,
    baseline_dirty: HashSet<String>,
    frozen_changes: Option<(u64, HashSet<String>)>,
}

struct ExecutionGuard {
    record: Arc<Mutex<RunRecord>>,
    pid: Option<u32>,
    job: Option<Arc<WindowsJob>>,
    armed: bool,
}

impl ExecutionGuard {
    fn new(record: Arc<Mutex<RunRecord>>, pid: Option<u32>, job: Option<Arc<WindowsJob>>) -> Self {
        Self {
            record,
            pid,
            job,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ExecutionGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Some(pid) = self.pid {
            terminate_process_tree(pid, self.job.as_deref());
        }
        let mut item = self.record.lock();
        if item.ended_at.is_none() {
            item.status = "cancelled".to_owned();
            item.ended_at = Some(Utc::now());
            item.pid = None;
            item.job = None;
            if !item.output.is_empty() {
                item.output.push('\n');
            }
            item.output
                .push_str("Bash run was abandoned; the process tree was terminated.");
        }
    }
}

#[derive(Clone)]
pub struct BashSupervisor {
    runs: Arc<Mutex<HashMap<String, Arc<Mutex<RunRecord>>>>>,
    next_run_sequence: Arc<AtomicU64>,
    run_permit: Arc<Semaphore>,
    policy: PolicyConfig,
    readiness: BashReadiness,
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

struct ExecutionRequest {
    bash_executable: String,
    command: String,
    cwd: PathBuf,
    timeout_ms: u64,
    max_output: usize,
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
    command: String,
    cwd: PathBuf,
    background: bool,
    timeout_ms: u64,
}

impl BashSupervisor {
    pub fn new(_cache_root: PathBuf, policy: PolicyConfig) -> AppResult<Self> {
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
        let readiness = resolve_bash(&policy);
        Ok(Self {
            runs: Arc::new(Mutex::new(HashMap::new())),
            next_run_sequence: Arc::new(AtomicU64::new(0)),
            run_permit: Arc::new(Semaphore::new(1)),
            policy,
            readiness,
        })
    }

    pub fn readiness(&self) -> BashReadiness {
        self.readiness.clone()
    }

    pub fn ensure_available(&self) -> AppResult<()> {
        if self.readiness.is_ready() {
            return Ok(());
        }
        Err(AppError::details(
            "BASH_UNAVAILABLE",
            self.readiness
                .failure_reason
                .clone()
                .unwrap_or_else(|| "No usable Bash implementation found".to_owned()),
            json!({"execution": self.readiness()}),
        ))
    }

    pub fn running_count(&self) -> usize {
        self.runs
            .lock()
            .values()
            .filter(|record| record.lock().ended_at.is_none())
            .count()
    }

    pub(crate) fn set_change_baseline(
        &self,
        run_id: &str,
        generation: u64,
        dirty_files: HashSet<String>,
    ) {
        if let Some(record) = self.runs.lock().get(run_id).cloned() {
            let mut record = record.lock();
            record.baseline_generation = Some(generation);
            record.baseline_dirty = dirty_files;
        }
    }

    pub(crate) fn observe_changes<F>(
        &self,
        run_id: &str,
        current_generation: u64,
        current_dirty: HashSet<String>,
        terminal: bool,
        calculate: F,
    ) -> AppResult<(u64, u64, HashSet<String>)>
    where
        F: FnOnce(
            u64,
            &HashSet<String>,
            Option<&DateTime<Utc>>,
            &HashSet<String>,
        ) -> HashSet<String>,
    {
        let record = self.runs.lock().get(run_id).cloned().ok_or_else(|| {
            AppError::details(
                "RUN_NOT_FOUND",
                "Bash run not found",
                json!({"run_id": run_id}),
            )
        })?;
        let mut record = record.lock();
        if record.baseline_generation.is_none() {
            record.baseline_generation = Some(current_generation);
            record.baseline_dirty = current_dirty.clone();
        }
        let start = record.baseline_generation.unwrap_or(current_generation);
        if let Some((generation, paths)) = &record.frozen_changes {
            return Ok((start, *generation, paths.clone()));
        }
        let changed = calculate(
            start,
            &record.baseline_dirty,
            if terminal {
                record.ended_at.as_ref()
            } else {
                None
            },
            &current_dirty,
        );
        if terminal {
            record.frozen_changes = Some((current_generation, changed.clone()));
        }
        Ok((start, current_generation, changed))
    }

    fn prepare_start_request(
        &self,
        root: &Path,
        request: StartRequest,
    ) -> AppResult<PreparedStartRequest> {
        self.ensure_available()?;
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
            command,
            cwd,
            background,
            timeout_ms,
        } = prepared;
        let queued = immediate_permit.is_none();
        let run_id = format!("run_{}", Uuid::new_v4().simple());
        let sequence = self.next_run_sequence.fetch_add(1, Ordering::Relaxed);
        let record = Arc::new(Mutex::new(RunRecord {
            run_id: run_id.clone(),
            sequence,
            status: "queued".to_owned(),
            command: command.clone(),
            cwd: cwd.clone(),
            started_at: Utc::now(),
            ended_at: None,
            exit_code: None,
            output: String::new(),
            stdout: String::new(),
            stderr: String::new(),
            combined: String::new(),
            output_truncated: false,
            stdout_dropped_chars: 0,
            stderr_dropped_chars: 0,
            combined_dropped_chars: 0,
            pid: None,
            cancel_requested: false,
            job: None,
            baseline_generation: None,
            baseline_dirty: HashSet::new(),
            frozen_changes: None,
        }));
        self.runs.lock().insert(run_id.clone(), record.clone());
        self.trim_runs();
        let bash_executable = self
            .readiness
            .executable()
            .expect("prepare_start_request guarantees a resolved executable");
        let max_output = self.policy.bash.max_output_chars;

        // Execution always runs on a detached task, even for foreground calls.
        // If the client aborts the request, the command keeps running and owns
        // the permit until completion. Queued internal runs wait for that permit
        // inside their detached task, so they are immediately pollable without
        // violating the single-slot execution order.
        let semaphore = self.run_permit.clone();
        let execution_record = record.clone();
        let mut handle = tokio::spawn(async move {
            let permit = match immediate_permit {
                Some(permit) => permit,
                None => match semaphore.acquire_owned().await {
                    Ok(permit) => permit,
                    Err(_) => {
                        let error = AppError::new(
                            "BASH_QUEUE_CLOSED",
                            "Bash execution queue is unavailable",
                        );
                        finalize_run_error(&execution_record, &error);
                        return Err(error);
                    }
                },
            };
            let _run_permit = permit;
            let request = ExecutionRequest {
                bash_executable,
                command,
                cwd,
                timeout_ms,
                max_output,
            };
            let result = execute(record, request).await;
            if let Err(error) = &result {
                finalize_run_error(&execution_record, error);
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
                eprintln!(
                    "{}",
                    json!({
                        "event": "bash_foreground_auto_promoted",
                        "run_id": run_id,
                        "budget_ms": budget_ms
                    })
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
        let records = self.runs.lock().values().cloned().collect::<Vec<_>>();
        records
            .into_iter()
            .filter_map(|record| {
                let record = record.lock();
                if record.ended_at.is_some() {
                    return None;
                }
                let priority = match record.status.as_str() {
                    "running" | "cancelling" => 0,
                    "queued" => 1,
                    _ => 2,
                };
                let elapsed_ms = (Utc::now() - record.started_at).num_milliseconds().max(0);
                Some((
                    priority,
                    record.sequence,
                    json!({
                        "run_id": record.run_id,
                        "status": record.status,
                        "command": record.command,
                        "elapsed_ms": elapsed_ms,
                        "status_fetch": {"kind": "bash_status", "value": record.run_id}
                    }),
                ))
            })
            .min_by_key(|(priority, started_at, _)| (*priority, *started_at))
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
        let record = self.runs.lock().get(run_id).cloned().ok_or_else(|| {
            AppError::details(
                "RUN_NOT_FOUND",
                "Bash run not found",
                json!({"run_id": run_id}),
            )
        })?;
        let record = record.lock();
        Ok(serde_json::to_value(view(&record, max_output))?)
    }

    pub fn output_stream(
        &self,
        run_id: &str,
        continuation: Option<&str>,
        requested_stream: Option<&str>,
    ) -> AppResult<serde_json::Value> {
        let stream = OutputStream::parse(requested_stream)
            .map_err(|error| AppError::invalid(error.to_string()))?;
        let record = self.runs.lock().get(run_id).cloned().ok_or_else(|| {
            AppError::details(
                "RUN_NOT_FOUND",
                "Bash run not found",
                json!({"run_id": run_id}),
            )
        })?;
        let record = record.lock();
        let full = match stream {
            OutputStream::Stdout => &record.stdout,
            OutputStream::Stderr => &record.stderr,
            OutputStream::Combined => &record.combined,
        };
        let dropped_prefix_chars = match stream {
            OutputStream::Stdout => record.stdout_dropped_chars,
            OutputStream::Stderr => record.stderr_dropped_chars,
            OutputStream::Combined => record.combined_dropped_chars,
        };
        let requested_offset = continuation_offset(continuation, run_id, stream)
            .unwrap_or(0)
            .min(full.len());
        let offset = char_boundary(full, requested_offset);
        let end = char_boundary(
            full,
            (offset + self.policy.bash.max_output_chars).min(full.len()),
        );
        let next = (end < full.len()).then(|| format!("bash:{run_id}:{}:{end}", stream.as_str()));
        Ok(json!({
            "run_id": run_id,
            "status": record.status,
            "exit_code": record.exit_code,
            "stream": stream.as_str(),
            "output": strip_ansi(&full[offset..end]),
            "continuation": next,
            "total_chars": dropped_prefix_chars + full.chars().count(),
            "retained_chars": full.chars().count(),
            "output_truncated": dropped_prefix_chars > 0,
            "retention_policy": "tail",
            "dropped_prefix_chars": dropped_prefix_chars,
            "retained_start_offset": dropped_prefix_chars,
            "continuation_scope": "retained_buffer"
        }))
    }

    pub fn cancel(&self, run_id: &str) -> AppResult<serde_json::Value> {
        let record = self.runs.lock().get(run_id).cloned().ok_or_else(|| {
            AppError::details(
                "RUN_NOT_FOUND",
                "Bash run not found",
                json!({"run_id": run_id}),
            )
        })?;
        let (pid, job) = {
            let mut item = record.lock();
            if item.ended_at.is_some() {
                return Ok(serde_json::to_value(view(
                    &item,
                    self.policy.bash.max_output_chars,
                ))?);
            }
            item.status = "cancelling".to_owned();
            item.cancel_requested = true;
            (item.pid, item.job.clone())
        };
        if let Some(pid) = pid {
            terminate_process_tree(pid, job.as_deref());
        }
        Ok(json!({"run_id": run_id, "status": "cancelling"}))
    }

    pub fn read_output(&self, run_id: &str) -> AppResult<String> {
        let record = self
            .runs
            .lock()
            .get(run_id)
            .cloned()
            .ok_or_else(|| AppError::new("RUN_NOT_FOUND", "Bash run not found"))?;
        let record = record.lock();
        Ok(strip_ansi(&record.combined))
    }

    fn trim_runs(&self) {
        let mut runs = self.runs.lock();
        if runs.len() > MAX_RETAINED_RUNS {
            let mut completed: Vec<_> = runs
                .iter()
                .filter_map(|(run_id, record)| {
                    record.lock().ended_at.map(|ended| (run_id.clone(), ended))
                })
                .collect();
            completed.sort_by_key(|(_, ended)| *ended);
            let remove_count = runs.len().saturating_sub(MAX_RETAINED_RUNS);
            for (run_id, _) in completed.into_iter().take(remove_count) {
                runs.remove(&run_id);
            }
        }
    }
}

fn view(record: &RunRecord, max_output: usize) -> BashRunView {
    let failed = matches!(record.status.as_str(), "failed" | "timed_out" | "cancelled");
    let output = if record.output.chars().count() > max_output {
        if failed || record.ended_at.is_none() {
            tail_chars(&record.output, max_output)
        } else {
            head_chars(&record.output, max_output)
        }
    } else {
        record.output.clone()
    };
    BashRunView {
        run_id: record.run_id.clone(),
        status: record.status.clone(),
        command: record.command.clone(),
        cwd: record.cwd.to_string_lossy().into_owned(),
        started_at: record.started_at,
        ended_at: record.ended_at,
        exit_code: record.exit_code,
        output,
        output_truncated: record.output_truncated || record.output.chars().count() > max_output,
        retention_policy: "tail",
        dropped_prefix_chars: record.combined_dropped_chars,
        retained_start_offset: record.combined_dropped_chars,
        log_handle: format!("bash-log:{}", record.run_id),
        status_fetch: json!({"kind": "bash_status", "value": record.run_id}),
        pid: record.pid,
    }
}

fn continuation_offset(
    continuation: Option<&str>,
    run_id: &str,
    stream: OutputStream,
) -> Option<usize> {
    let value = continuation?;
    let prefix = format!("bash:{run_id}:{}:", stream.as_str());
    if let Some(offset) = value.strip_prefix(&prefix) {
        return offset.parse::<usize>().ok();
    }
    if stream == OutputStream::Combined {
        return value
            .strip_prefix(&format!("bash:{run_id}:"))
            .and_then(|offset| offset.parse::<usize>().ok());
    }
    None
}

fn head_chars(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}

fn tail_chars(value: &str, limit: usize) -> String {
    let count = value.chars().count();
    value.chars().skip(count.saturating_sub(limit)).collect()
}

fn char_boundary(value: &str, mut index: usize) -> usize {
    while index > 0 && !value.is_char_boundary(index) {
        index -= 1;
    }
    index
}

#[cfg(test)]
mod tests;
