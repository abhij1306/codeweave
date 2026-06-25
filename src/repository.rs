use crate::model::{AppError, AppResult};
use serde::Serialize;
use serde_json::json;
use std::collections::HashSet;
use std::path::Path;
use std::process::{Command, Output, Stdio};

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
    fn run_raw(&self, root: &Path, args: &[String]) -> AppResult<Output> {
        Command::new("git")
            .current_dir(root)
            .arg("--no-pager")
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|error| {
                AppError::details("GIT_UNAVAILABLE", error.to_string(), json!({"args": args}))
            })
    }

    fn run(&self, root: &Path, args: &[String], max_chars: usize) -> AppResult<String> {
        let output = self.run_raw(root, args)?;
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
                    if let Some(path) = porcelain_path(record, 9) {
                        dirty.insert(path);
                    }
                }
                Some(b'2') => {
                    if let Some(path) = porcelain_path(record, 10) {
                        dirty.insert(path);
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
                    if let Some(path) = porcelain_path(record, 11) {
                        dirty.insert(path);
                    }
                }
                Some(b'?') => {
                    if let Some(path) = record.strip_prefix("? ").and_then(normalized_path) {
                        dirty.insert(path);
                    }
                }
                _ => {}
            }
        }
        index += 1;
    }

    status.dirty_files = dirty.into_iter().collect();
    status.dirty_files.sort();
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

#[cfg(test)]
mod tests {
    use super::*;

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
    }

    #[test]
    fn parses_unborn_detached_status_without_fake_values() {
        let status =
            parse_porcelain_v2("# branch.oid (initial)\0# branch.head (detached)\0? new.rs\0");

        assert!(status.is_git);
        assert!(status.head.is_empty());
        assert!(status.branch.is_empty());
        assert_eq!(status.dirty_files, vec!["new.rs"]);
    }
}
