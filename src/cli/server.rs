use anyhow::Result;
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use codeweave_rust::{manager, model, tools};
use manager::Application;
use serde_json::{json, Value};
use std::sync::Arc;
use subtle::ConstantTimeEq;
use tracing_subscriber::EnvFilter;

use super::args::{Cli, Transport};
use super::config::{
    config_relative_path, load_config, load_or_create_bearer_token, validate_auth_mode,
    ServerConfig,
};
use super::mcp_transport;
use super::SERVER_NAME;

#[derive(Clone)]
pub(super) struct AppState {
    pub(super) manager: Arc<Application>,
    pub(super) server: ServerConfig,
    pub(super) token: Option<Arc<Vec<u8>>>,
    pub(super) tool_access: Arc<tools::ToolAccess>,
    pub(super) instance_id: Arc<str>,
}

pub(super) fn resolve_tool_access() -> tools::ToolAccess {
    tools::fixed_access()
}

pub(super) fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("codeweave_rust=info,tower_http=info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .init();
}

pub(super) fn is_loopback(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "::1" | "localhost")
}

pub(super) fn authorized(headers: &HeaderMap, state: &AppState) -> bool {
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
pub(super) fn tool_result(value: Value) -> Value {
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

pub(super) fn tool_failure(error: model::AppError) -> Value {
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

pub(super) async fn live(State(state): State<AppState>) -> Json<Value> {
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

pub(super) async fn health(State(state): State<AppState>) -> impl IntoResponse {
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

/// Load the config, initialize the repository, and start the selected transport.
pub(super) async fn run_serve(cli: Cli) -> Result<()> {
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
