mod bash;
mod index;
mod intelligence;
mod manager;
mod mcp_transport;
mod model;
mod process_runtime;
mod repository;
mod security;
mod symbols;
mod tools;
mod workspace;

use anyhow::{Context, Result};
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use clap::{Parser, Subcommand, ValueEnum};
use manager::{SessionKey, WorkspaceManager};
use serde::Deserialize;
use serde_json::{json, Map, Value};
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

const SERVER_NAME: &str = "codeweave-rust";

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Transport {
    Http,
    Stdio,
}

#[derive(Parser, Debug)]
#[command(version, about = "Rust-only CodeWeave MCP server")]
struct Cli {
    /// Subcommand to run. Omitting it runs the server (same as `serve`), so the
    /// historical bare invocation (`--transport http --config config.json`) keeps
    /// working unchanged.
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
    /// Run the MCP server (the default when no subcommand is given).
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
#[serde(rename_all = "camelCase")]
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
    #[serde(default = "default_stateful_mode")]
    stateful_mode: bool,
    #[serde(default = "default_json_response")]
    json_response: bool,
    /// Idle HTTP keep-alive timeout in milliseconds. Hyper's equivalent of
    /// Uvicorn's `timeout_keep_alive`: an idle kept-alive connection is closed
    /// after this long, so a tunnel/connector does not hold the socket open to
    /// its own ~90s deadline and report that as the connection lifetime. `0`
    /// disables the bound (connections stay open until the peer closes them).
    #[serde(default = "default_idle_timeout_ms")]
    idle_timeout_ms: u64,
    /// Named tool profile: `full` (default), `read-only`, `edit`, or `custom`.
    /// `custom` selects tools via the `tools` include/exclude lists below.
    #[serde(default = "default_tool_profile")]
    tool_profile: String,
    /// Custom tool selection, applied only when `toolProfile` is `custom`.
    #[serde(default)]
    tools: ToolsConfig,
}

#[derive(Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ToolsConfig {
    #[serde(default)]
    include: Vec<String>,
    #[serde(default)]
    exclude: Vec<String>,
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
fn default_stateful_mode() -> bool {
    false
}
/// Single-shot JSON responses are the default, not SSE streaming. Long-running work
/// usually does not block the response: when the configured foreground budget is
/// enabled (default about 20s), `bash` auto-promotes past that budget to a background
/// run the caller polls with `bash_status`. JSON keeps the framing simple and
/// maximally connector-compatible. Operators who want server-push can still opt in
/// with `"jsonResponse": false`.
fn default_json_response() -> bool {
    true
}
fn default_idle_timeout_ms() -> u64 {
    5000
}
fn default_tool_profile() -> String {
    "full".into()
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
    manager: Arc<WorkspaceManager>,
    config: Value,
    server: ServerConfig,
    token: Option<Arc<Vec<u8>>>,
    /// Resolved tool set for the active `toolProfile`. Drives `tools/list`, the
    /// transport callable-name gate, and edit-`validate` availability.
    tool_access: Arc<tools::ToolAccess>,
}

/// Resolve the tool set from `server.toolProfile` (+ custom include/exclude) and
/// `policy.bash.enabled`. Fails startup with an actionable error on an unknown
/// profile or a custom list that references an unknown tool.
fn resolve_tool_access(server: &ServerConfig, config: &Value) -> Result<tools::ToolAccess> {
    let profile = if server.tool_profile == "custom" {
        None
    } else {
        Some(tools::Profile::parse(&server.tool_profile).ok_or_else(|| {
            anyhow::anyhow!(
                "unknown server.toolProfile '{}'; expected 'full', 'read-only', 'edit', or 'custom'",
                server.tool_profile
            )
        })?)
    };
    let custom = tools::CustomSelection {
        include: server.tools.include.clone(),
        exclude: server.tools.exclude.clone(),
    };
    let policy_bash_enabled = config
        .get("policy")
        .and_then(|policy| policy.get("bash"))
        .and_then(|bash| bash.get("enabled"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    tools::resolve_access(profile, &custom, policy_bash_enabled).map_err(|e| anyhow::anyhow!(e))
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
    object.entry("cache_root").or_insert_with(|| {
        let base = path.parent().unwrap_or_else(|| Path::new("."));
        Value::String(base.join(".codeweave-cache").to_string_lossy().into_owned())
    });
    object.remove("server");
    object.remove("rust");
    Ok((server, root))
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

fn object(value: Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}

fn is_code_mutation(method: &str) -> bool {
    matches!(
        method,
        "code_write"
            | "code_replace"
            | "code_replace_range"
            | "code_insert"
            | "code_delete"
            | "code_rename"
    )
}

fn normalize_code_mutation(method: &str, params: &mut Map<String, Value>) {
    let (kind, fields): (&str, &[&str]) = match method {
        "code_write" => ("create", &["path", "content", "overwrite", "expected_hash"]),
        "code_replace" => (
            "replace",
            &[
                "path",
                "old_text",
                "new_text",
                "expected_replacements",
                "expected_hash",
                "handle",
            ],
        ),
        "code_replace_range" => ("replace_range", &["path", "handle", "new_text"]),
        "code_insert" => (
            "insert",
            &[
                "path",
                "content",
                "anchor_symbol",
                "position",
                "expected_hash",
            ],
        ),
        "code_delete" => ("delete", &["path", "expected_hash"]),
        "code_rename" => ("rename", &["path", "to", "expected_hash"]),
        _ => return,
    };

    let mut change = Map::new();
    change.insert("kind".into(), Value::String(kind.into()));
    for field in fields {
        if let Some(value) = params.remove(*field) {
            change.insert((*field).into(), value);
        }
    }
    if method == "code_write" && !change.contains_key("overwrite") {
        change.insert("overwrite".into(), Value::Bool(true));
    }
    params.insert("changes".into(), Value::Array(vec![Value::Object(change)]));
}

fn tool_action(method: &str) -> Option<&'static str> {
    match method {
        "git_status" => Some("status"),
        "git_diff" => Some("diff"),
        "git_log" => Some("log"),
        "git_show" => Some("show"),
        "git_blame" => Some("blame"),
        "git_preflight" => Some("preflight"),
        "git_stage" => Some("stage"),
        "git_commit" => Some("commit"),
        "git_restore" => Some("restore"),
        "git_push" => Some("push"),
        _ => None,
    }
}

async fn prepare(
    _manager: &Arc<WorkspaceManager>,
    _config: &Value,
    tool_access: &tools::ToolAccess,
    method: &str,
    input: Value,
) -> Result<Value, model::AppError> {
    if matches!(
        method,
        "task_run" | "task_status" | "task_output" | "task_cancel"
    ) {
        return Err(model::AppError::details(
            "METHOD_NOT_FOUND",
            "Task profile tools were removed; use bash, bash_status, bash_output, or bash_cancel",
            json!({"method": method}),
        ));
    }
    // Edit tools carry `validate` shell commands that run through bash. When the
    // active profile (or policy) makes bash unavailable, an edit that requests
    // validation cannot be honored — reject it up front rather than silently
    // dropping the validation step.
    if !tool_access.bash_tools_available()
        && matches!(
            method,
            "code_write"
                | "code_replace"
                | "code_replace_range"
                | "code_insert"
                | "code_delete"
                | "code_rename"
                | "code_transaction"
        )
    {
        let has_validate = input
            .get("validate")
            .and_then(Value::as_array)
            .is_some_and(|commands| !commands.is_empty());
        if has_validate {
            return Err(model::AppError::details(
                "VALIDATE_UNAVAILABLE",
                "Edit 'validate' commands require bash, which is unavailable under the active tool profile or policy",
                json!({"method": method}),
            ));
        }
    }
    let mut params = object(input);
    // A CodeWeave server serves exactly one repository, fixed at startup. Legacy
    // workspace_id/workspace arguments are accepted but stripped so they can never
    // redirect a tool call.
    params.remove("workspace_id");
    params.remove("workspace");
    if method == "code_fetch" && !params.contains_key("items") {
        if let Some(ranges) = params
            .remove("ranges")
            .and_then(|value| value.as_array().cloned())
        {
            let items = ranges
                .into_iter()
                .map(|range| {
                    let mut item = json!({
                        "kind": "path",
                        "value": range.get("path").cloned().unwrap_or(Value::Null),
                    });
                    if let Some(value) = range.get("start_line").or_else(|| range.get("start")) {
                        item["start_line"] = value.clone();
                    }
                    if let Some(value) = range.get("end_line").or_else(|| range.get("end")) {
                        item["end_line"] = value.clone();
                    }
                    item
                })
                .collect::<Vec<_>>();
            params.insert("items".into(), json!(items));
        } else if let Some(path) = params.remove("path") {
            let mut item = json!({"kind":"path","value":path});
            if let Some(v) = params.remove("start_line") {
                item["start_line"] = v;
            }
            if let Some(v) = params.remove("end_line") {
                item["end_line"] = v;
            }
            params.insert("items".into(), json!([item]));
        }
    }
    if method == "code_preview" {
        params.insert("preview".into(), Value::Bool(true));
    }
    if is_code_mutation(method) {
        normalize_code_mutation(method, &mut params);
    }
    if matches!(
        method,
        "bash" | "bash_status" | "bash_output" | "bash_cancel"
    ) {
        let allowed_fields: &[&str] = match method {
            "bash" => &["command", "cwd", "background", "timeout_ms"],
            "bash_status" | "bash_cancel" => &["run_id"],
            "bash_output" => &["run_id", "stream", "continuation"],
            _ => unreachable!("matched Bash tool"),
        };
        if params
            .keys()
            .any(|field| !allowed_fields.contains(&field.as_str()))
        {
            return Err(model::AppError::invalid(format!(
                "{method} received an unknown or spoofed field"
            )));
        }
    }
    if method == "bash" {
        let has_command = params
            .get("command")
            .and_then(Value::as_str)
            .is_some_and(|command| !command.trim().is_empty());
        if !has_command {
            return Err(model::AppError::invalid(
                "bash requires a non-empty command",
            ));
        }
    }
    if let Some(action) = tool_action(method) {
        params.insert("action".into(), Value::String(action.into()));
    }
    Ok(Value::Object(params))
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
        // Keep the public liveness response operationally useful without
        // exposing authentication, repository, or build provenance details.
        "statefulMode": state.server.stateful_mode,
        "jsonResponse": state.server.json_response,
        // Idle keep-alive timeout actually applied to accepted connections, so a
        // tunnel operator can confirm sockets are closed at ~5s (matching the
        // ngrok "Connections" p50/p90) rather than held to the connector deadline.
        "idleTimeoutMs": state.server.idle_timeout_ms,
        "rmcp": "1.8",
    }))
}

async fn health(State(state): State<AppState>) -> impl IntoResponse {
    match state
        .manager
        .dispatch(SessionKey::stateless(), "health", &json!({}))
        .await
    {
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
            std::fs::write(path, &token)
                .with_context(|| format!("creating bearer token file {}", path.display()))?;
            eprintln!("generated bearer token at {}", path.display());
            Ok(token)
        }
        Err(error) => Err(error).with_context(|| format!("reading token file {}", path.display())),
    }
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
    match resolve_tool_access(&server, &config) {
        Ok(_) => checks.push(Check::ok(
            "tool profile",
            format!("{} resolves", server.tool_profile),
        )),
        Err(error) => checks.push(Check::fail(
            "tool profile",
            format!("{error}; fix server.toolProfile or server.tools"),
        )),
    }

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
        ("python", "basedpyright-langserver"),
        ("typescript", "typescript-language-server"),
    ] {
        let check_name = match language {
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
            Ok(value) if !value.trim().is_empty() => checks.push(Check::ok(
                "token",
                format!("{} is present", token_path.display()),
            )),
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
        let manager = Arc::new(WorkspaceManager::default());
        match manager
            .dispatch(SessionKey::stdio(), "initialize", &config)
            .await
        {
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
                let bash_enabled = config["policy"]["bash"]["enabled"]
                    .as_bool()
                    .unwrap_or(false);
                let bash_available = init["bash_available"].as_bool().unwrap_or(false);
                if !bash_enabled {
                    checks.push(Check::ok("bash", "disabled by policy"));
                } else if bash_available {
                    checks.push(Check::ok(
                        "bash",
                        "available (pre-probed during initialization)",
                    ));
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
async fn main() -> Result<()> {
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

/// Load the config, initialize the single repository eagerly, and start the
/// selected transport. This is the historical `main` body; the bare invocation
/// (no subcommand) routes here.
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
    let manager = Arc::new(WorkspaceManager::default());
    let init = manager
        .dispatch(SessionKey::stdio(), "initialize", &config)
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    tracing::info!(
        workspace = %init["workspace"]["path"].as_str().unwrap_or_default(),
        file_count = init["file_count"].as_u64().unwrap_or_default(),
        index_ready = init["index_ready"].as_bool().unwrap_or(false),
        bash_available = init["bash_available"].as_bool().unwrap_or(false),
        "repository ready before transport bind"
    );
    let tool_access = Arc::new(resolve_tool_access(&server, &config)?);
    tracing::info!(
        profile = %server.tool_profile,
        bash_tools_available = tool_access.bash_tools_available(),
        "tool profile resolved"
    );
    let state = AppState {
        manager,
        config,
        server,
        token,
        tool_access,
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
    fn bare_cli_keeps_the_historical_serve_defaults() {
        let cli = Cli::parse_from(["codeweave"]);
        assert!(cli.command.is_none());
        assert_eq!(cli.config, PathBuf::from("config.json"));
        assert!(matches!(cli.transport, Transport::Http));
        assert!(cli.host.is_none());
        assert!(cli.port.is_none());
    }

    /// Full-profile tool access with bash available — the default context for
    /// `prepare` tests that are not specifically exercising profile gating.
    fn full_access() -> tools::ToolAccess {
        tools::resolve_access(
            Some(tools::Profile::Full),
            &tools::CustomSelection::default(),
            true,
        )
        .unwrap()
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
            manager: Arc::new(WorkspaceManager::default()),
            config: json!({
                "workspace": {
                    "path": "C:\\private\\repository"
                }
            }),
            server: serde_json::from_value(json!({
                "authMode": "bearer",
                "statefulMode": false,
                "jsonResponse": true,
                "idleTimeoutMs": 5000
            }))
            .unwrap(),
            token: Some(Arc::new(b"secret".to_vec())),
            tool_access: Arc::new(
                tools::resolve_access(
                    Some(tools::Profile::Full),
                    &tools::CustomSelection::default(),
                    false,
                )
                .unwrap(),
            ),
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
    fn public_tool_schemas_are_hosted_client_compatible() {
        let all = crate::tools::full_list_payload();
        let items = all.as_array().expect("tools array");
        let expected_annotations = [
            ("workspace", true, false, true, false),
            ("code_context", true, false, true, false),
            ("code_capabilities", true, false, true, false),
            ("code_fetch", true, false, true, false),
            ("code_search", true, false, true, false),
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

        assert!(tool(&all, "code_context")["inputSchema"]
            .get("required")
            .is_none());
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
        let fetch_item = &tool(&all, "code_fetch")["inputSchema"]["properties"]["items"]["items"];
        assert_eq!(fetch_item["required"], json!(["kind", "value"]));
        assert_eq!(
            fetch_item["properties"]["kind"]["enum"],
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
        assert_eq!(fetch_item["additionalProperties"], false);
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
    async fn prepare_normalizes_compatibility_inputs() {
        let manager = Arc::new(WorkspaceManager::default());
        let config = json!({"workspace": {"path": "/repo"}});

        let context = prepare(
            &manager,
            &config,
            &full_access(),
            "code_context",
            json!({"query": "WorkspaceActor"}),
        )
        .await
        .unwrap();
        assert_eq!(context["query"], "WorkspaceActor");

        let fetch = prepare(
            &manager,
            &config,
            &full_access(),
            "code_fetch",
            json!({"ranges": [{"path": "src/main.rs", "start": 2, "end": 4}]}),
        )
        .await
        .unwrap();
        assert_eq!(fetch["items"][0]["kind"], "path");
        assert_eq!(fetch["items"][0]["start_line"], 2);
        assert_eq!(fetch["items"][0]["end_line"], 4);

        let replace = prepare(
            &manager,
            &config,
            &full_access(),
            "code_replace",
            json!({
                "snapshot_id": "snap_test",
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
            let prepared = prepare(
                &manager,
                &config,
                &full_access(),
                method,
                json!({"action": "spoofed"}),
            )
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
        assert!(prepare(
            &manager,
            &config,
            &full_access(),
            "bash",
            json!({"command": "  "})
        )
        .await
        .is_err());
        assert!(prepare(
            &manager,
            &config,
            &full_access(),
            "bash",
            json!({"command": "printf test", "unknown": true})
        )
        .await
        .is_err());
        assert!(prepare(
            &manager,
            &config,
            &full_access(),
            "bash_status",
            json!({"run_id": "run_test", "action": "cancel"})
        )
        .await
        .is_err());
        let removed = prepare(
            &manager,
            &config,
            &full_access(),
            "task_run",
            json!({"profile": "test"}),
        )
        .await
        .unwrap_err();
        assert_eq!(removed.0.code, "METHOD_NOT_FOUND");
    }

    #[tokio::test]
    async fn public_bash_payloads_dispatch_after_preparation() {
        let root = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let manager = Arc::new(WorkspaceManager::default());
        let config = json!({
            "workspace": {"path": root.path(), "artifactPaths": []},
            "skills": {"enabled": false, "roots": [], "explicitOnly": true},
            "policy": {
                "maxFileBytes": 1000000,
                "maxContextChars": 50000,
                "maxSearchResults": 100,
                "bash": {
                    "enabled": true,
                    "executable": crate::model::test_bash_executable(),
                    "defaultTimeoutMs": 120000,
                    "maxTimeoutMs": 300000,
                    "maxOutputChars": 30000,
                    "retentionHours": 1
                }
            },
            "cache_root": cache.path()
        });
        manager
            .dispatch(SessionKey::stdio(), "initialize", &config)
            .await
            .unwrap();

        for input in [
            json!({"command": "printf command-only"}),
            json!({"command": "printf command-with-timeout", "timeout_ms": 5000}),
        ] {
            let prepared = prepare(&manager, &config, &full_access(), "bash", input)
                .await
                .unwrap();
            let result = manager
                .dispatch(SessionKey::stdio(), "bash", &prepared)
                .await
                .unwrap();

            assert_eq!(result["status"], "succeeded");
            assert_eq!(result["exit_code"], 0);
        }
    }

    #[tokio::test]
    async fn narrow_write_tool_dispatches_through_transactional_engine() {
        let root = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let manager = Arc::new(WorkspaceManager::default());
        let config = json!({
            "workspace": {"path": root.path(), "artifactPaths": []},
            "skills": {"enabled": false, "roots": [], "explicitOnly": true},
            "policy": {
                "maxFileBytes": 1000000,
                "maxContextChars": 50000,
                "maxSearchResults": 100,
                "bash": {"enabled": false}
            },
            "cache_root": cache.path()
        });
        manager
            .dispatch(SessionKey::stdio(), "initialize", &config)
            .await
            .unwrap();

        let prepared = prepare(
            &manager,
            &config,
            &full_access(),
            "code_write",
            json!({
                "workspace_id": "main",
                "path": "created.txt",
                "content": "created through code_write\n"
            }),
        )
        .await
        .unwrap();
        let result = manager
            .dispatch(SessionKey::stdio(), "code_write", &prepared)
            .await
            .unwrap();

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
        let preview_result = manager
            .dispatch(SessionKey::stdio(), "code_preview", &preview)
            .await
            .unwrap();
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
            .dispatch(SessionKey::stdio(), "code_preview", &syntax_error)
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
            .dispatch(SessionKey::stdio(), "code_transaction", &transaction)
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
    async fn prepare_strips_legacy_routing_args_for_the_fixed_repository() {
        let configured = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        std::fs::write(
            configured.path().join("configured.rs"),
            "fn configured() {}\n",
        )
        .unwrap();
        let manager = Arc::new(WorkspaceManager::default());
        let daemon_config = json!({
            "workspace": {"path": configured.path(), "artifactPaths": []},
            "skills": {"enabled": false, "roots": [], "explicitOnly": true},
            "policy": {"maxFileBytes": 1000000, "maxContextChars": 50000, "maxSearchResults": 100, "bash": {"enabled": false}},
            "cache_root": cache.path()
        });
        manager
            .dispatch(SessionKey::stdio(), "initialize", &daemon_config)
            .await
            .unwrap();

        // Legacy workspace_id/path routing args are stripped before dispatch so a
        // client can never redirect a tool call off the single configured repo.
        let public_config = json!({"workspace": {"path": configured.path()}});
        let prepared = prepare(
            &manager,
            &public_config,
            &full_access(),
            "code_search",
            json!({"workspace_id": "main", "mode": "filename", "query": "configured.rs"}),
        )
        .await
        .unwrap();
        assert!(prepared.get("workspace_id").is_none());

        let summary = manager
            .dispatch(
                SessionKey::stdio(),
                "workspace",
                &json!({"action": "summary"}),
            )
            .await
            .unwrap();
        assert_eq!(
            std::path::PathBuf::from(summary["root"].as_str().unwrap()),
            std::fs::canonicalize(configured.path()).unwrap()
        );
    }

    #[test]
    fn relative_token_path_is_resolved_from_config_directory() {
        let config = Path::new("C:/path/to/codeweave/config.json");
        let resolved = config_relative_path(config, ".mcp-token");
        assert_eq!(resolved, PathBuf::from("C:/path/to/codeweave/.mcp-token"));
    }

    /// D1: `config.example.json` must deserialize through the *real* config path
    /// (`load_config` + `DaemonConfig`) and its advertised defaults must match the
    /// code defaults, so the shipped example can never silently drift from what the
    /// server actually does. Regression guard for the historical `8813` vs `8820` skew.
    #[test]
    fn shipped_config_example_deserializes_and_matches_code_defaults() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("config.example.json");
        let (server, root) = load_config(&path).expect("config.example.json must load");

        // Port advertised in the example must equal the code default so a copy-paste
        // start hits the port the README and connectors expect.
        assert_eq!(server.port, default_port());
        assert_eq!(server.port, 8813);

        // The remainder must deserialize as the daemon config the server actually
        // uses at startup (load_config injects cache_root).
        let daemon: crate::model::DaemonConfig =
            serde_json::from_value(root).expect("example must deserialize as DaemonConfig");

        // foregroundBudgetMs is present and non-zero (auto-promotion enabled), and
        // the example's bash budget matches the documented code default.
        assert_eq!(daemon.policy.bash.foreground_budget_ms, 20_000);
    }

    /// The shipped example must resolve a valid, non-empty tool profile so a
    /// copy-paste start exposes the tools the docs advertise.
    #[test]
    fn shipped_config_example_resolves_full_tool_profile() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("config.example.json");
        let (server, root) = load_config(&path).expect("config.example.json must load");
        assert_eq!(server.tool_profile, "full");
        let access = resolve_tool_access(&server, &root).expect("example profile must resolve");
        assert!(access.is_allowed("bash"));
        assert!(access.bash_tools_available());
        assert_eq!(access.list_payload().as_array().unwrap().len(), 28);
    }

    #[test]
    fn unknown_tool_profile_fails_startup_resolution() {
        let server: ServerConfig =
            serde_json::from_value(json!({"toolProfile": "nonsense"})).unwrap();
        let error = resolve_tool_access(&server, &json!({})).unwrap_err();
        assert!(error.to_string().contains("nonsense"));
    }

    /// An edit that carries `validate` commands is rejected up front when bash is
    /// unavailable under the active profile — the validation could not run.
    #[tokio::test]
    async fn prepare_rejects_validate_when_bash_unavailable() {
        let manager = Arc::new(WorkspaceManager::default());
        let config = json!({"workspace": {"path": "/repo"}});
        // read-only profile has no bash tools, so validate cannot be honored.
        let read_only = tools::resolve_access(
            Some(tools::Profile::ReadOnly),
            &tools::CustomSelection::default(),
            true,
        )
        .unwrap();

        let error = prepare(
            &manager,
            &config,
            &read_only,
            "code_write",
            json!({"path": "a.rs", "content": "x", "validate": ["cargo check"]}),
        )
        .await
        .unwrap_err();
        assert_eq!(error.0.code, "VALIDATE_UNAVAILABLE");

        // The same edit without validate is accepted.
        let ok = prepare(
            &manager,
            &config,
            &read_only,
            "code_write",
            json!({"path": "a.rs", "content": "x"}),
        )
        .await
        .unwrap();
        assert_eq!(ok["changes"][0]["kind"], "create");
    }
}
