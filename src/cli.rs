mod mcp_transport;

use anyhow::{Context, Result};
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use clap::{Parser, Subcommand, ValueEnum};
#[cfg(test)]
use codeweave_rust::contracts;
use codeweave_rust::{manager, model, security, tools};
use manager::Application;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    io::{self, Write},
    net::TcpListener,
    path::{Path, PathBuf},
    process::Command as ProcessCommand,
    sync::Arc,
};
use subtle::ConstantTimeEq;
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

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

const SERVER_NAME: &str = "codeweave-rust";

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Transport {
    Http,
    Stdio,
}

#[derive(Parser, Debug)]
#[command(version, about = "Rust-only CodeWeave MCP server")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    #[arg(long, global = true, default_value = "config.json")]
    config: PathBuf,
    #[arg(long, global = true, value_enum, default_value_t = Transport::Http)]
    transport: Transport,
    #[arg(long, global = true)]
    host: Option<String>,
    #[arg(long, global = true)]
    port: Option<u16>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the MCP server.
    Serve,
    /// Create config.json and a bearer token for a project, then print the
    /// connector URL and ChatGPT/Claude next steps.
    Init {
        /// Project directory to serve. Prompted for when omitted.
        #[arg(long)]
        path: Option<PathBuf>,
        /// Overwrite an existing config.json instead of refusing.
        #[arg(long)]
        force: bool,
    },
    /// Validate a config end-to-end (config, workspace, git, bash, port, token,
    /// index). Exits non-zero if any check fails.
    Doctor,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ServerConfig {
    #[serde(default = "default_host")]
    host: String,
    #[serde(default = "default_port")]
    port: u16,
    #[serde(default = "default_auth")]
    auth_mode: String,
    #[serde(default = "default_token")]
    token_file: String,
    #[serde(default)]
    allowed_hosts: Vec<String>,
    #[serde(default)]
    allowed_origins: Vec<String>,
    /// Idle HTTP keep-alive timeout in milliseconds. Hyper's equivalent of
    /// Uvicorn's `timeout_keep_alive`: an idle kept-alive connection is closed
    /// after this long, so a tunnel/connector does not hold the socket open to
    /// its own ~90s deadline and report that as the connection lifetime. `0`
    /// disables the bound (connections stay open until the peer closes them).
    #[serde(default = "default_idle_timeout_ms")]
    idle_timeout_ms: u64,
}
fn default_host() -> String {
    "127.0.0.1".into()
}
fn default_port() -> u16 {
    8813
}
fn default_auth() -> String {
    "bearer".into()
}
fn default_token() -> String {
    ".mcp-token".into()
}
fn default_idle_timeout_ms() -> u64 {
    5000
}

fn validate_auth_mode(auth_mode: &str) -> Result<()> {
    match auth_mode {
        "bearer" | "none" => Ok(()),
        unsupported => anyhow::bail!(
            "unsupported server.authMode '{unsupported}'; expected 'bearer' or 'none'"
        ),
    }
}

#[derive(Clone)]
struct AppState {
    manager: Arc<Application>,
    server: ServerConfig,
    token: Option<Arc<Vec<u8>>>,
    tool_access: Arc<tools::ToolAccess>,
    instance_id: Arc<str>,
}

fn resolve_tool_access() -> tools::ToolAccess {
    tools::fixed_access()
}

fn load_config(path: &Path) -> Result<(ServerConfig, Value)> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut root: Value = serde_json::from_str(&text).context("parsing config JSON")?;
    let server: ServerConfig =
        serde_json::from_value(root.get("server").cloned().unwrap_or_else(|| json!({})))?;
    let object = root
        .as_object_mut()
        .context("config root must be an object")?;
    object.entry("cacheRoot").or_insert_with(|| {
        let base = path.parent().unwrap_or_else(|| Path::new("."));
        Value::String(base.join(".codeweave-cache").to_string_lossy().into_owned())
    });
    object.remove("server");
    object.remove("rust");
    let parsed = model::parse_daemon_config(&root)
        .map_err(|error| anyhow::anyhow!(error.0.message))?;
    if parsed.config_version != 2 {
        anyhow::bail!(
            "unsupported configVersion {}; configVersion must be 2",
            parsed.config_version
        );
    }
    Ok((server, serde_json::to_value(parsed)?))
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("codeweave_rust=info,tower_http=info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .init();
}

fn config_relative_path(config_path: &Path, configured_path: &str) -> PathBuf {
    let configured = PathBuf::from(configured_path);
    if configured.is_absolute() {
        configured
    } else {
        config_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(configured)
    }
}

fn is_loopback(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "::1" | "localhost")
}

fn authorized(headers: &HeaderMap, state: &AppState) -> bool {
    let Some(expected) = &state.token else {
        return true;
    };
    let supplied = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("")
        .as_bytes();
    supplied.len() == expected.len() && bool::from(supplied.ct_eq(expected.as_slice()))
}
fn tool_result(value: Value) -> Value {
    let structured = if value.is_object() {
        value
    } else {
        json!({"value": value})
    };
    let text = serde_json::to_string(&structured).unwrap_or_else(|_| "{}".into());
    json!({
        "content": [{"type": "text", "text": text}],
        "structuredContent": structured
    })
}

fn tool_failure(error: model::AppError) -> Value {
    let body = error.0;
    let retryable = body
        .details
        .as_ref()
        .and_then(|details| details.get("retryable"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || matches!(
            body.code.as_str(),
            "STALE_SNAPSHOT" | "STALE_FILE" | "STALE_HANDLE" | "STALE_CONTINUATION"
        );
    let retry_kind = body
        .details
        .as_ref()
        .and_then(|details| details.get("retry_kind"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| {
            if matches!(
                body.code.as_str(),
                "STALE_SNAPSHOT" | "STALE_FILE" | "STALE_HANDLE" | "STALE_CONTINUATION"
            ) {
                "retry_same_request".to_owned()
            } else if retryable {
                "retry_with_changes".to_owned()
            } else {
                "not_retryable".to_owned()
            }
        });
    let structured = json!({
        "error": {
            "code": body.code,
            "message": body.message,
            "retryable": retryable,
            "retry_kind": retry_kind,
            "details": body.details
        }
    });
    let text = serde_json::to_string(&structured).unwrap_or_else(|_| "{}".into());
    json!({
        "content": [{"type": "text", "text": text}],
        "structuredContent": structured,
        "isError": true
    })
}

async fn live(State(state): State<AppState>) -> Json<Value> {
    Json(json!({
        "ok": true,
        "name": SERVER_NAME,
        "version": env!("CARGO_PKG_VERSION"),
        "transport": "http",
        "instanceId": state.instance_id,
        // Idle keep-alive timeout actually applied to accepted connections, so a
        // tunnel operator can confirm sockets are closed at ~5s (matching the
        // ngrok "Connections" p50/p90) rather than held to the connector deadline.
        "idleTimeoutMs": state.server.idle_timeout_ms,
        "rmcp": "1.8",
    }))
}

async fn health(State(state): State<AppState>) -> impl IntoResponse {
    match state.manager.dispatch("health", &json!({})).await {
        Ok(value) => (
            StatusCode::OK,
            Json(json!({"ok":true,"gateway_ready":true,"engine":value})),
        )
            .into_response(),
        Err(error) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"ok":false,"error":error.0})),
        )
            .into_response(),
    }
}

fn load_or_create_bearer_token(path: &Path) -> Result<String> {
    match std::fs::read_to_string(path) {
        Ok(value) => {
            validate_token_permissions(path)?;
            let token = value.trim();
            if token.is_empty() {
                anyhow::bail!("bearer token file is empty: {}", path.display());
            }
            eprintln!("bearer authentication loaded from {}", path.display());
            Ok(token.to_owned())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).with_context(|| {
                    format!("creating bearer token directory {}", parent.display())
                })?;
            }
            let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options
                .open(path)
                .with_context(|| format!("creating bearer token file {}", path.display()))?;
            file.write_all(token.as_bytes())
                .with_context(|| format!("writing bearer token file {}", path.display()))?;
            eprintln!("generated bearer token at {}", path.display());
            Ok(token)
        }
        Err(error) => Err(error).with_context(|| format!("reading token file {}", path.display())),
    }
}

#[cfg(unix)]
fn validate_token_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path)?.permissions().mode();
    if mode & 0o077 != 0 {
        anyhow::bail!(
            "bearer token file {} must not be accessible by group or other users; run chmod 600 {}",
            path.display(),
            path.display()
        );
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_token_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Check {
    name: &'static str,
    ok: bool,
    detail: String,
}

fn executable_on_path(command: &str) -> bool {
    let direct = Path::new(command);
    if direct.components().count() > 1 {
        return direct.is_file();
    }
    let extensions: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.CMD;.BAT".into())
            .split(';')
            .map(str::to_owned)
            .collect()
    } else {
        vec![String::new()]
    };
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .any(|dir| {
            extensions.iter().any(|ext| {
                dir.join(format!("{command}{ext}")).is_file() || dir.join(command).is_file()
            })
        })
}

impl Check {
    fn ok(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            ok: true,
            detail: detail.into(),
        }
    }

    fn fail(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            ok: false,
            detail: detail.into(),
        }
    }
}

/// Run the same preflight work as normal startup, retaining individual failures
/// so `doctor` can explain every actionable problem in one invocation.
async fn doctor_checks(cli: &Cli) -> Vec<Check> {
    let mut checks = Vec::new();
    let (server, config) = match load_config(&cli.config) {
        Ok(value) => {
            checks.push(Check::ok(
                "config",
                format!("parsed {}", cli.config.display()),
            ));
            value
        }
        Err(error) => {
            checks.push(Check::fail(
                "config",
                format!("{error}; fix the JSON or pass --config <path>"),
            ));
            return checks;
        }
    };
    let bash_config: model::BashConfig = serde_json::from_value(config["policy"]["bash"].clone())
        .expect("load_config already validated policy.bash");

    match validate_auth_mode(&server.auth_mode) {
        Ok(()) => checks.push(Check::ok(
            "auth",
            format!("{} authentication", server.auth_mode),
        )),
        Err(error) => checks.push(Check::fail(
            "auth",
            format!("{error}; set server.authMode to bearer or none"),
        )),
    }
    checks.push(Check::ok("tools", "fixed 25-tool contract"));

    let workspace_path = config
        .get("workspace")
        .and_then(|workspace| workspace.get("path"))
        .and_then(Value::as_str)
        .map(PathBuf::from);
    let workspace_ok = match workspace_path.as_deref() {
        Some(path) => match security::canonical_root(path) {
            Ok(root) => {
                checks.push(Check::ok("workspace", root.display().to_string()));
                true
            }
            Err(error) => {
                checks.push(Check::fail(
                    "workspace",
                    format!("{error}; set workspace.path to an existing directory"),
                ));
                false
            }
        },
        None => {
            checks.push(Check::fail(
                "workspace",
                "workspace.path is missing; set it to the project directory",
            ));
            false
        }
    };

    match ProcessCommand::new("git").arg("--version").output() {
        Ok(output) if output.status.success() => checks.push(Check::ok(
            "git",
            String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        )),
        Ok(output) => checks.push(Check::fail(
            "git",
            format!(
                "git --version exited {}; install Git and add it to PATH",
                output.status
            ),
        )),
        Err(error) => checks.push(Check::fail(
            "git",
            format!("{error}; install Git and add it to PATH"),
        )),
    }

    for (language, default_command) in [
        ("rust", "rust-analyzer"),
        ("python", "basedpyright-langserver"),
        ("typescript", "typescript-language-server"),
    ] {
        let check_name = match language {
            "rust" => "intelligence rust",
            "python" => "intelligence python",
            "typescript" => "intelligence typescript",
            _ => unreachable!("fixed language list"),
        };
        let settings = config
            .get("intelligence")
            .and_then(|value| value.get(language));
        let enabled = settings
            .and_then(|value| value.get("enabled"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !enabled {
            checks.push(Check::ok(
                check_name,
                "disabled; syntactic and lexical fallback remain available",
            ));
            continue;
        }
        let command = settings
            .and_then(|value| value.get("command"))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .unwrap_or(default_command);
        if executable_on_path(command) {
            checks.push(Check::ok(
                check_name,
                format!("{command} is available; server starts lazily"),
            ));
        } else {
            checks.push(Check::fail(check_name, format!("{command} is unavailable; install it, fix intelligence.{language}.command, or disable the adapter")));
        }
    }

    if matches!(cli.transport, Transport::Stdio) {
        checks.push(Check::ok("port", "skipped for stdio transport"));
    } else {
        let host = cli.host.as_deref().unwrap_or(&server.host);
        let port = cli.port.unwrap_or(server.port);
        match TcpListener::bind((host, port)) {
            Ok(listener) => {
                drop(listener);
                checks.push(Check::ok("port", format!("{host}:{port} is available")));
            }
            Err(error) if error.kind() == io::ErrorKind::AddrInUse => checks.push(Check::fail("port", format!("{host}:{port} is already in use; stop the other instance or change server.port"))),
            Err(error) => checks.push(Check::fail("port", format!("cannot bind {host}:{port}: {error}; verify server.host and server.port"))),
        }
    }

    if matches!(cli.transport, Transport::Http) && server.auth_mode == "bearer" {
        let token_path = config_relative_path(&cli.config, &server.token_file);
        match std::fs::read_to_string(&token_path) {
            Ok(value) if !value.trim().is_empty() => {
                match validate_token_permissions(&token_path) {
                    Ok(()) => checks.push(Check::ok(
                        "token",
                        format!("{} is present and protected", token_path.display()),
                    )),
                    Err(error) => checks.push(Check::fail("token", error.to_string())),
                }
            }
            Ok(_) => checks.push(Check::fail(
                "token",
                format!(
                    "{} is empty; delete it and run serve, or write a token",
                    token_path.display()
                ),
            )),
            Err(error) if error.kind() == io::ErrorKind::NotFound => checks.push(Check::fail(
                "token",
                format!(
                    "{} is missing; run serve once or run init",
                    token_path.display()
                ),
            )),
            Err(error) => checks.push(Check::fail(
                "token",
                format!("cannot read {}: {error}", token_path.display()),
            )),
        }
    } else {
        checks.push(Check::ok(
            "token",
            "not required for this transport/auth mode",
        ));
    }

    if workspace_ok {
        let manager = Arc::new(Application::default());
        match manager.dispatch("initialize", &config).await {
            Ok(init) => {
                let indexed = init["index_ready"].as_bool().unwrap_or(false);
                let files = init["file_count"].as_u64().unwrap_or_default();
                if indexed {
                    checks.push(Check::ok("index", format!("ready; {files} files indexed")));
                } else {
                    checks.push(Check::fail(
                        "index",
                        "initialization returned index_ready=false",
                    ));
                }
                let bash_configured = !bash_config.executable.trim().is_empty();
                let bash_available = init["bash_available"].as_bool().unwrap_or(false);
                if bash_configured && bash_available {
                    checks.push(Check::ok(
                        "bash",
                        "available (pre-probed during initialization)",
                    ));
                } else if !bash_configured {
                    checks.push(Check::fail("bash", "policy.bash.executable is empty"));
                } else {
                    checks.push(Check::fail("bash", "configured bash is unavailable; install it or update policy.bash.executable"));
                }
            }
            Err(error) => {
                checks.push(Check::fail(
                    "index",
                    format!("{error}; fix the workspace or index configuration"),
                ));
                checks.push(Check::fail(
                    "bash",
                    "not checked because initialization failed",
                ));
            }
        }
    } else {
        checks.push(Check::fail(
            "index",
            "not checked until workspace.path is fixed",
        ));
        checks.push(Check::fail(
            "bash",
            "not checked until workspace.path is fixed",
        ));
    }
    checks
}

async fn run_doctor(cli: &Cli) -> Result<()> {
    let checks = doctor_checks(cli).await;
    let failed = checks.iter().any(|check| !check.ok);
    for check in checks {
        let status = if check.ok { "ok" } else { "FAIL" };
        println!("[{status}] {} — {}", check.name, check.detail);
    }
    if failed {
        anyhow::bail!("doctor found configuration problems")
    }
    Ok(())
}

fn run_init(cli: &Cli, requested_path: Option<PathBuf>, force: bool) -> Result<()> {
    let project = match requested_path {
        Some(path) => path,
        None => {
            let cwd = std::env::current_dir().context("reading current directory")?;
            print!("Project directory [{}]: ", cwd.display());
            io::stdout().flush().context("flushing prompt")?;
            let mut input = String::new();
            io::stdin()
                .read_line(&mut input)
                .context("reading project directory")?;
            let trimmed = input.trim();
            if trimmed.is_empty() {
                cwd
            } else {
                PathBuf::from(trimmed)
            }
        }
    };
    let project = security::canonical_root(&project).map_err(|error| anyhow::anyhow!(error))?;
    if cli.config.exists() && !force {
        anyhow::bail!(
            "{} already exists; rerun with --force to replace it",
            cli.config.display()
        );
    }

    let mut template: Value = serde_json::from_str(include_str!("../config.example.json"))
        .context("parsing embedded config.example.json")?;
    template["workspace"]["path"] = Value::String(project.to_string_lossy().into_owned());
    let rendered =
        serde_json::to_string_pretty(&template).context("serializing config template")?;
    if let Some(parent) = cli
        .config
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&cli.config, format!("{rendered}\n"))
        .with_context(|| format!("writing {}", cli.config.display()))?;

    let (server, _) = load_config(&cli.config)?;
    let token_path = config_relative_path(&cli.config, &server.token_file);
    if server.auth_mode == "bearer" {
        load_or_create_bearer_token(&token_path)?;
    }
    println!(
        "Created {} for {}.",
        cli.config.display(),
        project.display()
    );
    println!("Local MCP URL: http://{}:{}/mcp", server.host, server.port);
    if server.auth_mode == "bearer" {
        println!("Origin bearer token: {}", token_path.display());
    }
    println!("Next: codeweave serve --config {}", cli.config.display());
    println!("Then follow docs/connect-chatgpt.md or docs/connect-claude.md to expose the local URL over HTTPS.");
    Ok(())
}

#[tokio::main(flavor = "multi_thread")]
pub(crate) async fn main() -> Result<()> {
    let cli = Cli::parse();
    match &cli.command {
        None | Some(Command::Serve) => {
            init_tracing();
            run_serve(cli).await
        }
        Some(Command::Init { path, force }) => {
            let (path, force) = (path.clone(), *force);
            run_init(&cli, path, force)
        }
        Some(Command::Doctor) => run_doctor(&cli).await,
    }
}

/// Load the config, initialize the repository, and start the selected transport.
async fn run_serve(cli: Cli) -> Result<()> {
    let (server, config) = load_config(&cli.config)?;
    validate_auth_mode(&server.auth_mode)?;
    let token = if matches!(cli.transport, Transport::Http) && server.auth_mode == "bearer" {
        let token_path = config_relative_path(&cli.config, &server.token_file);
        let token_value = load_or_create_bearer_token(&token_path)?;
        Some(Arc::new(token_value.into_bytes()))
    } else {
        None
    };
    let manager = Arc::new(Application::default());
    let init = manager
        .dispatch("initialize", &config)
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    tracing::info!(
        workspace = %init["workspace"]["path"].as_str().unwrap_or_default(),
        file_count = init["file_count"].as_u64().unwrap_or_default(),
        index_ready = init["index_ready"].as_bool().unwrap_or(false),
        bash_available = init["bash_available"].as_bool().unwrap_or(false),
        "repository ready before transport bind"
    );
    let tool_access = Arc::new(resolve_tool_access());
    let instance_id = manager.instance_id();
    let state = AppState {
        manager,
        server,
        token,
        tool_access,
        instance_id,
    };
    match cli.transport {
        Transport::Http => mcp_transport::run_http(state, &cli).await,
        Transport::Stdio => mcp_transport::run_stdio(state).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            serde_json::from_str(include_str!("../config.example.json")).unwrap();
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
        let daemon: crate::model::DaemonConfig = serde_json::from_value(root).unwrap();
        assert_eq!(
            PathBuf::from(daemon.workspace.path),
            project.path().canonicalize().unwrap()
        );
        assert!(config_relative_path(&config_path, ".mcp-token").is_file());
        assert!(run_init(&cli, Some(project.path().to_path_buf()), false).is_err());
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
        crate::manager::prepare_tool_request(method, input)
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
        let all = crate::tools::full_list_payload();
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
            assert!(!encoded.contains("\"oneOf\""));
            assert!(!encoded.contains("\"allOf\""));
            assert!(!encoded.contains("\"not\""));
            assert!(!encoded.contains("\"const\""));
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
                    "executable": crate::test_bash_executable(),
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
                "bash": {"executable": crate::test_bash_executable()}
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

    #[cfg(unix)]
    #[test]
    fn generated_token_is_exclusive_and_private() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("token");
        let token = load_or_create_bearer_token(&path).unwrap();
        assert!(!token.is_empty());
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);

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
            ["localhost", "127.0.0.1", "::1", "codeweave.example.com"].map(str::to_owned)
        );
        assert!(!server.allowed_hosts.iter().any(|host| host == "*"));

        // The remainder must deserialize as the daemon config the server actually
        // uses at startup (load_config injects cache_root).
        let daemon: crate::model::DaemonConfig =
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
}
