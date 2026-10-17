mod ngrok;

use super::args::Transport;
#[cfg(windows)]
use super::config::validate_token_permissions;
use super::config::{config_relative_path, default_port, load_config, load_or_create_bearer_token};
use super::doctor::doctor_checks;
use super::install::{run_init, run_install_with_io};
use super::server::{live, AppState};
use super::*;
use axum::{extract::State, Json};
use codeweave_rust::{manager::Application, model};
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
