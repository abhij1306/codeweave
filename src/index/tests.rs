use super::*;
use crate::symbols::extract_symbols;
use std::collections::HashSet;
use std::fs;
use std::path::Path;

fn test_entry(path: &str, content: &str) -> FileEntry {
    FileEntry::with_symbols(
        path.to_owned(),
        content.to_owned(),
        content_hash(content),
        extract_symbols(Path::new(path), content),
        content.len() as u64,
        0,
    )
}

#[cfg(unix)]
#[test]
fn incremental_refresh_does_not_follow_symlinks_outside_workspace() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.rs"), "fn outside_secret() {}\n").unwrap();
    let exclusions = WorkspaceExclusions::new(root.path(), &[]).unwrap();
    let mut index = CodeIndex::scan(root.path(), 1_000_000, &[], &exclusions).unwrap();
    let link = root.path().join("linked.rs");
    symlink(outside.path().join("secret.rs"), &link).unwrap();

    index
        .refresh_paths(root.path(), &HashSet::from([link]), 1_000_000, &exclusions)
        .unwrap();

    assert!(index.get("linked.rs").is_none());
}

#[test]
fn handles_round_trip() {
    let original = RangeHandle {
        workspace_id: "w".into(),
        path: "a.rs".into(),
        start_line: 1,
        end_line: 2,
        content_hash: "h".into(),
    };
    let encoded = encode_handle(&original).unwrap();
    let decoded = decode_handle(&encoded).unwrap();
    assert_eq!(decoded.path, "a.rs");
    assert!(encoded.len() < 180);
    assert!(!encoded.contains("snapshot_id"));
}
#[test]
fn content_hash_is_lowercase_hex_sha256() {
    assert_eq!(
        content_hash("x"),
        "2d711642b726b04401627ca9fbac32f5c8530fb1903cc4db02258717921a4881"
    );
}

#[test]
fn index_metrics_report_loc_postings_symbols_and_memory_floor() {
    let mut index = CodeIndex::default();
    index.insert(test_entry(
        "src/lib.rs",
        "pub fn alpha() {}\npub fn beta() { alpha(); }\n",
    ));
    index.insert(test_entry("README.md", "# Fixture\nalpha docs\n"));

    let metrics = index.metrics();
    assert_eq!(metrics.indexed_file_count, 2);
    assert_eq!(metrics.indexed_loc, 4);
    assert_eq!(metrics.indexed_source_loc, 4);
    assert!(metrics.indexed_content_bytes > 0);
    assert!(metrics.estimated_heap_bytes_lower_bound >= metrics.indexed_content_bytes);
    assert!(metrics.symbol_name_count >= 2);
    assert!(metrics.symbol_declaration_count >= 2);
}

#[test]
fn incremental_snapshots_are_order_independent_and_update_on_change() {
    let mut first = CodeIndex::default();
    first.insert(test_entry("src/a.rs", "fn a() {}\n"));
    first.insert(test_entry("src/b.rs", "fn b() {}\n"));

    let mut second = CodeIndex::default();
    second.insert(test_entry("src/b.rs", "fn b() {}\n"));
    second.insert(test_entry("src/a.rs", "fn a() {}\n"));

    assert_eq!(first.snapshot_id("head"), second.snapshot_id("head"));
    let original = first.snapshot_id("head");
    assert_ne!(first.snapshot_id("other-head"), original);
    assert_eq!(first.snapshot_id("head"), original);
    first.insert(test_entry(
        "src/a.rs",
        "fn a() { println!(\"changed\"); }\n",
    ));
    assert_ne!(first.snapshot_id("head"), original);
    first.insert(test_entry("src/a.rs", "fn a() {}\n"));
    assert_eq!(first.snapshot_id("head"), original);
}

#[test]
fn symbol_index_replaces_stale_definitions() {
    let mut index = CodeIndex::default();
    index.insert(test_entry("src/a.rs", "fn old_name() {}\n"));
    assert!(index.find_symbol(None, "old_name").is_some());

    index.insert(test_entry("src/a.rs", "fn new_name() {}\n"));
    assert!(index.find_symbol(None, "old_name").is_none());
    assert_eq!(index.find_symbol(None, "new_name").unwrap().0, "src/a.rs");
}

#[test]
fn slice_lines_uses_inclusive_line_numbers() {
    assert_eq!(slice_lines("one\ntwo\nthree\n", 2, 3), "two\nthree");
}

#[test]
fn reference_search_is_explicit_and_excludes_the_declaration() {
    let mut index = CodeIndex::default();
    for (path, content) in [
        ("src/workspace.rs", "pub fn open_workspace() {}\n"),
        ("src/main.rs", "fn main() { open_workspace(); }\n"),
    ] {
        index.insert(test_entry(path, content));
    }
    let result = index
        .search(SearchParams {
            workspace_id: "main",
            snapshot_id: "snap_test",
            mode: "references",
            query: "open_workspace",
            path_filters: &[],
            case_sensitive: true,
            max_results: 10,
            context_lines: 1,
            reference_scope: "all",
            reference_kinds: &[],
            definition_path: None,
            definition_line: None,
        })
        .unwrap();
    assert_eq!(result["mode"], "references");
    assert_eq!(result["definitions"][0]["path"], "src/workspace.rs");
    assert_eq!(result["results"][0]["path"], "src/main.rs");
}

#[test]
fn reference_search_scans_every_allowed_file() {
    let mut index = CodeIndex::default();
    index.insert(test_entry(
        "src/owner.rs",
        "pub struct Validator;\nimpl Validator { pub fn run_edit_validation(&self) {} }\n",
    ));
    index.insert(test_entry(
        "src/caller.rs",
        "impl Validator {\n    fn apply(&self) {\n        self.run_edit_validation();\n    }\n}\n",
    ));

    let result = index
        .search(SearchParams {
            workspace_id: "main",
            snapshot_id: "snap_test",
            mode: "references",
            query: "run_edit_validation",
            path_filters: &[],
            case_sensitive: true,
            max_results: 10,
            context_lines: 0,
            reference_scope: "all",
            reference_kinds: &[],
            definition_path: Some("src/owner.rs"),
            definition_line: Some(2),
        })
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
    let mut index = CodeIndex::default();
    index.insert(test_entry("src/owner.rs", "pub fn x() {}\n"));
    index.insert(test_entry("src/caller.rs", "fn call() { x(); }\n"));

    let result = index
        .search(SearchParams {
            workspace_id: "main",
            snapshot_id: "snap_test",
            mode: "references",
            query: "x",
            path_filters: &[],
            case_sensitive: true,
            max_results: 10,
            context_lines: 0,
            reference_scope: "all",
            reference_kinds: &[],
            definition_path: None,
            definition_line: None,
        })
        .unwrap();

    assert_eq!(result["definitions"][0]["path"], "src/owner.rs");
    assert_eq!(result["results"][0]["path"], "src/caller.rs");
}

#[test]
fn symbol_search_orders_exact_matches_before_contains_matches() {
    let mut index = CodeIndex::default();
    index.insert(test_entry(
        "src/settings.rs",
        "pub struct CrawlRunSettings;\n",
    ));
    index.insert(test_entry("src/model.rs", "pub struct CrawlRun;\n"));

    let result = index
        .search(SearchParams {
            workspace_id: "main",
            snapshot_id: "snap_test",
            mode: "symbol",
            query: "CrawlRun",
            path_filters: &[],
            case_sensitive: true,
            max_results: 10,
            context_lines: 1,
            reference_scope: "all",
            reference_kinds: &[],
            definition_path: None,
            definition_line: None,
        })
        .unwrap();

    assert_eq!(result["results"][0]["path"], "src/model.rs");
    assert_eq!(result["results"][0]["symbol"]["name"], "CrawlRun");
}

#[test]
fn reference_search_distributes_results_across_files() {
    let mut index = CodeIndex::default();
    index.insert(test_entry("src/owner.rs", "pub fn process_run() {}\n"));
    index.insert(test_entry("tests/noisy.rs", &"process_run();\n".repeat(12)));
    index.insert(test_entry(
        "src/caller.rs",
        "fn call() { process_run(); }\n",
    ));

    let result = index
        .search(SearchParams {
            workspace_id: "main",
            snapshot_id: "snap_test",
            mode: "references",
            query: "process_run",
            path_filters: &[],
            case_sensitive: true,
            max_results: 8,
            context_lines: 0,
            reference_scope: "all",
            reference_kinds: &[],
            definition_path: None,
            definition_line: None,
        })
        .unwrap();

    let noisy_count = result["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["path"] == "tests/noisy.rs")
        .count();
    assert!(noisy_count <= 3);
    assert!(result["results"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["path"] == "src/caller.rs"));
    assert_eq!(result["truncated"], true);
}

#[test]
fn reference_search_merges_adjacent_matches_into_bounded_windows() {
    let mut index = CodeIndex::default();
    index.insert(test_entry("src/owner.rs", "pub fn process_run() {}\n"));
    index.insert(test_entry(
        "src/caller.rs",
        "fn call() {\n    process_run();\n    process_run();\n    process_run();\n}\n",
    ));

    let result = index
        .search(SearchParams {
            workspace_id: "main",
            snapshot_id: "snap_test",
            mode: "references",
            query: "process_run",
            path_filters: &[],
            case_sensitive: true,
            max_results: 10,
            context_lines: 1,
            reference_scope: "all",
            reference_kinds: &[],
            definition_path: None,
            definition_line: None,
        })
        .unwrap();

    let caller_results: Vec<_> = result["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["path"] == "src/caller.rs")
        .collect();
    assert_eq!(caller_results.len(), 1);
    assert_eq!(caller_results[0]["start_line"], 1);
    assert_eq!(caller_results[0]["end_line"], 5);
    assert_eq!(result["truncated"], false);
}

#[test]
fn literal_search_merges_overlapping_hits_and_distributes_files() {
    let mut index = CodeIndex::default();
    index.insert(test_entry(
        "src/alpha.rs",
        "needle one\nneedle two\nneedle three\n",
    ));
    index.insert(test_entry("src/beta.rs", "prefix\nneedle beta\nsuffix\n"));

    let result = index
        .search(SearchParams {
            workspace_id: "main",
            snapshot_id: "snap_test",
            mode: "literal",
            query: "needle",
            path_filters: &[],
            case_sensitive: false,
            max_results: 10,
            context_lines: 1,
            reference_scope: "all",
            reference_kinds: &[],
            definition_path: None,
            definition_line: None,
        })
        .unwrap();
    let paths: HashSet<_> = result["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|item| item["path"].as_str())
        .collect();

    assert_eq!(result["result_count"], 2);
    assert_eq!(paths, HashSet::from(["src/alpha.rs", "src/beta.rs"]));
    assert!(result["results"][0].get("hash").is_none());
}

#[test]
fn literal_search_scans_for_short_and_stop_word_queries() {
    let mut index = CodeIndex::default();
    index.insert(test_entry("src/value.rs", "let x = to();\n"));

    for query in ["x", "to"] {
        let result = index
            .search(SearchParams {
                workspace_id: "main",
                snapshot_id: "snap_test",
                mode: "literal",
                query,
                path_filters: &[],
                case_sensitive: true,
                max_results: 10,
                context_lines: 0,
                reference_scope: "all",
                reference_kinds: &[],
                definition_path: None,
                definition_line: None,
            })
            .unwrap();

        assert_eq!(result["result_count"], 1, "query: {query}");
        assert_eq!(result["results"][0]["path"], "src/value.rs");
    }
}

#[test]
fn regex_search_honors_case_sensitivity() {
    let mut index = CodeIndex::default();
    index.insert(test_entry("src/value.rs", "Alpha\nmiddle\nalpha\n"));

    let insensitive = index
        .search(SearchParams {
            workspace_id: "main",
            snapshot_id: "snap_test",
            mode: "regex",
            query: "^alpha$",
            path_filters: &[],
            case_sensitive: false,
            max_results: 10,
            context_lines: 0,
            reference_scope: "all",
            reference_kinds: &[],
            definition_path: None,
            definition_line: None,
        })
        .unwrap();
    let sensitive = index
        .search(SearchParams {
            workspace_id: "main",
            snapshot_id: "snap_test",
            mode: "regex",
            query: "^alpha$",
            path_filters: &[],
            case_sensitive: true,
            max_results: 10,
            context_lines: 0,
            reference_scope: "all",
            reference_kinds: &[],
            definition_path: None,
            definition_line: None,
        })
        .unwrap();

    assert_eq!(insensitive["result_count"], 2);
    assert_eq!(sensitive["result_count"], 1);
    assert_eq!(sensitive["results"][0]["line"], 3);
}

#[test]
fn filename_search_supports_glob_wildcards() {
    let mut index = CodeIndex::default();
    index.insert(test_entry(
        "backend/app/core/records/output_safety.py",
        "def sanitize_output():\n    pass\n",
    ));
    index.insert(test_entry(
        "backend/app/core/other.py",
        "def other():\n    pass\n",
    ));

    let result = index
        .search(SearchParams {
            workspace_id: "main",
            snapshot_id: "snap_test",
            mode: "filename",
            query: "*output*safety*",
            path_filters: &[],
            case_sensitive: false,
            max_results: 10,
            context_lines: 0,
            reference_scope: "all",
            reference_kinds: &[],
            definition_path: None,
            definition_line: None,
        })
        .unwrap();

    assert_eq!(result["match_semantics"], "glob");
    assert_eq!(result["result_count"], 1);
    assert_eq!(
        result["results"][0]["path"],
        "backend/app/core/records/output_safety.py"
    );
}

#[test]
fn repo_map_honors_path_filters_as_strict_scope() {
    let mut index = CodeIndex::default();
    index.insert(test_entry("backend/app/main.py", "def main():\n    pass\n"));
    index.insert(test_entry(
        "backend/tests/test_main.py",
        "def test_main():\n    pass\n",
    ));
    index.insert(test_entry("README.md", "backend app docs\n"));

    let result = index
        .search(SearchParams {
            workspace_id: "main",
            snapshot_id: "snap_test",
            mode: "repo_map",
            query: "",
            path_filters: &["backend/app".to_owned()],
            case_sensitive: false,
            max_results: 10,
            context_lines: 0,
            reference_scope: "all",
            reference_kinds: &[],
            definition_path: None,
            definition_line: None,
        })
        .unwrap();

    assert_eq!(result["scope_applied"], true);
    assert_eq!(result["file_count"], 1);
    assert_eq!(result["total_file_count"], 3);
    assert_eq!(result["directories"][0]["path"], "backend/app");
}

#[test]
fn ignores_custom_cargo_target_directories() {
    assert!(ignored_workspace_path("core/target-audit/release/app.exe"));
    assert!(ignored_workspace_path(
        "core/target-auditDQhH1o/CACHEDIR.TAG"
    ));
    assert!(!ignored_workspace_path("core/src/targeting.rs"));
}

#[test]
fn configured_exclusions_apply_to_scans_and_incremental_refreshes() {
    let workspace = tempfile::tempdir().unwrap();
    fs::create_dir_all(workspace.path().join("backend/artifacts")).unwrap();
    fs::create_dir_all(workspace.path().join("src")).unwrap();
    fs::write(
        workspace.path().join("backend/artifacts/result.json"),
        "generated result",
    )
    .unwrap();
    fs::write(workspace.path().join("debug.log"), "noisy log").unwrap();
    fs::write(workspace.path().join("src/lib.rs"), "fn retained() {}\n").unwrap();
    let exclusions = WorkspaceExclusions::new(
        workspace.path(),
        &["backend/artifacts/".to_owned(), "*.log".to_owned()],
    )
    .unwrap();

    let mut index = CodeIndex::scan(workspace.path(), 2_000_000, &[], &exclusions).unwrap();

    assert!(index.get("src/lib.rs").is_some());
    assert!(index.get("backend/artifacts/result.json").is_none());
    assert!(index.get("debug.log").is_none());

    let generated = workspace.path().join("backend/artifacts/new.json");
    fs::write(&generated, "new generated result").unwrap();
    let changed = index
        .refresh_paths(
            workspace.path(),
            &HashSet::from([generated]),
            2_000_000,
            &exclusions,
        )
        .unwrap();
    assert!(changed.is_empty());
    assert!(index.get("backend/artifacts/new.json").is_none());
}

#[cfg(unix)]
#[test]
fn configured_exclusions_resolve_noncanonical_absolute_paths() {
    let workspace = tempfile::tempdir().unwrap();
    let canonical_workspace = workspace.path().canonicalize().unwrap();
    let excluded_dir = canonical_workspace.join("backend/artifacts");
    fs::create_dir_all(&excluded_dir).unwrap();
    fs::write(excluded_dir.join("result.json"), "generated result").unwrap();
    let alias_parent = tempfile::tempdir().unwrap();
    let aliased_workspace = alias_parent.path().join("workspace");
    std::os::unix::fs::symlink(&canonical_workspace, &aliased_workspace).unwrap();
    let aliased_file = aliased_workspace.join("backend/artifacts/result.json");
    let exclusions =
        WorkspaceExclusions::new(&canonical_workspace, &["backend/artifacts/".to_owned()]).unwrap();

    assert!(aliased_file.exists());
    assert!(exclusions.is_ignored(&aliased_file, false));
}

#[test]
fn artifact_scan_canonicalizes_workspace_root_for_containment() {
    let workspace = tempfile::tempdir().unwrap();
    fs::create_dir_all(workspace.path().join("nested")).unwrap();
    fs::create_dir_all(workspace.path().join("artifacts")).unwrap();
    fs::write(
        workspace.path().join("artifacts/inside.rs"),
        "fn inside() {}\n",
    )
    .unwrap();
    let noncanonical_root = workspace.path().join("nested").join("..");
    let exclusions = WorkspaceExclusions::new(&noncanonical_root, &[]).unwrap();

    let index = CodeIndex::scan(
        &noncanonical_root,
        2_000_000,
        &["artifacts".to_owned()],
        &exclusions,
    )
    .unwrap();

    assert!(index.get("artifacts/inside.rs").is_some());
}

#[test]
fn exclusion_patterns_reject_reinclusion_rules() {
    let workspace = tempfile::tempdir().unwrap();
    let error =
        WorkspaceExclusions::new(workspace.path(), &["!generated/keep.rs".to_owned()]).unwrap_err();
    assert_eq!(error.0.code, "INVALID_EXCLUDE_PATTERN");
}

#[test]
fn lexical_reference_search_prioritizes_calls_over_imports() {
    let mut index = CodeIndex::default();
    index.insert(test_entry("src/owner.rs", "pub fn extract() {}\n"));
    index.insert(test_entry(
        "src/a_import.rs",
        "use crate::owner::extract;\n",
    ));
    index.insert(test_entry("src/z_call.rs", "fn run() { extract(); }\n"));

    let result = index
        .search(SearchParams {
            workspace_id: "main",
            snapshot_id: "snap_test",
            mode: "references",
            query: "extract",
            path_filters: &[],
            case_sensitive: true,
            max_results: 10,
            context_lines: 0,
            reference_scope: "all",
            reference_kinds: &[],
            definition_path: None,
            definition_line: None,
        })
        .unwrap();

    assert_eq!(result["results"][0]["path"], "src/z_call.rs");
    assert_eq!(result["results"][0]["reference_kind"], "call");
    assert!(
        result["results"][0]["score"].as_f64().unwrap()
            > result["results"][1]["score"].as_f64().unwrap()
    );
}

#[test]
fn document_classification_uses_path_segments_not_substrings() {
    for (path, expected) in [
        ("src/testament.rs", DocumentType::Source),
        ("src/fixtures_parser.rs", DocumentType::Source),
        ("tests/unit/test_output.py", DocumentType::Test),
        ("src/value_test.rs", DocumentType::Test),
        ("web/app.spec.tsx", DocumentType::Test),
        ("fixtures/http_response.json", DocumentType::Artifact),
        ("recordings/login.recording.json", DocumentType::Artifact),
        ("runtime/state.json", DocumentType::RuntimeEvidence),
        ("evidence/state.txt", DocumentType::Source),
        ("runtime/state.txt", DocumentType::Source),
        ("logs/server.log", DocumentType::Log),
        ("docs/CLAUDE.md", DocumentType::Instruction),
    ] {
        assert_eq!(DocumentType::classify(path), expected, "path: {path}");
    }
    assert_eq!(
        serde_json::to_value(DocumentType::RuntimeEvidence).unwrap(),
        "runtime_evidence"
    );
}

#[test]
fn lookups_accept_backslash_and_dot_prefixed_paths() {
    let mut index = CodeIndex::default();
    index.insert(test_entry("src/a.rs", "fn a() {}\n"));

    assert_eq!(index.get(".\\src\\a.rs").unwrap().path(), "src/a.rs");
    assert_eq!(index.get("./src/a.rs").unwrap().path(), "src/a.rs");
}

#[test]
fn decoding_rejects_reversed_ranges_and_paths_outside_the_workspace() {
    for (path, start_line, end_line) in [
        ("a.rs", 5, 2),
        ("../secret.rs", 1, 1),
        ("src/..\\..\\secret.rs", 1, 1),
        ("", 1, 1),
    ] {
        let encoded = encode_handle(&RangeHandle {
            workspace_id: "w".into(),
            path: path.into(),
            start_line,
            end_line,
            content_hash: "h".into(),
        })
        .unwrap();
        let error = decode_handle(&encoded).unwrap_err();
        assert_eq!(error.0.code, "INVALID_HANDLE", "path: {path}");
    }
}

#[test]
fn removal_restores_the_previous_snapshot_and_lookups() {
    let mut index = CodeIndex::default();
    index.insert(test_entry("src/a.rs", "fn a() {}\n"));
    let before = index.snapshot_id("head");
    index.insert(test_entry("src/b.rs", "fn b() {}\n"));
    assert_ne!(index.snapshot_id("head"), before);

    assert_eq!(index.remove("src/b.rs").unwrap().path(), "src/b.rs");

    assert_eq!(index.snapshot_id("head"), before);
    assert!(index.find_symbol(None, "b").is_none());
    assert_eq!(index.file_count(), 1);
}
