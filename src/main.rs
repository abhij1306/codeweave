mod index;
mod manager;
mod mcp_transport;
mod model;
mod repository;
mod security;
mod symbols;
mod tasks;
mod workspace;

use anyhow::{Context, Result};
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use clap::{Parser, ValueEnum};
use manager::WorkspaceManager;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use subtle::ConstantTimeEq;
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
    #[arg(long, default_value = "config.json")]
    config: PathBuf,
    #[arg(long, value_enum, default_value_t = Transport::Http)]
    transport: Transport,
    #[arg(long)]
    host: Option<String>,
    #[arg(long)]
    port: Option<u16>,
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
}
fn default_host() -> String {
    "127.0.0.1".into()
}
fn default_port() -> u16 {
    8820
}
fn default_auth() -> String {
    "bearer".into()
}
fn default_token() -> String {
    ".mcp-token".into()
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

fn tools() -> Value {
    let read = json!({
        "readOnlyHint": true,
        "destructiveHint": false,
        "idempotentHint": true,
        "openWorldHint": false
    });
    let write = json!({
        "readOnlyHint": false,
        "destructiveHint": false,
        "idempotentHint": false,
        "openWorldHint": false
    });
    let execution = json!({"taskSupport":"forbidden"});

    // Keep the public schemas deliberately simple. Some hosted MCP clients reject or
    // mishandle deeply nested oneOf/not/const schemas even though they are valid JSON Schema.
    // These definitions mirror the TypeScript gateway that is known to work with Perplexity;
    // the Rust request normalizer still accepts the richer compatibility forms internally.
    json!([
      {
        "name":"workspace",
        "title":"Workspace",
        "description":"Open or switch the single active repository, view its summary or changes, refresh it, or explicitly list/read configured skills. Pass path to switch repositories without restarting the server. Skills must only be used when the user explicitly asks.",
        "annotations":read.clone(),
        "execution":execution.clone(),
        "inputSchema":{
          "type":"object",
          "properties":{
            "action":{"default":"open","type":"string","enum":["open","summary","refresh","changes","skills","skill"]},
            "path":{"type":"string","description":"Absolute repository path to make active. Must be within configured allowed roots."},
            "skill_name":{"type":"string","description":"Skill directory name. Use only after an explicit user request to use that skill."},
            "force":{"type":"boolean"}
          },
          "$schema":"http://json-schema.org/draft-07/schema#"
        }
      },
      {
        "name":"code_context",
        "title":"Ranked Code Context",
        "description":"Find relevant code for a task. Pass a short list of identifiers or concepts in terms, not the full user request.",
        "annotations":read.clone(),
        "execution":execution.clone(),
        "inputSchema":{
          "type":"object",
          "properties":{
            "terms":{"minItems":1,"maxItems":12,"type":"array","items":{"type":"string","minLength":1,"maxLength":80}},
            "paths":{"type":"array","items":{"type":"string"}}
          },
          "required":["terms"],
          "$schema":"http://json-schema.org/draft-07/schema#"
        }
      },
      {
        "name":"code_fetch",
        "title":"Fetch Exact Code or Logs",
        "description":"Read a file, file range, symbol, task log, or previous continuation. For a single file, pass path directly; use items to batch reads.",
        "annotations":read.clone(),
        "execution":execution.clone(),
        "inputSchema":{
          "type":"object",
          "properties":{
            "path":{"type":"string"},
            "start_line":{"type":"integer","minimum":1,"maximum":9007199254740991_i64},
            "end_line":{"type":"integer","minimum":1,"maximum":9007199254740991_i64},
            "items":{"type":"array","items":{"type":"object","properties":{"kind":{"type":"string","enum":["path","handle","symbol","task_log","continuation"]},"value":{"type":"string","minLength":1},"start_line":{"type":"integer","minimum":1,"maximum":9007199254740991_i64},"end_line":{"type":"integer","minimum":1,"maximum":9007199254740991_i64}},"required":["kind","value"],"additionalProperties":false}},
            "max_chars":{"type":"integer","minimum":1,"maximum":200000}
          },
          "$schema":"http://json-schema.org/draft-07/schema#"
        }
      },
      {
        "name":"code_search",
        "title":"Deterministic Code Search",
        "description":"Search the project by text, regex, filename, symbol, references, outline, or repository map. Literal text search is the default.",
        "annotations":read.clone(),
        "execution":execution.clone(),
        "inputSchema":{
          "type":"object",
          "properties":{
            "query":{"default":"","type":"string"},
            "mode":{"type":"string","enum":["literal","regex","filename","symbol","references","outline","repo_map"]},
            "paths":{"type":"array","items":{"type":"string"}},
            "max_results":{"type":"integer","minimum":1,"maximum":200},
            "context_lines":{"type":"integer","minimum":0,"maximum":20},
            "case_sensitive":{"type":"boolean"}
          },
          "$schema":"http://json-schema.org/draft-07/schema#"
        }
      },
      {
        "name":"code_write",
        "title":"Write One File",
        "description":"Create or overwrite exactly one file. Use expected_hash when replacing an existing file.",
        "annotations":write.clone(),
        "execution":execution.clone(),
        "inputSchema":{
          "type":"object",
          "properties":{
            "path":{"type":"string"},
            "content":{"type":"string"},
            "overwrite":{"type":"boolean","default":true},
            "expected_hash":{"type":"string"},
            "validate":{"type":"array","items":{"type":"string"}},
            "rollback_on_failure":{"type":"boolean"}
          },
          "required":["path","content"],
          "additionalProperties":false,
          "$schema":"http://json-schema.org/draft-07/schema#"
        }
      },
      {
        "name":"code_replace",
        "title":"Replace Text in One File",
        "description":"Replace exact text in exactly one file. The replacement count is checked before writing.",
        "annotations":write.clone(),
        "execution":execution.clone(),
        "inputSchema":{
          "type":"object",
          "properties":{
            "path":{"type":"string"},
            "old_text":{"type":"string"},
            "new_text":{"type":"string"},
            "expected_replacements":{"type":"integer","minimum":1,"default":1},
            "expected_hash":{"type":"string"},
            "handle":{"type":"string"},
            "validate":{"type":"array","items":{"type":"string"}},
            "rollback_on_failure":{"type":"boolean"}
          },
          "required":["path","old_text","new_text"],
          "additionalProperties":false,
          "$schema":"http://json-schema.org/draft-07/schema#"
        }
      },
      {
        "name":"code_insert",
        "title":"Insert Text in One File",
        "description":"Insert text before, after, or inside one named symbol in exactly one file.",
        "annotations":write.clone(),
        "execution":execution.clone(),
        "inputSchema":{
          "type":"object",
          "properties":{
            "path":{"type":"string"},
            "content":{"type":"string"},
            "anchor_symbol":{"type":"string"},
            "position":{"type":"string","enum":["before","after","inside_start","inside_end"]},
            "expected_hash":{"type":"string"},
            "validate":{"type":"array","items":{"type":"string"}},
            "rollback_on_failure":{"type":"boolean"}
          },
          "required":["path","content","anchor_symbol","position"],
          "additionalProperties":false,
          "$schema":"http://json-schema.org/draft-07/schema#"
        }
      },
      {
        "name":"code_delete",
        "title":"Delete One File",
        "description":"Delete exactly one file with an optional content-hash precondition.",
        "annotations":write.clone(),
        "execution":execution.clone(),
        "inputSchema":{
          "type":"object",
          "properties":{
            "path":{"type":"string"},
            "expected_hash":{"type":"string"},
            "validate":{"type":"array","items":{"type":"string"}},
            "rollback_on_failure":{"type":"boolean"}
          },
          "required":["path"],
          "additionalProperties":false,
          "$schema":"http://json-schema.org/draft-07/schema#"
        }
      },
      {
        "name":"code_rename",
        "title":"Rename One File",
        "description":"Rename exactly one file with an optional content-hash precondition.",
        "annotations":write.clone(),
        "execution":execution.clone(),
        "inputSchema":{
          "type":"object",
          "properties":{
            "path":{"type":"string"},
            "to":{"type":"string"},
            "expected_hash":{"type":"string"},
            "validate":{"type":"array","items":{"type":"string"}},
            "rollback_on_failure":{"type":"boolean"}
          },
          "required":["path","to"],
          "additionalProperties":false,
          "$schema":"http://json-schema.org/draft-07/schema#"
        }
      },
      {
        "name":"git",
        "title":"Git",
        "description":"Check Git status or diff, inspect history, stage files, commit, or restore files. Restore requires confirm=true.",
        "annotations":write.clone(),
        "execution":execution.clone(),
        "inputSchema":{
          "type":"object",
          "properties":{
            "action":{"type":"string","enum":["status","diff","log","show","blame","stage","commit","restore"]},
            "paths":{"type":"array","items":{"type":"string"}},
            "ref":{"type":"string"},
            "message":{"type":"string"},
            "max_chars":{"type":"integer","minimum":1,"maximum":200000},
            "confirm":{"type":"boolean"}
          },
          "required":["action"],
          "$schema":"http://json-schema.org/draft-07/schema#"
        }
      },
      {
        "name":"run",
        "title":"Run Task or Controlled Command",
        "description":"Run tests, builds, or another allowed command. Use a configured profile when available, otherwise pass command as an argument array. Also supports background task status, output, and cancellation.",
        "annotations":write,
        "execution":execution,
        "inputSchema":{
          "type":"object",
          "properties":{
            "action":{"default":"start","type":"string","enum":["start","status","output","cancel"]},
            "command":{"type":"array","items":{"type":"string"}},
            "profile":{"type":"string"},
            "cwd":{"type":"string"},
            "task_id":{"type":"string"}
          },
          "$schema":"http://json-schema.org/draft-07/schema#"
        }
      }
    ])
}

fn object(value: Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}

fn split_command_line(input: &str) -> std::result::Result<Vec<String>, model::AppError> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' && quote == Some('"') {
            if chars.peek().is_some_and(|next| matches!(next, '"' | '\\')) {
                current.push(chars.next().expect("peeked character must exist"));
            } else {
                current.push(ch);
            }
            continue;
        }
        if matches!(ch, '\'' | '"') {
            if quote == Some(ch) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(ch);
            } else {
                current.push(ch);
            }
            continue;
        }
        if ch.is_whitespace() && quote.is_none() {
            if !current.is_empty() {
                args.push(std::mem::take(&mut current));
            }
        } else {
            current.push(ch);
        }
    }
    if quote.is_some() {
        return Err(model::AppError::invalid(
            "Command string has an unterminated quote",
        ));
    }
    if !current.is_empty() {
        args.push(current);
    }
    if args.is_empty() {
        return Err(model::AppError::invalid("Command cannot be empty"));
    }
    Ok(args)
}

fn is_code_mutation(method: &str) -> bool {
    matches!(
        method,
        "code_write" | "code_replace" | "code_insert" | "code_delete" | "code_rename"
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

async fn prepare(
    _manager: &Arc<WorkspaceManager>,
    _config: &Value,
    method: &str,
    input: Value,
) -> Result<Value, model::AppError> {
    let mut params = object(input);
    if method == "code_context" {
        if let Some(terms) = params.remove("terms").and_then(|v| v.as_array().cloned()) {
            params.insert(
                "query".into(),
                Value::String(
                    terms
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(" "),
                ),
            );
        }
    }
    // A CodeWeave process owns exactly one active repository. Legacy workspace_id
    // arguments are accepted but ignored so they can never reopen or redirect a tool call.
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
    if method == "run" {
        if let Some(command) = params.get("command").and_then(Value::as_str) {
            params.insert("command".into(), json!(split_command_line(command)?));
        }
    }
    if is_code_mutation(method) {
        normalize_code_mutation(method, &mut params);
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
    let structured = json!({
        "error": {
            "code": body.code,
            "message": body.message,
            "retryable": retryable,
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
    Json(
        json!({"ok":true,"name":SERVER_NAME,"version":env!("CARGO_PKG_VERSION"),"transport":"http","auth":state.server.auth_mode}),
    )
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

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let cli = Cli::parse();
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
    manager
        .dispatch("initialize", &config)
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    let state = AppState {
        manager,
        config,
        server,
        token,
    };
    match cli.transport {
        Transport::Http => mcp_transport::run_http(state, &cli).await,
        Transport::Stdio => mcp_transport::run_stdio(state).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool<'a>(all: &'a Value, name: &str) -> &'a Value {
        all.as_array()
            .and_then(|items| {
                items
                    .iter()
                    .find(|item| item.get("name").and_then(Value::as_str) == Some(name))
            })
            .expect("tool must exist")
    }

    #[test]
    fn public_tool_schemas_are_hosted_client_compatible() {
        let all = tools();
        let items = all.as_array().expect("tools array");
        assert_eq!(items.len(), 11);

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

        assert_eq!(
            tool(&all, "code_context")["inputSchema"]["required"],
            json!(["terms"])
        );
        assert_eq!(
            tool(&all, "git")["inputSchema"]["required"],
            json!(["action"])
        );
        assert!(items.iter().all(|item| item["name"] != "code_edit"));
        let fetch_item = &tool(&all, "code_fetch")["inputSchema"]["properties"]["items"]["items"];
        assert_eq!(fetch_item["required"], json!(["kind", "value"]));
        assert_eq!(
            fetch_item["properties"]["kind"]["enum"],
            json!(["path", "handle", "symbol", "task_log", "continuation"])
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
    }

    #[test]
    fn command_strings_are_split_without_shell_expansion() {
        assert_eq!(
            split_command_line("cargo test --manifest-path 'project dir/Cargo.toml'").unwrap(),
            vec![
                "cargo".to_owned(),
                "test".to_owned(),
                "--manifest-path".to_owned(),
                "project dir/Cargo.toml".to_owned(),
            ]
        );
        assert!(split_command_line("cargo 'test").is_err());
    }

    #[tokio::test]
    async fn prepare_normalizes_compatibility_inputs() {
        let manager = Arc::new(WorkspaceManager::default());
        let config = json!({"workspaces": []});

        let context = prepare(
            &manager,
            &config,
            "code_context",
            json!({"query": "WorkspaceActor"}),
        )
        .await
        .unwrap();
        assert_eq!(context["query"], "WorkspaceActor");

        let fetch = prepare(
            &manager,
            &config,
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

        let run = prepare(&manager, &config, "run", json!({"command": "cargo test"}))
            .await
            .unwrap();
        assert_eq!(run["command"], json!(["cargo", "test"]));
    }

    #[tokio::test]
    async fn narrow_write_tool_dispatches_through_transactional_engine() {
        let root = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let manager = Arc::new(WorkspaceManager::default());
        let config = json!({
            "workspaces": [{
                "id": "main",
                "name": "Main",
                "path": root.path(),
                "artifactPaths": []
            }],
            "workspace": {"allowedRoots": [root.path()]},
            "skills": {"enabled": false, "roots": [], "explicitOnly": true},
            "policy": {
                "maxFileBytes": 1000000,
                "maxContextChars": 50000,
                "maxSearchResults": 100,
                "maxTaskOutputChars": 30000,
                "shellEnabled": false,
                "allowedCommands": ["cargo"]
            },
            "tasks": {},
            "cache_root": cache.path()
        });
        manager.dispatch("initialize", &config).await.unwrap();

        let prepared = prepare(
            &manager,
            &config,
            "code_write",
            json!({
                "workspace_id": "main",
                "path": "created.txt",
                "content": "created through code_write\n"
            }),
        )
        .await
        .unwrap();
        let result = manager.dispatch("code_write", &prepared).await.unwrap();

        assert_eq!(result["applied"], true);
        assert_eq!(
            std::fs::read_to_string(root.path().join("created.txt")).unwrap(),
            "created through code_write\n"
        );
    }

    #[tokio::test]
    async fn prepare_does_not_reopen_legacy_default_after_dynamic_switch() {
        let configured = tempfile::tempdir().unwrap();
        let dynamic = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        std::fs::write(
            configured.path().join("configured.rs"),
            "fn configured() {}\n",
        )
        .unwrap();
        std::fs::write(dynamic.path().join("dynamic.rs"), "fn dynamic() {}\n").unwrap();
        let manager = Arc::new(WorkspaceManager::default());
        let daemon_config = json!({
            "workspaces": [{"id": "main", "name": "Configured", "path": configured.path(), "artifactPaths": []}],
            "workspace": {"allowedRoots": [configured.path().parent().unwrap()]},
            "skills": {"enabled": false, "roots": [], "explicitOnly": true},
            "policy": {"maxFileBytes": 1000000, "maxContextChars": 50000, "maxSearchResults": 100, "maxTaskOutputChars": 30000, "shellEnabled": false, "allowedCommands": ["cargo"]},
            "tasks": {},
            "cache_root": cache.path()
        });
        manager
            .dispatch("initialize", &daemon_config)
            .await
            .unwrap();
        manager
            .dispatch(
                "workspace",
                &json!({"action": "open", "path": dynamic.path()}),
            )
            .await
            .unwrap();
        let public_config = json!({"workspaces": [{"id": "main", "name": "Configured", "path": configured.path()}]});
        let prepared = prepare(
            &manager,
            &public_config,
            "code_search",
            json!({"workspace_id": "main", "mode": "filename", "query": "dynamic.rs"}),
        )
        .await
        .unwrap();
        assert!(prepared.get("workspace_id").is_none());
        let summary = manager
            .dispatch(
                "workspace",
                &json!({"action": "summary", "workspace_id": "main"}),
            )
            .await
            .unwrap();
        assert_eq!(
            std::path::PathBuf::from(summary["root"].as_str().unwrap()),
            std::fs::canonicalize(dynamic.path()).unwrap()
        );
    }

    #[test]
    fn relative_token_path_is_resolved_from_config_directory() {
        let config = Path::new("C:/path/to/codeweave/config.json");
        let resolved = config_relative_path(config, ".mcp-token");
        assert_eq!(resolved, PathBuf::from("C:/path/to/codeweave/.mcp-token"));
    }
}
