//! One worker thread per configured language server. The thread owns the
//! server process; callers talk to it through a bounded queue and every
//! request carries a deadline the worker honors.
mod error;
mod server;

pub(crate) use error::WorkerError;

use super::backend::SemanticBackend;
use super::preset::LspPreset;
use super::protocol::ServerCapabilities;
use super::request::Position;
use super::sync::{DocumentSnapshot, SynchronizedDocument};
use super::transport::{RpcTransport, StdioRpcTransport, TransportFactory};
use super::uri::UriCodec;
use crate::error::{AppError, AppResult, ErrorCode};
use crate::model::LanguageServerSettings;
use parking_lot::RwLock;
use serde_json::{json, Value};
use server::{RequestContext, Server};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Requests waiting for the worker beyond this are refused with `LSP_BUSY`.
const QUEUE_CAPACITY: usize = 16;
/// Extra time the caller waits past the request deadline for the worker's reply.
const REPLY_GRACE: Duration = Duration::from_millis(250);
const LATENCY_SAMPLES: usize = 64;

#[derive(Debug, Clone)]
pub(crate) enum WorkerOperation {
    Definition {
        position: Position,
    },
    References {
        position: Position,
    },
    Rename {
        position: Position,
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

    fn method(&self) -> &'static str {
        match self {
            Self::Definition { .. } => "textDocument/definition",
            Self::References { .. } => "textDocument/references",
            Self::Rename { .. } => "textDocument/rename",
            Self::Diagnostics => "textDocument/diagnostic",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct WorkerResponse {
    pub(crate) result: Value,
    pub(crate) synchronized: SynchronizedDocument,
    pub(crate) capabilities: ServerCapabilities,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Readiness {
    Disabled,
    Lazy,
    Starting,
    Ready,
}

impl Readiness {
    fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Lazy => "lazy",
            Self::Starting => "starting",
            Self::Ready => "ready",
        }
    }
}

#[derive(Debug, Clone)]
struct WorkerStatus {
    readiness: Readiness,
    restart_count: u64,
    last_error: Option<String>,
    capabilities: Option<ServerCapabilities>,
    synchronized_document_count: usize,
    request_count: u64,
    first_request_ms: Option<u128>,
    last_request_ms: Option<u128>,
    warm_request_samples_ms: VecDeque<u128>,
}

impl WorkerStatus {
    fn new(readiness: Readiness) -> Self {
        Self {
            readiness,
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

    fn record_latency(&mut self, elapsed_ms: u128) {
        self.request_count += 1;
        self.last_request_ms = Some(elapsed_ms);
        if self.first_request_ms.is_none() {
            self.first_request_ms = Some(elapsed_ms);
        } else {
            self.warm_request_samples_ms.push_back(elapsed_ms);
            if self.warm_request_samples_ms.len() > LATENCY_SAMPLES {
                self.warm_request_samples_ms.pop_front();
            }
        }
    }
}

struct Command {
    operation: WorkerOperation,
    document: DocumentSnapshot,
    deadline: Instant,
    reply: Sender<Result<WorkerResponse, WorkerError>>,
}

/// The caller's side of a running worker thread.
struct WorkerThread {
    sender: Option<SyncSender<Command>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for WorkerThread {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.sender.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub(crate) struct LspWorker {
    preset: LspPreset,
    command: String,
    args: Vec<String>,
    timeout: Duration,
    thread: Option<WorkerThread>,
    status: Arc<RwLock<WorkerStatus>>,
}

impl LspWorker {
    /// Start the worker for one preset. A disabled preset gets no thread.
    pub(crate) fn spawn(
        root: &Path,
        preset: LspPreset,
        settings: &LanguageServerSettings,
    ) -> AppResult<Self> {
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
        let (spawn_command, spawn_args, cwd) = (command.clone(), args.clone(), root.to_owned());
        let factory: TransportFactory = Arc::new(move || {
            StdioRpcTransport::spawn(&spawn_command, &spawn_args, &cwd)
                .map(|process| Box::new(process) as Box<dyn RpcTransport>)
        });
        Self::with_factory(root, preset, settings, command, args, factory)
    }

    fn with_factory(
        root: &Path,
        preset: LspPreset,
        settings: &LanguageServerSettings,
        command: String,
        args: Vec<String>,
        factory: TransportFactory,
    ) -> AppResult<Self> {
        let timeout = Duration::from_millis(settings.timeout_ms.max(1));
        let readiness = if settings.enabled {
            Readiness::Lazy
        } else {
            Readiness::Disabled
        };
        let status = Arc::new(RwLock::new(WorkerStatus::new(readiness)));
        let thread = if settings.enabled {
            let (sender, receiver) = mpsc::sync_channel(QUEUE_CAPACITY);
            let stop = Arc::new(AtomicBool::new(false));
            let runtime = WorkerRuntime {
                root: root.to_owned(),
                codec: UriCodec::new(root),
                preset,
                timeout,
                factory,
                status: Arc::clone(&status),
                stop: Arc::clone(&stop),
                server: ServerState::Stopped,
            };
            let handle = thread::Builder::new()
                .name(format!("codeweave-lsp-{}", preset.key()))
                .spawn(move || runtime.run(receiver))
                .map_err(|error| {
                    AppError::with_details(
                        ErrorCode::LspWorkerStartFailed,
                        format!("Cannot start the language-server worker thread: {error}"),
                        json!({"language": preset.key()}),
                    )
                })?;
            Some(WorkerThread {
                sender: Some(sender),
                stop,
                thread: Some(handle),
            })
        } else {
            None
        };
        Ok(Self {
            preset,
            command,
            args,
            timeout,
            thread,
            status,
        })
    }

    /// A worker over an arbitrary transport, for tests.
    #[cfg(test)]
    pub(crate) fn with_transport(
        root: &Path,
        preset: LspPreset,
        timeout: Duration,
        factory: TransportFactory,
    ) -> Self {
        let settings = LanguageServerSettings {
            enabled: true,
            command: "fixture".to_owned(),
            args: Vec::new(),
            timeout_ms: timeout.as_millis() as u64,
        };
        Self::with_factory(
            root,
            preset,
            &settings,
            "fixture".to_owned(),
            Vec::new(),
            factory,
        )
        .expect("test worker thread starts")
    }

    /// The longest a caller waits for one request, including one restart.
    fn request_budget(&self) -> Duration {
        self.timeout.saturating_mul(4) + Duration::from_secs(2)
    }
}

impl SemanticBackend for LspWorker {
    fn preset(&self) -> LspPreset {
        self.preset
    }

    fn configured(&self) -> bool {
        self.thread.is_some()
    }

    fn execute(
        &self,
        operation: WorkerOperation,
        document: DocumentSnapshot,
    ) -> Result<WorkerResponse, WorkerError> {
        let language = self.preset.key();
        let sender = self
            .thread
            .as_ref()
            .ok_or(WorkerError::Disabled { language })?
            .sender
            .as_ref()
            .ok_or(WorkerError::Stopped)?;
        let budget = self.request_budget();
        let (reply, response) = mpsc::channel();
        sender
            .try_send(Command {
                operation,
                document,
                deadline: Instant::now() + budget,
                reply,
            })
            .map_err(|error| match error {
                TrySendError::Full(_) => WorkerError::Busy { language },
                TrySendError::Disconnected(_) => WorkerError::Stopped,
            })?;
        match response.recv_timeout(budget + REPLY_GRACE) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => Err(WorkerError::TimedOut {
                language,
                stderr_tail: None,
            }),
            Err(RecvTimeoutError::Disconnected) => Err(WorkerError::Stopped),
        }
    }

    fn status(&self) -> Value {
        let status = self.status.read().clone();
        let initialization_ms = status
            .capabilities
            .as_ref()
            .map(|capabilities| capabilities.initialization_ms);
        json!({
            "language": self.preset.key(),
            "configured": self.thread.is_some(),
            "command": self.command,
            "args": self.args,
            "timeout_ms": self.timeout.as_millis(),
            "readiness": status.readiness.as_str(),
            "restart_count": status.restart_count,
            "last_error": status.last_error,
            "synchronized_document_count": status.synchronized_document_count,
            "capabilities": status.capabilities.map(|value| value.to_json()),
            "latency_ms": {
                "initialization": initialization_ms,
                "first_request": status.first_request_ms,
                "last_request": status.last_request_ms,
                "warm_request_p50": latency_p50(&status.warm_request_samples_ms),
                "request_count": status.request_count
            },
            "fallback": "tree_sitter_then_lexical"
        })
    }
}

#[derive(Default)]
enum ServerState {
    #[default]
    Stopped,
    Ready(Box<Server>),
}

struct WorkerRuntime {
    root: PathBuf,
    codec: UriCodec,
    preset: LspPreset,
    timeout: Duration,
    factory: TransportFactory,
    status: Arc<RwLock<WorkerStatus>>,
    stop: Arc<AtomicBool>,
    server: ServerState,
}

impl WorkerRuntime {
    fn run(mut self, receiver: Receiver<Command>) {
        while let Ok(command) = receiver.recv() {
            let result = if self.stop.load(Ordering::Acquire) {
                Err(WorkerError::Stopped)
            } else if Instant::now() >= command.deadline {
                // The caller has given up; never start abandoned work.
                Err(WorkerError::TimedOut {
                    language: self.preset.key(),
                    stderr_tail: None,
                })
            } else {
                let started = Instant::now();
                let result = self.execute_with_restart(
                    &command.operation,
                    &command.document,
                    command.deadline,
                );
                self.status
                    .write()
                    .record_latency(started.elapsed().as_millis());
                result
            };
            let _ = command.reply.send(result);
        }
    }

    /// Run once; after a restartable failure, retry once on a fresh server
    /// if the deadline allows.
    fn execute_with_restart(
        &mut self,
        operation: &WorkerOperation,
        document: &DocumentSnapshot,
        deadline: Instant,
    ) -> Result<WorkerResponse, WorkerError> {
        let first = self.execute_once(operation, document, deadline);
        let result = match first {
            Err(error)
                if error.restartable()
                    && Instant::now() < deadline
                    && !self.stop.load(Ordering::Acquire) =>
            {
                self.record_failure(&error, true);
                self.execute_once(operation, document, deadline)
            }
            other => other,
        };
        match &result {
            Ok(_) => self.record_success(),
            Err(error) => self.record_failure(error, false),
        }
        result
    }

    fn execute_once(
        &mut self,
        operation: &WorkerOperation,
        document: &DocumentSnapshot,
        deadline: Instant,
    ) -> Result<WorkerResponse, WorkerError> {
        let context = RequestContext {
            root: &self.root,
            codec: &self.codec,
            language: self.preset.key(),
            timeout: self.timeout,
            deadline,
            stop: &self.stop,
        };
        let mut server = match std::mem::take(&mut self.server) {
            ServerState::Ready(server) => server,
            ServerState::Stopped => {
                self.status.write().readiness = Readiness::Starting;
                let transport = (self.factory)().map_err(|error| WorkerError::StartFailed {
                    language: context.language,
                    message: error.to_string(),
                })?;
                let server = Server::initialize(transport, &context)?;
                let mut status = self.status.write();
                status.readiness = Readiness::Ready;
                status.capabilities = Some(server.capabilities().clone());
                Box::new(server)
            }
        };
        let result = server.execute(operation, document, &context);
        let discard = matches!(&result, Err(error) if error.restartable() || matches!(error, WorkerError::Stopped));
        if !discard {
            self.server = ServerState::Ready(server);
        }
        // A discarded server is dropped here, which terminates its process.
        result
    }

    fn record_success(&self) {
        let mut status = self.status.write();
        status.readiness = Readiness::Ready;
        status.last_error = None;
        if let ServerState::Ready(server) = &self.server {
            status.synchronized_document_count = server.document_count();
        }
    }

    fn record_failure(&self, error: &WorkerError, restarting: bool) {
        let mut status = self.status.write();
        status.last_error = Some(error.to_app_error().message().to_owned());
        if restarting {
            status.restart_count += 1;
        }
        if matches!(self.server, ServerState::Stopped) {
            status.readiness = Readiness::Lazy;
            status.capabilities = None;
            status.synchronized_document_count = 0;
        }
    }
}

fn latency_p50(samples: &VecDeque<u128>) -> Option<u128> {
    if samples.is_empty() {
        return None;
    }
    let mut values = samples.iter().copied().collect::<Vec<_>>();
    values.sort_unstable();
    Some(values[(values.len() - 1) / 2])
}

#[cfg(test)]
mod tests;
