use super::{
    head_chars, tail_chars, ExecutionGuard, ExecutionRequest, RunCompletionObserver, RunRecord,
};
use crate::model::{AppError, AppResult, OutputFilter};
use crate::process_runtime::{
    render_preview, stream_output, terminate_process_tree, WarmShell, WindowsJob,
};
use chrono::Utc;
use parking_lot::Mutex;
use serde_json::json;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::process::Stdio;
use std::sync::Arc;
use tokio::process::Command;
use tokio::time::{timeout, Duration};

const OUTPUT_DRAIN_TIMEOUT_SECS: u64 = 10;

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

pub(super) async fn execute_warm(
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

    let mut execution_guard = ExecutionGuard::new(record.clone(), shell.pid(), shell.job());
    {
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

pub(super) async fn execute(
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

pub(super) fn finalize_run_error(
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
