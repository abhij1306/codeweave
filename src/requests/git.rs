//! Requests for the ten `git_*` tools and the `GitAction` they select.

use schemars::JsonSchema;
use serde::Deserialize;

// Plain comments throughout: schemars publishes doc comments as descriptions.

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(extend("properties" = {}))]
pub struct GitStatusRequest {}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GitDiffRequest {
    pub paths: Option<Vec<String>>,
    #[schemars(extend("default" = false))]
    pub staged: Option<bool>,
    #[schemars(range(min = 1, max = 200_000))]
    pub max_chars: Option<usize>,
    #[schemars(range(min = 1))]
    pub start_line: Option<usize>,
    #[schemars(range(min = 1))]
    pub end_line: Option<usize>,
    pub symbol: Option<String>,
    pub hunk_ids: Option<Vec<String>>,
    #[schemars(
        description = "Snapshot- and scope-bound continuation returned by a truncated git_diff response. Reuses the original paths, staged mode, symbol/line focus, and hunk IDs."
    )]
    pub continuation: Option<String>,
}

impl GitDiffRequest {
    /// Whether the caller sent any scope field (as opposed to only a
    /// continuation and size limit).
    pub fn has_scope(&self) -> bool {
        self.paths.is_some()
            || self.staged.is_some()
            || self.symbol.is_some()
            || self.start_line.is_some()
            || self.end_line.is_some()
            || self.hunk_ids.is_some()
    }
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GitLogRequest {
    #[serde(default = "default_log_limit")]
    #[schemars(range(min = 1, max = 200))]
    pub limit: usize,
}

fn default_log_limit() -> usize {
    20
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GitShowRequest {
    #[serde(rename = "ref", default = "default_ref")]
    pub reference: String,
    #[schemars(range(min = 1, max = 200_000))]
    pub max_chars: Option<usize>,
}

fn default_ref() -> String {
    "HEAD".to_owned()
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GitBlameRequest {
    pub paths: Vec<String>,
    #[schemars(range(min = 1))]
    pub start_line: Option<usize>,
    #[schemars(range(min = 1))]
    pub end_line: Option<usize>,
    #[schemars(range(min = 1, max = 200_000))]
    pub max_chars: Option<usize>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(extend("properties" = {}))]
pub struct GitPreflightRequest {}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GitStageRequest {
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GitCommitRequest {
    pub message: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GitRestoreRequest {
    pub paths: Vec<String>,
    #[serde(default)]
    pub staged: bool,
    pub confirm: bool,
}

#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GitPushRequest {
    #[schemars(description = "Push remote name (default: origin)")]
    pub remote: Option<String>,
    #[schemars(description = "Branch to push (default: current branch)")]
    pub branch: Option<String>,
    pub confirm: bool,
}

/// One Git operation with its typed arguments.
#[derive(Debug, Clone)]
pub enum GitAction {
    Status,
    Diff(GitDiffRequest),
    Log(GitLogRequest),
    Show(GitShowRequest),
    Blame(GitBlameRequest),
    Preflight,
    Stage(GitStageRequest),
    Commit(GitCommitRequest),
    Restore(GitRestoreRequest),
    Push(GitPushRequest),
}

impl GitAction {
    /// The action name reported in responses.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Diff(_) => "diff",
            Self::Log(_) => "log",
            Self::Show(_) => "show",
            Self::Blame(_) => "blame",
            Self::Preflight => "preflight",
            Self::Stage(_) => "stage",
            Self::Commit(_) => "commit",
            Self::Restore(_) => "restore",
            Self::Push(_) => "push",
        }
    }

    /// Actions that change the worktree or index and so run under the
    /// mutation permit.
    pub fn is_mutating(&self) -> bool {
        matches!(self, Self::Stage(_) | Self::Commit(_) | Self::Restore(_))
    }
}
