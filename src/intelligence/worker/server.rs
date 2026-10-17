//! One running, initialized language-server process and the documents and
//! diagnostics it has been told about.
use super::super::position::position_params;
use super::super::protocol::{
    initialize_params, parse_initialize_result, server_request_result, ServerCapabilities,
    TextDocumentSyncKind,
};
use super::super::sync::{plan_sync, DocumentSnapshot, DocumentState};
use super::super::transport::{RpcTransport, TransportError};
use super::super::uri::UriCodec;
use super::error::WorkerError;
use super::{WorkerOperation, WorkerResponse};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Longest single blocking receive, so shutdown is noticed promptly.
const RECEIVE_SLICE: Duration = Duration::from_millis(100);

/// Per-request facts the server needs from its worker.
pub(super) struct RequestContext<'a> {
    pub(super) root: &'a Path,
    pub(super) codec: &'a UriCodec,
    pub(super) language: &'static str,
    pub(super) timeout: Duration,
    pub(super) deadline: Instant,
    pub(super) stop: &'a AtomicBool,
}

impl RequestContext<'_> {
    /// When the next awaited message must have arrived.
    fn message_deadline(&self) -> Instant {
        (Instant::now() + self.timeout).min(self.deadline)
    }
}

pub(super) struct Server {
    transport: Box<dyn RpcTransport>,
    capabilities: ServerCapabilities,
    next_id: u64,
    documents: HashMap<String, DocumentState>,
    diagnostics: HashMap<String, Value>,
}

impl Server {
    /// Initialize a freshly spawned transport.
    pub(super) fn initialize(
        transport: Box<dyn RpcTransport>,
        context: &RequestContext<'_>,
    ) -> Result<Self, WorkerError> {
        let started = Instant::now();
        let mut server = Self {
            transport,
            capabilities: ServerCapabilities::none(),
            next_id: 1,
            documents: HashMap::new(),
            diagnostics: HashMap::new(),
        };
        let result = server.request(
            "initialize",
            initialize_params(&context.codec.root_uri()),
            context,
        )?;
        server.capabilities = parse_initialize_result(&result, started.elapsed().as_millis())
            .map_err(|error| WorkerError::Protocol {
                message: error.0.message,
                details: error.0.details,
            })?;
        server.notify(
            &json!({"jsonrpc":"2.0","method":"initialized","params":{}}),
            context,
        )?;
        Ok(server)
    }

    pub(super) fn capabilities(&self) -> &ServerCapabilities {
        &self.capabilities
    }

    pub(super) fn document_count(&self) -> usize {
        self.documents.len()
    }

    pub(super) fn execute(
        &mut self,
        operation: &WorkerOperation,
        document: &DocumentSnapshot,
        context: &RequestContext<'_>,
    ) -> Result<WorkerResponse, WorkerError> {
        let capabilities = self.capabilities.clone();
        if !capabilities.supports(operation.capability_name()) {
            return Err(WorkerError::Unsupported {
                operation: operation.capability_name(),
            });
        }
        if capabilities.sync_kind == TextDocumentSyncKind::None {
            return Err(WorkerError::SyncUnsupported);
        }
        let sync = plan_sync(&mut self.documents, document);
        if let Some(notification) = &sync.notification {
            self.notify(notification, context)?;
            if sync.changed {
                self.diagnostics.remove(&document.uri);
            }
        }

        let result = match operation {
            WorkerOperation::Diagnostics => self.diagnostics_for(&document.uri, context)?,
            WorkerOperation::Definition { position }
            | WorkerOperation::References { position }
            | WorkerOperation::Rename { position, .. } => {
                let mut params = position_params(
                    &document.uri,
                    &document.content,
                    *position,
                    capabilities.position_encoding,
                )
                .map_err(WorkerError::Request)?;
                match operation {
                    WorkerOperation::References { .. } => {
                        params["context"] = json!({"includeDeclaration": false});
                    }
                    WorkerOperation::Rename { new_name, .. } => {
                        params["newName"] = json!(new_name);
                    }
                    WorkerOperation::Definition { .. } | WorkerOperation::Diagnostics => {}
                }
                self.request(operation.method(), params, context)?
            }
        };
        Ok(WorkerResponse {
            result,
            synchronized: sync.synchronized,
            capabilities,
        })
    }

    fn diagnostics_for(
        &mut self,
        uri: &str,
        context: &RequestContext<'_>,
    ) -> Result<Value, WorkerError> {
        if let Some(cached) = self.diagnostics.get(uri) {
            return Ok(cached.clone());
        }
        if self.capabilities.pull_diagnostics_provider {
            let report = self.request(
                "textDocument/diagnostic",
                json!({"textDocument": {"uri": uri}}),
                context,
            )?;
            let diagnostics = diagnostic_report_items(report)?;
            self.diagnostics.insert(uri.to_owned(), diagnostics.clone());
            return Ok(diagnostics);
        }
        let deadline = context.message_deadline();
        loop {
            let message = self.receive_before(deadline, context)?;
            if let Some((published, diagnostics)) = published_diagnostics(&message) {
                let matches = diagnostic_uri_matches(context.codec, uri, published);
                let diagnostics = diagnostics.clone();
                self.diagnostics
                    .insert(published.to_owned(), diagnostics.clone());
                if matches {
                    self.diagnostics.insert(uri.to_owned(), diagnostics.clone());
                    return Ok(diagnostics);
                }
            } else {
                self.answer_server_request(&message, context)?;
            }
        }
    }

    fn request(
        &mut self,
        method: &str,
        params: Value,
        context: &RequestContext<'_>,
    ) -> Result<Value, WorkerError> {
        let id = self.next_id;
        self.next_id += 1;
        self.notify(
            &json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}),
            context,
        )?;
        let deadline = context.message_deadline();
        loop {
            let message = self.receive_before(deadline, context)?;
            if message.get("id").and_then(Value::as_u64) == Some(id)
                && message.get("method").is_none()
            {
                if let Some(error) = message.get("error") {
                    return Err(WorkerError::Server {
                        language: context.language,
                        error: error.clone(),
                    });
                }
                return message.get("result").cloned().ok_or_else(|| {
                    WorkerError::protocol("JSON-RPC response contains neither result nor error")
                });
            }
            self.handle_message(&message, context)?;
        }
    }

    fn handle_message(
        &mut self,
        message: &Value,
        context: &RequestContext<'_>,
    ) -> Result<(), WorkerError> {
        if let Some((published, diagnostics)) = published_diagnostics(message) {
            self.diagnostics
                .insert(published.to_owned(), diagnostics.clone());
            return Ok(());
        }
        self.answer_server_request(message, context)
    }

    fn answer_server_request(
        &mut self,
        message: &Value,
        context: &RequestContext<'_>,
    ) -> Result<(), WorkerError> {
        if let (Some(id), Some(method)) = (
            message.get("id"),
            message.get("method").and_then(Value::as_str),
        ) {
            let result = server_request_result(method, &message["params"], context.root);
            self.notify(&json!({"jsonrpc":"2.0","id":id,"result":result}), context)?;
        }
        Ok(())
    }

    fn notify(&mut self, value: &Value, context: &RequestContext<'_>) -> Result<(), WorkerError> {
        self.transport
            .send(value)
            .map_err(|error| self.transport_error(error, context))
    }

    /// Wait for the next message, polling in short slices so a worker
    /// shutdown or the request deadline interrupts the wait.
    fn receive_before(
        &mut self,
        deadline: Instant,
        context: &RequestContext<'_>,
    ) -> Result<Value, WorkerError> {
        loop {
            if context.stop.load(Ordering::Acquire) {
                return Err(WorkerError::Stopped);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(self.transport_error(TransportError::TimedOut, context));
            }
            match self.transport.receive(remaining.min(RECEIVE_SLICE)) {
                Ok(message) => return Ok(message),
                Err(TransportError::TimedOut) if remaining > RECEIVE_SLICE => continue,
                Err(error) => return Err(self.transport_error(error, context)),
            }
        }
    }

    fn transport_error(&self, error: TransportError, context: &RequestContext<'_>) -> WorkerError {
        WorkerError::from_transport(error, context.language, self.transport.stderr_tail())
    }
}

fn published_diagnostics(message: &Value) -> Option<(&str, &Value)> {
    if message.get("method").and_then(Value::as_str) != Some("textDocument/publishDiagnostics") {
        return None;
    }
    let params = message.get("params")?;
    Some((params.get("uri")?.as_str()?, params.get("diagnostics")?))
}

fn diagnostic_report_items(report: Value) -> Result<Value, WorkerError> {
    if report.is_array() {
        return Ok(report);
    }
    if let Some(items) = report.get("items").filter(|items| items.is_array()) {
        return Ok(items.clone());
    }
    Err(WorkerError::Protocol {
        message: "Language-server diagnostic response does not contain an items array".into(),
        details: Some(json!({"response": report})),
    })
}

/// Published URIs may differ from the requested one in escaping or Windows
/// drive casing; compare the workspace paths they name.
pub(super) fn diagnostic_uri_matches(codec: &UriCodec, requested: &str, published: &str) -> bool {
    requested == published
        || matches!(
            (codec.relative(requested), codec.relative(published)),
            (Ok(requested), Ok(published)) if requested == published
        )
}
