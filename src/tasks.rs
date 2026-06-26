use crate::model::{AppError, AppResult, OutputFilter, PolicyConfig, TaskProfile};
use crate::security::resolve_existing;
use crate::task_runtime::{
    remove_logs, render_preview, stream_output, strip_ansi, terminate_process_tree, OutputStream,
    TaskLogPaths, WindowsJob,
};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use parking_lot::Mutex;
use serde::Serialize;
use serde_json::json;
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
pub struct TaskView {
    pub task_id: String,
    pub status: String,
    pub command: Vec<String>,
    pub cwd: String,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub exit_code: Option<i32>,
    pub output: String,
    pub output_truncated: bool,
    pub log_handle: String,
    pub pid: Option<u32>,
}

#[derive(Debug)]
pub(crate) struct TaskRecord {
    task_id: String,
    status: String,
    command: Vec<String>,
    cwd: PathBuf,
    started_at: DateTime<Utc>,
    ended_at: Option<DateTime<Utc>>,
    exit_code: Option<i32>,
    pub(crate) output: String,
    pub(crate) output_truncated: bool,
    pub(crate) logs: TaskLogPaths,
    pid: Option<u32>,
    cancel_requested: bool,
    job: Option<Arc<WindowsJob>>,
}

struct ExecutionGuard {
    record: Arc<Mutex<TaskRecord>>,
    pid: Option<u32>,
    job: Option<Arc<WindowsJob>>,
    armed: bool,
}

impl ExecutionGuard {
    fn new(record: Arc<Mutex<TaskRecord>>, pid: Option<u32>, job: Option<Arc<WindowsJob>>) -> Self {
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
                .push_str("Task execution was abandoned; the process tree was terminated.");
        }
    }
}

fn cleanup_orphan_logs(log_root: &Path, task_retention_hours: i64) {
    let Ok(entries) = fs::read_dir(log_root) else {
        return;
    };
    let max_age = std::time::Duration::from_secs((task_retention_hours as u64) * 60 * 60);
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
pub struct TaskSupervisor {
    tasks: Arc<Mutex<HashMap<String, Arc<Mutex<TaskRecord>>>>>,
    run_permit: Arc<Semaphore>,
    cache_root: PathBuf,
    policy: PolicyConfig,
    profiles: HashMap<String, TaskProfile>,
    retention_hours: i64,
}

const MAX_RETAINED_TASKS: usize = 256;
const TASK_RETENTION_HOURS: i64 = 1;
const OUTPUT_DRAIN_TIMEOUT_SECS: u64 = 10;

#[derive(Debug, Clone)]
pub struct StartRequest {
    pub profile: Option<String>,
    pub command: Option<Vec<String>>,
    pub cwd: Option<String>,
    pub shell: bool,
    pub background: Option<bool>,
    pub timeout_ms: Option<u64>,
}

fn command_uses_path(value: &str) -> bool {
    Path::new(value).is_absolute() || value.contains('/') || value.contains('\\')
}

fn normalized_command_name(value: &str) -> String {
    Path::new(value)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(value)
        .to_ascii_lowercase()
        .trim_end_matches(".exe")
        .to_owned()
}

fn authorize_executable(
    requested: &str,
    cwd: &Path,
    allowed_commands: &[String],
) -> AppResult<Option<PathBuf>> {
    if !command_uses_path(requested) {
        let requested_name = normalized_command_name(requested);
        let allowed = allowed_commands.iter().any(|allowed| {
            !command_uses_path(allowed)
                && normalized_command_name(allowed).eq_ignore_ascii_case(&requested_name)
        });
        return if allowed {
            Ok(None)
        } else {
            Err(AppError::details(
                "COMMAND_NOT_ALLOWED",
                "Command is not allowed by policy",
                json!({"command": requested, "allowed": allowed_commands}),
            ))
        };
    }

    let requested_path = Path::new(requested);
    let candidate = if requested_path.is_absolute() {
        requested_path.to_path_buf()
    } else {
        cwd.join(requested_path)
    };
    let canonical = candidate.canonicalize().map_err(|error| {
        AppError::details(
            "COMMAND_NOT_FOUND",
            "Executable path could not be resolved",
            json!({"command": requested, "error": error.to_string()}),
        )
    })?;
    if !canonical.is_file() {
        return Err(AppError::details(
            "COMMAND_NOT_FOUND",
            "Executable path is not a file",
            json!({"command": requested, "resolved": canonical}),
        ));
    }

    let explicitly_allowed = allowed_commands.iter().any(|allowed| {
        let allowed_path = Path::new(allowed);
        allowed_path.is_absolute()
            && allowed_path
                .canonicalize()
                .is_ok_and(|configured| configured == canonical)
    });
    if !explicitly_allowed {
        return Err(AppError::details(
            "COMMAND_NOT_ALLOWED",
            "Executable paths must be explicitly allowlisted by absolute canonical path",
            json!({
                "command": requested,
                "resolved": canonical,
                "allowed": allowed_commands
            }),
        ));
    }
    Ok(Some(canonical))
}

impl TaskSupervisor {
    pub fn new(
        cache_root: PathBuf,
        policy: PolicyConfig,
        profiles: HashMap<String, TaskProfile>,
    ) -> AppResult<Self> {
        let log_root = cache_root.join("task-logs");
        fs::create_dir_all(&log_root)?;
        let retention_hours = policy.task_retention_hours.unwrap_or(TASK_RETENTION_HOURS);
        if retention_hours <= 0 {
            return Err(AppError::details(
                "INVALID_POLICY",
                "task_retention_hours must be greater than zero",
                json!({"task_retention_hours": retention_hours}),
            ));
        }
        cleanup_orphan_logs(&log_root, retention_hours);
        Ok(Self {
            tasks: Arc::new(Mutex::new(HashMap::new())),
            run_permit: Arc::new(Semaphore::new(1)),
            cache_root,
            policy,
            profiles,
            retention_hours,
        })
    }

    pub fn profile_names(&self) -> Vec<String> {
        let mut names: Vec<_> = self.profiles.keys().cloned().collect();
        names.sort();
        names
    }

    pub fn running_count(&self) -> usize {
        self.tasks
            .lock()
            .values()
            .filter(|record| record.lock().ended_at.is_none())
            .count()
    }

    pub fn retained_task_ids(&self) -> HashSet<String> {
        self.tasks.lock().keys().cloned().collect()
    }

    pub fn validate_profiles(&self, requested: &[String]) -> AppResult<()> {
        let available = self.profile_names();
        let missing: Vec<String> = requested
            .iter()
            .filter(|profile| !self.profiles.contains_key(profile.as_str()))
            .cloned()
            .collect();
        if missing.is_empty() {
            return Ok(());
        }
        Err(AppError::details(
            "UNKNOWN_VALIDATION_PROFILE",
            "One or more validation profiles are not configured",
            json!({
                "requested": requested,
                "missing": missing,
                "available": available,
                "validate_accepts": "Configured task profile names only; do not pass shell commands or command strings.",
                "suggested_action": "Remove validate and call run(action='start', command=[...]) after the edit, or configure tasks.<name> and restart CodeWeave.",
            }),
        ))
    }

    pub async fn start(&self, root: &Path, request: StartRequest) -> AppResult<serde_json::Value> {
        let (mut command, profile_cwd, profile_timeout, profile_background, output_filter) =
            if let Some(profile) = &request.profile {
                let value = self.profiles.get(profile).ok_or_else(|| {
                    AppError::details(
                        "UNKNOWN_TASK_PROFILE",
                        "Unknown task profile",
                        json!({
                            "profile": profile,
                            "available": self.profile_names(),
                            "configuration_hint": "Add tasks.<name> to the active CodeWeave config and restart the server.",
                        }),
                    )
                })?;
                (
                    value.command.clone(),
                    value.cwd.clone(),
                    Some(value.timeout_ms),
                    value.background,
                    value.output_filter.clone(),
                )
            } else {
                (
                    request
                        .command
                        .clone()
                        .ok_or_else(|| AppError::invalid("Provide profile or command"))?,
                    None,
                    None,
                    false,
                    OutputFilter::Raw,
                )
            };
        if command.is_empty() {
            return Err(AppError::invalid("Command cannot be empty"));
        }
        if request.shell && !self.policy.shell_enabled {
            return Err(AppError::new(
                "SHELL_DISABLED",
                "Shell mode is disabled by policy",
            ));
        }
        let cwd_relative = request
            .cwd
            .or(profile_cwd)
            .unwrap_or_else(|| ".".to_owned());
        let cwd = if cwd_relative == "." {
            root.to_path_buf()
        } else {
            resolve_existing(root, &cwd_relative)?
        };
        if !cwd.is_dir() {
            return Err(AppError::new("INVALID_CWD", "Task cwd is not a directory"));
        }
        if let Some(resolved) =
            authorize_executable(&command[0], &cwd, &self.policy.allowed_commands)?
        {
            command[0] = resolved.to_string_lossy().into_owned();
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
        let task_id = format!("task_{}", Uuid::new_v4().simple());
        let log_root = self.cache_root.join("task-logs");
        let logs = TaskLogPaths::new(&log_root, &task_id);
        let record = Arc::new(Mutex::new(TaskRecord {
            task_id: task_id.clone(),
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
        self.tasks.lock().insert(task_id.clone(), record.clone());
        self.trim_tasks();
        let timeout_ms = request.timeout_ms.or(profile_timeout).unwrap_or(120_000);
        let background = request.background.unwrap_or(profile_background);
        let policy = self.policy.clone();
        let execution_record = record.clone();
        let runner = async move {
            let result = execute(
                record,
                command,
                cwd,
                request.shell,
                timeout_ms,
                policy.max_task_output_chars,
                output_filter,
            )
            .await;
            if let Err(error) = &result {
                finalize_task_error(&execution_record, error);
            }
            result
        };
        if background {
            tokio::spawn(async move {
                let _run_permit = run_permit;
                let _ = runner.await;
            });
            Ok(json!({
                "task_id": task_id,
                "status": "queued",
                "background": true,
                "log_handle": format!("task-log:{task_id}")
            }))
        } else {
            let _run_permit = run_permit;
            runner.await?;
            self.status(&task_id)
        }
    }

    pub fn status(&self, task_id: &str) -> AppResult<serde_json::Value> {
        let record = self.tasks.lock().get(task_id).cloned().ok_or_else(|| {
            AppError::details(
                "TASK_NOT_FOUND",
                "Task not found",
                json!({"task_id": task_id}),
            )
        })?;
        let record = record.lock();
        Ok(serde_json::to_value(view(
            &record,
            self.policy.max_task_output_chars,
        ))?)
    }

    pub fn output_stream(
        &self,
        task_id: &str,
        continuation: Option<&str>,
        requested_stream: Option<&str>,
    ) -> AppResult<serde_json::Value> {
        let stream = OutputStream::parse(requested_stream)
            .map_err(|error| AppError::invalid(error.to_string()))?;
        let record = self.tasks.lock().get(task_id).cloned().ok_or_else(|| {
            AppError::details(
                "TASK_NOT_FOUND",
                "Task not found",
                json!({"task_id": task_id}),
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
        let requested_offset = continuation_offset(continuation, task_id, stream)
            .unwrap_or(0)
            .min(full.len());
        let offset = char_boundary(&full, requested_offset);
        let end = char_boundary(
            &full,
            (offset + self.policy.max_task_output_chars).min(full.len()),
        );
        let next = (end < full.len()).then(|| format!("task:{task_id}:{}:{end}", stream.as_str()));
        Ok(json!({
            "task_id": task_id,
            "status": record.status,
            "stream": stream.as_str(),
            "output": strip_ansi(&full[offset..end]),
            "continuation": next,
            "total_chars": full.len()
        }))
    }

    pub fn cancel(&self, task_id: &str) -> AppResult<serde_json::Value> {
        let record = self.tasks.lock().get(task_id).cloned().ok_or_else(|| {
            AppError::details(
                "TASK_NOT_FOUND",
                "Task not found",
                json!({"task_id": task_id}),
            )
        })?;
        let (pid, job) = {
            let mut item = record.lock();
            if item.ended_at.is_some() {
                return Ok(serde_json::to_value(view(
                    &item,
                    self.policy.max_task_output_chars,
                ))?);
            }
            item.status = "cancelling".to_owned();
            item.cancel_requested = true;
            (item.pid, item.job.clone())
        };
        if let Some(pid) = pid {
            terminate_process_tree(pid, job.as_deref());
        }
        Ok(json!({"task_id": task_id, "status": "cancelling"}))
    }

    pub fn read_log(&self, task_id: &str) -> AppResult<String> {
        let record = self
            .tasks
            .lock()
            .get(task_id)
            .cloned()
            .ok_or_else(|| AppError::new("TASK_NOT_FOUND", "Task not found"))?;
        let record = record.lock();
        Ok(fs::read(&record.logs.combined)
            .map(|bytes| strip_ansi(&String::from_utf8_lossy(&bytes)))
            .unwrap_or_else(|_| record.output.clone()))
    }

    fn trim_tasks(&self) {
        let retention_hours = self.retention_hours;
        let cutoff = Utc::now() - ChronoDuration::hours(retention_hours);
        let mut removed_logs = Vec::new();
        {
            let mut tasks = self.tasks.lock();
            let expired: Vec<String> = tasks
                .iter()
                .filter_map(|(task_id, record)| {
                    let record = record.lock();
                    record
                        .ended_at
                        .is_some_and(|ended| ended < cutoff)
                        .then(|| task_id.clone())
                })
                .collect();
            for task_id in expired {
                if let Some(record) = tasks.remove(&task_id) {
                    removed_logs.push(record.lock().logs.clone());
                }
            }

            if tasks.len() > MAX_RETAINED_TASKS {
                let mut completed: Vec<_> = tasks
                    .iter()
                    .filter_map(|(task_id, record)| {
                        let record = record.lock();
                        record.ended_at.map(|ended| (task_id.clone(), ended))
                    })
                    .collect();
                completed.sort_by_key(|(_, ended)| *ended);
                let remove_count = tasks.len().saturating_sub(MAX_RETAINED_TASKS);
                for (task_id, _) in completed.into_iter().take(remove_count) {
                    if let Some(record) = tasks.remove(&task_id) {
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
            .tasks
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
            let command_key = format!(
                "{}\0{}",
                record.cwd.to_string_lossy(),
                record.command.join("\0")
            );
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
                "task_id": record.task_id,
                "status": record.status,
                "command": record.command,
                "output": tail_chars(&record.output, 4_000),
                "log_handle": format!("task-log:{}", record.task_id),
                "reason_codes": ["recent_task_failure"]
            }));
            if output.len() >= limit {
                break;
            }
        }
        output
    }
}

fn finalize_cancelled_before_start(record: &Arc<Mutex<TaskRecord>>) -> bool {
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
        .push_str("Task was cancelled before the process started.");
    true
}

async fn execute(
    record: Arc<Mutex<TaskRecord>>,
    command: Vec<String>,
    cwd: PathBuf,
    shell: bool,
    timeout_ms: u64,
    max_output: usize,
    output_filter: OutputFilter,
) -> AppResult<()> {
    if finalize_cancelled_before_start(&record) {
        return Ok(());
    }

    let mut process = if shell {
        let script = if cfg!(windows) {
            powershell_command(&command)
        } else {
            posix_shell_command(&command)
        };
        if cfg!(windows) {
            let mut command = Command::new("powershell.exe");
            command.args(["-NoProfile", "-NonInteractive", "-Command", &script]);
            command
        } else {
            let mut command = Command::new("bash");
            command.args(["-lc", &script]);
            command
        }
    } else {
        let mut process = Command::new(&command[0]);
        process.args(&command[1..]);
        process
    };
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
            "TASK_START_FAILED",
            error.to_string(),
            json!({"command": command, "cwd": cwd}),
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
        .ok_or_else(|| AppError::new("TASK_PIPE_FAILED", "Task stdout pipe is unavailable"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| AppError::new("TASK_PIPE_FAILED", "Task stderr pipe is unavailable"))?;
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
            item.output.push_str(&format!("Task wait failed: {error}"));
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
                "CodeWeave warning: live task logging failed: {error}"
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
        render_preview(&paths, &output_filter, status, max_output, log_limited).await;
    if display_output.is_empty() {
        display_output = record.lock().output.clone();
    }
    if let Some(warning) = collector_warning {
        append_note(&mut display_output, &warning);
    }
    if status == "timed_out" {
        append_note(
            &mut display_output,
            &format!("Task exceeded timeout of {timeout_ms} ms; partial output was retained."),
        );
    } else if status == "cancelled" {
        append_note(
            &mut display_output,
            "Task was cancelled; partial output was retained.",
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

fn finalize_task_error(record: &Arc<Mutex<TaskRecord>>, error: &AppError) {
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
        "CodeWeave task execution failed: {}",
        error.0.message
    ));
}

fn view(record: &TaskRecord, max_output: usize) -> TaskView {
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
    TaskView {
        task_id: record.task_id.clone(),
        status: record.status.clone(),
        command: record.command.clone(),
        cwd: record.cwd.to_string_lossy().into_owned(),
        started_at: record.started_at,
        ended_at: record.ended_at,
        exit_code: record.exit_code,
        output,
        output_truncated: record.output_truncated || record.output.chars().count() > max_output,
        log_handle: format!("task-log:{}", record.task_id),
        pid: record.pid,
    }
}

fn continuation_offset(
    continuation: Option<&str>,
    task_id: &str,
    stream: OutputStream,
) -> Option<usize> {
    let value = continuation?;
    let prefix = format!("task:{task_id}:{}:", stream.as_str());
    if let Some(offset) = value.strip_prefix(&prefix) {
        return offset.parse::<usize>().ok();
    }
    if stream == OutputStream::Combined {
        return value
            .strip_prefix(&format!("task:{task_id}:"))
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

fn posix_shell_command(command: &[String]) -> String {
    command
        .iter()
        .map(|argument| format!("'{}'", argument.replace('\'', "'\"'\"'")))
        .collect::<Vec<_>>()
        .join(" ")
}

fn powershell_command(command: &[String]) -> String {
    format!(
        "& {}",
        command
            .iter()
            .map(|argument| format!("'{}'", argument.replace('\'', "''")))
            .collect::<Vec<_>>()
            .join(" ")
    )
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
    use tempfile::tempdir;

    fn policy() -> PolicyConfig {
        PolicyConfig {
            max_file_bytes: 1_000_000,
            max_context_chars: 50_000,
            max_search_results: 100,
            max_task_output_chars: 30_000,
            shell_enabled: false,
            allowed_commands: vec!["cargo".to_owned(), "python".to_owned()],
            task_retention_hours: None,
        }
    }

    fn record(cache: &Path, task_id: &str, status: &str, output: &str) -> TaskRecord {
        TaskRecord {
            task_id: task_id.to_owned(),
            status: status.to_owned(),
            command: vec!["cargo".to_owned(), "test".to_owned()],
            cwd: cache.to_path_buf(),
            started_at: Utc::now(),
            ended_at: Some(Utc::now()),
            exit_code: (status == "succeeded").then_some(0),
            output: output.to_owned(),
            output_truncated: false,
            logs: TaskLogPaths::new(cache, task_id),
            pid: None,
            cancel_requested: false,
            job: None,
        }
    }

    #[test]
    fn validation_profiles_are_rejected_before_execution() {
        let cache = tempdir().unwrap();
        let supervisor =
            TaskSupervisor::new(cache.path().to_path_buf(), policy(), HashMap::new()).unwrap();
        let error = supervisor
            .validate_profiles(&["typecheck".to_owned()])
            .unwrap_err();
        assert_eq!(error.0.code, "UNKNOWN_VALIDATION_PROFILE");
        let details = error.0.details.unwrap();
        assert_eq!(details["missing"], json!(["typecheck"]));
    }

    #[test]
    fn executable_paths_require_an_explicit_absolute_allowlist_entry() {
        let root = tempdir().unwrap();
        let executable_name = if cfg!(windows) { "cargo.exe" } else { "cargo" };
        let executable = root.path().join(executable_name);
        fs::write(&executable, b"not actually executed").unwrap();
        let relative = format!("./{executable_name}");

        assert!(
            authorize_executable("cargo", root.path(), &["cargo".to_owned()])
                .unwrap()
                .is_none()
        );
        let rejected =
            authorize_executable(&relative, root.path(), &["cargo".to_owned()]).unwrap_err();
        assert_eq!(rejected.0.code, "COMMAND_NOT_ALLOWED");

        let allowed = authorize_executable(
            &relative,
            root.path(),
            &[executable.to_string_lossy().into_owned()],
        )
        .unwrap();
        assert_eq!(allowed, Some(executable.canonicalize().unwrap()));
    }

    #[test]
    fn command_allowlist_normalizes_uppercase_exe_suffix() {
        assert_eq!(normalized_command_name("CARGO.EXE"), "cargo");
        assert_eq!(normalized_command_name("C:/Tools/CARGO.EXE"), "cargo");
    }

    #[tokio::test]
    async fn cancellation_before_spawn_prevents_process_start() {
        let cache = tempdir().unwrap();
        let mut item = record(cache.path(), "cancelled", "queued", "");
        item.ended_at = None;
        item.cancel_requested = true;
        let item = Arc::new(Mutex::new(item));

        execute(
            item.clone(),
            vec!["definitely-not-a-real-codeweave-command".to_owned()],
            cache.path().to_path_buf(),
            false,
            1_000,
            10_000,
            OutputFilter::Raw,
        )
        .await
        .unwrap();

        let item = item.lock();
        assert_eq!(item.status, "cancelled");
        assert!(item.ended_at.is_some());
        assert!(item.output.contains("before the process started"));
    }

    #[test]
    fn later_success_suppresses_same_command_failure() {
        let cache = tempdir().unwrap();
        let supervisor =
            TaskSupervisor::new(cache.path().to_path_buf(), policy(), HashMap::new()).unwrap();
        let failed_at = Utc::now() - ChronoDuration::seconds(2);
        let succeeded_at = Utc::now() - ChronoDuration::seconds(1);
        let mut failed = record(cache.path(), "failed", "failed", "compile error");
        failed.started_at = failed_at;
        failed.ended_at = Some(failed_at);
        let mut succeeded = record(cache.path(), "succeeded", "succeeded", "tests passed");
        succeeded.started_at = succeeded_at;
        succeeded.ended_at = Some(succeeded_at);
        supervisor
            .tasks
            .lock()
            .insert("failed".to_owned(), Arc::new(Mutex::new(failed)));
        supervisor
            .tasks
            .lock()
            .insert("succeeded".to_owned(), Arc::new(Mutex::new(succeeded)));
        assert!(supervisor.recent_failures("compile error", 3).is_empty());
    }

    #[test]
    fn task_errors_preserve_live_output() {
        let cache = tempdir().unwrap();
        let record = Arc::new(Mutex::new(record(
            cache.path(),
            "failed",
            "running",
            "partial output",
        )));
        record.lock().ended_at = None;
        finalize_task_error(
            &record,
            &AppError::new("TASK_LOG_FAILED", "Access is denied"),
        );
        let record = record.lock();
        assert!(record.output.contains("partial output"));
        assert!(record.output.contains("Access is denied"));
    }

    #[test]
    fn read_log_falls_back_to_in_memory_output() {
        let cache = tempdir().unwrap();
        let supervisor =
            TaskSupervisor::new(cache.path().to_path_buf(), policy(), HashMap::new()).unwrap();
        supervisor.tasks.lock().insert(
            "memory-only".to_owned(),
            Arc::new(Mutex::new(record(
                cache.path(),
                "memory-only",
                "succeeded",
                "tests passed",
            ))),
        );
        assert_eq!(supervisor.read_log("memory-only").unwrap(), "tests passed");
    }

    #[test]
    fn failed_views_use_the_tail() {
        let cache = tempdir().unwrap();
        let item = record(
            cache.path(),
            "failed",
            "failed",
            &format!("{}important failure", "noise".repeat(100)),
        );
        let view = view(&item, 40);
        assert!(view.output.contains("important failure"));
        assert!(view.output_truncated);
    }

    #[test]
    fn abandoned_execution_guard_finalizes_the_record() {
        let cache = tempdir().unwrap();
        let item = Arc::new(Mutex::new(record(
            cache.path(),
            "abandoned",
            "running",
            "partial output",
        )));
        item.lock().ended_at = None;

        drop(ExecutionGuard::new(item.clone(), None, None));

        let item = item.lock();
        assert_eq!(item.status, "cancelled");
        assert!(item.ended_at.is_some());
        assert!(item.output.contains("process tree was terminated"));
    }

    #[test]
    fn output_action_selects_stdout_and_stderr_streams() {
        let cache = tempdir().unwrap();
        let supervisor =
            TaskSupervisor::new(cache.path().to_path_buf(), policy(), HashMap::new()).unwrap();
        let item = record(cache.path(), "streams", "failed", "combined");
        fs::write(&item.logs.combined, "combined").unwrap();
        fs::write(&item.logs.stdout, "stdout-only").unwrap();
        fs::write(&item.logs.stderr, "stderr-only").unwrap();
        supervisor
            .tasks
            .lock()
            .insert("streams".to_owned(), Arc::new(Mutex::new(item)));

        let stdout = supervisor
            .output_stream("streams", None, Some("stdout"))
            .unwrap();
        let stderr = supervisor
            .output_stream("streams", None, Some("stderr"))
            .unwrap();
        assert_eq!(stdout["output"], "stdout-only");
        assert_eq!(stderr["output"], "stderr-only");
    }

    #[test]
    fn shell_arguments_are_quoted_as_literals() {
        assert_eq!(
            posix_shell_command(&[
                "cargo".to_owned(),
                "test; rm -rf /".to_owned(),
                "a'b".to_owned(),
            ]),
            "'cargo' 'test; rm -rf /' 'a'\"'\"'b'"
        );
        assert_eq!(
            powershell_command(&[
                "cargo".to_owned(),
                "test; Remove-Item C:\\\\".to_owned(),
                "a'b".to_owned(),
            ]),
            "& 'cargo' 'test; Remove-Item C:\\\\' 'a''b'"
        );
    }
}
