use super::*;
use crate::model::{
    BashConfig, DaemonConfig, IntelligenceSettings, PolicyConfig, WorkspaceSettings,
};
use crate::test_bash_executable;
use serde_json::json;
use std::path::Path;
use tempfile::{tempdir, TempDir};

#[test]
fn advertised_tools_match_the_published_wire_fixture() {
    // The fixture is the tools/list payload served before the registry
    // generated schemas from request types; clients must see no difference.
    let expected: Value =
        serde_json::from_str(include_str!("../../tests/fixtures/tools_list.json")).unwrap();

    assert_eq!(serde_json::to_value(mcp_tools()).unwrap(), expected);
}

#[test]
fn find_resolves_registered_names_only() {
    assert_eq!(find("git_diff").map(|tool| tool.title), Some("Git Diff"));
    assert_eq!(
        find("bash_output").map(|tool| tool.title),
        Some("Bash Run Output")
    );
    assert!(find("git").is_none());
}

struct Fixture {
    root: TempDir,
    _cache: TempDir,
    app: Arc<Application>,
}

async fn fixture() -> Fixture {
    let root = tempdir().unwrap();
    let cache = tempdir().unwrap();
    let status = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(root.path())
        .status()
        .expect("git must be available for tests");
    assert!(status.success());
    std::fs::write(root.path().join("lib.rs"), "fn value() -> i32 { 1 }\n").unwrap();
    let app = Arc::new(Application::default());
    app.initialize(config(root.path(), cache.path()))
        .await
        .unwrap();
    Fixture {
        root,
        _cache: cache,
        app,
    }
}

fn config(root: &Path, cache: &Path) -> DaemonConfig {
    DaemonConfig {
        config_version: 2,
        workspace: WorkspaceSettings {
            path: root.to_string_lossy().into_owned(),
            artifact_paths: Vec::new(),
            exclude_paths: Vec::new(),
        },
        intelligence: IntelligenceSettings::default(),
        policy: PolicyConfig {
            max_file_bytes: 1_000_000,
            max_context_chars: 50_000,
            max_search_results: 100,
            bash: BashConfig {
                executable: test_bash_executable(),
                default_timeout_ms: 120_000,
                foreground_budget_ms: 20_000,
                max_timeout_ms: 300_000,
                max_output_chars: 30_000,
            },
        },
        cache_root: cache.to_string_lossy().into_owned(),
    }
}

impl Fixture {
    async fn call(&self, name: &str, arguments: Value) -> AppResult<Value> {
        find(name)
            .expect("registered tool")
            .call(Arc::clone(&self.app), arguments)
            .await
    }

    async fn error(&self, name: &str, arguments: Value) -> (String, String, Value) {
        let error = self.call(name, arguments).await.unwrap_err();
        (
            error.0.code,
            error.0.message,
            error.0.details.unwrap_or(Value::Null),
        )
    }
}

#[tokio::test]
async fn unknown_top_level_fields_are_rejected_with_the_advertised_field_list() {
    let fixture = fixture().await;

    let (code, message, details) = fixture
        .error("git_log", json!({"limit": 5, "author": "someone"}))
        .await;

    assert_eq!(code, "UNKNOWN_TOOL_FIELD");
    assert_eq!(message, "tool 'git_log' received unsupported fields");
    assert_eq!(
        details,
        json!({"unknown": ["author"], "allowed": ["limit"]})
    );
}

#[tokio::test]
async fn out_of_range_and_wrongly_typed_arguments_are_invalid() {
    let fixture = fixture().await;

    let (code, message, _) = fixture.error("git_log", json!({"limit": 500})).await;
    assert_eq!(code, "INVALID_ARGUMENT");
    assert_eq!(message, "limit must be an integer from 1 to 200");

    let (code, _, details) = fixture
        .error("workspace", json!({"action": "rebuild"}))
        .await;
    assert_eq!(code, "INVALID_ARGUMENT");
    assert_eq!(details["field"], "action");
}

#[tokio::test]
async fn null_arguments_count_as_omitted() {
    let fixture = fixture().await;

    let changes = fixture
        .call(
            "workspace",
            json!({"action": "changes", "since_generation": null, "source": null, "limit": null}),
        )
        .await
        .unwrap();

    assert_eq!(changes["mutations"], json!([]));
}

#[tokio::test]
async fn bash_arguments_are_rejected_with_the_bash_request_code() {
    let fixture = fixture().await;

    let blank = fixture.error("bash", json!({"command": "   "})).await;
    let missing = fixture.error("bash_status", json!({})).await;
    let stream = fixture
        .error("bash_output", json!({"run_id": "r1", "stream": "both"}))
        .await;
    let unknown = fixture
        .error("bash", json!({"command": "true", "shell": "zsh"}))
        .await;

    assert_eq!(
        (blank.0.as_str(), blank.1.as_str()),
        (
            "INVALID_BASH_REQUEST",
            "bash field 'command' must be a non-empty string"
        )
    );
    assert_eq!(missing.0, "INVALID_BASH_REQUEST");
    assert_eq!(missing.2["missing"], json!(["run_id"]));
    assert_eq!(stream.0, "INVALID_BASH_REQUEST");
    assert_eq!(stream.2["field"], "stream");
    assert_eq!(unknown.0, "UNKNOWN_TOOL_FIELD");
}

#[tokio::test]
async fn a_narrow_write_tool_writes_through_the_edit_engine() {
    let fixture = fixture().await;

    let result = fixture
        .call(
            "code_write",
            json!({"path": "src/new.rs", "content": "pub fn added() {}\n"}),
        )
        .await
        .unwrap();

    assert_eq!(result["applied"], true);
    assert_eq!(
        std::fs::read_to_string(fixture.root.path().join("src/new.rs")).unwrap(),
        "pub fn added() {}\n"
    );
}

#[tokio::test]
async fn narrow_edit_tools_report_fields_with_the_change_codes() {
    let fixture = fixture().await;

    let missing = fixture
        .error("code_replace", json!({"path": "lib.rs", "new_text": "2"}))
        .await;
    let invalid = fixture
        .error(
            "code_replace",
            json!({"path": "lib.rs", "old_text": "1", "new_text": "2", "expected_replacements": 0}),
        )
        .await;

    assert_eq!(missing.0, "MISSING_CHANGE_FIELD");
    assert_eq!(missing.2["missing"], json!(["old_text"]));
    assert_eq!(invalid.0, "INVALID_CHANGE_FIELD");
    assert_eq!(
        invalid.1,
        "expected_replacements must be an integer of at least 1"
    );
}

#[tokio::test]
async fn preview_plans_without_writing_and_transaction_applies() {
    let fixture = fixture().await;
    let changes = json!([{
        "kind": "replace",
        "path": "lib.rs",
        "old_text": "{ 1 }",
        "new_text": "{ 2 }"
    }]);

    let preview = fixture
        .call("code_preview", json!({"changes": changes.clone()}))
        .await
        .unwrap();
    let before = std::fs::read_to_string(fixture.root.path().join("lib.rs")).unwrap();
    let applied = fixture
        .call("code_transaction", json!({"changes": changes}))
        .await
        .unwrap();
    let after = std::fs::read_to_string(fixture.root.path().join("lib.rs")).unwrap();

    assert_eq!(preview["applied"], false);
    assert_eq!(before, "fn value() -> i32 { 1 }\n");
    assert_eq!(applied["applied"], true);
    assert_eq!(after, "fn value() -> i32 { 2 }\n");
}

#[tokio::test]
async fn change_items_are_rejected_with_the_change_codes() {
    let fixture = fixture().await;

    let unsupported = fixture
        .error(
            "code_transaction",
            json!({"changes": [{"kind": "chmod", "path": "lib.rs"}]}),
        )
        .await;
    let unknown = fixture
        .error(
            "code_preview",
            json!({"changes": [{"kind": "delete", "path": "lib.rs", "mode": "force"}]}),
        )
        .await;

    assert_eq!(unsupported.0, "UNSUPPORTED_CHANGE_KIND");
    assert_eq!(unknown.0, "UNKNOWN_CHANGE_FIELD");
    assert_eq!(unknown.2["unknown"], json!(["mode"]));
}

#[tokio::test]
async fn bash_runs_a_command_with_and_without_a_timeout() {
    let fixture = fixture().await;

    for arguments in [
        json!({"command": "printf command-only"}),
        json!({"command": "printf command-with-timeout", "timeout_ms": 5000}),
    ] {
        let result = fixture.call("bash", arguments).await.unwrap();

        assert_eq!(result["status"], "succeeded");
        assert_eq!(result["exit_code"], 0);
    }
}

#[tokio::test]
async fn preview_reports_syntax_errors_without_writing() {
    let fixture = fixture().await;

    let (code, _, _) = fixture
        .error(
            "code_preview",
            json!({"changes": [{"kind": "create", "path": "broken.rs", "content": "fn broken(\n"}]}),
        )
        .await;

    assert_eq!(code, "SYNTAX_ERROR");
    assert!(!fixture.root.path().join("broken.rs").exists());
}

#[tokio::test]
async fn a_transaction_applies_every_change() {
    let fixture = fixture().await;

    let result = fixture
        .call(
            "code_transaction",
            json!({"changes": [
                {"kind": "create", "path": "tx-one.txt", "content": "one\n"},
                {"kind": "create", "path": "tx-two.txt", "content": "two\n"}
            ]}),
        )
        .await
        .unwrap();

    assert_eq!(result["applied"], true);
    let read = |name: &str| std::fs::read_to_string(fixture.root.path().join(name)).unwrap();
    assert_eq!(
        (read("tx-one.txt"), read("tx-two.txt")),
        ("one\n".to_owned(), "two\n".to_owned())
    );
}
