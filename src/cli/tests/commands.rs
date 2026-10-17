//! The `doctor`, `init` and `install` commands and argument parsing.

use super::super::args::{Cli, Command, Transport};
use super::super::config::{config_relative_path, load_config};
use super::super::doctor::doctor_checks;
use super::super::install::{run_init, run_install_with_io};
use clap::Parser;
use serde_json::{json, Value};
use std::{io, path::PathBuf};

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
        serde_json::from_str(include_str!("../../../config.example.json")).unwrap();
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
    let daemon = load_config(&config_path).unwrap().daemon;
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
