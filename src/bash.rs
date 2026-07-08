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
use std::io::Read;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, Stdio};
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::Semaphore;
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

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct BashReadiness {
    pub configured: bool,
    pub configured_executable: String,
    pub resolved_executable: Option<String>,
    pub shell_type: String,
    pub readiness: String,
    pub failure_reason: Option<String>,
}

impl BashReadiness {
    pub fn is_ready(&self) -> bool {
        self.readiness == "ready" && self.resolved_executable.is_some()
    }

    fn executable(&self) -> Option<String> {
        self.resolved_executable.clone()
    }
}

fn bash_readiness(
    configured: &str,
    readiness: &str,
    executable: Option<&Path>,
    failure_reason: Option<String>,
) -> BashReadiness {
    BashReadiness {
        configured: readiness != "disabled",
        configured_executable: configured.to_owned(),
        resolved_executable: executable.map(|path| path.to_string_lossy().into_owned()),
        shell_type: "bash".to_owned(),
        readiness: readiness.to_owned(),
        failure_reason,
    }
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

/// Guards a warm-shell execution running on a detached task. If the task is
/// aborted or panics mid-command (so the marker is never consumed), the guard
/// terminates the shell's process tree and marks the run record terminal. The
/// shell is removed from the shared slot before execution, so a shell whose
/// marker was not consumed can never be reused: its next output would interleave
/// with the previous command's.
struct WarmExecutionGuard {
    record: Arc<Mutex<RunRecord>>,
    pid: Option<u32>,
    job: Option<Arc<WindowsJob>>,
    armed: bool,
}

impl WarmExecutionGuard {
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

impl Drop for WarmExecutionGuard {
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
const OUTPUT_DRAIN_TIMEOUT_SECS: u64 = 10;
const READINESS_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);
const READINESS_PROBE_OUTPUT_CAP: usize = 8 * 1024;

#[derive(Debug, Clone)]
pub struct StartRequest {
    pub command: String,
    pub cwd: Option<String>,
    pub background: Option<bool>,
    pub timeout_ms: Option<u64>,
}

fn resolve_bash(policy: &PolicyConfig) -> BashReadiness {
    let configured = policy.bash.executable.trim();
    if !policy.bash.enabled {
        return bash_readiness(
            configured,
            "disabled",
            None,
            Some("Bash execution is disabled by policy".to_owned()),
        );
    }
    if configured.is_empty() {
        return bash_readiness(
            configured,
            "unavailable",
            None,
            Some("policy.bash.executable must not be empty".to_owned()),
        );
    }

    let mut failures = Vec::new();
    let configured_path = PathBuf::from(configured);
    if configured_path.is_absolute() {
        return match probe_bash(&configured_path) {
            Ok(()) => bash_readiness(configured, "ready", Some(&configured_path), None),
            Err(error) => bash_readiness(
                configured,
                "unavailable",
                None,
                Some(format!("Configured Bash executable is not usable: {error}")),
            ),
        };
    }

    match probe_bash(&configured_path) {
        Ok(()) => {
            return bash_readiness(configured, "ready", Some(&configured_path), None);
        }
        Err(error) => failures.push(format!("{configured}: {error}")),
    }

    if is_default_bash_name(configured) {
        for candidate in discover_bash_candidates(configured) {
            if probe_bash(&candidate).is_ok() {
                return bash_readiness(configured, "ready", Some(&candidate), None);
            }
        }
    }

    let reason = if failures.is_empty() {
        "No usable Bash implementation found".to_owned()
    } else {
        format!(
            "No usable Bash implementation found; readiness probe failures: {}",
            failures.join("; ")
        )
    };
    bash_readiness(configured, "unavailable", None, Some(reason))
}

fn is_default_bash_name(configured: &str) -> bool {
    matches!(
        configured.trim().to_ascii_lowercase().as_str(),
        "bash" | "bash.exe"
    )
}

fn probe_bash(executable: &Path) -> Result<(), String> {
    let mut child = StdCommand::new(executable)
        .args(["-c", "printf codeweave-bash-ready"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| error.to_string())?;
    let started = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut stdout = Vec::new();
                let mut stderr = Vec::new();
                if let Some(mut reader) = child.stdout.take() {
                    let _ = reader
                        .by_ref()
                        .take(READINESS_PROBE_OUTPUT_CAP as u64)
                        .read_to_end(&mut stdout);
                }
                if let Some(mut reader) = child.stderr.take() {
                    let _ = reader
                        .by_ref()
                        .take(READINESS_PROBE_OUTPUT_CAP as u64)
                        .read_to_end(&mut stderr);
                }
                let stdout = String::from_utf8_lossy(&stdout);
                let stderr = String::from_utf8_lossy(&stderr);
                if status.success() && stdout.contains("codeweave-bash-ready") {
                    return Ok(());
                }
                let detail = stderr.trim();
                return Err(if detail.is_empty() {
                    format!("readiness probe exited with status {status}")
                } else {
                    format!("readiness probe exited with status {status}: {detail}")
                });
            }
            Ok(None) if started.elapsed() < READINESS_PROBE_TIMEOUT => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "readiness probe timed out after {} ms",
                    READINESS_PROBE_TIMEOUT.as_millis()
                ));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error.to_string());
            }
        }
    }
}

fn discover_bash_candidates(_configured: &str) -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        discover_windows_bash_candidates()
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

#[cfg(windows)]
fn discover_windows_bash_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    let mut push = |path: PathBuf| {
        if !path.as_os_str().is_empty() && !candidates.iter().any(|existing| existing == &path) {
            candidates.push(path);
        }
    };

    if let Ok(output) = StdCommand::new("where.exe").arg("bash.exe").output() {
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let path = PathBuf::from(line.trim());
            if path.is_file() {
                push(path);
            }
        }
    }

    for root in [
        std::env::var_os("ProgramW6432").map(PathBuf::from),
        std::env::var_os("ProgramFiles").map(PathBuf::from),
        std::env::var_os("ProgramFiles(x86)").map(PathBuf::from),
        std::env::var_os("LocalAppData").map(|value| PathBuf::from(value).join("Programs")),
    ]
    .into_iter()
    .flatten()
    {
        push(root.join("Git").join("bin").join("bash.exe"));
        push(root.join("Git").join("usr").join("bin").join("bash.exe"));
    }

    if let Ok(output) = StdCommand::new("where.exe").arg("git.exe").output() {
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let git = PathBuf::from(line.trim());
            let Some(parent) = git.parent() else {
                continue;
            };
            push(parent.join("..").join("bin").join("bash.exe"));
            push(parent.join("..").join("usr").join("bin").join("bash.exe"));
        }
    }

    candidates
        .into_iter()
        .filter_map(|path| {
            let path = fs::canonicalize(&path).unwrap_or(path);
            path.is_file().then_some(path)
        })
        .collect()
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

    pub async fn start_for_session(
        &self,
        root: &Path,
        session_id: &str,
        request: StartRequest,
    ) -> AppResult<serde_json::Value> {
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
        let background = request.background.unwrap_or(false);
        let run_permit = match self.run_permit.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                // The single run slot is busy. If the caller re-issued an
                // identical command against the same cwd (ChatGPT retries a
                // timed-out call verbatim), hand back the in-flight run so the
                // model polls it instead of flailing on RUN_BUSY.
                if let Some(active) = self.active_run_matching(session_id, &command, &cwd) {
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
        let run_id = format!("run_{}", Uuid::new_v4().simple());
        let log_root = self.cache_root.join("bash-logs");
        let logs = RunLogPaths::new(&log_root, &run_id);
        let record = Arc::new(Mutex::new(RunRecord {
            run_id: run_id.clone(),
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
            .expect("ensure_available guarantees a resolved executable");
        let max_output = self.policy.bash.max_output_chars;
        let completion_observer = self.completion_observer.lock().clone();

        // Execution always runs on a detached task, even for foreground calls.
        // If the client aborts the request (dropping the request future), the
        // command keeps running, the permit stays held until it finishes, and
        // the warm shell cannot be handed to the next call mid-command.
        let warm_shell = self.warm_shell.clone();
        let execution_record = record.clone();
        let execution_completion_observer = completion_observer.clone();
        let mut handle = tokio::spawn(async move {
            let _run_permit = run_permit;
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
        self.runs.lock().values().find_map(|record| {
            let record = record.lock();
            if record.ended_at.is_some() || record.session_id != session_id {
                return None;
            }
            let elapsed_ms = (Utc::now() - record.started_at).num_milliseconds().max(0);
            Some(json!({
                "run_id": record.run_id,
                "command": record.command,
                "elapsed_ms": elapsed_ms,
                "status_fetch": {"kind": "bash_status", "value": record.run_id}
            }))
        })
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

    pub fn recent_failures_for_session(
        &self,
        session_id: &str,
        query: &str,
        limit: usize,
    ) -> Vec<serde_json::Value> {
        self.recent_failures_filtered(Some(session_id), query, limit)
    }

    fn recent_failures_filtered(
        &self,
        session_id: Option<&str>,
        query: &str,
        limit: usize,
    ) -> Vec<serde_json::Value> {
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
            if session_id.is_some_and(|session_id| session_id != record.session_id) {
                continue;
            }
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

fn finalize_cancelled_before_start(
    record: &Arc<Mutex<RunRecord>>,
    completion_observer: Option<&RunCompletionObserver>,
) -> bool {
    let mut item = record.lock();
    if !item.cancel_requested {
        return false;
    }
    let ended_at = Utc::now();
    item.status = "cancelled".to_owned();
    item.ended_at = Some(ended_at);
    item.pid = None;
    item.job = None;
    if !item.output.is_empty() {
        item.output.push('\n');
    }
    item.output
        .push_str("Bash run was cancelled before the process started.");
    if let Some(observer) = completion_observer {
        observer(&item.run_id, ended_at);
    }
    true
}

async fn execute_warm(
    warm_shell: Arc<tokio::sync::Mutex<Option<WarmShell>>>,
    record: Arc<Mutex<RunRecord>>,
    request: ExecutionRequest,
) -> AppResult<()> {
    let ExecutionRequest {
        bash_executable,
        command,
        cwd,
        timeout_ms,
        max_output,
        completion_observer,
    } = request;
    if finalize_cancelled_before_start(&record, completion_observer.as_ref()) {
        return Ok(());
    }

    {
        let mut item = record.lock();
        item.status = "running".to_owned();
        item.started_at = Utc::now();
    }

    let mut shell = {
        let mut slot = warm_shell.lock().await;
        match slot.take() {
            Some(shell) => shell,
            None => WarmShell::spawn(&bash_executable).map_err(|error| {
                AppError::details(
                    "BASH_START_FAILED",
                    error.to_string(),
                    json!({"bash_executable": bash_executable}),
                )
            })?,
        }
    };

    let mut execution_guard = WarmExecutionGuard::new(record.clone(), shell.pid(), shell.job());
    {
        // Expose the shell pid so bash_cancel can terminate a warm run. The
        // shell dies with the command's process group; needs_respawn handles
        // the replacement below.
        let mut item = record.lock();
        item.pid = shell.pid();
        item.job = shell.job();
    }

    let paths = record.lock().logs.clone();
    let outcome = match shell.run(&command, &cwd, timeout_ms, &paths).await {
        Ok(outcome) => outcome,
        Err(error) => {
            execution_guard.disarm();
            return Err(AppError::details(
                "BASH_RUN_FAILED",
                error.to_string(),
                json!({"command": command}),
            ));
        }
    };
    let reuse_shell = !outcome.needs_respawn;

    let cancelled = record.lock().cancel_requested && outcome.status != "succeeded";
    let status = if cancelled {
        "cancelled"
    } else {
        outcome.status
    };

    let (mut display_output, mut output_truncated) = render_preview(
        &paths,
        &OutputFilter::Raw,
        status,
        max_output,
        outcome.limited,
    )
    .await;

    append_status_notes(&mut display_output, status, timeout_ms);
    clamp_display(
        &mut display_output,
        &mut output_truncated,
        status,
        max_output,
    );
    finalize_record_output(
        &record,
        status,
        outcome.exit_code,
        display_output,
        output_truncated,
        completion_observer.as_ref(),
    );
    execution_guard.disarm();
    if reuse_shell {
        *warm_shell.lock().await = Some(shell);
    }
    Ok(())
}

async fn execute(record: Arc<Mutex<RunRecord>>, request: ExecutionRequest) -> AppResult<()> {
    let ExecutionRequest {
        bash_executable,
        command,
        cwd,
        timeout_ms,
        max_output,
        completion_observer,
    } = request;
    if finalize_cancelled_before_start(&record, completion_observer.as_ref()) {
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
    append_status_notes(&mut display_output, status, timeout_ms);
    if let Some(warning) = setup_warning {
        append_note(&mut display_output, &warning);
    }
    clamp_display(
        &mut display_output,
        &mut output_truncated,
        status,
        max_output,
    );
    finalize_record_output(
        &record,
        status,
        exit_code,
        display_output,
        output_truncated,
        completion_observer.as_ref(),
    );
    execution_guard.disarm();
    Ok(())
}

fn append_note(output: &mut String, note: &str) {
    if !output.is_empty() {
        output.push_str("\n\n");
    }
    output.push_str(note);
}

/// Append the standard timeout/cancellation note to a rendered output preview.
/// Shared by the warm-shell and cold-process finalizers so the wording stays in sync.
fn append_status_notes(output: &mut String, status: &str, timeout_ms: u64) {
    match status {
        "timed_out" => append_note(
            output,
            &format!("Bash run exceeded timeout of {timeout_ms} ms; partial output was retained."),
        ),
        "cancelled" => append_note(
            output,
            "Bash run was cancelled; partial output was retained.",
        ),
        _ => {}
    }
}

/// Clamp a rendered preview to `max_output` characters, keeping the tail for
/// failure-like statuses (where the error is at the end) and the head otherwise.
/// Sets `truncated` when it trims. Shared by both finalizers.
fn clamp_display(output: &mut String, truncated: &mut bool, status: &str, max_output: usize) {
    if output.chars().count() > max_output {
        *output = if matches!(status, "failed" | "timed_out" | "cancelled") {
            tail_chars(output, max_output)
        } else {
            head_chars(output, max_output)
        };
        *truncated = true;
    }
}

/// Write the terminal outcome of a run onto its record. Shared by both finalizers.
fn finalize_record_output(
    record: &Arc<Mutex<RunRecord>>,
    status: &str,
    exit_code: Option<i32>,
    display_output: String,
    output_truncated: bool,
    completion_observer: Option<&RunCompletionObserver>,
) {
    let mut item = record.lock();
    let ended_at = Utc::now();
    item.status = status.to_owned();
    item.exit_code = exit_code;
    item.ended_at = Some(ended_at);
    item.pid = None;
    item.job = None;
    item.output = display_output;
    item.output_truncated = output_truncated;
    if let Some(observer) = completion_observer {
        observer(&item.run_id, ended_at);
    }
}

fn finalize_run_error(
    record: &Arc<Mutex<RunRecord>>,
    error: &AppError,
    completion_observer: Option<&RunCompletionObserver>,
) {
    let mut item = record.lock();
    if item.ended_at.is_some() {
        return;
    }
    let ended_at = Utc::now();
    item.status = "failed".to_owned();
    item.exit_code = None;
    item.ended_at = Some(ended_at);
    item.pid = None;
    item.job = None;
    if !item.output.is_empty() {
        item.output.push('\n');
    }
    item.output.push_str(&format!(
        "CodeWeave Bash execution failed: {}",
        error.0.message
    ));
    if let Some(observer) = completion_observer {
        observer(&item.run_id, ended_at);
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
                foreground_budget_ms: 4_000,
                max_timeout_ms: 10_000,
                max_output_chars: 30_000,
                retention_hours: 1,
            },
        }
    }

    fn policy_with_executable(executable: String) -> PolicyConfig {
        let mut policy = policy();
        policy.bash.executable = executable;
        policy
    }

    fn record(cache: &Path, run_id: &str, status: &str, output: &str) -> RunRecord {
        record_for_session(cache, run_id, "test-session", status, output)
    }

    fn record_for_session(
        cache: &Path,
        run_id: &str,
        session_id: &str,
        status: &str,
        output: &str,
    ) -> RunRecord {
        RunRecord {
            run_id: run_id.to_owned(),
            session_id: session_id.to_owned(),
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
    async fn explicit_invalid_bash_path_reports_unavailable_before_starting() {
        let root = tempdir().unwrap();
        let cache = tempdir().unwrap();
        let missing = cache.path().join("not-a-bash.exe");
        let supervisor = BashSupervisor::new(
            cache.path().to_path_buf(),
            policy_with_executable(missing.to_string_lossy().into_owned()),
        )
        .unwrap();
        let readiness = supervisor.readiness();
        assert!(readiness.configured);
        assert_eq!(readiness.readiness, "unavailable");
        assert_eq!(readiness.resolved_executable, None);

        let error = supervisor
            .start_for_session(
                root.path(),
                "test-session",
                StartRequest {
                    command: "printf test".to_owned(),
                    cwd: None,
                    background: None,
                    timeout_ms: None,
                },
            )
            .await
            .unwrap_err();

        assert_eq!(error.0.code, "BASH_UNAVAILABLE");
        assert!(supervisor.retained_run_ids().is_empty());
    }

    #[test]
    fn non_default_relative_bash_name_fails_closed_after_probe_failure() {
        let supervisor = BashSupervisor::new(
            tempdir().unwrap().path().to_path_buf(),
            policy_with_executable("missing-codeweave-bash".to_owned()),
        )
        .unwrap();

        let readiness = supervisor.readiness();

        assert_eq!(readiness.readiness, "unavailable");
        assert_eq!(readiness.resolved_executable, None);
    }

    #[test]
    fn recent_failures_can_be_scoped_to_session() {
        let cache = tempdir().unwrap();
        let supervisor = BashSupervisor::new(cache.path().to_path_buf(), policy()).unwrap();
        supervisor.runs.lock().insert(
            "a".to_owned(),
            Arc::new(Mutex::new(record_for_session(
                cache.path(),
                "a",
                "session-a",
                "failed",
                "alpha failure",
            ))),
        );
        supervisor.runs.lock().insert(
            "b".to_owned(),
            Arc::new(Mutex::new(record_for_session(
                cache.path(),
                "b",
                "session-b",
                "failed",
                "beta failure",
            ))),
        );

        let scoped = supervisor.recent_failures_for_session("session-a", "failure", 10);

        assert_eq!(scoped.len(), 1);
        assert_eq!(scoped[0]["run_id"], "a");
        assert!(scoped[0]["output"].as_str().unwrap().contains("alpha"));
        assert!(!scoped[0]["output"].as_str().unwrap().contains("beta"));
    }

    #[tokio::test]
    async fn cwd_must_exist_inside_workspace() {
        let root = tempdir().unwrap();
        let cache = tempdir().unwrap();
        let supervisor = BashSupervisor::new(cache.path().to_path_buf(), policy()).unwrap();

        let missing = supervisor
            .start_for_session(
                root.path(),
                "test-session",
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
            .start_for_session(
                root.path(),
                "test-session",
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
            .start_for_session(
                root.path(),
                "test-session",
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
            .start_for_session(
                root.path(),
                "test-session",
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
            .start_for_session(
                root.path(),
                "test-session",
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
            .start_for_session(
                root.path(),
                "test-session",
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
            .start_for_session(
                root.path(),
                "test-session",
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
            .start_for_session(
                &root,
                "test-session",
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
            .start_for_session(
                root.path(),
                "test-session",
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
            .output_stream_for_session("test-session", run_id, None, Some("combined"))
            .unwrap();
        for _ in 0..50 {
            if output["output"].as_str().unwrap().contains("started") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
            output = supervisor
                .output_stream_for_session("test-session", run_id, None, Some("combined"))
                .unwrap();
        }
        assert!(output["output"].as_str().unwrap().contains("started"));
        assert_eq!(
            supervisor
                .cancel_for_session("test-session", run_id)
                .unwrap()["status"],
            "cancelling"
        );

        for _ in 0..50 {
            if supervisor.status(run_id).unwrap()["ended_at"].is_string() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(supervisor.status(run_id).unwrap()["status"], "cancelled");
    }

    #[tokio::test]
    async fn client_abort_keeps_run_alive_and_respawns_warm_shell() {
        // Simulates ChatGPT aborting the HTTP request: the request future is
        // dropped mid-command. The detached execution task must keep running,
        // the permit must free once it finishes, and the next warm run must be
        // clean (not interleaved with the abandoned command's output).
        let root = tempdir().unwrap();
        let cache = tempdir().unwrap();
        let mut abort_policy = policy();
        abort_policy.bash.foreground_budget_ms = 20_000;
        abort_policy.bash.default_timeout_ms = 10_000;
        let supervisor =
            Arc::new(BashSupervisor::new(cache.path().to_path_buf(), abort_policy).unwrap());

        // Start a slow command on a task we then abort, mirroring a dropped
        // request future.
        let bg = supervisor.clone();
        let root_path = root.path().to_path_buf();
        let request_task = tokio::spawn(async move {
            bg.start_for_session(
                &root_path,
                "test-session",
                StartRequest {
                    command: "echo abandoned; sleep 5".to_owned(),
                    cwd: None,
                    background: None,
                    timeout_ms: None,
                },
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        request_task.abort();
        let _ = request_task.await;

        // The permit is still held by the detached command, so an identical
        // retry dedupes rather than colliding.
        let retry = supervisor
            .start_for_session(
                root.path(),
                "test-session",
                StartRequest {
                    command: "echo abandoned; sleep 5".to_owned(),
                    cwd: None,
                    background: None,
                    timeout_ms: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(retry["deduplicated"], true);

        // Wait for the abandoned command to finish and free the permit.
        for _ in 0..100 {
            if supervisor.running_count() == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(supervisor.running_count(), 0);

        // The next warm run is clean: only its own output, no leakage.
        let fresh = supervisor
            .start_for_session(
                root.path(),
                "test-session",
                StartRequest {
                    command: "printf fresh-output".to_owned(),
                    cwd: None,
                    background: None,
                    timeout_ms: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(fresh["status"], "succeeded");
        let output = fresh["output"].as_str().unwrap();
        assert!(output.contains("fresh-output"));
        assert!(!output.contains("abandoned"));
    }

    #[tokio::test]
    async fn foreground_budget_auto_promotes_long_command() {
        let root = tempdir().unwrap();
        let cache = tempdir().unwrap();
        let mut promote_policy = policy();
        promote_policy.bash.foreground_budget_ms = 200;
        promote_policy.bash.default_timeout_ms = 10_000;
        let supervisor = BashSupervisor::new(cache.path().to_path_buf(), promote_policy).unwrap();
        let result = supervisor
            .start_for_session(
                root.path(),
                "test-session",
                StartRequest {
                    command: "echo warming; sleep 30".to_owned(),
                    cwd: None,
                    background: None,
                    timeout_ms: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(result["status"], "running");
        assert_eq!(result["detached"], true);
        assert_eq!(result["reason"], "foreground_budget_exceeded");
        let run_id = result["run_id"].as_str().unwrap().to_owned();
        assert!(supervisor
            .cancel_for_session("test-session", &run_id)
            .is_ok());
    }

    #[tokio::test]
    async fn identical_command_dedupes_to_running_run() {
        let root = tempdir().unwrap();
        let cache = tempdir().unwrap();
        let mut dedupe_policy = policy();
        dedupe_policy.bash.foreground_budget_ms = 200;
        dedupe_policy.bash.default_timeout_ms = 10_000;
        let supervisor = BashSupervisor::new(cache.path().to_path_buf(), dedupe_policy).unwrap();
        let request = || StartRequest {
            command: "sleep 30".to_owned(),
            cwd: None,
            background: None,
            timeout_ms: None,
        };
        let first = supervisor
            .start_for_session(root.path(), "test-session", request())
            .await
            .unwrap();
        assert_eq!(first["status"], "running");
        let run_id = first["run_id"].as_str().unwrap().to_owned();

        let retry = supervisor
            .start_for_session(root.path(), "test-session", request())
            .await
            .unwrap();
        assert_eq!(retry["deduplicated"], true);
        assert_eq!(retry["run_id"], run_id);

        // A genuinely different command still gets a busy error carrying the
        // active run so the model can poll or cancel.
        let busy = supervisor
            .start_for_session(
                root.path(),
                "test-session",
                StartRequest {
                    command: "echo other".to_owned(),
                    cwd: None,
                    background: None,
                    timeout_ms: None,
                },
            )
            .await
            .unwrap_err();
        assert_eq!(busy.0.code, "RUN_BUSY");
        let details = busy.0.details.unwrap();
        assert_eq!(details["active_run"]["run_id"], run_id);
        assert!(supervisor
            .cancel_for_session("test-session", &run_id)
            .is_ok());
    }

    #[tokio::test]
    async fn timeout_retains_partial_output() {
        let root = tempdir().unwrap();
        let cache = tempdir().unwrap();
        let mut timeout_policy = policy();
        timeout_policy.bash.default_timeout_ms = 100;
        let supervisor = BashSupervisor::new(cache.path().to_path_buf(), timeout_policy).unwrap();
        let result = supervisor
            .start_for_session(
                root.path(),
                "test-session",
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
                .output_stream_for_session("test-session", "streams", None, Some("stdout"))
                .unwrap()["output"],
            "stdout-only"
        );
        assert_eq!(
            supervisor
                .output_stream_for_session("test-session", "streams", None, Some("stderr"))
                .unwrap()["output"],
            "stderr-only"
        );
    }
}
