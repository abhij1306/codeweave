//! Offline retrieval + latency benchmark for CodeWeave.
//!
//! Deliberately minimal (per the maintainer's P3 decision): it measures the
//! *real* retrieval engine against CodeWeave or the local CrawlerAI checkout.
//! Its single job is to produce trustworthy relative baselines so ranking
//! changes can be compared across both a Rust tool and a larger Python/TS app.
//!
//! Run:  cargo run -p eval -- --repo crawlerai --ranking v1
//! Out:  prints a table; writes eval/baseline/crawlerai/<ranking>.json
//!
//! An in-process score cannot reproduce how ChatGPT/Claude web clients drive the
//! tools, so treat these numbers as a *relative* regression gate for ranking
//! changes, not an absolute measure of end-to-end agent quality.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use codeweave_rust::index::{CodeIndex, ContextParams, Ranking, SymbolDetail, WorkspaceExclusions};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Deserialize)]
struct QuerySet {
    repo: String,
    #[serde(default)]
    revision: Option<String>,
    #[allow(dead_code)]
    note: String,
    queries: Vec<Query>,
}

#[derive(Deserialize)]
struct Query {
    id: String,
    category: String,
    query: String,
    expected: Vec<String>,
    #[serde(default)]
    dirty: Vec<String>,
    #[serde(default)]
    recent_mutations: Vec<String>,
}

#[derive(Serialize)]
struct QueryResult {
    id: String,
    category: String,
    hit_rank: Option<usize>,
    recall_at_1: bool,
    recall_at_5: bool,
    recall_at_10: bool,
    result_count: usize,
    chars: usize,
    search_ms: f64,
    complete_symbols: usize,
    total_results: usize,
    top_paths: Vec<String>,
}

#[derive(Serialize)]
struct Baseline {
    ranking: String,
    repo: String,
    revision: Option<String>,
    dirty_worktree: bool,
    query_count: usize,
    // Aggregate retrieval quality.
    recall_at_1: f64,
    recall_at_5: f64,
    recall_at_10: f64,
    mrr_at_10: f64,
    mean_chars: f64,
    /// Fraction of returned results that span a complete symbol (v2 only; 0.0
    /// under v1, which does not emit `complete_symbol`).
    complete_symbol_rate: f64,
    // Latency (milliseconds).
    cold_index_ms: f64,
    warm_index_ms: f64,
    search_p50_ms: f64,
    search_p95_ms: f64,
    per_query: Vec<QueryResult>,
}

const BUDGET_CHARS: usize = 50_000;
const MAX_RESULTS: usize = 10;
const MAX_FILE_BYTES: usize = 2_000_000;

struct Args {
    ranking: String,
    repo: String,
    repo_path: Option<PathBuf>,
}

fn main() {
    let args = parse_args();
    let ranking_mode = match args.ranking.as_str() {
        "v1" => Ranking::V1,
        "v2" => Ranking::V2,
        other => {
            eprintln!("unknown ranking '{other}' (expected 'v1' or 'v2').");
            std::process::exit(2);
        }
    };

    let codeweave_root = codeweave_root();
    let query_path = codeweave_root
        .join("eval/queries")
        .join(format!("{}.json", args.repo));
    let set: QuerySet = serde_json::from_slice(
        &std::fs::read(&query_path)
            .unwrap_or_else(|e| panic!("reading {}: {e}", query_path.display())),
    )
    .expect("parsing query set");
    if set.repo != args.repo {
        panic!(
            "query set {} declares repo '{}', expected '{}'",
            query_path.display(),
            set.repo,
            args.repo
        );
    }

    let target_root = resolve_target_root(&codeweave_root, &args);
    validate_expected_paths(&target_root, &set);
    let (revision, dirty_worktree) = git_state(&target_root);
    if let (Some(expected), Some(actual)) = (&set.revision, &revision) {
        if expected != actual {
            eprintln!(
                "warning: query set is pinned to {expected}, but {} is at {actual}",
                target_root.display()
            );
        }
    }
    if dirty_worktree {
        eprintln!(
            "warning: {} has uncommitted changes; baseline records dirty_worktree=true",
            target_root.display()
        );
    }

    let exclusions = WorkspaceExclusions::new(&target_root, &[]).expect("exclusions");

    // Cold index: no cache file, full scan.
    let cold_start = Instant::now();
    let index = CodeIndex::scan(&target_root, MAX_FILE_BYTES, &[], &exclusions).expect("cold scan");
    let cold_index_ms = cold_start.elapsed().as_secs_f64() * 1e3;

    // Warm index: a second scan approximates re-open cost on an unchanged tree.
    let warm_start = Instant::now();
    let _ = CodeIndex::scan(&target_root, MAX_FILE_BYTES, &[], &exclusions).expect("warm scan");
    let warm_index_ms = warm_start.elapsed().as_secs_f64() * 1e3;

    let mut per_query = Vec::new();
    let mut search_times = Vec::new();

    for query in &set.queries {
        let optional: Vec<String> = Vec::new();
        let dirty: HashSet<String> = query.dirty.iter().cloned().collect();
        let recent_mutations: HashSet<String> = query.recent_mutations.iter().cloned().collect();
        let start = Instant::now();
        let response = index
            .context(ContextParams {
                workspace_id: "eval",
                snapshot_id: "eval",
                query: &query.query,
                terms: &[],
                required_terms: &[],
                optional_terms: &optional,
                exclude_terms: &[],
                document_types: &[],
                min_score: 0.0,
                path_filters: &[],
                evidence: &[],
                dirty: &dirty,
                recent_mutations: &recent_mutations,
                budget_chars: BUDGET_CHARS,
                max_results: MAX_RESULTS,
                symbol_detail: SymbolDetail::Auto,
                ranking: ranking_mode,
            })
            .expect("context query");
        let search_ms = start.elapsed().as_secs_f64() * 1e3;
        search_times.push(search_ms);

        let paths = result_paths(&response);
        let (complete_syms, total_results) = complete_symbol_counts(&response);
        let expected: HashSet<&str> = query
            .expected
            .iter()
            .map(|target| {
                target
                    .split_once("::")
                    .map_or(target.as_str(), |item| item.0)
            })
            .collect();
        let hit_rank = paths
            .iter()
            .position(|path| expected.contains(path.as_str()))
            .map(|zero_based| zero_based + 1);
        let chars = response
            .get("used_chars")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;

        per_query.push(QueryResult {
            id: query.id.clone(),
            category: query.category.clone(),
            hit_rank,
            recall_at_1: hit_rank.is_some_and(|r| r <= 1),
            recall_at_5: hit_rank.is_some_and(|r| r <= 5),
            recall_at_10: hit_rank.is_some_and(|r| r <= 10),
            result_count: paths.len(),
            chars,
            search_ms,
            complete_symbols: complete_syms,
            total_results,
            top_paths: paths.into_iter().take(5).collect(),
        });
    }

    let n = per_query.len() as f64;
    let recall_at_1 = fraction(&per_query, |q| q.recall_at_1);
    let recall_at_5 = fraction(&per_query, |q| q.recall_at_5);
    let recall_at_10 = fraction(&per_query, |q| q.recall_at_10);
    let mrr_at_10 = per_query
        .iter()
        .map(|q| q.hit_rank.map_or(0.0, |r| 1.0 / r as f64))
        .sum::<f64>()
        / n;
    let mean_chars = per_query.iter().map(|q| q.chars as f64).sum::<f64>() / n;
    let total_results: usize = per_query.iter().map(|q| q.total_results).sum();
    let complete_symbols: usize = per_query.iter().map(|q| q.complete_symbols).sum();
    let complete_symbol_rate = if total_results == 0 {
        0.0
    } else {
        complete_symbols as f64 / total_results as f64
    };

    let baseline = Baseline {
        ranking: args.ranking.clone(),
        repo: set.repo.clone(),
        revision,
        dirty_worktree,
        query_count: per_query.len(),
        recall_at_1,
        recall_at_5,
        recall_at_10,
        mrr_at_10,
        mean_chars,
        complete_symbol_rate,
        cold_index_ms,
        warm_index_ms,
        search_p50_ms: percentile(&search_times, 0.50),
        search_p95_ms: percentile(&search_times, 0.95),
        per_query,
    };

    print_table(&baseline);

    let out_dir = if args.repo == "codeweave" {
        codeweave_root.join("eval/baseline")
    } else {
        codeweave_root.join("eval/baseline").join(&args.repo)
    };
    std::fs::create_dir_all(&out_dir).expect("baseline dir");
    let out_path = out_dir.join(format!("{}.json", args.ranking));
    std::fs::write(
        &out_path,
        serde_json::to_string_pretty(&baseline).expect("serialize baseline"),
    )
    .expect("write baseline");
    println!("\nwrote {}", out_path.display());
}

fn parse_args() -> Args {
    let mut parsed = Args {
        ranking: "v1".to_owned(),
        repo: "codeweave".to_owned(),
        repo_path: None,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--ranking" => {
                parsed.ranking = args.next().expect("--ranking requires v1 or v2");
            }
            other if other.starts_with("--ranking=") => {
                parsed.ranking = other["--ranking=".len()..].to_owned();
            }
            "--repo" => {
                parsed.repo = args.next().expect("--repo requires a query-set name");
            }
            other if other.starts_with("--repo=") => {
                parsed.repo = other["--repo=".len()..].to_owned();
            }
            "--repo-path" => {
                parsed.repo_path = Some(PathBuf::from(
                    args.next().expect("--repo-path requires a directory"),
                ));
            }
            other if other.starts_with("--repo-path=") => {
                parsed.repo_path = Some(PathBuf::from(&other["--repo-path=".len()..]));
            }
            "--help" | "-h" => {
                println!(
                    "Usage: cargo run -p eval -- --repo <codeweave|crawlerai> --ranking <v1|v2> [--repo-path <path>]"
                );
                std::process::exit(0);
            }
            other => panic!("unknown argument '{other}' (use --help)"),
        }
    }
    parsed
}

/// The eval crate lives at `<repo>/eval`; its manifest dir is that subfolder, so
/// the repository root is one level up.
fn codeweave_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("eval crate has a parent directory")
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
        .unwrap_or_else(|error| panic!("opening eval repository {}: {error}", path.display()))
}

fn validate_expected_paths(root: &Path, set: &QuerySet) {
    for query in &set.queries {
        assert!(
            !query.expected.is_empty(),
            "{} has no expected targets",
            query.id
        );
        for target in &query.expected {
            let path = target
                .split_once("::")
                .map_or(target.as_str(), |item| item.0);
            assert!(
                root.join(path).is_file(),
                "{} expected target does not exist: {}",
                query.id,
                root.join(path).display()
            );
        }
    }
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

fn result_paths(response: &Value) -> Vec<String> {
    response
        .get("results")
        .and_then(Value::as_array)
        .map(|results| {
            results
                .iter()
                .filter_map(|item| item.get("path").and_then(Value::as_str))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// Count `(complete_symbols, total_results)` in a response. `complete_symbol` is
/// absent under v1, so v1 always reports 0 complete of N.
fn complete_symbol_counts(response: &Value) -> (usize, usize) {
    let results = response.get("results").and_then(Value::as_array);
    let Some(results) = results else {
        return (0, 0);
    };
    let complete = results
        .iter()
        .filter(|item| {
            item.get("complete_symbol")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        })
        .count();
    (complete, results.len())
}

fn fraction(results: &[QueryResult], predicate: impl Fn(&QueryResult) -> bool) -> f64 {
    if results.is_empty() {
        return 0.0;
    }
    results.iter().filter(|q| predicate(q)).count() as f64 / results.len() as f64
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

fn print_table(baseline: &Baseline) {
    println!(
        "CodeWeave retrieval baseline — ranking={}",
        baseline.ranking
    );
    println!("repo={}  queries={}", baseline.repo, baseline.query_count);
    if let Some(revision) = &baseline.revision {
        println!("revision={revision}  dirty={}", baseline.dirty_worktree);
    }
    println!("{:-<64}", "");
    println!("{:<24}{:>10}", "Recall@1", fmt_pct(baseline.recall_at_1));
    println!("{:<24}{:>10}", "Recall@5", fmt_pct(baseline.recall_at_5));
    println!("{:<24}{:>10}", "Recall@10", fmt_pct(baseline.recall_at_10));
    println!("{:<24}{:>10.3}", "MRR@10", baseline.mrr_at_10);
    println!("{:<24}{:>10.0}", "Mean chars", baseline.mean_chars);
    println!(
        "{:<24}{:>10.3}",
        "Complete-symbol rate", baseline.complete_symbol_rate
    );
    println!("{:-<64}", "");
    println!("{:<24}{:>10.1}", "Cold index (ms)", baseline.cold_index_ms);
    println!("{:<24}{:>10.1}", "Warm index (ms)", baseline.warm_index_ms);
    println!("{:<24}{:>10.3}", "Search p50 (ms)", baseline.search_p50_ms);
    println!("{:<24}{:>10.3}", "Search p95 (ms)", baseline.search_p95_ms);
    println!("{:-<64}", "");
    println!("{:<28}{:<8}{:>6}", "query", "rank", "ms");
    for q in &baseline.per_query {
        let rank = q
            .hit_rank
            .map_or_else(|| "miss".to_owned(), |r| r.to_string());
        println!("{:<28}{:<8}{:>6.2}", truncate(&q.id, 27), rank, q.search_ms);
    }
}

fn fmt_pct(fraction: f64) -> String {
    format!("{:.1}%", fraction * 100.0)
}

fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        text.to_owned()
    } else {
        format!("{}…", &text[..max - 1])
    }
}
