use super::normalize::path_uri;
use super::protocol::PositionEncoding;
use super::service::IntelligenceService;
use super::workspace_edit::workspace_edit_changes;
use crate::model::IntelligenceSettings;
use codeweave_rust::index::{CodeIndex, SearchParams, WorkspaceExclusions};
use codeweave_rust::reference_service::{
    ReferencePosition, ReferenceRange, ReferenceService, SemanticReferenceLocation,
    SemanticReferenceMetadata,
};
use parking_lot::RwLock;
use serde_json::{json, Value};
use std::fs;
use std::sync::Arc;

fn shared_reference_fixture() -> (
    tempfile::TempDir,
    Arc<RwLock<CodeIndex>>,
    Arc<RwLock<String>>,
    IntelligenceService,
) {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.path().join("src")).unwrap();
    fs::write(
        root.path().join("src/owner.rs"),
        "pub fn open_workspace() {}\n",
    )
    .unwrap();
    fs::write(
        root.path().join("src/caller.rs"),
        "fn call() { open_workspace(); }\n",
    )
    .unwrap();
    let exclusions = WorkspaceExclusions::new(root.path(), &[]).unwrap();
    let index = Arc::new(RwLock::new(
        CodeIndex::scan(root.path(), 1_000_000, &[], &exclusions).unwrap(),
    ));
    let snapshot = Arc::new(RwLock::new("snap_phase5".to_owned()));
    let service = IntelligenceService::new(
        root.path().to_path_buf(),
        IntelligenceSettings::default(),
        "phase5".to_owned(),
        Arc::clone(&index),
        Arc::clone(&snapshot),
    );
    (root, index, snapshot, service)
}

fn strip_reference_handles(value: &mut Value) {
    if let Some(results) = value.get_mut("results").and_then(Value::as_array_mut) {
        for result in results {
            result.as_object_mut().unwrap().remove("handle");
        }
    }
}

#[test]
fn public_reference_entry_points_share_exact_fallback_response() {
    let (_root, index, snapshot, intelligence) = shared_reference_fixture();
    let snapshot_id = snapshot.read().clone();
    let retrieve = index
        .read()
        .search(SearchParams {
            workspace_id: "phase5",
            snapshot_id: &snapshot_id,
            mode: "references",
            query: "open_workspace",
            path_filters: &[],
            case_sensitive: true,
            max_results: 20,
            context_lines: 0,
            reference_scope: "all",
            reference_kinds: &[],
            definition_path: Some("src/owner.rs"),
            definition_line: Some(1),
        })
        .unwrap();
    let semantic_entry = intelligence
        .execute(&json!({
            "operation": "references",
            "path": "src/owner.rs",
            "line": 1,
            "column": 0,
            "max_results": 20
        }))
        .unwrap();
    assert_eq!(semantic_entry, retrieve);
    assert_eq!(retrieve["backend"], "fallback");
    assert_eq!(retrieve["evidence"], "lexical");
    assert_eq!(retrieve["freshness"], "current");
}

#[test]
fn shared_reference_response_matches_golden() {
    let (_root, index, snapshot, _intelligence) = shared_reference_fixture();
    let snapshot_id = snapshot.read().clone();
    let guard = index.read();
    let service = ReferenceService::new(&guard);
    let mut fallback = service
        .fallback_at_position("phase5", &snapshot_id, "src/owner.rs", 1, 20)
        .unwrap();
    strip_reference_handles(&mut fallback);
    let target = service.resolve_position("src/owner.rs", 1).unwrap();
    let mut semantic = service
        .semantic(
            "phase5",
            &snapshot_id,
            target,
            vec![SemanticReferenceLocation {
                path: "src/caller.rs".to_owned(),
                range: ReferenceRange {
                    start: ReferencePosition {
                        line: 1,
                        column: 13,
                        byte: None,
                    },
                    end: ReferencePosition {
                        line: 1,
                        column: 27,
                        byte: None,
                    },
                },
            }],
            20,
            SemanticReferenceMetadata {
                freshness: "current",
                evidence_caveat: "Language-server locations were produced from a full-text synchronized document hash that still matches disk and the live index.",
            },
        )
        .unwrap();
    strip_reference_handles(&mut semantic);
    let actual = json!({"fallback": fallback, "semantic": semantic});
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("eval/fixtures/references/shared-service-golden.json");
    if std::env::var_os("UPDATE_EVAL_SNAPSHOTS").is_some() {
        fs::write(&path, serde_json::to_string_pretty(&actual).unwrap() + "\n").unwrap();
    }
    let expected: Value = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(actual, expected);
}

#[test]
fn workspace_edit_compiles_utf16_ranges_to_transaction_changes() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("sample.py");
    fs::write(&path, "def café():\n    return 1\n").unwrap();
    let edit = json!({"changes":{path_uri(&path):[{
        "range":{"start":{"line":0,"character":4},"end":{"line":0,"character":8}},
        "newText":"bistro"
    }]}});
    let changes = workspace_edit_changes(root.path(), &edit, PositionEncoding::Utf16).unwrap();
    assert_eq!(changes[0]["kind"], "replace");
    assert!(changes[0]["new_text"].as_str().unwrap().contains("bistro"));
}
