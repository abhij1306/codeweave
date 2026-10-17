//! The intelligence service's only window onto the reconciled workspace state.
//! Every index and snapshot read goes through this type.
use crate::index::CodeIndex;
use crate::reference_service::ReferenceService;
use parking_lot::RwLock;
use std::sync::Arc;

/// Indexed content and hash for one file at the moment it was read.
#[derive(Debug, Clone)]
pub(crate) struct IndexedFile {
    pub(crate) content: String,
    pub(crate) hash: String,
}

#[derive(Clone)]
pub(crate) struct LiveIndex {
    index: Arc<RwLock<CodeIndex>>,
    snapshot_id: Arc<RwLock<String>>,
}

impl LiveIndex {
    pub(crate) fn new(index: Arc<RwLock<CodeIndex>>, snapshot_id: Arc<RwLock<String>>) -> Self {
        Self { index, snapshot_id }
    }

    pub(crate) fn file(&self, relative: &str) -> Option<IndexedFile> {
        self.index.read().get(relative).map(|entry| IndexedFile {
            content: entry.content().to_owned(),
            hash: entry.hash().to_owned(),
        })
    }

    pub(crate) fn hash(&self, relative: &str) -> Option<String> {
        self.index
            .read()
            .get(relative)
            .map(|entry| entry.hash().to_owned())
    }

    pub(crate) fn snapshot_id(&self) -> String {
        self.snapshot_id.read().clone()
    }

    /// Run `query` against the shared reference service under one index read.
    pub(crate) fn references<T>(&self, query: impl FnOnce(&ReferenceService<'_>) -> T) -> T {
        let index = self.index.read();
        query(&ReferenceService::new(&index))
    }
}
