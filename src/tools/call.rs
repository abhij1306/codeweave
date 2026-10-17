//! One MCP tool call, shared by every transport: resolve the tool, parse and
//! run its request, and shape the outcome as an MCP `CallToolResult`.
//!
//! A tool error is a successful MCP response with `isError: true` and the
//! error in `structuredContent.error`, including whether and how the client may
//! retry. Only an unknown tool name is a JSON-RPC error.

use super::{find, mcp_tools};
use crate::manager::Application;
use crate::model::{AppError, ErrorBody};
use rmcp::model::{CallToolResult, ContentBlock, ListToolsResult};
use rmcp::ErrorData;
use serde_json::{json, Map, Value};
use std::sync::Arc;

/// Run the tool called `name` with the client's `arguments`.
pub async fn call_tool(
    app: Arc<Application>,
    name: &str,
    arguments: Option<Map<String, Value>>,
) -> Result<CallToolResult, ErrorData> {
    let tool = find(name)
        .ok_or_else(|| ErrorData::invalid_params(format!("Unknown tool: {name}"), None))?;
    let arguments = Value::Object(arguments.unwrap_or_default());
    Ok(match tool.call(app, arguments).await {
        Ok(value) => success(value),
        Err(error) => failure(error),
    })
}

/// The `tools/list` result.
pub fn list_tools() -> ListToolsResult {
    let mut result = ListToolsResult::with_all_items(mcp_tools().to_vec());
    // The rmcp handler adds `resultType` for peers that require it.
    result.result_type = None;
    result
}

fn success(value: Value) -> CallToolResult {
    let structured = if value.is_object() {
        value
    } else {
        json!({ "value": value })
    };
    result(structured, None)
}

fn failure(error: AppError) -> CallToolResult {
    let body = error.0;
    let retry = Retry::of(&body);
    let structured = json!({
        "error": {
            "code": body.code,
            "message": body.message,
            "retryable": retry.retryable,
            "retry_kind": retry.kind,
            "details": body.details
        }
    });
    result(structured, Some(true))
}

fn result(structured: Value, is_error: Option<bool>) -> CallToolResult {
    let text = serde_json::to_string(&structured).unwrap_or_else(|_| "{}".into());
    let mut result = CallToolResult::default();
    result.result_type = None;
    result.content = vec![ContentBlock::text(text)];
    result.structured_content = Some(structured);
    result.is_error = is_error;
    result
}

/// Errors whose request may be retried unchanged: the state it was checked
/// against moved on, and a fresh attempt re-reads it.
const RETRY_SAME_REQUEST: [&str; 4] = [
    "STALE_SNAPSHOT",
    "STALE_FILE",
    "STALE_HANDLE",
    "STALE_CONTINUATION",
];

/// How a client may retry a failed call. An error's own `details.retryable`
/// and `details.retry_kind` take precedence over the code-based default.
struct Retry {
    retryable: bool,
    kind: String,
}

impl Retry {
    fn of(body: &ErrorBody) -> Self {
        let details = body.details.as_ref();
        let stale = RETRY_SAME_REQUEST.contains(&body.code.as_str());
        let retryable = stale
            || details
                .and_then(|details| details.get("retryable"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
        let kind = details
            .and_then(|details| details.get("retry_kind"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| {
                if stale {
                    "retry_same_request"
                } else if retryable {
                    "retry_with_changes"
                } else {
                    "not_retryable"
                }
                .to_owned()
            });
        Self { retryable, kind }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire(result: &CallToolResult) -> Value {
        serde_json::to_value(result).unwrap()
    }

    #[test]
    fn a_tool_value_is_returned_as_text_and_structured_content() {
        assert_eq!(
            wire(&success(json!({"applied": true}))),
            json!({
                "content": [{"type": "text", "text": "{\"applied\":true}"}],
                "structuredContent": {"applied": true}
            })
        );
        assert_eq!(
            wire(&success(json!(3)))["structuredContent"],
            json!({"value": 3})
        );
    }

    #[test]
    fn a_stale_error_is_retryable_unchanged() {
        let result = failure(AppError::new("STALE_FILE", "file changed"));

        assert_eq!(
            wire(&result),
            json!({
                "content": [{
                    "type": "text",
                    "text": "{\"error\":{\"code\":\"STALE_FILE\",\"details\":null,\"message\":\"file changed\",\"retry_kind\":\"retry_same_request\",\"retryable\":true}}"
                }],
                "structuredContent": {"error": {
                    "code": "STALE_FILE",
                    "message": "file changed",
                    "retryable": true,
                    "retry_kind": "retry_same_request",
                    "details": null
                }},
                "isError": true
            })
        );
    }

    #[test]
    fn error_details_decide_retryability_for_other_codes() {
        let kinds = [
            AppError::new("GIT_FAILED", "failed"),
            AppError::details("BUSY", "busy", json!({"retryable": true})),
            AppError::details(
                "BUSY",
                "busy",
                json!({"retryable": true, "retry_kind": "retry_after_poll"}),
            ),
        ]
        .map(|error| {
            let error = &wire(&failure(error))["structuredContent"]["error"];
            (error["retryable"].clone(), error["retry_kind"].clone())
        });

        assert_eq!(
            kinds,
            [
                (json!(false), json!("not_retryable")),
                (json!(true), json!("retry_with_changes")),
                (json!(true), json!("retry_after_poll")),
            ]
        );
    }

    #[tokio::test]
    async fn an_unknown_tool_is_an_invalid_params_error() {
        let error = call_tool(Arc::new(Application::default()), "git", None)
            .await
            .unwrap_err();

        assert_eq!(error.code, rmcp::model::ErrorCode::INVALID_PARAMS);
        assert_eq!(error.message, "Unknown tool: git");
    }
}
