//! The semantic seam: the service asks a `SemanticBackend` to answer an
//! operation against a document snapshot. The production adapter is the LSP
//! worker; tests substitute workers over scripted transports.
use super::preset::LspPreset;
use super::sync::DocumentSnapshot;
use super::worker::{LspWorker, WorkerError, WorkerOperation, WorkerResponse};
use crate::model::{AppResult, IntelligenceSettings};
use serde_json::{Map, Value};
use std::path::Path;
use std::sync::Arc;

pub(crate) trait SemanticBackend: Send + Sync {
    fn preset(&self) -> LspPreset;
    fn configured(&self) -> bool;
    /// Synchronize `document` and answer `operation` against exactly that text.
    fn execute(
        &self,
        operation: WorkerOperation,
        document: DocumentSnapshot,
    ) -> Result<WorkerResponse, WorkerError>;
    fn status(&self) -> Value;
}

/// One backend per preset, in preset order.
pub(crate) struct Backends {
    backends: Vec<Arc<dyn SemanticBackend>>,
}

pub(crate) struct BackendCapabilities {
    pub(crate) languages: Vec<Value>,
    pub(crate) restart_state: Map<String, Value>,
    pub(crate) semantic_available: bool,
}

impl Backends {
    pub(crate) fn spawn(root: &Path, settings: &IntelligenceSettings) -> AppResult<Self> {
        let backends = LspPreset::ALL
            .into_iter()
            .map(|preset| {
                let settings = match preset {
                    LspPreset::Rust => &settings.rust,
                    LspPreset::Python => &settings.python,
                    LspPreset::TypeScript => &settings.typescript,
                };
                LspWorker::spawn(root, preset, settings)
                    .map(|worker| Arc::new(worker) as Arc<dyn SemanticBackend>)
            })
            .collect::<AppResult<Vec<_>>>()?;
        Ok(Self { backends })
    }

    pub(crate) fn new(backends: Vec<Arc<dyn SemanticBackend>>) -> Self {
        Self { backends }
    }

    /// The configured backend serving `relative`, if any.
    pub(crate) fn for_path(&self, relative: &str) -> Option<&dyn SemanticBackend> {
        let preset = LspPreset::for_path(Path::new(relative))?;
        self.backends
            .iter()
            .find(|backend| backend.preset() == preset && backend.configured())
            .map(Arc::as_ref)
    }

    pub(crate) fn capabilities(&self) -> BackendCapabilities {
        let mut restart_state = Map::new();
        let mut languages = Vec::with_capacity(self.backends.len());
        for backend in &self.backends {
            let status = backend.status();
            restart_state.insert(backend.preset().key().to_owned(), status.clone());
            languages.push(status);
        }
        BackendCapabilities {
            languages,
            restart_state,
            semantic_available: self.backends.iter().any(|backend| backend.configured()),
        }
    }
}
