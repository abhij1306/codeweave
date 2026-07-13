use super::edit::PlannedFile;
use super::journal::{rotate_journal_if_needed, MutationRecord, MAX_JOURNAL_BYTES};
use super::util::{
    line_ending_label, line_range_bytes, normalize_line_endings_for_content,
    summarize_changed_paths, MAX_CHANGED_PATH_GROUPS, MAX_OBSERVED_CHANGED_PATHS,
};
use super::{validated_push_target, RunBaseline, WorkspaceActor};
use crate::index::content_hash;
use crate::model::{BashConfig, PolicyConfig, WorkspaceConfig};
use crate::test_bash_executable;
use chrono::Utc;
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
            enabled: true,
            executable: test_bash_executable(),
            default_timeout_ms: 120_000,
            foreground_budget_ms: 20_000,
            max_timeout_ms: 300_000,
            max_output_chars: 30_000,
            retention_hours: 1,
        },
    }
}

fn test_actor(root: &Path) -> Arc<WorkspaceActor> {
    test_actor_with_exclusions(root, Vec::new())
}

fn test_actor_with_policy(root: &Path, policy: PolicyConfig) -> Arc<WorkspaceActor> {
    test_actor_with_policy_and_exclusions(root, policy, Vec::new())
}

fn test_actor_with_policy_and_exclusions(
    root: &Path,
    policy: PolicyConfig,
    exclude_paths: Vec<String>,
) -> Arc<WorkspaceActor> {
    let cache = tempdir().unwrap().keep();
    Arc::new(
        WorkspaceActor::open(
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

fn test_actor_with_budget(root: &Path, foreground_budget_ms: u64) -> Arc<WorkspaceActor> {
    let mut policy = test_policy();
    policy.bash.foreground_budget_ms = foreground_budget_ms;
    test_actor_with_policy(root, policy)
}

fn test_actor_with_exclusions(root: &Path, exclude_paths: Vec<String>) -> Arc<WorkspaceActor> {
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

#[test]
fn git_diff_continuation_preserves_the_original_scope() {
    let root = tempdir().unwrap();
    run_git(root.path(), &["init", "-q"]);
    run_git(
        root.path(),
        &["config", "user.email", "codeweave@example.test"],
    );
    run_git(root.path(), &["config", "user.name", "CodeWeave Test"]);

    let base = (0..30)
        .map(|index| format!("line-{index:02}-{}", "x".repeat(80)))
        .collect::<Vec<_>>();
    fs::write(root.path().join("a.rs"), format!("{}\n", base.join("\n"))).unwrap();
    fs::write(root.path().join("b.rs"), format!("{}\n", base.join("\n"))).unwrap();
    run_git(root.path(), &["add", "a.rs", "b.rs"]);
    run_git(root.path(), &["commit", "-q", "-m", "baseline"]);

    let mut changed_a = base.clone();
    changed_a[1] = format!("changed-near-start-{}", "y".repeat(80));
    changed_a[25] = format!("changed-near-end-{}", "z".repeat(80));
    fs::write(
        root.path().join("a.rs"),
        format!("{}\n", changed_a.join("\n")),
    )
    .unwrap();
    let mut changed_b = base.clone();
    changed_b[1] = format!("unrelated-change-{}", "q".repeat(80));
    fs::write(
        root.path().join("b.rs"),
        format!("{}\n", changed_b.join("\n")),
    )
    .unwrap();

    let actor = test_actor(root.path());
    let first = actor
        .git(&json!({
            "action": "diff",
            "paths": ["a.rs"],
            "max_chars": 1_200
        }))
        .unwrap();
    assert_eq!(first["truncated"], true);
    assert!(!first["hunks"].as_array().unwrap().is_empty());
    assert!(first["hunks"]
        .as_array()
        .unwrap()
        .iter()
        .all(|hunk| hunk["path"] == "a.rs"));
    let continuation = first["continuation"].as_str().unwrap();

    let second = actor
        .git(&json!({
            "action": "diff",
            "continuation": continuation,
            "max_chars": 5_000
        }))
        .unwrap();
    assert!(second["hunks"]
        .as_array()
        .unwrap()
        .iter()
        .all(|hunk| hunk["path"] == "a.rs"));
    assert!(!second["output"].as_str().unwrap().contains("b.rs"));
    assert_eq!(second["scope"]["paths"], json!(["a.rs"]));

    let error = actor
        .git(&json!({
            "action": "diff",
            "continuation": continuation,
            "paths": ["b.rs"]
        }))
        .unwrap_err();
    assert_eq!(error.0.code, "CONTINUATION_SCOPE_MISMATCH");
}

#[test]
fn push_target_defaults_to_current_branch_and_rejects_git_syntax() {
    assert_eq!(
        validated_push_target(&json!({}), "feature/current").unwrap(),
        ("origin".to_owned(), "feature/current".to_owned())
    );
    assert_eq!(
        validated_push_target(
            &json!({"remote": "upstream", "branch": "feature/explicit"}),
            "feature/current"
        )
        .unwrap(),
        ("upstream".to_owned(), "feature/explicit".to_owned())
    );

    for params in [
        json!({"remote": "--mirror"}),
        json!({"remote": "https://example.com/repository.git"}),
        json!({"branch": ":main"}),
        json!({"branch": "+main"}),
        json!({"branch": "main~1"}),
    ] {
        assert!(validated_push_target(&params, "main").is_err());
    }
    assert!(validated_push_target(&json!({}), "").is_err());
}

#[test]
fn fetch_batches_return_successes_and_item_errors() {
    let root = tempdir().unwrap();
    fs::write(root.path().join("valid.rs"), "fn valid() {}\n").unwrap();
    let actor = test_actor(root.path());
    let result = actor
        .read_targets(&json!({
            "items": [
                {"kind": "path", "value": "valid.rs"},
                {"kind": "path", "value": "missing.rs"}
            ]
        }))
        .unwrap();
    assert_eq!(result["results"].as_array().unwrap().len(), 1);
    assert_eq!(result["errors"].as_array().unwrap().len(), 1);
    assert_eq!(result["partial_success"], true);
}

#[test]
fn fetch_resolves_qualified_python_method_names() {
    let root = tempdir().unwrap();
    fs::write(
        root.path().join("runner.py"),
        "class BrowserAttemptRunner:\n    def run(self):\n        return 'browser'\n\nclass OtherRunner:\n    def run(self):\n        return 'other'\n",
    )
    .unwrap();
    let actor = test_actor(root.path());

    let result = actor
        .read_targets(&json!({
            "items": [{
                "kind": "symbol",
                "path": "runner.py",
                "value": "BrowserAttemptRunner.run"
            }]
        }))
        .unwrap();

    assert_eq!(result["result_count"], 1);
    assert!(result["results"][0]["content"]
        .as_str()
        .unwrap()
        .contains("return 'browser'"));
    assert!(!result["results"][0]["content"]
        .as_str()
        .unwrap()
        .contains("return 'other'"));
}

#[test]
fn fetch_disambiguates_path_and_rust_qualified_method() {
    let root = tempdir().unwrap();
    fs::create_dir_all(root.path().join("src")).unwrap();
    fs::write(
        root.path().join("src/runner.rs"),
        "struct BrowserAttemptRunner;\nimpl BrowserAttemptRunner {\n    fn run(&self) -> &'static str { \"browser\" }\n}\nstruct OtherRunner;\nimpl OtherRunner {\n    fn run(&self) -> &'static str { \"other\" }\n}\n",
    )
    .unwrap();
    let actor = test_actor(root.path());

    let result = actor
        .read_targets(&json!({
            "items": [{
                "kind": "symbol",
                "value": "src/runner.rs::BrowserAttemptRunner::run"
            }]
        }))
        .unwrap();

    assert_eq!(result["result_count"], 1);
    assert!(result["results"][0]["content"]
        .as_str()
        .unwrap()
        .contains("\"browser\""));
    assert!(!result["results"][0]["content"]
        .as_str()
        .unwrap()
        .contains("\"other\""));
}

#[test]
fn fetch_accepts_direct_path_parameters() {
    let root = tempdir().unwrap();
    fs::write(root.path().join("direct.rs"), "one\ntwo\nthree\n").unwrap();
    let actor = test_actor(root.path());

    let result = actor
        .read_targets(&json!({
            "path": "direct.rs",
            "start_line": 2,
            "end_line": 3,
            "max_chars": 5_000
        }))
        .unwrap();

    assert_eq!(result["result_count"], 1);
    assert_eq!(result["error_count"], 0);
    assert_eq!(result["results"][0]["path"], "direct.rs");
    assert_eq!(result["results"][0]["content"], "two\nthree");
    assert_eq!(result["truncated"], false);
}

#[tokio::test]
async fn fetched_windows_text_can_be_previewed_and_applied_exactly() {
    let root = tempdir().unwrap();
    let path = root.path().join("windows.txt");
    fs::write(&path, b"before\r\nold value\r\nafter\r\n").unwrap();
    let actor = test_actor(root.path());

    let fetched = actor
        .read_targets(&json!({
            "path": "windows.txt",
            "start_line": 1,
            "end_line": 2,
            "max_chars": 5_000
        }))
        .unwrap();
    let result = &fetched["results"][0];
    assert_eq!(result["content"], "before\nold value");
    assert_eq!(result["line_ending"], "crlf");
    let handle = result["handle"].as_str().unwrap();
    let changes = json!([{
        "kind": "replace",
        "path": "windows.txt",
        "handle": handle,
        "old_text": result["content"],
        "new_text": "before\nnew value"
    }]);

    let preview = actor
        .code_edit(
            "test-session",
            &json!({"preview": true, "changes": changes.clone()}),
        )
        .await
        .unwrap();
    assert_eq!(preview["preview"], true);
    assert_eq!(
        fs::read(&path).unwrap(),
        b"before\r\nold value\r\nafter\r\n"
    );

    let applied = actor
        .code_edit("test-session", &json!({"changes": changes}))
        .await
        .unwrap();
    assert_eq!(applied["applied"], true);
    assert_eq!(
        fs::read(&path).unwrap(),
        b"before\r\nnew value\r\nafter\r\n"
    );
}

#[tokio::test]
async fn exact_replace_prefers_normalized_crlf_match_over_raw_lf_suffix() {
    let root = tempdir().unwrap();
    let path = root.path().join("windows.txt");
    fs::write(&path, b"before\r\nold value\r\nafter\r\n").unwrap();
    let actor = test_actor(root.path());
    let fetched = actor
        .read_targets(&json!({
            "path": "windows.txt",
            "start_line": 1,
            "end_line": 2,
            "max_chars": 5_000
        }))
        .unwrap();
    let handle = fetched["results"][0]["handle"].as_str().unwrap();

    actor
        .code_edit(
            "test-session",
            &json!({
                "changes": [{
                    "kind": "replace",
                    "path": "windows.txt",
                    "handle": handle,
                    "old_text": "\nold value",
                    "new_text": "\nnew value"
                }]
            }),
        )
        .await
        .unwrap();

    assert_eq!(
        fs::read(&path).unwrap(),
        b"before\r\nnew value\r\nafter\r\n"
    );
}

#[tokio::test]
async fn handle_range_replace_preserves_windows_line_endings() {
    let root = tempdir().unwrap();
    let path = root.path().join("windows.txt");
    fs::write(&path, b"first\r\nsecond\r\nthird\r\n").unwrap();
    let actor = test_actor(root.path());

    let fetched = actor
        .read_targets(&json!({
            "path": "windows.txt",
            "start_line": 2,
            "end_line": 2,
            "max_chars": 5_000
        }))
        .unwrap();
    let result = &fetched["results"][0];
    assert_eq!(result["content"], "second");
    assert_eq!(result["line_ending"], "crlf");
    let handle = result["handle"].as_str().unwrap();
    let changes = json!([{
        "kind": "replace_range",
        "path": "windows.txt",
        "handle": handle,
        "new_text": "updated\ncontinued\n"
    }]);

    let preview = actor
        .code_edit(
            "test-session",
            &json!({"preview": true, "changes": changes.clone()}),
        )
        .await
        .unwrap();
    assert_eq!(preview["preview"], true);
    assert_eq!(fs::read(&path).unwrap(), b"first\r\nsecond\r\nthird\r\n");

    actor
        .code_edit("test-session", &json!({"changes": changes}))
        .await
        .unwrap();
    assert_eq!(
        fs::read(&path).unwrap(),
        b"first\r\nupdated\r\ncontinued\r\nthird\r\n"
    );
}

#[tokio::test]
async fn replace_range_preserves_boundary_when_new_text_omits_newline() {
    let root = tempdir().unwrap();
    let path = root.path().join("value.txt");
    fs::write(&path, "first\nsecond\nthird\n").unwrap();
    let actor = test_actor(root.path());
    let fetched = actor
        .read_targets(&json!({
            "path": "value.txt",
            "start_line": 2,
            "end_line": 2
        }))
        .unwrap();
    let handle = fetched["results"][0]["handle"].as_str().unwrap();

    actor
        .code_edit(
            "test-session",
            &json!({
                "changes": [{
                    "kind": "replace_range",
                    "path": "value.txt",
                    "handle": handle,
                    "new_text": "updated"
                }]
            }),
        )
        .await
        .unwrap();

    assert_eq!(fs::read_to_string(path).unwrap(), "first\nupdated\nthird\n");
}

#[tokio::test]
async fn exact_full_line_replace_preserves_boundary() {
    let root = tempdir().unwrap();
    let path = root.path().join("value.txt");
    let original = "first\nsecond\nthird\n";
    fs::write(&path, original).unwrap();
    let actor = test_actor(root.path());

    actor
        .code_edit(
            "test-session",
            &json!({
                "changes": [{
                    "kind": "replace",
                    "path": "value.txt",
                    "old_text": "first\nsecond\n",
                    "new_text": "updated",
                    "expected_hash": content_hash(original)
                }]
            }),
        )
        .await
        .unwrap();

    assert_eq!(fs::read_to_string(path).unwrap(), "updated\nthird\n");
}

#[test]
fn outline_accepts_multiple_paths_and_reports_partial_errors() {
    let root = tempdir().unwrap();
    fs::write(root.path().join("one.rs"), "pub fn one() {}\n").unwrap();
    fs::write(root.path().join("two.rs"), "pub fn two() {}\n").unwrap();
    let actor = test_actor(root.path());

    let single = actor
        .search_index(&json!({"mode": "outline", "paths": ["one.rs"]}))
        .unwrap();
    assert_eq!(single["path"], "one.rs");
    assert!(single["symbols"].is_array());

    let batch = actor
        .search_index(&json!({
            "mode": "outline",
            "paths": ["one.rs", "missing.rs", "two.rs"]
        }))
        .unwrap();
    assert_eq!(batch["result_count"], 2);
    assert_eq!(batch["error_count"], 1);
    assert_eq!(batch["partial_success"], true);
    assert_eq!(batch["results"][0]["path"], "one.rs");
    assert_eq!(batch["results"][1]["path"], "two.rs");
}

#[test]
fn fetch_supports_compact_metadata_and_symbol_import_context() {
    let root = tempdir().unwrap();
    fs::write(
        root.path().join("lib.rs"),
        "use std::fmt;\n\nfn helper() {}\n\npub fn render() {\n    helper();\n}\n",
    )
    .unwrap();
    let actor = test_actor(root.path());

    let metadata = actor
        .read_targets(&json!({"items": [{"kind": "metadata", "value": "lib.rs"}]}))
        .unwrap();
    assert_eq!(metadata["results"][0]["kind"], "metadata");
    assert_eq!(metadata["results"][0]["language"], "rust");
    assert_eq!(metadata["results"][0]["line_count"], 7);
    assert!(metadata["results"][0].get("content").is_none());

    let symbol = actor
        .read_targets(&json!({
            "items": [{
                "kind": "symbol",
                "value": "render",
                "context_lines": 1,
                "include_imports": true
            }]
        }))
        .unwrap();
    assert_eq!(symbol["results"][0]["start_line"], 4);
    assert_eq!(symbol["results"][0]["end_line"], 7);
    assert_eq!(symbol["results"][0]["imports"][0]["text"], "use std::fmt;");

    let compact = actor
        .read_targets(&json!({
            "path": "lib.rs",
            "response_detail": "compact"
        }))
        .unwrap();
    assert_eq!(compact["response_detail"], "compact");
    assert_eq!(compact["results"][0]["path"], "lib.rs");
    assert!(compact["results"][0].get("handle").is_none());
    assert!(compact["results"][0]["content"]
        .as_str()
        .unwrap()
        .contains("render"));
}

#[test]
fn workspace_diagnostics_exposes_bash_policy_and_limits() {
    let root = tempdir().unwrap();
    fs::write(root.path().join("main.rs"), "fn main() {}\n").unwrap();
    let actor = test_actor(root.path());

    let diagnostics = actor.diagnostics().unwrap();

    assert_eq!(diagnostics["workspace_id"], "main");
    assert_eq!(diagnostics["file_count"], 1);
    assert_eq!(diagnostics["policy"]["max_search_results"], 100);
    assert_eq!(diagnostics["policy"]["bash"]["enabled"], true);
}

#[test]
fn code_capabilities_reports_public_contracts() {
    let root = tempdir().unwrap();
    fs::write(root.path().join("main.rs"), "fn main() {}\n").unwrap();
    let actor = test_actor(root.path());

    let capabilities = actor.code_capabilities().unwrap();

    assert_eq!(capabilities["workspace_id"], "main");
    assert_eq!(capabilities["retrieval"]["tool"], "code_retrieve");
    assert!(capabilities["retrieval"]["operations"]
        .as_array()
        .unwrap()
        .iter()
        .any(|operation| operation == "read"));
    assert!(capabilities["retrieval"]["read_targets"]
        .as_array()
        .unwrap()
        .iter()
        .any(|target| target == "metadata"));
    assert_eq!(capabilities["editing"]["supports_transaction"], true);
    assert_eq!(
        capabilities["editing"]["supports_handle_range_replace"],
        true
    );
}

#[test]
fn code_retrieve_batches_explicit_primitives_in_one_round_trip() {
    let root = tempdir().unwrap();
    fs::write(
        root.path().join("engine.rs"),
        "pub fn extract() { panic!(\"runtime failed\"); }\n",
    )
    .unwrap();
    fs::write(
        root.path().join("Cargo.toml"),
        "[package]\nname = \"fixture\"\n",
    )
    .unwrap();
    let actor = test_actor(root.path());

    let result = actor
        .code_retrieve_for_session(
            "session",
            &json!({
                "operations": [
                    {"id": "file", "operation": "find_file", "name": "Cargo.toml"},
                    {"id": "symbol", "operation": "find_symbol", "symbol": "extract", "paths": ["engine.rs"]},
                    {"id": "pattern", "operation": "search_text", "pattern": "runtime failed", "paths": ["engine.rs"]},
                    {"id": "outline", "operation": "symbols_overview", "paths": ["engine.rs"]},
                    {"id": "read", "operation": "read", "target": "symbol", "value": "extract", "path": "engine.rs"}
                ]
            }),
        )
        .unwrap();

    assert_eq!(result["retrieval_contract_version"], 2);
    assert_eq!(result["result_count"], 5);
    assert_eq!(result["error_count"], 0);
    assert_eq!(result["execution"]["round_trips"], 1);
    assert_eq!(result["execution"]["parallel"], false);
    let ids = result["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(ids, ["file", "symbol", "pattern", "outline", "read"]);
    assert_eq!(
        result["results"][0]["result"]["results"][0]["path"],
        "Cargo.toml"
    );
    assert_eq!(
        result["results"][1]["result"]["results"][0]["path"],
        "engine.rs"
    );
    assert!(result["results"][2]["result"]["results"][0]["preview"]
        .as_str()
        .unwrap()
        .contains("runtime failed"));
    assert_eq!(result["results"][4]["result"]["path"], "engine.rs");
    assert!(result["results"][4]["result"]["content"]
        .as_str()
        .unwrap()
        .contains("pub fn extract"));
}

#[test]
fn code_retrieve_preserves_success_when_an_operation_fails() {
    let root = tempdir().unwrap();
    fs::write(root.path().join("engine.rs"), "pub fn extract() {}\n").unwrap();
    let actor = test_actor(root.path());

    let result = actor
        .code_retrieve_for_session(
            "session",
            &json!({
                "operations": [
                    {"id": "file", "operation": "find_file", "name": "engine.rs"},
                    {"id": "invalid", "operation": "find_symbol"}
                ]
            }),
        )
        .unwrap();

    assert_eq!(result["result_count"], 1);
    assert_eq!(result["error_count"], 1);
    assert_eq!(result["partial_success"], true);
    assert_eq!(result["results"][0]["id"], "file");
    assert_eq!(result["errors"][0]["id"], "invalid");
}

#[test]
fn code_retrieve_reports_malformed_entries_without_losing_successes() {
    let root = tempdir().unwrap();
    fs::write(root.path().join("engine.rs"), "pub fn extract() {}\n").unwrap();
    let actor = test_actor(root.path());

    let result = actor
        .code_retrieve_for_session(
            "session",
            &json!({
                "operations": [
                    {"id": "file", "operation": "find_file", "name": "engine.rs"},
                    "not-an-object",
                    {"id": "missing-operation"},
                    {"id": "file", "operation": "find_symbol", "symbol": "extract"}
                ]
            }),
        )
        .unwrap();

    assert_eq!(result["result_count"], 1);
    assert_eq!(result["error_count"], 3);
    assert_eq!(result["partial_success"], true);
    assert_eq!(result["results"][0]["id"], "file");
    assert_eq!(result["errors"][0]["id"], "op_2");
    assert_eq!(result["errors"][1]["id"], "missing-operation");
    assert_eq!(
        result["errors"][2]["error"]["code"],
        "DUPLICATE_RETRIEVAL_OPERATION_ID"
    );
}

#[test]
fn code_retrieve_rejects_a_stale_snapshot() {
    let root = tempdir().unwrap();
    fs::write(root.path().join("engine.rs"), "pub fn extract() {}\n").unwrap();
    let actor = test_actor(root.path());

    let error = actor
        .code_retrieve_for_session(
            "session",
            &json!({
                "snapshot_id": "snap_stale",
                "operations": [
                    {"operation": "find_file", "name": "engine.rs"}
                ]
            }),
        )
        .unwrap_err();

    assert_eq!(error.0.code, "STALE_SNAPSHOT");
}

#[tokio::test]
async fn bash_status_fetch_and_run_local_changed_paths_are_bounded() {
    let root = tempdir().unwrap();
    fs::write(root.path().join("main.rs"), "fn main() {}\n").unwrap();
    let actor = test_actor(root.path());
    actor.mutations.lock().push_back(MutationRecord {
        mutation_id: "historical".to_owned(),
        session_id: "external".to_owned(),
        path: "unrelated/generated.txt".to_owned(),
        before_hash: None,
        after_hash: Some("hash".to_owned()),
        source: "external".to_owned(),
        request_id: "test".to_owned(),
        timestamp: Utc::now(),
        generation: actor.generation(),
    });

    let started = actor
        .run(
            "session",
            &json!({
                "command": "printf codeweave-bash-test",
                "background": false
            }),
        )
        .await
        .unwrap();
    let run_id = started["run_id"].as_str().unwrap();
    assert_eq!(started["status_fetch"]["kind"], "bash_status");
    assert_eq!(started["status_fetch"]["value"], run_id);

    let fetched = actor
        .read_targets_for_session(
            "session",
            &json!({
                "items": [{"kind": "bash_status", "value": run_id}]
            }),
        )
        .unwrap();

    assert_eq!(fetched["result_count"], 1);
    assert_eq!(fetched["results"][0]["run_id"], run_id);
    assert_eq!(started["observed_changed_path_count"], 0);
    assert_eq!(fetched["results"][0]["status"], "succeeded");

    let bounded = actor
        .read_targets_for_session(
            "session",
            &json!({
                "items": [
                    {"kind": "bash_status", "value": run_id},
                    {"kind": "bash_status", "value": run_id}
                ],
                "max_chars": 5
            }),
        )
        .unwrap();
    assert!(bounded["results"][0]["output"].as_str().unwrap().len() <= 5);
    assert_eq!(bounded["results"][0]["output_truncated"], true);
    assert_eq!(bounded["result_count"], 1);
    assert_eq!(bounded["items_truncated"], true);
    assert_eq!(bounded["chars_truncated"], true);
    assert_eq!(bounded["truncated"], true);
}

#[tokio::test]
async fn completed_bash_status_does_not_attribute_later_workspace_writes() {
    let root = tempdir().unwrap();
    fs::write(root.path().join("main.rs"), "fn main() {}\n").unwrap();
    let actor = test_actor(root.path());

    let started = actor
        .run(
            "session",
            &json!({
                "command": "printf codeweave-bash-test",
                "background": true
            }),
        )
        .await
        .unwrap();
    let run_id = started["run_id"].as_str().unwrap();

    let mut raw_status = actor.bash.status(run_id).unwrap();
    for _ in 0..100 {
        if raw_status["ended_at"].is_string() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        raw_status = actor.bash.status(run_id).unwrap();
    }
    assert!(raw_status["ended_at"].is_string());

    actor
        .commit_plan(
            "session",
            &[PlannedFile {
                path: ".ai-bridge/codeweave-audit-success.md".to_owned(),
                before: None,
                after: Some("created after bash exit\n".to_owned()),
            }],
            "later-write",
        )
        .unwrap();

    let fetched = actor
        .run(
            "session",
            &json!({
                "action": "status",
                "run_id": run_id
            }),
        )
        .await
        .unwrap();

    assert_eq!(fetched["status"], "succeeded");
    assert_eq!(fetched["observed_changed_path_count"], 0);
    assert_eq!(fetched["observed_changed_paths"], json!([]));

    let fetched_again = actor
        .run(
            "session",
            &json!({
                "action": "status",
                "run_id": run_id
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        fetched_again["observed_changed_paths"],
        fetched["observed_changed_paths"]
    );
}

#[test]
fn run_change_detection_includes_new_mutations_to_already_dirty_files() {
    let root = tempdir().unwrap();
    fs::write(root.path().join("existing.rs"), "fn existing() {}\n").unwrap();
    let actor = test_actor(root.path());
    let generation = actor.generation();
    let dirty_files = HashSet::from(["existing.rs".to_owned()]);
    let baseline = RunBaseline::new(generation, dirty_files.clone());
    actor.mutations.lock().push_back(MutationRecord {
        mutation_id: "during-run".to_owned(),
        session_id: "external".to_owned(),
        path: "existing.rs".to_owned(),
        before_hash: Some("before".to_owned()),
        after_hash: Some("after".to_owned()),
        source: "external".to_owned(),
        request_id: "watcher".to_owned(),
        timestamp: Utc::now(),
        generation: generation + 1,
    });

    let observed = actor.observed_run_changed_paths(&baseline, generation + 1, None, &dirty_files);

    assert_eq!(observed, HashSet::from(["existing.rs".to_owned()]));
}

#[test]
fn search_accepts_multiple_queries() {
    let root = tempdir().unwrap();
    fs::write(root.path().join("alpha.rs"), "fn alpha() {}\n").unwrap();
    fs::write(root.path().join("beta.rs"), "fn beta() {}\n").unwrap();
    let actor = test_actor(root.path());
    let result = actor
        .search_index(&json!({
            "mode": "literal",
            "queries": ["alpha", "beta"]
        }))
        .unwrap();
    assert_eq!(result["query_count"], 2);
    assert_eq!(result["result_count"], 2);
    assert_eq!(result["error_count"], 0);
}

#[test]
fn read_tools_report_pending_reconciliation_without_blocking() {
    let root = tempdir().unwrap();
    fs::write(root.path().join("existing.rs"), "fn existing_symbol() {}\n").unwrap();
    let actor = test_actor(root.path());
    fs::write(root.path().join("pending.rs"), "fn pending_symbol() {}\n").unwrap();
    actor
        .pending_paths
        .lock()
        .insert(root.path().join("pending.rs"));
    actor
        .needs_reconcile
        .store(true, std::sync::atomic::Ordering::Release);

    let fetch = actor
        .read_targets(&json!({"path": "existing.rs", "max_chars": 5_000}))
        .unwrap();
    assert_eq!(fetch["reconcile_pending"], true);
    assert!(fetch["phase_ms"]["fetch_items"].is_number());

    let search = actor
        .search_index(&json!({"mode": "literal", "query": "pending_symbol"}))
        .unwrap();
    assert_eq!(search["reconcile_pending"], true);
    assert_eq!(search["result_count"], 0);
    assert!(search["phase_ms"]["index_search"].is_number());

    assert!(actor
        .needs_reconcile
        .load(std::sync::atomic::Ordering::Acquire));
}

#[test]
fn reconciliation_discards_configured_excluded_paths() {
    let root = tempdir().unwrap();
    fs::create_dir_all(root.path().join("backend/artifacts")).unwrap();
    fs::write(root.path().join("source.rs"), "fn source() {}\n").unwrap();
    fs::write(
        root.path().join("backend/artifacts/existing.json"),
        "existing",
    )
    .unwrap();
    let actor = test_actor_with_exclusions(
        root.path(),
        vec!["backend/artifacts/".to_owned(), "*.log".to_owned()],
    );
    assert!(actor.index.read().get("source.rs").is_some());
    assert!(actor
        .index
        .read()
        .get("backend/artifacts/existing.json")
        .is_none());
    let generation = actor.generation();
    let generated = root.path().join("backend/artifacts/new.json");
    fs::write(&generated, "generated").unwrap();
    actor.pending_paths.lock().insert(generated);
    actor
        .needs_reconcile
        .store(true, std::sync::atomic::Ordering::Release);

    let summary = actor.summary("test-session", false).unwrap();

    assert_eq!(actor.generation(), generation);
    assert_eq!(summary["dirty_ownership"]["counts"]["observed_external"], 0);
    assert!(actor
        .index
        .read()
        .get("backend/artifacts/new.json")
        .is_none());
    assert!(!actor
        .needs_reconcile
        .load(std::sync::atomic::Ordering::Acquire));
}

#[test]
fn fetch_rejects_a_stale_snapshot() {
    let root = tempdir().unwrap();
    fs::write(root.path().join("valid.rs"), "fn valid() {}\n").unwrap();
    let actor = test_actor(root.path());
    let error = actor
        .read_targets(&json!({
            "snapshot_id": "snap_stale",
            "items": [{"kind": "path", "value": "valid.rs"}]
        }))
        .unwrap_err();
    assert_eq!(error.0.code, "STALE_SNAPSHOT");
}

#[test]
fn fetch_reports_character_truncation_separately() {
    let root = tempdir().unwrap();
    fs::write(root.path().join("large.txt"), "abcdefghijklmnopqrstuvwxyz").unwrap();
    let actor = test_actor(root.path());
    let result = actor
        .read_targets(&json!({
            "items": [{"kind": "path", "value": "large.txt"}],
            "max_chars": 5
        }))
        .unwrap();
    assert_eq!(result["truncated"], true);
    assert_eq!(result["items_truncated"], false);
    assert_eq!(result["chars_truncated"], true);
}

#[test]
fn open_ended_line_fetch_reports_clamped_end_line() {
    let root = tempdir().unwrap();
    fs::write(root.path().join("short.txt"), "one\ntwo\n").unwrap();
    let actor = test_actor(root.path());

    let result = actor
        .read_targets(&json!({
            "path": "short.txt",
            "start_line": 2,
            "max_chars": 5_000
        }))
        .unwrap();

    assert_eq!(result["results"][0]["start_line"], 2);
    assert_eq!(result["results"][0]["end_line"], 2);
    assert_eq!(result["results"][0]["content"], "two");
}

#[test]
fn out_of_bounds_line_fetch_clamps_start_before_end() {
    let root = tempdir().unwrap();
    fs::write(root.path().join("short.txt"), "one\ntwo\n").unwrap();
    let actor = test_actor(root.path());

    let result = actor
        .read_targets(&json!({
            "path": "short.txt",
            "start_line": 999,
            "max_chars": 5_000
        }))
        .unwrap();

    assert_eq!(result["results"][0]["start_line"], 2);
    assert_eq!(result["results"][0]["end_line"], 2);
    assert_eq!(result["results"][0]["content"], "two");
}

#[test]
fn ranged_fetch_continuation_stays_within_the_original_range() {
    let root = tempdir().unwrap();
    fs::write(
        root.path().join("range.txt"),
        "outside-before\nalpha\nbeta\ngamma\noutside-after\n",
    )
    .unwrap();
    let actor = test_actor(root.path());

    let first = actor
        .read_targets(&json!({
            "path": "range.txt",
            "start_line": 2,
            "end_line": 4,
            "max_chars": 7
        }))
        .unwrap();
    assert_eq!(first["results"][0]["content"], "alpha\nb");
    let continuation = first["results"][0]["continuation"].as_str().unwrap();

    let second = actor
        .read_targets(&json!({
            "items": [{"kind": "continuation", "value": continuation}],
            "max_chars": 100
        }))
        .unwrap();

    assert_eq!(second["results"][0]["content"], "eta\ngamma");
    assert!(!second["results"][0]["content"]
        .as_str()
        .unwrap()
        .contains("outside"));
    assert!(second["results"][0]["continuation"].is_null());
}

#[test]
fn handle_fetch_continuation_preserves_the_handle_range() {
    let root = tempdir().unwrap();
    fs::write(
        root.path().join("handle.txt"),
        "outside-before\nalpha\nbeta\ngamma\noutside-after\n",
    )
    .unwrap();
    let actor = test_actor(root.path());
    let direct = actor
        .read_targets(&json!({
            "path": "handle.txt",
            "start_line": 2,
            "end_line": 4,
            "max_chars": 100
        }))
        .unwrap();
    let handle = direct["results"][0]["handle"].as_str().unwrap();

    let first = actor
        .read_targets(&json!({
            "items": [{"kind": "handle", "value": handle}],
            "max_chars": 7
        }))
        .unwrap();
    let continuation = first["results"][0]["continuation"].as_str().unwrap();
    let second = actor
        .read_targets(&json!({
            "items": [{"kind": "continuation", "value": continuation}],
            "max_chars": 100
        }))
        .unwrap();

    assert_eq!(second["results"][0]["content"], "eta\ngamma");
    assert!(!second["results"][0]["content"]
        .as_str()
        .unwrap()
        .contains("outside"));
    assert!(second["results"][0]["continuation"].is_null());
}

#[test]
fn rollback_recheck_rejects_concurrent_changes() {
    let root = tempdir().unwrap();
    fs::write(root.path().join("value.rs"), "after\n").unwrap();
    let actor = test_actor(root.path());
    let plan = vec![PlannedFile {
        path: "value.rs".to_owned(),
        before: Some("before\n".to_owned()),
        after: Some("after\n".to_owned()),
    }];
    fs::write(root.path().join("value.rs"), "concurrent\n").unwrap();
    let error = actor.recheck_applied_state(&plan).unwrap_err();
    assert_eq!(error.0.code, "ROLLBACK_CONFLICT");
}

#[test]
fn failed_write_does_not_leave_an_internal_write_marker() {
    let root = tempdir().unwrap();
    fs::create_dir(root.path().join("blocked")).unwrap();
    let actor = test_actor(root.path());
    let plan = vec![PlannedFile {
        path: "blocked".to_owned(),
        before: None,
        after: None,
    }];

    let error = actor
        .commit_plan("test-session", &plan, "failed-write")
        .unwrap_err();
    assert_eq!(error.0.code, "ATOMIC_WRITE_FAILED");
    assert!(!actor
        .internal_writes
        .lock()
        .contains_key(&actor.root.join("blocked")));
}

#[test]
fn journal_failure_rolls_back_applied_files_before_returning_error() {
    let root = tempdir().unwrap();
    let original = "before\n";
    fs::write(root.path().join("value.txt"), original).unwrap();
    let actor = test_actor(root.path());
    let generation = actor.generation();
    *actor.journal_file.lock() = None;
    let plan = vec![PlannedFile {
        path: "value.txt".to_owned(),
        before: Some(original.to_owned()),
        after: Some("after\n".to_owned()),
    }];

    let error = actor
        .commit_plan("test-session", &plan, "journal-failure")
        .unwrap_err();

    assert_eq!(error.0.code, "JOURNAL_COMMIT_FAILED");
    assert_eq!(
        error
            .0
            .details
            .as_ref()
            .unwrap()
            .get("rollback_refresh_error"),
        Some(&serde_json::Value::Null)
    );
    assert_eq!(
        fs::read_to_string(root.path().join("value.txt")).unwrap(),
        original
    );
    let fetched = actor
        .read_targets(&json!({"path": "value.txt", "max_chars": 5_000}))
        .unwrap();
    assert_eq!(fetched["results"][0]["content"], original);
    assert_eq!(actor.generation(), generation);
}

#[test]
fn journal_rotates_during_append_when_the_live_file_is_oversized() {
    let root = tempdir().unwrap();
    fs::write(root.path().join("value.txt"), "value\n").unwrap();
    let actor = test_actor(root.path());
    {
        let mut slot = actor.journal_file.lock();
        let file = slot.as_mut().unwrap();
        file.set_len(MAX_JOURNAL_BYTES + 1).unwrap();
        std::io::Write::flush(file).unwrap();
    }
    let record = MutationRecord {
        mutation_id: "rotation-record".to_owned(),
        session_id: "test-session".to_owned(),
        path: "value.txt".to_owned(),
        before_hash: None,
        after_hash: Some("hash".to_owned()),
        source: "test".to_owned(),
        request_id: "rotation".to_owned(),
        timestamp: Utc::now(),
        generation: actor.generation(),
    };

    actor.record_mutations(&[record]).unwrap();

    let archive = actor
        .journal_path
        .with_file_name("mutations.previous.jsonl");
    assert!(archive.exists());
    let live = fs::read_to_string(&actor.journal_path).unwrap();
    assert!(live.contains("rotation-record"));
}

#[tokio::test]
async fn stale_snapshot_rebases_when_file_hash_is_current() {
    let root = tempdir().unwrap();
    let original = "fn value() -> i32 { 1 }\n";
    fs::write(root.path().join("value.rs"), original).unwrap();
    let actor = test_actor(root.path());
    let old_snapshot = actor.snapshot();
    fs::write(root.path().join("unrelated.rs"), "fn unrelated() {}\n").unwrap();
    actor.refresh(true, "test-session", false).unwrap();
    let result = actor
        .code_edit(
            "test-session",
            &json!({
                "snapshot_id": old_snapshot,
                "preview": true,
                "changes": [{
                    "kind": "replace",
                    "path": "value.rs",
                    "old_text": "{ 1 }",
                    "new_text": "{ 2 }",
                    "expected_hash": content_hash(original)
                }]
            }),
        )
        .await
        .unwrap();
    assert_eq!(result["preview"], true);
    assert!(result["snapshot_rebased_from"].is_string());
}

#[test]
fn summary_caps_large_instruction_files() {
    let root = tempdir().unwrap();
    let big = format!("{}étail", "a".repeat(4_095));
    assert!(big.len() > 4_096);
    fs::write(root.path().join("AGENTS.md"), &big).unwrap();
    fs::write(root.path().join("CLAUDE.md"), "short and sweet\n").unwrap();
    let actor = test_actor(root.path());
    let summary = actor.summary("test-session", false).unwrap();
    let instructions = summary["instructions"].as_array().unwrap();

    let agents = instructions
        .iter()
        .find(|entry| entry["path"] == "AGENTS.md")
        .unwrap();
    assert_eq!(agents["content_truncated"], true);
    assert_eq!(agents["content_bytes"], big.len());
    assert!(agents["content"].as_str().unwrap().len() <= 4_096);
    assert_eq!(agents["content"].as_str().unwrap().len(), 4_095);

    let claude = instructions
        .iter()
        .find(|entry| entry["path"] == "CLAUDE.md")
        .unwrap();
    assert!(claude.get("content_truncated").is_none());
    assert_eq!(claude["content"], "short and sweet\n");
}

#[tokio::test]
async fn response_detail_shapes_edit_diff_payload() {
    let root = tempdir().unwrap();
    let original = "fn value() -> i32 { 1 }\n";
    fs::write(root.path().join("value.rs"), original).unwrap();
    let actor = test_actor(root.path());
    let change = json!([{
        "kind": "replace",
        "path": "value.rs",
        "old_text": "{ 1 }",
        "new_text": "{ 2 }",
        "expected_hash": content_hash(original)
    }]);

    // compact: no unified diff, but the per-file stat is still present.
    let compact = actor
        .code_edit(
            "test-session",
            &json!({"changes": change, "response_detail": "compact"}),
        )
        .await
        .unwrap();
    assert_eq!(compact["applied"], true);
    assert!(compact["diff"].is_null());
    assert_eq!(compact["diff_omitted"], true);
    assert_eq!(compact["diff_stat"][0]["path"], "value.rs");
    assert_eq!(compact["diff_stat"][0]["added"], 1);
    assert_eq!(compact["diff_stat"][0]["removed"], 1);

    // debug: full unified diff is returned verbatim.
    fs::write(root.path().join("value.rs"), original).unwrap();
    actor.refresh(true, "test-session", false).unwrap();
    let debug = actor
        .code_edit(
            "test-session",
            &json!({"changes": change, "response_detail": "debug"}),
        )
        .await
        .unwrap();
    assert_eq!(debug["diff_omitted"], false);
    assert_eq!(debug["diff_truncated"], false);
    assert!(debug["diff"].as_str().unwrap().contains("{ 2 }"));
}

#[tokio::test]
async fn standard_response_detail_caps_oversized_edit_diff() {
    let root = tempdir().unwrap();
    // A file large enough that its unified diff exceeds max_context_chars.
    let original: String = (0..4_000).map(|i| format!("line {i}\n")).collect();
    fs::write(root.path().join("big.txt"), &original).unwrap();
    let mut policy = test_policy();
    policy.max_context_chars = 2_000;
    let actor = test_actor_with_policy(root.path(), policy);
    let replaced: String = (0..4_000).map(|i| format!("edited {i}\n")).collect();
    let result = actor
        .code_edit(
            "test-session",
            &json!({
                "changes": [{
                    "kind": "create",
                    "path": "big.txt",
                    "content": replaced,
                    "overwrite": true,
                    "expected_hash": content_hash(&original)
                }]
            }),
        )
        .await
        .unwrap();
    assert_eq!(result["applied"], true);
    assert_eq!(result["diff_truncated"], true);
    assert_eq!(result["diff_omitted"], false);
    let diff = result["diff"].as_str().unwrap();
    assert!(diff.len() <= 2_000);
    assert!(diff.ends_with('\n'));
}

#[tokio::test]
async fn failed_bash_validation_rolls_back_mutation() {
    let root = tempdir().unwrap();
    let original = "fn value() -> i32 { 1 }\n";
    fs::write(root.path().join("value.rs"), original).unwrap();
    let actor = test_actor(root.path());
    let summary = actor.summary("test-session", false).unwrap();
    assert_eq!(summary["capabilities"]["bash_available"], true);
    assert!(summary["warnings"].as_array().is_some_and(Vec::is_empty));
    let result = actor
        .code_edit(
            "test-session",
            &json!({
                "changes": [{
                    "kind": "replace",
                    "path": "value.rs",
                    "old_text": "{ 1 }",
                    "new_text": "{ 2 }",
                    "expected_hash": content_hash(original)
                }],
                "validate": [
                    "printf validation-started",
                    "printf validation-failed >&2; exit 1"
                ]
            }),
        )
        .await
        .unwrap();
    assert_eq!(result["applied"], false);
    assert_eq!(result["rolled_back"], true);
    assert_eq!(result["reason"], "validation_failed");
    assert_eq!(result["validation"].as_array().unwrap().len(), 2);
    assert_eq!(
        result["validation"][1]["command"],
        "printf validation-failed >&2; exit 1"
    );
    assert_eq!(
        fs::read_to_string(root.path().join("value.rs")).unwrap(),
        original
    );
}

#[tokio::test]
async fn unavailable_bash_validation_rejects_before_mutation() {
    let root = tempdir().unwrap();
    let path = root.path().join("value.rs");
    let original = "fn value() -> i32 { 1 }\n";
    fs::write(&path, original).unwrap();
    let mut policy = test_policy();
    policy.bash.executable = root
        .path()
        .join("missing-bash.exe")
        .to_string_lossy()
        .into_owned();
    let actor = test_actor_with_policy(root.path(), policy);

    let summary = actor.summary("test-session", false).unwrap();
    assert_eq!(summary["capabilities"]["bash_available"], false);
    assert_eq!(summary["capabilities"]["bash"]["readiness"], "unavailable");

    let error = actor
        .code_edit(
            "test-session",
            &json!({
                "changes": [{
                    "kind": "replace",
                    "path": "value.rs",
                    "old_text": "{ 1 }",
                    "new_text": "{ 2 }",
                    "expected_hash": content_hash(original)
                }],
                "validate": ["true"]
            }),
        )
        .await
        .unwrap_err();

    assert_eq!(error.0.code, "BASH_UNAVAILABLE");
    assert_eq!(fs::read_to_string(path).unwrap(), original);
}

#[test]
fn dirty_ownership_tracks_only_current_dirty_mcp_paths() {
    let root = tempdir().unwrap();
    fs::write(root.path().join("still_dirty.rs"), "fn dirty() {}\n").unwrap();
    fs::write(root.path().join("clean_now.rs"), "fn clean() {}\n").unwrap();
    let actor = test_actor(root.path());
    let generation = actor.generation();
    actor.mutations.lock().extend([
        MutationRecord {
            mutation_id: "dirty".to_owned(),
            session_id: "test-session".to_owned(),
            path: "still_dirty.rs".to_owned(),
            before_hash: None,
            after_hash: Some("dirty".to_owned()),
            source: "mcp_edit".to_owned(),
            request_id: "request".to_owned(),
            timestamp: Utc::now(),
            generation,
        },
        MutationRecord {
            mutation_id: "clean".to_owned(),
            session_id: "test-session".to_owned(),
            path: "clean_now.rs".to_owned(),
            before_hash: None,
            after_hash: Some("clean".to_owned()),
            source: "mcp_edit".to_owned(),
            request_id: "request".to_owned(),
            timestamp: Utc::now(),
            generation,
        },
    ]);
    actor.repo_status.write().dirty_files = vec!["still_dirty.rs".to_owned()];

    let summary = actor.summary("test-session", false).unwrap();
    let changed = summary["dirty_ownership"]["changed_by_mcp"]
        .as_array()
        .unwrap();

    assert_eq!(changed.len(), 1);
    assert_eq!(changed[0], "still_dirty.rs");
}

#[tokio::test]
async fn slow_bash_validation_detaches_and_reports_pending() {
    let root = tempdir().unwrap();
    let original = "fn value() -> i32 { 1 }\n";
    fs::write(root.path().join("value.rs"), original).unwrap();
    let actor = test_actor_with_budget(root.path(), 200);
    let result = actor
        .code_edit(
            "test-session",
            &json!({
                "changes": [{
                    "kind": "replace",
                    "path": "value.rs",
                    "old_text": "{ 1 }",
                    "new_text": "{ 2 }",
                    "expected_hash": content_hash(original)
                }],
                "validate": ["echo checking; sleep 30", "echo later"],
                "rollback_on_failure": false
            }),
        )
        .await
        .unwrap();
    assert_eq!(result["applied"], true);
    assert_eq!(result["validation_pending"], true);
    assert!(result["validation_run_id"].is_string());
    assert_eq!(result["validation"].as_array().unwrap().len(), 2);
    assert_eq!(result["validation"][1]["command"], "echo later");
    assert_eq!(
        result["validation"][1]["result"]["reason"],
        "blocked_by_pending_validation"
    );
    // Edit stays applied; validation could not drive a synchronous rollback.
    assert_eq!(
        fs::read_to_string(root.path().join("value.rs")).unwrap(),
        "fn value() -> i32 { 2 }\n"
    );
    let run_id = result["validation_run_id"].as_str().unwrap();
    let _ = actor.bash.cancel_for_session("test-session", run_id);
}

#[test]
fn reversed_handle_ranges_are_rejected() {
    let error = line_range_bytes("first\nsecond\n", 3, 2).unwrap_err();
    assert_eq!(error.0.code, "INVALID_HANDLE_RANGE");
}

#[test]
fn cr_only_content_is_not_treated_as_supported_multiline_text() {
    let content = "first\rsecond\r";

    assert_eq!(line_ending_label(content), "mixed");
    assert_eq!(
        normalize_line_endings_for_content(content, "replacement\ntext"),
        "replacement\ntext"
    );
    assert_eq!(
        line_range_bytes(content, 2, 2).unwrap(),
        (content.len(), content.len())
    );
}

#[test]
fn changes_treats_since_generation_as_exclusive() {
    let root = tempdir().unwrap();
    fs::write(root.path().join("value.rs"), "fn value() {}\n").unwrap();
    let actor = test_actor(root.path());
    actor.mutations.lock().push_back(MutationRecord {
        mutation_id: "current".to_owned(),
        session_id: "test-session".to_owned(),
        path: "value.rs".to_owned(),
        before_hash: None,
        after_hash: Some("hash".to_owned()),
        source: "mcp_edit".to_owned(),
        request_id: "request".to_owned(),
        timestamp: Utc::now(),
        generation: 7,
    });

    let after_six = actor
        .changes("test-session", &json!({"since_generation": 6}))
        .unwrap();
    assert_eq!(after_six["mutations"].as_array().unwrap().len(), 1);

    let after_seven = actor
        .changes("test-session", &json!({"since_generation": 7}))
        .unwrap();
    assert!(after_seven["mutations"].as_array().unwrap().is_empty());
}

#[test]
fn changes_are_filtered_by_calling_session() {
    let root = tempdir().unwrap();
    fs::write(root.path().join("value.rs"), "fn value() {}\n").unwrap();
    let actor = test_actor(root.path());
    for session_id in ["session-a", "session-b"] {
        actor.mutations.lock().push_back(MutationRecord {
            mutation_id: format!("mutation-{session_id}"),
            session_id: session_id.to_owned(),
            path: format!("{session_id}.rs"),
            before_hash: None,
            after_hash: Some("hash".to_owned()),
            source: "mcp_edit".to_owned(),
            request_id: "request".to_owned(),
            timestamp: Utc::now(),
            generation: actor.generation(),
        });
    }

    let result = actor
        .changes("session-a", &json!({"since_generation": 0}))
        .unwrap();
    let mutations = result["mutations"].as_array().unwrap();

    assert_eq!(mutations.len(), 1);
    assert_eq!(mutations[0]["session_id"], "session-a");
    assert_eq!(mutations[0]["path"], "session-a.rs");
}

#[test]
fn historical_journal_records_are_not_current_session_changes() {
    let root = tempdir().unwrap();
    fs::write(root.path().join("value.rs"), "fn value() {}\n").unwrap();
    let actor = test_actor(root.path());
    actor.mutations.lock().push_back(MutationRecord {
        mutation_id: "old".to_owned(),
        session_id: "previous-session".to_owned(),
        path: "value.rs".to_owned(),
        before_hash: None,
        after_hash: Some("hash".to_owned()),
        source: "external".to_owned(),
        request_id: "old-request".to_owned(),
        timestamp: Utc::now(),
        generation: 99,
    });

    let result = actor
        .changes("test-session", &json!({"since_generation": 0}))
        .unwrap();
    assert!(result["mutations"].as_array().unwrap().is_empty());
}

#[test]
fn workspace_generation_resumes_after_persisted_journal_records() {
    let root = tempdir().unwrap();
    let cache = tempdir().unwrap();
    fs::write(root.path().join("value.rs"), "fn value() {}\n").unwrap();
    let canonical_root = root.path().canonicalize().unwrap();
    let repo_cache = cache
        .path()
        .join("repos")
        .join(content_hash(&canonical_root.to_string_lossy()));
    fs::create_dir_all(&repo_cache).unwrap();
    let record = MutationRecord {
        mutation_id: "persisted".to_owned(),
        session_id: "previous-session".to_owned(),
        path: "value.rs".to_owned(),
        before_hash: None,
        after_hash: Some("hash".to_owned()),
        source: "external".to_owned(),
        request_id: "old-request".to_owned(),
        timestamp: Utc::now(),
        generation: 99,
    };
    fs::write(
        repo_cache.join("mutations.jsonl"),
        format!("{}\n", serde_json::to_string(&record).unwrap()),
    )
    .unwrap();

    let actor = WorkspaceActor::open(
        &WorkspaceConfig {
            id: "main".to_owned(),
            name: "Main".to_owned(),
            path: root.path().to_string_lossy().into_owned(),
            artifact_paths: Vec::new(),
            exclude_paths: Vec::new(),
        },
        test_policy(),
        cache.path().to_path_buf(),
    )
    .unwrap();

    assert_eq!(actor.generation(), 99);
}

#[test]
fn oversized_journal_rotates_at_open() {
    let cache = tempdir().unwrap();
    let journal = cache.path().join("mutations.jsonl");
    fs::write(&journal, vec![b'x'; (MAX_JOURNAL_BYTES + 1) as usize]).unwrap();

    rotate_journal_if_needed(&journal).unwrap();

    assert!(!journal.exists());
    assert!(cache.path().join("mutations.previous.jsonl").exists());
}

#[test]
fn interrupted_journal_rotation_recovers_previous_archive() {
    let cache = tempdir().unwrap();
    let journal = cache.path().join("mutations.jsonl");
    let archive = cache.path().join("mutations.previous.jsonl");
    let backup = cache.path().join("mutations.previous.backup.jsonl");
    fs::write(&journal, b"current").unwrap();
    fs::write(&backup, b"previous").unwrap();

    rotate_journal_if_needed(&journal).unwrap();

    assert_eq!(fs::read(&archive).unwrap(), b"previous");
    assert!(!backup.exists());
    assert_eq!(fs::read(&journal).unwrap(), b"current");
}

#[test]
fn journal_rotation_replaces_archive_without_discarding_live_journal_first() {
    let cache = tempdir().unwrap();
    let journal = cache.path().join("mutations.jsonl");
    let archive = cache.path().join("mutations.previous.jsonl");
    let backup = cache.path().join("mutations.previous.backup.jsonl");
    fs::write(&archive, b"older").unwrap();
    fs::write(&journal, vec![b'x'; (MAX_JOURNAL_BYTES + 1) as usize]).unwrap();

    rotate_journal_if_needed(&journal).unwrap();

    assert!(!journal.exists());
    assert_eq!(fs::metadata(&archive).unwrap().len(), MAX_JOURNAL_BYTES + 1);
    assert!(!backup.exists());
}

#[test]
fn changed_paths_are_filtered_and_capped() {
    let mut paths: HashSet<String> = (0..150)
        .map(|index| format!("src/file_{index}.rs"))
        .collect();
    paths.insert("core/target-audit/release/app.exe".to_owned());
    let summary = summarize_changed_paths(paths);
    assert_eq!(summary.paths.len(), MAX_OBSERVED_CHANGED_PATHS);
    assert_eq!(summary.count, 150);
    assert!(summary.truncated);
    assert_eq!(summary.groups.len(), 1);
    assert_eq!(summary.groups[0].path, "src");
    assert_eq!(summary.groups[0].count, 150);
}

#[test]
fn changed_path_groups_reserve_slot_for_other_bucket() {
    let paths: HashSet<String> = (0..(MAX_CHANGED_PATH_GROUPS + 5))
        .map(|index| format!("dir_{index}/file.rs"))
        .collect();

    let summary = summarize_changed_paths(paths);

    assert_eq!(summary.groups.len(), MAX_CHANGED_PATH_GROUPS);
    assert_eq!(summary.groups.last().unwrap().path, "(other)");
    assert_eq!(summary.groups.last().unwrap().count, 6);
}

#[test]
fn workspace_summary_caps_and_groups_large_change_sets() {
    let root = tempdir().unwrap();
    fs::write(root.path().join("value.rs"), "fn value() {}\n").unwrap();
    let actor = test_actor(root.path());
    actor.external_changed.lock().extend(
        (0..45)
            .map(|index| format!("backend/artifacts/result_{index}.json"))
            .chain((0..5).map(|index| format!("src/feature_{index}.rs"))),
    );
    actor.repo_status.write().dirty_files = (0..50)
        .map(|index| {
            if index < 45 {
                format!("backend/artifacts/result_{index}.json")
            } else {
                format!("src/feature_{}.rs", index - 45)
            }
        })
        .collect();

    let summary = actor.summary("test-session", false).unwrap();

    assert_eq!(
        summary["dirty_ownership"]["observed_external"]
            .as_array()
            .unwrap()
            .len(),
        MAX_OBSERVED_CHANGED_PATHS
    );
    assert_eq!(
        summary["dirty_ownership"]["counts"]["observed_external"],
        50
    );
    assert_eq!(
        summary["dirty_ownership"]["groups"]["observed_external"][0]["path"],
        "backend/artifacts"
    );
    assert_eq!(
        summary["dirty_ownership"]["groups"]["observed_external"][0]["count"],
        45
    );
    assert_eq!(
        summary["repository"]["dirty_files"]
            .as_array()
            .unwrap()
            .len(),
        MAX_OBSERVED_CHANGED_PATHS
    );
    assert_eq!(summary["repository"]["dirty_file_count"], 50);
    assert_eq!(summary["repository"]["dirty_files_truncated"], true);
}

// D7: coverage for edit-pipeline invariants that had no regression test.

#[tokio::test]
async fn overlapping_exact_edits_in_one_transaction_are_rejected() {
    let root = tempdir().unwrap();
    let original = "let value = compute(alpha, alpha);\n";
    fs::write(root.path().join("value.rs"), original).unwrap();
    let actor = test_actor(root.path());

    // Two `replace` changes whose matched byte ranges overlap on the same file
    // must be refused before anything is written.
    let error = actor
        .code_edit(
            "test-session",
            &json!({
                "changes": [
                    {
                        "kind": "replace",
                        "path": "value.rs",
                        "old_text": "compute(alpha, alpha)",
                        "new_text": "compute(beta, beta)",
                        "expected_hash": content_hash(original)
                    },
                    {
                        "kind": "replace",
                        "path": "value.rs",
                        "old_text": "alpha, alpha",
                        "new_text": "gamma, gamma",
                        "expected_hash": content_hash(original)
                    }
                ]
            }),
        )
        .await
        .unwrap_err();

    assert_eq!(error.0.code, "OVERLAPPING_EDITS");
    assert_eq!(
        fs::read_to_string(root.path().join("value.rs")).unwrap(),
        original
    );
}

#[tokio::test]
async fn handle_edit_cannot_share_a_file_with_another_change() {
    let root = tempdir().unwrap();
    let path = root.path().join("value.txt");
    let original = "one\ntwo\nthree\n";
    fs::write(&path, original).unwrap();
    let actor = test_actor(root.path());
    let fetched = actor
        .read_targets(&json!({
            "path": "value.txt",
            "start_line": 2,
            "end_line": 2
        }))
        .unwrap();
    let handle = fetched["results"][0]["handle"].as_str().unwrap();

    let error = actor
        .code_edit(
            "test-session",
            &json!({
                "changes": [
                    {
                        "kind": "replace_range",
                        "path": "value.txt",
                        "handle": handle,
                        "new_text": "TWO"
                    },
                    {
                        "kind": "replace",
                        "path": "value.txt",
                        "old_text": "three",
                        "new_text": "THREE",
                        "expected_hash": content_hash(original)
                    }
                ]
            }),
        )
        .await
        .unwrap_err();

    assert_eq!(error.0.code, "AMBIGUOUS_HANDLE_EDIT_ORDER");
    assert_eq!(fs::read_to_string(path).unwrap(), original);
}

#[tokio::test]
async fn syntax_error_gate_blocks_broken_rust_and_leaves_file_untouched() {
    let root = tempdir().unwrap();
    let original = "fn value() -> i32 { 1 }\n";
    fs::write(root.path().join("value.rs"), original).unwrap();
    let actor = test_actor(root.path());

    let error = actor
        .code_edit(
            "test-session",
            &json!({
                "changes": [{
                    "kind": "replace",
                    "path": "value.rs",
                    "old_text": "{ 1 }",
                    "new_text": "{ 1 ",
                    "expected_hash": content_hash(original)
                }]
            }),
        )
        .await
        .unwrap_err();

    assert_eq!(error.0.code, "SYNTAX_ERROR");
    assert_eq!(
        fs::read_to_string(root.path().join("value.rs")).unwrap(),
        original
    );
}

#[tokio::test]
async fn json_edits_are_syntax_checked_and_yaml_edits_are_reported_skipped() {
    let root = tempdir().unwrap();
    let json_original = "{\n  \"a\": 1\n}\n";
    let yaml_original = "a: 1\n";
    fs::write(root.path().join("data.json"), json_original).unwrap();
    fs::write(root.path().join("data.yaml"), yaml_original).unwrap();
    let actor = test_actor(root.path());

    // D5: JSON now has a bundled grammar, so a broken JSON edit is gated.
    let error = actor
        .code_edit(
            "test-session",
            &json!({
                "changes": [{
                    "kind": "replace",
                    "path": "data.json",
                    "old_text": "\"a\": 1",
                    "new_text": "\"a\": 1,",
                    "expected_hash": content_hash(json_original)
                }]
            }),
        )
        .await
        .unwrap_err();
    assert_eq!(error.0.code, "SYNTAX_ERROR");
    assert_eq!(
        fs::read_to_string(root.path().join("data.json")).unwrap(),
        json_original
    );

    // A valid JSON edit reports the check ran; a YAML edit (no grammar) reports
    // the bypass explicitly as "skipped" rather than silently passing.
    let applied = actor
        .code_edit(
            "test-session",
            &json!({
                "changes": [
                    {
                        "kind": "replace",
                        "path": "data.json",
                        "old_text": "\"a\": 1",
                        "new_text": "\"a\": 2",
                        "expected_hash": content_hash(json_original)
                    },
                    {
                        "kind": "replace",
                        "path": "data.yaml",
                        "old_text": "a: 1",
                        "new_text": "a: 2",
                        "expected_hash": content_hash(yaml_original)
                    }
                ]
            }),
        )
        .await
        .unwrap();

    assert_eq!(applied["applied"], true);
    let checks = applied["syntax_checks"].as_array().unwrap();
    let json_check = checks
        .iter()
        .find(|item| item["path"] == "data.json")
        .unwrap();
    let yaml_check = checks
        .iter()
        .find(|item| item["path"] == "data.yaml")
        .unwrap();
    assert_eq!(json_check["syntax_check"], "checked");
    assert_eq!(yaml_check["syntax_check"], "skipped");
}

#[tokio::test]
async fn symbol_anchored_insert_positions_place_content_relative_to_the_symbol() {
    let cases = [
        (
            "before",
            "// before-marker\nfn target() {\n    body();\n}\n",
        ),
        ("after", "fn target() {\n    body();\n}\n// after-marker\n"),
        (
            "inside_start",
            "fn target() {\n// inside-marker\n    body();\n}\n",
        ),
        (
            "inside_end",
            "fn target() {\n    body();\n// inside-marker\n}\n",
        ),
    ];

    for (position, expected) in cases {
        let root = tempdir().unwrap();
        let original = "fn target() {\n    body();\n}\n";
        fs::write(root.path().join("value.rs"), original).unwrap();
        let actor = test_actor(root.path());
        let marker = match position {
            "before" => "// before-marker\n",
            "after" => "// after-marker\n",
            _ => "// inside-marker\n",
        };

        let result = actor
            .code_edit(
                "test-session",
                &json!({
                    "changes": [{
                        "kind": "insert",
                        "path": "value.rs",
                        "anchor_symbol": "target",
                        "position": position,
                        "content": marker,
                        "expected_hash": content_hash(original)
                    }]
                }),
            )
            .await
            .unwrap();

        assert_eq!(result["applied"], true, "position {position} should apply");
        assert_eq!(
            fs::read_to_string(root.path().join("value.rs")).unwrap(),
            expected,
            "position {position} placement"
        );
    }
}

#[tokio::test]
async fn same_file_multi_change_accumulates_on_the_in_progress_plan() {
    let root = tempdir().unwrap();
    let original = "one\ntwo\nthree\n";
    fs::write(root.path().join("value.txt"), original).unwrap();
    let actor = test_actor(root.path());

    // Two distinct, non-overlapping edits to the same file in one transaction each
    // match against the original text (overlap preflight is snapshot-based) but
    // must both land: the second is planned on top of the first via put_plan, not
    // re-read from disk. Only one expected_hash precondition is needed.
    let result = actor
        .code_edit(
            "test-session",
            &json!({
                "changes": [
                    {
                        "kind": "replace",
                        "path": "value.txt",
                        "old_text": "one",
                        "new_text": "ONE",
                        "expected_hash": content_hash(original)
                    },
                    {
                        "kind": "replace",
                        "path": "value.txt",
                        "old_text": "three",
                        "new_text": "THREE",
                        "expected_hash": content_hash(original)
                    }
                ]
            }),
        )
        .await
        .unwrap();

    assert_eq!(result["applied"], true);
    assert_eq!(
        fs::read_to_string(root.path().join("value.txt")).unwrap(),
        "ONE\ntwo\nTHREE\n"
    );
}

#[tokio::test]
async fn deferred_validation_rolls_back_when_requested() {
    let root = tempdir().unwrap();
    let original = "fn value() -> i32 { 1 }\n";
    fs::write(root.path().join("value.rs"), original).unwrap();
    let actor = test_actor_with_budget(root.path(), 200);

    // A rollback-protected edit must never be left applied with validation still
    // running in the background. The promoted validation is cancelled and the
    // original content is restored.
    let result = actor
        .code_edit(
            "test-session",
            &json!({
                "changes": [{
                    "kind": "replace",
                    "path": "value.rs",
                    "old_text": "{ 1 }",
                    "new_text": "{ 2 }",
                    "expected_hash": content_hash(original)
                }],
                "validate": ["echo checking; sleep 30"],
                "rollback_on_failure": true
            }),
        )
        .await
        .unwrap();

    assert_eq!(result["applied"], false);
    assert_eq!(result["rolled_back"], true);
    assert_eq!(result["reason"], "validation_failed");
    assert!(result["validation"]
        .as_array()
        .unwrap()
        .iter()
        .any(|entry| {
            entry["reason"] == "rollback_requires_synchronous_validation"
                && entry["cancellation"]["terminal"]["status"] == "cancelled"
        }));
    assert_eq!(
        fs::read_to_string(root.path().join("value.rs")).unwrap(),
        original
    );
}
