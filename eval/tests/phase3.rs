use codeweave_rust::index::{CodeIndex, WorkspaceExclusions};
use codeweave_rust::retrieval::{
    execute_index_search, prepare_retrieval_operation, PreparedRetrievalOperation,
};
use serde_json::{json, Value};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const FILE_COUNT: usize = 300;
const LINES_PER_FILE: usize = 1_000;
const EXPECTED_SOURCE_LOC: usize = FILE_COUNT * LINES_PER_FILE;
const DEFAULT_P95_GATE_MS: f64 = 250.0;

#[test]
#[ignore = "deterministic 300k-LOC fallback reference performance gate"]
fn fallback_reference_300k_p95_stays_within_gate() {
    let base = unique_temp_dir("codeweave-phase3-300k");
    let root = base.join("repo");
    let source_root = root.join("src");
    std::fs::create_dir_all(&source_root).expect("create scale fixture");

    for file_index in 0..FILE_COUNT {
        let mut content = String::new();
        if file_index == 0 {
            content.push_str("pub fn phase3_target() {}\n");
        } else if file_index + 1 == FILE_COUNT {
            content.push_str("pub fn consume() { phase3_target(); }\n");
        } else {
            content.push_str("// scale fixture\n");
        }
        for line_index in 1..LINES_PER_FILE {
            writeln!(content, "// filler {file_index:03} {line_index:04}")
                .expect("write fixture line");
        }
        std::fs::write(
            source_root.join(format!("file_{file_index:03}.rs")),
            content,
        )
        .expect("write scale fixture file");
    }

    let exclusions = WorkspaceExclusions::new(&root, &[]).expect("scale exclusions");
    let cache_path = base.join("index.json");
    let (mut index, cache_hit) =
        CodeIndex::scan_cached(&root, 2_000_000, &[], &exclusions, &cache_path)
            .expect("scan scale fixture");
    assert!(!cache_hit);
    assert_eq!(index.metrics().indexed_source_loc, EXPECTED_SOURCE_LOC);
    let snapshot = index.snapshot_id("phase3-scale");
    let operation = json!({
        "operation": "find_references",
        "symbol": "phase3_target",
        "definition_path": "src/file_000.rs",
        "max_results": 20
    });

    for _ in 0..3 {
        let response = execute_reference(&index, &snapshot, &operation);
        assert_reference_response(&response);
    }

    let mut timings = Vec::new();
    for _ in 0..20 {
        let started = Instant::now();
        let response = execute_reference(&index, &snapshot, &operation);
        timings.push(started.elapsed());
        assert_reference_response(&response);
    }
    timings.sort();
    let p95 = percentile_duration(&timings, 0.95);
    let p95_ms = p95.as_secs_f64() * 1_000.0;
    let gate_ms = std::env::var("CODEWEAVE_FALLBACK_300K_P95_GATE_MS")
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
        .unwrap_or(DEFAULT_P95_GATE_MS);
    eprintln!(
        "fallback_reference_300k_p95_ms={p95_ms:.3} gate_ms={gate_ms:.3} files={FILE_COUNT} loc={EXPECTED_SOURCE_LOC}"
    );
    assert!(
        p95_ms <= gate_ms,
        "300k fallback reference p95 {p95_ms:.3}ms exceeded gate {gate_ms:.3}ms"
    );

    let _ = std::fs::remove_dir_all(base);
}

fn execute_reference(index: &CodeIndex, snapshot: &str, operation: &Value) -> Value {
    let object = operation.as_object().expect("operation object");
    let kind = object["operation"].as_str().expect("operation name");
    let PreparedRetrievalOperation::Search(params) =
        prepare_retrieval_operation(kind, object).expect("prepare reference operation")
    else {
        panic!("reference operation must prepare as search");
    };
    execute_index_search(index, "eval", snapshot, &params, 100, false)
        .expect("execute reference operation")
}

fn assert_reference_response(response: &Value) {
    assert_eq!(response["backend"], "fallback");
    assert_eq!(response["freshness"], "current");
    assert_eq!(response["scanned_scope"]["file_count"], FILE_COUNT);
    assert_eq!(response["result_count"], 1);
    assert_eq!(response["results"][0]["path"], "src/file_299.rs");
    assert_eq!(response["results"][0]["reference_kind"], "call");
}

fn percentile_duration(values: &[Duration], quantile: f64) -> Duration {
    let rank = (quantile * (values.len() as f64 - 1.0)).round() as usize;
    values[rank.min(values.len() - 1)]
}

fn unique_temp_dir(prefix: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock")
        .as_nanos();
    std::env::temp_dir().join(format!("{prefix}-{}-{nonce}", std::process::id()))
}
