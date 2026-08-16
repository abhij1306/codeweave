use super::normalize::path_uri;
use super::protocol::TextDocumentSyncKind;
use crate::index::content_hash;
use crate::model::{AppError, AppResult};
use crate::security::read_workspace_file;
use serde_json::{json, Value};
use std::collections::HashMap;
#[cfg(test)]
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub(crate) struct DocumentSnapshot {
    pub(crate) root: PathBuf,
    pub(crate) relative: String,
    pub(crate) path: PathBuf,
    pub(crate) uri: String,
    pub(crate) language_id: &'static str,
    pub(crate) content: String,
    pub(crate) hash: String,
}

impl DocumentSnapshot {
    pub(crate) fn from_content(
        root: &Path,
        relative: &str,
        language_id: &'static str,
        content: String,
    ) -> AppResult<Self> {
        let relative = crate::security::validate_relative(relative)?
            .to_string_lossy()
            .replace('\\', "/");
        let path = root.join(&relative);
        Ok(Self {
            root: root.to_owned(),
            relative,
            path: path.clone(),
            uri: path_uri(&path),
            language_id,
            hash: content_hash(&content),
            content,
        })
    }

    #[cfg(test)]
    pub(crate) fn read(path: &Path, language_id: &'static str) -> AppResult<Self> {
        let root = path.parent().unwrap_or_else(|| Path::new("."));
        let relative = path
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| AppError::invalid("test document path has no file name"))?;
        Self::from_content(root, relative, language_id, fs::read_to_string(path)?)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SynchronizedDocument {
    pub(crate) path: PathBuf,
    pub(crate) hash: String,
    pub(crate) version: i64,
}

#[derive(Debug, Clone)]
pub(crate) struct DocumentState {
    hash: String,
    version: i64,
}

pub(crate) struct SyncPlan {
    pub(crate) notification: Option<Value>,
    pub(crate) synchronized: SynchronizedDocument,
    pub(crate) changed: bool,
}

pub(crate) fn plan_sync(
    documents: &mut HashMap<PathBuf, DocumentState>,
    snapshot: &DocumentSnapshot,
    sync_kind: TextDocumentSyncKind,
) -> AppResult<SyncPlan> {
    if sync_kind == TextDocumentSyncKind::None {
        return Err(AppError::new(
            "LSP_SYNC_UNSUPPORTED",
            "Language server does not advertise document synchronization",
        ));
    }

    let (notification, version, changed) = match documents.get(&snapshot.path) {
        None => (
            Some(json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didOpen",
                "params": {
                    "textDocument": {
                        "uri": snapshot.uri,
                        "languageId": snapshot.language_id,
                        "version": 1,
                        "text": snapshot.content
                    }
                }
            })),
            1,
            true,
        ),
        Some(state) if state.hash != snapshot.hash => {
            let version = state.version + 1;
            (
                Some(json!({
                    "jsonrpc": "2.0",
                    "method": "textDocument/didChange",
                    "params": {
                        "textDocument": {"uri": snapshot.uri, "version": version},
                        "contentChanges": [{"text": snapshot.content}]
                    }
                })),
                version,
                true,
            )
        }
        Some(state) => (None, state.version, false),
    };

    documents.insert(
        snapshot.path.clone(),
        DocumentState {
            hash: snapshot.hash.clone(),
            version,
        },
    );
    Ok(SyncPlan {
        notification,
        synchronized: SynchronizedDocument {
            path: snapshot.path.clone(),
            hash: snapshot.hash.clone(),
            version,
        },
        changed,
    })
}

pub(crate) fn verify_current(snapshot: &DocumentSnapshot) -> AppResult<()> {
    let max_bytes = snapshot.content.len().saturating_add(1);
    let current = read_workspace_file(&snapshot.root, &snapshot.relative, max_bytes)?
        .and_then(|file| String::from_utf8(file.bytes).ok())
        .map(|content| content_hash(&content));
    if current.as_deref() == Some(snapshot.hash.as_str()) {
        Ok(())
    } else {
        Err(AppError::details(
            "LSP_STALE_DOCUMENT",
            "Document changed while the semantic request was running",
            json!({
                "path": snapshot.path,
                "synchronized_hash": snapshot.hash,
                "current_hash": current
            }),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_document_hash_is_rejected_after_request() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("main.rs");
        fs::write(&path, "fn before() {}\n").unwrap();
        let synchronized = DocumentSnapshot::from_content(
            root.path(),
            "main.rs",
            "rust",
            fs::read_to_string(&path).unwrap(),
        )
        .unwrap();
        fs::write(&path, "fn after() {}\n").unwrap();
        let error = verify_current(&synchronized).unwrap_err();
        assert_eq!(error.0.code, "LSP_STALE_DOCUMENT");
        assert_ne!(
            error.0.details.as_ref().unwrap()["synchronized_hash"],
            error.0.details.as_ref().unwrap()["current_hash"]
        );
    }
}
