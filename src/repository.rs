use crate::model::{AppError, AppResult};
use serde::Serialize;
use serde_json::json;
use std::collections::HashSet;
use std::path::Path;
use std::process::{Command, Stdio};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RepoStatus {
    pub is_git: bool,
    pub head: String,
    pub branch: String,
    pub dirty_files: Vec<String>,
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
}

#[derive(Debug, Default)]
pub struct CliGitBackend;

impl CliGitBackend {
    fn run(&self, root: &Path, args: &[String], max_chars: usize) -> AppResult<String> {
        let output = Command::new("git")
            .current_dir(root)
            .arg("--no-pager")
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| {
                AppError::details("GIT_UNAVAILABLE", e.to_string(), json!({"args": args}))
            })?;
        let mut text = String::from_utf8_lossy(&output.stdout).to_string();
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
        if text.len() > max_chars {
            let mut end = max_chars;
            while end > 0 && !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
            text.push_str("\n… output truncated …");
        }
        Ok(text)
    }

    fn is_repo(&self, root: &Path) -> AppResult<bool> {
        let status = Command::new("git")
            .current_dir(root)
            .args(["rev-parse", "--is-inside-work-tree"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|error| {
                AppError::details(
                    "GIT_UNAVAILABLE",
                    error.to_string(),
                    json!({"command": "git rev-parse --is-inside-work-tree"}),
                )
            })?;
        Ok(status.success())
    }
}

impl RepositoryBackend for CliGitBackend {
    fn status(&self, root: &Path) -> AppResult<RepoStatus> {
        if !self.is_repo(root)? {
            return Ok(RepoStatus::default());
        }
        let head = self
            .run(root, &["rev-parse".into(), "HEAD".into()], 1_000)?
            .trim()
            .to_owned();
        let branch = self
            .run(root, &["branch".into(), "--show-current".into()], 1_000)?
            .trim()
            .to_owned();
        let raw = self.run(
            root,
            &[
                "status".into(),
                "--porcelain=v1".into(),
                "-z".into(),
                "--untracked-files=all".into(),
            ],
            2_000_000,
        )?;
        let mut dirty = HashSet::new();
        let mut records = raw.split('\0').filter(|part| !part.is_empty());
        while let Some(record) = records.next() {
            if record.len() < 4 {
                continue;
            }
            let status = &record[..2];
            let path = record[3..].replace('\\', "/");
            if !path.is_empty() {
                dirty.insert(path);
            }
            if status.contains('R') || status.contains('C') {
                if let Some(destination) = records.next() {
                    let destination = destination.replace('\\', "/");
                    if !destination.is_empty() {
                        dirty.insert(destination);
                    }
                }
            }
        }
        let mut dirty_files: Vec<_> = dirty.into_iter().collect();
        dirty_files.sort();
        Ok(RepoStatus {
            is_git: true,
            head,
            branch,
            dirty_files,
        })
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
        self.run(root, &args, max_chars)
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
}
