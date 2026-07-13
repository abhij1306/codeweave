use codeweave_rust::index::{CodeIndex, WorkspaceExclusions};
use codeweave_rust::retrieval::{
    execute_index_search, prepare_retrieval_operation, PreparedRetrievalOperation,
};
use serde::Deserialize;
use serde_json::Value;
use std::path::PathBuf;

#[derive(Debug, Deserialize)]
struct MalformedFixtureSet {
    cases: Vec<MalformedCase>,
}

#[derive(Debug, Deserialize)]
struct MalformedCase {
    id: String,
    operation: Value,
    expected_code: String,
}

#[derive(Debug, Deserialize)]
struct ReferenceFixture {
    operation: Value,
    expected_references: Vec<ExpectedReference>,
    baseline_status: String,
}

#[derive(Debug, Deserialize)]
struct ExpectedReference {
    path: String,
    line: usize,
    kind: String,
    evidence: String,
    enclosing_symbol: String,
}

fn eval_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn public_tool_schema_snapshot_matches() {
    let snapshot_path = eval_root().join("fixtures/tool-schemas.json");
    let actual = format!(
        "{}\n",
        serde_json::to_string_pretty(&codeweave_rust::tools::full_list_payload())
            .expect("serialize public tool schemas")
    );

    if std::env::var_os("UPDATE_EVAL_SNAPSHOTS").is_some() {
        if let Some(parent) = snapshot_path.parent() {
            std::fs::create_dir_all(parent).expect("create fixture directory");
        }
        std::fs::write(&snapshot_path, actual).expect("write tool schema snapshot");
        return;
    }

    let expected = std::fs::read_to_string(&snapshot_path).unwrap_or_else(|error| {
        panic!(
            "reading {}: {error}; regenerate with UPDATE_EVAL_SNAPSHOTS=1 cargo test -p eval --test phase0 public_tool_schema_snapshot_matches",
            snapshot_path.display()
        )
    });
    assert_eq!(actual, expected, "public tool schema changed");
}

#[test]
fn malformed_operation_fixtures_match_dispatcher_errors() {
    let path = eval_root().join("fixtures/malformed-code-retrieve.json");
    let fixtures: MalformedFixtureSet =
        serde_json::from_slice(&std::fs::read(&path).expect("read malformed fixtures"))
            .expect("parse malformed fixtures");

    for fixture in fixtures.cases {
        let object = fixture
            .operation
            .as_object()
            .unwrap_or_else(|| panic!("{} operation must be an object", fixture.id));
        let kind = object
            .get("operation")
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("{} requires operation", fixture.id));
        let error = match prepare_retrieval_operation(kind, object) {
            Ok(_) => panic!("{} unexpectedly succeeded", fixture.id),
            Err(error) => error,
        };
        assert_eq!(error.0.code, fixture.expected_code, "{}", fixture.id);
    }
}

#[test]
fn receiver_qualified_reference_full_scan_is_complete() {
    let root = eval_root().join("fixtures/references/receiver-qualified-rust");
    let fixture: ReferenceFixture = serde_json::from_slice(
        &std::fs::read(root.join("expected.json")).expect("read reference fixture"),
    )
    .expect("parse reference fixture");
    assert_eq!(fixture.baseline_status, "expected_pass");
    assert!(!fixture.expected_references.is_empty());
    for expected in &fixture.expected_references {
        assert!(root.join(&expected.path).is_file());
        assert!(expected.line > 0);
        assert_eq!(expected.kind, "call");
        assert_eq!(expected.evidence, "syntactic");
        assert!(!expected.enclosing_symbol.is_empty());
    }

    let exclusions =
        WorkspaceExclusions::new(&root, &["expected.json".to_owned()]).expect("fixture exclusions");
    let cache_path = std::env::temp_dir().join(format!(
        "codeweave-reference-fixture-{}-{}.json",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos()
    ));
    let (_, first_cache_hit) =
        CodeIndex::scan_cached(&root, 2_000_000, &[], &exclusions, &cache_path)
            .expect("write fixture cache");
    assert!(!first_cache_hit);

    // Model the incomplete natural-language posting that previously caused the
    // receiver-qualified call to be omitted before the exact identifier scan.
    let mut cached: Value =
        serde_json::from_slice(&std::fs::read(&cache_path).expect("read fixture cache"))
            .expect("parse fixture cache");
    let consumer = cached["files"]
        .as_array_mut()
        .expect("cached files")
        .iter_mut()
        .find(|file| file["path"] == "src/consumer.rs")
        .expect("consumer cache entry");
    let terms = consumer["indexed_terms"]
        .as_array_mut()
        .expect("consumer indexed terms");
    terms.retain(|term| term.as_str() != Some("run_edit_validation"));
    assert!(!terms
        .iter()
        .any(|term| term.as_str() == Some("run_edit_validation")));
    std::fs::write(
        &cache_path,
        serde_json::to_vec(&cached).expect("serialize fixture cache"),
    )
    .expect("rewrite fixture cache");

    let (mut index, second_cache_hit) =
        CodeIndex::scan_cached(&root, 2_000_000, &[], &exclusions, &cache_path)
            .expect("load fixture cache");
    assert!(second_cache_hit);
    let _ = std::fs::remove_file(&cache_path);
    let snapshot = index.snapshot_id("fixture");
    let operation = fixture.operation.as_object().expect("operation object");
    let kind = operation["operation"].as_str().expect("operation name");
    let PreparedRetrievalOperation::Search(params) =
        prepare_retrieval_operation(kind, operation).expect("prepare fixture")
    else {
        panic!("reference fixture must prepare as an index search");
    };
    let response = execute_index_search(&index, "eval", &snapshot, &params, 100, false)
        .expect("execute reference fixture");

    assert_eq!(response["backend"], "fallback");
    assert_eq!(response["freshness"], "current");
    assert_eq!(response["target_evidence"], "syntactic");
    assert_eq!(response["target"]["path"], "src/validator.rs");
    assert_eq!(response["scanned_scope"]["file_count"], 3);
    assert!(response["scanned_scope"]["byte_count"].as_u64().unwrap() > 0);
    assert_eq!(response["result_count"], fixture.expected_references.len());

    for expected in fixture.expected_references {
        let result = response["results"]
            .as_array()
            .expect("reference results")
            .iter()
            .find(|result| result["path"] == expected.path && result["line"] == expected.line)
            .unwrap_or_else(|| {
                panic!(
                    "missing exact reference {}:{}",
                    expected.path, expected.line
                )
            });
        assert_eq!(result["reference_kind"], expected.kind);
        assert_eq!(result["classification_evidence"], expected.evidence);
        assert_eq!(result["enclosing_symbol"], expected.enclosing_symbol);
        assert_eq!(
            result["occurrences"][0]["range"]["start"]["line"],
            expected.line
        );
        assert_eq!(result["occurrences"][0]["evidence"], "syntactic");
    }
    assert!(!response.to_string().contains("\"semantic\""));
}
