mod edit;
mod fetch_search;
mod git;
mod run_reconcile;
mod state;
mod state_module;
mod validation;

use super::edit::PlannedFile;
use super::events::MutationRecord;
use super::state::RunWindow;
use super::util::{
    line_ending_label, line_range_bytes, normalize_line_endings_for_content,
    summarize_changed_paths, MAX_CHANGED_PATH_GROUPS, MAX_OBSERVED_CHANGED_PATHS,
};
use super::{validated_push_target, Workspace};
use crate::index::content_hash;
use crate::model::{AppError, AppResult, BashConfig, PolicyConfig, WorkspaceConfig};
use crate::repository::{RepoStatus, RepositoryBackend};
use crate::test_bash_executable;
use chrono::{Duration as ChronoDuration, Utc};
use parking_lot::Mutex;
use serde_json::json;
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use tempfile::tempdir;

fn test_policy() -> PolicyConfig {
    PolicyConfig {
        max_file_bytes: 1_000_000,
        max_context_chars: 50_000,
        max_search_results: 100,
        bash: BashConfig {
            executable: test_bash_executable(),
            default_timeout_ms: 120_000,
            foreground_budget_ms: 20_000,
            max_timeout_ms: 300_000,
            max_output_chars: 30_000,
        },
    }
}

fn test_actor(root: &Path) -> Arc<Workspace> {
    test_actor_with_exclusions(root, Vec::new())
}

fn test_actor_with_policy(root: &Path, policy: PolicyConfig) -> Arc<Workspace> {
    test_actor_with_policy_and_exclusions(root, policy, Vec::new())
}

fn test_actor_with_policy_and_exclusions(
    root: &Path,
    policy: PolicyConfig,
    exclude_paths: Vec<String>,
) -> Arc<Workspace> {
    let cache = tempdir().unwrap().keep();
    Arc::new(
        Workspace::open(
            &WorkspaceConfig {
                id: "main".to_owned(),
                name: "Main".to_owned(),
                path: root.to_string_lossy().into_owned(),
                artifact_paths: Vec::new(),
                exclude_paths,
            },
            policy,
            cache,
        )
        .unwrap(),
    )
}

/// Repository backend whose `git status` result the test controls. Every other
/// action succeeds with empty output.
struct ScriptedRepository {
    status: Mutex<Option<RepoStatus>>,
}

impl ScriptedRepository {
    fn new(status: RepoStatus) -> Arc<Self> {
        Arc::new(Self {
            status: Mutex::new(Some(status)),
        })
    }

    fn succeed_with(&self, status: RepoStatus) {
        *self.status.lock() = Some(status);
    }

    fn fail(&self) {
        *self.status.lock() = None;
    }
}

impl RepositoryBackend for ScriptedRepository {
    fn status(&self, _root: &Path) -> AppResult<RepoStatus> {
        self.status
            .lock()
            .clone()
            .ok_or_else(|| AppError::new("GIT_FAILED", "scripted status failure"))
    }
    fn diff(&self, _: &Path, _: bool, _: &[String], _: usize) -> AppResult<String> {
        Ok(String::new())
    }
    fn log(&self, _: &Path, _: usize) -> AppResult<String> {
        Ok(String::new())
    }
    fn show(&self, _: &Path, _: &str, _: usize) -> AppResult<String> {
        Ok(String::new())
    }
    fn blame(
        &self,
        _: &Path,
        _: &str,
        _: Option<usize>,
        _: Option<usize>,
        _: usize,
    ) -> AppResult<String> {
        Ok(String::new())
    }
    fn stage(&self, _: &Path, _: &[String]) -> AppResult<String> {
        Ok(String::new())
    }
    fn commit(&self, _: &Path, _: &str) -> AppResult<String> {
        Ok(String::new())
    }
    fn restore(&self, _: &Path, _: &[String], _: bool) -> AppResult<String> {
        Ok(String::new())
    }
    fn push(&self, _: &Path, _: &str, _: &str) -> AppResult<String> {
        Ok(String::new())
    }
}

fn test_actor_with_repository(root: &Path, repository: Arc<ScriptedRepository>) -> Arc<Workspace> {
    let cache = tempdir().unwrap().keep();
    Arc::new(
        Workspace::open_with_repository(
            &WorkspaceConfig {
                id: "main".to_owned(),
                name: "Main".to_owned(),
                path: root.to_string_lossy().into_owned(),
                artifact_paths: Vec::new(),
                exclude_paths: Vec::new(),
            },
            test_policy(),
            cache,
            repository,
        )
        .unwrap(),
    )
}

/// A mutation record for `path` from `source` at `generation`.
fn mutation(path: &str, source: &str, generation: u64) -> MutationRecord {
    MutationRecord {
        mutation_id: format!("mutation-{path}"),
        path: path.to_owned(),
        before_hash: None,
        after_hash: Some("hash".to_owned()),
        source: source.to_owned(),
        request_id: "request".to_owned(),
        timestamp: Utc::now(),
        generation,
    }
}

fn test_actor_with_budget(root: &Path, foreground_budget_ms: u64) -> Arc<Workspace> {
    let mut policy = test_policy();
    policy.bash.foreground_budget_ms = foreground_budget_ms;
    test_actor_with_policy(root, policy)
}

fn test_actor_with_exclusions(root: &Path, exclude_paths: Vec<String>) -> Arc<Workspace> {
    test_actor_with_policy_and_exclusions(root, test_policy(), exclude_paths)
}

fn run_git(root: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(root)
        .args(args)
        .status()
        .unwrap();
    assert!(status.success(), "git command failed: {args:?}");
}
