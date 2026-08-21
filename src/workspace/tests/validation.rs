use super::*;

#[tokio::test]
async fn detached_validation_keeps_leading_failure_as_primary_run() {
    let root = tempdir().unwrap();
    let original = "fn value() -> i32 { 1 }\n";
    fs::write(root.path().join("value.rs"), original).unwrap();
    let actor = test_actor_with_budget(root.path(), 20);
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
                "sleep 0.2; exit 7",
                "printf later > validation-after-failure.txt"
            ]
        }))
        .await
        .unwrap();

    assert_eq!(result["validation_status"], "pending");
    let leading_run_id = result["validation_run_id"].as_str().unwrap();
    let deferred_run_id = result["deferred_validation_run_id"].as_str().unwrap();
    assert_eq!(result["validation"][0]["result"]["run_id"], leading_run_id);
    assert_eq!(result["validation"][1]["result"]["run_id"], deferred_run_id);
    assert!(result["guidance"]
        .as_str()
        .unwrap()
        .contains("every ID in validation_run_ids"));

    let mut leading = actor.bash.status(leading_run_id).unwrap();
    for _ in 0..100 {
        if leading["ended_at"].is_string() {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(20)).await;
        leading = actor.bash.status(leading_run_id).unwrap();
    }
    assert_eq!(leading["status"], "failed");

    let mut deferred = actor.bash.status(deferred_run_id).unwrap();
    for _ in 0..100 {
        if deferred["ended_at"].is_string() {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(20)).await;
        deferred = actor.bash.status(deferred_run_id).unwrap();
    }
    assert_eq!(deferred["status"], "succeeded");
    assert_eq!(
        fs::read_to_string(root.path().join("validation-after-failure.txt")).unwrap(),
        "later"
    );
}

#[tokio::test]
async fn syntax_error_gate_blocks_broken_rust_and_leaves_file_untouched() {
    let root = tempdir().unwrap();
    let original = "fn value() -> i32 { 1 }\n";
    fs::write(root.path().join("value.rs"), original).unwrap();
    let actor = test_actor(root.path());

    let error = actor
        .code_edit(&json!({
            "changes": [{
                "kind": "replace",
                "path": "value.rs",
                "old_text": "{ 1 }",
                "new_text": "{ 1 ",
                "expected_hash": content_hash(original)
            }]
        }))
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
        .code_edit(&json!({
            "changes": [{
                "kind": "replace",
                "path": "data.json",
                "old_text": "\"a\": 1",
                "new_text": "\"a\": 1,",
                "expected_hash": content_hash(json_original)
            }]
        }))
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
        .code_edit(&json!({
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
        }))
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
