use super::*;

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
        .code_edit(&json!({"preview": true, "changes": changes.clone()}))
        .await
        .unwrap();
    assert_eq!(preview["preview"], true);
    assert_eq!(
        fs::read(&path).unwrap(),
        b"before\r\nold value\r\nafter\r\n"
    );

    let applied = actor.code_edit(&json!({"changes": changes})).await.unwrap();
    assert_eq!(applied["applied"], true);
    assert_eq!(
        fs::read(&path).unwrap(),
        b"before\r\nnew value\r\nafter\r\n"
    );
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
        .code_retrieve(
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
        .code_retrieve(&json!({
            "operations": [
                {"id": "file", "operation": "find_file", "name": "engine.rs"},
                {"id": "invalid", "operation": "find_symbol"}
            ]
        }))
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
        .code_retrieve(&json!({
            "operations": [
                {"id": "file", "operation": "find_file", "name": "engine.rs"},
                "not-an-object",
                {"id": "missing-operation"},
                {"id": "file", "operation": "find_symbol", "symbol": "extract"}
            ]
        }))
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
        .code_retrieve(&json!({
            "snapshot_id": "snap_stale",
            "operations": [
                {"operation": "find_file", "name": "engine.rs"}
            ]
        }))
        .unwrap_err();

    assert_eq!(error.0.code, "STALE_SNAPSHOT");
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
