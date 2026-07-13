use super::normalize::{normalize_locations, normalize_reference_locations};
use super::sync::DocumentSnapshot;
use super::worker::{LspPreset, LspWorker, WorkerOperation, WorkerResponse};
use super::workspace_edit::workspace_edit_changes;
use crate::model::{AppError, AppResult, IntelligenceSettings};
use crate::security::validate_relative;
use crate::symbols::{extract_symbols, language_name, parse_has_error};
use codeweave_rust::index::CodeIndex;
use codeweave_rust::reference_service::{ReferenceService, SemanticReferenceMetadata};
use parking_lot::RwLock;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Clone)]
pub struct IntelligenceService {
    root: PathBuf,
    workspace_id: String,
    index: Arc<RwLock<CodeIndex>>,
    snapshot_id: Arc<RwLock<String>>,
    rust: Arc<LspWorker>,
    python: Arc<LspWorker>,
    typescript: Arc<LspWorker>,
}

pub trait CodeIntelligenceBackend: Send + Sync {
    fn definition(&self, path: &Path, line: usize, column: usize) -> AppResult<Value>;
    fn diagnostics(&self, path: &Path) -> AppResult<Value>;
}

#[derive(Default)]
pub struct TreeSitterBackend;

impl CodeIntelligenceBackend for TreeSitterBackend {
    fn definition(&self, path: &Path, line: usize, _column: usize) -> AppResult<Value> {
        let content = fs::read_to_string(path)?;
        let symbol = extract_symbols(path, &content)
            .into_iter()
            .find(|symbol| symbol.start_line <= line && symbol.end_line >= line);
        Ok(json!({
            "evidence": "syntactic",
            "result": symbol.map(|symbol| json!({"path": path, "symbol": symbol}))
        }))
    }

    fn diagnostics(&self, path: &Path) -> AppResult<Value> {
        let content = fs::read_to_string(path)?;
        Ok(json!({
            "evidence": "syntactic",
            "diagnostics": if parse_has_error(path, &content) == Some(true) {
                vec![json!({"severity": "error", "message": "Tree-sitter reported a syntax error"})]
            } else {
                Vec::<Value>::new()
            }
        }))
    }
}

impl IntelligenceService {
    pub fn new(
        root: PathBuf,
        settings: IntelligenceSettings,
        workspace_id: String,
        index: Arc<RwLock<CodeIndex>>,
        snapshot_id: Arc<RwLock<String>>,
    ) -> Self {
        Self {
            rust: Arc::new(LspWorker::new(root.clone(), LspPreset::Rust, settings.rust)),
            python: Arc::new(LspWorker::new(
                root.clone(),
                LspPreset::Python,
                settings.python,
            )),
            typescript: Arc::new(LspWorker::new(
                root.clone(),
                LspPreset::TypeScript,
                settings.typescript,
            )),
            root,
            workspace_id,
            index,
            snapshot_id,
        }
    }

    pub fn capabilities(&self) -> Value {
        let languages = vec![
            self.rust.status(),
            self.python.status(),
            self.typescript.status(),
        ];
        json!({
            "supported_operations": ["definition", "references", "diagnostics", "rename_preview"],
            "languages": languages,
            "restart_state": {
                "rust": self.rust.status(),
                "python": self.python.status(),
                "typescript": self.typescript.status()
            },
            "semantic_available": self.rust.configured()
                || self.python.configured()
                || self.typescript.configured(),
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

    pub fn execute(&self, params: &Value) -> AppResult<Value> {
        let operation = params
            .get("operation")
            .and_then(Value::as_str)
            .ok_or_else(|| AppError::invalid("code_intelligence requires operation"))?;
        match operation {
            "definition" => self.definition(params),
            "references" => self.references(params),
            "diagnostics" => self.diagnostics(params),
            "rename_preview" => self.rename_preview(params),
            _ => Err(AppError::invalid("Unknown code_intelligence operation")),
        }
    }

    fn relative_and_local_path(&self, params: &Value) -> AppResult<(String, PathBuf)> {
        let relative = params
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| AppError::invalid("code_intelligence requires path"))?;
        let relative = validate_relative(relative)?
            .to_string_lossy()
            .replace('\\', "/");
        Ok((relative.clone(), self.root.join(relative)))
    }

    fn worker_for(&self, path: &Path) -> Option<&LspWorker> {
        let worker = match language_name(path) {
            "rust" => self.rust.as_ref(),
            "python" => self.python.as_ref(),
            "typescript" | "tsx" | "javascript" => self.typescript.as_ref(),
            _ => return None,
        };
        worker.configured().then_some(worker)
    }

    fn document_for(&self, worker: &LspWorker, path: &Path) -> AppResult<DocumentSnapshot> {
        DocumentSnapshot::read(path, worker.preset().language_id())
    }

    fn verify_index_hash(&self, relative: &str, response: &WorkerResponse) -> AppResult<()> {
        let index = self.index.read();
        let indexed = index.get(relative).ok_or_else(|| {
            AppError::details(
                "LSP_INDEX_STALE",
                "Semantic target is not present in the live index",
                json!({"path": relative}),
            )
        })?;
        if indexed.hash == response.synchronized.hash {
            Ok(())
        } else {
            Err(AppError::details(
                "LSP_INDEX_STALE",
                "Semantic result does not match the live indexed file hash",
                json!({
                    "path": relative,
                    "synchronized_hash": response.synchronized.hash,
                    "indexed_hash": indexed.hash
                }),
            ))
        }
    }

    fn semantic_metadata(response: &WorkerResponse) -> Value {
        json!({
            "freshness": "current",
            "synchronized_hash": response.synchronized.hash,
            "document_version": response.synchronized.version,
            "server": response.capabilities.to_json()
        })
    }

    fn definition(&self, params: &Value) -> AppResult<Value> {
        let (_relative, path) = self.relative_and_local_path(params)?;
        let line = params
            .get("line")
            .and_then(Value::as_u64)
            .ok_or_else(|| AppError::invalid("definition requires line"))?
            as usize;
        let column = params.get("column").and_then(Value::as_u64).unwrap_or(0) as usize;
        if let Some(worker) = self.worker_for(&path) {
            let semantic = self
                .document_for(worker, &path)
                .and_then(|document| {
                    worker.execute(WorkerOperation::Definition { line, column }, document)
                })
                .and_then(|response| {
                    let results = normalize_locations(
                        &self.root,
                        &response.result,
                        response.capabilities.position_encoding,
                    )?;
                    Ok((response, results))
                });
            match semantic {
                Ok((response, results)) => {
                    return Ok(json!({
                        "operation": "definition",
                        "backend": "semantic",
                        "semantic": true,
                        "evidence": "semantic",
                        "results": results,
                        "synchronization": Self::semantic_metadata(&response)
                    }));
                }
                Err(error) => {
                    return self.definition_fallback(&path, line, column, Some(error));
                }
            }
        }
        self.definition_fallback(&path, line, column, None)
    }

    fn definition_fallback(
        &self,
        path: &Path,
        line: usize,
        column: usize,
        reason: Option<AppError>,
    ) -> AppResult<Value> {
        let result = TreeSitterBackend.definition(path, line, column)?;
        let mut response = json!({
            "operation": "definition",
            "backend": "fallback",
            "evidence": "syntactic",
            "result": result
        });
        insert_fallback_reason(&mut response, reason);
        Ok(response)
    }

    fn diagnostics(&self, params: &Value) -> AppResult<Value> {
        let (_relative, path) = self.relative_and_local_path(params)?;
        if let Some(worker) = self.worker_for(&path) {
            let semantic = self
                .document_for(worker, &path)
                .and_then(|document| worker.execute(WorkerOperation::Diagnostics, document));
            match semantic {
                Ok(response) => {
                    return Ok(json!({
                        "operation": "diagnostics",
                        "backend": "semantic",
                        "semantic": true,
                        "evidence": "semantic",
                        "diagnostics": response.result,
                        "synchronization": Self::semantic_metadata(&response)
                    }));
                }
                Err(error) => return self.diagnostics_fallback(&path, Some(error)),
            }
        }
        self.diagnostics_fallback(&path, None)
    }

    fn diagnostics_fallback(&self, path: &Path, reason: Option<AppError>) -> AppResult<Value> {
        let result = TreeSitterBackend.diagnostics(path)?;
        let mut response = json!({
            "operation": "diagnostics",
            "backend": "fallback",
            "evidence": "syntactic",
            "result": result
        });
        insert_fallback_reason(&mut response, reason);
        Ok(response)
    }

    fn references(&self, params: &Value) -> AppResult<Value> {
        let (relative, path) = self.relative_and_local_path(params)?;
        let line = params
            .get("line")
            .and_then(Value::as_u64)
            .ok_or_else(|| AppError::invalid("references requires line"))?
            as usize;
        let column = params.get("column").and_then(Value::as_u64).unwrap_or(0) as usize;
        let max_results = params
            .get("max_results")
            .and_then(Value::as_u64)
            .unwrap_or(20)
            .min(200) as usize;
        let snapshot_id = self.snapshot_id.read().clone();

        let semantic = self.worker_for(&path).map(|worker| {
            self.document_for(worker, &path)
                .and_then(|document| {
                    worker.execute(WorkerOperation::References { line, column }, document)
                })
                .and_then(|response| {
                    self.verify_index_hash(&relative, &response)?;
                    let locations = normalize_reference_locations(
                        &self.root,
                        &response.result,
                        response.capabilities.position_encoding,
                    )?;
                    Ok((response, locations))
                })
                .and_then(|(response, locations)| {
                    let target = {
                        let index = self.index.read();
                        ReferenceService::new(&index).resolve_position(&relative, line)?
                    };
                    Ok((response, locations, target))
                })
        });

        match semantic {
            Some(Ok((response, locations, target))) => {
                let index = self.index.read();
                let mut result = ReferenceService::new(&index).semantic(
                    &self.workspace_id,
                    &snapshot_id,
                    target,
                    locations,
                    max_results,
                    SemanticReferenceMetadata {
                        freshness: "current",
                        evidence_caveat: "Language-server locations were produced from a full-text synchronized document hash that still matches disk and the live index.",
                    },
                )?;
                if let Some(object) = result.as_object_mut() {
                    object.insert(
                        "synchronization".to_owned(),
                        Self::semantic_metadata(&response),
                    );
                }
                Ok(result)
            }
            Some(Err(error)) => self.reference_fallback(&relative, line, max_results, Some(error)),
            None => self.reference_fallback(&relative, line, max_results, None),
        }
    }

    fn reference_fallback(
        &self,
        relative: &str,
        line: usize,
        max_results: usize,
        reason: Option<AppError>,
    ) -> AppResult<Value> {
        let snapshot_id = self.snapshot_id.read().clone();
        let index = self.index.read();
        let mut response = ReferenceService::new(&index).fallback_at_position(
            &self.workspace_id,
            &snapshot_id,
            relative,
            line,
            max_results,
        )?;
        insert_fallback_reason(&mut response, reason);
        Ok(response)
    }

    fn rename_preview(&self, params: &Value) -> AppResult<Value> {
        let (relative, path) = self.relative_and_local_path(params)?;
        let line = params
            .get("line")
            .and_then(Value::as_u64)
            .ok_or_else(|| AppError::invalid("rename_preview requires line"))?
            as usize;
        let column = params.get("column").and_then(Value::as_u64).unwrap_or(0) as usize;
        let new_name = params
            .get("new_name")
            .and_then(Value::as_str)
            .ok_or_else(|| AppError::invalid("rename_preview requires new_name"))?;
        let worker = self.worker_for(&path).ok_or_else(|| {
            AppError::new(
                "SEMANTIC_BACKEND_UNAVAILABLE",
                "No enabled LSP backend supports this file",
            )
        })?;
        let response = worker.execute(
            WorkerOperation::Rename {
                line,
                column,
                new_name: new_name.to_owned(),
            },
            self.document_for(worker, &path)?,
        )?;
        self.verify_index_hash(&relative, &response)?;
        let changes = workspace_edit_changes(
            &self.root,
            &response.result,
            response.capabilities.position_encoding,
        )?;
        Ok(json!({
            "operation": "rename_preview",
            "backend": "semantic",
            "semantic": true,
            "evidence": "semantic",
            "changes": changes,
            "synchronization": Self::semantic_metadata(&response)
        }))
    }
}

fn insert_fallback_reason(response: &mut Value, reason: Option<AppError>) {
    if let (Some(object), Some(error)) = (response.as_object_mut(), reason) {
        object.insert("fallback_reason".to_owned(), json!(error.0));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intelligence::protocol::{
        PositionEncoding, ServerCapabilities, TextDocumentSyncKind,
    };
    use crate::intelligence::sync::{DocumentSnapshot, SynchronizedDocument};
    use crate::intelligence::worker::WorkerResponse;
    use codeweave_rust::index::WorkspaceExclusions;

    #[test]
    fn semantic_reference_hash_must_match_live_index() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("src")).unwrap();
        let path = root.path().join("src/main.rs");
        fs::write(&path, "fn before() {}\n").unwrap();
        let exclusions = WorkspaceExclusions::new(root.path(), &[]).unwrap();
        let index = Arc::new(RwLock::new(
            CodeIndex::scan(root.path(), 1_000_000, &[], &exclusions).unwrap(),
        ));
        let service = IntelligenceService::new(
            root.path().to_path_buf(),
            IntelligenceSettings::default(),
            "phase5".to_owned(),
            Arc::clone(&index),
            Arc::new(RwLock::new("snap_phase5".to_owned())),
        );
        fs::write(&path, "fn after() {}\n").unwrap();
        let disk = DocumentSnapshot::read(&path, "rust").unwrap();
        let response = WorkerResponse {
            result: Value::Null,
            synchronized: SynchronizedDocument {
                path,
                hash: disk.hash,
                version: 2,
            },
            capabilities: ServerCapabilities {
                references_provider: true,
                definition_provider: true,
                rename_provider: true,
                diagnostics_provider: true,
                sync_kind: TextDocumentSyncKind::Full,
                position_encoding: PositionEncoding::Utf16,
                server_name: Some("fixture".to_owned()),
                server_version: Some("1".to_owned()),
                initialization_ms: 1,
            },
        };
        let error = service
            .verify_index_hash("src/main.rs", &response)
            .unwrap_err();
        assert_eq!(error.0.code, "LSP_INDEX_STALE");
    }
}
