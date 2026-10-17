use anyhow::Result;
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use codeweave_rust::manager::Application;
use serde_json::{json, Value};
use std::sync::Arc;
use subtle::ConstantTimeEq;
use tracing_subscriber::EnvFilter;

use super::args::{Cli, Transport};
use super::config::{
    config_relative_path, load_config, load_or_create_bearer_token, AuthMode, Config, ServerConfig,
};
use super::mcp_transport;
use super::SERVER_NAME;

#[derive(Clone)]
pub struct AppState {
    pub manager: Arc<Application>,
    pub server: ServerConfig,
    pub token: Option<Arc<Vec<u8>>>,
    pub instance_id: Arc<str>,
}

pub fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("codeweave_rust=info,tower_http=info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .init();
}

pub fn is_loopback(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "::1" | "localhost")
}

pub fn authorized(headers: &HeaderMap, state: &AppState) -> bool {
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
pub async fn live(State(state): State<AppState>) -> Json<Value> {
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
    }))
}

pub async fn health(State(state): State<AppState>) -> impl IntoResponse {
    (
        StatusCode::OK,
        Json(json!({"ok": true, "gateway_ready": true, "engine": state.manager.health()})),
    )
}

/// Load the config, initialize the repository, and start the selected transport.
pub async fn run_serve(cli: Cli) -> Result<()> {
    let Config { server, daemon } = load_config(&cli.config)?;
    let token = if matches!(cli.transport, Transport::Http) && server.auth_mode == AuthMode::Bearer
    {
        let token_path = config_relative_path(&cli.config, &server.token_file);
        let token_value = load_or_create_bearer_token(&token_path)?;
        Some(Arc::new(token_value.into_bytes()))
    } else {
        None
    };
    let manager = Arc::new(Application::default());
    let init = manager
        .initialize(daemon)
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    tracing::info!(
        workspace = %init.workspace.path,
        file_count = init.file_count,
        index_ready = init.index_ready,
        bash_available = init.bash_available,
        "repository ready before transport bind"
    );
    let instance_id = manager.instance_id();
    let state = AppState {
        manager,
        server,
        token,
        instance_id,
    };
    match cli.transport {
        Transport::Http => mcp_transport::run_http(state, &cli).await,
        Transport::Stdio => mcp_transport::run_stdio(state).await,
    }
}
