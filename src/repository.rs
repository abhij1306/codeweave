use crate::model::{AppError, AppResult};
use crate::process_runtime::{terminate_process_tree, WindowsJob};
use serde::Serialize;
use serde_json::json;
use std::collections::HashSet;
use std::fs;
use std::io::Read;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// Default wall-clock bound for every git invocation. Without it a hung
/// credential prompt, a stuck lock, or an unresponsive remote wedges the
/// workspace actor indefinitely. Push reaches the network and keeps the
/// longer [`GIT_PUSH_TIMEOUT`].
const GIT_DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const GIT_PUSH_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RepoStatus {
    pub is_git: bool,
    pub head: String,
    pub branch: String,
    pub dirty_files: Vec<String>,
    pub staged_files: Vec<String>,
    pub unstaged_files: Vec<String>,
    pub untracked_files: Vec<String>,
    pub partially_staged_files: Vec<String>,
}

pub trait RepositoryBackend: Send + Sync {
    fn status(&self, root: &Path) -> AppResult<RepoStatus>;
    fn diff(
        &self,
        root: &Path,
        staged: bool,
        paths: &[String],
        max_chars: usize,
    ) -> AppResult<String>;
    fn log(&self, root: &Path, limit: usize) -> AppResult<String>;
    fn show(&self, root: &Path, reference: &str, max_chars: usize) -> AppResult<String>;
    fn blame(
        &self,
        root: &Path,
        path: &str,
        start: Option<usize>,
        end: Option<usize>,
        max_chars: usize,
    ) -> AppResult<String>;
    fn stage(&self, root: &Path, paths: &[String]) -> AppResult<String>;
    fn commit(&self, root: &Path, message: &str) -> AppResult<String>;
    fn restore(&self, root: &Path, paths: &[String], staged: bool) -> AppResult<String>;
    fn push(&self, root: &Path, remote: &str, branch: &str) -> AppResult<String>;
}

#[derive(Debug, Default)]
pub struct CliGitBackend;

impl CliGitBackend {
    pub fn require_worktree(root: &Path) -> AppResult<()> {
        Self::require_worktree_with(root, "git")
    }

    fn require_worktree_with(root: &Path, executable: &str) -> AppResult<()> {
        let args = vec!["rev-parse".to_owned(), "--is-inside-work-tree".to_owned()];
        let output = Self
            .run_raw_with_timeout(root, &args, GIT_DEFAULT_TIMEOUT, executable)
            .map_err(|error| {
                if error.0.code == "GIT_UNAVAILABLE" {
                    AppError::details(
                        "GIT_UNAVAILABLE",
                        error.0.message,
                        json!({"command": "git rev-parse --is-inside-work-tree"}),
                    )
                } else {
                    error
                }
            })?;
        if output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "true" {
            Ok(())
        } else {
            Err(AppError::details(
                "NOT_GIT_REPOSITORY",
                "workspace.path must be a Git worktree",
                json!({
                    "path": root,
                    "stderr": String::from_utf8_lossy(&output.stderr).trim()
                }),
            ))
        }
    }

    /// Build a git command with stdio piped, credential prompts disabled, and
    /// interactive askpass suppressed so a missing/expired credential fails fast
    /// instead of blocking on a hidden prompt until the timeout fires.
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
            //   immediately (empty), so no dialog can wedge the actor.
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ASKPASS", "echo");
        #[cfg(unix)]
        command.process_group(0);
        command
    }

    /// Run a git command under a timeout, returning raw `Output`. Every
    /// invocation is bounded so no git call can wedge the workspace actor.
    fn run_raw_with_timeout(
        &self,
        root: &Path,
        args: &[String],
        timeout: Duration,
        executable: &str,
    ) -> AppResult<Output> {
        let child = Self::command(root, args, executable)
            .spawn()
            .map_err(|error| {
                AppError::details("GIT_UNAVAILABLE", error.to_string(), json!({"args": args}))
            })?;
        wait_for_output(child, args, timeout)
    }

    fn run_raw(&self, root: &Path, args: &[String]) -> AppResult<Output> {
        self.run_raw_with_timeout(root, args, GIT_DEFAULT_TIMEOUT, "git")
    }

    fn run(&self, root: &Path, args: &[String], max_chars: usize) -> AppResult<String> {
        let output = self.run_raw(root, args)?;
        checked_output(output, args, max_chars)
    }

    fn run_with_timeout(
        &self,
        root: &Path,
        args: &[String],
        max_chars: usize,
        timeout: Duration,
    ) -> AppResult<String> {
        let output = self.run_raw_with_timeout(root, args, timeout, "git")?;
        checked_output(output, args, max_chars)
    }

    fn untracked_diff(&self, root: &Path, paths: &[String], max_chars: usize) -> AppResult<String> {
        let mut args = vec![
            "ls-files".to_owned(),
            "--others".to_owned(),
            "--exclude-standard".to_owned(),
            "-z".to_owned(),
            "--".to_owned(),
        ];
        args.extend(paths.iter().filter_map(|path| normalized_path(path)));
        let untracked = self.run(root, &args, usize::MAX)?;
        let mut diff = String::new();
        for path in untracked.split('\0').filter(|path| !path.is_empty()) {
            let Some(normalized) = normalized_path(path) else {
                continue;
            };
            let full_path = root.join(&normalized);
            let Ok(metadata) = fs::symlink_metadata(&full_path) else {
                continue;
            };
            let is_symlink = metadata.file_type().is_symlink();
            if !is_symlink && !metadata.is_file() {
                continue;
            }
            let mode = if is_symlink { "120000" } else { "100644" };
            let header = format!(
                "diff --git a/{0} b/{0}\nnew file mode {1}\n--- /dev/null\n+++ b/{0}\n",
                normalized, mode
            );
            let remaining = max_chars.saturating_sub(diff.len().saturating_add(header.len()));
            let content = if is_symlink {
                let Ok(target) = fs::read_link(&full_path) else {
                    continue;
                };
                string_prefix(&target.to_string_lossy(), remaining)
            } else {
                let Some(content) = read_utf8_prefix(&full_path, remaining) else {
                    continue;
                };
                content
            };
            let (content, content_truncated) = content;
            diff.push_str(&header);
            for line in content.lines() {
                diff.push('+');
                diff.push_str(line);
                diff.push('\n');
            }
            if !content_truncated && !content.ends_with('\n') {
                diff.push_str("\\ No newline at end of file\n");
            }
            if content_truncated || diff.len() > max_chars {
                if diff.len() <= max_chars {
                    diff.push('\n');
                }
                return Ok(truncate_output(diff, max_chars));
            }
        }
        Ok(diff)
    }
}

fn checked_output(output: Output, args: &[String], max_chars: usize) -> AppResult<String> {
    let text = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        return Err(AppError::details(
            "GIT_FAILED",
            stderr.trim().to_owned(),
            json!({"args": args, "exit_code": output.status.code()}),
        ));
    }
    if !stderr.trim().is_empty() {
        eprintln!("git warning for {:?}: {}", args, stderr.trim());
    }
    Ok(truncate_output(text, max_chars))
}

fn wait_for_output(mut child: Child, args: &[String], timeout: Duration) -> AppResult<Output> {
    let pid = child.id();
    #[cfg(windows)]
    let job = WindowsJob::assign(pid).ok();
    #[cfg(not(windows))]
    let job: Option<std::sync::Arc<WindowsJob>> = None;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| AppError::internal("git stdout was not piped"))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| AppError::internal("git stderr was not piped"))?;
    let stdout_reader = thread::spawn(move || {
        let mut output = Vec::new();
        stdout.read_to_end(&mut output).map(|_| output)
    });
    let stderr_reader = thread::spawn(move || {
        let mut output = Vec::new();
        stderr.read_to_end(&mut output).map(|_| output)
    });
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < timeout => {
                thread::sleep(Duration::from_millis(10));
            }
            Ok(None) => {
                terminate_process_tree(pid, job.as_deref());
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(AppError::details(
                    "GIT_TIMEOUT",
                    "Git operation timed out",
                    json!({"args": args, "timeout_ms": timeout.as_millis() as u64}),
                ));
            }
            Err(error) => {
                terminate_process_tree(pid, job.as_deref());
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(AppError::details(
                    "GIT_FAILED",
                    error.to_string(),
                    json!({"args": args}),
                ));
            }
        }
    };
    let stdout = stdout_reader
        .join()
        .map_err(|_| AppError::internal("git stdout reader panicked"))??;
    let stderr = stderr_reader
        .join()
        .map_err(|_| AppError::internal("git stderr reader panicked"))??;
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

fn string_prefix(text: &str, max_bytes: usize) -> (String, bool) {
    if text.len() <= max_bytes {
        return (text.to_owned(), false);
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_owned(), true)
}

fn read_utf8_prefix(path: &Path, max_bytes: usize) -> Option<(String, bool)> {
    let file = fs::File::open(path).ok()?;
    let limit = u64::try_from(max_bytes.saturating_add(1)).unwrap_or(u64::MAX);
    let mut bytes = Vec::new();
    file.take(limit).read_to_end(&mut bytes).ok()?;
    let truncated = bytes.len() > max_bytes;
    if truncated {
        bytes.truncate(max_bytes);
    }
    let valid_len = match std::str::from_utf8(&bytes) {
        Ok(_) => bytes.len(),
        Err(error) if truncated && error.error_len().is_none() => error.valid_up_to(),
        Err(_) => return None,
    };
    bytes.truncate(valid_len);
    Some((String::from_utf8(bytes).ok()?, truncated))
}

fn truncate_output(mut text: String, max_chars: usize) -> String {
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

fn normalized_path(path: &str) -> Option<String> {
    let path = path.replace('\\', "/");
    (!path.is_empty()).then_some(path)
}

fn porcelain_path(record: &str, field_count: usize) -> Option<String> {
    record
        .splitn(field_count, ' ')
        .nth(field_count - 1)
        .and_then(normalized_path)
}

fn parse_porcelain_v2(raw: &str) -> RepoStatus {
    let records: Vec<_> = raw
        .split('\0')
        .filter(|record| !record.is_empty())
        .collect();
    let mut status = RepoStatus {
        is_git: true,
        ..RepoStatus::default()
    };
    let mut dirty = HashSet::new();
    let mut staged: HashSet<String> = HashSet::new();
    let mut unstaged: HashSet<String> = HashSet::new();
    let mut untracked: HashSet<String> = HashSet::new();
    let mut index = 0usize;

    while index < records.len() {
        let record = records[index];
        if let Some(oid) = record.strip_prefix("# branch.oid ") {
            if oid != "(initial)" {
                status.head = oid.to_owned();
            }
        } else if let Some(branch) = record.strip_prefix("# branch.head ") {
            if branch != "(detached)" {
                status.branch = branch.to_owned();
            }
        } else {
            match record.as_bytes().first().copied() {
                Some(b'1') => {
                    // "1 XY sub mH mI mW hH hI path"; '.' = unmodified in that dimension
                    if let Some(path) = porcelain_path(record, 9) {
                        dirty.insert(path.clone());
                        let x = record.chars().nth(2).unwrap_or('.');
                        let y = record.chars().nth(3).unwrap_or('.');
                        if x != '.' {
                            staged.insert(path.clone());
                        }
                        if y != '.' {
                            unstaged.insert(path);
                        }
                    }
                }
                Some(b'2') => {
                    // "2 XY sub mH mI mW hH hI X score path"
                    if let Some(path) = porcelain_path(record, 10) {
                        dirty.insert(path.clone());
                        let x = record.chars().nth(2).unwrap_or('.');
                        let y = record.chars().nth(3).unwrap_or('.');
                        if x != '.' {
                            staged.insert(path.clone());
                        }
                        if y != '.' {
                            unstaged.insert(path);
                        }
                    }
                    if let Some(original) = records
                        .get(index + 1)
                        .and_then(|path| normalized_path(path))
                    {
                        dirty.insert(original);
                        index += 1;
                    }
                }
                Some(b'u') => {
                    // Unmerged: always in both staged and unstaged
                    if let Some(path) = porcelain_path(record, 11) {
                        dirty.insert(path.clone());
                        staged.insert(path.clone());
                        unstaged.insert(path);
                    }
                }
                Some(b'?') => {
                    if let Some(path) = record.strip_prefix("? ").and_then(normalized_path) {
                        dirty.insert(path.clone());
                        untracked.insert(path);
                    }
                }
                _ => {}
            }
        }
        index += 1;
    }

    let partially_staged: HashSet<String> = staged.intersection(&unstaged).cloned().collect();
    status.dirty_files = dirty.into_iter().collect();
    status.dirty_files.sort();
    status.staged_files = staged.into_iter().collect();
    status.staged_files.sort();
    status.unstaged_files = unstaged.into_iter().collect();
    status.unstaged_files.sort();
    status.untracked_files = untracked.into_iter().collect();
    status.untracked_files.sort();
    status.partially_staged_files = partially_staged.into_iter().collect();
    status.partially_staged_files.sort();
    status
}

impl RepositoryBackend for CliGitBackend {
    fn status(&self, root: &Path) -> AppResult<RepoStatus> {
        let args = [
            "status",
            "--porcelain=v2",
            "-z",
            "--branch",
            "--untracked-files=all",
        ];
        let args = args.into_iter().map(str::to_owned).collect::<Vec<_>>();
        let output = self.run_raw(root, &args)?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !output.status.success() {
            if stderr.contains("not a git repository") {
                return Ok(RepoStatus::default());
            }
            return Err(AppError::details(
                "GIT_FAILED",
                stderr.trim().to_owned(),
                json!({"args": args, "exit_code": output.status.code()}),
            ));
        }
        if !stderr.trim().is_empty() {
            eprintln!("git warning for {:?}: {}", args, stderr.trim());
        }
        Ok(parse_porcelain_v2(&String::from_utf8_lossy(&output.stdout)))
    }

    fn diff(
        &self,
        root: &Path,
        staged: bool,
        paths: &[String],
        max_chars: usize,
    ) -> AppResult<String> {
        let mut args = vec![
            "diff".to_owned(),
            "--no-ext-diff".to_owned(),
            "--unified=3".to_owned(),
        ];
        if staged {
            args.push("--cached".to_owned());
        }
        if !paths.is_empty() {
            args.push("--".to_owned());
            args.extend(paths.iter().cloned());
        }
        let mut diff = self.run(root, &args, max_chars)?;
        if staged || paths.is_empty() || diff.len() >= max_chars {
            return Ok(diff);
        }
        let untracked = self.untracked_diff(root, paths, max_chars - diff.len())?;
        if !diff.is_empty() && !untracked.is_empty() && !diff.ends_with('\n') {
            diff.push('\n');
        }
        diff.push_str(&untracked);
        Ok(truncate_output(diff, max_chars))
    }
    fn log(&self, root: &Path, limit: usize) -> AppResult<String> {
        self.run(
            root,
            &[
                "log".into(),
                format!("-n{limit}"),
                "--date=iso-strict".into(),
                "--pretty=format:%H%x09%ad%x09%an%x09%s".into(),
            ],
            100_000,
        )
    }
    fn show(&self, root: &Path, reference: &str, max_chars: usize) -> AppResult<String> {
        self.run(
            root,
            &[
                "show".into(),
                "--stat".into(),
                "--patch".into(),
                reference.into(),
            ],
            max_chars,
        )
    }
    fn blame(
        &self,
        root: &Path,
        path: &str,
        start: Option<usize>,
        end: Option<usize>,
        max_chars: usize,
    ) -> AppResult<String> {
        let mut args = vec!["blame".to_owned(), "--date=short".to_owned()];
        if let (Some(start), Some(end)) = (start, end) {
            args.push(format!("-L{start},{end}"));
        }
        args.push("--".to_owned());
        args.push(path.to_owned());
        self.run(root, &args, max_chars)
    }
    fn stage(&self, root: &Path, paths: &[String]) -> AppResult<String> {
        if paths.is_empty() {
            return Err(AppError::invalid("git stage requires paths"));
        }
        let mut args = vec!["add".to_owned(), "--".to_owned()];
        args.extend(paths.iter().cloned());
        self.run(root, &args, 20_000)?;
        Ok(format!("Staged {} path(s)", paths.len()))
    }
    fn commit(&self, root: &Path, message: &str) -> AppResult<String> {
        if message.trim().is_empty() {
            return Err(AppError::invalid("git commit requires message"));
        }
        self.run(
            root,
            &["commit".into(), "-m".into(), message.into()],
            50_000,
        )
    }
    fn restore(&self, root: &Path, paths: &[String], staged: bool) -> AppResult<String> {
        if paths.is_empty() {
            return Err(AppError::invalid("git restore requires paths"));
        }
        let mut args = vec!["restore".to_owned()];
        if staged {
            args.push("--staged".to_owned());
        }
        args.push("--".to_owned());
        args.extend(paths.iter().cloned());
        self.run(root, &args, 20_000)?;
        Ok(format!("Restored {} path(s)", paths.len()))
    }
    fn push(&self, root: &Path, remote: &str, branch: &str) -> AppResult<String> {
        let mut args = vec!["push".to_owned(), remote.to_owned()];
        if !branch.is_empty() {
            args.push(branch.to_owned());
        }
        self.run_with_timeout(root, &args, 20_000, GIT_PUSH_TIMEOUT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn startup_distinguishes_missing_git_from_non_repository() {
        let root = tempdir().unwrap();
        let unavailable =
            CliGitBackend::require_worktree_with(root.path(), "definitely-missing-codeweave-git")
                .unwrap_err();
        assert_eq!(unavailable.0.code, "GIT_UNAVAILABLE");

        let not_repository = CliGitBackend::require_worktree(root.path()).unwrap_err();
        assert_eq!(not_repository.0.code, "NOT_GIT_REPOSITORY");
    }

    #[test]
    fn timed_process_is_terminated_and_reported() {
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
        let child = command.spawn().unwrap();

        let error = wait_for_output(
            child,
            &["test-timeout".to_owned()],
            Duration::from_millis(50),
        )
        .unwrap_err();

        assert_eq!(error.0.code, "GIT_TIMEOUT");
    }

    #[test]
    fn parses_porcelain_v2_branch_and_dirty_paths() {
        let raw = concat!(
            "# branch.oid abc123\0",
            "# branch.head feature/status\0",
            "1 .M N... 100644 100644 100644 aaa bbb src/changed file.rs\0",
            "2 R. N... 100644 100644 100644 aaa bbb R100 src/new name.rs\0",
            "src/old name.rs\0",
            "u UU N... 100644 100644 100644 100644 aaa bbb ccc src/conflict.rs\0",
            "? src/untracked file.rs\0",
        );

        let status = parse_porcelain_v2(raw);

        assert!(status.is_git);
        assert_eq!(status.head, "abc123");
        assert_eq!(status.branch, "feature/status");
        assert_eq!(
            status.dirty_files,
            vec![
                "src/changed file.rs",
                "src/conflict.rs",
                "src/new name.rs",
                "src/old name.rs",
                "src/untracked file.rs",
            ]
        );
        assert_eq!(
            status.staged_files,
            vec!["src/conflict.rs", "src/new name.rs"]
        );
        assert_eq!(
            status.unstaged_files,
            vec!["src/changed file.rs", "src/conflict.rs"]
        );
        assert_eq!(status.untracked_files, vec!["src/untracked file.rs"]);
        assert_eq!(status.partially_staged_files, vec!["src/conflict.rs"]);
    }

    #[test]
    fn parses_unborn_detached_status_without_fake_values() {
        let status =
            parse_porcelain_v2("# branch.oid (initial)\0# branch.head (detached)\0? new.rs\0");

        assert!(status.is_git);
        assert!(status.head.is_empty());
        assert!(status.branch.is_empty());
        assert_eq!(status.dirty_files, vec!["new.rs"]);
        assert!(status.staged_files.is_empty());
        assert!(status.unstaged_files.is_empty());
        assert_eq!(status.untracked_files, vec!["new.rs"]);
        assert!(status.partially_staged_files.is_empty());
    }

    #[test]
    fn diff_returns_synthetic_patch_for_untracked_scoped_file() {
        let root = tempdir().unwrap();
        let backend = CliGitBackend;
        backend
            .run(root.path(), &["init".to_owned()], 20_000)
            .unwrap();
        fs::write(root.path().join("new.py"), "print('hello')\n").unwrap();

        let diff = backend
            .diff(root.path(), false, &["new.py".to_owned()], 20_000)
            .unwrap();

        assert!(diff.contains("new file mode 100644"));
        assert!(diff.contains("+++ b/new.py"));
        assert!(diff.contains("+print('hello')"));
    }

    #[test]
    fn diff_combines_tracked_and_untracked_scoped_files() {
        let root = tempdir().unwrap();
        let backend = CliGitBackend;
        backend
            .run(root.path(), &["init".to_owned()], 20_000)
            .unwrap();
        fs::write(root.path().join("tracked.py"), "print('before')\n").unwrap();
        backend
            .run(
                root.path(),
                &["add".to_owned(), "--".to_owned(), "tracked.py".to_owned()],
                20_000,
            )
            .unwrap();
        fs::write(root.path().join("tracked.py"), "print('after')\n").unwrap();
        fs::write(root.path().join("new.py"), "print('new')\n").unwrap();

        let diff = backend
            .diff(
                root.path(),
                false,
                &["tracked.py".to_owned(), "new.py".to_owned()],
                20_000,
            )
            .unwrap();

        assert!(diff.contains("+++ b/tracked.py"));
        assert!(diff.contains("+++ b/new.py"));
        assert!(diff.contains("+print('new')"));
    }

    #[test]
    fn untracked_diff_reads_and_returns_only_the_bounded_prefix() {
        let root = tempdir().unwrap();
        let backend = CliGitBackend;
        backend
            .run(root.path(), &["init".to_owned()], 20_000)
            .unwrap();
        fs::write(root.path().join("large.txt"), "x".repeat(10_000)).unwrap();

        let diff = backend
            .diff(root.path(), false, &["large.txt".to_owned()], 200)
            .unwrap();

        assert!(diff.len() <= 200);
        assert!(diff.ends_with("… output truncated …"));
    }

    #[cfg(unix)]
    #[test]
    fn untracked_diff_does_not_follow_symlinks() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let backend = CliGitBackend;
        backend
            .run(root.path(), &["init".to_owned()], 20_000)
            .unwrap();
        let secret = outside.path().join("secret.txt");
        fs::write(&secret, "outside secret").unwrap();
        symlink(&secret, root.path().join("linked.txt")).unwrap();

        let diff = backend
            .diff(root.path(), false, &["linked.txt".to_owned()], 20_000)
            .unwrap();

        assert!(diff.contains("new file mode 120000"));
        assert!(diff.contains(&secret.to_string_lossy().replace('\\', "/")));
        assert!(!diff.contains("outside secret"));
    }
}
