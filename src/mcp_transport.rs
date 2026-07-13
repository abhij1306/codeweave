use std::sync::Arc;

use anyhow::Result;
use axum::{
    extract::{Request, State},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use futures_util::{stream, Stream, StreamExt};
use rmcp::{
    model::{
        CallToolRequestParams, CallToolResult, ClientJsonRpcMessage, Extensions, GetExtensions,
        ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerInfo,
        ServerJsonRpcMessage,
    },
    service::RequestContext,
    transport::{
        stdio,
        streamable_http_server::{
            session::{
                local::{LocalSessionManager, LocalSessionManagerError},
                RestoreOutcome, ServerSseMessage, SessionId, SessionManager,
            },
            StreamableHttpServerConfig, StreamableHttpService,
        },
    },
    ErrorData as McpError, RoleServer, ServerHandler, ServiceExt,
};
use serde_json::Value;

use crate::manager::SessionKey;
use crate::tools::ToolAccess;
use crate::{
    health, is_loopback, live, prepare, tool_failure, tool_result, AppState, Cli, SERVER_NAME,
};

const INSTRUCTIONS: &str = "Use code_capabilities to inspect supported contracts, code_context for unfamiliar code, code_search for exact discovery, code_fetch for exact reads, code_preview/code_transaction for multi-file edits, and the single-operation code_write/code_replace/code_replace_range/code_insert/code_delete/code_rename tools for narrow changes. Run commands with bash; inspect or stop retained runs with bash_status, bash_output, and bash_cancel. Bash executes as the CodeWeave OS user and is not sandboxed. Use the narrowly scoped git_status/git_diff/git_log/git_show/git_blame/git_preflight/git_stage/git_commit/git_restore/git_push tools for repository operations. CodeWeave serves one repository, fixed for the server's lifetime and configured in config.json; call workspace to inspect its summary, changes, diagnostics, or skills.";

#[derive(Clone, Debug, PartialEq, Eq)]
struct CodeWeaveSessionId(String);

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
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let name = request.name.as_ref();
        let args = request
            .arguments
            .map(Value::Object)
            .unwrap_or_else(|| serde_json::json!({}));

        // The registry is the single source of truth for callable names. An
        // unknown name is a hard MCP error; a real tool that the active profile
        // does not expose is a structured TOOL_NOT_IN_PROFILE failure so the
        // caller can tell "no such tool" from "not in this profile".
        if !ToolAccess::is_known_tool(name) {
            return Err(McpError::invalid_params(
                format!("Unknown tool: {name}"),
                None,
            ));
        }
        if !self.state.tool_access.is_allowed(name) {
            let value = tool_failure(crate::model::AppError::details(
                "TOOL_NOT_IN_PROFILE",
                format!(
                    "Tool '{name}' is not available under the active tool profile '{}'",
                    self.state.server.tool_profile
                ),
                serde_json::json!({"tool": name, "profile": self.state.server.tool_profile}),
            ));
            return serde_json::from_value(value)
                .map_err(|error| McpError::internal_error(error.to_string(), None));
        }

        let session = session_key(&context);
        let result = match prepare(
            &self.state.manager,
            &self.state.config,
            &self.state.tool_access,
            name,
            args,
        )
        .await
        {
            Ok(prepared) => self.state.manager.dispatch(session, name, &prepared).await,
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

fn session_key(context: &RequestContext<RoleServer>) -> SessionKey {
    session_key_from_extensions(&context.extensions).unwrap_or_else(SessionKey::stateless)
}

fn session_key_from_extensions(extensions: &Extensions) -> Option<SessionKey> {
    if let Some(session) = extensions.get::<CodeWeaveSessionId>() {
        return Some(SessionKey::new(format!("http:{}", session.0)));
    }

    extensions
        .get::<axum::http::request::Parts>()
        .map(|parts| session_key_from_headers(&parts.headers))
        .filter(|session| !session.is_stateless())
}

fn session_key_from_headers(headers: &axum::http::HeaderMap) -> SessionKey {
    headers
        .get("mcp-session-id")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .map(|value| SessionKey::new(format!("http:{value}")))
        .unwrap_or_else(SessionKey::stateless)
}

struct CompatibleSessionManager {
    inner: LocalSessionManager,
    manager: Arc<crate::manager::WorkspaceManager>,
}

impl Default for CompatibleSessionManager {
    fn default() -> Self {
        Self::new(Arc::new(crate::manager::WorkspaceManager::default()))
    }
}

impl CompatibleSessionManager {
    fn new(manager: Arc<crate::manager::WorkspaceManager>) -> Self {
        let mut inner = LocalSessionManager::default();
        inner.session_config.sse_retry = None;
        Self { inner, manager }
    }
}

fn compatibility_ready_event() -> ServerSseMessage {
    let message: ServerJsonRpcMessage = serde_json::from_value(serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/codeweave/ready",
        "params": {
            "transport": "streamable-http",
            "stateful": true
        }
    }))
    .expect("static compatibility notification must be valid JSON-RPC");
    ServerSseMessage::from_message(message)
}

fn attach_codeweave_session_id(message: &mut ClientJsonRpcMessage, id: &SessionId) {
    let session = CodeWeaveSessionId(id.to_string());
    match message {
        ClientJsonRpcMessage::Request(request) => {
            request.request.extensions_mut().insert(session);
        }
        ClientJsonRpcMessage::Notification(notification) => {
            notification.notification.extensions_mut().insert(session);
        }
        _ => {}
    }
}

impl SessionManager for CompatibleSessionManager {
    type Error = LocalSessionManagerError;
    type Transport = <LocalSessionManager as SessionManager>::Transport;

    async fn create_session(&self) -> Result<(SessionId, Self::Transport), Self::Error> {
        self.inner.create_session().await
    }

    async fn initialize_session(
        &self,
        id: &SessionId,
        mut message: ClientJsonRpcMessage,
    ) -> Result<ServerJsonRpcMessage, Self::Error> {
        attach_codeweave_session_id(&mut message, id);
        self.inner.initialize_session(id, message).await
    }

    async fn has_session(&self, id: &SessionId) -> Result<bool, Self::Error> {
        self.inner.has_session(id).await
    }

    async fn close_session(&self, id: &SessionId) -> Result<(), Self::Error> {
        let result = self.inner.close_session(id).await;
        self.manager
            .close_session(&SessionKey::new(format!("http:{id}")));
        result
    }

    async fn create_stream(
        &self,
        id: &SessionId,
        mut message: ClientJsonRpcMessage,
    ) -> Result<impl Stream<Item = ServerSseMessage> + Send + Sync + 'static, Self::Error> {
        attach_codeweave_session_id(&mut message, id);
        self.inner.create_stream(id, message).await
    }

    async fn accept_message(
        &self,
        id: &SessionId,
        mut message: ClientJsonRpcMessage,
    ) -> Result<(), Self::Error> {
        attach_codeweave_session_id(&mut message, id);
        self.inner.accept_message(id, message).await
    }

    async fn create_standalone_stream(
        &self,
        id: &SessionId,
    ) -> Result<impl Stream<Item = ServerSseMessage> + Send + Sync + 'static, Self::Error> {
        let stream = self.inner.create_standalone_stream(id).await?;
        Ok(stream::iter([compatibility_ready_event()]).chain(stream))
    }

    async fn resume(
        &self,
        id: &SessionId,
        last_event_id: String,
    ) -> Result<impl Stream<Item = ServerSseMessage> + Send + Sync + 'static, Self::Error> {
        self.inner.resume(id, last_event_id).await
    }

    async fn restore_session(
        &self,
        id: SessionId,
    ) -> Result<RestoreOutcome<Self::Transport>, Self::Error> {
        self.inner.restore_session(id).await
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

    let allowed_hosts = configured_allowed_hosts(&state.server);

    let mut config = StreamableHttpServerConfig::default();
    config.stateful_mode = state.server.stateful_mode;
    config.json_response = state.server.json_response;
    config.sse_retry = None;
    config.allowed_hosts = allowed_hosts;
    config.allowed_origins = state.server.allowed_origins.clone();

    let service: StreamableHttpService<CodeWeaveMcp, CompatibleSessionManager> =
        StreamableHttpService::new(
            {
                let state = state.clone();
                move || Ok::<_, std::io::Error>(CodeWeaveMcp::new(state.clone()))
            },
            Arc::new(CompatibleSessionManager::new(state.manager.clone())),
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
    serve_with_idle_timeout(listener, app, state.server.idle_timeout_ms).await
}

fn is_accept_resource_exhaustion(error: &std::io::Error) -> bool {
    // Unix EMFILE/ENFILE and Windows ERROR_TOO_MANY_OPEN_FILES/WSAEMFILE.
    matches!(error.raw_os_error(), Some(4 | 23 | 24 | 10024))
}

/// Serves the app on a manual hyper accept loop so we can bound idle keep-alive
/// connection lifetime — something `axum::serve` does not expose.
///
/// Why this matters: `jsonResponse`/`statefulMode` only control how fast a
/// *request body* returns. They do not close the underlying TCP connection.
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
/// HTTP version: we keep the auto (h1/h2) builder but only tune HTTP/1.1. Every
/// real client here — the OpenAI connector, ngrok, curl — speaks HTTP/1.1 to the
/// origin (TLS/ALPN is terminated at the tunnel, so h2c prior-knowledge does not
/// reach us). `header_read_timeout` is an HTTP/1 setting; HTTP/2 has its own
/// keep-alive knobs and would bypass the idle bound. Rather than add a parallel
/// h2 idle configuration for traffic that never arrives, the idle guarantee is
/// intentionally scoped to HTTP/1.1.
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
    let builder = Arc::new(builder);
    let mut accept_backoff = std::time::Duration::from_millis(10);

    loop {
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
            let _ = builder.serve_connection_with_upgrades(io, service).await;
        });
    }
}

fn configured_allowed_hosts(server: &crate::ServerConfig) -> Vec<String> {
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
    use super::*;

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
    fn compatibility_event_contains_json_rpc_message() {
        assert!(compatibility_ready_event().message.is_some());
    }

    #[test]
    fn compatibility_manager_disables_empty_priming() {
        let manager = CompatibleSessionManager::default();
        assert!(manager.inner.session_config.sse_retry.is_none());
    }

    #[test]
    fn session_key_uses_http_session_header_when_available() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("mcp-session-id", "abc123".parse().unwrap());

        let session = session_key_from_headers(&headers);

        assert_eq!(session.as_str(), "http:abc123");
        assert!(!session.is_stateless());
    }

    #[test]
    fn session_key_prefers_internal_session_marker_over_headers() {
        let request = axum::http::Request::builder()
            .header("mcp-session-id", "stale-header")
            .body(())
            .unwrap();
        let (parts, _) = request.into_parts();
        let mut extensions = Extensions::new();
        extensions.insert(parts);
        extensions.insert(CodeWeaveSessionId("actual-session".to_owned()));

        let session = session_key_from_extensions(&extensions).unwrap();

        assert_eq!(session.as_str(), "http:actual-session");
        assert!(!session.is_stateless());
    }

    #[test]
    fn attach_session_id_tags_tool_request_without_http_headers() {
        let mut message: ClientJsonRpcMessage = serde_json::from_value(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "code_capabilities",
                "arguments": {}
            }
        }))
        .unwrap();
        let session_id: SessionId = "session-42".into();

        attach_codeweave_session_id(&mut message, &session_id);

        let ClientJsonRpcMessage::Request(request) = message else {
            panic!("expected request");
        };
        assert_eq!(
            request
                .request
                .extensions()
                .get::<CodeWeaveSessionId>()
                .unwrap(),
            &CodeWeaveSessionId("session-42".to_owned())
        );
    }

    #[test]
    fn session_key_falls_back_to_stateless_without_header() {
        let headers = axum::http::HeaderMap::new();

        let session = session_key_from_headers(&headers);

        assert_eq!(session.as_str(), "stateless");
        assert!(session.is_stateless());
    }

    #[test]
    fn session_key_falls_back_to_stateless_for_empty_header() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("mcp-session-id", "".parse().unwrap());

        let session = session_key_from_headers(&headers);

        assert_eq!(session.as_str(), "stateless");
        assert!(session.is_stateless());
    }

    #[test]
    fn wildcard_allowed_host_disables_rmcp_host_guard_for_trusted_tunnels() {
        let server = crate::ServerConfig {
            host: "127.0.0.1".to_owned(),
            port: 8813,
            auth_mode: "bearer".to_owned(),
            token_file: ".mcp-token".to_owned(),
            allowed_hosts: vec!["*".to_owned()],
            allowed_origins: Vec::new(),
            stateful_mode: true,
            json_response: false,
            idle_timeout_ms: 5000,
            tool_profile: "full".to_owned(),
            tools: Default::default(),
        };

        assert!(configured_allowed_hosts(&server).is_empty());
    }

    #[test]
    fn configured_allowed_hosts_keeps_loopback_defaults() {
        let server = crate::ServerConfig {
            host: "127.0.0.1".to_owned(),
            port: 8813,
            auth_mode: "bearer".to_owned(),
            token_file: ".mcp-token".to_owned(),
            allowed_hosts: vec!["example.ngrok-free.dev".to_owned()],
            allowed_origins: Vec::new(),
            stateful_mode: true,
            json_response: false,
            idle_timeout_ms: 5000,
            tool_profile: "full".to_owned(),
            tools: Default::default(),
        };

        let hosts = configured_allowed_hosts(&server);

        assert!(hosts.contains(&"127.0.0.1:8813".to_owned()));
        assert!(hosts.contains(&"example.ngrok-free.dev".to_owned()));
    }

    #[test]
    fn configured_allowed_hosts_brackets_ipv6_loopback_authorities() {
        let server = crate::ServerConfig {
            host: "::1".to_owned(),
            port: 8813,
            auth_mode: "bearer".to_owned(),
            token_file: ".mcp-token".to_owned(),
            allowed_hosts: Vec::new(),
            allowed_origins: Vec::new(),
            stateful_mode: true,
            json_response: false,
            idle_timeout_ms: 5000,
            tool_profile: "full".to_owned(),
            tools: Default::default(),
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
