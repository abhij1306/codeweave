//! The intelligence service's only window onto the reconciled workspace state.
//! Every index and snapshot read goes through this type.
use crate::reference_service::ReferenceService;
use crate::workspace::StateReader;

/// Indexed content and hash for one file at the moment it was read.
#[derive(Debug, Clone)]
pub(crate) struct IndexedFile {
    pub(crate) content: String,
    pub(crate) hash: String,
}

#[derive(Clone)]
pub(crate) struct LiveIndex {
    state: StateReader,
}

impl LiveIndex {
    pub(crate) fn new(state: StateReader) -> Self {
        Self { state }
    }

    pub(crate) fn file(&self, relative: &str) -> Option<IndexedFile> {
        self.state
            .read()
            .index()
            .get(relative)
            .map(|entry| IndexedFile {
                content: entry.content().to_owned(),
                hash: entry.hash().to_owned(),
            })
    }

    pub(crate) fn hash(&self, relative: &str) -> Option<String> {
        self.state
            .read()
            .index()
            .get(relative)
            .map(|entry| entry.hash().to_owned())
    }

    pub(crate) fn snapshot_id(&self) -> String {
        self.state.read().snapshot_id().to_owned()
    }

    /// Run `query` against the shared reference service under one index read.
    pub(crate) fn references<T>(&self, query: impl FnOnce(&ReferenceService<'_>) -> T) -> T {
        let view = self.state.read();
        query(&ReferenceService::new(view.index()))
    }
}
