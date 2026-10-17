//! End to end over the MCP protocol: a client speaks JSON-RPC lines to the
//! server over an in-process stdio pipe.

use super::super::config::{AuthMode, ServerConfig};
use super::super::mcp_transport::CodeWeaveMcp;
use super::super::serve::AppState;
use codeweave_rust::manager::Application;
use codeweave_rust::model::{
    BashConfig, DaemonConfig, IntelligenceSettings, PolicyConfig, WorkspaceSettings,
};
use codeweave_rust::test_bash_executable;
use rmcp::ServiceExt;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Arc;
use tokio::io::{
    AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines, ReadHalf, WriteHalf,
};

struct Client {
    lines: Lines<BufReader<ReadHalf<DuplexStream>>>,
    writer: WriteHalf<DuplexStream>,
}

impl Client {
    async fn send(&mut self, message: Value) {
        let mut line = serde_json::to_vec(&message).unwrap();
        line.push(b'\n');
        self.writer.write_all(&line).await.unwrap();
        self.writer.flush().await.unwrap();
    }

    async fn request(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        let line = self
            .lines
            .next_line()
            .await
            .unwrap()
            .expect("server response");
        let response: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(response["id"], id, "{response}");
        response
    }
}

fn daemon_config(root: &Path, cache: &Path) -> DaemonConfig {
    DaemonConfig {
        config_version: 2,
        workspace: WorkspaceSettings {
            path: root.to_string_lossy().into_owned(),
            artifact_paths: Vec::new(),
            exclude_paths: Vec::new(),
        },
        intelligence: IntelligenceSettings::default(),
        policy: PolicyConfig {
            max_file_bytes: 1_000_000,
            max_context_chars: 50_000,
            max_search_results: 100,
            bash: BashConfig {
                executable: test_bash_executable(),
                default_timeout_ms: 120_000,
                foreground_budget_ms: 20_000,
                max_timeout_ms: 300_000,
                max_output_chars: 30_000,
            },
        },
        cache_root: cache.to_string_lossy().into_owned(),
    }
}

#[tokio::test]
async fn a_client_initializes_lists_tools_and_calls_one_over_stdio() {
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    assert!(std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(root.path())
        .status()
        .unwrap()
        .success());
    let manager = Arc::new(Application::default());
    manager
        .initialize(daemon_config(root.path(), cache.path()))
        .await
        .unwrap();
    let state = AppState {
        instance_id: manager.instance_id(),
        manager,
        server: ServerConfig {
            host: "127.0.0.1".to_owned(),
            port: 8813,
            auth_mode: AuthMode::None,
            token_file: ".mcp-token".to_owned(),
            allowed_hosts: Vec::new(),
            allowed_origins: Vec::new(),
            idle_timeout_ms: 5000,
        },
        token: None,
    };
    let (client_stream, server_stream) = tokio::io::duplex(1 << 20);
    let server = tokio::spawn(async move {
        let running = CodeWeaveMcp::new(state)
            .serve(tokio::io::split(server_stream))
            .await
            .unwrap();
        running.waiting().await.unwrap();
    });
    let (reader, writer) = tokio::io::split(client_stream);
    let mut client = Client {
        lines: BufReader::new(reader).lines(),
        writer,
    };

    let initialized = client
        .request(
            1,
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "e2e", "version": "1"}
            }),
        )
        .await;
    client
        .send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
        .await;
    let listed = client.request(2, "tools/list", json!({})).await;
    let written = client
        .request(
            3,
            "tools/call",
            json!({"name": "code_write", "arguments": {"path": "hello.txt", "content": "hi\n"}}),
        )
        .await;
    let rejected = client
        .request(
            4,
            "tools/call",
            json!({"name": "git_log", "arguments": {"author": "someone"}}),
        )
        .await;
    let unknown = client
        .request(5, "tools/call", json!({"name": "git", "arguments": {}}))
        .await;
    drop(client);
    server.await.unwrap();

    assert_eq!(initialized["result"]["serverInfo"]["name"], "codeweave");
    let fixture: Value =
        serde_json::from_str(include_str!("../../../tests/fixtures/tools_list.json")).unwrap();
    assert_eq!(listed["result"], json!({"tools": fixture}));
    assert_eq!(written["result"]["structuredContent"]["applied"], true);
    assert_eq!(written["result"].get("isError"), None);
    assert_eq!(
        std::fs::read_to_string(root.path().join("hello.txt")).unwrap(),
        "hi\n"
    );
    assert_eq!(rejected["result"]["isError"], true);
    assert_eq!(
        rejected["result"]["structuredContent"]["error"]["code"],
        "UNKNOWN_TOOL_FIELD"
    );
    assert_eq!(unknown["error"]["code"], -32602);
    assert_eq!(unknown["error"]["message"], "Unknown tool: git");
}
