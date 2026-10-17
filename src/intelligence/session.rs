//! The fresh-document session and the semantic-or-fallback decision.
//!
//! Freshness invariant (the only one): a semantic result is current only when
//! the text the server answered against has the same hash as the target on
//! disk and in the live index, both before the request and after it, and
//! every other file the result points into matches its live index entry.
//! Any violation is `INTELLIGENCE_STALE`, with `details.phase` naming where:
//!
//! - `before_request`: the target is missing from the index or the index no
//!   longer matches disk. This is a request error; there is no current
//!   content to answer from, semantically or syntactically.
//! - `after_request`: disk or the index changed while the server worked.
//! - `result_file`: a file named in the result no longer matches the index.
//!
//! The last two make the semantic result unusable; operations with a
//! fallback return it with this error as `fallback_reason`.
use super::backend::SemanticBackend;
use super::live_index::LiveIndex;
use super::sync::DocumentSnapshot;
use super::uri::UriCodec;
use super::worker::{WorkerOperation, WorkerResponse};
use crate::index::content_hash;
use crate::model::{AppError, AppResult};
use crate::security::read_workspace_file;
use serde_json::{json, Value};
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::path::Path;

pub(crate) const STALE: &str = "INTELLIGENCE_STALE";
/// Largest file a semantic result may point into.
const MAX_RESULT_FILE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy)]
pub(crate) struct Workspace<'a> {
    pub(crate) root: &'a Path,
    pub(crate) codec: &'a UriCodec,
    pub(crate) live: &'a LiveIndex,
}

/// A semantic answer whose freshness has been verified.
pub(crate) struct Semantic<T> {
    pub(crate) value: T,
    pub(crate) response: WorkerResponse,
}

/// Why the semantic path was not used, when one was attempted.
pub(crate) struct FallbackReason {
    pub(crate) error: AppError,
    pub(crate) backend_status: Value,
}

pub(crate) enum Attempt<T> {
    Semantic(Semantic<T>),
    /// `None` when no configured backend serves the file.
    Fallback(Option<FallbackReason>),
}

/// The target file at a verified-current state.
pub(crate) struct Session<'a> {
    workspace: Workspace<'a>,
    pub(crate) relative: String,
    pub(crate) content: String,
    hash: String,
}

impl<'a> Session<'a> {
    /// Read the target from the live index and confirm disk agrees.
    pub(crate) fn open(workspace: Workspace<'a>, relative: &str) -> AppResult<Self> {
        let indexed = workspace.live.file(relative).ok_or_else(|| {
            stale(
                "before_request",
                "Code-intelligence target is not present in the confined live index",
                json!({"path": relative}),
            )
        })?;
        let disk_hash = read_text(workspace.root, relative, indexed.content.len() + 1)?
            .map(|text| content_hash(&text));
        if disk_hash.as_deref() != Some(indexed.hash.as_str()) {
            return Err(stale(
                "before_request",
                "Code-intelligence target no longer matches the confined live index",
                json!({"path": relative, "indexed_hash": indexed.hash, "disk_hash": disk_hash}),
            ));
        }
        Ok(Self {
            workspace,
            relative: relative.to_owned(),
            content: indexed.content,
            hash: indexed.hash,
        })
    }

    /// Ask `backend` (if any) and interpret its answer, or explain why the
    /// caller must fall back.
    pub(crate) fn attempt<T>(
        &self,
        backend: Option<&dyn SemanticBackend>,
        operation: WorkerOperation,
        interpret: impl FnOnce(&WorkerResponse, &mut ResultFiles<'_>) -> AppResult<T>,
    ) -> Attempt<T> {
        let Some(backend) = backend else {
            return Attempt::Fallback(None);
        };
        match self.semantic(backend, operation, interpret) {
            Ok(semantic) => Attempt::Semantic(semantic),
            Err(error) => Attempt::Fallback(Some(FallbackReason {
                error,
                backend_status: backend.status(),
            })),
        }
    }

    fn semantic<T>(
        &self,
        backend: &dyn SemanticBackend,
        operation: WorkerOperation,
        interpret: impl FnOnce(&WorkerResponse, &mut ResultFiles<'_>) -> AppResult<T>,
    ) -> AppResult<Semantic<T>> {
        let document = DocumentSnapshot::new(
            self.workspace.codec.document_uri(&self.relative),
            backend.preset().language_id(),
            self.content.clone(),
        );
        let response = backend.execute(operation, document)?;
        self.verify_after_request(&response.synchronized.hash)?;
        let mut files = ResultFiles {
            workspace: self.workspace,
            contents: HashMap::new(),
        };
        let value = interpret(&response, &mut files)?;
        Ok(Semantic { value, response })
    }

    fn verify_after_request(&self, synchronized_hash: &str) -> AppResult<()> {
        let disk_hash = read_text(self.workspace.root, &self.relative, self.content.len() + 1)?
            .map(|text| content_hash(&text));
        let indexed_hash = self.workspace.live.hash(&self.relative);
        let current = synchronized_hash == self.hash
            && disk_hash.as_deref() == Some(synchronized_hash)
            && indexed_hash.as_deref() == Some(synchronized_hash);
        if current {
            return Ok(());
        }
        Err(stale(
            "after_request",
            "Target changed while the semantic request was running",
            json!({
                "path": self.relative,
                "synchronized_hash": synchronized_hash,
                "disk_hash": disk_hash,
                "indexed_hash": indexed_hash
            }),
        ))
    }
}

/// Confined, freshness-checked reads of the files a semantic result names.
pub(crate) struct ResultFiles<'a> {
    workspace: Workspace<'a>,
    contents: HashMap<String, String>,
}

impl ResultFiles<'_> {
    /// Workspace-relative path for an LSP URI; never escapes the root.
    pub(crate) fn relative(&self, uri: &str) -> AppResult<String> {
        self.workspace.codec.relative(uri)
    }

    /// Current content of `relative`, read once per result through the
    /// confined reader and checked against the live index when indexed.
    pub(crate) fn content(&mut self, relative: &str) -> AppResult<&str> {
        let text = match self.contents.entry(relative.to_owned()) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => entry.insert(read_current(self.workspace, relative)?),
        };
        Ok(text.as_str())
    }
}

/// Read a result file and require it to match its live index entry, if any.
fn read_current(workspace: Workspace<'_>, relative: &str) -> AppResult<String> {
    let text = read_text(workspace.root, relative, MAX_RESULT_FILE_BYTES)?.ok_or_else(|| {
        AppError::details(
            "INVALID_LSP_RESPONSE",
            "LSP result does not name a readable regular workspace file",
            json!({"path": relative}),
        )
    })?;
    if let Some(indexed_hash) = workspace.live.hash(relative) {
        let disk_hash = content_hash(&text);
        if disk_hash != indexed_hash {
            return Err(stale(
                "result_file",
                "A file named in the semantic result no longer matches the live index",
                json!({"path": relative, "indexed_hash": indexed_hash, "disk_hash": disk_hash}),
            ));
        }
    }
    Ok(text)
}

fn read_text(root: &Path, relative: &str, max_bytes: usize) -> AppResult<Option<String>> {
    Ok(read_workspace_file(root, relative, max_bytes)?
        .and_then(|file| String::from_utf8(file.bytes).ok()))
}

fn stale(phase: &str, message: &str, mut details: Value) -> AppError {
    details["phase"] = json!(phase);
    AppError::details(STALE, message, details)
}
