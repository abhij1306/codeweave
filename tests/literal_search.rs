use codeweave_rust::index::{CodeIndex, WorkspaceExclusions};
use codeweave_rust::retrieval::{
    execute_index_search, prepare_retrieval_operation, PreparedRetrievalOperation,
};
use codeweave_rust::security::WorkspaceRoot;
use serde_json::{json, Value};

fn indexed(files: &[(&str, &str)]) -> (tempfile::TempDir, CodeIndex) {
    let root = tempfile::tempdir().unwrap();
    for (path, content) in files {
        let path = root.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }
    let exclusions = WorkspaceExclusions::new(root.path(), &[]).unwrap();
    let index = CodeIndex::scan(
        &WorkspaceRoot::open(root.path()).unwrap(),
        1_000_000,
        &[],
        &exclusions,
    )
    .unwrap();
    (root, index)
}

fn search_text(index: &CodeIndex, operation: Value) -> Value {
    let object = operation.as_object().unwrap();
    let PreparedRetrievalOperation::Search(params) =
        prepare_retrieval_operation("search_text", object).unwrap()
    else {
        panic!("search_text is a search operation")
    };
    execute_index_search(index, "main", "snap_test", &params, 100, false).unwrap()
}

fn hits(result: &Value) -> Vec<(String, u64)> {
    result["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| {
            (
                item["path"].as_str().unwrap().to_owned(),
                item["line"].as_u64().unwrap(),
            )
        })
        .collect()
}

#[test]
fn partial_identifier_matches_inside_longer_identifiers() {
    let (_root, index) = indexed(&[
        ("src/lib.rs", "pub fn open_workspace() {}\n"),
        ("src/other.rs", "fn unrelated() {}\n"),
    ]);

    let result = search_text(
        &index,
        json!({"operation": "search_text", "pattern": "workspac", "context_lines": 0}),
    );

    assert_eq!(hits(&result), vec![("src/lib.rs".to_owned(), 1)]);
}

#[test]
fn query_made_of_stop_words_matches_the_text() {
    let (_root, index) = indexed(&[("docs/guide.md", "intro\nAdd the file to the tools list.\n")]);

    let result = search_text(
        &index,
        json!({"operation": "search_text", "pattern": "to the tools", "context_lines": 0}),
    );

    assert_eq!(hits(&result), vec![("docs/guide.md".to_owned(), 2)]);
}

#[test]
fn case_insensitive_query_matches_other_casing_and_case_sensitive_does_not() {
    let (_root, index) = indexed(&[("src/config.rs", "struct HttpClientConfig;\n")]);

    let insensitive = search_text(
        &index,
        json!({"operation": "search_text", "pattern": "httpclientconf", "context_lines": 0}),
    );
    let sensitive = search_text(
        &index,
        json!({
            "operation": "search_text",
            "pattern": "httpclientconf",
            "case_sensitive": true,
            "context_lines": 0
        }),
    );

    assert_eq!(hits(&insensitive), vec![("src/config.rs".to_owned(), 1)]);
    assert_eq!(sensitive["result_count"], 0);
}
