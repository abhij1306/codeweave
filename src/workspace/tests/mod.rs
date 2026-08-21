mod edit;
mod fetch_search;
mod git;
mod run_reconcile;
mod state;
mod validation;

use super::edit::PlannedFile;
use super::events::MutationRecord;
use super::util::{
    line_ending_label, line_range_bytes, normalize_line_endings_for_content,
    summarize_changed_paths, MAX_CHANGED_PATH_GROUPS, MAX_OBSERVED_CHANGED_PATHS,
};
use super::{validated_push_target, Workspace};
use crate::index::content_hash;
use crate::model::{BashConfig, PolicyConfig, WorkspaceConfig};
use crate::test_bash_executable;
use chrono::{Duration as ChronoDuration, Utc};
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
