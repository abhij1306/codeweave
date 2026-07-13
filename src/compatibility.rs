use crate::manager::WorkspaceManager;
use crate::tools::ToolAccess;
use codeweave_rust::model::{AppError, AppResult};
use serde_json::{json, Map, Value};
use std::sync::Arc;

const LEGACY_ROUTING_FIELDS: &[&str] = &["workspace_id", "workspace"];
const LEGACY_IGNORED_FIELDS: &[&str] = &["rollback_on_failure"];

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

/// Normalize compatibility inputs at the single public request boundary.
///
/// The manager and configuration arguments are retained for a stable internal
/// call signature while all current compatibility behavior is intentionally
/// local and deterministic.
pub(crate) async fn prepare(
    _manager: &Arc<WorkspaceManager>,
    _config: &Value,
    tool_access: &ToolAccess,
    method: &str,
    input: Value,
) -> AppResult<Value> {
    if matches!(
        method,
        "task_run" | "task_status" | "task_output" | "task_cancel"
    ) {
        return Err(AppError::details(
            "METHOD_NOT_FOUND",
            "Task profile tools were removed; use bash, bash_status, bash_output, or bash_cancel",
            json!({"method": method}),
        ));
    }
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
            return Err(AppError::details(
                "VALIDATE_UNAVAILABLE",
                "Edit 'validate' commands require bash, which is unavailable under the active tool profile or policy",
                json!({"method": method}),
            ));
        }
    }

    let mut params = object(input);
    for field in LEGACY_ROUTING_FIELDS
        .iter()
        .chain(LEGACY_IGNORED_FIELDS.iter())
    {
        params.remove(*field);
    }
    if method == "code_preview" {
        params.insert("preview".into(), Value::Bool(true));
    }
    if is_code_mutation(method) {
        normalize_code_mutation(method, &mut params);
    }
    if let Some(action) = tool_action(method) {
        params.insert("action".into(), Value::String(action.into()));
    }
    Ok(Value::Object(params))
}
