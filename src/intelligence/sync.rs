//! Full-text, hash-versioned document synchronization with a language server.
use crate::index::{content_hash, ContentHash};
use serde_json::{json, Value};
use std::collections::HashMap;

/// The exact text a semantic request is answered against.
#[derive(Debug, Clone)]
pub(crate) struct DocumentSnapshot {
    pub(crate) uri: String,
    pub(crate) language_id: &'static str,
    pub(crate) content: String,
    pub(crate) hash: ContentHash,
}

impl DocumentSnapshot {
    pub(crate) fn new(uri: String, language_id: &'static str, content: String) -> Self {
        Self {
            uri,
            language_id,
            hash: content_hash(&content),
            content,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SynchronizedDocument {
    pub(crate) hash: ContentHash,
    pub(crate) version: i64,
}

#[derive(Debug, Clone)]
pub(crate) struct DocumentState {
    hash: ContentHash,
    version: i64,
}

pub(crate) struct SyncPlan {
    pub(crate) notification: Option<Value>,
    pub(crate) synchronized: SynchronizedDocument,
    pub(crate) changed: bool,
}

/// Record `snapshot` as the server's view of its URI and return the
/// notification (if any) that makes the server agree.
pub(crate) fn plan_sync(
    documents: &mut HashMap<String, DocumentState>,
    snapshot: &DocumentSnapshot,
) -> SyncPlan {
    let (notification, version, changed) = match documents.get(&snapshot.uri) {
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
        snapshot.uri.clone(),
        DocumentState {
            hash: snapshot.hash.clone(),
            version,
        },
    );
    SyncPlan {
        notification,
        synchronized: SynchronizedDocument {
            hash: snapshot.hash.clone(),
            version,
        },
        changed,
    }
}
