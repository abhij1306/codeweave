//! Git access behind one trait, [`RepositoryBackend`].
//!
//! The only adapter, [`CliGitBackend`], drives the `git` executable. It is
//! split by concern: `runner` starts one bounded `git` process and classifies
//! its failure into a typed `GitError`, `porcelain` parses
//! `git status --porcelain=v2`, `diff` parses unified diffs into structured
//! hunks, and `cli` maps each backend operation onto `git` arguments.

mod cli;
mod diff;
mod porcelain;
mod runner;

pub use cli::CliGitBackend;
pub use diff::{Diff, DiffHunk, LineSpan};

use crate::error::AppResult;
use serde::Serialize;
use std::path::Path;

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
    /// The working-tree (or, with `staged`, the index) diff for `paths`, at
    /// most `max_chars` long, with its hunks parsed. Untracked files named in
    /// `paths` appear as new-file patches.
    fn diff(
        &self,
        root: &Path,
        staged: bool,
        paths: &[String],
        max_chars: usize,
    ) -> AppResult<Diff>;
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
