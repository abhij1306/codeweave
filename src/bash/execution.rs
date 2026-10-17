//! Running one queued record to completion.
use super::launch::{Launched, Launcher};
use super::lifecycle::{ProcessOutcome, RunRecord, RunStatus};
use super::output::Pipe;
use crate::model::{AppError, AppResult};
use parking_lot::Mutex;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::time::{timeout, Duration};

/// How long to wait for the output pipes to close after the process ends.
const OUTPUT_DRAIN_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a process may take to exit after a stop request.
const STOP_WAIT: Duration = Duration::from_secs(5);
/// Time between the polite stop and the forced kill of a process group.
pub(super) const TERMINATE_GRACE: Duration = Duration::from_secs(2);

pub(super) struct ExecutionRequest {
    pub(super) command: String,
    pub(super) cwd: PathBuf,
    pub(super) timeout_ms: u64,
}

/// Marks a run abandoned if its execution task is dropped before the run ends
/// (for example at runtime shutdown). Ending the record releases its process
/// group, which kills the tree. After a normal finish it does nothing.
pub(super) struct AbandonGuard(pub(super) Arc<Mutex<RunRecord>>);

impl Drop for AbandonGuard {
    fn drop(&mut self) {
        self.0.lock().finish(
            RunStatus::Cancelled,
            None,
            Some("Bash run was abandoned; the process tree was terminated.".to_owned()),
        );
    }
}

pub(super) async fn execute(
    record: &Arc<Mutex<RunRecord>>,
    launcher: &dyn Launcher,
    request: ExecutionRequest,
) -> AppResult<()> {
    if record.lock().finish_if_cancelled_before_start() {
        return Ok(());
    }
    let Launched {
        stdout,
        stderr,
        mut exit,
        group,
        warning,
    } = launcher.launch(&request.command, &request.cwd)?;
    let cancel_after_spawn = {
        let mut item = record.lock();
        if let Some(warning) = warning {
            item.add_note(warning);
        }
        item.start_process(group.clone())
    };
    let collector_record = record.clone();
    let mut collector = tokio::spawn(collect_output(stdout, stderr, collector_record));

    if cancel_after_spawn {
        group.terminate(TERMINATE_GRACE);
    }
    let wait = if cancel_after_spawn {
        STOP_WAIT
    } else {
        Duration::from_millis(request.timeout_ms)
    };
    let outcome = match timeout(wait, &mut exit).await {
        Ok(Ok(exit)) => ProcessOutcome::Exited {
            code: exit.code,
            success: exit.success,
        },
        Ok(Err(error)) => ProcessOutcome::WaitFailed(error.to_string()),
        Err(_) => {
            group.terminate(TERMINATE_GRACE);
            let _ = timeout(STOP_WAIT, &mut exit).await;
            ProcessOutcome::TimedOut {
                timeout_ms: request.timeout_ms,
            }
        }
    };

    let collector_warning = match timeout(OUTPUT_DRAIN_TIMEOUT, &mut collector).await {
        Ok(Ok(Ok(()))) => None,
        Ok(Ok(Err(error))) => Some(format!(
            "CodeWeave warning: live Bash logging failed: {}",
            error.0.message
        )),
        Ok(Err(error)) => Some(format!(
            "CodeWeave warning: output collector failed: {error}"
        )),
        Err(_) => {
            collector.abort();
            Some(
                "CodeWeave warning: output pipes did not close after process termination"
                    .to_owned(),
            )
        }
    };
    let mut item = record.lock();
    if let Some(warning) = collector_warning {
        item.add_note(warning);
    }
    item.finish_process(outcome);
    Ok(())
}

async fn collect_output(
    mut stdout: impl AsyncRead + Unpin,
    mut stderr: impl AsyncRead + Unpin,
    record: Arc<Mutex<RunRecord>>,
) -> AppResult<()> {
    let mut stdout_done = false;
    let mut stderr_done = false;
    let mut out = [0_u8; 8192];
    let mut err = [0_u8; 8192];
    let pipe_error = |error: std::io::Error| AppError::new("BASH_PIPE_FAILED", error.to_string());
    while !stdout_done || !stderr_done {
        tokio::select! {
            read = stdout.read(&mut out), if !stdout_done => {
                let count = read.map_err(pipe_error)?;
                let mut item = record.lock();
                if count == 0 {
                    stdout_done = true;
                    item.output_mut().finish(Pipe::Stdout);
                } else {
                    item.output_mut().ingest(Pipe::Stdout, &out[..count]);
                }
            }
            read = stderr.read(&mut err), if !stderr_done => {
                let count = read.map_err(pipe_error)?;
                let mut item = record.lock();
                if count == 0 {
                    stderr_done = true;
                    item.output_mut().finish(Pipe::Stderr);
                } else {
                    item.output_mut().ingest(Pipe::Stderr, &err[..count]);
                }
            }
        }
    }
    Ok(())
}

/// End a run whose execution failed before it could finish normally.
pub(super) fn finalize_run_error(record: &Mutex<RunRecord>, error: &AppError) {
    record.lock().finish(
        RunStatus::Failed,
        None,
        Some(format!(
            "CodeWeave Bash execution failed: {}",
            error.0.message
        )),
    );
}
