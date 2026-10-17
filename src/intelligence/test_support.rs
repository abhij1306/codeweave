//! Scripted language servers and workspace fixtures for intelligence tests.
use super::backend::{Backends, SemanticBackend};
use super::live_index::LiveIndex;
use super::preset::LspPreset;
use super::service::IntelligenceService;
use super::sync::DocumentSnapshot;
use super::transport::{RpcTransport, TransportError, TransportFactory};
use super::uri::{path_uri, UriCodec};
use super::worker::LspWorker;
use crate::index::{CodeIndex, WorkspaceExclusions};
use crate::model::IntelligenceSettings;
use parking_lot::{Mutex, RwLock};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// How the scripted server answers one request.
pub(crate) enum Reply {
    Result(Value),
    Error(Value),
    /// Never answer.
    Silence,
    /// Close the connection, as a crashed server would.
    Exit,
}

type Responder = dyn Fn(&str, &Value) -> Reply + Send + Sync;

/// A scripted language server. Every transport it creates shares the
/// responder and the record of messages sent to it.
#[derive(Clone)]
pub(crate) struct FakeServer {
    sent: Arc<Mutex<Vec<Value>>>,
    capabilities: Value,
    responder: Arc<Responder>,
    publish: Option<Arc<dyn Fn(&str) -> Value + Send + Sync>>,
    spawned: Arc<AtomicUsize>,
    alive: Arc<AtomicUsize>,
}

impl FakeServer {
    /// A UTF-16, full-sync server advertising every operation (pull diagnostics).
    pub(crate) fn new(responder: impl Fn(&str, &Value) -> Reply + Send + Sync + 'static) -> Self {
        Self {
            sent: Arc::new(Mutex::new(Vec::new())),
            capabilities: json!({
                "referencesProvider": true,
                "definitionProvider": true,
                "renameProvider": true,
                "diagnosticProvider": {},
                "textDocumentSync": 1,
                "positionEncoding": "utf-16"
            }),
            responder: Arc::new(responder),
            publish: None,
            spawned: Arc::new(AtomicUsize::new(0)),
            alive: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub(crate) fn with_capability(mut self, key: &str, value: Value) -> Self {
        if value.is_null() {
            self.capabilities.as_object_mut().unwrap().remove(key);
        } else {
            self.capabilities[key] = value;
        }
        self
    }

    /// After every didOpen/didChange of `uri`, publish diagnostics with the
    /// `params` returned by `publish(uri)`.
    pub(crate) fn publishing(
        mut self,
        publish: impl Fn(&str) -> Value + Send + Sync + 'static,
    ) -> Self {
        self.publish = Some(Arc::new(publish));
        self
    }

    pub(crate) fn factory(&self) -> TransportFactory {
        let server = self.clone();
        Arc::new(move || {
            server.spawned.fetch_add(1, Ordering::SeqCst);
            server.alive.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(FakeTransport {
                server: server.clone(),
                queued: VecDeque::new(),
                exited: false,
            }) as Box<dyn RpcTransport>)
        })
    }

    pub(crate) fn worker(&self, root: &Path, preset: LspPreset, timeout: Duration) -> LspWorker {
        LspWorker::with_transport(root, preset, timeout, self.factory())
    }

    /// Messages sent to the server with `method`, in order.
    pub(crate) fn messages(&self, method: &str) -> Vec<Value> {
        self.sent
            .lock()
            .iter()
            .filter(|message| message["method"] == method)
            .cloned()
            .collect()
    }

    /// Server processes started so far.
    pub(crate) fn spawn_count(&self) -> usize {
        self.spawned.load(Ordering::SeqCst)
    }

    /// Server processes started and not yet shut down.
    pub(crate) fn running_count(&self) -> usize {
        self.alive.load(Ordering::SeqCst)
    }
}

impl Drop for FakeTransport {
    fn drop(&mut self) {
        self.server.alive.fetch_sub(1, Ordering::SeqCst);
    }
}

struct FakeTransport {
    server: FakeServer,
    queued: VecDeque<Value>,
    exited: bool,
}

impl RpcTransport for FakeTransport {
    fn send(&mut self, value: &Value) -> Result<(), TransportError> {
        if self.exited {
            return Err(TransportError::Disconnected);
        }
        self.server.sent.lock().push(value.clone());
        let method = value.get("method").and_then(Value::as_str).unwrap_or("");
        let Some(id) = value.get("id").filter(|_| !method.is_empty()) else {
            if matches!(method, "textDocument/didOpen" | "textDocument/didChange") {
                if let Some(publish) = &self.server.publish {
                    let uri = value["params"]["textDocument"]["uri"].as_str().unwrap();
                    self.queued.push_back(json!({
                        "jsonrpc": "2.0",
                        "method": "textDocument/publishDiagnostics",
                        "params": publish(uri)
                    }));
                }
            }
            return Ok(());
        };
        let reply = if method == "initialize" {
            Reply::Result(json!({
                "capabilities": self.server.capabilities,
                "serverInfo": {"name": "fixture", "version": "1"}
            }))
        } else {
            (self.server.responder)(method, &value["params"])
        };
        match reply {
            Reply::Result(result) => self
                .queued
                .push_back(json!({"jsonrpc": "2.0", "id": id, "result": result})),
            Reply::Error(error) => self
                .queued
                .push_back(json!({"jsonrpc": "2.0", "id": id, "error": error})),
            Reply::Silence => {}
            Reply::Exit => self.exited = true,
        }
        Ok(())
    }

    fn receive(&mut self, timeout: Duration) -> Result<Value, TransportError> {
        if let Some(message) = self.queued.pop_front() {
            return Ok(message);
        }
        if self.exited {
            return Err(TransportError::Disconnected);
        }
        std::thread::sleep(timeout);
        Err(TransportError::TimedOut)
    }

    fn stderr_tail(&self) -> Option<String> {
        self.exited.then(|| "fixture crashed".to_owned())
    }
}

/// A temporary workspace with a scanned live index.
pub(crate) struct Fixture {
    pub(crate) root: tempfile::TempDir,
    index: Arc<RwLock<CodeIndex>>,
    snapshot: Arc<RwLock<String>>,
}

impl Fixture {
    pub(crate) fn new(files: &[(&str, &str)]) -> Self {
        let root = tempfile::tempdir().unwrap();
        for (relative, content) in files {
            let path = root.path().join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }
        let exclusions = WorkspaceExclusions::new(root.path(), &[]).unwrap();
        let index = CodeIndex::scan(root.path(), 1_000_000, &[], &exclusions).unwrap();
        Self {
            root,
            index: Arc::new(RwLock::new(index)),
            snapshot: Arc::new(RwLock::new("snap_fixture".to_owned())),
        }
    }

    pub(crate) fn path(&self, relative: &str) -> std::path::PathBuf {
        self.root.path().join(relative)
    }

    pub(crate) fn uri(&self, relative: &str) -> String {
        path_uri(&self.path(relative))
    }

    pub(crate) fn write(&self, relative: &str, content: &str) {
        fs::write(self.path(relative), content).unwrap();
    }

    pub(crate) fn index(&self) -> &Arc<RwLock<CodeIndex>> {
        &self.index
    }

    pub(crate) fn live(&self) -> LiveIndex {
        LiveIndex::new(Arc::clone(&self.index), Arc::clone(&self.snapshot))
    }

    /// A service whose only backend is `server` serving `preset`.
    pub(crate) fn service_with(
        &self,
        server: &FakeServer,
        preset: LspPreset,
    ) -> IntelligenceService {
        let worker = server.worker(self.root.path(), preset, Duration::from_millis(200));
        self.service_with_backends(vec![Arc::new(worker)])
    }

    pub(crate) fn service_with_backends(
        &self,
        backends: Vec<Arc<dyn SemanticBackend>>,
    ) -> IntelligenceService {
        IntelligenceService::with_backends(
            self.root.path().to_path_buf(),
            "fixture".to_owned(),
            self.live(),
            Backends::new(backends),
        )
    }

    /// A service with every language server disabled.
    pub(crate) fn service(&self) -> IntelligenceService {
        IntelligenceService::new(
            self.root.path().to_path_buf(),
            IntelligenceSettings::default(),
            "fixture".to_owned(),
            Arc::clone(&self.index),
            Arc::clone(&self.snapshot),
        )
        .unwrap()
    }

    /// The document the service would synchronize for `relative`.
    pub(crate) fn document(&self, relative: &str, language_id: &'static str) -> DocumentSnapshot {
        DocumentSnapshot::new(
            UriCodec::new(self.root.path()).document_uri(relative),
            language_id,
            fs::read_to_string(self.path(relative)).unwrap(),
        )
    }
}
