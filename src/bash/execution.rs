use super::{head_chars, tail_chars, ExecutionGuard, ExecutionRequest, RunRecord};
use crate::model::{AppError, AppResult};
use crate::process_runtime::{terminate_process_tree, WindowsJob};
use chrono::Utc;
use parking_lot::Mutex;
use serde_json::json;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::time::{timeout, Duration};

const OUTPUT_DRAIN_TIMEOUT_SECS: u64 = 10;

fn finalize_cancelled_before_start(record: &Arc<Mutex<RunRecord>>) -> bool {
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
    true
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
    } = request;
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
            async move { collect_output(stdout, stderr, collector_record, max_output).await },
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

    let mut display_output = record.lock().combined.clone();
    let mut output_truncated = collected.unwrap_or(false);
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
    finalize_record_output(&record, status, exit_code, display_output, output_truncated);
    execution_guard.disarm();
    Ok(())
}

async fn collect_output(
    mut stdout: impl AsyncRead + Unpin,
    mut stderr: impl AsyncRead + Unpin,
    record: Arc<Mutex<RunRecord>>,
    limit: usize,
) -> AppResult<bool> {
    let mut stdout_done = false;
    let mut stderr_done = false;
    let mut truncated = false;
    let mut out = [0_u8; 8192];
    let mut err = [0_u8; 8192];
    let mut stdout_decoder = StreamDecoder::default();
    let mut stderr_decoder = StreamDecoder::default();
    while !stdout_done || !stderr_done {
        tokio::select! {
            read = stdout.read(&mut out), if !stdout_done => {
                let count = read.map_err(AppError::internal)?;
                if count == 0 {
                    stdout_done = true;
                    append_stream(&record, true, &mut stdout_decoder, &[], true, limit, &mut truncated);
                } else {
                    append_stream(&record, true, &mut stdout_decoder, &out[..count], false, limit, &mut truncated);
                }
            }
            read = stderr.read(&mut err), if !stderr_done => {
                let count = read.map_err(AppError::internal)?;
                if count == 0 {
                    stderr_done = true;
                    append_stream(&record, false, &mut stderr_decoder, &[], true, limit, &mut truncated);
                } else {
                    append_stream(&record, false, &mut stderr_decoder, &err[..count], false, limit, &mut truncated);
                }
            }
        }
    }
    Ok(truncated)
}

fn append_stream(
    record: &Arc<Mutex<RunRecord>>,
    stdout: bool,
    decoder: &mut StreamDecoder,
    bytes: &[u8],
    eof: bool,
    limit: usize,
    truncated: &mut bool,
) {
    let mut text = decoder.decode(bytes);
    if eof {
        text.push_str(&decoder.finish());
    }
    if text.is_empty() {
        return;
    }
    let mut item = record.lock();
    let target = if stdout {
        &mut item.stdout
    } else {
        &mut item.stderr
    };
    append_bounded(target, &text, limit, truncated);
    append_bounded(&mut item.combined, &text, limit, truncated);
    item.output = item.combined.clone();
    item.output_truncated = *truncated;
}

#[derive(Default)]
struct StreamDecoder {
    pending_utf8: Vec<u8>,
    ansi: AnsiState,
}

#[derive(Default)]
enum AnsiState {
    #[default]
    Text,
    Escape,
    Csi,
    Osc,
    OscEscape,
}

impl StreamDecoder {
    fn decode(&mut self, bytes: &[u8]) -> String {
        self.pending_utf8.extend_from_slice(bytes);
        let mut consumed = 0;
        let mut decoded = String::new();
        while consumed < self.pending_utf8.len() {
            match std::str::from_utf8(&self.pending_utf8[consumed..]) {
                Ok(valid) => {
                    decoded.push_str(valid);
                    consumed = self.pending_utf8.len();
                }
                Err(error) => {
                    let valid_end = consumed + error.valid_up_to();
                    decoded.push_str(
                        std::str::from_utf8(&self.pending_utf8[consumed..valid_end])
                            .expect("UTF-8 validator identified a valid prefix"),
                    );
                    consumed = valid_end;
                    let Some(error_len) = error.error_len() else {
                        break;
                    };
                    decoded.push('\u{fffd}');
                    consumed += error_len;
                }
            }
        }
        self.pending_utf8.drain(..consumed);
        self.strip_ansi(&decoded)
    }

    fn finish(&mut self) -> String {
        let pending = String::from_utf8_lossy(&self.pending_utf8).into_owned();
        self.pending_utf8.clear();
        let text = self.strip_ansi(&pending);
        self.ansi = AnsiState::Text;
        text
    }

    fn strip_ansi(&mut self, text: &str) -> String {
        let mut output = String::with_capacity(text.len());
        for ch in text.chars() {
            self.ansi = match self.ansi {
                AnsiState::Text if ch == '\u{1b}' => AnsiState::Escape,
                AnsiState::Text => {
                    output.push(ch);
                    AnsiState::Text
                }
                AnsiState::Escape if ch == '[' => AnsiState::Csi,
                AnsiState::Escape if ch == ']' => AnsiState::Osc,
                AnsiState::Escape if ch == '\u{1b}' => AnsiState::Escape,
                AnsiState::Escape => AnsiState::Text,
                AnsiState::Csi if ('@'..='~').contains(&ch) => AnsiState::Text,
                AnsiState::Csi => AnsiState::Csi,
                AnsiState::Osc if ch == '\u{7}' => AnsiState::Text,
                AnsiState::Osc if ch == '\u{1b}' => AnsiState::OscEscape,
                AnsiState::Osc => AnsiState::Osc,
                AnsiState::OscEscape if ch == '\\' => AnsiState::Text,
                AnsiState::OscEscape if ch == '\u{1b}' => AnsiState::OscEscape,
                AnsiState::OscEscape => AnsiState::Osc,
            };
        }
        output
    }
}

fn append_bounded(target: &mut String, text: &str, limit: usize, truncated: &mut bool) {
    target.push_str(text);
    if target.chars().count() > limit {
        *target = tail_chars(target, limit);
        *truncated = true;
    }
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
}

pub(super) fn finalize_run_error(record: &Arc<Mutex<RunRecord>>, error: &AppError) {
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
}

#[cfg(test)]
mod tests {
    use super::StreamDecoder;

    #[test]
    fn stream_decoder_preserves_split_utf8_and_ansi_state() {
        let mut stdout = StreamDecoder::default();
        let mut stderr = StreamDecoder::default();

        assert_eq!(stdout.decode(&[b'a', 0xc3]), "a");
        assert_eq!(stderr.decode(b"err\x1b[3"), "err");
        assert_eq!(stdout.decode(&[0xa9, 0x1b, b'[', b'3']), "é");
        assert_eq!(stderr.decode(b"1mred\x1b[0m"), "red");
        assert_eq!(stdout.decode(b"2mgreen\x1b[0m"), "green");
        assert_eq!(stdout.finish(), "");
        assert_eq!(stderr.finish(), "");
    }

    #[test]
    fn stream_decoder_flushes_incomplete_utf8_at_eof() {
        let mut decoder = StreamDecoder::default();
        assert_eq!(decoder.decode(&[0xe2, 0x82]), "");
        assert_eq!(decoder.finish(), "�");
    }
}
