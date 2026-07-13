//! Production-path retrieval evaluator.
//!
//! Every indexed operation is parsed by `prepare_retrieval_operation` and
//! executed by `execute_index_search`, the same functions used by the MCP
//! `code_retrieve` route. This keeps evaluation and production semantics joined.

use codeweave_rust::index::{CodeIndex, IndexMetrics, WorkspaceExclusions};
use codeweave_rust::retrieval::{
    execute_index_search, prepare_retrieval_operation, PreparedRetrievalOperation,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const MAX_FILE_BYTES: usize = 2_000_000;
const MAX_SEARCH_RESULTS: usize = 100;

#[derive(Debug, Deserialize)]
struct FixtureSet {
    repo: String,
    #[allow(dead_code)]
    note: String,
    cases: Vec<FixtureCase>,
}

#[derive(Debug, Deserialize)]
struct FixtureCase {
    id: String,
    category: String,
    operation: Value,
    #[serde(default)]
    expected_paths: Vec<String>,
    #[serde(default)]
    known_failure: bool,
}

#[derive(Debug, Serialize)]
struct CaseResult {
    id: String,
    category: String,
    operation: String,
    known_failure: bool,
    status: String,
    error_code: Option<String>,
    hit_rank: Option<usize>,
    recall_at_1: bool,
    recall_at_5: bool,
    recall_at_10: bool,
    result_count: usize,
    latency_ms: f64,
    top_paths: Vec<String>,
}

#[derive(Debug, Serialize)]
struct LiveBaseline {
    evaluation_contract: &'static str,
    adapter: &'static str,
    repo: String,
    revision: Option<String>,
    dirty_worktree: bool,
    fixture_count: usize,
    quality_fixture_count: usize,
    known_failure_count: usize,
    known_failure_miss_count: usize,
    recall_at_1: f64,
    recall_at_5: f64,
    recall_at_10: f64,
    mrr_at_10: f64,
    cold_index_ms: f64,
    warm_index_ms: f64,
    cold_cache_hit: bool,
    warm_cache_hit: bool,
    operation_p50_ms: f64,
    operation_p95_ms: f64,
    fallback_reference_p50_ms: f64,
    fallback_reference_p95_ms: f64,
    index: IndexMetrics,
    per_case: Vec<CaseResult>,
}

#[derive(Debug)]
struct Args {
    repo: String,
    repo_path: Option<PathBuf>,
}

pub fn run() {
    let args = parse_args();
    let codeweave_root = codeweave_root();
    let fixture_path = codeweave_root
        .join("eval/operations")
        .join(format!("{}.json", args.repo));
    let fixture_set: FixtureSet = serde_json::from_slice(
        &std::fs::read(&fixture_path)
            .unwrap_or_else(|error| panic!("reading {}: {error}", fixture_path.display())),
    )
    .unwrap_or_else(|error| panic!("parsing {}: {error}", fixture_path.display()));
    assert_eq!(fixture_set.repo, args.repo, "fixture repo mismatch");

    let target_root = resolve_target_root(&codeweave_root, &args);
    validate_expected_paths(&target_root, &fixture_set);
    let (revision, dirty_worktree) = git_state(&target_root);
    let exclusions = WorkspaceExclusions::new(&target_root, &[]).expect("workspace exclusions");
    let cache_path = temporary_cache_path(&args.repo);

    let cold_started = Instant::now();
    let (mut index, cold_cache_hit) =
        CodeIndex::scan_cached(&target_root, MAX_FILE_BYTES, &[], &exclusions, &cache_path)
            .expect("cold index scan");
    let cold_index_ms = cold_started.elapsed().as_secs_f64() * 1_000.0;

    let warm_started = Instant::now();
    let (_warm_index, warm_cache_hit) =
        CodeIndex::scan_cached(&target_root, MAX_FILE_BYTES, &[], &exclusions, &cache_path)
            .expect("warm cached index scan");
    let warm_index_ms = warm_started.elapsed().as_secs_f64() * 1_000.0;
    let _ = std::fs::remove_file(&cache_path);

    let snapshot_id = index.snapshot_id(revision.as_deref().unwrap_or("eval"));
    let index_metrics = index.metrics();
    let mut per_case = Vec::new();
    let mut operation_times = Vec::new();

    for fixture in &fixture_set.cases {
        let started = Instant::now();
        let result = execute_fixture(&index, &snapshot_id, fixture);
        let latency_ms = started.elapsed().as_secs_f64() * 1_000.0;
        operation_times.push(latency_ms);

        match result {
            Ok(response) => {
                let paths = response_paths(&response);
                let expected: HashSet<&str> =
                    fixture.expected_paths.iter().map(String::as_str).collect();
                let hit_rank = paths
                    .iter()
                    .position(|path| expected.contains(path.as_str()))
                    .map(|index| index + 1);
                let status = if fixture.expected_paths.is_empty() || hit_rank.is_some() {
                    "pass"
                } else if fixture.known_failure {
                    "known_failure"
                } else {
                    "miss"
                };
                per_case.push(CaseResult {
                    id: fixture.id.clone(),
                    category: fixture.category.clone(),
                    operation: operation_name(fixture),
                    known_failure: fixture.known_failure,
                    status: status.to_owned(),
                    error_code: None,
                    hit_rank,
                    recall_at_1: hit_rank.is_some_and(|rank| rank <= 1),
                    recall_at_5: hit_rank.is_some_and(|rank| rank <= 5),
                    recall_at_10: hit_rank.is_some_and(|rank| rank <= 10),
                    result_count: paths.len(),
                    latency_ms,
                    top_paths: paths.into_iter().take(10).collect(),
                });
            }
            Err(error) => per_case.push(CaseResult {
                id: fixture.id.clone(),
                category: fixture.category.clone(),
                operation: operation_name(fixture),
                known_failure: fixture.known_failure,
                status: if fixture.known_failure {
                    "known_failure".to_owned()
                } else {
                    "error".to_owned()
                },
                error_code: Some(error.0.code),
                hit_rank: None,
                recall_at_1: false,
                recall_at_5: false,
                recall_at_10: false,
                result_count: 0,
                latency_ms,
                top_paths: Vec::new(),
            }),
        }
    }

    let quality: Vec<&CaseResult> = per_case
        .iter()
        .zip(&fixture_set.cases)
        .filter_map(|(result, fixture)| (!fixture.expected_paths.is_empty()).then_some(result))
        .collect();
    let reference_fixtures = fixture_set
        .cases
        .iter()
        .filter(|fixture| operation_name(fixture) == "find_references")
        .collect::<Vec<_>>();
    for fixture in &reference_fixtures {
        execute_fixture(&index, &snapshot_id, fixture)
            .unwrap_or_else(|error| panic!("warming {}: {error}", fixture.id));
    }
    let mut fallback_reference_times = Vec::new();
    for _ in 0..10 {
        for fixture in &reference_fixtures {
            let started = Instant::now();
            execute_fixture(&index, &snapshot_id, fixture)
                .unwrap_or_else(|error| panic!("measuring {}: {error}", fixture.id));
            fallback_reference_times.push(started.elapsed().as_secs_f64() * 1_000.0);
        }
    }
    let baseline = LiveBaseline {
        evaluation_contract: "code_retrieve-live-v1",
        adapter: "prepare_retrieval_operation+execute_index_search",
        repo: fixture_set.repo,
        revision,
        dirty_worktree,
        fixture_count: per_case.len(),
        quality_fixture_count: quality.len(),
        known_failure_count: per_case.iter().filter(|case| case.known_failure).count(),
        known_failure_miss_count: per_case
            .iter()
            .filter(|case| case.known_failure && case.hit_rank.is_none())
            .count(),
        recall_at_1: fraction(&quality, |case| case.recall_at_1),
        recall_at_5: fraction(&quality, |case| case.recall_at_5),
        recall_at_10: fraction(&quality, |case| case.recall_at_10),
        mrr_at_10: mean_reciprocal_rank(&quality),
        cold_index_ms,
        warm_index_ms,
        cold_cache_hit,
        warm_cache_hit,
        operation_p50_ms: percentile(&operation_times, 0.50),
        operation_p95_ms: percentile(&operation_times, 0.95),
        fallback_reference_p50_ms: percentile(&fallback_reference_times, 0.50),
        fallback_reference_p95_ms: percentile(&fallback_reference_times, 0.95),
        index: index_metrics,
        per_case,
    };

    print_summary(&baseline);
    let out_dir = codeweave_root.join("eval/baseline/live");
    std::fs::create_dir_all(&out_dir).expect("live baseline directory");
    let out_path = out_dir.join(format!("{}.json", args.repo));
    std::fs::write(
        &out_path,
        serde_json::to_string_pretty(&baseline).expect("serialize live baseline"),
    )
    .expect("write live baseline");
    println!("\nwrote {}", out_path.display());
}

fn execute_fixture(
    index: &CodeIndex,
    snapshot_id: &str,
    fixture: &FixtureCase,
) -> codeweave_rust::model::AppResult<Value> {
    let operation = fixture.operation.as_object().ok_or_else(|| {
        codeweave_rust::model::AppError::invalid("fixture operation must be an object")
    })?;
    let kind = operation
        .get("operation")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            codeweave_rust::model::AppError::invalid("fixture operation requires operation")
        })?;
    match prepare_retrieval_operation(kind, operation)? {
        PreparedRetrievalOperation::Search(params) => execute_index_search(
            index,
            "eval",
            snapshot_id,
            &params,
            MAX_SEARCH_RESULTS,
            false,
        ),
        PreparedRetrievalOperation::Read(_) => Err(codeweave_rust::model::AppError::details(
            "EVAL_READ_REQUIRES_WORKSPACE",
            "read operations require WorkspaceActor state and are covered by server integration tests",
            serde_json::json!({"fixture": fixture.id}),
        )),
    }
}

fn operation_name(fixture: &FixtureCase) -> String {
    fixture
        .operation
        .get("operation")
        .and_then(Value::as_str)
        .unwrap_or("invalid")
        .to_owned()
}

fn response_paths(response: &Value) -> Vec<String> {
    fn visit(value: &Value, paths: &mut Vec<String>) {
        match value {
            Value::Object(object) => {
                if let Some(path) = object.get("path").and_then(Value::as_str) {
                    if !paths.iter().any(|known| known == path) {
                        paths.push(path.to_owned());
                    }
                }
                for key in ["result", "results", "directories"] {
                    if let Some(child) = object.get(key) {
                        visit(child, paths);
                    }
                }
            }
            Value::Array(values) => {
                for value in values {
                    visit(value, paths);
                }
            }
            _ => {}
        }
    }

    let mut paths = Vec::new();
    visit(response, &mut paths);
    paths
}

fn parse_args() -> Args {
    let mut parsed = Args {
        repo: "codeweave".to_owned(),
        repo_path: None,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--repo" => parsed.repo = args.next().expect("--repo requires a name"),
            value if value.starts_with("--repo=") => {
                parsed.repo = value["--repo=".len()..].to_owned();
            }
            "--repo-path" => {
                parsed.repo_path = Some(PathBuf::from(
                    args.next().expect("--repo-path requires a directory"),
                ));
            }
            value if value.starts_with("--repo-path=") => {
                parsed.repo_path = Some(PathBuf::from(&value["--repo-path=".len()..]));
            }
            "--help" | "-h" => {
                println!("Usage: cargo run -p eval -- [--repo <name>] [--repo-path <path>]");
                std::process::exit(0);
            }
            other => panic!("unknown evaluator argument '{other}'"),
        }
    }
    parsed
}

fn codeweave_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("eval crate has a parent")
        .to_path_buf()
}

fn resolve_target_root(codeweave_root: &Path, args: &Args) -> PathBuf {
    let configured = args.repo_path.clone().or_else(|| {
        (args.repo == "crawlerai")
            .then(|| std::env::var_os("CRAWLERAI_REPO").map(PathBuf::from))
            .flatten()
    });
    let path = configured.unwrap_or_else(|| {
        if args.repo == "codeweave" {
            codeweave_root.to_path_buf()
        } else if args.repo == "crawlerai" {
            codeweave_root
                .parent()
                .expect("CodeWeave repository has a parent")
                .join("CrawlerAI")
        } else {
            panic!("--repo-path is required for repo '{}'", args.repo);
        }
    });
    path.canonicalize()
        .unwrap_or_else(|error| panic!("opening {}: {error}", path.display()))
}

fn validate_expected_paths(root: &Path, fixtures: &FixtureSet) {
    for fixture in &fixtures.cases {
        for expected in &fixture.expected_paths {
            assert!(
                root.join(expected).is_file(),
                "{} expected path does not exist: {}",
                fixture.id,
                root.join(expected).display()
            );
        }
    }
}

fn temporary_cache_path(repo: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "codeweave-live-eval-{repo}-{}-{nonce}.json",
        std::process::id()
    ))
}

fn git_state(root: &Path) -> (Option<String>, bool) {
    let output = |args: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .ok()
            .filter(|output| output.status.success())
    };
    let revision = output(&["rev-parse", "HEAD"])
        .map(|value| String::from_utf8_lossy(&value.stdout).trim().to_owned());
    let dirty = output(&["status", "--porcelain"]).is_some_and(|value| !value.stdout.is_empty());
    (revision, dirty)
}

fn fraction(results: &[&CaseResult], predicate: impl Fn(&CaseResult) -> bool) -> f64 {
    if results.is_empty() {
        return 0.0;
    }
    results.iter().filter(|result| predicate(result)).count() as f64 / results.len() as f64
}

fn mean_reciprocal_rank(results: &[&CaseResult]) -> f64 {
    if results.is_empty() {
        return 0.0;
    }
    results
        .iter()
        .map(|result| {
            result
                .hit_rank
                .filter(|rank| *rank <= 10)
                .map_or(0.0, |rank| 1.0 / rank as f64)
        })
        .sum::<f64>()
        / results.len() as f64
}

fn percentile(values: &[f64], quantile: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let rank = (quantile * (sorted.len() as f64 - 1.0)).round() as usize;
    sorted[rank.min(sorted.len() - 1)]
}

fn print_summary(baseline: &LiveBaseline) {
    println!("CodeWeave live retrieval baseline");
    println!(
        "repo={} fixtures={} dirty={}",
        baseline.repo, baseline.fixture_count, baseline.dirty_worktree
    );
    println!("{:-<68}", "");
    println!("{:<28}{:>12}", "Recall@1", format_pct(baseline.recall_at_1));
    println!("{:<28}{:>12}", "Recall@5", format_pct(baseline.recall_at_5));
    println!(
        "{:<28}{:>12}",
        "Recall@10",
        format_pct(baseline.recall_at_10)
    );
    println!("{:<28}{:>12.3}", "MRR@10", baseline.mrr_at_10);
    println!("{:<28}{:>12.1}", "Cold index ms", baseline.cold_index_ms);
    println!("{:<28}{:>12.1}", "Warm cache ms", baseline.warm_index_ms);
    println!(
        "{:<28}{:>12.3}",
        "Fallback ref p50 ms", baseline.fallback_reference_p50_ms
    );
    println!(
        "{:<28}{:>12.3}",
        "Fallback ref p95 ms", baseline.fallback_reference_p95_ms
    );
    println!(
        "{:<28}{:>12}",
        "Indexed source LOC", baseline.index.indexed_source_loc
    );
    println!(
        "{:<28}{:>12}",
        "Index heap floor bytes", baseline.index.estimated_heap_bytes_lower_bound
    );
    println!("{:-<68}", "");
    println!("{:<32}{:<18}{:>8}", "fixture", "status", "ms");
    for case in &baseline.per_case {
        println!(
            "{:<32}{:<18}{:>8.3}",
            truncate(&case.id, 31),
            case.status,
            case.latency_ms
        );
    }
}

fn format_pct(value: f64) -> String {
    format!("{:.1}%", value * 100.0)
}

fn truncate(value: &str, max: usize) -> String {
    if value.len() <= max {
        value.to_owned()
    } else {
        format!("{}…", &value[..max.saturating_sub(1)])
    }
}
