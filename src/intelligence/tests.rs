//! `code_intelligence` behaviour through the service interface, with scripted
//! language servers behind the semantic seam.
use super::preset::LspPreset;
use super::test_support::{FakeServer, Fixture, Reply};
use super::IntelligenceResponse;
use crate::index::{content_hash, SearchScope};
use crate::model::AppResult;
use crate::reference_service::{ReferenceQuery, ReferenceScope, ReferenceService};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

const LIB: &str = "pub fn open_workspace() {}\n";
const CALLER: &str = "fn call() { open_workspace(); }\n";

fn fixture() -> Fixture {
    Fixture::new(&[("src/lib.rs", LIB), ("src/caller.rs", CALLER)])
}

fn range(line: u64, start: u64, end: u64) -> Value {
    json!({"start": {"line": line, "character": start}, "end": {"line": line, "character": end}})
}

fn run(service: &super::IntelligenceService, params: Value) -> AppResult<Value> {
    service
        .execute(&params)
        .map(IntelligenceResponse::into_value)
}

fn definition() -> Value {
    json!({"operation": "definition", "path": "src/lib.rs", "line": 1, "column": 10})
}

fn references() -> Value {
    json!({"operation": "references", "path": "src/lib.rs", "line": 1, "column": 10})
}

fn diagnostics() -> Value {
    json!({"operation": "diagnostics", "path": "src/lib.rs", "max_results": 2})
}

fn rename() -> Value {
    json!({"operation": "rename_preview", "path": "src/lib.rs", "line": 1, "column": 10, "new_name": "open_repo"})
}

/// The scripted answer to every operation for the fixture.
fn answer(lib: &str, caller: &str, method: &str) -> Reply {
    match method {
        "textDocument/definition" => Reply::Result(json!([{"uri": lib, "range": range(0, 7, 21)}])),
        "textDocument/references" => {
            Reply::Result(json!([{"uri": caller, "range": range(0, 12, 26)}]))
        }
        "textDocument/diagnostic" => Reply::Result(json!({"kind": "full", "items": [
            {"message": "one"}, {"message": "two"}, {"message": "three"}
        ]})),
        "textDocument/rename" => Reply::Result(json!({"changes": {
            lib: [{"range": range(0, 7, 21), "newText": "open_repo"}],
            caller: [{"range": range(0, 12, 26), "newText": "open_repo"}]
        }})),
        _ => Reply::Result(Value::Null),
    }
}

fn answering_server(fixture: &Fixture) -> FakeServer {
    let (lib, caller) = (fixture.uri("src/lib.rs"), fixture.uri("src/caller.rs"));
    FakeServer::new(move |method, _| answer(&lib, &caller, method))
}

#[test]
fn definition_returns_semantic_locations_from_the_server() {
    let fixture = fixture();
    let server = answering_server(&fixture);
    let service = fixture.service_with(&server, LspPreset::Rust);

    let result = run(&service, definition()).unwrap();

    assert_eq!(result["backend"], "semantic");
    assert_eq!(
        result["results"],
        json!([{"path": "src/lib.rs", "line": 1, "column": 7, "end_line": 1, "end_column": 21, "evidence": "semantic"}])
    );
    assert_eq!(
        result["synchronization"]["synchronized_hash"],
        content_hash(LIB)
    );
    assert_eq!(result["synchronization"]["document_version"], 1);
    assert_eq!(
        server.messages("textDocument/definition")[0]["params"]["position"],
        json!({"line": 0, "character": 10})
    );
}

#[test]
fn references_return_semantic_locations_through_the_reference_service() {
    let fixture = fixture();
    let server = answering_server(&fixture);
    let service = fixture.service_with(&server, LspPreset::Rust);

    let result = run(&service, references()).unwrap();

    assert_eq!(result["backend"], "semantic");
    assert_eq!(result["result_count"], 1);
    assert_eq!(result["results"][0]["path"], "src/caller.rs");
    assert_eq!(result["results"][0]["range"]["start"]["column"], 13);
    assert_eq!(result["synchronization"]["freshness"], "current");
}

#[test]
fn diagnostics_return_bounded_semantic_results() {
    let fixture = fixture();
    let server = answering_server(&fixture);
    let service = fixture.service_with(&server, LspPreset::Rust);

    let result = run(&service, diagnostics()).unwrap();

    assert_eq!(result["backend"], "semantic");
    assert_eq!(result["status"], "available");
    assert_eq!(
        result["diagnostics"],
        json!([{"message": "one"}, {"message": "two"}])
    );
    assert_eq!(
        [
            &result["total_count"],
            &result["result_count"],
            &result["truncated"]
        ],
        [&json!(3), &json!(2), &json!(true)]
    );
}

#[test]
fn rename_preview_compiles_server_edits_into_one_edit_request() {
    let fixture = fixture();
    let server = answering_server(&fixture);
    let service = fixture.service_with(&server, LspPreset::Rust);

    let IntelligenceResponse::RenamePreview(preview) = service.execute(&rename()).unwrap() else {
        panic!("rename_preview must return a rename preview");
    };
    let request = preview.edit_request(json!("snap_1"));

    assert_eq!(
        request,
        json!({
            "preview": true,
            "snapshot_id": "snap_1",
            "changes": [
                {
                    "kind": "replace", "path": "src/caller.rs", "old_text": CALLER,
                    "new_text": "fn call() { open_repo(); }\n",
                    "expected_replacements": 1, "expected_hash": content_hash(CALLER)
                },
                {
                    "kind": "replace", "path": "src/lib.rs", "old_text": LIB,
                    "new_text": "pub fn open_repo() {}\n",
                    "expected_replacements": 1, "expected_hash": content_hash(LIB)
                }
            ]
        })
    );
    assert_eq!(preview.into_report()["changes"], request["changes"]);
}

#[test]
fn a_target_edited_during_the_request_is_stale_for_every_operation() {
    let fixture = fixture();
    let target = fixture.path("src/lib.rs");
    let (lib, caller) = (fixture.uri("src/lib.rs"), fixture.uri("src/caller.rs"));
    let server = FakeServer::new(move |method, _| {
        std::fs::write(
            &target,
            "pub fn edited() {}
",
        )
        .unwrap();
        answer(&lib, &caller, method)
    });
    let service = fixture.service_with(&server, LspPreset::Rust);

    let mut reasons = Vec::new();
    for params in [definition(), references(), diagnostics()] {
        let result = run(&service, params).unwrap();
        reasons.push((
            result["backend"].clone(),
            result["fallback_reason"]["code"].clone(),
            result["fallback_reason"]["details"]["phase"].clone(),
        ));
        fixture.write("src/lib.rs", LIB);
    }
    let rename_error = run(&service, rename()).unwrap_err();

    let expected = (
        json!("fallback"),
        json!("INTELLIGENCE_STALE"),
        json!("after_request"),
    );
    assert_eq!(reasons, vec![expected.clone(), expected.clone(), expected]);
    assert_eq!(rename_error.0.code, "INTELLIGENCE_STALE");
}

#[test]
fn a_target_that_changed_before_the_request_is_rejected() {
    let fixture = fixture();
    fixture.write("src/lib.rs", "pub fn edited() {}\n");
    let service = fixture.service();

    let error = run(&service, definition()).unwrap_err();

    assert_eq!(error.0.code, "INTELLIGENCE_STALE");
    assert_eq!(error.0.details.unwrap()["phase"], "before_request");
}

#[test]
fn a_rename_touching_a_file_that_changed_since_indexing_is_stale() {
    let fixture = fixture();
    let server = answering_server(&fixture);
    let service = fixture.service_with(&server, LspPreset::Rust);
    fixture.write(
        "src/caller.rs",
        "fn call() { open_workspace(); open_workspace(); }\n",
    );

    let error = run(&service, rename()).unwrap_err();

    assert_eq!(error.0.code, "INTELLIGENCE_STALE");
    let details = error.0.details.unwrap();
    assert_eq!(
        [&details["phase"], &details["path"]],
        [&json!("result_file"), &json!("src/caller.rs")]
    );
}

#[test]
fn a_silent_server_times_out_and_every_operation_falls_back() {
    let fixture = fixture();
    let server = FakeServer::new(|_, _| Reply::Silence);
    let service = fixture.service_with(&server, LspPreset::Rust);

    let definition = run(&service, definition()).unwrap();
    let references = run(&service, references()).unwrap();
    let diagnostics = run(&service, diagnostics()).unwrap();
    let rename = run(&service, rename()).unwrap_err();

    assert_eq!(
        [
            &definition["backend"],
            &definition["fallback_reason"]["code"],
            &references["backend"],
            &references["evidence"],
            &references["fallback_reason"]["code"],
            &diagnostics["status"],
            &diagnostics["fallback_reason"]["code"],
            &diagnostics["semantic_backend"]["readiness"],
        ],
        [
            &json!("fallback"),
            &json!("LSP_TIMEOUT"),
            &json!("fallback"),
            &json!("lexical"),
            &json!("LSP_TIMEOUT"),
            &json!("partial"),
            &json!("LSP_TIMEOUT"),
            &json!("lazy"),
        ]
    );
    assert_eq!(
        definition["result"]["result"]["symbol"]["name"],
        "open_workspace"
    );
    assert_eq!(rename.0.code, "LSP_TIMEOUT");
    // Each timed-out operation restarts the server once before giving up.
    assert_eq!(server.spawn_count(), 8);
}

#[test]
fn without_a_backend_reads_fall_back_and_rename_is_unavailable() {
    let fixture = fixture();
    let service = fixture.service();

    let definition = run(&service, definition()).unwrap();
    let diagnostics = run(&service, diagnostics()).unwrap();
    let rename = run(&service, rename()).unwrap_err();

    assert_eq!(definition["backend"], "fallback");
    assert_eq!(definition["result"]["result"]["path"], "src/lib.rs");
    assert!(definition.get("fallback_reason").is_none());
    assert_eq!(diagnostics["diagnostic_scope"], "syntax_only");
    assert_eq!(diagnostics["result"]["diagnostics"], json!([]));
    assert_eq!(rename.0.code, "SEMANTIC_BACKEND_UNAVAILABLE");
}

#[test]
fn reference_fallback_matches_code_retrieve_references() {
    let fixture = fixture();
    let service = fixture.service();
    let snapshot_id = fixture.live().snapshot_id();
    let expected = ReferenceService::new(fixture.state().read().index())
        .fallback(
            &SearchScope {
                workspace_id: "fixture",
                snapshot_id: &snapshot_id,
                paths: &[],
                max_results: 20,
            },
            &ReferenceQuery {
                symbol: "open_workspace".to_owned(),
                scope: ReferenceScope::All,
                kinds: Vec::new(),
                definition_path: Some("src/lib.rs".to_owned()),
                definition_line: Some(1),
                context_lines: 0,
            },
        )
        .unwrap();

    let actual = run(&service, references()).unwrap();

    assert_eq!(actual, expected);
    assert_eq!(actual["backend"], "fallback");
    assert_eq!(actual["results"][0]["path"], "src/caller.rs");
}

#[test]
fn a_crashed_server_fails_fast_with_its_stderr() {
    let fixture = fixture();
    let server = FakeServer::new(|_, _| Reply::Exit);
    let worker = server.worker(fixture.root.path(), LspPreset::Rust, Duration::from_secs(5));
    let service = fixture.service_with_backends(vec![std::sync::Arc::new(worker)]);
    let started = Instant::now();

    let result = run(&service, definition()).unwrap();

    assert!(
        started.elapsed() < Duration::from_secs(4),
        "{:?}",
        started.elapsed()
    );
    let reason = &result["fallback_reason"];
    assert_eq!(
        [
            &reason["code"],
            &reason["details"]["cause"],
            &reason["details"]["stderr_tail"]
        ],
        [
            &json!("LSP_TRANSPORT_ERROR"),
            &json!("server_exited"),
            &json!("fixture crashed")
        ]
    );
}

#[test]
fn dropping_the_service_shuts_down_its_servers() {
    let fixture = fixture();
    let server = answering_server(&fixture);
    let service = fixture.service_with(&server, LspPreset::Rust);
    run(&service, definition()).unwrap();
    let running_before = server.running_count();

    drop(service);

    assert_eq!([running_before, server.running_count()], [1, 0]);
}

#[test]
fn result_uris_outside_the_workspace_are_never_read() {
    let fixture = fixture();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.rs"), "fn outside_secret() {}\n").unwrap();
    let secret = super::uri::path_uri(&outside.path().join("secret.rs"));
    let server = FakeServer::new(move |_, _| {
        Reply::Result(json!([{"uri": secret, "range": range(0, 3, 17)}]))
    });
    let service = fixture.service_with(&server, LspPreset::Rust);

    let result = run(&service, definition()).unwrap();

    assert_eq!(result["fallback_reason"]["code"], "OUTSIDE_ROOT");
    assert!(!result.to_string().contains("outside_secret"));
}

#[cfg(unix)]
#[test]
fn result_uris_naming_a_symlink_out_of_the_workspace_are_never_read() {
    let fixture = fixture();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.rs"), "fn outside_secret() {}\n").unwrap();
    std::os::unix::fs::symlink(
        outside.path().join("secret.rs"),
        fixture.path("src/link.rs"),
    )
    .unwrap();
    let link = fixture.uri("src/link.rs");
    let server = FakeServer::new(move |method, _| match method {
        "textDocument/rename" => Reply::Result(json!({"changes": {
            link.clone(): [{"range": range(0, 3, 17), "newText": "renamed"}]
        }})),
        _ => Reply::Result(json!([{"uri": link, "range": range(0, 3, 17)}])),
    });
    let service = fixture.service_with(&server, LspPreset::Rust);

    let definition = run(&service, definition()).unwrap();
    let rename = run(&service, rename()).unwrap_err();

    assert_eq!(
        definition["fallback_reason"]["code"],
        "INVALID_LSP_RESPONSE"
    );
    assert!(!definition.to_string().contains("outside_secret"));
    assert_eq!(rename.0.code, "INVALID_LSP_RESPONSE");
}

#[cfg(unix)]
#[test]
fn a_target_replaced_by_a_symlink_is_never_read() {
    let fixture = fixture();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.rs"), "fn outside_secret() {}\n").unwrap();
    std::fs::remove_file(fixture.path("src/lib.rs")).unwrap();
    std::os::unix::fs::symlink(outside.path().join("secret.rs"), fixture.path("src/lib.rs"))
        .unwrap();
    let service = fixture.service();

    let error = run(&service, definition()).unwrap_err();

    assert_eq!(error.0.code, "INTELLIGENCE_STALE");
    assert!(!error.to_string().contains("outside_secret"));
}

#[test]
fn rename_ranges_use_the_negotiated_utf8_encoding() {
    let fixture = Fixture::new(&[("sample.py", "def café():\n    return 1\n")]);
    let uri = fixture.uri("sample.py");
    let server = FakeServer::new(move |_, _| {
        Reply::Result(json!({"changes": {
            uri.as_str(): [{"range": range(0, 4, 9), "newText": "bistro"}]
        }}))
    })
    .with_capability("positionEncoding", json!("utf-8"));
    let service = fixture.service_with(&server, LspPreset::Python);

    let report = run(
        &service,
        json!({"operation": "rename_preview", "path": "sample.py", "line": 1, "column": 8, "new_name": "bistro"}),
    )
    .unwrap();

    assert_eq!(
        report["changes"][0]["new_text"],
        "def bistro():\n    return 1\n"
    );
    // Public column 8 (UTF-16, just after "café") is UTF-8 byte 9 on the wire.
    assert_eq!(
        server.messages("textDocument/rename")[0]["params"]["position"],
        json!({"line": 0, "character": 9})
    );
}
