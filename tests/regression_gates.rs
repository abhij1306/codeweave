use codeweave_rust::index::{CodeIndex, WorkspaceExclusions};
use codeweave_rust::retrieval::{
    execute_index_search, prepare_retrieval_operation, PreparedRetrievalOperation,
};
use serde_json::{json, Value};
use std::fmt::Write;
use std::time::{Duration, Instant};

fn execute(index: &CodeIndex, snapshot: &str, operation: &Value) -> Value {
    let object = operation.as_object().unwrap();
    let PreparedRetrievalOperation::Search(params) =
        prepare_retrieval_operation(operation["operation"].as_str().unwrap(), object).unwrap()
    else {
        panic!("expected a search operation")
    };
    execute_index_search(index, "gate", snapshot, &params, 100, false).unwrap()
}

#[test]
fn malformed_retrieval_calls_are_rejected_deterministically() {
    for (operation, code) in [
        (
            json!({"operation":"find_references"}),
            "MISSING_OPERATION_FIELD",
        ),
        (
            json!({"operation":"search_text","pattern":"x","syntax":"glob"}),
            "INVALID_OPERATION_FIELD",
        ),
        (
            json!({"operation":"unknown"}),
            "UNSUPPORTED_RETRIEVAL_OPERATION",
        ),
    ] {
        let object = operation.as_object().unwrap();
        let error = prepare_retrieval_operation(operation["operation"].as_str().unwrap(), object)
            .unwrap_err();
        assert_eq!(error.0.code, code);
    }
}

#[test]
fn receiver_qualified_reference_scan_has_complete_recall() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("src")).unwrap();
    std::fs::write(
        root.path().join("src/validator.rs"),
        "pub struct Validator; impl Validator { pub fn run_edit_validation(&self) {} }\n",
    )
    .unwrap();
    std::fs::write(
        root.path().join("src/consumer.rs"),
        "fn use_it(v: Validator) { v.run_edit_validation(); }\n",
    )
    .unwrap();
    std::fs::write(
        root.path().join("src/lib.rs"),
        "mod validator; mod consumer;\n",
    )
    .unwrap();
    let exclusions = WorkspaceExclusions::new(root.path(), &[]).unwrap();
    let mut index = CodeIndex::scan(root.path(), 2_000_000, &[], &exclusions).unwrap();
    let snapshot = index.snapshot_id("gate");
    let response = execute(
        &index,
        &snapshot,
        &json!({
            "operation":"find_references", "symbol":"run_edit_validation",
            "definition_path":"src/validator.rs", "max_results":20
        }),
    );
    assert_eq!(response["result_count"], 1);
    assert_eq!(response["results"][0]["path"], "src/consumer.rs");
    assert_eq!(response["results"][0]["reference_kind"], "call");
}

#[test]
fn fallback_reference_300k_returns_one_reference_and_meets_opt_in_timing_gate() {
    const FILES: usize = 300;
    const LINES: usize = 1_000;
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("src")).unwrap();
    for file in 0..FILES {
        let mut content = if file == 0 {
            "pub fn scale_target() {}\n".to_owned()
        } else if file == FILES - 1 {
            "fn consume() { scale_target(); }\n".to_owned()
        } else {
            "// scale\n".to_owned()
        };
        for line in 1..LINES {
            writeln!(content, "// filler {file:03} {line:04}").unwrap();
        }
        std::fs::write(root.path().join(format!("src/file_{file:03}.rs")), content).unwrap();
    }
    let exclusions = WorkspaceExclusions::new(root.path(), &[]).unwrap();
    let mut index = CodeIndex::scan(root.path(), 2_000_000, &[], &exclusions).unwrap();
    assert_eq!(index.metrics().indexed_source_loc, FILES * LINES);
    let snapshot = index.snapshot_id("scale");
    let operation = json!({"operation":"find_references","symbol":"scale_target","definition_path":"src/file_000.rs","max_results":20});
    let mut timings = Vec::new();
    for _ in 0..3 {
        assert_eq!(execute(&index, &snapshot, &operation)["result_count"], 1);
    }
    for _ in 0..20 {
        let start = Instant::now();
        assert_eq!(execute(&index, &snapshot, &operation)["result_count"], 1);
        timings.push(start.elapsed());
    }
    timings.sort();
    let p95: Duration = timings[((timings.len() - 1) as f64 * 0.95).round() as usize];
    if std::env::var_os("CODEWEAVE_ENFORCE_PERF_GATES").is_some() {
        let limit_ms = std::env::var("CODEWEAVE_FALLBACK_P95_LIMIT_MS")
            .ok()
            .and_then(|value| value.parse::<u128>().ok())
            .unwrap_or(250);
        assert!(
            p95.as_millis() <= limit_ms,
            "fallback p95 was {p95:?}; configured limit was {limit_ms} ms"
        );
    }
}
