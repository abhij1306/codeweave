use super::*;

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
        .code_edit(&json!({
            "changes": [{
                "kind": "replace",
                "path": "windows.txt",
                "handle": handle,
                "old_text": "\nold value",
                "new_text": "\nnew value"
            }]
        }))
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
        .code_edit(&json!({"preview": true, "changes": changes.clone()}))
        .await
        .unwrap();
    assert_eq!(preview["preview"], true);
    assert_eq!(fs::read(&path).unwrap(), b"first\r\nsecond\r\nthird\r\n");

    actor.code_edit(&json!({"changes": changes})).await.unwrap();
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
        .code_edit(&json!({
            "changes": [{
                "kind": "replace_range",
                "path": "value.txt",
                "handle": handle,
                "new_text": "updated"
            }]
        }))
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
        .code_edit(&json!({
            "changes": [{
                "kind": "replace",
                "path": "value.txt",
                "old_text": "first\nsecond\n",
                "new_text": "updated",
                "expected_hash": content_hash(original)
            }]
        }))
        .await
        .unwrap();

    assert_eq!(fs::read_to_string(path).unwrap(), "updated\nthird\n");
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

    let error = actor.commit_plan(&plan, "failed-write").unwrap_err();
    assert_eq!(error.0.code, "ATOMIC_WRITE_FAILED");
    let details = error.0.details.as_ref().unwrap();
    assert_eq!(details["failed_path"], "blocked");
    assert_eq!(details["completed_before_failure"], json!([]));
    assert_eq!(details["restored_paths"], json!([]));
    assert_eq!(details["compensation_failures"], json!([]));
    assert_eq!(details["manual_recovery_required"], false);
    assert!(!actor
        .internal_writes
        .lock()
        .contains_key(&actor.root.join("blocked")));
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
        .code_edit(&json!({"changes": change, "response_detail": "compact"}))
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
    actor.refresh(true).unwrap();
    let debug = actor
        .code_edit(&json!({"changes": change, "response_detail": "debug"}))
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
        .code_edit(&json!({
            "changes": [{
                "kind": "create",
                "path": "big.txt",
                "content": replaced,
                "overwrite": true,
                "expected_hash": content_hash(&original)
            }]
        }))
        .await
        .unwrap();
    assert_eq!(result["applied"], true);
    assert_eq!(result["diff_truncated"], true);
    assert_eq!(result["diff_omitted"], false);
    let diff = result["diff"].as_str().unwrap();
    assert!(diff.len() <= 2_000);
    assert!(diff.ends_with('\n'));
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

#[tokio::test]
async fn overlapping_exact_edits_in_one_transaction_are_rejected() {
    let root = tempdir().unwrap();
    let original = "let value = compute(alpha, alpha);\n";
    fs::write(root.path().join("value.rs"), original).unwrap();
    let actor = test_actor(root.path());

    // Two `replace` changes whose matched byte ranges overlap on the same file
    // must be refused before anything is written.
    let error = actor
        .code_edit(&json!({
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
        }))
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
        .code_edit(&json!({
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
        }))
        .await
        .unwrap_err();

    assert_eq!(error.0.code, "AMBIGUOUS_HANDLE_EDIT_ORDER");
    assert_eq!(fs::read_to_string(path).unwrap(), original);
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
            .code_edit(&json!({
                "changes": [{
                    "kind": "insert",
                    "path": "value.rs",
                    "anchor_symbol": "target",
                    "position": position,
                    "content": marker,
                    "expected_hash": content_hash(original)
                }]
            }))
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
        .code_edit(&json!({
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
        }))
        .await
        .unwrap();

    assert_eq!(result["applied"], true);
    assert_eq!(
        fs::read_to_string(root.path().join("value.txt")).unwrap(),
        "ONE\ntwo\nTHREE\n"
    );
}
