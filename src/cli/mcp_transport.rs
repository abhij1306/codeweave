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
        CallToolRequestParams, CallToolResponse, CallToolResult, ListToolsResult,
        PaginatedRequestParams, ServerCapabilities, ServerInfo,
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
use serde_json::Value;

use super::args::Cli;
use super::config::ServerConfig;
use super::server::{authorized, health, is_loopback, live, tool_failure, tool_result, AppState};
use super::SERVER_NAME;
use codeweave_rust::manager;
use codeweave_rust::tools::ToolAccess;

const INSTRUCTIONS: &str = "Use code_retrieve for repository discovery and exact reads, code_intelligence for semantic operations, the narrow edit tools for one-file changes, and code_preview/code_transaction for coordinated changes. Run commands with bash and use the narrowly scoped Git tools for repository operations. CodeWeave serves one shared repository fixed in config.json.";
const MAX_MCP_REQUEST_BYTES: usize = 4 * 1024 * 1024;
const MAX_BUFFERED_MCP_REQUEST_BYTES: usize = 16 * 1024 * 1024;
const MAX_CONCURRENT_MCP_BODY_COLLECTIONS: usize =
    MAX_BUFFERED_MCP_REQUEST_BYTES / MAX_MCP_REQUEST_BYTES;
const MAX_HTTP_CONNECTIONS: usize = 128;

#[derive(Clone)]
struct McpBodyBudget(Arc<tokio::sync::Semaphore>);

impl McpBodyBudget {
    fn new(permits: usize) -> Self {
        Self(Arc::new(tokio::sync::Semaphore::new(permits)))
    }
}

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
        serde_json::from_value(
            serde_json::json!({ "tools": self.state.tool_access.list_payload() }),
        )
        .map_err(|error| McpError::internal_error(error.to_string(), None))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let name = request.name.as_ref();
        let args = request
            .arguments
            .map(Value::Object)
            .unwrap_or_else(|| serde_json::json!({}));

        if !ToolAccess::is_known_tool(name) {
            return Err(McpError::invalid_params(
                format!("Unknown tool: {name}"),
                None,
            ));
        }
        let result = match manager::prepare_tool_request(name, args) {
            Ok(prepared) => self.state.manager.dispatch(name, &prepared).await,
            Err(error) => Err(error),
        };
        let value = match result {
            Ok(value) => tool_result(value),
            Err(error) => tool_failure(error),
        };
        serde_json::from_value::<CallToolResult>(value)
            .map(Into::into)
            .map_err(|error| McpError::internal_error(error.to_string(), None))
    }
}

async fn require_auth(State(state): State<AppState>, request: Request, next: Next) -> Response {
    if authorized(request.headers(), &state) {
        next.run(request).await
    } else {
        (
            axum::http::StatusCode::UNAUTHORIZED,
            axum::Json(serde_json::json!({"error":"unauthorized"})),
        )
            .into_response()
    }
}

async fn limit_mcp_body(
    State(budget): State<McpBodyBudget>,
    request: Request,
    next: Next,
) -> Response {
    let _permit = match budget.0.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            return (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                axum::Json(serde_json::json!({
                    "error": "MCP request body capacity is temporarily exhausted"
                })),
            )
                .into_response();
        }
    };
    let (parts, body) = request.into_parts();
    match axum::body::to_bytes(body, MAX_MCP_REQUEST_BYTES).await {
        Ok(bytes) => {
            next.run(Request::from_parts(parts, axum::body::Body::from(bytes)))
                .await
        }
        Err(_) => (
            axum::http::StatusCode::PAYLOAD_TOO_LARGE,
            axum::Json(serde_json::json!({
                "error": "request body exceeds the 4 MiB MCP limit"
            })),
        )
            .into_response(),
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

    let app = build_http_app(state.clone());

    let address = format!("{}:{}", state.server.host, state.server.port);
    let listener = tokio::net::TcpListener::bind(&address).await?;
    eprintln!("{SERVER_NAME} listening on http://{address}/mcp");
    serve_with_idle_timeout(listener, app, state.server.idle_timeout_ms).await
}

fn build_http_app(state: AppState) -> Router {
    build_http_app_with_body_budget(
        state,
        McpBodyBudget::new(MAX_CONCURRENT_MCP_BODY_COLLECTIONS),
    )
}

fn build_http_app_with_body_budget(state: AppState, body_budget: McpBodyBudget) -> Router {
    let allowed_hosts = configured_allowed_hosts(&state.server);

    let mut config = StreamableHttpServerConfig::default();
    config.legacy_session_mode = false;
    config.json_response = true;
    config.sse_retry = None;
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
        .layer(middleware::from_fn_with_state(body_budget, limit_mcp_body))
        .layer(middleware::from_fn_with_state(state.clone(), require_auth));
    Router::new()
        .route("/live", get(live))
        .route("/health", get(health))
        .merge(mcp_routes)
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state)
}

fn is_accept_resource_exhaustion(error: &std::io::Error) -> bool {
    // Unix EMFILE/ENFILE and Windows ERROR_TOO_MANY_OPEN_FILES/WSAEMFILE.
    matches!(error.raw_os_error(), Some(4 | 23 | 24 | 10024))
}

/// Serves the app on a manual hyper accept loop so we can bound idle keep-alive
/// connection lifetime — something `axum::serve` does not expose.
///
/// JSON responses do not close the underlying TCP connection.
/// Hyper keeps an idle keep-alive socket open indefinitely, so a tunnel/proxy
/// (ngrok, the OpenAI connector) holds it until its own ~90s deadline and
/// reports that as the connection's p50/p90 lifetime — even though every
/// request finished in milliseconds. Uvicorn (Serena's server) closes idle
/// keep-alive connections after ~5s, which is why its dashboard reads ~5s.
///
/// `header_read_timeout` is hyper's equivalent of Uvicorn's `timeout_keep_alive`:
/// it bounds the time spent waiting to read a request head, including the wait
/// for the *next* request on a kept-alive connection, so an idle socket is
/// closed after the timeout. It resets per request and does not interrupt an
/// in-flight request/response (e.g. a long foreground `bash` POST).
///
/// HTTP/2 is disabled at the loopback origin because supported clients and
/// tunnels speak HTTP/1.1 there, while h2c would bypass the HTTP/1 request-head
/// idle bound. A fixed connection semaphore also bounds sockets and tasks before
/// application authentication runs.
async fn serve_with_idle_timeout(
    listener: tokio::net::TcpListener,
    app: Router,
    idle_timeout_ms: u64,
) -> Result<()> {
    use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
    use hyper_util::server::conn::auto::Builder as ConnBuilder;
    use hyper_util::service::TowerToHyperService;

    let mut builder = ConnBuilder::new(TokioExecutor::new());
    if idle_timeout_ms > 0 {
        builder
            .http1()
            .timer(TokioTimer::new())
            .header_read_timeout(std::time::Duration::from_millis(idle_timeout_ms));
    }
    let builder = Arc::new(builder.http1_only());
    let connection_limit = Arc::new(tokio::sync::Semaphore::new(MAX_HTTP_CONNECTIONS));
    let mut accept_backoff = std::time::Duration::from_millis(10);

    loop {
        let permit = connection_limit
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| anyhow::anyhow!("HTTP connection limiter closed"))?;
        let (stream, _addr) = match listener.accept().await {
            Ok(accepted) => {
                accept_backoff = std::time::Duration::from_millis(10);
                accepted
            }
            Err(error) => {
                let resource_exhausted = is_accept_resource_exhaustion(&error);
                let min_backoff = if resource_exhausted {
                    std::time::Duration::from_millis(100)
                } else {
                    std::time::Duration::from_millis(10)
                };
                let max_backoff = if resource_exhausted {
                    std::time::Duration::from_secs(5)
                } else {
                    std::time::Duration::from_secs(1)
                };
                accept_backoff = accept_backoff.max(min_backoff).min(max_backoff);
                eprintln!(
                    "{SERVER_NAME} listener accept failed ({}): {error}; retrying in {} ms",
                    if resource_exhausted {
                        "file-descriptor exhaustion"
                    } else {
                        "transient error"
                    },
                    accept_backoff.as_millis()
                );
                tokio::time::sleep(accept_backoff).await;
                accept_backoff = accept_backoff.saturating_mul(2).min(max_backoff);
                continue;
            }
        };
        // Disable Nagle: without this the final small TCP segment of each
        // response waits on the peer's delayed-ACK timer (~200ms on Windows).
        let _ = stream.set_nodelay(true);

        let io = TokioIo::new(stream);
        let service = TowerToHyperService::new(app.clone());
        let builder = builder.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let _ = builder.serve_connection_with_upgrades(io, service).await;
        });
    }
}

fn configured_allowed_hosts(server: &ServerConfig) -> Vec<String> {
    if server.allowed_hosts.iter().any(|host| host.trim() == "*") {
        return Vec::new();
    }
    let mut allowed_hosts = Vec::new();
    extend_host_authorities(&mut allowed_hosts, &server.host, server.port);
    extend_host_authorities(&mut allowed_hosts, "localhost", server.port);
    extend_host_authorities(&mut allowed_hosts, "127.0.0.1", server.port);
    extend_host_authorities(&mut allowed_hosts, "::1", server.port);
    allowed_hosts.extend(server.allowed_hosts.iter().cloned());
    allowed_hosts.sort();
    allowed_hosts.dedup();
    allowed_hosts
}

fn extend_host_authorities(hosts: &mut Vec<String>, host: &str, port: u16) {
    let host = host.trim();
    if host.is_empty() {
        return;
    }
    if is_ipv6_literal(host) {
        let bare = host.trim_start_matches('[').trim_end_matches(']');
        hosts.push(format!("[{bare}]"));
        hosts.push(format!("[{bare}]:{port}"));
    } else {
        hosts.push(host.to_owned());
        hosts.push(format!("{host}:{port}"));
    }
}

fn is_ipv6_literal(host: &str) -> bool {
    let value = host.trim_start_matches('[').trim_end_matches(']');
    value.contains(':')
}

pub(crate) async fn run_stdio(state: AppState) -> Result<()> {
    let service = CodeWeaveMcp::new(state).serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::config::ServerConfig;
    use super::super::server::resolve_tool_access;
    use super::*;
    use axum::{
        body::Body,
        http::{header, Request, StatusCode},
    };
    use codeweave_rust::manager::Application;
    use tower::ServiceExt;

    fn test_state() -> AppState {
        AppState {
            manager: Arc::new(Application::default()),
            server: ServerConfig {
                host: "127.0.0.1".to_owned(),
                port: 8813,
                auth_mode: "bearer".to_owned(),
                token_file: ".mcp-token".to_owned(),
                allowed_hosts: Vec::new(),
                allowed_origins: Vec::new(),
                idle_timeout_ms: 5000,
            },
            token: Some(Arc::new(b"test-token".to_vec())),
            tool_access: Arc::new(resolve_tool_access()),
            instance_id: Arc::from("transport-test"),
        }
    }

    #[tokio::test]
    async fn real_mcp_route_rejects_body_over_four_mib() {
        let request = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header(header::HOST, "127.0.0.1:8813")
            .header(header::AUTHORIZATION, "Bearer test-token")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json, text/event-stream")
            .body(Body::from(vec![b' '; MAX_MCP_REQUEST_BYTES + 1]))
            .unwrap();

        let response = build_http_app(test_state()).oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn authentication_runs_before_oversized_body_collection() {
        let request = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header(header::HOST, "127.0.0.1:8813")
            .header(header::AUTHORIZATION, "Bearer wrong-token")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(vec![b' '; MAX_MCP_REQUEST_BYTES + 1]))
            .unwrap();

        let response = build_http_app(test_state()).oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn small_authenticated_mcp_request_reaches_service() {
        let request = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header(header::HOST, "127.0.0.1:8813")
            .header(header::AUTHORIZATION, "Bearer test-token")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json, text/event-stream")
            .body(Body::from(
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"transport-test","version":"1"}}}"#,
            ))
            .unwrap();

        let response = build_http_app(test_state()).oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn excess_request_is_rejected_when_body_budget_is_saturated() {
        let budget = McpBodyBudget::new(1);
        let permit = budget.0.clone().try_acquire_owned().unwrap();
        let request = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header(header::HOST, "127.0.0.1:8813")
            .header(header::AUTHORIZATION, "Bearer test-token")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .unwrap();

        let response = build_http_app_with_body_budget(test_state(), budget)
            .oneshot(request)
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        drop(permit);
    }

    #[test]
    fn origin_connection_capacity_is_bounded() {
        let limiter = tokio::sync::Semaphore::new(MAX_HTTP_CONNECTIONS);
        let permits = (0..MAX_HTTP_CONNECTIONS)
            .map(|_| limiter.try_acquire())
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        assert!(limiter.try_acquire().is_err());
        drop(permits);
        assert!(limiter.try_acquire().is_ok());
    }

    #[test]
    fn accept_resource_exhaustion_classifies_common_platform_codes() {
        for code in [4, 23, 24, 10024] {
            assert!(is_accept_resource_exhaustion(
                &std::io::Error::from_raw_os_error(code)
            ));
        }
        assert!(!is_accept_resource_exhaustion(
            &std::io::Error::from_raw_os_error(111)
        ));
    }

    #[test]
    fn wildcard_allowed_host_disables_rmcp_host_guard_for_trusted_tunnels() {
        let server = ServerConfig {
            host: "127.0.0.1".to_owned(),
            port: 8813,
            auth_mode: "bearer".to_owned(),
            token_file: ".mcp-token".to_owned(),
            allowed_hosts: vec!["*".to_owned()],
            allowed_origins: Vec::new(),
            idle_timeout_ms: 5000,
        };

        assert!(configured_allowed_hosts(&server).is_empty());
    }

    #[test]
    fn configured_allowed_hosts_keeps_loopback_defaults() {
        let server = ServerConfig {
            host: "127.0.0.1".to_owned(),
            port: 8813,
            auth_mode: "bearer".to_owned(),
            token_file: ".mcp-token".to_owned(),
            allowed_hosts: vec!["example.ngrok-free.dev".to_owned()],
            allowed_origins: Vec::new(),
            idle_timeout_ms: 5000,
        };

        let hosts = configured_allowed_hosts(&server);

        assert!(hosts.contains(&"127.0.0.1:8813".to_owned()));
        assert!(hosts.contains(&"example.ngrok-free.dev".to_owned()));
    }

    #[test]
    fn configured_allowed_hosts_brackets_ipv6_loopback_authorities() {
        let server = ServerConfig {
            host: "::1".to_owned(),
            port: 8813,
            auth_mode: "bearer".to_owned(),
            token_file: ".mcp-token".to_owned(),
            allowed_hosts: Vec::new(),
            allowed_origins: Vec::new(),
            idle_timeout_ms: 5000,
        };

        let hosts = configured_allowed_hosts(&server);

        assert!(hosts.contains(&"[::1]".to_owned()));
        assert!(hosts.contains(&"[::1]:8813".to_owned()));
        assert!(!hosts.contains(&"::1:8813".to_owned()));
        let authority = axum::http::uri::Authority::try_from("[::1]:8813").unwrap();
        assert_eq!(authority.host(), "[::1]");
        assert_eq!(authority.host().trim_matches(['[', ']']), "::1");
        assert_eq!(authority.port_u16(), Some(8813));
    }
}
