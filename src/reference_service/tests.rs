use super::*;
use crate::error::ErrorCode;
use crate::index::WorkspaceExclusions;
use crate::security::WorkspaceRoot;
use serde_json::Value;

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

fn query(symbol: &str) -> ReferenceQuery {
    ReferenceQuery {
        symbol: symbol.to_owned(),
        scope: ReferenceScope::All,
        kinds: Vec::new(),
        definition_path: None,
        definition_line: None,
        context_lines: 0,
    }
}

fn references(index: &CodeIndex, max_results: usize, query: &ReferenceQuery) -> AppResult<Value> {
    ReferenceService::new(index).fallback(
        &SearchScope {
            workspace_id: "main",
            snapshot_id: "snap_test",
            paths: &[],
            max_results,
        },
        query,
    )
}

fn result_paths(result: &Value) -> Vec<&str> {
    result["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["path"].as_str().unwrap())
        .collect()
}

#[test]
fn reference_search_is_explicit_and_excludes_the_declaration() {
    let (_root, index) = indexed(&[
        ("src/workspace.rs", "pub fn open_workspace() {}\n"),
        ("src/main.rs", "fn main() { open_workspace(); }\n"),
    ]);

    let result = references(
        &index,
        10,
        &ReferenceQuery {
            context_lines: 1,
            ..query("open_workspace")
        },
    )
    .unwrap();

    assert_eq!(result["mode"], "references");
    assert_eq!(result["definitions"][0]["path"], "src/workspace.rs");
    assert_eq!(result_paths(&result), ["src/main.rs"]);
}

#[test]
fn reference_search_classifies_receiver_qualified_calls_syntactically() {
    let (_root, index) = indexed(&[
        (
            "src/owner.rs",
            "pub struct Validator;\nimpl Validator { pub fn run_edit_validation(&self) {} }\n",
        ),
        (
            "src/caller.rs",
            "impl Validator {\n    fn apply(&self) {\n        self.run_edit_validation();\n    }\n}\n",
        ),
    ]);

    let result = references(
        &index,
        10,
        &ReferenceQuery {
            definition_path: Some("src/owner.rs".to_owned()),
            definition_line: Some(2),
            ..query("run_edit_validation")
        },
    )
    .unwrap();

    assert_eq!(result["backend"], "fallback");
    assert_eq!(result["freshness"], "current");
    assert_eq!(result["target_evidence"], "syntactic");
    assert_eq!(result["target"]["path"], "src/owner.rs");
    assert_eq!(result["scanned_scope"]["file_count"], 2);
    assert_eq!(result["result_count"], 1);
    assert_eq!(result["results"][0]["path"], "src/caller.rs");
    assert_eq!(result["results"][0]["line"], 3);
    assert_eq!(result["results"][0]["reference_kind"], "call");
    assert_eq!(result["results"][0]["classification_evidence"], "syntactic");
    assert_eq!(result["results"][0]["enclosing_symbol"], "apply");
    assert_eq!(
        result["results"][0]["occurrences"][0]["range"]["start"]["line"],
        3
    );
}

#[test]
fn reference_search_finds_usages_for_single_character_symbols() {
    let (_root, index) = indexed(&[
        ("src/owner.rs", "pub fn x() {}\n"),
        ("src/caller.rs", "fn call() { x(); }\n"),
    ]);

    let result = references(&index, 10, &query("x")).unwrap();

    assert_eq!(result["definitions"][0]["path"], "src/owner.rs");
    assert_eq!(result_paths(&result), ["src/caller.rs"]);
}

#[test]
fn reference_search_distributes_results_across_files() {
    let noisy = "process_run();\n".repeat(12);
    let (_root, index) = indexed(&[
        ("src/owner.rs", "pub fn process_run() {}\n"),
        ("tests/noisy.rs", noisy.as_str()),
        ("src/caller.rs", "fn call() { process_run(); }\n"),
    ]);

    let result = references(&index, 8, &query("process_run")).unwrap();

    let paths = result_paths(&result);
    assert_eq!(
        paths
            .iter()
            .filter(|path| **path == "tests/noisy.rs")
            .count(),
        3
    );
    assert!(paths.contains(&"src/caller.rs"));
    assert_eq!(result["truncated"], true);
}

#[test]
fn reference_search_merges_adjacent_matches_into_bounded_windows() {
    let (_root, index) = indexed(&[
        ("src/owner.rs", "pub fn process_run() {}\n"),
        (
            "src/caller.rs",
            "fn call() {\n    process_run();\n    process_run();\n    process_run();\n}\n",
        ),
    ]);

    let result = references(
        &index,
        10,
        &ReferenceQuery {
            context_lines: 1,
            ..query("process_run")
        },
    )
    .unwrap();

    assert_eq!(result_paths(&result), ["src/caller.rs"]);
    assert_eq!(result["results"][0]["start_line"], 1);
    assert_eq!(result["results"][0]["end_line"], 5);
    assert_eq!(
        result["results"][0]["occurrences"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    assert_eq!(result["truncated"], false);
}

#[test]
fn lexical_reference_search_prioritizes_calls_over_imports() {
    let (_root, index) = indexed(&[
        ("src/owner.rs", "pub fn extract() {}\n"),
        ("src/a_import.rs", "use crate::owner::extract;\n"),
        ("src/z_call.rs", "fn run() { extract(); }\n"),
    ]);

    let result = references(&index, 10, &query("extract")).unwrap();

    assert_eq!(result_paths(&result), ["src/z_call.rs", "src/a_import.rs"]);
    assert_eq!(result["results"][0]["reference_kind"], "call");
    assert_eq!(result["results"][0]["score"], 1.0);
    assert_eq!(result["results"][1]["reference_kind"], "import");
    assert_eq!(result["results"][1]["score"], 0.75);
}

#[test]
fn reference_kinds_filter_occurrences_and_can_add_the_declaration() {
    let (_root, index) = indexed(&[
        ("src/owner.rs", "pub fn extract() {}\n"),
        ("src/a_import.rs", "use crate::owner::extract;\n"),
        ("src/z_call.rs", "fn run() { extract(); }\n"),
    ]);

    let result = references(
        &index,
        10,
        &ReferenceQuery {
            kinds: vec![ReferenceKind::Declaration, ReferenceKind::Call],
            ..query("extract")
        },
    )
    .unwrap();

    assert_eq!(result_paths(&result), ["src/owner.rs", "src/z_call.rs"]);
    assert_eq!(result["results"][0]["match_type"], "declaration");
    assert_eq!(
        result["reference_kinds"],
        serde_json::json!(["declaration", "call"])
    );
}

#[test]
fn production_scope_skips_tests_and_docs() {
    let (_root, index) = indexed(&[
        ("src/owner.rs", "pub fn extract() {}\n"),
        ("src/use.rs", "fn run() { extract(); }\n"),
        ("tests/owner_test.rs", "fn check() { extract(); }\n"),
        ("docs/example.rs", "fn doc() { extract(); }\n"),
    ]);

    let production = references(
        &index,
        10,
        &ReferenceQuery {
            scope: ReferenceScope::Production,
            ..query("extract")
        },
    )
    .unwrap();
    let tests = references(
        &index,
        10,
        &ReferenceQuery {
            scope: ReferenceScope::Tests,
            ..query("extract")
        },
    )
    .unwrap();

    assert_eq!(result_paths(&production), ["src/use.rs"]);
    assert_eq!(production["reference_scope"], "production");
    assert_eq!(result_paths(&tests), ["tests/owner_test.rs"]);
}

#[test]
fn ambiguous_selector_lists_candidates_until_a_definition_is_chosen() {
    let (_root, index) = indexed(&[
        ("src/a.rs", "pub fn run() {}\n"),
        ("src/b.rs", "pub fn run() {}\n"),
    ]);

    let error = references(&index, 10, &query("run")).unwrap_err();
    let chosen = references(
        &index,
        10,
        &ReferenceQuery {
            definition_path: Some("src/b.rs".to_owned()),
            ..query("run")
        },
    )
    .unwrap();

    assert_eq!(error.code(), ErrorCode::AmbiguousSymbol);
    assert_eq!(
        error.details().unwrap()["candidates"][1]["path"],
        "src/b.rs"
    );
    assert_eq!(chosen["definitions"][0]["path"], "src/b.rs");
}

#[test]
fn reference_kind_wire_names_round_trip() {
    for kind in ReferenceKind::ALL {
        assert_eq!(serde_json::to_value(kind).unwrap(), kind.as_str());
        assert_eq!(ReferenceKind::parse(kind.as_str()), Some(kind));
    }
    assert_eq!(ReferenceKind::parse("calls"), None);
}
