use crate::model::PolicyConfig;
use serde::Serialize;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, Stdio};

const READINESS_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);
const READINESS_PROBE_OUTPUT_CAP: usize = 8 * 1024;

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

    pub(super) fn executable(&self) -> Option<String> {
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

pub(super) fn resolve_bash(policy: &PolicyConfig) -> BashReadiness {
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
