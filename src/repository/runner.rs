//! One bounded `git` process: spawn it with prompts disabled, wait for it
//! under a timeout (stopping its whole process tree on expiry), and classify a
//! failure into a typed [`GitError`].

use crate::error::{AppError, ErrorCode};
use crate::process_runtime::ProcessTree;
use serde_json::json;
use std::io::Read;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// Default wall-clock bound for every git invocation. Without it a hung
/// credential prompt, a stuck lock, or an unresponsive remote wedges the
/// workspace indefinitely. Push reaches the network and keeps the longer
/// [`GIT_PUSH_TIMEOUT`].
pub(super) const GIT_DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
pub(super) const GIT_PUSH_TIMEOUT: Duration = Duration::from_secs(120);

/// Why a `git` invocation did not succeed.
#[derive(Debug)]
pub(super) enum GitFailure {
    /// The executable could not be started.
    Unavailable { message: String },
    /// The process outlived its timeout and was stopped.
    TimedOut { timeout: Duration },
    /// Git reported that the directory is not inside a repository.
    NotRepository { stderr: String },
    /// Git exited unsuccessfully for another reason.
    Failed {
        exit_code: Option<i32>,
        stderr: String,
    },
    /// Waiting on the process or reading its output failed.
    Wait { message: String },
}

/// A failed `git` invocation and the arguments it ran with.
#[derive(Debug)]
pub(super) struct GitError {
    pub(super) args: Vec<String>,
    pub(super) failure: GitFailure,
}

impl From<GitError> for AppError {
    fn from(error: GitError) -> Self {
        let args = error.args;
        match error.failure {
            GitFailure::Unavailable { message } => {
                AppError::with_details(ErrorCode::GitUnavailable, message, json!({"args": args}))
            }
            GitFailure::TimedOut { timeout } => AppError::with_details(
                ErrorCode::GitTimeout,
                "Git operation timed out",
                json!({"args": args, "timeout_ms": u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX)}),
            ),
            GitFailure::NotRepository { stderr } => AppError::with_details(
                ErrorCode::GitFailed,
                stderr,
                json!({"args": args, "exit_code": NOT_REPOSITORY_EXIT}),
            ),
            GitFailure::Failed { exit_code, stderr } => AppError::with_details(
                ErrorCode::GitFailed,
                stderr,
                json!({"args": args, "exit_code": exit_code}),
            ),
            GitFailure::Wait { message } => {
                AppError::with_details(ErrorCode::GitFailed, message, json!({"args": args}))
            }
        }
    }
}

/// Git's exit code for fatal errors, including "not a git repository".
const NOT_REPOSITORY_EXIT: i32 = 128;

/// Classify an unsuccessful exit from its code and standard error. This is the
/// one place that reads Git's error text.
fn classify(exit_code: Option<i32>, stderr: &str) -> GitFailure {
    let stderr = stderr.trim().to_owned();
    if exit_code == Some(NOT_REPOSITORY_EXIT)
        && stderr.to_ascii_lowercase().contains("not a git repository")
    {
        GitFailure::NotRepository { stderr }
    } else {
        GitFailure::Failed { exit_code, stderr }
    }
}

/// Run `git <args>` in `root` and return its standard output. Standard error
/// from a successful run is logged as a warning.
pub(super) fn run_git(
    root: &Path,
    args: &[String],
    timeout: Duration,
    executable: &str,
) -> Result<Vec<u8>, GitError> {
    let fail = |failure| GitError {
        args: args.to_vec(),
        failure,
    };
    let child = command(root, args, executable).spawn().map_err(|error| {
        fail(GitFailure::Unavailable {
            message: error.to_string(),
        })
    })?;
    let output = wait_for_output(child, timeout).map_err(fail)?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        return Err(fail(classify(output.status.code(), &stderr)));
    }
    if !stderr.trim().is_empty() {
        tracing::warn!(?args, stderr = %stderr.trim(), "git wrote to standard error");
    }
    Ok(output.stdout)
}

/// Build a git command with stdio piped, credential prompts disabled, and
/// interactive askpass suppressed so a missing or expired credential fails
/// fast instead of blocking on a hidden prompt until the timeout fires.
fn command(root: &Path, args: &[String], executable: &str) -> Command {
    let mut command = Command::new(executable);
    command
        .current_dir(root)
        .arg("--no-pager")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Fail closed on any credential/prompt path rather than hang:
        // - GIT_TERMINAL_PROMPT=0 turns terminal prompts into errors.
        // - GIT_ASKPASS=echo makes the graphical/askpass path return
        //   immediately (empty), so no dialog can wedge the workspace.
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "echo");
    // `ProcessTree` stops the whole group on timeout.
    #[cfg(unix)]
    command.process_group(0);
    command
}

struct Output {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Wait for `child` while draining both pipes; past `timeout`, stop its
/// process tree.
fn wait_for_output(mut child: Child, timeout: Duration) -> Result<Output, GitFailure> {
    let tree = ProcessTree::attach(child.id());
    let wait_failure = |message: &str| GitFailure::Wait {
        message: message.to_owned(),
    };
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| wait_failure("git stdout was not piped"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| wait_failure("git stderr was not piped"))?;
    let stdout_reader = thread::spawn(move || read_all(stdout));
    let stderr_reader = thread::spawn(move || read_all(stderr));
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if started.elapsed() < timeout => thread::sleep(Duration::from_millis(10)),
            Ok(None) => break Err(GitFailure::TimedOut { timeout }),
            Err(error) => {
                break Err(GitFailure::Wait {
                    message: error.to_string(),
                })
            }
        }
    };
    if status.is_err() {
        tree.terminate();
        let _ = child.wait();
    }
    let stdout = stdout_reader.join();
    let stderr = stderr_reader.join();
    let status = status?;
    let collect = |joined: thread::Result<std::io::Result<Vec<u8>>>, stream: &str| {
        joined
            .map_err(|_| wait_failure(&format!("git {stream} reader panicked")))?
            .map_err(|error| GitFailure::Wait {
                message: error.to_string(),
            })
    };
    Ok(Output {
        status,
        stdout: collect(stdout, "stdout")?,
        stderr: collect(stderr, "stderr")?,
    })
}

fn read_all(mut stream: impl Read) -> std::io::Result<Vec<u8>> {
    let mut output = Vec::new();
    stream.read_to_end(&mut output).map(|_| output)
}

/// `text` cut to at most `max_chars` bytes on a character boundary, ending in
/// a truncation marker when it was cut.
pub(super) fn truncate_output(mut text: String, max_chars: usize) -> String {
    const MARKER: &str = "\n… output truncated …";
    if text.len() <= max_chars {
        return text;
    }

    let marker = if MARKER.len() <= max_chars {
        MARKER
    } else {
        ""
    };
    let mut end = max_chars.saturating_sub(marker.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push_str(marker);
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_executable_is_unavailable() {
        let root = tempfile::tempdir().unwrap();
        let error = run_git(
            root.path(),
            &["status".to_owned()],
            GIT_DEFAULT_TIMEOUT,
            "definitely-missing-codeweave-git",
        )
        .unwrap_err();

        assert!(matches!(error.failure, GitFailure::Unavailable { .. }));
        assert_eq!(AppError::from(error).code(), ErrorCode::GitUnavailable);
    }

    #[test]
    fn exit_status_and_stderr_classify_the_failure() {
        let outside = classify(
            Some(128),
            "fatal: not a git repository (or any of the parent directories): .git\n",
        );
        assert!(matches!(
            &outside,
            GitFailure::NotRepository { stderr }
                if stderr == "fatal: not a git repository (or any of the parent directories): .git"
        ));

        let failed = classify(Some(1), "error: pathspec 'x' did not match\n");
        assert!(matches!(
            &failed,
            GitFailure::Failed { exit_code: Some(1), stderr } if stderr == "error: pathspec 'x' did not match"
        ));
        let error = AppError::from(GitError {
            args: vec!["add".to_owned()],
            failure: failed,
        });
        assert_eq!(error.code(), ErrorCode::GitFailed);
        assert_eq!(error.message(), "error: pathspec 'x' did not match");
        assert_eq!(
            error.details(),
            Some(&json!({"args": ["add"], "exit_code": 1}))
        );
    }

    #[test]
    fn a_run_past_its_timeout_is_stopped_and_reported() {
        let mut command = if cfg!(windows) {
            let mut command = Command::new("powershell.exe");
            command.args(["-NoProfile", "-NonInteractive", "-Command", "Start-Sleep 5"]);
            command
        } else {
            let mut command = Command::new("sh");
            command.args(["-c", "sleep 5"]);
            command
        };
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        #[cfg(unix)]
        command.process_group(0);
        let started = Instant::now();

        let failure = wait_for_output(command.spawn().unwrap(), Duration::from_millis(50))
            .err()
            .unwrap();

        assert!(matches!(failure, GitFailure::TimedOut { .. }));
        assert!(started.elapsed() < Duration::from_secs(4));
        let error = AppError::from(GitError {
            args: vec!["test-timeout".to_owned()],
            failure,
        });
        assert_eq!(error.code(), ErrorCode::GitTimeout);
        assert_eq!(
            error.details(),
            Some(&json!({"args": ["test-timeout"], "timeout_ms": 50}))
        );
    }

    #[test]
    fn truncation_keeps_a_character_boundary_and_marks_the_cut() {
        assert_eq!(truncate_output("short".to_owned(), 10), "short");
        let cut = truncate_output("é".repeat(40), 30);
        assert!(cut.len() <= 30);
        assert_eq!(cut, "éé\n… output truncated …");
    }
}
