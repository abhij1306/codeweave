use super::normalize::position_params;
use super::protocol::{
    initialize_params, parse_initialize_result, server_request_result, ServerCapabilities,
};
use super::sync::{
    plan_sync, verify_current, DocumentSnapshot, DocumentState, SynchronizedDocument,
};
use crate::model::{AppError, AppResult, LanguageServerSettings};
use crate::process_runtime::{terminate_process_tree, WindowsJob};
use parking_lot::RwLock;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::io;
use std::io::{BufRead as _, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

pub(crate) trait RpcTransport: Send {
    fn send(&mut self, value: &Value) -> io::Result<()>;
    fn receive(&mut self, timeout: Duration) -> io::Result<Value>;
}

struct StdioRpcTransport {
    child: Child,
    stdin: ChildStdin,
    messages: Receiver<Value>,
    pid: u32,
    #[cfg(windows)]
    job: Option<Arc<WindowsJob>>,
}

impl StdioRpcTransport {
    fn spawn(command: &str, args: &[String], cwd: &Path) -> io::Result<Self> {
        let mut child = Command::new(command)
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let pid = child.id();
        #[cfg(windows)]
        let job = WindowsJob::assign(pid).ok();
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("JSON-RPC stdin unavailable"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("JSON-RPC stdout unavailable"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("JSON-RPC stderr unavailable"))?;
        let (tx, messages) = mpsc::channel();
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut length = None;
                loop {
                    let mut line = String::new();
                    if reader
                        .read_line(&mut line)
                        .ok()
                        .filter(|count| *count > 0)
                        .is_none()
                    {
                        return;
                    }
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        break;
                    }
                    if let Some(value) = trimmed.strip_prefix("Content-Length:") {
                        length = value.trim().parse::<usize>().ok();
                    }
                }
                let Some(length) = length else {
                    continue;
                };
                let mut body = vec![0; length];
                if std::io::Read::read_exact(&mut reader, &mut body).is_err() {
                    return;
                }
                if let Ok(value) = serde_json::from_slice(&body) {
                    if tx.send(value).is_err() {
                        return;
                    }
                }
            }
        });
        thread::spawn(move || {
            let _ = std::io::copy(&mut BufReader::new(stderr), &mut std::io::sink());
        });
        Ok(Self {
            child,
            stdin,
            messages,
            pid,
            #[cfg(windows)]
            job,
        })
    }
}

impl RpcTransport for StdioRpcTransport {
    fn send(&mut self, value: &Value) -> io::Result<()> {
        let body = serde_json::to_vec(value).map_err(io::Error::other)?;
        write!(self.stdin, "Content-Length: {}\r\n\r\n", body.len())?;
        self.stdin.write_all(&body)?;
        self.stdin.flush()
    }

    fn receive(&mut self, timeout: Duration) -> io::Result<Value> {
        self.messages
            .recv_timeout(timeout)
            .map_err(|error| io::Error::new(io::ErrorKind::TimedOut, error))
    }
}

impl Drop for StdioRpcTransport {
    fn drop(&mut self) {
        terminate_process_tree(self.pid, {
            #[cfg(windows)]
            {
                self.job.as_deref()
            }
            #[cfg(not(windows))]
            {
                None
            }
        });
        let _ = self.child.wait();
    }
}

type TransportFactory = Arc<dyn Fn() -> io::Result<Box<dyn RpcTransport>> + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LspPreset {
    Rust,
    Python,
    TypeScript,
}

impl LspPreset {
    pub(crate) fn key(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Python => "python",
            Self::TypeScript => "typescript",
        }
    }

    pub(crate) fn language_id(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Python => "python",
            Self::TypeScript => "typescript",
        }
    }

    pub(crate) fn default_command(self) -> &'static str {
        match self {
            Self::Rust => "rust-analyzer",
            Self::Python => "basedpyright-langserver",
            Self::TypeScript => "typescript-language-server",
        }
    }

    fn default_args(self) -> &'static [&'static str] {
        match self {
            Self::Rust => &[],
            Self::Python | Self::TypeScript => &["--stdio"],
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum WorkerOperation {
    Definition {
        line: usize,
        column: usize,
    },
    References {
        line: usize,
        column: usize,
    },
    Rename {
        line: usize,
        column: usize,
        new_name: String,
    },
    Diagnostics,
}

impl WorkerOperation {
    fn capability_name(&self) -> &'static str {
        match self {
            Self::Definition { .. } => "definition",
            Self::References { .. } => "references",
            Self::Rename { .. } => "rename",
            Self::Diagnostics => "diagnostics",
        }
    }

    fn method(&self) -> Option<&'static str> {
        match self {
            Self::Definition { .. } => Some("textDocument/definition"),
            Self::References { .. } => Some("textDocument/references"),
            Self::Rename { .. } => Some("textDocument/rename"),
            Self::Diagnostics => None,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct WorkerResponse {
    pub(crate) result: Value,
    pub(crate) synchronized: SynchronizedDocument,
    pub(crate) capabilities: ServerCapabilities,
}

#[derive(Debug, Clone)]
struct WorkerStatus {
    readiness: &'static str,
    restart_count: u64,
    last_error: Option<String>,
    capabilities: Option<ServerCapabilities>,
    synchronized_document_count: usize,
    request_count: u64,
    first_request_ms: Option<u128>,
    last_request_ms: Option<u128>,
    warm_request_samples_ms: VecDeque<u128>,
}

impl Default for WorkerStatus {
    fn default() -> Self {
        Self {
            readiness: "lazy",
            restart_count: 0,
            last_error: None,
            capabilities: None,
            synchronized_document_count: 0,
            request_count: 0,
            first_request_ms: None,
            last_request_ms: None,
            warm_request_samples_ms: VecDeque::new(),
        }
    }
}

enum WorkerCommand {
    Execute {
        operation: WorkerOperation,
        document: DocumentSnapshot,
        response: Sender<AppResult<WorkerResponse>>,
    },
}

pub(crate) struct LspWorker {
    preset: LspPreset,
    configured: bool,
    command: String,
    args: Vec<String>,
    timeout: Duration,
    sender: Option<Sender<WorkerCommand>>,
    status: Arc<RwLock<WorkerStatus>>,
}

impl LspWorker {
    pub(crate) fn new(root: PathBuf, preset: LspPreset, settings: LanguageServerSettings) -> Self {
        let command = if settings.command.is_empty() {
            preset.default_command().to_owned()
        } else {
            settings.command.clone()
        };
        let args = if settings.args.is_empty() {
            preset
                .default_args()
                .iter()
                .map(|value| (*value).to_owned())
                .collect()
        } else {
            settings.args.clone()
        };
        let command_for_factory = command.clone();
        let args_for_factory = args.clone();
        let root_for_factory = root.clone();
        let factory: TransportFactory = Arc::new(move || {
            StdioRpcTransport::spawn(&command_for_factory, &args_for_factory, &root_for_factory)
                .map(|process| Box::new(process) as Box<dyn RpcTransport>)
        });
        Self::with_factory(root, preset, settings, command, args, factory)
    }

    fn with_factory(
        root: PathBuf,
        preset: LspPreset,
        settings: LanguageServerSettings,
        command: String,
        args: Vec<String>,
        factory: TransportFactory,
    ) -> Self {
        let configured = settings.enabled;
        let timeout = Duration::from_millis(settings.timeout_ms.max(1));
        let status = Arc::new(RwLock::new(WorkerStatus {
            readiness: if configured { "lazy" } else { "disabled" },
            ..WorkerStatus::default()
        }));
        let sender = configured.then(|| {
            let (sender, receiver) = mpsc::channel();
            let worker_status = Arc::clone(&status);
            let worker_name = format!("codeweave-lsp-{}", preset.key());
            thread::Builder::new()
                .name(worker_name)
                .spawn(move || {
                    WorkerRuntime::new(root, preset, timeout, factory, worker_status).run(receiver);
                })
                .expect("LSP worker thread must start");
            sender
        });
        Self {
            preset,
            configured,
            command,
            args,
            timeout,
            sender,
            status,
        }
    }

    #[cfg(test)]
    fn test_with_factory(
        root: PathBuf,
        preset: LspPreset,
        timeout: Duration,
        factory: TransportFactory,
    ) -> Self {
        Self::with_factory(
            root,
            preset,
            LanguageServerSettings {
                enabled: true,
                command: "fixture".to_owned(),
                args: Vec::new(),
                timeout_ms: timeout.as_millis() as u64,
            },
            "fixture".to_owned(),
            Vec::new(),
            factory,
        )
    }

    pub(crate) fn configured(&self) -> bool {
        self.configured
    }

    pub(crate) fn preset(&self) -> LspPreset {
        self.preset
    }

    pub(crate) fn execute(
        &self,
        operation: WorkerOperation,
        document: DocumentSnapshot,
    ) -> AppResult<WorkerResponse> {
        let sender = self.sender.as_ref().ok_or_else(|| {
            AppError::new(
                "SEMANTIC_BACKEND_UNAVAILABLE",
                format!("{} language server is disabled", self.preset.key()),
            )
        })?;
        let (response_tx, response_rx) = mpsc::channel();
        sender
            .send(WorkerCommand::Execute {
                operation,
                document,
                response: response_tx,
            })
            .map_err(|_| {
                AppError::new("LSP_WORKER_STOPPED", "Language-server worker has stopped")
            })?;
        // A restart cycle can spend one timeout initializing and one timeout on the
        // request for each of two attempts. Keep the existing fixed scheduling buffer.
        let wait = self.timeout.saturating_mul(4) + Duration::from_secs(2);
        response_rx.recv_timeout(wait).map_err(|error| {
            AppError::details(
                "LSP_TIMEOUT",
                "Language-server worker did not answer before its deadline",
                json!({"language": self.preset.key(), "error": error.to_string()}),
            )
        })?
    }

    pub(crate) fn status(&self) -> Value {
        let status = self.status.read().clone();
        let initialization_ms = status
            .capabilities
            .as_ref()
            .map(|capabilities| capabilities.initialization_ms);
        let warm_request_p50_ms = latency_p50(&status.warm_request_samples_ms);
        json!({
            "language": self.preset.key(),
            "configured": self.configured,
            "command": self.command,
            "args": self.args,
            "timeout_ms": self.timeout.as_millis(),
            "readiness": status.readiness,
            "restart_count": status.restart_count,
            "last_error": status.last_error,
            "synchronized_document_count": status.synchronized_document_count,
            "capabilities": status.capabilities.map(|value| value.to_json()),
            "latency_ms": {
                "initialization": initialization_ms,
                "first_request": status.first_request_ms,
                "last_request": status.last_request_ms,
                "warm_request_p50": warm_request_p50_ms,
                "request_count": status.request_count
            },
            "fallback": "tree_sitter_then_lexical"
        })
    }
}

struct WorkerRuntime {
    root: PathBuf,
    preset: LspPreset,
    timeout: Duration,
    factory: TransportFactory,
    status: Arc<RwLock<WorkerStatus>>,
    process: Option<Box<dyn RpcTransport>>,
    next_id: u64,
    capabilities: Option<ServerCapabilities>,
    documents: HashMap<PathBuf, DocumentState>,
    diagnostics: HashMap<String, Value>,
}

impl WorkerRuntime {
    fn new(
        root: PathBuf,
        preset: LspPreset,
        timeout: Duration,
        factory: TransportFactory,
        status: Arc<RwLock<WorkerStatus>>,
    ) -> Self {
        Self {
            root,
            preset,
            timeout,
            factory,
            status,
            process: None,
            next_id: 1,
            capabilities: None,
            documents: HashMap::new(),
            diagnostics: HashMap::new(),
        }
    }

    fn run(mut self, receiver: Receiver<WorkerCommand>) {
        while let Ok(command) = receiver.recv() {
            match command {
                WorkerCommand::Execute {
                    operation,
                    document,
                    response,
                } => {
                    let started = Instant::now();
                    let result = self.execute_with_restart(operation, document);
                    let elapsed_ms = started.elapsed().as_millis();
                    {
                        let mut status = self.status.write();
                        status.request_count += 1;
                        status.last_request_ms = Some(elapsed_ms);
                        if status.first_request_ms.is_none() {
                            status.first_request_ms = Some(elapsed_ms);
                        } else {
                            status.warm_request_samples_ms.push_back(elapsed_ms);
                            if status.warm_request_samples_ms.len() > 64 {
                                status.warm_request_samples_ms.pop_front();
                            }
                        }
                    }
                    let _ = response.send(result);
                }
            }
        }
    }

    fn execute_with_restart(
        &mut self,
        operation: WorkerOperation,
        document: DocumentSnapshot,
    ) -> AppResult<WorkerResponse> {
        for attempt in 0..2 {
            match self.execute_once(&operation, &document) {
                Ok(response) => {
                    let mut status = self.status.write();
                    status.readiness = "ready";
                    status.last_error = None;
                    status.synchronized_document_count = self.documents.len();
                    return Ok(response);
                }
                Err(error) => {
                    let restartable = matches!(
                        error.0.code.as_str(),
                        "LSP_TIMEOUT" | "LSP_TRANSPORT_ERROR" | "LSP_PROTOCOL_ERROR"
                    );
                    {
                        let mut status = self.status.write();
                        status.last_error = Some(error.0.message.clone());
                    }
                    if attempt == 0 && restartable {
                        self.reset(true);
                        continue;
                    }
                    if restartable {
                        self.reset(false);
                    }
                    return Err(error);
                }
            }
        }
        unreachable!()
    }

    fn execute_once(
        &mut self,
        operation: &WorkerOperation,
        document: &DocumentSnapshot,
    ) -> AppResult<WorkerResponse> {
        self.ensure_started()?;
        let capabilities = self.capabilities.clone().expect("started capabilities");
        capabilities.require(operation.capability_name())?;
        let sync = plan_sync(&mut self.documents, document, capabilities.sync_kind)?;
        if let Some(notification) = sync.notification {
            self.send(&notification)?;
            if sync.changed {
                self.diagnostics.remove(&document.uri);
            }
        }

        let result = match operation {
            WorkerOperation::Diagnostics => {
                if let Some(cached) = self.diagnostics.get(&document.uri) {
                    cached.clone()
                } else {
                    self.wait_for_diagnostics(&document.uri)?
                }
            }
            _ => {
                let mut params = match operation {
                    WorkerOperation::Definition { line, column }
                    | WorkerOperation::References { line, column }
                    | WorkerOperation::Rename { line, column, .. } => position_params(
                        &document.path,
                        &document.content,
                        *line,
                        *column,
                        capabilities.position_encoding,
                    )?,
                    WorkerOperation::Diagnostics => unreachable!(),
                };
                match operation {
                    WorkerOperation::References { .. } => {
                        params["context"] = json!({"includeDeclaration": false});
                    }
                    WorkerOperation::Rename { new_name, .. } => {
                        params["newName"] = json!(new_name);
                    }
                    WorkerOperation::Definition { .. } => {}
                    WorkerOperation::Diagnostics => unreachable!(),
                }
                self.request(operation.method().expect("request method"), params)?
            }
        };
        verify_current(document)?;
        Ok(WorkerResponse {
            result,
            synchronized: sync.synchronized,
            capabilities,
        })
    }

    fn ensure_started(&mut self) -> AppResult<()> {
        if self.process.is_some() {
            return Ok(());
        }
        {
            let mut status = self.status.write();
            status.readiness = "starting";
        }
        let mut process = (self.factory)().map_err(|error| {
            AppError::details(
                "LSP_START_FAILED",
                error.to_string(),
                json!({"language": self.preset.key()}),
            )
        })?;
        let id = self.next_request_id();
        let started = Instant::now();
        process
            .send(&json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "initialize",
                "params": initialize_params(&super::normalize::path_uri(&self.root))
            }))
            .map_err(transport_error)?;
        let result = self.wait_for_response_on(&mut *process, id)?;
        let capabilities = parse_initialize_result(&result, started.elapsed().as_millis())?;
        process
            .send(&json!({"jsonrpc":"2.0","method":"initialized","params":{}}))
            .map_err(transport_error)?;
        self.process = Some(process);
        self.capabilities = Some(capabilities.clone());
        let mut status = self.status.write();
        status.readiness = "ready";
        status.capabilities = Some(capabilities);
        status.last_error = None;
        Ok(())
    }

    fn reset(&mut self, count_restart: bool) {
        self.process = None;
        self.capabilities = None;
        self.documents.clear();
        self.diagnostics.clear();
        let mut status = self.status.write();
        if count_restart {
            status.restart_count += 1;
        }
        status.readiness = "lazy";
        status.capabilities = None;
        status.synchronized_document_count = 0;
    }

    fn next_request_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn send(&mut self, value: &Value) -> AppResult<()> {
        self.process
            .as_mut()
            .expect("started")
            .send(value)
            .map_err(transport_error)
    }

    fn request(&mut self, method: &str, params: Value) -> AppResult<Value> {
        let id = self.next_request_id();
        self.send(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))?;
        let mut process = self.process.take().expect("started");
        let result = self.wait_for_response_on(&mut *process, id);
        self.process = Some(process);
        result
    }

    fn wait_for_response_on(
        &mut self,
        process: &mut dyn RpcTransport,
        id: u64,
    ) -> AppResult<Value> {
        let deadline = Instant::now() + self.timeout;
        loop {
            let message = receive_before(process, deadline)?;
            if message.get("id").and_then(Value::as_u64) == Some(id)
                && message.get("method").is_none()
            {
                if let Some(error) = message.get("error") {
                    return Err(AppError::details(
                        "LSP_ERROR",
                        format!("{} language-server request failed", self.preset.key()),
                        error.clone(),
                    ));
                }
                return message.get("result").cloned().ok_or_else(|| {
                    AppError::new(
                        "LSP_PROTOCOL_ERROR",
                        "JSON-RPC response contains neither result nor error",
                    )
                });
            }
            self.handle_message(process, message)?;
        }
    }

    fn wait_for_diagnostics(&mut self, uri: &str) -> AppResult<Value> {
        let deadline = Instant::now() + self.timeout;
        let mut process = self.process.take().expect("started");
        let result = loop {
            let message = receive_before(&mut *process, deadline)?;
            if message.get("method").and_then(Value::as_str)
                == Some("textDocument/publishDiagnostics")
                && message["params"]["uri"].as_str() == Some(uri)
            {
                let diagnostics = message["params"]["diagnostics"].clone();
                self.diagnostics.insert(uri.to_owned(), diagnostics.clone());
                break Ok(diagnostics);
            }
            self.handle_message(&mut *process, message)?;
        };
        self.process = Some(process);
        result
    }

    fn handle_message(&mut self, process: &mut dyn RpcTransport, message: Value) -> AppResult<()> {
        if message.get("method").and_then(Value::as_str) == Some("textDocument/publishDiagnostics")
        {
            if let Some(uri) = message["params"]["uri"].as_str() {
                self.diagnostics
                    .insert(uri.to_owned(), message["params"]["diagnostics"].clone());
            }
            return Ok(());
        }
        if let (Some(id), Some(method)) = (
            message.get("id").cloned(),
            message.get("method").and_then(Value::as_str),
        ) {
            let result = server_request_result(method, &message["params"], &self.root);
            process
                .send(&json!({"jsonrpc":"2.0","id":id,"result":result}))
                .map_err(transport_error)?;
        }
        Ok(())
    }
}

fn receive_before(process: &mut dyn RpcTransport, deadline: Instant) -> AppResult<Value> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(AppError::new(
            "LSP_TIMEOUT",
            "Language-server response timed out",
        ));
    }
    process.receive(remaining).map_err(|error| {
        if error.kind() == io::ErrorKind::TimedOut {
            AppError::new("LSP_TIMEOUT", "Language-server response timed out")
        } else {
            transport_error(error)
        }
    })
}

fn latency_p50(samples: &VecDeque<u128>) -> Option<u128> {
    if samples.is_empty() {
        return None;
    }
    let mut values = samples.iter().copied().collect::<Vec<_>>();
    values.sort_unstable();
    Some(values[(values.len() - 1) / 2])
}

fn transport_error(error: io::Error) -> AppError {
    AppError::new("LSP_TRANSPORT_ERROR", error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use serde_json::json;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct FakeTransport {
        sent: Arc<Mutex<Vec<Value>>>,
        queued: VecDeque<io::Result<Value>>,
        fail_request_once: Arc<AtomicBool>,
    }

    impl RpcTransport for FakeTransport {
        fn send(&mut self, value: &Value) -> io::Result<()> {
            self.sent.lock().push(value.clone());
            if let (Some(id), Some(method)) = (
                value.get("id").and_then(Value::as_u64),
                value.get("method").and_then(Value::as_str),
            ) {
                if method == "initialize" {
                    self.queued.push_back(Ok(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "capabilities": {
                                "referencesProvider": true,
                                "definitionProvider": true,
                                "renameProvider": true,
                                "diagnosticProvider": {},
                                "textDocumentSync": {"change": 2},
                                "positionEncoding": "utf-8"
                            },
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
                let uri = value["params"]["textDocument"]["uri"]
                    .as_str()
                    .unwrap()
                    .to_owned();
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
        Arc::new(move || {
            Ok(Box::new(FakeTransport {
                sent: Arc::clone(&sent),
                queued: VecDeque::new(),
                fail_request_once: Arc::clone(&fail_request_once),
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
}
