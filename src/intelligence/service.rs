//! `code_intelligence`: one typed request, answered semantically when a
//! backend can produce a current result, otherwise by the fallback adapter.
use super::backend::{BackendCapabilities, Backends};
use super::fallback::{Fallback, LimitedDiagnostics};
use super::live_index::LiveIndex;
use super::position::{normalize_locations, normalize_reference_locations};
use super::request::{IntelligenceRequest, Operation};
use super::session::{Attempt, Semantic, Session, Workspace};
use super::uri::UriCodec;
use super::worker::{WorkerOperation, WorkerResponse};
use super::workspace_edit::{workspace_edit_changes, RenameChange};
use crate::error::{AppError, AppResult, ErrorCode};
use crate::model::IntelligenceSettings;
use crate::reference_service::{Freshness, SemanticReferenceMetadata};
use crate::requests::edit::{EditRequest, ResponseDetail};
use crate::workspace::StateReader;
use serde_json::{json, Value};
use std::path::PathBuf;

const REFERENCE_CAVEAT: &str = "Language-server locations were produced from a full-text synchronized document hash that still matches disk and the live index.";

pub struct IntelligenceService {
    root: PathBuf,
    codec: UriCodec,
    workspace_id: String,
    live: LiveIndex,
    backends: Backends,
}

/// A `code_intelligence` answer. Rename previews carry typed changes that the
/// caller turns into an edit preview.
pub enum IntelligenceResponse {
    Report(Value),
    RenamePreview(RenamePreview),
}

pub struct RenamePreview {
    report: Value,
    changes: Vec<RenameChange>,
}

impl RenamePreview {
    /// The edit preview request for these changes at `snapshot_id`.
    pub fn edit_request(&self, snapshot_id: Option<String>) -> EditRequest {
        EditRequest {
            changes: self.changes.iter().map(RenameChange::to_change).collect(),
            snapshot_id,
            preview: true,
            validate: Vec::new(),
            response_detail: ResponseDetail::Standard,
        }
    }

    pub fn into_report(self) -> Value {
        self.report
    }
}

impl IntelligenceResponse {
    pub fn into_value(self) -> Value {
        match self {
            Self::Report(value) => value,
            Self::RenamePreview(preview) => preview.into_report(),
        }
    }
}

impl IntelligenceService {
    pub fn new(
        root: PathBuf,
        settings: IntelligenceSettings,
        workspace_id: String,
        state: StateReader,
    ) -> AppResult<Self> {
        let backends = Backends::spawn(&root, &settings)?;
        Ok(Self::with_backends(
            root,
            workspace_id,
            LiveIndex::new(state),
            backends,
        ))
    }

    pub(crate) fn with_backends(
        root: PathBuf,
        workspace_id: String,
        live: LiveIndex,
        backends: Backends,
    ) -> Self {
        Self {
            codec: UriCodec::new(&root),
            root,
            workspace_id,
            live,
            backends,
        }
    }

    pub fn capabilities(&self) -> Value {
        let BackendCapabilities {
            languages,
            restart_state,
            semantic_available,
        } = self.backends.capabilities();
        json!({
            "supported_operations": ["definition", "references", "diagnostics", "rename_preview"],
            "languages": languages,
            "restart_state": restart_state,
            "semantic_available": semantic_available,
            "fallback_status": "operation_specific",
            "reference_backend": "shared_reference_service",
            "worker_model": "one_thread_per_configured_backend",
            "document_sync": "full_text_hash_versioned",
            "public_position_contract": {
                "line": "one_based",
                "column": "zero_based_utf16_code_units"
            }
        })
    }

    pub fn execute(&self, request: IntelligenceRequest) -> AppResult<IntelligenceResponse> {
        let session = Session::open(self.workspace(), &request.path)?;
        let backend = self.backends.for_path(&request.path);
        let fallback = Fallback {
            live: &self.live,
            workspace_id: &self.workspace_id,
        };
        let report = match request.operation {
            Operation::Definition { position } => {
                let attempt = session.attempt(
                    backend,
                    WorkerOperation::Definition { position },
                    |response, files| {
                        normalize_locations(
                            files,
                            &response.result,
                            response.capabilities.position_encoding,
                        )
                    },
                );
                match attempt {
                    Attempt::Semantic(Semantic { value, response }) => json!({
                        "operation": "definition",
                        "backend": "semantic",
                        "semantic": true,
                        "evidence": "semantic",
                        "results": value,
                        "synchronization": synchronization(&response)
                    }),
                    Attempt::Fallback(reason) => {
                        fallback.definition(&session, position.line, reason)
                    }
                }
            }
            Operation::Diagnostics { max_results } => {
                let attempt =
                    session.attempt(backend, WorkerOperation::Diagnostics, |response, _| {
                        let items = response.result.as_array().ok_or_else(|| {
                            AppError::new(
                                ErrorCode::LspProtocolError,
                                "Diagnostic result is not an array",
                            )
                        })?;
                        Ok(LimitedDiagnostics::new(items.clone(), max_results))
                    });
                match attempt {
                    Attempt::Semantic(Semantic { value, response }) => json!({
                        "operation": "diagnostics",
                        "backend": "semantic",
                        "semantic": true,
                        "evidence": "semantic",
                        "status": "available",
                        "diagnostics_available": true,
                        "semantic_diagnostics_available": true,
                        "total_count": value.total_count,
                        "result_count": value.items.len(),
                        "truncated": value.truncated(),
                        "diagnostics": value.items,
                        "synchronization": synchronization(&response)
                    }),
                    Attempt::Fallback(reason) => {
                        fallback.diagnostics(&session, max_results, reason)
                    }
                }
            }
            Operation::References {
                position,
                max_results,
            } => {
                let attempt = session.attempt(
                    backend,
                    WorkerOperation::References { position },
                    |response, files| {
                        let locations = normalize_reference_locations(
                            files,
                            &response.result,
                            response.capabilities.position_encoding,
                        )?;
                        let target = self.live.references(|references| {
                            references.resolve_position(&session.relative, position.line)
                        })?;
                        Ok((target, locations))
                    },
                );
                match attempt {
                    Attempt::Semantic(Semantic {
                        value: (target, locations),
                        response,
                    }) => {
                        let snapshot_id = self.live.snapshot_id();
                        let mut result = self.live.references(|references| {
                            references.semantic(
                                &self.workspace_id,
                                &snapshot_id,
                                target,
                                locations,
                                max_results,
                                SemanticReferenceMetadata {
                                    freshness: Freshness::Current,
                                    evidence_caveat: REFERENCE_CAVEAT,
                                },
                            )
                        })?;
                        if let Some(object) = result.as_object_mut() {
                            object.insert("synchronization".to_owned(), synchronization(&response));
                        }
                        result
                    }
                    Attempt::Fallback(reason) => {
                        fallback.references(&session, position.line, max_results, reason)?
                    }
                }
            }
            Operation::RenamePreview { position, new_name } => {
                let attempt = session.attempt(
                    backend,
                    WorkerOperation::Rename { position, new_name },
                    |response, files| {
                        workspace_edit_changes(
                            files,
                            &response.result,
                            response.capabilities.position_encoding,
                        )
                    },
                );
                return match attempt {
                    Attempt::Semantic(Semantic { value, response }) => {
                        Ok(IntelligenceResponse::RenamePreview(RenamePreview {
                            report: json!({
                                "operation": "rename_preview",
                                "backend": "semantic",
                                "semantic": true,
                                "evidence": "semantic",
                                "changes": value.iter().map(RenameChange::to_json).collect::<Vec<_>>(),
                                "synchronization": synchronization(&response)
                            }),
                            changes: value,
                        }))
                    }
                    Attempt::Fallback(Some(reason)) => Err(reason.error),
                    Attempt::Fallback(None) => Err(AppError::new(
                        ErrorCode::SemanticBackendUnavailable,
                        "No enabled LSP backend supports this file",
                    )),
                };
            }
        };
        Ok(IntelligenceResponse::Report(report))
    }

    fn workspace(&self) -> Workspace<'_> {
        Workspace {
            root: &self.root,
            codec: &self.codec,
            live: &self.live,
        }
    }
}

fn synchronization(response: &WorkerResponse) -> Value {
    json!({
        "freshness": "current",
        "synchronized_hash": response.synchronized.hash,
        "document_version": response.synchronized.version,
        "server": response.capabilities.to_json()
    })
}
