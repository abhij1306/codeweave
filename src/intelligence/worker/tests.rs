use super::*;
use parking_lot::Mutex;
use serde_json::json;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};

struct FakeTransport {
    sent: Arc<Mutex<Vec<Value>>>,
    queued: VecDeque<io::Result<Value>>,
    fail_request_once: Arc<AtomicBool>,
    advertise_pull_diagnostics: bool,
    publish_equivalent_uri: bool,
}

impl RpcTransport for FakeTransport {
    fn send(&mut self, value: &Value) -> io::Result<()> {
        self.sent.lock().push(value.clone());
        if let (Some(id), Some(method)) = (
            value.get("id").and_then(Value::as_u64),
            value.get("method").and_then(Value::as_str),
        ) {
            if method == "initialize" {
                let mut capabilities = json!({
                    "referencesProvider": true,
                    "definitionProvider": true,
                    "renameProvider": true,
                    "textDocumentSync": {"change": 2},
                    "positionEncoding": "utf-8"
                });
                if self.advertise_pull_diagnostics {
                    capabilities["diagnosticProvider"] = json!({});
                }
                self.queued.push_back(Ok(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "capabilities": capabilities,
                        "serverInfo": {"name": "fixture", "version": "1"}
                    }
                })));
            } else if self.fail_request_once.swap(false, Ordering::SeqCst) {
                self.queued.push_back(Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "fixture timeout",
                )));
            } else {
                self.queued.push_back(Ok(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": []
                })));
            }
        } else if matches!(
            value.get("method").and_then(Value::as_str),
            Some("textDocument/didOpen" | "textDocument/didChange")
        ) {
            let mut uri = value["params"]["textDocument"]["uri"]
                .as_str()
                .unwrap()
                .to_owned();
            if self.publish_equivalent_uri {
                uri = uri.replace("%C3%A9", "%c3%a9");
            }
            self.queued.push_back(Ok(json!({
                "jsonrpc": "2.0",
                "method": "textDocument/publishDiagnostics",
                "params": {"uri": uri, "diagnostics": []}
            })));
        }
        Ok(())
    }

    fn receive(&mut self, _timeout: Duration) -> io::Result<Value> {
        self.queued
            .pop_front()
            .unwrap_or_else(|| Err(io::Error::new(io::ErrorKind::TimedOut, "fixture empty")))
    }
}

fn fake_factory(
    sent: Arc<Mutex<Vec<Value>>>,
    fail_request_once: Arc<AtomicBool>,
) -> TransportFactory {
    fake_factory_with_diagnostics(sent, fail_request_once, true, false)
}

fn fake_factory_with_diagnostics(
    sent: Arc<Mutex<Vec<Value>>>,
    fail_request_once: Arc<AtomicBool>,
    advertise_pull_diagnostics: bool,
    publish_equivalent_uri: bool,
) -> TransportFactory {
    Arc::new(move || {
        Ok(Box::new(FakeTransport {
            sent: Arc::clone(&sent),
            queued: VecDeque::new(),
            fail_request_once: Arc::clone(&fail_request_once),
            advertise_pull_diagnostics,
            publish_equivalent_uri,
        }))
    })
}

#[test]
fn worker_sends_full_text_change_after_hash_changes() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("main.rs");
    std::fs::write(&path, "fn café() {}\n").unwrap();
    let sent = Arc::new(Mutex::new(Vec::new()));
    let worker = LspWorker::test_with_factory(
        root.path().to_path_buf(),
        LspPreset::Rust,
        Duration::from_millis(100),
        fake_factory(Arc::clone(&sent), Arc::new(AtomicBool::new(false))),
    );
    let first = DocumentSnapshot::read(&path, "rust").unwrap();
    worker
        .execute(
            WorkerOperation::Definition { line: 1, column: 3 },
            first.clone(),
        )
        .unwrap();
    std::fs::write(&path, "fn café() { println!(\"new\"); }\n").unwrap();
    let second = DocumentSnapshot::read(&path, "rust").unwrap();
    let response = worker
        .execute(
            WorkerOperation::Definition { line: 1, column: 3 },
            second.clone(),
        )
        .unwrap();
    assert_ne!(first.hash, response.synchronized.hash);
    let status = worker.status();
    assert_eq!(status["capabilities"]["position_encoding"], "utf-8");
    assert_eq!(status["capabilities"]["server_name"], "fixture");
    assert!(status["capabilities"]["initialization_ms"].is_number());
    assert_eq!(status["synchronized_document_count"], 1);
    assert_eq!(status["latency_ms"]["request_count"], 2);
    assert!(status["latency_ms"]["first_request"].is_number());
    assert!(status["latency_ms"]["last_request"].is_number());
    assert!(status["latency_ms"]["warm_request_p50"].is_number());
    let messages = sent.lock();
    let change = messages
        .iter()
        .find(|message| message["method"] == "textDocument/didChange")
        .unwrap();
    assert_eq!(change["params"]["textDocument"]["version"], 2);
    assert_eq!(
        change["params"]["contentChanges"][0]["text"],
        second.content
    );
}

#[test]
fn python_diagnostics_use_pull_request_then_cached_result() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("sample.py");
    std::fs::write(&path, "value: int = 'wrong'\n").unwrap();
    let sent = Arc::new(Mutex::new(Vec::new()));
    let worker = LspWorker::test_with_factory(
        root.path().to_path_buf(),
        LspPreset::Python,
        Duration::from_millis(100),
        fake_factory(Arc::clone(&sent), Arc::new(AtomicBool::new(false))),
    );
    let document = DocumentSnapshot::read(&path, "python").unwrap();

    let cold = worker
        .execute(WorkerOperation::Diagnostics, document.clone())
        .unwrap();
    let warm = worker
        .execute(WorkerOperation::Diagnostics, document)
        .unwrap();

    assert_eq!(cold.result, json!([]));
    assert_eq!(warm.result, json!([]));
    let diagnostic_requests = sent
        .lock()
        .iter()
        .filter(|message| message["method"] == "textDocument/diagnostic")
        .count();
    assert_eq!(diagnostic_requests, 1);
}

#[test]
fn published_diagnostic_uri_matches_equivalent_file_uri_encoding() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("café.py");
    std::fs::write(&path, "value = 1\n").unwrap();
    let requested = super::super::normalize::path_uri(&path);
    let published = requested.replace("%C3%A9", "%c3%a9");
    assert_ne!(requested, published);
    assert!(diagnostic_uri_matches(root.path(), &requested, &published));
}

#[test]
fn publish_diagnostics_are_available_cold_and_cached_warm() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("café.py");
    std::fs::write(&path, "value: int = 'wrong'\n").unwrap();
    let sent = Arc::new(Mutex::new(Vec::new()));
    let worker = LspWorker::test_with_factory(
        root.path().to_path_buf(),
        LspPreset::Python,
        Duration::from_millis(100),
        fake_factory_with_diagnostics(
            Arc::clone(&sent),
            Arc::new(AtomicBool::new(false)),
            false,
            true,
        ),
    );
    let document = DocumentSnapshot::read(&path, "python").unwrap();

    let cold = worker
        .execute(WorkerOperation::Diagnostics, document.clone())
        .unwrap();
    let warm = worker
        .execute(WorkerOperation::Diagnostics, document)
        .unwrap();

    assert_eq!(cold.result, json!([]));
    assert_eq!(warm.result, json!([]));
    assert_eq!(worker.status()["restart_count"], 0);
    assert!(!sent
        .lock()
        .iter()
        .any(|message| message["method"] == "textDocument/diagnostic"));
}

#[test]
fn worker_restarts_and_reopens_after_timeout() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("main.rs");
    std::fs::write(&path, "fn target() {}\n").unwrap();
    let sent = Arc::new(Mutex::new(Vec::new()));
    let worker = LspWorker::test_with_factory(
        root.path().to_path_buf(),
        LspPreset::Rust,
        Duration::from_millis(100),
        fake_factory(Arc::clone(&sent), Arc::new(AtomicBool::new(true))),
    );
    worker
        .execute(
            WorkerOperation::References { line: 1, column: 3 },
            DocumentSnapshot::read(&path, "rust").unwrap(),
        )
        .unwrap();
    let messages = sent.lock();
    assert_eq!(
        messages
            .iter()
            .filter(|message| message["method"] == "initialize")
            .count(),
        2
    );
    assert_eq!(
        messages
            .iter()
            .filter(|message| message["method"] == "textDocument/didOpen")
            .count(),
        2
    );
    assert_eq!(worker.status()["restart_count"], 1);
}

#[test]
fn rust_python_and_typescript_presets_are_explicit() {
    assert_eq!(LspPreset::Rust.default_command(), "rust-analyzer");
    assert_eq!(
        LspPreset::Python.default_command(),
        "basedpyright-langserver"
    );
    assert_eq!(
        LspPreset::TypeScript.default_command(),
        "typescript-language-server"
    );
    assert!(LspPreset::Rust.default_args().is_empty());
    assert_eq!(LspPreset::Python.default_args(), &["--stdio"]);
}
