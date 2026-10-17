//! The tool registry: the one place a public tool is defined.
//!
//! Each entry names a tool, its title, description and safety class, and its
//! request type. The request type (see `crate::requests`) generates the
//! published input schema, defines the field contract checked at the boundary,
//! and through `ToolRequest::handle` binds the tool to its `Application`
//! method. `tools/list` and `tools/call` both read this registry, so adding a
//! tool means adding one entry here plus its request type.

mod bindings;
mod call;

pub use call::{call_tool, list_tools};

use crate::error::AppResult;
use crate::manager::Application;
use crate::requests::bash::{
    BashCancelRequest, BashOutputRequest, BashStartRequest, BashStatusRequest,
};
use crate::requests::edit::{
    CodeDeleteRequest, CodeInsertRequest, CodePreviewRequest, CodeRenameRequest,
    CodeReplaceRangeRequest, CodeReplaceRequest, CodeTransactionRequest, CodeWriteRequest,
};
use crate::requests::git::{
    GitBlameRequest, GitCommitRequest, GitDiffRequest, GitLogRequest, GitPreflightRequest,
    GitPushRequest, GitRestoreRequest, GitShowRequest, GitStageRequest, GitStatusRequest,
};
use crate::requests::intelligence::CodeIntelligenceRequest;
use crate::requests::retrieval::CodeRetrieveRequest;
use crate::requests::workspace::WorkspaceRequest;
use crate::requests::{input_schema, Contract, Rejection};
use rmcp::model::{Tool, ToolAnnotations};
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};

/// The future a tool handler returns.
pub type ToolFuture = Pin<Box<dyn Future<Output = AppResult<Value>> + Send>>;

/// A tool's typed request, bound to the operation that serves it.
pub trait ToolRequest: DeserializeOwned + JsonSchema + Send + 'static {
    /// How the request reports a rejected field.
    const REJECTION: Rejection = Rejection::Argument;

    fn handle(self, app: Arc<Application>) -> ToolFuture;
}

/// Hosted-client safety classification. Each tool maps to exactly one level; the
/// level determines the advertised MCP annotation hints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolSafety {
    /// No side effects.
    Read,
    /// Mutates local state only.
    WriteClosed,
    /// May discard local state.
    DestructiveClosed,
    /// Writes and reaches the network.
    WriteOpen,
    /// May run arbitrary commands as the CodeWeave OS user.
    DestructiveOpen,
}

impl ToolSafety {
    fn annotations(self) -> ToolAnnotations {
        let (read_only, destructive, idempotent, open_world) = match self {
            ToolSafety::Read => (true, false, true, false),
            ToolSafety::WriteClosed => (false, false, false, false),
            ToolSafety::DestructiveClosed => (false, true, false, false),
            ToolSafety::WriteOpen => (false, false, false, true),
            ToolSafety::DestructiveOpen => (false, true, false, true),
        };
        ToolAnnotations::new()
            .read_only(read_only)
            .destructive(destructive)
            .idempotent(idempotent)
            .open_world(open_world)
    }
}

/// One advertised tool.
pub struct ToolDefinition {
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub safety: ToolSafety,
    schema: fn() -> Map<String, Value>,
    call: fn(&'static ToolDefinition, Arc<Application>, Value) -> ToolFuture,
    contract: OnceLock<Contract>,
}

impl ToolDefinition {
    const fn of<R: ToolRequest>(
        name: &'static str,
        title: &'static str,
        description: &'static str,
        safety: ToolSafety,
    ) -> Self {
        Self {
            name,
            title,
            description,
            safety,
            schema: input_schema::<R>,
            call: call::<R>,
            contract: OnceLock::new(),
        }
    }

    /// The published input schema.
    pub fn input_schema(&self) -> Map<String, Value> {
        (self.schema)()
    }

    /// The field contract read from the input schema, built once.
    pub fn contract(&self) -> &Contract {
        self.contract
            .get_or_init(|| Contract::from_schema(self.name, &self.input_schema()))
    }

    /// Parse `arguments` into the tool's request and run its handler.
    pub fn call(&'static self, app: Arc<Application>, arguments: Value) -> ToolFuture {
        (self.call)(self, app, arguments)
    }

    fn to_mcp(&self) -> Tool {
        Tool::new(self.name, self.description, Arc::new(self.input_schema()))
            .with_title(self.title)
            .with_annotations(self.safety.annotations())
    }
}

fn call<R: ToolRequest>(
    tool: &'static ToolDefinition,
    app: Arc<Application>,
    arguments: Value,
) -> ToolFuture {
    match tool.contract().parse::<R>(arguments, R::REJECTION) {
        Ok(request) => request.handle(app),
        Err(error) => Box::pin(std::future::ready(Err(error))),
    }
}

/// The complete, ordered tool set. Order is preserved in `tools/list`.
pub fn registry() -> &'static [ToolDefinition] {
    &TOOLS
}

/// The tool called `name`.
pub fn find(name: &str) -> Option<&'static ToolDefinition> {
    static BY_NAME: OnceLock<HashMap<&'static str, &'static ToolDefinition>> = OnceLock::new();
    BY_NAME
        .get_or_init(|| TOOLS.iter().map(|tool| (tool.name, tool)).collect())
        .get(name)
        .copied()
}

/// The `tools/list` entries in registry order, built once.
pub fn mcp_tools() -> &'static [Tool] {
    static TOOL_LIST: OnceLock<Vec<Tool>> = OnceLock::new();
    TOOL_LIST.get_or_init(|| TOOLS.iter().map(ToolDefinition::to_mcp).collect())
}

static TOOLS: [ToolDefinition; 25] = [
    ToolDefinition::of::<WorkspaceRequest>(
        "workspace",
        "Workspace",
        "View this server's repository summary or process-local changes, refresh its index, or inspect diagnostics. The repository is fixed for the server's lifetime.",
        ToolSafety::Read,
    ),
    ToolDefinition::of::<CodeRetrieveRequest>(
        "code_retrieve",
        "Code Retrieval",
        "Use this single tool for all repository discovery and exact reads. Submit one or more explicit operations: find_file, find_symbol, search_text, find_references, symbols_overview, repo_map, or read.",
        ToolSafety::Read,
    ),
    ToolDefinition::of::<CodeIntelligenceRequest>(
        "code_intelligence",
        "Code Intelligence",
        "Resolve definitions, references, diagnostics, or a rename preview through the optional semantic backend. Results always label semantic, syntactic, or lexical evidence.",
        ToolSafety::Read,
    ),
    ToolDefinition::of::<CodeWriteRequest>(
        "code_write",
        "Write One File",
        "Create or overwrite exactly one file. Use expected_hash when replacing an existing file.",
        ToolSafety::WriteClosed,
    ),
    ToolDefinition::of::<CodeReplaceRequest>(
        "code_replace",
        "Replace Text in One File",
        "Replace exact text in exactly one file. The replacement count is checked before writing.",
        ToolSafety::WriteClosed,
    ),
    ToolDefinition::of::<CodeReplaceRangeRequest>(
        "code_replace_range",
        "Replace Fetched Range in One File",
        "Replace the complete line range selected by a code_retrieve read handle in exactly one file.",
        ToolSafety::WriteClosed,
    ),
    ToolDefinition::of::<CodeInsertRequest>(
        "code_insert",
        "Insert Text in One File",
        "Insert text before, after, or inside one named symbol in exactly one file.",
        ToolSafety::WriteClosed,
    ),
    ToolDefinition::of::<CodeDeleteRequest>(
        "code_delete",
        "Delete One File",
        "Delete exactly one file with an optional content-hash precondition.",
        ToolSafety::DestructiveClosed,
    ),
    ToolDefinition::of::<CodeRenameRequest>(
        "code_rename",
        "Rename One File",
        "Rename exactly one file with an optional content-hash precondition.",
        ToolSafety::WriteClosed,
    ),
    ToolDefinition::of::<CodePreviewRequest>(
        "code_preview",
        "Preview Code Transaction",
        "Preview a multi-file edit transaction and return the diff without writing files.",
        ToolSafety::Read,
    ),
    ToolDefinition::of::<CodeTransactionRequest>(
        "code_transaction",
        "Apply Code Transaction",
        "Apply a multi-file edit transaction through the same precondition, non-destructive validation reporting, diff, and internal atomic-recovery engine as the narrow write tools.",
        ToolSafety::WriteClosed,
    ),
    ToolDefinition::of::<GitStatusRequest>(
        "git_status",
        "Git Status",
        "Show the working tree status: staged, unstaged, untracked, and partially staged files.",
        ToolSafety::Read,
    ),
    ToolDefinition::of::<GitDiffRequest>(
        "git_diff",
        "Git Diff",
        "Show the diff for the working tree or, with staged=true, the staged index. Limit to specific paths when given.",
        ToolSafety::Read,
    ),
    ToolDefinition::of::<GitLogRequest>(
        "git_log",
        "Git Log",
        "Show recent commit history.",
        ToolSafety::Read,
    ),
    ToolDefinition::of::<GitShowRequest>(
        "git_show",
        "Git Show",
        "Show the patch for one commit ref (default HEAD).",
        ToolSafety::Read,
    ),
    ToolDefinition::of::<GitBlameRequest>(
        "git_blame",
        "Git Blame",
        "Show line-by-line authorship for one path, optionally bounded by start_line and end_line.",
        ToolSafety::Read,
    ),
    ToolDefinition::of::<GitPreflightRequest>(
        "git_preflight",
        "Git Preflight",
        "Return staged_files, partially_staged_files, and the cached staged diff without committing.",
        ToolSafety::Read,
    ),
    ToolDefinition::of::<GitStageRequest>(
        "git_stage",
        "Git Stage",
        "Stage the given paths into the index.",
        ToolSafety::WriteClosed,
    ),
    ToolDefinition::of::<GitCommitRequest>(
        "git_commit",
        "Git Commit",
        "Commit the currently staged changes with the given message.",
        ToolSafety::WriteClosed,
    ),
    ToolDefinition::of::<GitRestoreRequest>(
        "git_restore",
        "Git Restore",
        "Discard changes for the given paths, restoring them from the index or HEAD. Requires confirm=true because it overwrites local changes.",
        ToolSafety::DestructiveClosed,
    ),
    ToolDefinition::of::<GitPushRequest>(
        "git_push",
        "Git Push",
        "Push commits to a remote (default origin) and optional branch. This reaches the network and requires confirm=true, matching git_restore, because it is the only network-facing write.",
        ToolSafety::WriteOpen,
    ),
    ToolDefinition::of::<BashStartRequest>(
        "bash",
        "Run Bash Command",
        "Run one Bash process as the CodeWeave OS user. This is trusted-client functionality, not a sandbox. Commands exceeding the foreground budget continue as the same background run; poll bash_status with its run_id instead of reissuing the command. Output is bounded and timeout terminates the process tree.",
        ToolSafety::DestructiveOpen,
    ),
    ToolDefinition::of::<BashStatusRequest>(
        "bash_status",
        "Bash Run Status",
        "Return live or completed state and the retained output tail for a Bash run, including how many prefix characters were discarded.",
        ToolSafety::Read,
    ),
    ToolDefinition::of::<BashOutputRequest>(
        "bash_output",
        "Bash Run Output",
        "Page retained combined, stdout, or stderr output for a Bash run. Continuations cover only the bounded tail; retention metadata reports any discarded prefix.",
        ToolSafety::Read,
    ),
    ToolDefinition::of::<BashCancelRequest>(
        "bash_cancel",
        "Cancel Bash Run",
        "Cancel a running background Bash run. Partial output is retained.",
        ToolSafety::WriteClosed,
    ),
];

#[cfg(test)]
mod tests;
