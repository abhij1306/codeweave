//! Requests for the four `bash*` tools and the `BashRequest` they select.

use super::contract::Rejection;
use crate::bash::OutputStream;
use crate::bash::StartRequest;
use crate::error::AppResult;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

// Plain comments throughout: schemars publishes doc comments as descriptions.

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BashStartRequest {
    #[schemars(
        length(min = 1),
        description = "Command string passed to the configured Bash executable with -c."
    )]
    pub command: String,
    #[schemars(
        description = "Existing workspace-relative directory. Defaults to the workspace root."
    )]
    pub cwd: Option<String>,
    #[serde(default)]
    #[schemars(description = "Run detached and return immediately with a run_id to poll.")]
    pub background: bool,
    #[schemars(
        range(min = 1),
        description = "Per-run timeout in ms (<= maxTimeoutMs). The command is killed if it exceeds this; the foreground budget only detaches, it does not kill."
    )]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BashStatusRequest {
    #[schemars(length(min = 1))]
    pub run_id: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BashOutputRequest {
    #[schemars(length(min = 1))]
    pub run_id: String,
    pub stream: Option<OutputStream>,
    pub continuation: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BashCancelRequest {
    #[schemars(length(min = 1))]
    pub run_id: String,
}

/// One Bash run operation.
#[derive(Debug, Clone)]
pub enum BashRequest {
    Start(StartRequest),
    Status {
        run_id: String,
    },
    Output {
        run_id: String,
        stream: OutputStream,
        continuation: Option<String>,
    },
    Cancel {
        run_id: String,
    },
}

impl BashRequest {
    /// Polls observe a run without starting work.
    pub fn is_poll(&self) -> bool {
        !matches!(self, Self::Start(_))
    }
}

/// A whitespace-only command or run id is as empty as a missing one.
fn non_blank(method: &str, field: &str, value: String) -> AppResult<String> {
    if value.trim().is_empty() {
        Err(Rejection::Bash.invalid(
            format!("{method} field '{field}' must be a non-empty string"),
            json!({"field": field}),
        ))
    } else {
        Ok(value)
    }
}

impl BashStartRequest {
    pub fn into_request(self) -> AppResult<BashRequest> {
        Ok(BashRequest::Start(StartRequest {
            command: non_blank("bash", "command", self.command)?,
            cwd: self.cwd,
            background: Some(self.background),
            timeout_ms: self.timeout_ms,
        }))
    }
}

impl BashStatusRequest {
    pub fn into_request(self) -> AppResult<BashRequest> {
        Ok(BashRequest::Status {
            run_id: non_blank("bash_status", "run_id", self.run_id)?,
        })
    }
}

impl BashOutputRequest {
    pub fn into_request(self) -> AppResult<BashRequest> {
        Ok(BashRequest::Output {
            run_id: non_blank("bash_output", "run_id", self.run_id)?,
            stream: self.stream.unwrap_or(OutputStream::Combined),
            continuation: self.continuation,
        })
    }
}

impl BashCancelRequest {
    pub fn into_request(self) -> AppResult<BashRequest> {
        Ok(BashRequest::Cancel {
            run_id: non_blank("bash_cancel", "run_id", self.run_id)?,
        })
    }
}
