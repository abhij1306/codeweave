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
use crate::requests::bash::{
    BashCancelRequest, BashOutputRequest, BashRequest, BashStartRequest, BashStatusRequest,
};
use crate::requests::edit::{CodePreviewRequest, CodeTransactionRequest, EditRequest};
use crate::requests::git::{GitAction, GitPushRequest};
use crate::requests::retrieval::CodeRetrieveRequest;
use crate::test_bash_executable;
use chrono::{Duration as ChronoDuration, Utc};
use parking_lot::Mutex;
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use tempfile::tempdir;

// The tests write requests as the JSON a client sends; these helpers parse
// that JSON into the typed requests the workspace serves.

fn typed<T: DeserializeOwned>(value: Value) -> T {
    serde_json::from_value(value).expect("test request must parse")
}

/// `{"changes", "preview"?, ...}` as an edit request.
fn edit_request(params: &Value) -> EditRequest {
    let mut params = params.as_object().expect("edit params").clone();
    let preview = params.remove("preview").and_then(|value| value.as_bool());
    if preview == Some(true) {
        typed::<CodePreviewRequest>(Value::Object(params)).into_edit()
    } else {
        typed::<CodeTransactionRequest>(Value::Object(params)).into_edit()
    }
    .expect("test edit must be valid")
}

/// `{"action": "<git action>", ...}` as a Git action.
fn git_action(params: &Value) -> GitAction {
    let mut params = params.as_object().expect("git params").clone();
    let action = params.remove("action").expect("git action");
    let params = Value::Object(params);
    match action.as_str().expect("git action name") {
        "status" => GitAction::Status,
        "preflight" => GitAction::Preflight,
        "diff" => GitAction::Diff(typed(params)),
        "log" => GitAction::Log(typed(params)),
        "show" => GitAction::Show(typed(params)),
        "blame" => GitAction::Blame(typed(params)),
        "stage" => GitAction::Stage(typed(params)),
        "commit" => GitAction::Commit(typed(params)),
        "restore" => GitAction::Restore(typed(params)),
        "push" => GitAction::Push(typed(params)),
        other => panic!("unknown git action {other}"),
    }
}

/// Push arguments, confirmed.
fn push_request(params: &Value) -> GitPushRequest {
    let mut params = params.as_object().expect("push params").clone();
    params.insert("confirm".to_owned(), Value::Bool(true));
    typed(Value::Object(params))
}

/// `{"action"?: "start" | "status" | "output" | "cancel", ...}` as a Bash
/// request.
fn bash_request(params: &Value) -> BashRequest {
    let mut params = params.as_object().expect("bash params").clone();
    let action = params.remove("action");
    let params = Value::Object(params);
    match action.as_ref().and_then(Value::as_str).unwrap_or("start") {
        "start" => typed::<BashStartRequest>(params).into_request(),
        "status" => typed::<BashStatusRequest>(params).into_request(),
        "output" => typed::<BashOutputRequest>(params).into_request(),
        "cancel" => typed::<BashCancelRequest>(params).into_request(),
        other => panic!("unknown bash action {other}"),
    }
    .expect("test bash request must be valid")
}

fn retrieve_request(params: &Value) -> CodeRetrieveRequest {
    typed(params.clone())
}

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
