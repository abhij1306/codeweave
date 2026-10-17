//! The application error: one typed [`ErrorCode`], a message and optional
//! structured details.
//!
//! Every public error code is a variant of [`ErrorCode`], which serializes to
//! the wire string clients see. Policy that depends on the code (whether a
//! client may retry, the hint a request-validation error carries) is defined on
//! the enum, so no caller compares code strings.

use serde::{Serialize, Serializer};
use serde_json::Value;
use std::fmt;
use std::io;

pub type AppResult<T> = Result<T, AppError>;

macro_rules! error_codes {
    ($($(#[$doc:meta])* $variant:ident => $wire:literal,)+) => {
        /// Every error code a tool result can carry.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum ErrorCode {
            $($(#[$doc])* $variant,)+
        }

        impl ErrorCode {
            /// Every code, in declaration order.
            pub const ALL: &'static [ErrorCode] = &[$(ErrorCode::$variant,)+];

            /// The wire string for this code.
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(ErrorCode::$variant => $wire,)+
                }
            }
        }
    };
}

error_codes! {
    AmbiguousHandleEditOrder => "AMBIGUOUS_HANDLE_EDIT_ORDER",
    AmbiguousSymbol => "AMBIGUOUS_SYMBOL",
    AtomicWriteFailed => "ATOMIC_WRITE_FAILED",
    BashExecutionFailed => "BASH_EXECUTION_FAILED",
    BashPipeFailed => "BASH_PIPE_FAILED",
    BashQueueClosed => "BASH_QUEUE_CLOSED",
    BashStartFailed => "BASH_START_FAILED",
    BashUnavailable => "BASH_UNAVAILABLE",
    ConfirmationRequired => "CONFIRMATION_REQUIRED",
    ContinuationScopeMismatch => "CONTINUATION_SCOPE_MISMATCH",
    DuplicateRetrievalOperationId => "DUPLICATE_RETRIEVAL_OPERATION_ID",
    ExactMatchCount => "EXACT_MATCH_COUNT",
    GitFailed => "GIT_FAILED",
    GitTimeout => "GIT_TIMEOUT",
    GitUnavailable => "GIT_UNAVAILABLE",
    IndexRefreshFailed => "INDEX_REFRESH_FAILED",
    IntelligenceStale => "INTELLIGENCE_STALE",
    InternalError => "INTERNAL_ERROR",
    InvalidArgument => "INVALID_ARGUMENT",
    InvalidBashRequest => "INVALID_BASH_REQUEST",
    InvalidChangeField => "INVALID_CHANGE_FIELD",
    InvalidConfig => "INVALID_CONFIG",
    InvalidContinuation => "INVALID_CONTINUATION",
    InvalidCwd => "INVALID_CWD",
    InvalidExcludePattern => "INVALID_EXCLUDE_PATTERN",
    InvalidFetchKind => "INVALID_FETCH_KIND",
    InvalidHandle => "INVALID_HANDLE",
    InvalidHandleRange => "INVALID_HANDLE_RANGE",
    InvalidJson => "INVALID_JSON",
    InvalidLspEdit => "INVALID_LSP_EDIT",
    InvalidLspPosition => "INVALID_LSP_POSITION",
    InvalidLspResponse => "INVALID_LSP_RESPONSE",
    InvalidLspUri => "INVALID_LSP_URI",
    InvalidOperationField => "INVALID_OPERATION_FIELD",
    InvalidOutlinePath => "INVALID_OUTLINE_PATH",
    InvalidPolicy => "INVALID_POLICY",
    InvalidRegex => "INVALID_REGEX",
    InvalidTimeout => "INVALID_TIMEOUT",
    LspBusy => "LSP_BUSY",
    LspError => "LSP_ERROR",
    LspProtocolError => "LSP_PROTOCOL_ERROR",
    LspStartFailed => "LSP_START_FAILED",
    LspSyncUnsupported => "LSP_SYNC_UNSUPPORTED",
    LspTimeout => "LSP_TIMEOUT",
    LspTransportError => "LSP_TRANSPORT_ERROR",
    LspUnsupported => "LSP_UNSUPPORTED",
    LspWorkerStartFailed => "LSP_WORKER_START_FAILED",
    LspWorkerStopped => "LSP_WORKER_STOPPED",
    MissingChangeField => "MISSING_CHANGE_FIELD",
    MissingHandle => "MISSING_HANDLE",
    MissingOperationField => "MISSING_OPERATION_FIELD",
    MissingPrecondition => "MISSING_PRECONDITION",
    NotGitRepository => "NOT_GIT_REPOSITORY",
    NotInitialized => "NOT_INITIALIZED",
    OutsideRoot => "OUTSIDE_ROOT",
    OverlappingEdits => "OVERLAPPING_EDITS",
    OverlappingLspEdits => "OVERLAPPING_LSP_EDITS",
    PartialCommit => "PARTIAL_COMMIT",
    PathExists => "PATH_EXISTS",
    PathNotFound => "PATH_NOT_FOUND",
    PathNotIndexed => "PATH_NOT_INDEXED",
    /// The operating system refused access to a workspace path.
    PermissionDenied => "PERMISSION_DENIED",
    ReadFailed => "READ_FAILED",
    RunBusy => "RUN_BUSY",
    RunNotFound => "RUN_NOT_FOUND",
    SemanticBackendUnavailable => "SEMANTIC_BACKEND_UNAVAILABLE",
    StaleContinuation => "STALE_CONTINUATION",
    StaleFile => "STALE_FILE",
    StaleHandle => "STALE_HANDLE",
    StaleSnapshot => "STALE_SNAPSHOT",
    SymbolNotFound => "SYMBOL_NOT_FOUND",
    SyntaxError => "SYNTAX_ERROR",
    TooManyRetrievalOperations => "TOO_MANY_RETRIEVAL_OPERATIONS",
    UnknownChangeField => "UNKNOWN_CHANGE_FIELD",
    UnknownOperationField => "UNKNOWN_OPERATION_FIELD",
    UnknownToolField => "UNKNOWN_TOOL_FIELD",
    UnsupportedChangeKind => "UNSUPPORTED_CHANGE_KIND",
    UnsupportedLspUri => "UNSUPPORTED_LSP_URI",
    UnsupportedReadTarget => "UNSUPPORTED_READ_TARGET",
    UnsupportedRetrievalOperation => "UNSUPPORTED_RETRIEVAL_OPERATION",
    UnsupportedWorkspaceEdit => "UNSUPPORTED_WORKSPACE_EDIT",
    WorkspaceNotDirectory => "WORKSPACE_NOT_DIRECTORY",
    WorkspaceNotFound => "WORKSPACE_NOT_FOUND",
}

/// The fixed retry hint and category a request-validation error carries in
/// its details.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestPolicy {
    pub retry_kind: &'static str,
    pub category: &'static str,
}

impl ErrorCode {
    /// Whether the failed request may be retried unchanged: the state it was
    /// checked against moved on, and a fresh attempt re-reads it.
    pub const fn retries_same_request(self) -> bool {
        matches!(
            self,
            Self::StaleSnapshot | Self::StaleFile | Self::StaleHandle | Self::StaleContinuation
        )
    }

    /// The retry hint and category for request-validation codes. Errors with
    /// these codes always carry them (and `retryable: false`) in their details.
    pub const fn request_policy(self) -> Option<RequestPolicy> {
        const fn policy(retry_kind: &'static str, category: &'static str) -> RequestPolicy {
            RequestPolicy {
                retry_kind,
                category,
            }
        }
        Some(match self {
            Self::MissingOperationField
            | Self::InvalidOperationField
            | Self::MissingChangeField
            | Self::InvalidChangeField
            | Self::InvalidBashRequest => policy("fix_request", "invalid_request"),
            Self::UnknownOperationField | Self::UnknownChangeField => {
                policy("remove_field", "invalid_request")
            }
            Self::UnsupportedRetrievalOperation => {
                policy("choose_supported_operation", "unsupported_request")
            }
            Self::UnsupportedChangeKind => policy("choose_supported_change", "unsupported_request"),
            _ => return None,
        })
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl Serialize for ErrorCode {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// A failed operation. Serializes as `{code, message, details?}`.
#[derive(Debug, Clone, Serialize)]
pub struct AppError {
    code: ErrorCode,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<Value>,
}

impl AppError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self::build(code, message.into(), None)
    }

    pub fn with_details(code: ErrorCode, message: impl Into<String>, details: Value) -> Self {
        Self::build(code, message.into(), Some(details))
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::InvalidArgument, message)
    }

    pub fn internal(error: impl fmt::Display) -> Self {
        Self::new(ErrorCode::InternalError, error.to_string())
    }

    fn build(code: ErrorCode, message: String, details: Option<Value>) -> Self {
        let details = match code.request_policy() {
            Some(policy) => {
                let mut object = details
                    .and_then(|details| match details {
                        Value::Object(object) => Some(object),
                        _ => None,
                    })
                    .unwrap_or_default();
                object.insert("retryable".to_owned(), Value::Bool(false));
                object.insert(
                    "retry_kind".to_owned(),
                    Value::String(policy.retry_kind.to_owned()),
                );
                object.insert(
                    "category".to_owned(),
                    Value::String(policy.category.to_owned()),
                );
                Some(Value::Object(object))
            }
            None => details,
        };
        Self {
            code,
            message,
            details,
        }
    }

    pub fn code(&self) -> ErrorCode {
        self.code
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn details(&self) -> Option<&Value> {
        self.details.as_ref()
    }

    /// The error with `message` in place of its own.
    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = message.into();
        self
    }
}

/// Filesystem failures keep the meaning a client can act on: a missing path,
/// an existing path, or a refused access. Everything else is internal.
impl From<io::Error> for AppError {
    fn from(error: io::Error) -> Self {
        let code = match error.kind() {
            io::ErrorKind::NotFound => ErrorCode::PathNotFound,
            io::ErrorKind::AlreadyExists => ErrorCode::PathExists,
            io::ErrorKind::PermissionDenied => ErrorCode::PermissionDenied,
            _ => ErrorCode::InternalError,
        };
        Self::new(code, error.to_string())
    }
}

impl From<serde_json::Error> for AppError {
    fn from(error: serde_json::Error) -> Self {
        Self::new(ErrorCode::InvalidJson, error.to_string())
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "[{}] {}", self.code, self.message)
    }
}

impl std::error::Error for AppError {}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The wire strings clients match on. Changing one is a breaking change.
    #[test]
    fn every_code_serializes_to_its_wire_string() {
        let wire = ErrorCode::ALL
            .iter()
            .map(|code| serde_json::to_value(code).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            wire,
            [
                "AMBIGUOUS_HANDLE_EDIT_ORDER",
                "AMBIGUOUS_SYMBOL",
                "ATOMIC_WRITE_FAILED",
                "BASH_EXECUTION_FAILED",
                "BASH_PIPE_FAILED",
                "BASH_QUEUE_CLOSED",
                "BASH_START_FAILED",
                "BASH_UNAVAILABLE",
                "CONFIRMATION_REQUIRED",
                "CONTINUATION_SCOPE_MISMATCH",
                "DUPLICATE_RETRIEVAL_OPERATION_ID",
                "EXACT_MATCH_COUNT",
                "GIT_FAILED",
                "GIT_TIMEOUT",
                "GIT_UNAVAILABLE",
                "INDEX_REFRESH_FAILED",
                "INTELLIGENCE_STALE",
                "INTERNAL_ERROR",
                "INVALID_ARGUMENT",
                "INVALID_BASH_REQUEST",
                "INVALID_CHANGE_FIELD",
                "INVALID_CONFIG",
                "INVALID_CONTINUATION",
                "INVALID_CWD",
                "INVALID_EXCLUDE_PATTERN",
                "INVALID_FETCH_KIND",
                "INVALID_HANDLE",
                "INVALID_HANDLE_RANGE",
                "INVALID_JSON",
                "INVALID_LSP_EDIT",
                "INVALID_LSP_POSITION",
                "INVALID_LSP_RESPONSE",
                "INVALID_LSP_URI",
                "INVALID_OPERATION_FIELD",
                "INVALID_OUTLINE_PATH",
                "INVALID_POLICY",
                "INVALID_REGEX",
                "INVALID_TIMEOUT",
                "LSP_BUSY",
                "LSP_ERROR",
                "LSP_PROTOCOL_ERROR",
                "LSP_START_FAILED",
                "LSP_SYNC_UNSUPPORTED",
                "LSP_TIMEOUT",
                "LSP_TRANSPORT_ERROR",
                "LSP_UNSUPPORTED",
                "LSP_WORKER_START_FAILED",
                "LSP_WORKER_STOPPED",
                "MISSING_CHANGE_FIELD",
                "MISSING_HANDLE",
                "MISSING_OPERATION_FIELD",
                "MISSING_PRECONDITION",
                "NOT_GIT_REPOSITORY",
                "NOT_INITIALIZED",
                "OUTSIDE_ROOT",
                "OVERLAPPING_EDITS",
                "OVERLAPPING_LSP_EDITS",
                "PARTIAL_COMMIT",
                "PATH_EXISTS",
                "PATH_NOT_FOUND",
                "PATH_NOT_INDEXED",
                "PERMISSION_DENIED",
                "READ_FAILED",
                "RUN_BUSY",
                "RUN_NOT_FOUND",
                "SEMANTIC_BACKEND_UNAVAILABLE",
                "STALE_CONTINUATION",
                "STALE_FILE",
                "STALE_HANDLE",
                "STALE_SNAPSHOT",
                "SYMBOL_NOT_FOUND",
                "SYNTAX_ERROR",
                "TOO_MANY_RETRIEVAL_OPERATIONS",
                "UNKNOWN_CHANGE_FIELD",
                "UNKNOWN_OPERATION_FIELD",
                "UNKNOWN_TOOL_FIELD",
                "UNSUPPORTED_CHANGE_KIND",
                "UNSUPPORTED_LSP_URI",
                "UNSUPPORTED_READ_TARGET",
                "UNSUPPORTED_RETRIEVAL_OPERATION",
                "UNSUPPORTED_WORKSPACE_EDIT",
                "WORKSPACE_NOT_DIRECTORY",
                "WORKSPACE_NOT_FOUND",
            ]
            .map(Value::from)
        );
    }

    #[test]
    fn an_error_serializes_as_code_message_and_details() {
        let error = AppError::with_details(
            ErrorCode::PathNotIndexed,
            "File is not indexed",
            json!({"path": "src/lib.rs"}),
        );
        assert_eq!(
            serde_json::to_value(&error).unwrap(),
            json!({
                "code": "PATH_NOT_INDEXED",
                "message": "File is not indexed",
                "details": {"path": "src/lib.rs"}
            })
        );
        assert_eq!(
            serde_json::to_value(AppError::new(ErrorCode::StaleFile, "changed")).unwrap(),
            json!({"code": "STALE_FILE", "message": "changed"})
        );
        assert_eq!(error.to_string(), "[PATH_NOT_INDEXED] File is not indexed");
    }

    #[test]
    fn request_validation_errors_carry_their_retry_hint() {
        let error = AppError::with_details(
            ErrorCode::UnknownChangeField,
            "change received unsupported fields",
            json!({"unknown": ["extra"]}),
        );
        assert_eq!(
            error.details(),
            Some(&json!({
                "unknown": ["extra"],
                "retryable": false,
                "retry_kind": "remove_field",
                "category": "invalid_request"
            }))
        );
        assert_eq!(
            AppError::new(ErrorCode::UnsupportedChangeKind, "unsupported").details(),
            Some(&json!({
                "retryable": false,
                "retry_kind": "choose_supported_change",
                "category": "unsupported_request"
            }))
        );
        assert_eq!(
            AppError::with_details(ErrorCode::InvalidArgument, "bad", json!({"field": "x"}))
                .details(),
            Some(&json!({"field": "x"}))
        );
    }

    #[test]
    fn io_errors_map_to_the_codes_clients_act_on() {
        let codes = [
            io::ErrorKind::NotFound,
            io::ErrorKind::AlreadyExists,
            io::ErrorKind::PermissionDenied,
            io::ErrorKind::Interrupted,
        ]
        .map(|kind| AppError::from(io::Error::new(kind, "boom")).code().as_str());
        assert_eq!(
            codes,
            [
                "PATH_NOT_FOUND",
                "PATH_EXISTS",
                "PERMISSION_DENIED",
                "INTERNAL_ERROR"
            ]
        );
    }
}
