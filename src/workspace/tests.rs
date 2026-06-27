use super::edit::PlannedFile;
use super::journal::{rotate_journal_if_needed, MutationRecord, MAX_JOURNAL_BYTES};
use super::util::{line_range_bytes, summarize_changed_paths, MAX_OBSERVED_CHANGED_PATHS};
use super::{TaskBaseline, WorkspaceActor};
use crate::index::content_hash;
use crate::model::{PolicyConfig, WorkspaceConfig};
use chrono::Utc;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::sync::Arc;
use tempfile::tempdir;

fn test_policy() -> PolicyConfig {
    PolicyConfig {
        max_file_bytes: 1_000_000,
        max_context_chars: 50_000,
        max_search_results: 100,
        max_task_output_chars: 30_000,
        shell_enabled: false,
        allowed_commands: vec!["cargo".to_owned(), "npm".to_owned()],
        task_retention_hours: None,
    }
}

fn test_actor(root: &Path) -> Arc<WorkspaceActor> {
    test_actor_with_exclusions(root, Vec::new())
}

fn test_actor_with_exclusions(root: &Path, exclude_paths: Vec<String>) -> Arc<WorkspaceActor> {
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
            test_policy(),
            HashMap::new(),
            cache,
        )
        .unwrap(),
    )
}

#[test]
fn fetch_batches_return_successes_and_item_errors() {
    let root = tempdir().unwrap();
    fs::write(root.path().join("valid.rs"), "fn valid() {}\n").unwrap();
    let actor = test_actor(root.path());
    let result = actor
        .code_fetch(&json!({
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
fn fetch_accepts_direct_path_parameters() {
    let root = tempdir().unwrap();
    fs::write(root.path().join("direct.rs"), "one\ntwo\nthree\n").unwrap();
    let actor = test_actor(root.path());

    let result = actor
        .code_fetch(&json!({
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

#[test]
fn outline_accepts_multiple_paths_and_reports_partial_errors() {
    let root = tempdir().unwrap();
    fs::write(root.path().join("one.rs"), "pub fn one() {}\n").unwrap();
    fs::write(root.path().join("two.rs"), "pub fn two() {}\n").unwrap();
    let actor = test_actor(root.path());

    let single = actor
        .code_search(&json!({"mode": "outline", "paths": ["one.rs"]}))
        .unwrap();
    assert_eq!(single["path"], "one.rs");
    assert!(single["symbols"].is_array());

    let batch = actor
        .code_search(&json!({
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
        .code_fetch(&json!({"items": [{"kind": "metadata", "value": "lib.rs"}]}))
        .unwrap();
    assert_eq!(metadata["results"][0]["kind"], "metadata");
    assert_eq!(metadata["results"][0]["language"], "rust");
    assert_eq!(metadata["results"][0]["line_count"], 7);
    assert!(metadata["results"][0].get("content").is_none());

    let symbol = actor
        .code_fetch(&json!({
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
        .code_fetch(&json!({
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
fn workspace_diagnostics_exposes_profiles_and_limits() {
    let root = tempdir().unwrap();
    fs::write(root.path().join("main.rs"), "fn main() {}\n").unwrap();
    let actor = test_actor(root.path());

    let diagnostics = actor.diagnostics().unwrap();

    assert_eq!(diagnostics["workspace_id"], "main");
    assert_eq!(diagnostics["file_count"], 1);
    assert_eq!(diagnostics["policy"]["max_search_results"], 100);
    assert!(diagnostics["task_profiles"].is_array());
}

#[test]
fn code_capabilities_reports_public_contracts() {
    let root = tempdir().unwrap();
    fs::write(root.path().join("main.rs"), "fn main() {}\n").unwrap();
    let actor = test_actor(root.path());

    let capabilities = actor.code_capabilities().unwrap();

    assert_eq!(capabilities["workspace_id"], "main");
    assert!(capabilities["search_modes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|mode| mode == "outline"));
    assert!(capabilities["fetch_kinds"]
        .as_array()
        .unwrap()
        .iter()
        .any(|kind| kind == "metadata"));
    assert_eq!(capabilities["editing"]["supports_transaction"], true);
}

#[test]
fn code_context_accepts_weighted_terms_without_legacy_query() {
    let root = tempdir().unwrap();
    fs::write(
        root.path().join("mcp.rs"),
        "pub fn register_mcp_tools() { map_structured_errors(); }\nfn map_structured_errors() {}\n",
    )
    .unwrap();
    let actor = test_actor(root.path());

    let context = actor
        .code_context(&json!({
            "required_terms": ["mcp"],
            "optional_terms": ["structured errors"],
            "document_types": ["source"]
        }))
        .unwrap();

    assert_eq!(context["result_count"], 1);
    assert_eq!(context["results"][0]["path"], "mcp.rs");
    assert!(context["results"][0]["score"].is_number());
}

#[tokio::test]
async fn task_status_fetch_and_task_local_changed_paths_are_bounded() {
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
                "command": ["cargo", "--version"],
                "background": false
            }),
        )
        .await
        .unwrap();
    let task_id = started["task_id"].as_str().unwrap();
    assert_eq!(started["status_fetch"]["kind"], "task_status");
    assert_eq!(started["status_fetch"]["value"], task_id);

    let fetched = actor
        .code_fetch(&json!({
            "items": [{"kind": "task_status", "value": task_id}]
        }))
        .unwrap();

    assert_eq!(fetched["result_count"], 1);
    assert_eq!(fetched["results"][0]["task_id"], task_id);
    assert_eq!(started["observed_changed_path_count"], 0);
    assert_eq!(fetched["results"][0]["status"], "succeeded");

    let bounded = actor
        .code_fetch(&json!({
            "items": [
                {"kind": "task_status", "value": task_id},
                {"kind": "task_status", "value": task_id}
            ],
            "max_chars": 5
        }))
        .unwrap();
    assert!(bounded["results"][0]["output"].as_str().unwrap().len() <= 5);
    assert_eq!(bounded["results"][0]["output_truncated"], true);
    assert_eq!(bounded["result_count"], 1);
    assert_eq!(bounded["items_truncated"], true);
    assert_eq!(bounded["chars_truncated"], true);
    assert_eq!(bounded["truncated"], true);
}

#[test]
fn task_change_detection_includes_new_mutations_to_already_dirty_files() {
    let root = tempdir().unwrap();
    fs::write(root.path().join("existing.rs"), "fn existing() {}\n").unwrap();
    let actor = test_actor(root.path());
    let generation = actor.generation();
    let dirty_files = HashSet::from(["existing.rs".to_owned()]);
    let baseline = TaskBaseline {
        generation,
        dirty_files: dirty_files.clone(),
    };
    actor.mutations.lock().push_back(MutationRecord {
        mutation_id: "during-task".to_owned(),
        session_id: "external".to_owned(),
        path: "existing.rs".to_owned(),
        before_hash: Some("before".to_owned()),
        after_hash: Some("after".to_owned()),
        source: "external".to_owned(),
        request_id: "watcher".to_owned(),
        timestamp: Utc::now(),
        generation: generation + 1,
    });

    let observed = actor.observed_task_changed_paths(&baseline, &dirty_files);

    assert_eq!(observed, HashSet::from(["existing.rs".to_owned()]));
}

#[test]
fn search_accepts_multiple_queries() {
    let root = tempdir().unwrap();
    fs::write(root.path().join("alpha.rs"), "fn alpha() {}\n").unwrap();
    fs::write(root.path().join("beta.rs"), "fn beta() {}\n").unwrap();
    let actor = test_actor(root.path());
    let result = actor
        .code_search(&json!({
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
        .code_fetch(&json!({"path": "existing.rs", "max_chars": 5_000}))
        .unwrap();
    assert_eq!(fetch["reconcile_pending"], true);
    assert!(fetch["phase_ms"]["fetch_items"].is_number());

    let search = actor
        .code_search(&json!({"mode": "literal", "query": "pending_symbol"}))
        .unwrap();
    assert_eq!(search["reconcile_pending"], true);
    assert_eq!(search["result_count"], 0);
    assert!(search["phase_ms"]["index_search"].is_number());

    let context = actor
        .code_context(&json!({"query": "existing_symbol"}))
        .unwrap();
    assert_eq!(context["reconcile_pending"], true);
    assert_eq!(context["result_count"], 1);
    assert!(context.get("recent_task_failures").is_none());
    assert!(context["phase_ms"]["index_context"].is_number());

    let context_with_failures = actor
        .code_context(&json!({
            "query": "existing_symbol",
            "include_task_failures": true
        }))
        .unwrap();
    assert!(context_with_failures["recent_task_failures"].is_array());
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
        .code_fetch(&json!({
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
        .code_fetch(&json!({
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
        .code_fetch(&json!({
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
        .code_fetch(&json!({
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
        .code_fetch(&json!({
            "path": "range.txt",
            "start_line": 2,
            "end_line": 4,
            "max_chars": 7
        }))
        .unwrap();
    assert_eq!(first["results"][0]["content"], "alpha\nb");
    let continuation = first["results"][0]["continuation"].as_str().unwrap();

    let second = actor
        .code_fetch(&json!({
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
        .code_fetch(&json!({
            "path": "handle.txt",
            "start_line": 2,
            "end_line": 4,
            "max_chars": 100
        }))
        .unwrap();
    let handle = direct["results"][0]["handle"].as_str().unwrap();

    let first = actor
        .code_fetch(&json!({
            "items": [{"kind": "handle", "value": handle}],
            "max_chars": 7
        }))
        .unwrap();
    let continuation = first["results"][0]["continuation"].as_str().unwrap();
    let second = actor
        .code_fetch(&json!({
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
        .code_fetch(&json!({"path": "value.txt", "max_chars": 5_000}))
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

#[tokio::test]
async fn unknown_validation_profile_fails_before_mutation() {
    let root = tempdir().unwrap();
    let original = "fn value() -> i32 { 1 }\n";
    fs::write(root.path().join("value.rs"), original).unwrap();
    let actor = test_actor(root.path());
    let summary = actor.summary("test-session", false).unwrap();
    assert_eq!(
        summary["capabilities"]["profile_validation_available"],
        false
    );
    assert_eq!(summary["capabilities"]["raw_commands_available"], true);
    assert!(summary["warnings"]
        .as_array()
        .is_some_and(|warnings| !warnings.is_empty()));
    let generation = actor.generation();
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
                "validate": ["typecheck"]
            }),
        )
        .await
        .unwrap_err();
    assert_eq!(error.0.code, "UNKNOWN_VALIDATION_PROFILE");
    assert_eq!(
        fs::read_to_string(root.path().join("value.rs")).unwrap(),
        original
    );
    assert_eq!(actor.generation(), generation);
}

#[test]
fn reversed_handle_ranges_are_rejected() {
    let error = line_range_bytes("first\nsecond\n", 3, 2).unwrap_err();
    assert_eq!(error.0.code, "INVALID_HANDLE_RANGE");
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
        HashMap::new(),
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
        .map(|index| format!("backend/artifacts/result_{index}.json"))
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
