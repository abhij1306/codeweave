//! [`RepositoryBackend`] over the `git` executable.

use super::diff::Diff;
use super::porcelain::{normalized_path, parse_porcelain_v2};
use super::runner::{run_git, truncate_output, GitFailure, GIT_DEFAULT_TIMEOUT, GIT_PUSH_TIMEOUT};
use super::{RepoStatus, RepositoryBackend};
use crate::error::{AppError, AppResult, ErrorCode};
use crate::security::{RelPath, WorkspaceRoot};
use serde_json::json;
use std::path::Path;
use std::time::Duration;

#[derive(Debug, Default)]
pub struct CliGitBackend;

impl CliGitBackend {
    /// Require `root` to be inside a Git worktree: `GIT_UNAVAILABLE` when the
    /// executable cannot start, `NOT_GIT_REPOSITORY` otherwise.
    pub fn require_worktree(root: &Path) -> AppResult<()> {
        Self::require_worktree_with(root, "git")
    }

    fn require_worktree_with(root: &Path, executable: &str) -> AppResult<()> {
        let args = ["rev-parse".to_owned(), "--is-inside-work-tree".to_owned()];
        let not_repository = |stderr: &str| {
            AppError::with_details(
                ErrorCode::NotGitRepository,
                "workspace.path must be a Git worktree",
                json!({"path": root, "stderr": stderr}),
            )
        };
        match run_git(root, &args, GIT_DEFAULT_TIMEOUT, executable) {
            Ok(stdout) if String::from_utf8_lossy(&stdout).trim() == "true" => Ok(()),
            Ok(_) => Err(not_repository("")),
            Err(error) => match error.failure {
                GitFailure::Unavailable { message } => Err(AppError::with_details(
                    ErrorCode::GitUnavailable,
                    message,
                    json!({"command": "git rev-parse --is-inside-work-tree"}),
                )),
                GitFailure::NotRepository { stderr } | GitFailure::Failed { stderr, .. } => {
                    Err(not_repository(&stderr))
                }
                failure => Err(super::runner::GitError {
                    args: args.to_vec(),
                    failure,
                }
                .into()),
            },
        }
    }

    fn run(root: &Path, args: &[String], max_chars: usize) -> AppResult<String> {
        Self::run_with_timeout(root, args, max_chars, GIT_DEFAULT_TIMEOUT)
    }

    fn run_with_timeout(
        root: &Path,
        args: &[String],
        max_chars: usize,
        timeout: Duration,
    ) -> AppResult<String> {
        let stdout = run_git(root, args, timeout, "git")?;
        Ok(truncate_output(
            String::from_utf8_lossy(&stdout).into_owned(),
            max_chars,
        ))
    }

    /// New-file patches, each with one hunk, for the untracked files among
    /// `paths`, read through the root handle. Symlinks are shown by target and
    /// never followed.
    fn untracked_diff(
        root: &WorkspaceRoot,
        paths: &[RelPath],
        max_chars: usize,
    ) -> AppResult<String> {
        let mut args = vec![
            "ls-files".to_owned(),
            "--others".to_owned(),
            "--exclude-standard".to_owned(),
            "-z".to_owned(),
            "--".to_owned(),
        ];
        args.extend(
            paths
                .iter()
                .filter(|path| !path.is_empty())
                .map(RelPath::to_string),
        );
        let untracked = Self::run(root.path(), &args, usize::MAX)?;
        let mut diff = String::new();
        for path in untracked.split('\0').filter(|path| !path.is_empty()) {
            let Some(path) = normalized_path(path).and_then(|path| RelPath::new(&path).ok()) else {
                continue;
            };
            // Both modes are six characters, so the budget does not depend on
            // which one applies.
            let header = |mode: &str| {
                format!("diff --git a/{path} b/{path}\nnew file mode {mode}\n--- /dev/null\n+++ b/{path}\n")
            };
            let remaining =
                max_chars.saturating_sub(diff.len().saturating_add(header("100644").len()));
            let (mode, (content, content_truncated)) =
                if let Ok(Some(target)) = root.read_link(&path) {
                    (
                        "120000",
                        string_prefix(&target.to_string_lossy(), remaining),
                    )
                } else if let Ok(Some((bytes, truncated))) = root.read_prefix(&path, remaining) {
                    let Some(content) = utf8_prefix(bytes, truncated) else {
                        continue;
                    };
                    ("100644", content)
                } else {
                    continue;
                };
            let header = header(mode);
            diff.push_str(&header);
            match content.lines().count() {
                0 => {}
                1 => diff.push_str("@@ -0,0 +1 @@\n"),
                lines => diff.push_str(&format!("@@ -0,0 +1,{lines} @@\n")),
            }
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

fn string_prefix(text: &str, max_bytes: usize) -> (String, bool) {
    if text.len() <= max_bytes {
        return (text.to_owned(), false);
    }
    (text[..text.floor_char_boundary(max_bytes)].to_owned(), true)
}

/// `bytes` as text. A prefix cut inside a character drops the partial
/// character; any other invalid UTF-8 is `None`.
fn utf8_prefix(mut bytes: Vec<u8>, truncated: bool) -> Option<(String, bool)> {
    let valid_len = match std::str::from_utf8(&bytes) {
        Ok(_) => bytes.len(),
        Err(error) if truncated && error.error_len().is_none() => error.valid_up_to(),
        Err(_) => return None,
    };
    bytes.truncate(valid_len);
    Some((String::from_utf8(bytes).ok()?, truncated))
}

impl RepositoryBackend for CliGitBackend {
    fn status(&self, root: &WorkspaceRoot) -> AppResult<RepoStatus> {
        let args = [
            "status",
            "--porcelain=v2",
            "-z",
            "--branch",
            "--untracked-files=all",
        ]
        .map(str::to_owned);
        match run_git(root.path(), &args, GIT_DEFAULT_TIMEOUT, "git") {
            Ok(stdout) => Ok(parse_porcelain_v2(&String::from_utf8_lossy(&stdout))),
            Err(error) if matches!(error.failure, GitFailure::NotRepository { .. }) => {
                Ok(RepoStatus::default())
            }
            Err(error) => Err(error.into()),
        }
    }

    fn diff(
        &self,
        root: &WorkspaceRoot,
        staged: bool,
        paths: &[RelPath],
        max_chars: usize,
    ) -> AppResult<Diff> {
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
            args.extend(paths.iter().map(RelPath::to_string));
        }
        let mut diff = Self::run(root.path(), &args, max_chars)?;
        if staged || paths.is_empty() || diff.len() >= max_chars {
            return Ok(Diff::parse(diff));
        }
        let untracked = Self::untracked_diff(root, paths, max_chars - diff.len())?;
        if !diff.is_empty() && !untracked.is_empty() && !diff.ends_with('\n') {
            diff.push('\n');
        }
        diff.push_str(&untracked);
        Ok(Diff::parse(truncate_output(diff, max_chars)))
    }

    fn log(&self, root: &WorkspaceRoot, limit: usize) -> AppResult<String> {
        Self::run(
            root.path(),
            &[
                "log".into(),
                format!("-n{limit}"),
                "--date=iso-strict".into(),
                "--pretty=format:%H%x09%ad%x09%an%x09%s".into(),
            ],
            100_000,
        )
    }

    fn show(&self, root: &WorkspaceRoot, reference: &str, max_chars: usize) -> AppResult<String> {
        Self::run(
            root.path(),
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
        root: &WorkspaceRoot,
        path: &RelPath,
        start: Option<usize>,
        end: Option<usize>,
        max_chars: usize,
    ) -> AppResult<String> {
        let mut args = vec!["blame".to_owned(), "--date=short".to_owned()];
        if let (Some(start), Some(end)) = (start, end) {
            args.push(format!("-L{start},{end}"));
        }
        args.push("--".to_owned());
        args.push(path.to_string());
        Self::run(root.path(), &args, max_chars)
    }

    fn stage(&self, root: &WorkspaceRoot, paths: &[RelPath]) -> AppResult<String> {
        if paths.is_empty() {
            return Err(AppError::invalid("git stage requires paths"));
        }
        let mut args = vec!["add".to_owned(), "--".to_owned()];
        args.extend(paths.iter().map(RelPath::to_string));
        Self::run(root.path(), &args, 20_000)?;
        Ok(format!("Staged {} path(s)", paths.len()))
    }

    fn commit(&self, root: &WorkspaceRoot, message: &str) -> AppResult<String> {
        if message.trim().is_empty() {
            return Err(AppError::invalid("git commit requires message"));
        }
        Self::run(
            root.path(),
            &["commit".into(), "-m".into(), message.into()],
            50_000,
        )
    }

    fn restore(&self, root: &WorkspaceRoot, paths: &[RelPath], staged: bool) -> AppResult<String> {
        if paths.is_empty() {
            return Err(AppError::invalid("git restore requires paths"));
        }
        let mut args = vec!["restore".to_owned()];
        if staged {
            args.push("--staged".to_owned());
        }
        args.push("--".to_owned());
        args.extend(paths.iter().map(RelPath::to_string));
        Self::run(root.path(), &args, 20_000)?;
        Ok(format!("Restored {} path(s)", paths.len()))
    }

    fn push(&self, root: &WorkspaceRoot, remote: &str, branch: &str) -> AppResult<String> {
        let mut args = vec!["push".to_owned(), remote.to_owned()];
        if !branch.is_empty() {
            args.push(branch.to_owned());
        }
        Self::run_with_timeout(root.path(), &args, 20_000, GIT_PUSH_TIMEOUT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn workspace(root: &Path) -> WorkspaceRoot {
        WorkspaceRoot::open(root).unwrap()
    }

    fn rel(path: &str) -> RelPath {
        RelPath::new(path).unwrap()
    }

    fn git(root: &Path, args: &[&str]) {
        let args = args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
        CliGitBackend::run(root, &args, 20_000).unwrap();
    }

    #[test]
    fn startup_distinguishes_missing_git_from_non_repository() {
        let root = tempdir().unwrap();
        let unavailable =
            CliGitBackend::require_worktree_with(root.path(), "definitely-missing-codeweave-git")
                .unwrap_err();
        assert_eq!(unavailable.code(), ErrorCode::GitUnavailable);

        let not_repository = CliGitBackend::require_worktree(root.path()).unwrap_err();
        assert_eq!(not_repository.code(), ErrorCode::NotGitRepository);

        git(root.path(), &["init", "-q"]);
        CliGitBackend::require_worktree(root.path()).unwrap();
    }

    #[test]
    fn status_outside_a_repository_is_an_empty_non_git_status() {
        let root = tempdir().unwrap();

        let status = CliGitBackend.status(&workspace(root.path())).unwrap();

        assert_eq!(status, RepoStatus::default());
        assert!(!status.is_git);
    }

    #[test]
    fn diff_returns_synthetic_patch_for_untracked_scoped_file() {
        let root = tempdir().unwrap();
        git(root.path(), &["init", "-q"]);
        fs::write(root.path().join("new.py"), "print('hello')\n").unwrap();

        let diff = CliGitBackend
            .diff(&workspace(root.path()), false, &[rel("new.py")], 20_000)
            .unwrap();

        assert!(diff.text.contains("new file mode 100644"));
        assert!(diff.text.contains("+++ b/new.py"));
        assert!(diff.text.contains("@@ -0,0 +1 @@\n+print('hello')\n"));
        assert_eq!(diff.hunks.len(), 1);
        assert_eq!(diff.hunks[0].path, "new.py");
        assert!(diff.hunks[0].overlaps(1, 1));
    }

    #[test]
    fn diff_combines_tracked_and_untracked_scoped_files() {
        let root = tempdir().unwrap();
        git(root.path(), &["init", "-q"]);
        fs::write(root.path().join("tracked.py"), "print('before')\n").unwrap();
        git(root.path(), &["add", "--", "tracked.py"]);
        fs::write(root.path().join("tracked.py"), "print('after')\n").unwrap();
        fs::write(root.path().join("new.py"), "print('new')\n").unwrap();

        let diff = CliGitBackend
            .diff(
                &workspace(root.path()),
                false,
                &[rel("tracked.py"), rel("new.py")],
                20_000,
            )
            .unwrap();

        assert!(diff.text.contains("+++ b/tracked.py"));
        assert!(diff.text.contains("+++ b/new.py"));
        assert!(diff.text.contains("+print('new')"));
        let hunk_paths = diff
            .hunks
            .iter()
            .map(|hunk| hunk.path.as_str())
            .collect::<Vec<_>>();
        assert_eq!(hunk_paths, ["tracked.py", "new.py"]);
    }

    #[test]
    fn untracked_diff_reads_and_returns_only_the_bounded_prefix() {
        let root = tempdir().unwrap();
        git(root.path(), &["init", "-q"]);
        fs::write(root.path().join("large.txt"), "x".repeat(10_000)).unwrap();

        let diff = CliGitBackend
            .diff(&workspace(root.path()), false, &[rel("large.txt")], 200)
            .unwrap();

        assert!(diff.text.len() <= 200);
        assert!(diff.text.ends_with("… output truncated …"));
    }

    #[cfg(unix)]
    #[test]
    fn untracked_diff_does_not_follow_symlinks() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        let outside = tempdir().unwrap();
        git(root.path(), &["init", "-q"]);
        let secret = outside.path().join("secret.txt");
        fs::write(&secret, "outside secret").unwrap();
        symlink(&secret, root.path().join("linked.txt")).unwrap();

        let diff = CliGitBackend
            .diff(&workspace(root.path()), false, &[rel("linked.txt")], 20_000)
            .unwrap();

        assert!(diff.text.contains("new file mode 120000"));
        assert!(diff
            .text
            .contains(&secret.to_string_lossy().replace('\\', "/")));
        assert!(!diff.text.contains("outside secret"));
    }
}
