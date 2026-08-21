use super::*;

#[test]
fn workspace_diagnostics_exposes_bash_policy_and_limits() {
    let root = tempdir().unwrap();
    fs::write(root.path().join("main.rs"), "fn main() {}\n").unwrap();
    let actor = test_actor(root.path());

    let diagnostics = actor.diagnostics().unwrap();

    assert_eq!(diagnostics["workspace_id"], "main");
    assert_eq!(diagnostics["file_count"], 1);
    assert_eq!(diagnostics["policy"]["max_search_results"], 100);
    assert!(diagnostics["policy"]["bash"].get("enabled").is_none());
    assert!(diagnostics["execution"]["bash"]["readiness"].is_string());
}

#[tokio::test]
async fn bash_status_fetch_and_run_local_changed_paths_are_bounded() {
    let root = tempdir().unwrap();
    fs::write(root.path().join("main.rs"), "fn main() {}\n").unwrap();
    let actor = test_actor(root.path());
    actor.mutations.lock().push_back(MutationRecord {
        mutation_id: "event".to_owned(),
        path: "unrelated/generated.txt".to_owned(),
        before_hash: None,
        after_hash: Some("hash".to_owned()),
        source: "external".to_owned(),
        request_id: "test".to_owned(),
        timestamp: Utc::now(),
        generation: actor.generation(),
    });

    let started = actor
        .run(&json!({
            "command": "printf codeweave-bash-test",
            "background": false
        }))
        .await
        .unwrap();
    let run_id = started["run_id"].as_str().unwrap();
    assert_eq!(started["status_fetch"]["kind"], "bash_status");
    assert_eq!(started["status_fetch"]["value"], run_id);

    let fetched = actor
        .read_targets_batch(&json!({
            "items": [{"kind": "bash_status", "value": run_id}]
        }))
        .unwrap();

    assert_eq!(fetched["result_count"], 1);
    assert_eq!(fetched["results"][0]["run_id"], run_id);
    assert_eq!(started["observed_changed_path_count"], 0);
    assert_eq!(fetched["results"][0]["status"], "succeeded");

    let bounded = actor
        .read_targets_batch(&json!({
            "items": [
                {"kind": "bash_status", "value": run_id},
                {"kind": "bash_status", "value": run_id}
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

#[tokio::test]
async fn completed_bash_status_does_not_attribute_later_workspace_writes() {
    let root = tempdir().unwrap();
    fs::write(root.path().join("main.rs"), "fn main() {}\n").unwrap();
    let actor = test_actor(root.path());

    let started = actor
        .run(&json!({
            "command": "printf codeweave-bash-test",
            "background": true
        }))
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
            &[PlannedFile {
                path: ".ai-bridge/codeweave-audit-success.md".to_owned(),
                before: None,
                after: Some("created after bash exit\n".to_owned()),
            }],
            "later-write",
        )
        .unwrap();

    let fetched = actor
        .run(&json!({
            "action": "status",
            "run_id": run_id
        }))
        .await
        .unwrap();

    assert_eq!(fetched["status"], "succeeded");
    assert_eq!(fetched["observed_changed_path_count"], 0);
    assert_eq!(fetched["observed_changed_paths"], json!([]));

    let fetched_again = actor
        .run(&json!({
            "action": "status",
            "run_id": run_id
        }))
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
    actor.mutations.lock().push_back(MutationRecord {
        mutation_id: "during-run".to_owned(),
        path: "existing.rs".to_owned(),
        before_hash: Some("before".to_owned()),
        after_hash: Some("after".to_owned()),
        source: "external".to_owned(),
        request_id: "watcher".to_owned(),
        timestamp: Utc::now(),
        generation: generation + 1,
    });

    let mutations = actor.mutations.lock().iter().cloned().collect::<Vec<_>>();
    let observed = actor.observed_run_changed_paths(
        &mutations,
        generation,
        &dirty_files,
        generation + 1,
        None,
        &dirty_files,
    );

    assert_eq!(observed, HashSet::from(["existing.rs".to_owned()]));
}

#[test]
fn delayed_reconcile_uses_file_modification_time_for_run_attribution() {
    let root = tempdir().unwrap();
    let path = root.path().join("existing.rs");
    fs::write(&path, "fn existing() {}\n").unwrap();
    let actor = test_actor(root.path());
    let generation = actor.generation();
    let dirty_files = HashSet::from(["existing.rs".to_owned()]);
    fs::write(&path, "fn existing() { println!(\"changed\"); }\n").unwrap();
    let ended_at = Utc::now() + ChronoDuration::seconds(5);
    actor.mutations.lock().push_back(MutationRecord {
        mutation_id: "delayed-watcher".to_owned(),
        path: "existing.rs".to_owned(),
        before_hash: Some("before".to_owned()),
        after_hash: Some("after".to_owned()),
        source: "external".to_owned(),
        request_id: "watcher".to_owned(),
        timestamp: ended_at + ChronoDuration::seconds(1),
        generation: generation + 1,
    });

    let mutations = actor.mutations.lock().iter().cloned().collect::<Vec<_>>();
    let observed = actor.observed_run_changed_paths(
        &mutations,
        generation,
        &dirty_files,
        generation + 1,
        Some(&ended_at),
        &dirty_files,
    );

    assert_eq!(observed, HashSet::from(["existing.rs".to_owned()]));
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

    let summary = actor.summary().unwrap();

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

#[tokio::test]
async fn failed_bash_validation_preserves_mutation() {
    let root = tempdir().unwrap();
    let original = "fn value() -> i32 { 1 }\n";
    fs::write(root.path().join("value.rs"), original).unwrap();
    let actor = test_actor(root.path());
    let summary = actor.summary().unwrap();
    assert_eq!(summary["capabilities"]["bash_available"], true);
    assert!(summary["warnings"].as_array().is_some_and(Vec::is_empty));
    let result = actor
        .code_edit(&json!({
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
        }))
        .await
        .unwrap();
    assert_eq!(result["applied"], true);
    assert_eq!(result["validation_failed"], true);
    assert_eq!(result["validation_status"], "failed");
    assert_eq!(result["reason"], "validation_failed");
    assert_eq!(result["validation"].as_array().unwrap().len(), 2);
    assert_eq!(
        result["validation"][1]["command"],
        "printf validation-failed >&2; exit 1"
    );
    assert_eq!(
        fs::read_to_string(root.path().join("value.rs")).unwrap(),
        "fn value() -> i32 { 2 }\n"
    );
    let changes = actor.changes(&json!({"since_generation": 0})).unwrap();
    assert!(changes["mutations"]
        .as_array()
        .unwrap()
        .iter()
        .all(|mutation| mutation["source"] == "mcp_edit"));
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

    let summary = actor.summary().unwrap();
    assert_eq!(summary["capabilities"]["bash_available"], false);
    assert_eq!(summary["capabilities"]["bash"]["readiness"], "unavailable");

    let error = actor
        .code_edit(&json!({
            "changes": [{
                "kind": "replace",
                "path": "value.rs",
                "old_text": "{ 1 }",
                "new_text": "{ 2 }",
                "expected_hash": content_hash(original)
            }],
            "validate": ["true"]
        }))
        .await
        .unwrap_err();

    assert_eq!(error.0.code, "BASH_UNAVAILABLE");
    assert_eq!(fs::read_to_string(path).unwrap(), original);
}

#[tokio::test]
async fn slow_bash_validation_queues_remaining_commands() {
    let root = tempdir().unwrap();
    let original = "fn value() -> i32 { 1 }\n";
    fs::write(root.path().join("value.rs"), original).unwrap();
    let actor = test_actor_with_budget(root.path(), 50);
    let result = actor
        .code_edit(&json!({
            "changes": [{
                "kind": "replace",
                "path": "value.rs",
                "old_text": "{ 1 }",
                "new_text": "{ 2 }",
                "expected_hash": content_hash(original)
            }],
            "validate": [
                "echo checking; sleep 1",
                "printf later > validation-later.txt"
            ]
        }))
        .await
        .unwrap();
    assert_eq!(result["applied"], true);
    assert_eq!(result["validation_pending"], true);
    assert_eq!(result["validation_status"], "pending");
    assert_eq!(result["validation"].as_array().unwrap().len(), 2);
    assert_eq!(
        result["validation"][1]["command"],
        "printf later > validation-later.txt"
    );
    assert_eq!(
        result["validation"][1]["result"]["reason"],
        "blocked_by_pending_validation"
    );
    assert_eq!(
        fs::read_to_string(root.path().join("value.rs")).unwrap(),
        "fn value() -> i32 { 2 }\n"
    );

    let leading_run_id = result["validation_run_id"].as_str().unwrap();
    let deferred_run_id = result["deferred_validation_run_id"].as_str().unwrap();
    assert_ne!(leading_run_id, deferred_run_id);
    assert_eq!(result["validation"][0]["result"]["run_id"], leading_run_id);
    assert_eq!(result["validation"][1]["result"]["run_id"], deferred_run_id);
    assert_eq!(
        result["validation_run_ids"],
        json!([leading_run_id, deferred_run_id])
    );

    let mut leading = actor.bash.status(leading_run_id).unwrap();
    for _ in 0..200 {
        if leading["ended_at"].is_string() {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(20)).await;
        leading = actor.bash.status(leading_run_id).unwrap();
    }
    assert_eq!(leading["status"], "succeeded");

    let mut deferred = actor.bash.status(deferred_run_id).unwrap();
    for _ in 0..200 {
        if deferred["ended_at"].is_string() {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(20)).await;
        deferred = actor.bash.status(deferred_run_id).unwrap();
    }
    assert_eq!(deferred["status"], "succeeded");
    assert_eq!(
        fs::read_to_string(root.path().join("validation-later.txt")).unwrap(),
        "later"
    );
}

#[test]
fn process_local_changes_include_external_mutations() {
    let root = tempdir().unwrap();
    fs::write(root.path().join("value.rs"), "fn value() {}\n").unwrap();
    let actor = test_actor(root.path());
    actor.mutations.lock().push_back(MutationRecord {
        mutation_id: "old".to_owned(),
        path: "value.rs".to_owned(),
        before_hash: None,
        after_hash: Some("hash".to_owned()),
        source: "external".to_owned(),
        request_id: "old-request".to_owned(),
        timestamp: Utc::now(),
        generation: 99,
    });

    let result = actor.changes(&json!({"since_generation": 0})).unwrap();
    assert_eq!(result["mutations"].as_array().unwrap().len(), 1);
}
