mod bash;
mod index;
mod manager;
mod mcp_transport;
mod model;
mod process_runtime;
mod repository;
mod security;
mod symbols;
mod workspace;

use anyhow::{Context, Result};
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use clap::{Parser, ValueEnum};
use manager::{SessionKey, WorkspaceManager};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::{
    path::{Path, PathBuf},
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
    #[serde(default = "default_stateful_mode")]
    stateful_mode: bool,
    #[serde(default = "default_json_response")]
    json_response: bool,
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
fn default_stateful_mode() -> bool {
    true
}
fn default_json_response() -> bool {
    false
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

fn tools() -> Value {
    // One annotation constant per risk level. The hosted-client safety classifier
    // reads these per advertised tool, so every tool must map to exactly one level.
    // READ: no side effects. WRITE_CLOSED: mutates local state only.
    // DESTRUCTIVE_CLOSED: may discard local state. WRITE_OPEN reaches the network.
    // DESTRUCTIVE_OPEN can run arbitrary commands as the CodeWeave OS user.
    let read = json!({
        "readOnlyHint": true,
        "destructiveHint": false,
        "idempotentHint": true,
        "openWorldHint": false
    });
    let write_closed = json!({
        "readOnlyHint": false,
        "destructiveHint": false,
        "idempotentHint": false,
        "openWorldHint": false
    });
    let destructive_closed = json!({
        "readOnlyHint": false,
        "destructiveHint": true,
        "idempotentHint": false,
        "openWorldHint": false
    });
    let write_open = json!({
        "readOnlyHint": false,
        "destructiveHint": false,
        "idempotentHint": false,
        "openWorldHint": true
    });
    let destructive_open = json!({
        "readOnlyHint": false,
        "destructiveHint": true,
        "idempotentHint": false,
        "openWorldHint": true
    });
    let execution = json!({"taskSupport":"forbidden"});

    // Keep the public schemas deliberately simple. Some hosted MCP clients reject or
    // mishandle deeply nested oneOf/not/const schemas even though they are valid JSON Schema.
    // These definitions mirror the TypeScript gateway that is known to work with Perplexity;
    // the Rust request normalizer still accepts the richer compatibility forms internally.
    let mut advertised = json!([
      {
        "name":"workspace",
        "title":"Workspace",
        "description":"Open or switch this MCP session's active repository, view its summary or changes, refresh it, or explicitly list/read configured skills. Pass path to switch repositories without restarting the server. Skills must only be used when the user explicitly asks.",
        "annotations":read.clone(),
        "execution":execution.clone(),
        "inputSchema":{
          "type":"object",
          "properties":{
            "action":{"default":"open","type":"string","enum":["open","summary","refresh","changes","diagnostics","skills","skill"]},
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
        "description":"Find relevant code for a task. Pass short identifiers or concepts using terms, required_terms, optional_terms, exclude_terms, and document_types.",
        "annotations":read.clone(),
        "execution":execution.clone(),
        "inputSchema":{
          "type":"object",
          "properties":{
            "terms":{"minItems":1,"maxItems":12,"type":"array","items":{"type":"string","minLength":1,"maxLength":80}},
            "paths":{"type":"array","items":{"type":"string"}},
            "required_terms":{"type":"array","items":{"type":"string","minLength":1,"maxLength":80}},
            "optional_terms":{"type":"array","items":{"type":"string","minLength":1,"maxLength":80}},
            "exclude_terms":{"type":"array","items":{"type":"string","minLength":1,"maxLength":80}},
            "document_types":{"type":"array","items":{"type":"string","enum":["source","test","instruction","artifact","runtime_evidence","log"]}},
            "min_score":{"type":"number","minimum":0},
            "include_bash_failures":{"type":"boolean","default":false,"description":"Include up to three recent Bash failures relevant to this query."}
          },
          "$schema":"http://json-schema.org/draft-07/schema#"
        }
      },
      {
        "name":"code_capabilities",
        "title":"CodeWeave Capabilities",
        "description":"Return supported search modes, fetch kinds, edit capabilities, limits, workspace identity, and known limitations.",
        "annotations":read.clone(),
        "execution":execution.clone(),
        "inputSchema":{
          "type":"object",
          "properties":{},
          "$schema":"http://json-schema.org/draft-07/schema#"
        }
      },
      {
        "name":"code_fetch",
        "title":"Fetch Exact Code or Logs",
        "description":"Read a file, file range, symbol, Bash log, or previous continuation. For a single file, pass path directly; use items to batch reads.",
        "annotations":read.clone(),
        "execution":execution.clone(),
        "inputSchema":{
          "type":"object",
          "properties":{
            "path":{"type":"string"},
            "start_line":{"type":"integer","minimum":1,"maximum":9007199254740991_i64},
            "end_line":{"type":"integer","minimum":1,"maximum":9007199254740991_i64},
            "items":{"type":"array","items":{"type":"object","properties":{"kind":{"type":"string","enum":["path","handle","symbol","metadata","bash_status","bash_log","continuation"]},"value":{"type":"string","minLength":1},"start_line":{"type":"integer","minimum":1,"maximum":9007199254740991_i64},"end_line":{"type":"integer","minimum":1,"maximum":9007199254740991_i64},"context_lines":{"type":"integer","minimum":0,"maximum":200},"include_imports":{"type":"boolean"}},"required":["kind","value"],"additionalProperties":false}},
            "response_detail":{"type":"string","enum":["compact","standard","debug"],"default":"standard"},
            "max_chars":{"type":"integer","minimum":1,"maximum":200000}
          },
          "$schema":"http://json-schema.org/draft-07/schema#"
        }
      },
      {
        "name":"code_search",
        "title":"Deterministic Code Search",
        "description":"Search the project by text, regex, filename, symbol, references, outline, or repository map. Filename mode accepts plain substrings or * and ? wildcards. repo_map paths are strict subtree scopes. Literal text search is the default.",
        "annotations":read.clone(),
        "execution":execution.clone(),
        "inputSchema":{
          "type":"object",
          "properties":{
            "query":{"default":"","type":"string"},
            "mode":{"type":"string","enum":["literal","regex","filename","symbol","references","outline","repo_map"]},
            "paths":{"type":"array","items":{"type":"string"},"description":"Strict workspace-relative path scope. repo_map returns only directories under these paths."},
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
        "annotations":write_closed.clone(),
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
        "annotations":write_closed.clone(),
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
        "name":"code_replace_range",
        "title":"Replace Fetched Range in One File",
        "description":"Replace the complete line range selected by a code_fetch handle in exactly one file.",
        "annotations":write_closed.clone(),
        "execution":execution.clone(),
        "inputSchema":{
          "type":"object",
          "properties":{
            "path":{"type":"string"},
            "handle":{"type":"string"},
            "new_text":{"type":"string"},
            "validate":{"type":"array","items":{"type":"string"}},
            "rollback_on_failure":{"type":"boolean"}
          },
          "required":["path","handle","new_text"],
          "additionalProperties":false,
          "$schema":"http://json-schema.org/draft-07/schema#"
        }
      },
      {
        "name":"code_insert",
        "title":"Insert Text in One File",
        "description":"Insert text before, after, or inside one named symbol in exactly one file.",
        "annotations":write_closed.clone(),
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
        "annotations":destructive_closed.clone(),
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
        "annotations":write_closed.clone(),
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
        "name":"code_preview",
        "title":"Preview Code Transaction",
        "description":"Preview a multi-file edit transaction and return the diff without writing files.",
        "annotations":read.clone(),
        "execution":execution.clone(),
        "inputSchema":{
          "type":"object",
          "properties":{
            "changes":{"type":"array","items":{"type":"object"}},
            "snapshot_id":{"type":"string"}
          },
          "required":["changes"],
          "$schema":"http://json-schema.org/draft-07/schema#"
        }
      },
      {
        "name":"code_transaction",
        "title":"Apply Code Transaction",
        "description":"Apply a multi-file edit transaction through the same precondition, validation, diff, and rollback engine as the narrow write tools.",
        "annotations":write_closed.clone(),
        "execution":execution.clone(),
        "inputSchema":{
          "type":"object",
          "properties":{
            "changes":{"type":"array","items":{"type":"object"}},
            "snapshot_id":{"type":"string"},
            "validate":{"type":"array","items":{"type":"string"}},
            "rollback_on_failure":{"type":"boolean"}
          },
          "required":["changes"],
          "$schema":"http://json-schema.org/draft-07/schema#"
        }
      }
    ]);
    let mut git_tools = json!([
      {
        "name":"git_status",
        "title":"Git Status",
        "description":"Show the working tree status: staged, unstaged, untracked, and partially staged files.",
        "annotations":read.clone(),
        "execution":execution.clone(),
        "inputSchema":{
          "type":"object",
          "properties":{},
          "$schema":"http://json-schema.org/draft-07/schema#"
        }
      },
      {
        "name":"git_diff",
        "title":"Git Diff",
        "description":"Show the diff for the working tree or, with staged=true, the staged index. Limit to specific paths when given.",
        "annotations":read.clone(),
        "execution":execution.clone(),
        "inputSchema":{
          "type":"object",
          "properties":{
            "paths":{"type":"array","items":{"type":"string"}},
            "staged":{"type":"boolean","default":false},
            "max_chars":{"type":"integer","minimum":1,"maximum":200000}
          },
          "$schema":"http://json-schema.org/draft-07/schema#"
        }
      },
      {
        "name":"git_log",
        "title":"Git Log",
        "description":"Show recent commit history.",
        "annotations":read.clone(),
        "execution":execution.clone(),
        "inputSchema":{
          "type":"object",
          "properties":{
            "limit":{"type":"integer","minimum":1,"maximum":200,"default":20}
          },
          "$schema":"http://json-schema.org/draft-07/schema#"
        }
      },
      {
        "name":"git_show",
        "title":"Git Show",
        "description":"Show the patch for one commit ref (default HEAD).",
        "annotations":read.clone(),
        "execution":execution.clone(),
        "inputSchema":{
          "type":"object",
          "properties":{
            "ref":{"type":"string","default":"HEAD"},
            "max_chars":{"type":"integer","minimum":1,"maximum":200000}
          },
          "$schema":"http://json-schema.org/draft-07/schema#"
        }
      },
      {
        "name":"git_blame",
        "title":"Git Blame",
        "description":"Show line-by-line authorship for one path, optionally bounded by start_line and end_line.",
        "annotations":read.clone(),
        "execution":execution.clone(),
        "inputSchema":{
          "type":"object",
          "properties":{
            "paths":{"type":"array","items":{"type":"string"}},
            "start_line":{"type":"integer","minimum":1},
            "end_line":{"type":"integer","minimum":1},
            "max_chars":{"type":"integer","minimum":1,"maximum":200000}
          },
          "required":["paths"],
          "$schema":"http://json-schema.org/draft-07/schema#"
        }
      },
      {
        "name":"git_preflight",
        "title":"Git Preflight",
        "description":"Return staged_files, partially_staged_files, and the cached staged diff without committing.",
        "annotations":read.clone(),
        "execution":execution.clone(),
        "inputSchema":{
          "type":"object",
          "properties":{},
          "$schema":"http://json-schema.org/draft-07/schema#"
        }
      },
      {
        "name":"git_stage",
        "title":"Git Stage",
        "description":"Stage the given paths into the index.",
        "annotations":write_closed.clone(),
        "execution":execution.clone(),
        "inputSchema":{
          "type":"object",
          "properties":{
            "paths":{"type":"array","items":{"type":"string"}}
          },
          "required":["paths"],
          "$schema":"http://json-schema.org/draft-07/schema#"
        }
      },
      {
        "name":"git_commit",
        "title":"Git Commit",
        "description":"Commit the currently staged changes with the given message.",
        "annotations":write_closed.clone(),
        "execution":execution.clone(),
        "inputSchema":{
          "type":"object",
          "properties":{
            "message":{"type":"string"}
          },
          "required":["message"],
          "$schema":"http://json-schema.org/draft-07/schema#"
        }
      },
      {
        "name":"git_restore",
        "title":"Git Restore",
        "description":"Discard changes for the given paths, restoring them from the index or HEAD. Requires confirm=true because it overwrites local changes.",
        "annotations":destructive_closed.clone(),
        "execution":execution.clone(),
        "inputSchema":{
          "type":"object",
          "properties":{
            "paths":{"type":"array","items":{"type":"string"}},
            "staged":{"type":"boolean","default":false},
            "confirm":{"type":"boolean"}
          },
          "required":["paths","confirm"],
          "$schema":"http://json-schema.org/draft-07/schema#"
        }
      },
      {
        "name":"git_push",
        "title":"Git Push",
        "description":"Push commits to a remote (default origin) and optional branch. This reaches the network.",
        "annotations":write_open.clone(),
        "execution":execution.clone(),
        "inputSchema":{
          "type":"object",
          "properties":{
            "remote":{"type":"string","description":"Push remote name (default: origin)"},
            "branch":{"type":"string","description":"Branch to push (default: current branch)"}
          },
          "$schema":"http://json-schema.org/draft-07/schema#"
        }
      }
    ]);
    let mut bash_tools = json!([
      {
        "name":"bash",
        "title":"Run Bash Command",
        "description":"Run a Bash command as the CodeWeave OS user. This is trusted-client functionality, not a sandbox.",
        "annotations":destructive_open.clone(),
        "execution":execution.clone(),
        "inputSchema":{
          "type":"object",
          "properties":{
            "command":{"type":"string","minLength":1,"description":"Command string passed to the configured Bash executable with -c."},
            "cwd":{"type":"string","description":"Existing workspace-relative directory. Defaults to the workspace root."},
            "background":{"type":"boolean","default":false},
            "timeout_ms":{"type":"integer","minimum":1}
          },
          "required":["command"],
          "$schema":"http://json-schema.org/draft-07/schema#"
        }
      },
      {
        "name":"bash_status",
        "title":"Bash Run Status",
        "description":"Return live or completed state and the retained output tail for a Bash run.",
        "annotations":read.clone(),
        "execution":execution.clone(),
        "inputSchema":{
          "type":"object",
          "properties":{
            "run_id":{"type":"string","minLength":1}
          },
          "required":["run_id"],
          "$schema":"http://json-schema.org/draft-07/schema#"
        }
      },
      {
        "name":"bash_output",
        "title":"Bash Run Output",
        "description":"Page retained combined, stdout, or stderr output for a Bash run.",
        "annotations":read.clone(),
        "execution":execution.clone(),
        "inputSchema":{
          "type":"object",
          "properties":{
            "run_id":{"type":"string","minLength":1},
            "stream":{"type":"string","enum":["combined","stdout","stderr"]},
            "continuation":{"type":"string"}
          },
          "required":["run_id"],
          "$schema":"http://json-schema.org/draft-07/schema#"
        }
      },
      {
        "name":"bash_cancel",
        "title":"Cancel Bash Run",
        "description":"Cancel a running background Bash run. Partial output is retained.",
        "annotations":write_closed.clone(),
        "execution":execution.clone(),
        "inputSchema":{
          "type":"object",
          "properties":{
            "run_id":{"type":"string","minLength":1}
          },
          "required":["run_id"],
          "$schema":"http://json-schema.org/draft-07/schema#"
        }
      }
    ]);

    for item in git_tools
        .as_array_mut()
        .expect("git tools must be an array")
        .iter_mut()
        .chain(
            bash_tools
                .as_array_mut()
                .expect("Bash tools must be an array")
                .iter_mut(),
        )
    {
        item["inputSchema"]["additionalProperties"] = Value::Bool(false);
    }

    advertised
        .as_array_mut()
        .expect("core tools must be an array")
        .append(
            git_tools
                .as_array_mut()
                .expect("git tools must be an array"),
        );
    advertised
        .as_array_mut()
        .expect("core tools must be an array")
        .append(
            bash_tools
                .as_array_mut()
                .expect("Bash tools must be an array"),
        );
    advertised
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
    // A CodeWeave MCP session owns exactly one active repository. Legacy workspace_id
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
    Json(
        json!({"ok":true,"name":SERVER_NAME,"version":env!("CARGO_PKG_VERSION"),"transport":"http","auth":state.server.auth_mode}),
    )
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

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    init_tracing();
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
        .dispatch(SessionKey::stdio(), "initialize", &config)
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
        let expected_annotations = [
            ("workspace", true, false, true, false),
            ("code_context", true, false, true, false),
            ("code_capabilities", true, false, true, false),
            ("code_fetch", true, false, true, false),
            ("code_search", true, false, true, false),
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

        let replace_range = prepare(
            &manager,
            &config,
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
            let prepared = prepare(&manager, &config, method, json!({"action": "spoofed"}))
                .await
                .unwrap();
            assert_eq!(prepared["action"], action, "{method}");
        }

        let bash = prepare(&manager, &config, "bash", json!({"command": "printf test"}))
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
            let prepared = prepare(&manager, &config, method, input).await.unwrap();
            assert!(prepared.get("action").is_none(), "{method}");
        }
        assert!(prepare(&manager, &config, "bash", json!({"command": "  "}))
            .await
            .is_err());
        assert!(prepare(
            &manager,
            &config,
            "bash",
            json!({"command": "printf test", "unknown": true})
        )
        .await
        .is_err());
        assert!(prepare(
            &manager,
            &config,
            "bash_status",
            json!({"run_id": "run_test", "action": "cancel"})
        )
        .await
        .is_err());
        let removed = prepare(&manager, &config, "task_run", json!({"profile": "test"}))
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
            let prepared = prepare(&manager, &config, "bash", input).await.unwrap();
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
            "policy": {"maxFileBytes": 1000000, "maxContextChars": 50000, "maxSearchResults": 100, "bash": {"enabled": false}},
            "cache_root": cache.path()
        });
        manager
            .dispatch(SessionKey::stdio(), "initialize", &daemon_config)
            .await
            .unwrap();
        manager
            .dispatch(
                SessionKey::stdio(),
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
                SessionKey::stdio(),
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
