//! Persistent cache behaviour, observed through `CodeIndex::scan_cached`.

use super::{CodeIndex, WorkspaceExclusions};
use std::fs;

fn declaring_paths(index: &CodeIndex, name: &str) -> Vec<String> {
    index
        .find_symbols(None, name)
        .into_iter()
        .map(|found| found.path)
        .collect()
}

#[test]
fn changing_exclusions_invalidates_the_index_cache() {
    let workspace = tempfile::tempdir().unwrap();
    let cache_dir = tempfile::tempdir().unwrap();
    let cache_file = cache_dir.path().join("index.json");
    fs::create_dir_all(workspace.path().join("generated")).unwrap();
    fs::write(workspace.path().join("generated/value.txt"), "value").unwrap();
    let no_exclusions = WorkspaceExclusions::new(workspace.path(), &[]).unwrap();
    CodeIndex::scan_cached(
        workspace.path(),
        2_000_000,
        &[],
        &no_exclusions,
        &cache_file,
    )
    .unwrap();
    let exclusions =
        WorkspaceExclusions::new(workspace.path(), &["generated/".to_owned()]).unwrap();

    let (index, cache_hit) =
        CodeIndex::scan_cached(workspace.path(), 2_000_000, &[], &exclusions, &cache_file).unwrap();

    assert!(!cache_hit);
    assert!(index.get("generated/value.txt").is_none());
}

#[test]
fn mismatched_cache_schema_is_rejected() {
    let workspace = tempfile::tempdir().unwrap();
    let cache_dir = tempfile::tempdir().unwrap();
    let cache_file = cache_dir.path().join("index.json");
    fs::write(workspace.path().join("value.rs"), "fn value() {}\n").unwrap();
    let exclusions = WorkspaceExclusions::new(workspace.path(), &[]).unwrap();

    let (_, initial_hit) =
        CodeIndex::scan_cached(workspace.path(), 2_000_000, &[], &exclusions, &cache_file).unwrap();
    assert!(!initial_hit);

    let mut cache: serde_json::Value =
        serde_json::from_slice(&fs::read(&cache_file).unwrap()).unwrap();
    cache["schema"] = serde_json::json!("invalid-schema");
    fs::write(&cache_file, serde_json::to_vec(&cache).unwrap()).unwrap();

    let (_, cache_hit) =
        CodeIndex::scan_cached(workspace.path(), 2_000_000, &[], &exclusions, &cache_file).unwrap();

    assert!(!cache_hit);
}

#[test]
fn warm_cache_scan_reports_a_hit_and_indexes_the_same_symbols() {
    let workspace = tempfile::tempdir().unwrap();
    let cache_dir = tempfile::tempdir().unwrap();
    fs::write(
        workspace.path().join("lib.rs"),
        "pub fn warm_cache_symbol() {}\n",
    )
    .unwrap();
    let cache_file = cache_dir.path().join("index.json");
    let exclusions = WorkspaceExclusions::new(workspace.path(), &[]).unwrap();

    let (_, first_hit) =
        CodeIndex::scan_cached(workspace.path(), 2_000_000, &[], &exclusions, &cache_file).unwrap();
    let (second, second_hit) =
        CodeIndex::scan_cached(workspace.path(), 2_000_000, &[], &exclusions, &cache_file).unwrap();

    assert!(!first_hit);
    assert!(second_hit);
    assert_eq!(declaring_paths(&second, "warm_cache_symbol"), ["lib.rs"]);
}

#[test]
fn cache_hit_never_serves_symbols_or_content_of_changed_files() {
    let workspace = tempfile::tempdir().unwrap();
    let cache_dir = tempfile::tempdir().unwrap();
    let source = workspace.path().join("lib.rs");
    let cache_file = cache_dir.path().join("index.json");
    let exclusions = WorkspaceExclusions::new(workspace.path(), &[]).unwrap();
    fs::write(&source, "fn old() {}\n").unwrap();
    CodeIndex::scan_cached(workspace.path(), 2_000_000, &[], &exclusions, &cache_file).unwrap();
    fs::write(&source, "fn new() {}\n").unwrap();

    let (index, cache_hit) =
        CodeIndex::scan_cached(workspace.path(), 2_000_000, &[], &exclusions, &cache_file).unwrap();

    assert!(cache_hit);
    assert_eq!(index.get("lib.rs").unwrap().content(), "fn new() {}\n");
    assert_eq!(declaring_paths(&index, "new"), ["lib.rs"]);
    assert!(declaring_paths(&index, "old").is_empty());
}

#[test]
fn corrupt_cache_is_a_miss_and_is_rewritten() {
    let workspace = tempfile::tempdir().unwrap();
    let cache_dir = tempfile::tempdir().unwrap();
    let cache_file = cache_dir.path().join("index.json");
    fs::write(workspace.path().join("value.rs"), "fn value() {}\n").unwrap();
    fs::write(&cache_file, b"{\"schema\": truncated").unwrap();
    let exclusions = WorkspaceExclusions::new(workspace.path(), &[]).unwrap();

    let (index, corrupt_hit) =
        CodeIndex::scan_cached(workspace.path(), 2_000_000, &[], &exclusions, &cache_file).unwrap();
    let (_, rewritten_hit) =
        CodeIndex::scan_cached(workspace.path(), 2_000_000, &[], &exclusions, &cache_file).unwrap();

    assert!(!corrupt_hit);
    assert_eq!(declaring_paths(&index, "value"), ["value.rs"]);
    assert!(rewritten_hit);
}

#[test]
fn cache_rewrite_replaces_the_previous_file_without_leftovers() {
    let workspace = tempfile::tempdir().unwrap();
    let cache_dir = tempfile::tempdir().unwrap();
    let cache_file = cache_dir.path().join("index.json");
    fs::write(workspace.path().join("value.rs"), "fn value() {}\n").unwrap();
    let exclusions = WorkspaceExclusions::new(workspace.path(), &[]).unwrap();

    for _ in 0..2 {
        CodeIndex::scan_cached(workspace.path(), 2_000_000, &[], &exclusions, &cache_file).unwrap();
    }

    let names: Vec<_> = fs::read_dir(cache_dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(names, vec!["index.json".to_owned()]);
}
