mod execution;
mod readiness;

use crate::model::{AppError, AppResult, PolicyConfig};
use crate::process_runtime::{
    remove_logs, strip_ansi, terminate_process_tree, OutputStream, RunLogPaths, WarmShell,
    WindowsJob,
};
use crate::security::resolve_existing;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use execution::{execute, execute_warm, finalize_run_error};
use parking_lot::Mutex;
use readiness::resolve_bash;
pub use readiness::BashReadiness;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::{timeout, Duration};
use uuid::Uuid;

pub(crate) type RunCompletionObserver = Arc<dyn Fn(&str, DateTime<Utc>) + Send + Sync>;

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
    pub log_handle: String,
    pub status_fetch: serde_json::Value,
    pub pid: Option<u32>,
}

#[derive(Debug)]
pub(crate) struct RunRecord {
    run_id: String,
    sequence: u64,
    session_id: String,
    status: String,
    command: String,
    cwd: PathBuf,
    started_at: DateTime<Utc>,
    ended_at: Option<DateTime<Utc>>,
    exit_code: Option<i32>,
    pub(crate) output: String,
    pub(crate) output_truncated: bool,
    pub(crate) logs: RunLogPaths,
    pid: Option<u32>,
    cancel_requested: bool,
    job: Option<Arc<WindowsJob>>,
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

fn cleanup_orphan_logs(log_root: &Path, retention_hours: i64) {
    let Ok(entries) = fs::read_dir(log_root) else {
        return;
    };
    let max_age = std::time::Duration::from_secs((retention_hours as u64) * 60 * 60);
    for entry in entries.flatten() {
        let path = entry.path();
        let is_stale = entry
            .metadata()
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > max_age);
        if is_stale {
            let _ = fs::remove_file(path);
        }
    }
}

#[derive(Clone)]
pub struct BashSupervisor {
    runs: Arc<Mutex<HashMap<String, Arc<Mutex<RunRecord>>>>>,
    next_run_sequence: Arc<AtomicU64>,
    run_permit: Arc<Semaphore>,
    warm_shell: Arc<tokio::sync::Mutex<Option<WarmShell>>>,
    cache_root: PathBuf,
    policy: PolicyConfig,
    retention_hours: i64,
    readiness: BashReadiness,
    completion_observer: Arc<Mutex<Option<RunCompletionObserver>>>,
}

impl std::fmt::Debug for BashSupervisor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BashSupervisor")
            .field("runs", &self.runs)
            .field("cache_root", &self.cache_root)
            .field("policy", &self.policy)
            .field("retention_hours", &self.retention_hours)
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
    completion_observer: Option<RunCompletionObserver>,
}

const MAX_RETAINED_RUNS: usize = 256;

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
    pub fn new(cache_root: PathBuf, policy: PolicyConfig) -> AppResult<Self> {
        let log_root = cache_root.join("bash-logs");
        fs::create_dir_all(&log_root)?;
        let retention_hours = policy.bash.retention_hours;
        if retention_hours <= 0 {
            return Err(AppError::details(
                "INVALID_POLICY",
                "policy.bash.retentionHours must be greater than zero",
                json!({"retention_hours": retention_hours}),
            ));
        }
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
        cleanup_orphan_logs(&log_root, retention_hours);
        let readiness = resolve_bash(&policy);
        Ok(Self {
            runs: Arc::new(Mutex::new(HashMap::new())),
            next_run_sequence: Arc::new(AtomicU64::new(0)),
            run_permit: Arc::new(Semaphore::new(1)),
            warm_shell: Arc::new(tokio::sync::Mutex::new(None)),
            cache_root,
            policy,
            retention_hours,
            readiness,
            completion_observer: Arc::new(Mutex::new(None)),
        })
    }

    pub(crate) fn set_completion_observer(&self, observer: RunCompletionObserver) {
        *self.completion_observer.lock() = Some(observer);
    }

    pub fn readiness(&self) -> BashReadiness {
        self.readiness.clone()
    }

    pub fn ensure_available(&self) -> AppResult<()> {
        if !self.policy.bash.enabled {
            return Err(AppError::details(
                "BASH_DISABLED",
                "Bash execution is disabled by policy",
                json!({"configuration_hint": "Set policy.bash.enabled to true and restart CodeWeave."}),
            ));
        }
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

    pub fn retained_run_ids(&self) -> HashSet<String> {
        self.runs.lock().keys().cloned().collect()
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

    pub async fn start_for_session(
        &self,
        root: &Path,
        session_id: &str,
        request: StartRequest,
    ) -> AppResult<serde_json::Value> {
        let prepared = self.prepare_start_request(root, request)?;
        let permit = match self.run_permit.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                // The single run slot is busy. If the caller re-issued an
                // identical command against the same cwd (ChatGPT retries a
                // timed-out call verbatim), hand back the in-flight run so the
                // model polls it instead of flailing on RUN_BUSY.
                if let Some(active) =
                    self.active_run_matching(session_id, &prepared.command, &prepared.cwd)
                {
                    let mut view = self.status_for_session(session_id, &active)?;
                    view["deduplicated"] = Value::Bool(true);
                    view["guidance"] = Value::String(
                        "An identical command is already running; poll bash_status with this run_id."
                            .to_owned(),
                    );
                    return Ok(view);
                }
                let active = self.active_run_view_for_session(session_id);
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
        self.start_prepared_for_session(session_id, prepared, Some(permit))
            .await
    }

    pub(crate) async fn queue_for_session(
        &self,
        root: &Path,
        session_id: &str,
        request: StartRequest,
    ) -> AppResult<serde_json::Value> {
        let mut prepared = self.prepare_start_request(root, request)?;
        prepared.background = true;
        self.start_prepared_for_session(session_id, prepared, None)
            .await
    }

    async fn start_prepared_for_session(
        &self,
        session_id: &str,
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
        let log_root = self.cache_root.join("bash-logs");
        let logs = RunLogPaths::new(&log_root, &run_id);
        let record = Arc::new(Mutex::new(RunRecord {
            run_id: run_id.clone(),
            sequence,
            session_id: session_id.to_owned(),
            status: "queued".to_owned(),
            command: command.clone(),
            cwd: cwd.clone(),
            started_at: Utc::now(),
            ended_at: None,
            exit_code: None,
            output: String::new(),
            output_truncated: false,
            logs,
            pid: None,
            cancel_requested: false,
            job: None,
        }));
        self.runs.lock().insert(run_id.clone(), record.clone());
        self.trim_runs();
        let bash_executable = self
            .readiness
            .executable()
            .expect("prepare_start_request guarantees a resolved executable");
        let max_output = self.policy.bash.max_output_chars;
        let completion_observer = self.completion_observer.lock().clone();

        // Execution always runs on a detached task, even for foreground calls.
        // If the client aborts the request, the command keeps running and owns
        // the permit until completion. Queued internal runs wait for that permit
        // inside their detached task, so they are immediately pollable without
        // violating the single-slot execution order.
        let warm_shell = self.warm_shell.clone();
        let semaphore = self.run_permit.clone();
        let execution_record = record.clone();
        let execution_completion_observer = completion_observer.clone();
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
                        finalize_run_error(
                            &execution_record,
                            &error,
                            execution_completion_observer.as_ref(),
                        );
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
                completion_observer: execution_completion_observer.clone(),
            };
            let result = if background {
                execute(record, request).await
            } else {
                execute_warm(warm_shell, record, request).await
            };
            if let Err(error) = &result {
                finalize_run_error(
                    &execution_record,
                    error,
                    execution_completion_observer.as_ref(),
                );
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

    /// Dedupe key for retried commands: an *active* run with the same command
    /// text and working directory. Single-slot semantics make an identical
    /// concurrent command an almost-certain client retry.
    fn active_run_matching(&self, session_id: &str, command: &str, cwd: &Path) -> Option<String> {
        self.runs.lock().values().find_map(|record| {
            let record = record.lock();
            (record.ended_at.is_none()
                && record.session_id == session_id
                && record.command == command
                && record.cwd == cwd)
                .then(|| record.run_id.clone())
        })
    }

    /// Summary of the currently active run, attached to RUN_BUSY so the model
    /// can poll or cancel instead of retrying blindly.
    fn active_run_view_for_session(&self, session_id: &str) -> Option<serde_json::Value> {
        let records = self.runs.lock().values().cloned().collect::<Vec<_>>();
        records
            .into_iter()
            .filter_map(|record| {
                let record = record.lock();
                if record.ended_at.is_some() || record.session_id != session_id {
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

    pub fn status_for_session(
        &self,
        session_id: &str,
        run_id: &str,
    ) -> AppResult<serde_json::Value> {
        self.status_with_limit_for_session(session_id, run_id, self.policy.bash.max_output_chars)
    }

    pub fn status_with_limit(
        &self,
        run_id: &str,
        max_output: usize,
    ) -> AppResult<serde_json::Value> {
        self.status_record(run_id, None, max_output)
    }

    pub fn status_with_limit_for_session(
        &self,
        session_id: &str,
        run_id: &str,
        max_output: usize,
    ) -> AppResult<serde_json::Value> {
        self.status_record(run_id, Some(session_id), max_output)
    }

    fn status_record(
        &self,
        run_id: &str,
        session_id: Option<&str>,
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
        if session_id.is_some_and(|session_id| session_id != record.session_id) {
            return Err(AppError::details(
                "RUN_NOT_FOUND",
                "Bash run not found in this session",
                json!({"run_id": run_id}),
            ));
        }
        Ok(serde_json::to_value(view(&record, max_output))?)
    }

    pub fn output_stream_for_session(
        &self,
        session_id: &str,
        run_id: &str,
        continuation: Option<&str>,
        requested_stream: Option<&str>,
    ) -> AppResult<serde_json::Value> {
        self.output_stream_for_owner(run_id, Some(session_id), continuation, requested_stream)
    }

    fn output_stream_for_owner(
        &self,
        run_id: &str,
        session_id: Option<&str>,
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
        if session_id.is_some_and(|session_id| session_id != record.session_id) {
            return Err(AppError::details(
                "RUN_NOT_FOUND",
                "Bash run not found in this session",
                json!({"run_id": run_id}),
            ));
        }
        let full = fs::read(record.logs.path(stream))
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_else(|_| {
                if stream == OutputStream::Combined {
                    record.output.clone()
                } else {
                    String::new()
                }
            });
        let requested_offset = continuation_offset(continuation, run_id, stream)
            .unwrap_or(0)
            .min(full.len());
        let offset = char_boundary(&full, requested_offset);
        let end = char_boundary(
            &full,
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
            "total_chars": full.len()
        }))
    }

    pub fn cancel_for_session(
        &self,
        session_id: &str,
        run_id: &str,
    ) -> AppResult<serde_json::Value> {
        self.cancel_for_owner(run_id, Some(session_id))
    }

    fn cancel_for_owner(
        &self,
        run_id: &str,
        session_id: Option<&str>,
    ) -> AppResult<serde_json::Value> {
        let record = self.runs.lock().get(run_id).cloned().ok_or_else(|| {
            AppError::details(
                "RUN_NOT_FOUND",
                "Bash run not found",
                json!({"run_id": run_id}),
            )
        })?;
        let (pid, job) = {
            let mut item = record.lock();
            if session_id.is_some_and(|session_id| session_id != item.session_id) {
                return Err(AppError::details(
                    "RUN_NOT_FOUND",
                    "Bash run not found in this session",
                    json!({"run_id": run_id}),
                ));
            }
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

    pub fn read_log_for_session(&self, session_id: &str, run_id: &str) -> AppResult<String> {
        self.read_log_for_owner(run_id, Some(session_id))
    }

    fn read_log_for_owner(&self, run_id: &str, session_id: Option<&str>) -> AppResult<String> {
        let record = self
            .runs
            .lock()
            .get(run_id)
            .cloned()
            .ok_or_else(|| AppError::new("RUN_NOT_FOUND", "Bash run not found"))?;
        let record = record.lock();
        if session_id.is_some_and(|session_id| session_id != record.session_id) {
            return Err(AppError::new(
                "RUN_NOT_FOUND",
                "Bash run not found in this session",
            ));
        }
        Ok(fs::read(&record.logs.combined)
            .map(|bytes| strip_ansi(&String::from_utf8_lossy(&bytes)))
            .unwrap_or_else(|_| record.output.clone()))
    }

    fn trim_runs(&self) {
        let retention_hours = self.retention_hours;
        let cutoff = Utc::now() - ChronoDuration::hours(retention_hours);
        let mut removed_logs = Vec::new();
        {
            let mut runs = self.runs.lock();
            let expired: Vec<String> = runs
                .iter()
                .filter_map(|(run_id, record)| {
                    let record = record.lock();
                    record
                        .ended_at
                        .is_some_and(|ended| ended < cutoff)
                        .then(|| run_id.clone())
                })
                .collect();
            for run_id in expired {
                if let Some(record) = runs.remove(&run_id) {
                    removed_logs.push(record.lock().logs.clone());
                }
            }

            if runs.len() > MAX_RETAINED_RUNS {
                let mut completed: Vec<_> = runs
                    .iter()
                    .filter_map(|(run_id, record)| {
                        let record = record.lock();
                        record.ended_at.map(|ended| (run_id.clone(), ended))
                    })
                    .collect();
                completed.sort_by_key(|(_, ended)| *ended);
                let remove_count = runs.len().saturating_sub(MAX_RETAINED_RUNS);
                for (run_id, _) in completed.into_iter().take(remove_count) {
                    if let Some(record) = runs.remove(&run_id) {
                        removed_logs.push(record.lock().logs.clone());
                    }
                }
            }
        }
        for logs in removed_logs {
            remove_logs(&logs);
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
