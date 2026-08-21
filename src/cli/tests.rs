mod ngrok;

#[cfg(test)]
fn test_bash_executable() -> String {
    #[cfg(windows)]
    {
        for root in [
            std::env::var_os("ProgramW6432"),
            std::env::var_os("ProgramFiles"),
        ]
        .into_iter()
        .flatten()
        {
            let candidate = PathBuf::from(root).join("Git").join("bin").join("bash.exe");
            if candidate.is_file() {
                return candidate.to_string_lossy().into_owned();
            }
        }
    }
    "bash".to_owned()
}

use super::args::Transport;
#[cfg(windows)]
use super::config::validate_token_permissions;
use super::config::{config_relative_path, default_port, load_config, load_or_create_bearer_token};
use super::doctor::doctor_checks;
use super::install::{run_init, run_install_with_io};
use super::server::{live, resolve_tool_access, AppState};
use super::*;
use axum::{extract::State, Json};
use codeweave_rust::{
    contracts,
    manager::{self, Application},
    model, tools,
};
use serde_json::{json, Value};
#[cfg(windows)]
use std::process::Command as ProcessCommand;
use std::{
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

fn cli_for(config: PathBuf) -> Cli {
    Cli {
        command: Some(Command::Doctor),
        config,
        transport: Transport::Http,
        host: None,
        port: None,
    }
}

#[tokio::test]
async fn doctor_checks_initialize_the_configured_workspace() {
    let temp = tempfile::tempdir().unwrap();
    let config_path = temp.path().join("config.json");
    let init_cli = Cli {
        command: Some(Command::Init {
            path: Some(PathBuf::from(env!("CARGO_MANIFEST_DIR"))),
            force: false,
        }),
        config: config_path.clone(),
        transport: Transport::Http,
        host: None,
        port: None,
    };
    run_init(
        &init_cli,
        Some(PathBuf::from(env!("CARGO_MANIFEST_DIR"))),
        false,
    )
    .unwrap();

    let checks = doctor_checks(&cli_for(config_path)).await;
    assert!(
        checks
            .iter()
            .find(|check| check.name == "workspace")
            .unwrap()
            .ok
    );
    let index = checks.iter().find(|check| check.name == "index").unwrap();
    assert!(index.ok, "{}", index.detail);
    assert!(index.detail.contains("files indexed"));
}

#[tokio::test]
async fn doctor_checks_reports_missing_workspace_without_panicking() {
    let temp = tempfile::tempdir().unwrap();
    let config_path = temp.path().join("config.json");
    let mut template: Value =
        serde_json::from_str(include_str!("../../config.example.json")).unwrap();
    template["workspace"]["path"] = json!(temp.path().join("does-not-exist").to_string_lossy());
    std::fs::write(&config_path, serde_json::to_vec(&template).unwrap()).unwrap();

    let checks = doctor_checks(&cli_for(config_path)).await;
    let workspace = checks
        .iter()
        .find(|check| check.name == "workspace")
        .unwrap();
    assert!(!workspace.ok);
    assert!(workspace.detail.contains("WORKSPACE_NOT_FOUND"));
}

#[test]
fn init_writes_a_real_config_and_refuses_accidental_overwrite() {
    let temp = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("main.rs"), "fn main() {}\n").unwrap();
    let config_path = temp.path().join("config.json");
    let cli = Cli {
        command: Some(Command::Init {
            path: Some(project.path().to_path_buf()),
            force: false,
        }),
        config: config_path.clone(),
        transport: Transport::Http,
        host: None,
        port: None,
    };

    run_init(&cli, Some(project.path().to_path_buf()), false).unwrap();
    let (_, root) = load_config(&config_path).unwrap();
    let daemon: codeweave_rust::model::DaemonConfig = serde_json::from_value(root).unwrap();
    assert_eq!(
        PathBuf::from(daemon.workspace.path),
        project.path().canonicalize().unwrap()
    );
    assert!(config_relative_path(&config_path, ".mcp-token").is_file());
    assert!(run_init(&cli, Some(project.path().to_path_buf()), false).is_err());
}

#[tokio::test]
async fn interactive_install_defaults_to_stdio_and_prints_client_config() {
    let temp = tempfile::tempdir().unwrap();
    let config_path = temp.path().join("config.json");
    let cli = Cli {
        command: Some(Command::Install {
            path: Some(PathBuf::from(env!("CARGO_MANIFEST_DIR"))),
            force: false,
        }),
        config: config_path.clone(),
        transport: Transport::Http,
        host: None,
        port: None,
    };
    let mut input = io::Cursor::new(b"\n");
    let mut output = Vec::new();

    run_install_with_io(
        &cli,
        Some(PathBuf::from(env!("CARGO_MANIFEST_DIR"))),
        false,
        &mut input,
        &mut output,
    )
    .await
    .unwrap();

    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("Installation complete"));
    assert!(output.contains("\"stdio\""));
    assert!(output.contains("\"command\""));
    assert!(output.contains("port — skipped for stdio transport"));
    assert!(config_path.is_file());
}

#[test]
fn install_subcommand_is_discoverable() {
    let cli = Cli::parse_from(["codeweave", "install", "--path", "."]);
    assert!(matches!(cli.command, Some(Command::Install { .. })));
}

#[test]
fn omitted_subcommand_serves_with_documented_defaults() {
    let cli = Cli::parse_from([
        "codeweave",
        "--transport",
        "http",
        "--config",
        "config.json",
    ]);
    assert!(cli.command.is_none());
    assert_eq!(cli.config, PathBuf::from("config.json"));
    assert!(matches!(cli.transport, Transport::Http));
    assert!(cli.host.is_none());
    assert!(cli.port.is_none());
}

#[test]
fn explicit_serve_subcommand_is_supported() {
    let cli = Cli::parse_from(["codeweave", "serve"]);
    assert!(matches!(cli.command, Some(Command::Serve)));
}

fn full_access() -> tools::ToolAccess {
    resolve_tool_access()
}

async fn prepare(
    _manager: &Arc<Application>,
    _config: &Value,
    _access: &tools::ToolAccess,
    method: &str,
    input: Value,
) -> model::AppResult<Value> {
    manager::prepare_tool_request(method, input)
}

fn tool<'a>(all: &'a Value, name: &str) -> &'a Value {
    all.as_array()
        .and_then(|items| {
            items
                .iter()
                .find(|item| item.get("name").and_then(Value::as_str) == Some(name))
        })
        .expect("tool must exist")
}

#[tokio::test]
async fn live_omits_sensitive_runtime_metadata() {
    let state = AppState {
        manager: Arc::new(Application::default()),
        server: serde_json::from_value(json!({
            "authMode": "bearer",
            "idleTimeoutMs": 5000
        }))
        .unwrap(),
        token: Some(Arc::new(b"secret".to_vec())),
        tool_access: Arc::new(resolve_tool_access()),
        instance_id: Arc::from("test-instance"),
    };

    let Json(payload) = live(State(state)).await;

    assert_eq!(payload["ok"], true);
    assert_eq!(payload["idleTimeoutMs"], 5000);
    for field in ["auth", "workspace", "build"] {
        assert!(payload.get(field).is_none(), "unexpected {field} field");
    }
    assert!(!payload.to_string().contains("private"));
    assert!(!payload.to_string().contains("secret"));
}

#[test]
fn public_tool_schemas_have_the_required_shape() {
    let all = tools::full_list_payload();
    let items = all.as_array().expect("tools array");
    let expected_annotations = [
        ("workspace", true, false, true, false),
        ("code_retrieve", true, false, true, false),
        ("code_intelligence", true, false, true, false),
        ("code_write", false, false, false, false),
        ("code_replace", false, false, false, false),
        ("code_replace_range", false, false, false, false),
        ("code_insert", false, false, false, false),
        ("code_delete", false, true, false, false),
        ("code_rename", false, false, false, false),
        ("code_preview", true, false, true, false),
        ("code_transaction", false, false, false, false),
        ("git_status", true, false, true, false),
        ("git_diff", true, false, true, false),
        ("git_log", true, false, true, false),
        ("git_show", true, false, true, false),
        ("git_blame", true, false, true, false),
        ("git_preflight", true, false, true, false),
        ("git_stage", false, false, false, false),
        ("git_commit", false, false, false, false),
        ("git_restore", false, true, false, false),
        ("git_push", false, false, false, true),
        ("bash", false, true, false, true),
        ("bash_status", true, false, true, false),
        ("bash_output", true, false, true, false),
        ("bash_cancel", false, false, false, false),
    ];
    assert_eq!(items.len(), expected_annotations.len());
    for (name, read_only, destructive, idempotent, open_world) in expected_annotations {
        let annotations = &tool(&all, name)["annotations"];
        assert_eq!(annotations["readOnlyHint"], read_only, "{name}");
        assert_eq!(annotations["destructiveHint"], destructive, "{name}");
        assert_eq!(annotations["idempotentHint"], idempotent, "{name}");
        assert_eq!(annotations["openWorldHint"], open_world, "{name}");
    }

    for item in items {
        let schema = &item["inputSchema"];
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["$schema"], "http://json-schema.org/draft-07/schema#");
        let encoded = schema.to_string();
        assert!(!encoded.contains("\"allOf\""));
        assert!(!encoded.contains("\"not\""));
        assert!(!encoded.contains("\"const\""));
        let allows_change_union = matches!(
            item["name"].as_str(),
            Some("code_preview" | "code_transaction")
        );
        assert_eq!(encoded.contains("\"oneOf\""), allows_change_union);
        assert_eq!(item["execution"]["taskSupport"], "forbidden");
    }

    assert!(items.iter().all(|item| item["name"] != "code_edit"));
    assert!(items
        .iter()
        .all(|item| !matches!(item["name"].as_str(), Some("git" | "run"))));
    for item in items.iter().filter(|item| {
        item["name"]
            .as_str()
            .is_some_and(|name| name.starts_with("git_") || name.starts_with("bash"))
    }) {
        assert_eq!(item["inputSchema"]["additionalProperties"], false);
    }
    let action_multiplexers = items
        .iter()
        .filter(|item| item["inputSchema"]["properties"]["action"]["enum"].is_array())
        .map(|item| item["name"].as_str().expect("tool name"))
        .collect::<Vec<_>>();
    assert_eq!(action_multiplexers, ["workspace"]);
    assert_eq!(tool(&all, "workspace")["annotations"]["readOnlyHint"], true);
    let workspace_properties = &tool(&all, "workspace")["inputSchema"]["properties"];
    assert_eq!(workspace_properties["since_generation"]["type"], "integer");
    assert_eq!(workspace_properties["since_generation"]["minimum"], 0);
    assert_eq!(workspace_properties["source"]["type"], "string");
    assert_eq!(workspace_properties["limit"]["minimum"], 1);
    assert_eq!(workspace_properties["limit"]["maximum"], 2_000);
    assert_eq!(workspace_properties["limit"]["default"], 200);
    let retrieval_operation =
        &tool(&all, "code_retrieve")["inputSchema"]["properties"]["operations"]["items"];
    assert_eq!(retrieval_operation["required"], json!(["operation"]));
    assert!(retrieval_operation["properties"].get("query").is_none());
    assert_eq!(
        retrieval_operation["properties"]["operation"]["enum"],
        json!([
            "find_file",
            "find_symbol",
            "search_text",
            "find_references",
            "symbols_overview",
            "repo_map",
            "read"
        ])
    );
    assert_eq!(
        retrieval_operation["properties"]["target"]["enum"],
        json!([
            "path",
            "handle",
            "symbol",
            "metadata",
            "bash_status",
            "bash_log",
            "continuation"
        ])
    );
    assert_eq!(retrieval_operation["additionalProperties"], false);
    assert_eq!(
        tool(&all, "code_write")["inputSchema"]["required"],
        json!(["path", "content"])
    );
    assert_eq!(
        tool(&all, "code_replace")["inputSchema"]["required"],
        json!(["path", "old_text", "new_text"])
    );
    assert_eq!(
        tool(&all, "code_replace_range")["inputSchema"]["required"],
        json!(["path", "handle", "new_text"])
    );
    assert_eq!(
        tool(&all, "code_insert")["inputSchema"]["required"],
        json!(["path", "content", "anchor_symbol", "position"])
    );
    assert_eq!(
        tool(&all, "code_delete")["inputSchema"]["required"],
        json!(["path"])
    );
    assert_eq!(
        tool(&all, "code_rename")["inputSchema"]["required"],
        json!(["path", "to"])
    );
    assert_eq!(
        tool(&all, "code_preview")["inputSchema"]["required"],
        json!(["changes"])
    );
    assert_eq!(
        tool(&all, "code_transaction")["inputSchema"]["required"],
        json!(["changes"])
    );
    assert_eq!(
        tool(&all, "bash")["inputSchema"]["required"],
        json!(["command"])
    );
    assert_eq!(
        tool(&all, "bash")["inputSchema"]["properties"]["command"]["minLength"],
        1
    );
    assert!(items
        .iter()
        .all(|item| !item["name"].as_str().unwrap().starts_with("task_")));
    assert_eq!(
        tool(&all, "git_restore")["inputSchema"]["required"],
        json!(["paths", "confirm"])
    );
    // D2: git_push is gated on confirm=true exactly like git_restore.
    assert_eq!(
        tool(&all, "git_push")["inputSchema"]["required"],
        json!(["confirm"])
    );
}

#[tokio::test]
async fn prepare_normalizes_mutation_inputs() {
    let manager = Arc::new(Application::default());
    let config = json!({"workspace": {"path": "/repo"}});

    let replace = prepare(
        &manager,
        &config,
        &full_access(),
        "code_replace",
        json!({
            "path": "src/main.rs",
            "old_text": "old",
            "new_text": "new",
            "expected_replacements": 1
        }),
    )
    .await
    .unwrap();
    assert_eq!(replace["changes"][0]["kind"], "replace");
    assert_eq!(replace["changes"][0]["path"], "src/main.rs");
    assert!(replace.get("old_text").is_none());

    let replace_range = prepare(
        &manager,
        &config,
        &full_access(),
        "code_replace_range",
        json!({
            "path": "src/main.rs",
            "handle": "range_handle",
            "new_text": "replacement"
        }),
    )
    .await
    .unwrap();
    assert_eq!(replace_range["changes"][0]["kind"], "replace_range");
    assert_eq!(replace_range["changes"][0]["handle"], "range_handle");

    for (method, action) in [
        ("git_status", "status"),
        ("git_diff", "diff"),
        ("git_log", "log"),
        ("git_show", "show"),
        ("git_blame", "blame"),
        ("git_preflight", "preflight"),
        ("git_stage", "stage"),
        ("git_commit", "commit"),
        ("git_restore", "restore"),
        ("git_push", "push"),
    ] {
        let prepared = prepare(&manager, &config, &full_access(), method, json!({}))
            .await
            .unwrap();
        assert_eq!(prepared["action"], action, "{method}");
    }

    let bash = prepare(
        &manager,
        &config,
        &full_access(),
        "bash",
        json!({"command": "printf test"}),
    )
    .await
    .unwrap();
    assert!(bash.get("action").is_none());
    for (method, input) in [
        ("bash_status", json!({"run_id": "run_test"})),
        (
            "bash_output",
            json!({"run_id": "run_test", "stream": "stderr"}),
        ),
        ("bash_cancel", json!({"run_id": "run_test"})),
    ] {
        let prepared = prepare(&manager, &config, &full_access(), method, input)
            .await
            .unwrap();
        assert!(prepared.get("action").is_none(), "{method}");
    }
    for (method, input) in [
        ("bash", json!({"command": "  "})),
        ("bash", json!({"command": "printf test", "unknown": true})),
        (
            "bash_status",
            json!({"run_id": "run_test", "action": "cancel"}),
        ),
    ] {
        let error = contracts::normalize_bash_request(method, &input).unwrap_err();
        assert_eq!(error.0.code, "INVALID_BASH_REQUEST", "{method}");
    }
}

#[tokio::test]
async fn public_bash_payloads_dispatch_after_preparation() {
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    assert!(std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(root.path())
        .status()
        .unwrap()
        .success());
    let manager = Arc::new(Application::default());
    let config = json!({
        "configVersion": 2,
        "workspace": {"path": root.path(), "artifactPaths": []},
        "policy": {
            "maxFileBytes": 1000000,
            "maxContextChars": 50000,
            "maxSearchResults": 100,
            "bash": {
                "executable": test_bash_executable(),
                "defaultTimeoutMs": 120000,
                "maxTimeoutMs": 300000,
                "maxOutputChars": 30000
            }
        },
        "cacheRoot": cache.path()
    });
    manager.dispatch("initialize", &config).await.unwrap();

    for input in [
        json!({"command": "printf command-only"}),
        json!({"command": "printf command-with-timeout", "timeout_ms": 5000}),
    ] {
        let prepared = prepare(&manager, &config, &full_access(), "bash", input)
            .await
            .unwrap();
        let result = manager.dispatch("bash", &prepared).await.unwrap();

        assert_eq!(result["status"], "succeeded");
        assert_eq!(result["exit_code"], 0);
    }
}

#[tokio::test]
async fn narrow_write_tool_dispatches_through_transactional_engine() {
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    assert!(std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(root.path())
        .status()
        .unwrap()
        .success());
    let manager = Arc::new(Application::default());
    let config = json!({
        "configVersion": 2,
        "workspace": {"path": root.path(), "artifactPaths": []},
        "policy": {
            "maxFileBytes": 1000000,
            "maxContextChars": 50000,
            "maxSearchResults": 100,
            "bash": {"executable": test_bash_executable()}
        },
        "cacheRoot": cache.path()
    });
    manager.dispatch("initialize", &config).await.unwrap();

    let prepared = prepare(
        &manager,
        &config,
        &full_access(),
        "code_write",
        json!({
            "path": "created.txt",
            "content": "created through code_write\n"
        }),
    )
    .await
    .unwrap();
    let result = manager.dispatch("code_write", &prepared).await.unwrap();

    assert_eq!(result["applied"], true);
    assert!(result["phase_ms"]["commit"].is_number());
    assert_eq!(
        std::fs::read_to_string(root.path().join("created.txt")).unwrap(),
        "created through code_write\n"
    );

    let preview = prepare(
        &manager,
        &config,
        &full_access(),
        "code_preview",
        json!({
            "changes": [{
                "kind": "create",
                "path": "preview.txt",
                "content": "preview only\n"
            }]
        }),
    )
    .await
    .unwrap();
    assert_eq!(preview["preview"], true);
    let preview_result = manager.dispatch("code_preview", &preview).await.unwrap();
    assert_eq!(preview_result["preview"], true);
    assert!(!root.path().join("preview.txt").exists());

    let syntax_error = prepare(
        &manager,
        &config,
        &full_access(),
        "code_preview",
        json!({
            "changes": [{
                "kind": "create",
                "path": "broken.rs",
                "content": "fn broken(\n"
            }]
        }),
    )
    .await
    .unwrap();
    let syntax_result = manager
        .dispatch("code_preview", &syntax_error)
        .await
        .unwrap_err();
    assert_eq!(syntax_result.0.code, "SYNTAX_ERROR");
    assert!(!root.path().join("broken.rs").exists());

    let transaction = prepare(
        &manager,
        &config,
        &full_access(),
        "code_transaction",
        json!({
            "changes": [
                {
                    "kind": "create",
                    "path": "tx-one.txt",
                    "content": "one\n"
                },
                {
                    "kind": "create",
                    "path": "tx-two.txt",
                    "content": "two\n"
                }
            ]
        }),
    )
    .await
    .unwrap();
    let transaction_result = manager
        .dispatch("code_transaction", &transaction)
        .await
        .unwrap();
    assert_eq!(transaction_result["applied"], true);
    assert_eq!(
        std::fs::read_to_string(root.path().join("tx-one.txt")).unwrap(),
        "one\n"
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("tx-two.txt")).unwrap(),
        "two\n"
    );
}

#[tokio::test]
async fn prepare_rejects_unknown_request_fields() {
    let configured = tempfile::tempdir().unwrap();
    std::fs::write(
        configured.path().join("configured.rs"),
        "fn configured() {}\n",
    )
    .unwrap();
    let manager = Arc::new(Application::default());
    let public_config = json!({"workspace": {"path": configured.path()}});
    let error = prepare(
        &manager,
        &public_config,
        &full_access(),
        "code_retrieve",
        json!({
            "unexpected_field": "value",
            "operations": [{"operation": "find_file", "name": "configured.rs"}]
        }),
    )
    .await
    .unwrap_err();
    assert_eq!(error.0.code, "UNKNOWN_TOOL_FIELD");
}

#[test]
fn relative_token_path_is_resolved_from_config_directory() {
    let config = Path::new("C:/path/to/codeweave/config.json");
    let resolved = config_relative_path(config, ".mcp-token");
    assert_eq!(resolved, PathBuf::from("C:/path/to/codeweave/.mcp-token"));
}

#[cfg(windows)]
#[test]
fn generated_windows_token_has_a_protected_explicit_acl() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("token");

    let token = load_or_create_bearer_token(&path).unwrap();

    assert!(!token.is_empty());
    validate_token_permissions(&path).unwrap();
}

#[cfg(windows)]
#[test]
fn inherited_windows_token_acl_is_rejected() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("token");
    std::fs::write(&path, "not-private").unwrap();

    assert!(validate_token_permissions(&path).is_err());
}

#[cfg(unix)]
#[test]
fn generated_token_is_exclusive_and_private() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("token");
    let token = load_or_create_bearer_token(&path).unwrap();
    assert!(!token.is_empty());
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );

    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(load_or_create_bearer_token(&path).is_err());
}

/// `config.example.json` must deserialize through the *real* config path
/// (`load_config` + `DaemonConfig`) and preserve the runtime contracts the
/// shipped template advertises, including the expected port and safe Host policy.
#[test]
fn shipped_config_example_deserializes_and_matches_runtime_contracts() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("config.example.json");
    let (server, root) = load_config(&path).expect("config.example.json must load");

    // Port advertised in the example must equal the code default so a copy-paste
    // start hits the port the README and connectors expect.
    assert_eq!(server.port, default_port());
    assert_eq!(server.port, 8813);
    assert_eq!(
        server.allowed_hosts,
        ["localhost", "127.0.0.1", "::1"].map(str::to_owned)
    );
    assert!(!server.allowed_hosts.iter().any(|host| host == "*"));

    // The remainder must deserialize as the daemon config the server actually
    // uses at startup (load_config injects cache_root).
    let daemon: model::DaemonConfig =
        serde_json::from_value(root).expect("example must deserialize as DaemonConfig");

    // foregroundBudgetMs is present and non-zero (auto-promotion enabled), and
    // the example's bash budget matches the documented code default.
    assert_eq!(daemon.policy.bash.foreground_budget_ms, 20_000);
}

#[test]
fn shipped_config_example_resolves_fixed_tool_contract() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("config.example.json");
    let (_server, _root) = load_config(&path).expect("config.example.json must load");
    let access = resolve_tool_access();
    assert!(tools::ToolAccess::is_known_tool("bash"));
    assert_eq!(access.list_payload().as_array().unwrap().len(), 25);
}
