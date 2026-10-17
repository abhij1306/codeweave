use super::super::transport::TransportError;
use crate::error::{AppError, ErrorCode};
use serde_json::{json, Value};

/// Why a semantic request did not produce a response. The restart policy and
/// the public error code both derive from the variant.
#[derive(Debug)]
pub(crate) enum WorkerError {
    /// The backend is not enabled in configuration.
    Disabled { language: &'static str },
    /// The bounded request queue is full.
    Busy { language: &'static str },
    /// The worker thread is gone or shutting down.
    Stopped,
    /// The server process could not be spawned.
    StartFailed {
        language: &'static str,
        message: String,
    },
    /// No answer before the per-message timeout or the request deadline.
    TimedOut {
        language: &'static str,
        stderr_tail: Option<String>,
    },
    /// The server exited or closed its output.
    ServerExited {
        language: &'static str,
        stderr_tail: Option<String>,
    },
    /// Writing to or reading from the server failed for another reason.
    Transport { message: String },
    /// The server violated the protocol.
    Protocol {
        message: String,
        details: Option<Value>,
    },
    /// The server answered the request with a JSON-RPC error.
    Server {
        language: &'static str,
        error: Value,
    },
    /// The server does not advertise the operation.
    Unsupported { operation: &'static str },
    /// The server does not advertise document synchronization.
    SyncUnsupported,
    /// The request itself could not be expressed (for example, a position
    /// outside the document). Not a server fault.
    Request(AppError),
}

impl WorkerError {
    /// Whether the server process should be discarded and the request retried
    /// on a fresh one.
    pub(crate) fn restartable(&self) -> bool {
        matches!(
            self,
            Self::TimedOut { .. }
                | Self::ServerExited { .. }
                | Self::Transport { .. }
                | Self::Protocol { .. }
        )
    }

    pub(super) fn from_transport(
        error: TransportError,
        language: &'static str,
        stderr_tail: Option<String>,
    ) -> Self {
        match error {
            TransportError::TimedOut => Self::TimedOut {
                language,
                stderr_tail,
            },
            TransportError::Disconnected => Self::ServerExited {
                language,
                stderr_tail,
            },
            TransportError::Protocol(message) => Self::Protocol {
                message,
                details: None,
            },
            TransportError::Io(error) => Self::Transport {
                message: error.to_string(),
            },
        }
    }

    pub(crate) fn protocol(message: impl Into<String>) -> Self {
        Self::Protocol {
            message: message.into(),
            details: None,
        }
    }
}

impl From<WorkerError> for AppError {
    fn from(error: WorkerError) -> Self {
        error.to_app_error()
    }
}

impl WorkerError {
    /// The public error for this failure.
    pub(crate) fn to_app_error(&self) -> AppError {
        match self {
            WorkerError::Disabled { language } => AppError::new(
                ErrorCode::SemanticBackendUnavailable,
                format!("{language} language server is disabled"),
            ),
            WorkerError::Busy { language } => AppError::with_details(
                ErrorCode::LspBusy,
                "Language-server request queue is full",
                json!({"language": language}),
            ),
            WorkerError::Stopped => AppError::new(
                ErrorCode::LspWorkerStopped,
                "Language-server worker has stopped",
            ),
            WorkerError::StartFailed { language, message } => AppError::with_details(
                ErrorCode::LspStartFailed,
                message.clone(),
                json!({"language": language}),
            ),
            WorkerError::TimedOut {
                language,
                stderr_tail,
            } => AppError::with_details(
                ErrorCode::LspTimeout,
                "Language-server response timed out",
                json!({"language": language, "stderr_tail": stderr_tail}),
            ),
            WorkerError::ServerExited {
                language,
                stderr_tail,
            } => AppError::with_details(
                ErrorCode::LspTransportError,
                "Language server exited or closed its connection",
                json!({"language": language, "cause": "server_exited", "stderr_tail": stderr_tail}),
            ),
            WorkerError::Transport { message } => {
                AppError::new(ErrorCode::LspTransportError, message.clone())
            }
            WorkerError::Protocol { message, details } => match details {
                Some(details) => AppError::with_details(
                    ErrorCode::LspProtocolError,
                    message.clone(),
                    details.clone(),
                ),
                None => AppError::new(ErrorCode::LspProtocolError, message.clone()),
            },
            WorkerError::Server { language, error } => AppError::with_details(
                ErrorCode::LspError,
                format!("{language} language-server request failed"),
                error.clone(),
            ),
            WorkerError::Unsupported { operation } => AppError::with_details(
                ErrorCode::LspUnsupported,
                format!("Language server does not advertise {operation} support"),
                json!({"operation": operation}),
            ),
            WorkerError::SyncUnsupported => AppError::new(
                ErrorCode::LspSyncUnsupported,
                "Language server does not advertise document synchronization",
            ),
            WorkerError::Request(error) => error.clone(),
        }
    }
}
