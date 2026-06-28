use crate::model::{AppError, AppResult, OutputFilter, PolicyConfig};
use crate::process_runtime::{
    remove_logs, render_preview, stream_output, strip_ansi, terminate_process_tree, OutputStream,
    RunLogPaths, WarmShell, WindowsJob,
};
use crate::security::resolve_existing;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use parking_lot::Mutex;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fs;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::Semaphore;
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
    pub log_handle: String,
    pub status_fetch: serde_json::Value,
    pub pid: Option<u32>,
}

#[derive(Debug)]
pub(crate) struct RunRecord {
    run_id: String,
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

#[derive(Debug, Clone)]
pub struct BashSupervisor {
    runs: Arc<Mutex<HashMap<String, Arc<Mutex<RunRecord>>>>>,
    run_permit: Arc<Semaphore>,
    warm_shell: Arc<tokio::sync::Mutex<Option<WarmShell>>>,
    cache_root: PathBuf,
    policy: PolicyConfig,
    retention_hours: i64,
}

const MAX_RETAINED_RUNS: usize = 256;
const OUTPUT_DRAIN_TIMEOUT_SECS: u64 = 10;

#[derive(Debug, Clone)]
pub struct StartRequest {
    pub command: String,
    pub cwd: Option<String>,
    pub background: Option<bool>,
    pub timeout_ms: Option<u64>,
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
        Ok(Self {
            runs: Arc::new(Mutex::new(HashMap::new())),
            run_permit: Arc::new(Semaphore::new(1)),
            warm_shell: Arc::new(tokio::sync::Mutex::new(None)),
            cache_root,
            policy,
            retention_hours,
        })
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

    pub async fn start(&self, root: &Path, request: StartRequest) -> AppResult<serde_json::Value> {
        if !self.policy.bash.enabled {
            return Err(AppError::details(
                "BASH_DISABLED",
                "Bash execution is disabled by policy",
                json!({"configuration_hint": "Set policy.bash.enabled to true and restart CodeWeave."}),
            ));
        }
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
        let run_permit = self
            .run_permit
            .clone()
            .try_acquire_owned()
            .map_err(|_| {
                AppError::details(
                    "RUN_BUSY",
                    "Another command is already running",
                    json!({
                        "retryable": true,
                        "active_run_limit": 1,
                        "suggested_action": "Wait for the active command to finish or cancel it before starting another command."
                    }),
                )
            })?;
        let run_id = format!("run_{}", Uuid::new_v4().simple());
        let log_root = self.cache_root.join("bash-logs");
        let logs = RunLogPaths::new(&log_root, &run_id);
        let record = Arc::new(Mutex::new(RunRecord {
            run_id: run_id.clone(),
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
        let background = request.background.unwrap_or(false);
        let bash_executable = self.policy.bash.executable.clone();
        let max_output = self.policy.bash.max_output_chars;

        if background {
            let execution_record = record.clone();
            let runner = async move {
                let result = execute(
                    record,
                    bash_executable,
                    command,
                    cwd,
                    timeout_ms,
                    max_output,
                )
                .await;
                if let Err(error) = &result {
                    finalize_run_error(&execution_record, error);
                }
                result
            };
            tokio::spawn(async move {
                let _run_permit = run_permit;
                let _ = runner.await;
            });
            let mut result = self.status(&run_id)?;
            result["background"] = Value::Bool(true);
            Ok(result)
        } else {
            let _run_permit = run_permit;
            let warm_shell = self.warm_shell.clone();
            let execution_record = record.clone();
            let result = execute_warm(
                warm_shell,
                record,
                bash_executable,
                command,
                cwd,
                timeout_ms,
                max_output,
            )
            .await;
            if let Err(error) = &result {
                finalize_run_error(&execution_record, error);
            }
            result?;
            self.status(&run_id)
        }
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

    pub fn read_log(&self, run_id: &str) -> AppResult<String> {
        let record = self
            .runs
            .lock()
            .get(run_id)
            .cloned()
            .ok_or_else(|| AppError::new("RUN_NOT_FOUND", "Bash run not found"))?;
        let record = record.lock();
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

    pub fn recent_failures(&self, query: &str, limit: usize) -> Vec<serde_json::Value> {
        let terms: Vec<String> = query
            .split(|ch: char| !ch.is_alphanumeric() && ch != '_')
            .filter(|term| term.len() > 2)
            .map(|term| term.to_ascii_lowercase())
            .collect();
        let mut keyed: Vec<_> = self
            .runs
            .lock()
            .values()
            .map(|record| {
                let key = {
                    let guard = record.lock();
                    guard.ended_at.unwrap_or(guard.started_at)
                };
                (key, record.clone())
            })
            .collect();
        keyed.sort_by(|(left, _), (right, _)| right.cmp(left));
        let mut output = Vec::new();
        let mut superseded_commands = HashSet::new();
        for (_, record) in keyed {
            let record = record.lock();
            let command_key = format!("{}\0{}", record.cwd.to_string_lossy(), record.command);
            if record.status == "succeeded" {
                superseded_commands.insert(command_key);
                continue;
            }
            if !matches!(record.status.as_str(), "failed" | "timed_out") {
                continue;
            }
            if superseded_commands.contains(&command_key) {
                continue;
            }
            let lower = record.output.to_ascii_lowercase();
            if !terms.is_empty() && !terms.iter().any(|term| lower.contains(term)) {
                continue;
            }
            output.push(json!({
                "run_id": record.run_id,
                "status": record.status,
                "command": record.command,
                "output": tail_chars(&record.output, 4_000),
                "log_handle": format!("bash-log:{}", record.run_id),
                "reason_codes": ["recent_bash_failure"]
            }));
            if output.len() >= limit {
                break;
            }
        }
        output
    }
}

fn finalize_cancelled_before_start(record: &Arc<Mutex<RunRecord>>) -> bool {
    let mut item = record.lock();
    if !item.cancel_requested {
        return false;
    }
    item.status = "cancelled".to_owned();
    item.ended_at = Some(Utc::now());
    item.pid = None;
    item.job = None;
    if !item.output.is_empty() {
        item.output.push('\n');
    }
    item.output
        .push_str("Bash run was cancelled before the process started.");
    true
}

async fn execute_warm(
    warm_shell: Arc<tokio::sync::Mutex<Option<WarmShell>>>,
    record: Arc<Mutex<RunRecord>>,
    bash_executable: String,
    command: String,
    cwd: PathBuf,
    timeout_ms: u64,
    max_output: usize,
) -> AppResult<()> {
    if finalize_cancelled_before_start(&record) {
        return Ok(());
    }

    {
        let mut item = record.lock();
        item.status = "running".to_owned();
        item.started_at = Utc::now();
    }

    let mut guard = warm_shell.lock().await;
    if guard.is_none() {
        *guard = Some(WarmShell::spawn(&bash_executable).map_err(|error| {
            AppError::details(
                "BASH_START_FAILED",
                error.to_string(),
                json!({"bash_executable": bash_executable}),
            )
        })?);
    }

    let paths = record.lock().logs.clone();
    let outcome = match guard
        .as_mut()
        .unwrap()
        .run(&command, &cwd, timeout_ms, &paths)
        .await
    {
        Ok(outcome) => outcome,
        Err(error) => {
            *guard = None;
            return Err(AppError::details(
                "BASH_RUN_FAILED",
                error.to_string(),
                json!({"command": command}),
            ));
        }
    };

    if outcome.needs_respawn {
        *guard = None;
    }

    let (mut display_output, mut output_truncated) = render_preview(
        &paths,
        &OutputFilter::Raw,
        outcome.status,
        max_output,
        outcome.limited,
    )
    .await;

    if outcome.status == "timed_out" {
        append_note(
            &mut display_output,
            &format!("Bash run exceeded timeout of {timeout_ms} ms; partial output was retained."),
        );
    }

    if display_output.chars().count() > max_output {
        display_output = if matches!(outcome.status, "failed" | "timed_out" | "cancelled") {
            tail_chars(&display_output, max_output)
        } else {
            head_chars(&display_output, max_output)
        };
        output_truncated = true;
    }

    let mut item = record.lock();
    item.status = outcome.status.to_owned();
    item.exit_code = outcome.exit_code;
    item.ended_at = Some(Utc::now());
    item.output = display_output;
    item.output_truncated = output_truncated;
    Ok(())
}

async fn execute(
    record: Arc<Mutex<RunRecord>>,
    bash_executable: String,
    command: String,
    cwd: PathBuf,
    timeout_ms: u64,
    max_output: usize,
) -> AppResult<()> {
    if finalize_cancelled_before_start(&record) {
        return Ok(());
    }

    let mut process = Command::new(&bash_executable);
    process.args(["-c", &command]);
    process
        .current_dir(&cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    process.as_std_mut().process_group(0);
    let mut child = process.spawn().map_err(|error| {
        AppError::details(
            "BASH_START_FAILED",
            error.to_string(),
            json!({"bash_executable": bash_executable, "command": command, "cwd": cwd}),
        )
    })?;
    let pid = child.id();

    #[cfg(windows)]
    let (job, setup_warning) = match pid {
        Some(pid) => match WindowsJob::assign(pid) {
            Ok(job) => (Some(job), None),
            Err(error) => (
                None,
                Some(format!(
                    "CodeWeave warning: Windows Job Object assignment failed; falling back to taskkill for cancellation: {error}"
                )),
            ),
        },
        None => (None, Some("CodeWeave warning: spawned task has no process id".to_owned())),
    };
    #[cfg(not(windows))]
    let (job, setup_warning): (Option<Arc<WindowsJob>>, Option<String>) = (None, None);

    let cancel_after_spawn = {
        let mut item = record.lock();
        let cancelled = item.cancel_requested;
        item.status = if cancelled {
            "cancelling".to_owned()
        } else {
            "running".to_owned()
        };
        item.started_at = Utc::now();
        item.pid = pid;
        item.job = job.clone();
        cancelled
    };
    let mut execution_guard = ExecutionGuard::new(record.clone(), pid, job.clone());
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| AppError::new("BASH_PIPE_FAILED", "Bash stdout pipe is unavailable"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| AppError::new("BASH_PIPE_FAILED", "Bash stderr pipe is unavailable"))?;
    let collector_record = record.clone();
    let mut collector =
        tokio::spawn(
            async move { stream_output(stdout, stderr, collector_record, max_output).await },
        );

    if cancel_after_spawn {
        if let Some(pid) = pid {
            let cancel_job = job.clone();
            let _ = tokio::task::spawn_blocking(move || {
                terminate_process_tree(pid, cancel_job.as_deref());
            })
            .await;
        }
    }
    let wait_duration = if cancel_after_spawn {
        Duration::from_secs(5)
    } else {
        Duration::from_millis(timeout_ms)
    };
    let wait_outcome = timeout(wait_duration, child.wait()).await;
    let (status, exit_code) = match wait_outcome {
        Ok(Ok(result)) => {
            let cancelled = record.lock().cancel_requested;
            (
                if cancelled {
                    "cancelled"
                } else if result.success() {
                    "succeeded"
                } else {
                    "failed"
                },
                result.code(),
            )
        }
        Ok(Err(error)) => {
            let mut item = record.lock();
            if !item.output.is_empty() {
                item.output.push('\n');
            }
            item.output.push_str(&format!("Bash wait failed: {error}"));
            ("failed", None)
        }
        Err(_) => {
            let cancelled = record.lock().cancel_requested;
            if let Some(pid) = pid {
                let kill_job = job.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    terminate_process_tree(pid, kill_job.as_deref());
                })
                .await;
            }
            let _ = timeout(Duration::from_secs(5), child.wait()).await;
            (if cancelled { "cancelled" } else { "timed_out" }, None)
        }
    };

    let (collected, collector_warning) = match timeout(
        Duration::from_secs(OUTPUT_DRAIN_TIMEOUT_SECS),
        &mut collector,
    )
    .await
    {
        Ok(Ok(Ok(result))) => (Some(result), None),
        Ok(Ok(Err(error))) => (
            None,
            Some(format!(
                "CodeWeave warning: live Bash logging failed: {error}"
            )),
        ),
        Ok(Err(error)) => (
            None,
            Some(format!(
                "CodeWeave warning: output collector failed: {error}"
            )),
        ),
        Err(_) => {
            collector.abort();
            (
                None,
                Some(
                    "CodeWeave warning: output pipes did not close after process termination"
                        .to_owned(),
                ),
            )
        }
    };

    let paths = record.lock().logs.clone();
    let log_limited = collected.is_some_and(|result| result.limited);
    let (mut display_output, mut output_truncated) =
        render_preview(&paths, &OutputFilter::Raw, status, max_output, log_limited).await;
    if display_output.is_empty() {
        display_output = record.lock().output.clone();
    }
    if let Some(warning) = collector_warning {
        append_note(&mut display_output, &warning);
    }
    if status == "timed_out" {
        append_note(
            &mut display_output,
            &format!("Bash run exceeded timeout of {timeout_ms} ms; partial output was retained."),
        );
    } else if status == "cancelled" {
        append_note(
            &mut display_output,
            "Bash run was cancelled; partial output was retained.",
        );
    }
    if let Some(warning) = setup_warning {
        append_note(&mut display_output, &warning);
    }
    if display_output.chars().count() > max_output {
        display_output = if matches!(status, "failed" | "timed_out" | "cancelled") {
            tail_chars(&display_output, max_output)
        } else {
            head_chars(&display_output, max_output)
        };
        output_truncated = true;
    }

    let mut item = record.lock();
    item.status = status.to_owned();
    item.exit_code = exit_code;
    item.ended_at = Some(Utc::now());
    item.pid = None;
    item.job = None;
    item.output = display_output;
    item.output_truncated = output_truncated;
    drop(item);
    execution_guard.disarm();
    Ok(())
}

fn append_note(output: &mut String, note: &str) {
    if !output.is_empty() {
        output.push_str("\n\n");
    }
    output.push_str(note);
}

fn finalize_run_error(record: &Arc<Mutex<RunRecord>>, error: &AppError) {
    let mut item = record.lock();
    if item.ended_at.is_some() {
        return;
    }
    item.status = "failed".to_owned();
    item.exit_code = None;
    item.ended_at = Some(Utc::now());
    item.pid = None;
    item.job = None;
    if !item.output.is_empty() {
        item.output.push('\n');
    }
    item.output.push_str(&format!(
        "CodeWeave Bash execution failed: {}",
        error.0.message
    ));
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
mod tests {
    use super::*;
    use crate::model::{test_bash_executable, BashConfig};
    use tempfile::tempdir;

    fn policy() -> PolicyConfig {
        PolicyConfig {
            max_file_bytes: 1_000_000,
            max_context_chars: 50_000,
            max_search_results: 100,
            bash: BashConfig {
                enabled: true,
                executable: test_bash_executable(),
                default_timeout_ms: 5_000,
                max_timeout_ms: 10_000,
                max_output_chars: 30_000,
                retention_hours: 1,
            },
        }
    }

    fn record(cache: &Path, run_id: &str, status: &str, output: &str) -> RunRecord {
        RunRecord {
            run_id: run_id.to_owned(),
            status: status.to_owned(),
            command: "printf test".to_owned(),
            cwd: cache.to_path_buf(),
            started_at: Utc::now(),
            ended_at: Some(Utc::now()),
            exit_code: (status == "succeeded").then_some(0),
            output: output.to_owned(),
            output_truncated: false,
            logs: RunLogPaths::new(cache, run_id),
            pid: None,
            cancel_requested: false,
            job: None,
        }
    }

    #[tokio::test]
    async fn cwd_must_exist_inside_workspace() {
        let root = tempdir().unwrap();
        let cache = tempdir().unwrap();
        let supervisor = BashSupervisor::new(cache.path().to_path_buf(), policy()).unwrap();

        let missing = supervisor
            .start(
                root.path(),
                StartRequest {
                    command: "printf test".to_owned(),
                    cwd: Some("missing".to_owned()),
                    background: None,
                    timeout_ms: None,
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(
            missing.0.code.as_str(),
            "PATH_NOT_FOUND" | "INVALID_CWD"
        ));

        let escaped = supervisor
            .start(
                root.path(),
                StartRequest {
                    command: "printf test".to_owned(),
                    cwd: Some("../outside".to_owned()),
                    background: None,
                    timeout_ms: None,
                },
            )
            .await
            .unwrap_err();
        assert_eq!(escaped.0.code, "OUTSIDE_ROOT");
    }

    #[tokio::test]
    async fn timeout_above_policy_maximum_is_rejected() {
        let root = tempdir().unwrap();
        let cache = tempdir().unwrap();
        let supervisor = BashSupervisor::new(cache.path().to_path_buf(), policy()).unwrap();
        let error = supervisor
            .start(
                root.path(),
                StartRequest {
                    command: "printf test".to_owned(),
                    cwd: None,
                    background: None,
                    timeout_ms: Some(10_001),
                },
            )
            .await
            .unwrap_err();
        assert_eq!(error.0.code, "INVALID_TIMEOUT");
    }

    #[tokio::test]
    async fn foreground_commands_capture_stdout_stderr_and_failures() {
        let root = tempdir().unwrap();
        let cache = tempdir().unwrap();
        let supervisor = BashSupervisor::new(cache.path().to_path_buf(), policy()).unwrap();
        let succeeded = supervisor
            .start(
                root.path(),
                StartRequest {
                    command: "printf stdout; printf stderr >&2".to_owned(),
                    cwd: None,
                    background: None,
                    timeout_ms: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(succeeded["status"], "succeeded");
        assert_eq!(succeeded["exit_code"], 0);
        assert!(succeeded["output"].as_str().unwrap().contains("stdout"));
        assert!(succeeded["output"].as_str().unwrap().contains("stderr"));

        let failed = supervisor
            .start(
                root.path(),
                StartRequest {
                    command: "printf failed >&2; exit 7".to_owned(),
                    cwd: None,
                    background: None,
                    timeout_ms: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(failed["status"], "failed");
        assert_eq!(failed["exit_code"], 7);
    }

    #[tokio::test]
    async fn foreground_warm_shell_preserves_quotes_and_non_utf8_output() {
        let root = tempdir().unwrap();
        let cache = tempdir().unwrap();
        let supervisor = BashSupervisor::new(cache.path().to_path_buf(), policy()).unwrap();

        let quoted = supervisor
            .start(
                root.path(),
                StartRequest {
                    command: "printf \"first's\"".to_owned(),
                    cwd: None,
                    background: None,
                    timeout_ms: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(quoted["status"], "succeeded");
        assert!(quoted["output"].as_str().unwrap().contains("first's"));

        let binary = supervisor
            .start(
                root.path(),
                StartRequest {
                    command: r"printf '\377'".to_owned(),
                    cwd: None,
                    background: None,
                    timeout_ms: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(binary["status"], "succeeded");
        assert!(!binary["output"].as_str().unwrap().is_empty());
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn foreground_wsl_bash_maps_windows_working_directory() {
        let wsl_bash = PathBuf::from(std::env::var_os("WINDIR").unwrap_or_default())
            .join("System32")
            .join("bash.exe");
        if !wsl_bash.is_file()
            || !std::process::Command::new(&wsl_bash)
                .args(["-c", "true"])
                .status()
                .is_ok_and(|status| status.success())
        {
            return;
        }

        let root = std::env::current_dir().unwrap().canonicalize().unwrap();
        let cache = tempdir().unwrap();
        let mut wsl_policy = policy();
        wsl_policy.bash.executable = wsl_bash.to_string_lossy().into_owned();
        let supervisor = BashSupervisor::new(cache.path().to_path_buf(), wsl_policy).unwrap();
        let result = supervisor
            .start(
                &root,
                StartRequest {
                    command: "pwd".to_owned(),
                    cwd: None,
                    background: None,
                    timeout_ms: None,
                },
            )
            .await
            .unwrap();

        assert_eq!(result["status"], "succeeded");
        assert!(result["output"]
            .as_str()
            .unwrap()
            .trim()
            .ends_with("/Projects/codeweave"));
    }

    #[tokio::test]
    async fn background_commands_can_be_polled_paged_and_cancelled() {
        let root = tempdir().unwrap();
        let cache = tempdir().unwrap();
        let supervisor = BashSupervisor::new(cache.path().to_path_buf(), policy()).unwrap();
        let started = supervisor
            .start(
                root.path(),
                StartRequest {
                    command: "echo started; sleep 30".to_owned(),
                    cwd: None,
                    background: Some(true),
                    timeout_ms: None,
                },
            )
            .await
            .unwrap();
        let run_id = started["run_id"].as_str().unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(matches!(
            supervisor.status(run_id).unwrap()["status"]
                .as_str()
                .unwrap(),
            "queued" | "running"
        ));
        let mut output = supervisor
            .output_stream(run_id, None, Some("combined"))
            .unwrap();
        for _ in 0..50 {
            if output["output"].as_str().unwrap().contains("started") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
            output = supervisor
                .output_stream(run_id, None, Some("combined"))
                .unwrap();
        }
        assert!(output["output"].as_str().unwrap().contains("started"));
        assert_eq!(supervisor.cancel(run_id).unwrap()["status"], "cancelling");

        for _ in 0..50 {
            if supervisor.status(run_id).unwrap()["ended_at"].is_string() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(supervisor.status(run_id).unwrap()["status"], "cancelled");
    }

    #[tokio::test]
    async fn timeout_retains_partial_output() {
        let root = tempdir().unwrap();
        let cache = tempdir().unwrap();
        let mut timeout_policy = policy();
        timeout_policy.bash.default_timeout_ms = 100;
        let supervisor = BashSupervisor::new(cache.path().to_path_buf(), timeout_policy).unwrap();
        let result = supervisor
            .start(
                root.path(),
                StartRequest {
                    command: "printf partial; sleep 30".to_owned(),
                    cwd: None,
                    background: None,
                    timeout_ms: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(result["status"], "timed_out");
        assert!(result["output"].as_str().unwrap().contains("partial"));
    }

    #[test]
    fn output_action_selects_stdout_and_stderr_streams() {
        let cache = tempdir().unwrap();
        let supervisor = BashSupervisor::new(cache.path().to_path_buf(), policy()).unwrap();
        let item = record(cache.path(), "streams", "failed", "combined");
        fs::write(&item.logs.combined, "combined").unwrap();
        fs::write(&item.logs.stdout, "stdout-only").unwrap();
        fs::write(&item.logs.stderr, "stderr-only").unwrap();
        supervisor
            .runs
            .lock()
            .insert("streams".to_owned(), Arc::new(Mutex::new(item)));

        assert_eq!(
            supervisor
                .output_stream("streams", None, Some("stdout"))
                .unwrap()["output"],
            "stdout-only"
        );
        assert_eq!(
            supervisor
                .output_stream("streams", None, Some("stderr"))
                .unwrap()["output"],
            "stderr-only"
        );
    }
}
