use crate::model::{AppError, AppResult, PolicyConfig, TaskProfile};
use crate::security::resolve_existing;
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
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
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
struct TaskRecord {
    task_id: String,
    status: String,
    command: Vec<String>,
    cwd: PathBuf,
    started_at: DateTime<Utc>,
    ended_at: Option<DateTime<Utc>>,
    exit_code: Option<i32>,
    output: String,
    log_path: PathBuf,
    pid: Option<u32>,
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
    cache_root: PathBuf,
    policy: PolicyConfig,
    profiles: HashMap<String, TaskProfile>,
    retention_hours: i64,
}

const MAX_RETAINED_TASKS: usize = 256;
const MAX_RUNNING_TASKS: usize = 32;
const MAX_TASK_LOG_BYTES: usize = 16 * 1024 * 1024;
const TASK_RETENTION_HOURS: i64 = 1;

#[derive(Debug, Clone)]
pub struct StartRequest {
    pub profile: Option<String>,
    pub command: Option<Vec<String>>,
    pub cwd: Option<String>,
    pub shell: bool,
    pub background: bool,
    pub timeout_ms: Option<u64>,
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
        let (mut command, profile_cwd, profile_timeout) = if let Some(profile) = &request.profile {
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
            )
        } else {
            (
                request
                    .command
                    .clone()
                    .ok_or_else(|| AppError::invalid("Provide profile or command"))?,
                None,
                None,
            )
        };
        if command.is_empty() {
            return Err(AppError::invalid("Command cannot be empty"));
        }
        let executable = Path::new(&command[0])
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or(&command[0])
            .trim_end_matches(".exe")
            .to_ascii_lowercase();
        if !self.policy.allowed_commands.iter().any(|allowed| {
            allowed
                .trim_end_matches(".exe")
                .eq_ignore_ascii_case(&executable)
        }) {
            return Err(AppError::details(
                "COMMAND_NOT_ALLOWED",
                "Command is not allowed by policy",
                json!({"command": executable, "allowed": self.policy.allowed_commands}),
            ));
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
        let requested_executable = Path::new(&command[0]);
        if requested_executable.is_relative()
            && (command[0].contains('/') || command[0].contains('\\'))
        {
            let resolved = cwd.join(requested_executable);
            if resolved.is_file() {
                command[0] = resolved.to_string_lossy().into_owned();
            }
        }
        let running_tasks = self
            .tasks
            .lock()
            .values()
            .filter(|record| record.lock().ended_at.is_none())
            .count();
        if running_tasks >= MAX_RUNNING_TASKS {
            return Err(AppError::details(
                "TASK_LIMIT_REACHED",
                "Too many tasks are already running",
                json!({"running": running_tasks, "limit": MAX_RUNNING_TASKS}),
            ));
        }
        let task_id = format!("task_{}", Uuid::new_v4().simple());
        let log_path = self
            .cache_root
            .join("task-logs")
            .join(format!("{task_id}.log"));
        let record = Arc::new(Mutex::new(TaskRecord {
            task_id: task_id.clone(),
            status: "queued".to_owned(),
            command: command.clone(),
            cwd: cwd.clone(),
            started_at: Utc::now(),
            ended_at: None,
            exit_code: None,
            output: String::new(),
            log_path: log_path.clone(),
            pid: None,
        }));
        self.tasks.lock().insert(task_id.clone(), record.clone());
        self.trim_tasks();
        let timeout_ms = request.timeout_ms.or(profile_timeout).unwrap_or(120_000);
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
            )
            .await;
            if let Err(error) = &result {
                finalize_task_error(&execution_record, error);
            }
            result
        };
        if request.background {
            tokio::spawn(async move {
                let _ = runner.await;
            });
            Ok(
                json!({"task_id": task_id, "status": "queued", "background": true, "log_handle": format!("task-log:{task_id}")}),
            )
        } else {
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
        let value = serde_json::to_value(view(&record, self.policy.max_task_output_chars))?;
        Ok(value)
    }

    pub fn output(
        &self,
        task_id: &str,
        continuation: Option<&str>,
    ) -> AppResult<serde_json::Value> {
        let record = self.tasks.lock().get(task_id).cloned().ok_or_else(|| {
            AppError::details(
                "TASK_NOT_FOUND",
                "Task not found",
                json!({"task_id": task_id}),
            )
        })?;
        let record = record.lock();
        let full = fs::read_to_string(&record.log_path).unwrap_or_else(|_| record.output.clone());
        let requested_offset = continuation
            .and_then(|value| value.strip_prefix(&format!("task:{task_id}:")))
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0)
            .min(full.len());
        let offset = char_boundary(&full, requested_offset);
        let end = char_boundary(
            &full,
            (offset + self.policy.max_task_output_chars).min(full.len()),
        );
        let next = (end < full.len()).then(|| format!("task:{task_id}:{end}"));
        Ok(
            json!({"task_id": task_id, "status": record.status, "output": &full[offset..end], "continuation": next, "total_chars": full.len()}),
        )
    }

    pub fn cancel(&self, task_id: &str) -> AppResult<serde_json::Value> {
        let record = self.tasks.lock().get(task_id).cloned().ok_or_else(|| {
            AppError::details(
                "TASK_NOT_FOUND",
                "Task not found",
                json!({"task_id": task_id}),
            )
        })?;
        let pid = {
            let mut item = record.lock();
            if item.ended_at.is_some() {
                // Task already finished — nothing to cancel.
                let value = serde_json::to_value(view(&item, self.policy.max_task_output_chars))?;
                return Ok(value);
            }
            item.status = "cancelling".to_owned();
            item.pid
        };
        if let Some(pid) = pid {
            kill_process_tree(pid);
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
        Ok(fs::read_to_string(&record.log_path).unwrap_or_else(|_| record.output.clone()))
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
                    removed_logs.push(record.lock().log_path.clone());
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
                        removed_logs.push(record.lock().log_path.clone());
                    }
                }
            }
        }
        for log_path in removed_logs {
            if let Err(error) = fs::remove_file(&log_path) {
                if error.kind() != std::io::ErrorKind::NotFound {
                    eprintln!(
                        "task log cleanup failed for {}: {error}",
                        log_path.display()
                    );
                }
            }
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
            .map(|r| {
                let key = {
                    let g = r.lock();
                    g.ended_at.unwrap_or(g.started_at)
                };
                (key, r.clone())
            })
            .collect();
        keyed.sort_by(|(a, _), (b, _)| b.cmp(a));
        let records: Vec<_> = keyed.into_iter().map(|(_, r)| r).collect();
        let mut output = Vec::new();
        let mut superseded_commands = HashSet::new();
        for record in records {
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
            let end = char_boundary(&record.output, record.output.len().min(4_000));
            output.push(json!({
                "task_id": record.task_id,
                "status": record.status,
                "command": record.command,
                "output": &record.output[..end],
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

async fn execute(
    record: Arc<Mutex<TaskRecord>>,
    command: Vec<String>,
    cwd: PathBuf,
    shell: bool,
    timeout_ms: u64,
    max_output: usize,
) -> AppResult<()> {
    let mut process = if shell {
        let script = if cfg!(windows) {
            powershell_command(&command)
        } else {
            posix_shell_command(&command)
        };
        if cfg!(windows) {
            let mut cmd = Command::new("powershell.exe");
            cmd.args(["-NoProfile", "-NonInteractive", "-Command", &script]);
            cmd
        } else {
            let mut cmd = Command::new("bash");
            cmd.args(["-lc", &script]);
            cmd
        }
    } else {
        let mut cmd = Command::new(&command[0]);
        cmd.args(&command[1..]);
        cmd
    };
    process
        .current_dir(&cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    process.as_std_mut().process_group(0);
    let mut child = process.spawn().map_err(|e| {
        AppError::details(
            "TASK_START_FAILED",
            e.to_string(),
            json!({"command": command, "cwd": cwd}),
        )
    })?;
    let pid = child.id();
    {
        let mut item = record.lock();
        item.status = "running".to_owned();
        item.started_at = Utc::now();
        item.pid = pid;
    }
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| AppError::new("TASK_PIPE_FAILED", "Task stdout pipe is unavailable"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| AppError::new("TASK_PIPE_FAILED", "Task stderr pipe is unavailable"))?;
    let execution = async {
        let output = collect_bounded_output(stdout, stderr, MAX_TASK_LOG_BYTES);
        let wait = child.wait();
        let (output, status) = tokio::join!(output, wait);
        Ok::<_, std::io::Error>((output?, status?))
    };
    let outcome = timeout(Duration::from_millis(timeout_ms), execution).await;
    let (status, exit_code, output) = match outcome {
        Ok(Ok(((stdout, stderr, output_limited), result))) => {
            let mut text = String::from_utf8_lossy(&stdout).to_string();
            if !stderr.is_empty() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&String::from_utf8_lossy(&stderr));
            }
            if output_limited {
                text.push_str("\n… task log limit reached; additional output was discarded …");
            }
            (
                if result.success() {
                    "succeeded"
                } else {
                    "failed"
                },
                result.code(),
                strip_ansi(&text),
            )
        }
        Ok(Err(error)) => ("failed", None, format!("Task wait failed: {error}")),
        Err(_) => {
            if let Some(pid) = pid {
                kill_process_tree(pid);
            }
            (
                "timed_out",
                None,
                format!("Task exceeded timeout of {timeout_ms} ms"),
            )
        }
    };
    let log_path = record.lock().log_path.clone();
    let log_error = tokio::fs::write(&log_path, &output).await.err();
    let mut display_output = if output.len() > max_output {
        let end = char_boundary(&output, max_output);
        let mut value = output[..end].to_owned();
        value.push_str("\n… output truncated; use run(action='output') …");
        value
    } else {
        output
    };
    if let Some(error) = log_error {
        if !display_output.is_empty() {
            display_output.push('\n');
        }
        display_output.push_str(&format!(
            "CodeWeave warning: task output could not be persisted to {}: {error}. The in-memory result is still available.",
            log_path.display()
        ));
    }
    let mut item = record.lock();
    item.status = status.to_owned();
    item.exit_code = exit_code;
    item.ended_at = Some(Utc::now());
    item.pid = None;
    item.output = display_output;
    Ok(())
}

async fn collect_bounded_output<O, E>(
    mut stdout: O,
    mut stderr: E,
    limit: usize,
) -> std::io::Result<(Vec<u8>, Vec<u8>, bool)>
where
    O: AsyncRead + Unpin,
    E: AsyncRead + Unpin,
{
    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();
    let mut stdout_done = false;
    let mut stderr_done = false;
    let mut limited = false;
    let mut total = 0usize;
    let mut stdout_buffer = [0u8; 8192];
    let mut stderr_buffer = [0u8; 8192];

    while !stdout_done || !stderr_done {
        tokio::select! {
            read = stdout.read(&mut stdout_buffer), if !stdout_done => {
                let read = read?;
                if read == 0 {
                    stdout_done = true;
                } else {
                    append_bounded(&mut stdout_bytes, &stdout_buffer[..read], limit, &mut total, &mut limited);
                }
            }
            read = stderr.read(&mut stderr_buffer), if !stderr_done => {
                let read = read?;
                if read == 0 {
                    stderr_done = true;
                } else {
                    append_bounded(&mut stderr_bytes, &stderr_buffer[..read], limit, &mut total, &mut limited);
                }
            }
        }
    }
    Ok((stdout_bytes, stderr_bytes, limited))
}

fn append_bounded(
    destination: &mut Vec<u8>,
    chunk: &[u8],
    limit: usize,
    total: &mut usize,
    limited: &mut bool,
) {
    let remaining = limit.saturating_sub(*total);
    let retained = remaining.min(chunk.len());
    destination.extend_from_slice(&chunk[..retained]);
    *total += retained;
    if retained < chunk.len() {
        *limited = true;
    }
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
    item.output = format!("CodeWeave task execution failed: {}", error.0.message);
}

fn view(record: &TaskRecord, max_output: usize) -> TaskView {
    TaskView {
        task_id: record.task_id.clone(),
        status: record.status.clone(),
        command: record.command.clone(),
        cwd: record.cwd.to_string_lossy().into_owned(),
        started_at: record.started_at,
        ended_at: record.ended_at,
        exit_code: record.exit_code,
        output: record.output.chars().take(max_output).collect(),
        output_truncated: record.output.len() > max_output,
        log_handle: format!("task-log:{}", record.task_id),
        pid: record.pid,
    }
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

fn strip_ansi(value: &str) -> String {
    #[derive(Clone, Copy)]
    enum State {
        Text,
        Escape,
        Csi,
        Osc,
        OscEscape,
    }
    let mut state = State::Text;
    let mut output = String::with_capacity(value.len());
    for ch in value.chars() {
        state = match state {
            State::Text if ch == '\u{1b}' => State::Escape,
            State::Text => {
                output.push(ch);
                State::Text
            }
            State::Escape if ch == '[' => State::Csi,
            State::Escape if ch == ']' => State::Osc,
            State::Escape => State::Text,
            State::Csi if ('@'..='~').contains(&ch) => State::Text,
            State::Csi => State::Csi,
            State::Osc if ch == '\u{7}' => State::Text,
            State::Osc if ch == '\u{1b}' => State::OscEscape,
            State::Osc => State::Osc,
            State::OscEscape if ch == '\\' => State::Text,
            State::OscEscape if ch == '\u{1b}' => State::OscEscape,
            State::OscEscape => State::Osc,
        };
    }
    output
}

fn char_boundary(value: &str, mut index: usize) -> usize {
    while index > 0 && !value.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn kill_process_tree(pid: u32) {
    if cfg!(windows) {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .status();
    } else {
        let process_group = format!("-{pid}");
        let _ = std::process::Command::new("kill")
            .args(["-TERM", "--", &process_group])
            .status();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(2));
            let process_group = format!("-{pid}");
            let still_running = std::process::Command::new("kill")
                .args(["-0", "--", &process_group])
                .status()
                .map(|status| status.success())
                .unwrap_or(false);
            if still_running {
                let _ = std::process::Command::new("kill")
                    .args(["-KILL", "--", &process_group])
                    .status();
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_csi_and_osc_sequences() {
        let value = "\u{1b}[32mok\u{1b}[0m \u{1b}]0;title\u{7}done";
        assert_eq!(strip_ansi(value), "ok done");
    }

    #[test]
    fn validation_profiles_are_rejected_before_execution() {
        let cache = tempfile::tempdir().unwrap();
        let supervisor = TaskSupervisor::new(
            cache.path().to_path_buf(),
            PolicyConfig {
                max_file_bytes: 1_000_000,
                max_context_chars: 50_000,
                max_search_results: 100,
                max_task_output_chars: 30_000,
                shell_enabled: false,
                allowed_commands: vec!["cargo".to_owned()],
                task_retention_hours: None,
            },
            HashMap::new(),
        )
        .unwrap();
        let error = supervisor
            .validate_profiles(&["typecheck".to_owned()])
            .unwrap_err();
        assert_eq!(error.0.code, "UNKNOWN_VALIDATION_PROFILE");
        let details = error.0.details.unwrap();
        assert_eq!(details["missing"], json!(["typecheck"]));
        assert_eq!(details["available"], json!([]));
    }

    #[test]
    fn later_success_suppresses_same_command_failure() {
        let cache = tempfile::tempdir().unwrap();
        let supervisor = TaskSupervisor::new(
            cache.path().to_path_buf(),
            PolicyConfig {
                max_file_bytes: 1_000_000,
                max_context_chars: 50_000,
                max_search_results: 100,
                max_task_output_chars: 30_000,
                shell_enabled: false,
                allowed_commands: vec!["cargo".to_owned()],
                task_retention_hours: None,
            },
            HashMap::new(),
        )
        .unwrap();
        let command = vec!["cargo".to_owned(), "test".to_owned()];
        let cwd = cache.path().to_path_buf();
        let failed_at = Utc::now() - ChronoDuration::seconds(2);
        let succeeded_at = Utc::now() - ChronoDuration::seconds(1);
        supervisor.tasks.lock().insert(
            "failed".to_owned(),
            Arc::new(Mutex::new(TaskRecord {
                task_id: "failed".to_owned(),
                status: "failed".to_owned(),
                command: command.clone(),
                cwd: cwd.clone(),
                started_at: failed_at,
                ended_at: Some(failed_at),
                exit_code: Some(1),
                output: "compile error".to_owned(),
                log_path: cache.path().join("failed.log"),
                pid: None,
            })),
        );
        supervisor.tasks.lock().insert(
            "succeeded".to_owned(),
            Arc::new(Mutex::new(TaskRecord {
                task_id: "succeeded".to_owned(),
                status: "succeeded".to_owned(),
                command,
                cwd,
                started_at: succeeded_at,
                ended_at: Some(succeeded_at),
                exit_code: Some(0),
                output: "tests passed".to_owned(),
                log_path: cache.path().join("succeeded.log"),
                pid: None,
            })),
        );
        assert!(supervisor.recent_failures("compile error", 3).is_empty());
    }

    #[test]
    fn task_errors_finalize_retained_records() {
        let now = Utc::now();
        let record = Arc::new(Mutex::new(TaskRecord {
            task_id: "failed".to_owned(),
            status: "running".to_owned(),
            command: vec!["cargo".to_owned(), "test".to_owned()],
            cwd: PathBuf::from("."),
            started_at: now,
            ended_at: None,
            exit_code: None,
            output: String::new(),
            log_path: PathBuf::from("missing.log"),
            pid: Some(123),
        }));
        finalize_task_error(
            &record,
            &AppError::new("TASK_LOG_FAILED", "Access is denied"),
        );
        let record = record.lock();
        assert_eq!(record.status, "failed");
        assert!(record.ended_at.is_some());
        assert_eq!(record.pid, None);
        assert!(record.output.contains("Access is denied"));
    }

    #[test]
    fn read_log_falls_back_to_in_memory_output() {
        let cache = tempfile::tempdir().unwrap();
        let supervisor = TaskSupervisor::new(
            cache.path().to_path_buf(),
            PolicyConfig {
                max_file_bytes: 1_000_000,
                max_context_chars: 50_000,
                max_search_results: 100,
                max_task_output_chars: 30_000,
                shell_enabled: false,
                allowed_commands: vec!["cargo".to_owned()],
                task_retention_hours: None,
            },
            HashMap::new(),
        )
        .unwrap();
        supervisor.tasks.lock().insert(
            "memory-only".to_owned(),
            Arc::new(Mutex::new(TaskRecord {
                task_id: "memory-only".to_owned(),
                status: "succeeded".to_owned(),
                command: vec!["cargo".to_owned(), "test".to_owned()],
                cwd: cache.path().to_path_buf(),
                started_at: Utc::now(),
                ended_at: Some(Utc::now()),
                exit_code: Some(0),
                output: "tests passed".to_owned(),
                log_path: cache.path().join("missing.log"),
                pid: None,
            })),
        );
        assert_eq!(supervisor.read_log("memory-only").unwrap(), "tests passed");
    }

    #[test]
    fn trimming_expired_tasks_removes_their_log_files() {
        let cache = tempfile::tempdir().unwrap();
        let supervisor = TaskSupervisor::new(
            cache.path().to_path_buf(),
            PolicyConfig {
                max_file_bytes: 1_000_000,
                max_context_chars: 50_000,
                max_search_results: 100,
                max_task_output_chars: 30_000,
                shell_enabled: false,
                allowed_commands: vec!["cargo".to_owned()],
                task_retention_hours: None,
            },
            HashMap::new(),
        )
        .unwrap();
        let log_path = cache.path().join("task-logs").join("expired.log");
        fs::write(&log_path, "old output").unwrap();
        let ended = Utc::now() - ChronoDuration::hours(TASK_RETENTION_HOURS + 1);
        supervisor.tasks.lock().insert(
            "expired".to_owned(),
            Arc::new(Mutex::new(TaskRecord {
                task_id: "expired".to_owned(),
                status: "succeeded".to_owned(),
                command: vec!["cargo".to_owned(), "test".to_owned()],
                cwd: cache.path().to_path_buf(),
                started_at: ended,
                ended_at: Some(ended),
                exit_code: Some(0),
                output: String::new(),
                log_path: log_path.clone(),
                pid: None,
            })),
        );

        supervisor.trim_tasks();

        assert!(!supervisor.tasks.lock().contains_key("expired"));
        assert!(!log_path.exists());
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
