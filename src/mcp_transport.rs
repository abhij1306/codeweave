use std::sync::Arc;

use anyhow::Result;
use axum::{
    extract::{Request, State},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use rmcp::{
    model::{
        CallToolRequestParams, CallToolResult, ListToolsResult, PaginatedRequestParams,
        ServerCapabilities, ServerInfo,
    },
    service::RequestContext,
    transport::{
        stdio,
        streamable_http_server::{
            session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
        },
    },
    ErrorData as McpError, RoleServer, ServerHandler, ServiceExt,
};

use crate::{
    health, is_loopback, live, prepare, tool_failure, tool_result, tools, AppState, Cli,
    SERVER_NAME,
};

const INSTRUCTIONS: &str = "Use code_context for unfamiliar code, code_search for exact discovery, code_fetch for exact reads, the single-operation code_write/code_replace/code_insert/code_delete/code_rename tools for changes, run for builds/tests, and git for repository operations. CodeWeave manages one active repository per server process; call workspace with an absolute path to switch it explicitly.";

#[derive(Clone)]
pub(crate) struct CodeWeaveMcp {
    state: AppState,
}

impl CodeWeaveMcp {
    pub(crate) fn new(state: AppState) -> Self {
        Self { state }
    }
}

impl ServerHandler for CodeWeaveMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(INSTRUCTIONS)
            .with_server_info(rmcp::model::Implementation::new(
                SERVER_NAME,
                env!("CARGO_PKG_VERSION"),
            ))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        serde_json::from_value(serde_json::json!({ "tools": tools() }))
            .map_err(|error| McpError::internal_error(error.to_string(), None))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let request_value = serde_json::to_value(request)
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        let name = request_value
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let args = request_value
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));

        if ![
            "workspace",
            "code_context",
            "code_fetch",
            "code_search",
            "code_write",
            "code_replace",
            "code_insert",
            "code_delete",
            "code_rename",
            "git",
            "run",
        ]
        .contains(&name)
        {
            return Err(McpError::invalid_params(
                format!("Unknown tool: {name}"),
                None,
            ));
        }

        let result = match prepare(&self.state.manager, &self.state.config, name, args).await {
            Ok(prepared) => self.state.manager.dispatch(name, &prepared).await,
            Err(error) => Err(error),
        };
        let value = match result {
            Ok(value) => tool_result(value),
            Err(error) => tool_failure(error),
        };
        serde_json::from_value(value)
            .map_err(|error| McpError::internal_error(error.to_string(), None))
    }
}

async fn require_auth(State(state): State<AppState>, request: Request, next: Next) -> Response {
    if crate::authorized(request.headers(), &state) {
        next.run(request).await
    } else {
        (
            axum::http::StatusCode::UNAUTHORIZED,
            axum::Json(serde_json::json!({"error":"unauthorized"})),
        )
            .into_response()
    }
}

pub(crate) async fn run_http(mut state: AppState, cli: &Cli) -> Result<()> {
    if let Some(host) = &cli.host {
        state.server.host = host.clone();
    }
    if let Some(port) = cli.port {
        state.server.port = port;
    }
    if state.server.auth_mode == "none" && !is_loopback(&state.server.host) {
        anyhow::bail!("refusing unauthenticated HTTP on non-loopback host")
    }

    let mut allowed_hosts = vec![
        state.server.host.clone(),
        format!("{}:{}", state.server.host, state.server.port),
        "localhost".to_owned(),
        format!("localhost:{}", state.server.port),
        "127.0.0.1".to_owned(),
        format!("127.0.0.1:{}", state.server.port),
        "::1".to_owned(),
    ];
    allowed_hosts.extend(state.server.allowed_hosts.iter().cloned());
    allowed_hosts.sort();
    allowed_hosts.dedup();

    let mut config = StreamableHttpServerConfig::default();
    config.stateful_mode = state.server.stateful_mode;
    config.json_response = state.server.json_response;
    config.allowed_hosts = allowed_hosts;
    config.allowed_origins = state.server.allowed_origins.clone();
    let service: StreamableHttpService<CodeWeaveMcp, LocalSessionManager> =
        StreamableHttpService::new(
            {
                let state = state.clone();
                move || Ok::<_, std::io::Error>(CodeWeaveMcp::new(state.clone()))
            },
            Arc::new(LocalSessionManager::default()),
            config,
        );

    let mcp_routes = Router::new()
        .nest_service("/mcp", service)
        .layer(middleware::from_fn_with_state(state.clone(), require_auth));
    let app = Router::new()
        .route("/live", get(live))
        .route("/health", get(health))
        .merge(mcp_routes)
        .layer(axum::extract::DefaultBodyLimit::max(4 * 1024 * 1024))
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state.clone());

    let address = format!("{}:{}", state.server.host, state.server.port);
    let listener = tokio::net::TcpListener::bind(&address).await?;
    eprintln!("{SERVER_NAME} listening on http://{address}/mcp");
    axum::serve(listener, app).await?;
    Ok(())
}

pub(crate) async fn run_stdio(state: AppState) -> Result<()> {
    let service = CodeWeaveMcp::new(state).serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
