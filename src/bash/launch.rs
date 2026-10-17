//! Starting a command: the seam between the run supervisor and the operating
//! system. The shell launcher spawns real Bash; tests use a scripted fake.
use crate::model::{AppError, AppResult};
use crate::process_runtime::{ProcessGroup, SpawnedGroup};
use serde_json::json;
use std::fmt;
use std::future::Future;
use std::io;
use std::path::Path;
use std::pin::Pin;
use std::process::Stdio;
use tokio::io::AsyncRead;
use tokio::process::Command;

/// How a launched process exited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProcessExit {
    pub(crate) code: Option<i32>,
    pub(crate) success: bool,
}

pub(crate) type ExitFuture = Pin<Box<dyn Future<Output = io::Result<ProcessExit>> + Send>>;
pub(crate) type OutputPipe = Box<dyn AsyncRead + Send + Unpin>;

/// A started process: its pipes, a future for its exit, and its group.
pub(crate) struct Launched {
    pub(crate) stdout: OutputPipe,
    pub(crate) stderr: OutputPipe,
    /// Resolves when the process exits. Dropping it kills the process.
    pub(crate) exit: ExitFuture,
    pub(crate) group: ProcessGroup,
    pub(crate) warning: Option<String>,
}

pub(crate) trait Launcher: Send + Sync + fmt::Debug {
    fn launch(&self, command: &str, cwd: &Path) -> AppResult<Launched>;
}

/// Runs commands with `bash -c` in a new process group.
#[derive(Debug)]
pub(crate) struct ShellLauncher {
    executable: String,
}

impl ShellLauncher {
    pub(crate) fn new(executable: String) -> Self {
        Self { executable }
    }
}

impl Launcher for ShellLauncher {
    fn launch(&self, command: &str, cwd: &Path) -> AppResult<Launched> {
        let mut process = Command::new(&self.executable);
        process
            .args(["-c", command])
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let SpawnedGroup {
            mut child,
            group,
            warning,
        } = ProcessGroup::spawn(&mut process).map_err(|error| {
            AppError::details(
                "BASH_START_FAILED",
                error.to_string(),
                json!({"bash_executable": self.executable, "command": command, "cwd": cwd}),
            )
        })?;
        let pipe = |missing: &str| {
            AppError::new(
                "BASH_PIPE_FAILED",
                format!("Bash {missing} pipe is unavailable"),
            )
        };
        let stdout = child.stdout.take().ok_or_else(|| pipe("stdout"))?;
        let stderr = child.stderr.take().ok_or_else(|| pipe("stderr"))?;
        Ok(Launched {
            stdout: Box::new(stdout),
            stderr: Box::new(stderr),
            exit: Box::pin(async move {
                child.wait().await.map(|status| ProcessExit {
                    code: status.code(),
                    success: status.success(),
                })
            }),
            group,
            warning,
        })
    }
}

#[cfg(test)]
pub(crate) mod fake {
    use super::{Launched, Launcher, ProcessExit};
    use crate::model::AppResult;
    use crate::process_runtime::fake::{fake_group, FakeEvents, FakeReaction};
    use std::path::Path;

    /// Launches scripted processes that print fixed output and react to stop
    /// requests as configured. Every launch and group event is recorded.
    #[derive(Debug)]
    pub(crate) struct FakeLauncher {
        pub(crate) stdout: &'static str,
        pub(crate) stderr: &'static str,
        pub(crate) reaction: FakeReaction,
        pub(crate) events: FakeEvents,
    }

    impl FakeLauncher {
        pub(crate) fn new(stdout: &'static str, reaction: FakeReaction) -> Self {
            Self {
                stdout,
                stderr: "",
                reaction,
                events: FakeEvents::default(),
            }
        }

        pub(crate) fn events(&self) -> Vec<&'static str> {
            self.events.lock().clone()
        }
    }

    impl Launcher for FakeLauncher {
        fn launch(&self, _command: &str, _cwd: &Path) -> AppResult<Launched> {
            self.events.lock().push("launch");
            let (group, exited) = fake_group(self.reaction, self.events.clone());
            Ok(Launched {
                stdout: Box::new(std::io::Cursor::new(self.stdout.as_bytes())),
                stderr: Box::new(std::io::Cursor::new(self.stderr.as_bytes())),
                exit: Box::pin(async move {
                    // A dropped sender means the group was dropped: killed.
                    let code = exited.await.unwrap_or(None);
                    Ok(ProcessExit {
                        code,
                        success: code == Some(0),
                    })
                }),
                group,
                warning: None,
            })
        }
    }
}
