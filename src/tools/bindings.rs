//! Binds each tool's request type to the `Application` operation that serves
//! it.

use super::{ToolFuture, ToolRequest};
use crate::intelligence::IntelligenceRequest;
use crate::manager::Application;
use crate::requests::bash::{
    BashCancelRequest, BashOutputRequest, BashStartRequest, BashStatusRequest,
};
use crate::requests::edit::{
    CodeDeleteRequest, CodeInsertRequest, CodePreviewRequest, CodeRenameRequest,
    CodeReplaceRangeRequest, CodeReplaceRequest, CodeTransactionRequest, CodeWriteRequest,
};
use crate::requests::git::{
    GitAction, GitBlameRequest, GitCommitRequest, GitDiffRequest, GitLogRequest,
    GitPreflightRequest, GitPushRequest, GitRestoreRequest, GitShowRequest, GitStageRequest,
    GitStatusRequest,
};
use crate::requests::intelligence::CodeIntelligenceRequest;
use crate::requests::retrieval::CodeRetrieveRequest;
use crate::requests::workspace::WorkspaceRequest;
use crate::requests::Rejection;
use std::sync::Arc;

impl ToolRequest for WorkspaceRequest {
    fn handle(self, app: Arc<Application>) -> ToolFuture {
        Box::pin(async move { app.workspace(self).await })
    }
}

impl ToolRequest for CodeRetrieveRequest {
    fn handle(self, app: Arc<Application>) -> ToolFuture {
        Box::pin(async move { app.code_retrieve(self).await })
    }
}

impl ToolRequest for CodeIntelligenceRequest {
    fn handle(self, app: Arc<Application>) -> ToolFuture {
        Box::pin(async move {
            app.code_intelligence(IntelligenceRequest::try_from(self)?)
                .await
        })
    }
}

/// The narrow edit tools report their fields with the change error codes,
/// because each one is a single change.
macro_rules! narrow_edit_tool {
    ($($request:ty),* $(,)?) => {$(
        impl ToolRequest for $request {
            const REJECTION: Rejection = Rejection::Change;

            fn handle(self, app: Arc<Application>) -> ToolFuture {
                Box::pin(async move { app.code_edit(self.into_edit()?).await })
            }
        }
    )*};
}

narrow_edit_tool!(
    CodeWriteRequest,
    CodeReplaceRequest,
    CodeReplaceRangeRequest,
    CodeInsertRequest,
    CodeDeleteRequest,
    CodeRenameRequest,
);

macro_rules! batch_edit_tool {
    ($($request:ty),* $(,)?) => {$(
        impl ToolRequest for $request {
            fn handle(self, app: Arc<Application>) -> ToolFuture {
                Box::pin(async move { app.code_edit(self.into_edit()?).await })
            }
        }
    )*};
}

batch_edit_tool!(CodePreviewRequest, CodeTransactionRequest);

macro_rules! git_tool {
    ($($request:ty => $action:expr),* $(,)?) => {$(
        impl ToolRequest for $request {
            #[allow(clippy::redundant_closure_call)]
            fn handle(self, app: Arc<Application>) -> ToolFuture {
                let action: GitAction = ($action)(self);
                Box::pin(async move { app.git(action).await })
            }
        }
    )*};
}

git_tool!(
    GitStatusRequest => |_| GitAction::Status,
    GitDiffRequest => GitAction::Diff,
    GitLogRequest => GitAction::Log,
    GitShowRequest => GitAction::Show,
    GitBlameRequest => GitAction::Blame,
    GitPreflightRequest => |_| GitAction::Preflight,
    GitStageRequest => GitAction::Stage,
    GitCommitRequest => GitAction::Commit,
    GitRestoreRequest => GitAction::Restore,
    GitPushRequest => GitAction::Push,
);

macro_rules! bash_tool {
    ($($request:ty),* $(,)?) => {$(
        impl ToolRequest for $request {
            const REJECTION: Rejection = Rejection::Bash;

            fn handle(self, app: Arc<Application>) -> ToolFuture {
                Box::pin(async move { app.bash(self.into_request()?).await })
            }
        }
    )*};
}

bash_tool!(
    BashStartRequest,
    BashStatusRequest,
    BashOutputRequest,
    BashCancelRequest,
);
